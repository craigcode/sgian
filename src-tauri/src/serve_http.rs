use super::*;

// ---------------------------------------------------------------------------
// `sgian serve` (docs/design/served-view.md): host the web client over HTTP
// on a loopback port for a device that reaches this machine through an SSH
// tunnel. The page speaks the same frontend contract as the Tauri host
// (`frontend_command` in, `frontend_event` out); the daemon does the rest.
// ---------------------------------------------------------------------------

/// The built web client, embedded at compile time. Empty when the frontend
/// was not built before the daemon; `serve` reports that instead of a blank
/// page.
static WEB_ASSETS: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/../dist");

pub(crate) const SERVE_DEFAULT_PORT: u16 = 8321;
/// The most `/api/invoke` will read: a frontend call is small (a layout is
/// the largest, a few KiB).
const SERVE_BODY_MAX: usize = 1024 * 1024;
const SERVE_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self' data:; connect-src 'self'; frame-ancestors 'none'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServeOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) port: u16,
    pub(crate) allow_write: bool,
}

/// `serve [--port N] [--allow-write]`.
pub(crate) fn parse_serve_args(
    workspace: PathBuf,
    args: &[String],
) -> Result<ServeOptions, String> {
    let mut options = ServeOptions {
        workspace,
        port: SERVE_DEFAULT_PORT,
        allow_write: false,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--port" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--port requires a number".to_string())?;
                options.port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port '{value}'"))?;
                index += 1;
            }
            "--allow-write" => options.allow_write = true,
            other => return Err(format!("unexpected argument for serve: {other}")),
        }
        index += 1;
    }
    Ok(options)
}

/// What a frontend `invoke` turns into. The names and camelCase argument
/// keys are the Tauri command contract (`generate_handler!` in lib.rs and
/// `ui/src/app-controller.js`), so the served page needs no changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrontendCall {
    Request(DaemonRequest),
    /// `client_holder`: answered locally.
    Holder,
    /// `ui_smoke_enabled`: always false when served.
    SmokeDisabled,
    /// Desktop-only commands (`install_update`, `complete_ui_smoke`).
    Unsupported(&'static str),
}

pub(crate) fn frontend_command(
    command: &str,
    args: &Value,
    holder: &str,
) -> Result<FrontendCall, String> {
    fn text(args: &Value, key: &str) -> Result<String, String> {
        args[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{key} must be a string"))
    }
    fn opt_text(args: &Value, key: &str) -> Option<String> {
        args[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
    let request = match command {
        "bootstrap_workspace" => DaemonRequest::BootstrapWorkspace,
        "create_pane" => DaemonRequest::CreatePane {
            title: opt_text(args, "title"),
            profile: opt_text(args, "profile"),
        },
        "close_pane" => DaemonRequest::ClosePane {
            pane_id: text(args, "paneId")?,
        },
        "rename_pane" => DaemonRequest::RenamePane {
            pane_id: text(args, "paneId")?,
            title: text(args, "title")?,
        },
        "ensure_pane_terminal" => DaemonRequest::EnsurePaneTerminal {
            pane_id: text(args, "paneId")?,
        },
        "restart_pane_terminal" => DaemonRequest::RestartPaneTerminal {
            pane_id: text(args, "paneId")?,
        },
        "write_to_pane" => DaemonRequest::SendInputAs {
            pane_id: text(args, "paneId")?,
            input: text(args, "data")?,
            holder: holder.to_string(),
            generation: None,
        },
        "resize_pane_terminal" => DaemonRequest::ResizePaneTerminal {
            pane_id: text(args, "paneId")?,
            cols: args["cols"]
                .as_u64()
                .unwrap_or(80)
                .clamp(1, u16::MAX as u64) as u16,
            rows: args["rows"]
                .as_u64()
                .unwrap_or(24)
                .clamp(1, u16::MAX as u64) as u16,
        },
        "set_active_pane" => DaemonRequest::SetActivePane {
            pane_id: text(args, "paneId")?,
        },
        "update_workspace_layout" => DaemonRequest::UpdateWorkspaceLayout {
            layout: args["layout"].clone(),
        },
        "get_config" => DaemonRequest::GetConfig,
        "write_config" => DaemonRequest::WriteConfig {
            config: args["config"].clone(),
        },
        "create_agent_pane" => DaemonRequest::CreateAgentPaneWithSpec {
            title: opt_text(args, "title"),
            backend: args["backend"]
                .as_str()
                .map(|value| serde_json::from_value(Value::String(value.to_string())))
                .transpose()
                .map_err(|_| "backend must be claude or droid".to_string())?,
            model: opt_text(args, "model"),
        },
        "send_agent_message" => DaemonRequest::SendAgentMessage {
            pane_id: text(args, "paneId")?,
            text: text(args, "text")?,
            message_id: opt_text(args, "messageId"),
        },
        "agent_approval" => DaemonRequest::AgentApproval {
            pane_id: text(args, "paneId")?,
            request_id: text(args, "requestId")?,
            allow: args["allow"].as_bool().unwrap_or(false),
            message: opt_text(args, "message"),
        },
        "interrupt_agent" => DaemonRequest::InterruptAgent {
            pane_id: text(args, "paneId")?,
        },
        "take_lease" => DaemonRequest::TakeLease {
            pane_id: text(args, "paneId")?,
            holder: holder.to_string(),
            force: args["force"].as_bool().unwrap_or(false),
            why: opt_text(args, "why"),
        },
        "release_lease" => DaemonRequest::ReleaseLease {
            pane_id: text(args, "paneId")?,
            holder: holder.to_string(),
            note: text(args, "note")?,
            generation: None,
        },
        "lease_status" => DaemonRequest::LeaseStatus {
            pane_id: text(args, "paneId")?,
        },
        "client_holder" => return Ok(FrontendCall::Holder),
        "ui_smoke_enabled" => return Ok(FrontendCall::SmokeDisabled),
        "install_update" => {
            return Ok(FrontendCall::Unsupported(
                "updates are installed on the desk machine",
            ))
        }
        "complete_ui_smoke" => return Ok(FrontendCall::Unsupported("not a packaged UI")),
        other => return Err(format!("unknown frontend command: {other}")),
    };
    Ok(FrontendCall::Request(request))
}

/// One server-sent event: `event: <name>` + one `data:` line.
pub(crate) fn sse_frame(name: &str, payload: &Value) -> String {
    format!("event: {name}\ndata: {payload}\n\n")
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Resolve a request path to an embedded asset. `/` is `index.html`;
/// anything with `..` or a leading absolute component is refused.
pub(crate) fn embedded_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    let trimmed = path.split('?').next().unwrap_or("").trim_start_matches('/');
    let name = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    if name.split('/').any(|part| part == ".." || part.is_empty()) {
        return None;
    }
    let file = WEB_ASSETS.get_file(name)?;
    Some((content_type(name), file.contents()))
}

pub(crate) fn assets_present() -> bool {
    WEB_ASSETS.get_file("index.html").is_some()
}

/// One parsed HTTP/1.1 request: enough for three routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HttpRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) host: Option<String>,
    pub(crate) content_length: usize,
}

const HEAD_MAX: usize = 64 * 1024;

/// Parse a request head (everything before the blank line). Returns `None`
/// for anything that is not a plausible HTTP/1.x request line.
pub(crate) fn parse_request_head(head: &str) -> Option<HttpRequest> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") || parts.next().is_some() || method.is_empty() {
        return None;
    }
    let mut host = None;
    let mut content_length = 0;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            host = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok()?;
        }
    }
    Some(HttpRequest {
        method,
        path: target.split('?').next().unwrap_or("/").to_string(),
        host,
        content_length,
    })
}

/// Only loopback hosts: a page on another origin (DNS rebinding) must not
/// be able to reach the API through the tunnel's local port.
pub(crate) fn host_is_loopback(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let bare = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    matches!(bare, "localhost" | "127.0.0.1" | "[::1]")
}

fn write_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    reason: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<()> {
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(out.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_json(stream: &mut std::net::TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write_response(
        stream,
        status,
        reason,
        &[
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-store"),
        ],
        body.to_string().as_bytes(),
    )
}

/// A running server: the bound address and the accept thread. `ctl serve`
/// runs until its process ends; `stop` exists for embedders and tests.
pub(crate) struct ServeHandle {
    pub(crate) addr: std::net::SocketAddr,
    #[allow(dead_code)]
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ServeHandle {
    #[allow(dead_code)]
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop so it notices the flag.
        let _ = std::net::TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Bind and start serving on a background thread. `port` 0 picks a free one.
pub(crate) fn start_serve(options: ServeOptions) -> Result<ServeHandle, String> {
    let client = DaemonClient::connect_existing(options.workspace.clone())?;
    start_serve_with(client, options.port, options.allow_write)
}

/// `start_serve` over an already-connected client (tests use a daemon whose
/// data dir is not where the workspace lookup expects it).
pub(crate) fn start_serve_with(
    client: DaemonClient,
    port: u16,
    allow_write: bool,
) -> Result<ServeHandle, String> {
    let identity: Value = client.request(DaemonRequest::Whoami)?;
    let holder = identity["holder"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(default_holder);
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| format!("cannot listen on 127.0.0.1:{port}: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("cannot read the listen address: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        for incoming in listener.incoming() {
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = incoming else {
                continue;
            };
            let client = client.clone();
            let holder = holder.clone();
            thread::spawn(move || handle_connection(stream, &client, &holder, allow_write));
        }
    });
    Ok(ServeHandle {
        addr,
        stop,
        thread: Some(thread),
    })
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    client: &DaemonClient,
    holder: &str,
    allow_write: bool,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    // Read the head, then exactly Content-Length bytes of body.
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        if raw.len() > HEAD_MAX {
            let _ = write_json(
                &mut stream,
                400,
                &json!({ "ok": false, "error": "request head too large" }),
            );
            return;
        }
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let Some(request) = parse_request_head(&head) else {
        let _ = write_json(
            &mut stream,
            400,
            &json!({ "ok": false, "error": "malformed request" }),
        );
        return;
    };
    if !host_is_loopback(request.host.as_deref()) {
        let _ = write_json(
            &mut stream,
            403,
            &json!({ "ok": false, "error": "loopback hosts only" }),
        );
        return;
    }
    if request.content_length > SERVE_BODY_MAX {
        let _ = write_json(
            &mut stream,
            400,
            &json!({ "ok": false, "error": "body too large" }),
        );
        return;
    }
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < request.content_length {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => return,
        }
    }
    body.truncate(request.content_length);

    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/api/invoke") => {
            let call: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let command = call["command"].as_str().unwrap_or("");
            let args = call.get("args").cloned().unwrap_or(Value::Null);
            let outcome = match frontend_command(command, &args, holder) {
                Err(error) => Err(error),
                Ok(FrontendCall::Holder) => Ok(json!(holder)),
                Ok(FrontendCall::SmokeDisabled) => Ok(json!(false)),
                Ok(FrontendCall::Unsupported(why)) => Err(format!(
                    "{command} is not available in a served view: {why}"
                )),
                Ok(FrontendCall::Request(daemon_request)) => {
                    if !allow_write && request_scope(&daemon_request) != ClientScope::Read {
                        Err(format!(
                            "read-only view: {command} needs `sgian ctl serve --allow-write` on the desk machine"
                        ))
                    } else {
                        client.request::<Value>(daemon_request)
                    }
                }
            };
            let answer = match outcome {
                Ok(result) => json!({ "ok": true, "result": result }),
                Err(error) => json!({ "ok": false, "error": error }),
            };
            let _ = write_json(&mut stream, 200, &answer);
        }
        ("GET", "/api/events") => stream_events(stream, client),
        ("GET", path) | ("HEAD", path) => match embedded_asset(path) {
            Some((kind, bytes)) => {
                let cache = if path == "/" || path.ends_with(".html") {
                    "no-store"
                } else {
                    "public, max-age=3600"
                };
                let headers = [
                    ("Content-Type", kind),
                    ("Cache-Control", cache),
                    ("Content-Security-Policy", SERVE_CSP),
                    ("X-Content-Type-Options", "nosniff"),
                    ("Referrer-Policy", "no-referrer"),
                ];
                let _ = write_response(
                    &mut stream,
                    200,
                    "OK",
                    &headers,
                    if request.method == "HEAD" { b"" } else { bytes },
                );
            }
            None => {
                let message = if assets_present() {
                    "not found"
                } else {
                    "the web client was not built into this binary (run `npm run frontend:build` before `cargo build`)"
                };
                let _ = write_response(
                    &mut stream,
                    404,
                    "Not Found",
                    &[("Content-Type", "text/plain; charset=utf-8")],
                    message.as_bytes(),
                );
            }
        },
        _ => {
            let _ = write_json(
                &mut stream,
                405,
                &json!({ "ok": false, "error": "method not allowed" }),
            );
        }
    }
}

/// Subscribe to the daemon and forward every frontend event as SSE, one
/// flush per event, until the page goes away (a write fails).
fn stream_events(mut stream: std::net::TcpStream, client: &DaemonClient) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n";
    if stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.flush())
        .is_err()
    {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let mut conn = match client.connect() {
        Ok(conn) => conn,
        Err(error) => {
            let _ =
                stream.write_all(sse_frame("serve-error", &json!({ "error": error })).as_bytes());
            return;
        }
    };
    if conn.write_request(&DaemonRequest::Subscribe).is_err() || conn.await_subscribe_ack().is_err()
    {
        let _ = stream.write_all(
            sse_frame("serve-error", &json!({ "error": "subscribe failed" })).as_bytes(),
        );
        return;
    }
    if stream
        .write_all(b": connected\n\n")
        .and_then(|_| stream.flush())
        .is_err()
    {
        return;
    }
    conn.set_read_timeout(None);
    while let Ok(Some(event)) = conn.read_event() {
        if let Some((name, payload)) = frontend_event(event) {
            if stream
                .write_all(sse_frame(name, &payload).as_bytes())
                .and_then(|_| stream.flush())
                .is_err()
            {
                return;
            }
        }
    }
}

/// `sgian ctl serve`: serve in the foreground until interrupted.
pub(crate) fn control_serve(options: ServeOptions) -> Result<(), String> {
    if !assets_present() {
        return Err(
            "the web client was not built into this binary: run `npm run frontend:build`, then rebuild"
                .to_string(),
        );
    }
    let allow_write = options.allow_write;
    let mut handle = start_serve(options)?;
    let port = handle.addr.port();
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "serving the Sgian web client on http://127.0.0.1:{port} ({}); from another machine:\n  ssh -N -L {port}:127.0.0.1:{port} <this-host>   then open http://localhost:{port}",
        if allow_write { "writes allowed" } else { "read-only; add --allow-write to type" }
    );
    let _ = stdout.flush();
    if let Some(thread) = handle.thread.take() {
        let _ = thread.join();
    }
    Ok(())
}
