use super::*;

#[derive(Debug, Serialize)]
pub(crate) struct WorkspaceInfo {
    pub(crate) key: String,
    pub(crate) cwd: String,
    pub(crate) panes: usize,
    pub(crate) active_pane_id: Option<String>,
}

/// Non-secret discovery metadata for native clients. The authentication token
/// is intentionally never included: clients read it from the owner-private
/// token file after locating that file through this response.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct NativeIpcEndpoint {
    pub(crate) transport: String,
    pub(crate) endpoint: String,
    pub(crate) token_path: String,
    pub(crate) workspace: String,
    pub(crate) workspace_key: String,
    pub(crate) protocol_version: u32,
    pub(crate) capabilities: Vec<String>,
}

pub(crate) fn native_ipc_endpoint(client: &DaemonClient) -> Result<NativeIpcEndpoint, String> {
    let workspace = canonical_workspace_path(&client.cwd);
    Ok(NativeIpcEndpoint {
        transport: if cfg!(windows) {
            "named_pipe".to_string()
        } else {
            "unix_socket".to_string()
        },
        endpoint: transport_endpoint(&client.socket_path)
            .map_err(|error| format!("failed to resolve daemon endpoint: {error}"))?,
        token_path: client.data_dir.join(TOKEN_FILE).display().to_string(),
        workspace: workspace.display().to_string(),
        workspace_key: workspace_key(&workspace),
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec!["subscribe-ack".to_string()],
    })
}

pub(crate) fn control_ipc_endpoint(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let info = native_ipc_endpoint(client)?;
    if json_output {
        return write_json_stdout(&info);
    }

    let mut stdout = std::io::stdout();
    writeln!(stdout, "transport\t{}", info.transport)
        .and_then(|_| writeln!(stdout, "endpoint\t{}", info.endpoint))
        .and_then(|_| writeln!(stdout, "token\t{}", info.token_path))
        .and_then(|_| writeln!(stdout, "workspace\t{}", info.workspace))
        .map_err(|error| format!("failed to write stdout: {error}"))
}

pub(crate) fn is_control_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(CTL_ARG)
        || args
            .first()
            .and_then(|path| Path::new(path).file_stem())
            .and_then(|name| name.to_str())
            .map(|name| matches!(name, "sgianctl" | "sgian2ctl"))
            .unwrap_or(false)
}

pub(crate) fn daemon_socket_for_key(key: &str) -> PathBuf {
    workspace_runtime_dir(key).join(SOCKET_FILE)
}

pub(crate) fn daemon_token_for_key(key: &str) -> Option<String> {
    read_token(&workspace_data_dir(key).join(TOKEN_FILE))
        .ok()
        .flatten()
}

/// True if a daemon is accepting and authenticating on this workspace's socket.
pub(crate) fn daemon_is_alive(key: &str) -> bool {
    let Some(token) = daemon_token_for_key(key) else {
        return false;
    };
    daemon_is_alive_probed(&daemon_socket_for_key(key), &token)
}

/// (07-19 CLI low) Liveness probe behind `ctl daemons` / `ctl shutdown --all`:
/// probe the workspace flock FIRST. No lock held ⇒ no live daemon ⇒ report
/// not-running WITHOUT pinging — a wedged workspace's ping would otherwise
/// cost up to CLIENT_READ_TIMEOUT per row. Lock held ⇒ ping as before (the
/// daemon may be alive, or wedged; only the ping distinguishes).
pub(crate) fn daemon_is_alive_probed(socket_path: &Path, token: &str) -> bool {
    if !daemon_lock_is_held(socket_path) {
        return false;
    }
    daemon_is_alive_at(socket_path, token)
}

/// Probe liveness + auth over the NEGOTIATED protocol (`DaemonConnection`, which
/// negotiates framed v2 and gracefully falls back to newline v1 against an old
/// daemon). Routing the probe here keeps EVERY ctl path — including `ctl daemons`
/// and `ctl shutdown --all`, which call `daemon_is_alive` — negotiating by default,
/// while still reporting an old v1-only daemon alive.
pub(crate) fn daemon_is_alive_at(socket_path: &Path, token: &str) -> bool {
    DaemonConnection::connect(socket_path, token).is_ok()
}

/// Send a request to an already-running daemon by workspace key (never spawns one).
pub(crate) fn daemon_request_by_key(
    key: &str,
    request: DaemonRequest,
) -> Result<IpcResponse, String> {
    let token = daemon_token_for_key(key).ok_or_else(|| format!("no token for workspace {key}"))?;
    let mut conn = DaemonConnection::connect(&daemon_socket_for_key(key), &token)?;
    conn.request(&request)
}

pub(crate) fn workspace_keys() -> Vec<String> {
    let root = app_support_dir().join("workspaces");
    let mut keys = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(key) = entry.file_name().to_str().map(ToString::to_string) {
                keys.push(key);
            }
        }
    }
    keys.sort();
    keys
}

pub(crate) fn workspace_cwd_for_key(key: &str) -> String {
    fs::read_to_string(workspace_data_dir(key).join(WORKSPACE_FILE))
        .ok()
        .and_then(|data| serde_json::from_str::<PersistedWorkspace>(&data).ok())
        .map(|persisted| persisted.cwd)
        .unwrap_or_default()
}

/// The most `ctl hook` / `ctl statusline` read from stdin: a Claude Code
/// payload is a few KiB; anything past this is not one.
pub(crate) const CTL_STDIN_PAYLOAD_MAX: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HookArgs {
    pub(crate) event: Option<String>,
    pub(crate) notification_type: Option<String>,
    pub(crate) pid: Option<u32>,
    /// Read the hook payload from stdin (the default; `--no-stdin` for scripts
    /// that pass everything as flags).
    pub(crate) read_stdin: bool,
}

/// `hook [--event NAME] [--type NAME] [--pid N] [--no-stdin]`.
pub(crate) fn parse_hook_args(args: &[String]) -> Result<HookArgs, String> {
    let mut parsed = HookArgs {
        read_stdin: true,
        ..HookArgs::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--event" => {
                parsed.event = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--event requires a NAME".to_string())?,
                );
                index += 1;
            }
            "--type" => {
                parsed.notification_type = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--type requires a NAME".to_string())?,
                );
                index += 1;
            }
            "--pid" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--pid requires a number".to_string())?;
                parsed.pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --pid '{value}'"))?,
                );
                index += 1;
            }
            "--no-stdin" => parsed.read_stdin = false,
            other => return Err(format!("unexpected argument for hook: {other}")),
        }
        index += 1;
    }
    if !parsed.read_stdin && parsed.event.is_none() {
        return Err("--no-stdin needs --event NAME".to_string());
    }
    Ok(parsed)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StatuslineArgs {
    pub(crate) pid: Option<u32>,
    /// A status-line command to run after reporting, fed the same payload;
    /// its stdout is passed through so the user's own line still shows.
    pub(crate) then: Vec<String>,
}

/// `statusline [--pid N] [--exec COMMAND [ARGS...]]`. (`--` cannot be the
/// separator: the global ctl parser consumes it.)
pub(crate) fn parse_statusline_args(args: &[String]) -> Result<StatuslineArgs, String> {
    let mut parsed = StatuslineArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pid" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--pid requires a number".to_string())?;
                parsed.pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --pid '{value}'"))?,
                );
                index += 1;
            }
            "--exec" => {
                parsed.then = args[index + 1..].to_vec();
                if parsed.then.is_empty() {
                    return Err("--exec needs a COMMAND to run".to_string());
                }
                break;
            }
            other => return Err(format!("unexpected argument for statusline: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

/// Send one status-line payload to the daemon that owns the calling process:
/// the payload's own cwd first (a session's cwd is its workspace more often
/// than not), then the ctl workspace, then every running daemon. Returns the
/// daemon's answer, or a `mapped: false` reason.
pub(crate) fn report_status_payload(workspace: PathBuf, pid: u32, payload: &Value) -> Value {
    let request = DaemonRequest::AgentStatus {
        pid,
        payload: payload.clone(),
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(cwd) = payload["workspace"]["current_dir"]
        .as_str()
        .or_else(|| payload["cwd"].as_str())
        .filter(|text| !text.is_empty())
    {
        candidates.push(PathBuf::from(cwd));
    }
    candidates.push(workspace);
    let mut tried = std::collections::HashSet::new();
    let mut answer = json!({ "mapped": false, "reason": "no running daemon" });
    for cwd in candidates {
        let key = workspace_key(&cwd);
        if !tried.insert(key.clone()) {
            continue;
        }
        let Ok(client) = DaemonClient::connect_existing(cwd) else {
            continue;
        };
        if let Ok(result) = client.request::<Value>(request.clone()) {
            answer = result;
            if answer["mapped"] == json!(true) {
                answer["workspace_key"] = json!(key);
                return answer;
            }
        }
    }
    for key in workspace_keys() {
        if !tried.insert(key.clone()) || !daemon_is_alive(&key) {
            continue;
        }
        let Ok(response) = daemon_request_by_key(&key, request.clone()) else {
            continue;
        };
        if response.ok && response.result["mapped"] == json!(true) {
            answer = response.result;
            answer["workspace_key"] = json!(key);
            return answer;
        }
    }
    answer
}

/// `ctl statusline`: the command Claude Code's status line runs. Reads the
/// payload from stdin, reports it to the owning daemon, then prints a line:
/// the output of the user's own status command when one follows `--exec` (fed
/// the same payload), else a compact default. Always exits 0 and always
/// prints something, so wiring Sgian in never costs the user their status
/// line. `--json` prints the daemon's answer instead.
pub(crate) fn control_statusline(
    workspace: PathBuf,
    parsed: StatuslineArgs,
    json_output: bool,
) -> Result<(), String> {
    let mut raw = String::new();
    let _ = std::io::stdin()
        .take(CTL_STDIN_PAYLOAD_MAX)
        .read_to_string(&mut raw);
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let answer = if payload.is_object() {
        report_status_payload(
            workspace,
            parsed.pid.unwrap_or(std::process::id()),
            &payload,
        )
    } else {
        json!({ "mapped": false, "reason": "stdin was not a JSON object" })
    };
    if json_output {
        return write_json_stdout(&answer);
    }
    let mut stdout = std::io::stdout();
    if let Some((program, rest)) = parsed.then.split_first() {
        let child = Command::new(program)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(raw.as_bytes());
            }
            if let Ok(output) = child.wait_with_output() {
                let _ = stdout.write_all(&output.stdout);
                let _ = stdout.flush();
                return Ok(());
            }
        }
        // The user's command failed: fall through to the default so the
        // status line is never blank because of us.
    }
    let line = AgentUsage::from_status_payload(&payload)
        .map(|usage| usage.summary(now_millis() / 1000))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "sgian".to_string());
    let _ = writeln!(stdout, "{line}");
    Ok(())
}

/// The fields `ctl hook` reads from a Claude Code hook payload (stdin JSON).
/// Everything is optional so a payload from a newer CLI still parses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct HookPayload {
    #[serde(default)]
    pub(crate) hook_event_name: Option<String>,
    #[serde(default)]
    pub(crate) notification_type: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<String>,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
}

/// Build the daemon request for a hook: flags win over the payload, the pid
/// defaults to this process (the daemon walks up from it to the pane).
pub(crate) fn hook_request(
    parsed: &HookArgs,
    payload: &HookPayload,
    own_pid: u32,
) -> Result<DaemonRequest, String> {
    let event = parsed
        .event
        .clone()
        .or_else(|| payload.hook_event_name.clone())
        .filter(|event| !event.trim().is_empty())
        .ok_or_else(|| "hook payload has no hook_event_name; pass --event NAME".to_string())?;
    Ok(DaemonRequest::AgentSignal {
        pid: parsed.pid.unwrap_or(own_pid),
        event,
        notification_type: parsed
            .notification_type
            .clone()
            .or_else(|| payload.notification_type.clone()),
        message: payload.message.clone(),
        session_id: payload.session_id.clone(),
    })
}

/// `ctl hook`: the command a Claude Code hook runs. Reads the hook payload
/// from stdin, asks the workspace daemon (this cwd first, then every other
/// running daemon) which pane owns the calling process, and hands it the
/// hook's reading. Exits 0 whatever happens: a hook must never fail the
/// session it reports on. `--json` prints the daemon's answer.
pub(crate) fn control_hook(
    workspace: PathBuf,
    parsed: HookArgs,
    json_output: bool,
) -> Result<(), String> {
    let payload: HookPayload = if parsed.read_stdin {
        let mut raw = String::new();
        let _ = std::io::stdin()
            .take(CTL_STDIN_PAYLOAD_MAX)
            .read_to_string(&mut raw);
        if raw.trim().is_empty() {
            HookPayload::default()
        } else {
            serde_json::from_str(&raw).unwrap_or_default()
        }
    } else {
        HookPayload::default()
    };
    let request = match hook_request(&parsed, &payload, std::process::id()) {
        Ok(request) => request,
        Err(error) => {
            return if json_output {
                write_json_stdout(&json!({ "mapped": false, "reason": error }))
            } else {
                Ok(())
            };
        }
    };
    let mut answer = json!({ "mapped": false, "reason": "no running daemon" });
    let mut tried = std::collections::HashSet::new();
    // This workspace first: a hook fires in the session's cwd, which is the
    // pane's cwd more often than not.
    let own_key = workspace_key(&workspace);
    tried.insert(own_key.clone());
    if let Ok(client) = DaemonClient::connect_existing(workspace) {
        if let Ok(result) = client.request::<Value>(request.clone()) {
            if result["mapped"] == json!(true) {
                answer = result;
                answer["workspace_key"] = json!(own_key);
            } else {
                answer = result;
            }
        }
    }
    if answer["mapped"] != json!(true) {
        for key in workspace_keys() {
            if !tried.insert(key.clone()) || !daemon_is_alive(&key) {
                continue;
            }
            let Ok(response) = daemon_request_by_key(&key, request.clone()) else {
                continue;
            };
            if response.ok && response.result["mapped"] == json!(true) {
                answer = response.result;
                answer["workspace_key"] = json!(key);
                break;
            }
        }
    }
    if json_output {
        return write_json_stdout(&answer);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityVerb {
    List,
    Issue,
    Revoke,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentityArgs {
    pub(crate) verb: IdentityVerb,
    pub(crate) holder: Option<String>,
    pub(crate) scopes: Vec<String>,
    pub(crate) id: Option<String>,
}

/// `identity [list] | issue --holder H [--scope read,write,admin] | revoke ID`.
pub(crate) fn parse_identity_args(args: &[String]) -> Result<IdentityArgs, String> {
    let verb = match args.first().map(String::as_str) {
        None | Some("list") => IdentityVerb::List,
        Some("issue") => IdentityVerb::Issue,
        Some("revoke") => IdentityVerb::Revoke,
        Some(other) => return Err(format!("unknown identity command: {other}")),
    };
    let mut parsed = IdentityArgs {
        verb,
        holder: None,
        scopes: Vec::new(),
        id: None,
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut index = if args.is_empty() { 0 } else { 1 };
    while index < args.len() {
        match args[index].as_str() {
            "--holder" => {
                parsed.holder = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--holder requires a NAME".to_string())?,
                );
                index += 1;
            }
            "--scope" | "--scopes" => {
                parsed.scopes.push(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--scope requires a list".to_string())?,
                );
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for identity: {other}"));
            }
            other => positionals.push(other.to_string()),
        }
        index += 1;
    }
    match verb {
        IdentityVerb::List => {
            if !positionals.is_empty() || parsed.holder.is_some() || !parsed.scopes.is_empty() {
                return Err("identity list takes no arguments".to_string());
            }
        }
        IdentityVerb::Issue => {
            if !positionals.is_empty() {
                return Err("identity issue takes --holder and --scope only".to_string());
            }
            if parsed.holder.is_none() {
                return Err("identity issue needs --holder NAME".to_string());
            }
        }
        IdentityVerb::Revoke => {
            if positionals.len() != 1 || parsed.holder.is_some() || !parsed.scopes.is_empty() {
                return Err("identity revoke needs exactly one credential ID".to_string());
            }
            parsed.id = positionals.pop();
        }
    }
    Ok(parsed)
}

pub(crate) fn control_identity(
    client: &DaemonClient,
    parsed: IdentityArgs,
    json_output: bool,
) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    let scopes_text = |record: &Value| {
        record["scopes"]
            .as_array()
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    match parsed.verb {
        IdentityVerb::List => {
            let records: Value = client.request(DaemonRequest::IdentityList)?;
            if json_output {
                return write_json_stdout(&records);
            }
            for record in records.as_array().into_iter().flatten() {
                writeln!(
                    stdout,
                    "{}\t{}\t{}\t{}",
                    record["id"].as_str().unwrap_or("-"),
                    record["holder"].as_str().unwrap_or("-"),
                    scopes_text(record),
                    if record["revoked_at_ms"].is_null() {
                        "active"
                    } else {
                        "revoked"
                    }
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        IdentityVerb::Issue => {
            let record: Value = client.request(DaemonRequest::IdentityIssue {
                holder: parsed.holder.unwrap_or_default(),
                scopes: parsed.scopes,
            })?;
            if json_output {
                return write_json_stdout(&record);
            }
            writeln!(
                stdout,
                "{}\t{}\t{}\n{}\n(shown once; export SGIAN_CLIENT_TOKEN=… on the client)",
                record["id"].as_str().unwrap_or("-"),
                record["holder"].as_str().unwrap_or("-"),
                scopes_text(&record),
                record["token"].as_str().unwrap_or("-")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))
        }
        IdentityVerb::Revoke => {
            let record: Value = client.request(DaemonRequest::IdentityRevoke {
                id: parsed.id.unwrap_or_default(),
            })?;
            if json_output {
                return write_json_stdout(&record);
            }
            writeln!(
                stdout,
                "{}\t{}\trevoked",
                record["id"].as_str().unwrap_or("-"),
                record["holder"].as_str().unwrap_or("-")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))
        }
    }
}

pub(crate) fn control_list_daemons(json_output: bool) -> Result<(), String> {
    let daemons = workspace_keys()
        .into_iter()
        .map(|key| {
            let cwd = workspace_cwd_for_key(&key);
            let alive = daemon_is_alive(&key);
            json!({ "key": key, "cwd": cwd, "alive": alive })
        })
        .collect::<Vec<_>>();

    if json_output {
        return write_json_stdout(&daemons);
    }
    let mut stdout = std::io::stdout();
    for daemon in &daemons {
        let alive = daemon["alive"].as_bool().unwrap_or(false);
        writeln!(
            stdout,
            "{}\t{}\t{}",
            if alive { "alive" } else { "dead " },
            daemon["key"].as_str().unwrap_or(""),
            daemon["cwd"].as_str().unwrap_or(""),
        )
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

pub(crate) fn control_shutdown_all(json_output: bool) -> Result<(), String> {
    let mut stopped = Vec::new();
    for key in workspace_keys() {
        if !daemon_is_alive(&key) {
            continue;
        }
        if daemon_request_by_key(&key, DaemonRequest::Shutdown)
            .map(|response| response.ok)
            .unwrap_or(false)
        {
            stopped.push(key);
        }
    }

    if json_output {
        write_json_stdout(&json!({ "stopped": stopped }))
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "stopped {} daemon(s)", stopped.len())
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// (H4) The `ctl shutdown` branch for an UNRESPONSIVE daemon (connect failed).
/// A HELD workspace lock means the daemon is alive but wedged — that is a
/// non-zero error, not the documented exit-0 "no daemon running" no-op, which
/// would both misreport and leave the wedged process holding its shells
/// hostage. A FREE (or absent) lock keeps the genuine no-daemon no-op.
pub(crate) fn shutdown_when_unresponsive(
    socket_path: &Path,
    json_output: bool,
) -> Result<(), String> {
    if daemon_lock_is_held(socket_path) {
        return Err(
            "daemon not responding (lock held); the daemon is alive but wedged — kill the process and retry"
                .to_string(),
        );
    }
    if json_output {
        write_json_stdout(&json!({ "ok": true, "stopped": false }))
    } else {
        writeln!(std::io::stdout(), "no daemon running")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn run_control_cli_from_args(args: &[String]) -> Result<(), String> {
    let raw_args = if args.get(1).map(String::as_str) == Some(CTL_ARG) {
        args.iter().skip(2).cloned().collect()
    } else {
        args.iter().skip(1).cloned().collect()
    };
    let options = parse_control_options(raw_args)?;

    let Some(command) = options.args.first().map(String::as_str) else {
        print_control_help()?;
        return Ok(());
    };

    match command {
        "help" | "--help" | "-h" => print_control_help(),
        "workspaces" => {
            ensure_no_extra_args("workspaces", &options.args[1..])?;
            control_list_workspaces(options.json)
        }
        "ipc-endpoint" => {
            ensure_no_extra_args("ipc-endpoint", &options.args[1..])?;
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_ipc_endpoint(&client, options.json)
        }
        "panes" | "list" => {
            ensure_no_extra_args(command, &options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_list_panes(&client, options.json)
        }
        "new" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_new_pane(&client, &options.args[1..], options.json)
        }
        "status" => {
            let client = DaemonClient::connect_existing(options.workspace)?;
            let status_args = &options.args[1..];
            if status_args
                .iter()
                .any(|arg| arg == "--verbose" || arg == "-v")
            {
                if let Some(extra) = status_args
                    .iter()
                    .find(|arg| *arg != "--verbose" && *arg != "-v")
                {
                    return Err(format!("unexpected argument for status: {extra}"));
                }
                control_status_verbose(&client, options.json)
            } else {
                if status_args.len() > 1 {
                    return Err(format!(
                        "unexpected argument for status: {}",
                        status_args[1]
                    ));
                }
                control_pane_status(&client, status_args, options.json)
            }
        }
        "send" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_send_input(&client, &options.args[1..])
        }
        "interrupt" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_interrupt(&client, &options.args[1..])
        }
        "restart" => {
            if options.args.len() > 2 {
                return Err(format!(
                    "unexpected argument for restart: {}",
                    options.args[2]
                ));
            }
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_restart_pane(&client, &options.args[1..], options.json)
        }
        "lease" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_lease_args(&options.args[1..])?;
            // Status never spawns a daemon; take/release start one on demand
            // like the other mutating pane commands.
            let client = if parsed.verb == LeaseVerb::Status {
                DaemonClient::connect_existing(options.workspace)?
            } else {
                DaemonClient::connect_or_spawn(options.workspace)?
            };
            control_lease(&client, parsed, options.json)
        }
        "project" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_project_args(&options.args[1..])?;
            let client = if matches!(
                parsed.verb,
                ProjectVerb::List | ProjectVerb::Show | ProjectVerb::Ledger
            ) {
                DaemonClient::connect_existing(options.workspace)?
            } else {
                DaemonClient::connect_or_spawn(options.workspace)?
            };
            control_project(&client, parsed, options.json)
        }
        "kranz" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_kranz_args(&options.args[1..])?;
            let client = if parsed.verb == KranzVerb::Status {
                DaemonClient::connect_existing(options.workspace)?
            } else {
                DaemonClient::connect_or_spawn(options.workspace)?
            };
            control_kranz(&client, parsed, options.json)
        }
        "search" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_search_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_search(&client, parsed, options.json)
        }
        "lines" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_lines_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_lines(&client, parsed, options.json)
        }
        "ledger" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_ledger_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_ledger(&client, parsed, options.json)
        }
        "attach" => {
            if options.args.len() > 2 {
                return Err(format!(
                    "unexpected argument for attach: {}",
                    options.args[2]
                ));
            }
            // (07-19 CLI low) the global parser consumes --json, but attach
            // streams raw PTY output — there is no JSON shape; fail loudly
            // instead of silently ignoring the flag.
            if options.json {
                return Err("--json is not supported for attach".to_string());
            }
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_attach_pane(&client, &options.args[1..])
        }
        "logs" => {
            // (07-19 CLI low) same silent `--json` trap as attach: the log tail
            // is plain text, so reject the flag instead of ignoring it.
            if options.json {
                return Err("--json is not supported for logs".to_string());
            }
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_logs(&client, &options.args[1..])
        }
        "exec" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_exec(&client, &options.args[1..], options.json)
        }
        "broadcast" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_broadcast(&client, &options.args[1..], options.json)
        }
        "sync" => {
            if options.args.len() > 2 {
                return Err(format!("unexpected argument for sync: {}", options.args[2]));
            }
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_sync_input(&client, &options.args[1..], options.json)
        }
        "run" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_run(&client, &options.args[1..], options.json)
        }
        "process" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_process(&client, &options.args[1..], options.json)
        }
        "diagnostic" | "diagnostics" => {
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_diagnostic(&client, &options.args[1..], options.json)
        }
        "wait" => {
            // wait blocks on an existing pane; never spawn a daemon just to wait.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_wait(&client, &options.args[1..], options.json)
        }
        "snapshot" => {
            // snapshot reads an existing pane; never spawn a daemon just to read it.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_snapshot(&client, &options.args[1..], options.json)
        }
        "find" => {
            // find queries existing panes; never spawn a daemon just to query.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_find(&client, &options.args[1..], options.json)
        }
        "agent" => {
            // agent reads/marks an existing pane; never spawn a daemon for it.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_agent(&client, &options.args[1..], options.json)
        }
        "daemons" => {
            ensure_no_extra_args("daemons", &options.args[1..])?;
            control_list_daemons(options.json)
        }
        "hook" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_hook_args(&options.args[1..])?;
            control_hook(options.workspace, parsed, options.json)
        }
        "statusline" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_statusline_args(&options.args[1..])?;
            control_statusline(options.workspace, parsed, options.json)
        }
        "identity" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_identity_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_identity(&client, parsed, options.json)
        }
        "whoami" => {
            ensure_no_extra_args("whoami", &options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            let identity: Value = client.request(DaemonRequest::Whoami)?;
            if options.json {
                return write_json_stdout(&identity);
            }
            let scopes = identity["scopes"]
                .as_array()
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let mut stdout = std::io::stdout();
            writeln!(
                stdout,
                "{}\t{}\t{}\tidentity={}",
                identity["credential"].as_str().unwrap_or("root"),
                identity["holder"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(default_holder),
                scopes,
                identity["identity_policy"].as_str().unwrap_or("open")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))
        }
        "write-config" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_write_config(&client, &options.args[1..], options.json)
        }
        "shutdown" => {
            if let Some(extra) = options.args[1..].iter().find(|arg| *arg != "--all") {
                return Err(format!("unexpected argument for shutdown: {extra}"));
            }
            if options.args.iter().any(|arg| arg == "--all") {
                control_shutdown_all(options.json)
            } else {
                // Stopping a daemon that isn't running is a success, not a reason to
                // spawn one just to kill it.
                let socket_path = daemon_socket_for_key(&workspace_key(&options.workspace));
                match DaemonClient::connect_existing(options.workspace) {
                    Ok(client) => control_shutdown(&client, options.json),
                    Err(_) => shutdown_when_unresponsive(&socket_path, options.json),
                }
            }
        }
        "pane" => control_pane_subcommand(options),
        other => Err(format!("unknown ctl command: {other}")),
    }
}

pub(crate) fn parse_control_options(raw_args: Vec<String>) -> Result<ControlOptions, String> {
    let mut workspace = resolve_workspace_dir();
    let mut json = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut after_double_dash = false;
    let mut freeform = false;
    let mut index = 0;

    while index < raw_args.len() {
        let arg = raw_args[index].as_str();

        if after_double_dash || freeform {
            positionals.push(raw_args[index].clone());
            index += 1;
            continue;
        }

        match arg {
            "--workspace" | "-w" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--workspace requires a path".to_string())?;
                workspace = PathBuf::from(value);
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--help" | "-h" => {
                if positionals.first().map(String::as_str) != Some("help") {
                    positionals.insert(0, "help".to_string());
                }
                index += 1;
            }
            "--" => {
                after_double_dash = true;
                index += 1;
            }
            value if value.starts_with('-') && positionals.is_empty() => {
                return Err(format!("unknown ctl option: {value}"));
            }
            value => {
                // The first positional is the subcommand. For commands that take free
                // text (send/exec), stop interpreting later tokens as global flags so a
                // payload containing --json/--workspace is preserved verbatim.
                if positionals.is_empty() && is_freeform_subcommand(value) {
                    freeform = true;
                }
                positionals.push(value.to_string());
                index += 1;
            }
        }
    }

    Ok(ControlOptions {
        workspace,
        json,
        args: positionals,
    })
}

/// The process exit code used when the CLI exits on an error (default 1).
/// `ctl run` stores the failed command's own exit code here so the `ctl run`
/// process mirrors it — scripts branch on `$?` directly instead of parsing
/// stdout/JSON (L2). Clamped to 1..=255 (0 would read as success; >255 wraps).
pub(crate) static CLI_EXIT_CODE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(1);

/// L4: commands with no (or fixed) arguments reject leftovers instead of
/// silently ignoring them — `ctl panes --jsonn` must error, not quietly print
/// the human list because of a typo'd flag.
pub(crate) fn ensure_no_extra_args(command: &str, extra: &[String]) -> Result<(), String> {
    if let Some(unexpected) = extra.first() {
        return Err(format!("unexpected argument for {command}: {unexpected}"));
    }
    Ok(())
}

pub(crate) fn is_freeform_subcommand(name: &str) -> bool {
    // `pane` is freeform so its whole tail reaches the alias re-dispatch
    // verbatim: the second parse then applies the real subcommand's rules,
    // instead of the FIRST parse eating `--json` out of a `pane send` payload
    // (L5 — the alias diverged from the direct command).
    matches!(
        name,
        "send" | "exec" | "broadcast" | "run" | "process" | "pane"
    )
}

/// `ctl write-config` — persist a config to the per-workspace config.json via
/// the daemon's WriteConfig request. The daemon validates and atomically
/// writes the config; the file-watch then live-reloads it and broadcasts a
/// ConfigChanged event. Accepts a file path or an inline JSON string (an
/// argument starting with `{` is treated as inline JSON).
pub(crate) fn control_write_config(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let config_value = if let Some(first) = args.first() {
        if first.starts_with('{') {
            // Inline JSON string.
            serde_json::from_str::<Value>(first)
                .map_err(|error| format!("invalid JSON config: {error}"))?
        } else {
            // File path.
            let content = fs::read_to_string(first)
                .map_err(|error| format!("failed to read config file {first}: {error}"))?;
            serde_json::from_str::<Value>(&content)
                .map_err(|error| format!("invalid JSON config in {first}: {error}"))?
        }
    } else {
        return Err("write-config requires a file path or inline JSON string".to_string());
    };

    let result: CommandOk = client.request(DaemonRequest::WriteConfig {
        config: config_value,
    })?;

    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "config written")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn control_pane_subcommand(mut options: ControlOptions) -> Result<(), String> {
    if options.args.len() < 2 {
        return Err("pane requires a subcommand".to_string());
    }

    let subcommand = options.args.remove(1);
    // `pane` is parsed freeform (L5), so a flag can land in the subcommand slot
    // (`ctl pane --json status`); reject it clearly — global flags go before
    // `pane` or after the real subcommand.
    if subcommand.starts_with('-') {
        return Err(format!(
            "pane requires a subcommand, got {subcommand} (put global flags before 'pane')"
        ));
    }
    options.args[0] = subcommand;
    run_control_cli_from_args(&prepend_ctl_args(options))
}

pub(crate) fn prepend_ctl_args(options: ControlOptions) -> Vec<String> {
    let mut args = vec!["sgian".to_string(), CTL_ARG.to_string()];
    args.push("--workspace".to_string());
    args.push(options.workspace.display().to_string());
    if options.json {
        args.push("--json".to_string());
    }
    args.extend(options.args);
    args
}

pub(crate) fn control_list_workspaces(json_output: bool) -> Result<(), String> {
    let root = app_support_dir().join("workspaces");
    let mut workspaces = Vec::new();

    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(key) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
            else {
                continue;
            };
            let Some(persisted) = fs::read_to_string(path.join(WORKSPACE_FILE))
                .ok()
                .and_then(|data| serde_json::from_str::<PersistedWorkspace>(&data).ok())
            else {
                continue;
            };
            workspaces.push(WorkspaceInfo {
                key,
                cwd: persisted.cwd,
                panes: persisted.panes.len(),
                active_pane_id: persisted.active_pane_id,
            });
        }
    }

    workspaces.sort_by(|left, right| left.cwd.cmp(&right.cwd));
    if json_output {
        write_json_stdout(&workspaces)
    } else {
        let mut stdout = std::io::stdout();
        for workspace in workspaces {
            writeln!(
                stdout,
                "{}\t{}\t{} panes",
                workspace.key, workspace.cwd, workspace.panes
            )
            .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        Ok(())
    }
}

pub(crate) fn control_list_panes(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;
    if json_output {
        return write_json_stdout(&list);
    }

    let mut stdout = std::io::stdout();
    for status in list.panes {
        let active = if list.active_pane_id.as_deref() == Some(status.pane.id.as_str()) {
            "*"
        } else {
            " "
        };
        writeln!(
            stdout,
            "{} {}\t{}\t{:?}",
            active, status.pane.id, status.pane.title, status.state
        )
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

/// Compatibility view used by the original parser tests. Provider-aware
/// parsing below deliberately preserves positional and `--name` title forms.
#[cfg(test)]
pub(crate) fn parse_new_pane_args(args: &[String]) -> Result<(bool, Option<String>), String> {
    let parsed = parse_new_pane_args_with_spec(args)?;
    Ok((parsed.agent, parsed.title))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedNewPaneArgs {
    pub(crate) agent: bool,
    pub(crate) title: Option<String>,
    pub(crate) backend: Option<AgentBackendKind>,
    pub(crate) model: Option<String>,
    pub(crate) profile: Option<String>,
    /// `--project NAME`: put the new pane in a project right away.
    pub(crate) project: Option<String>,
}

pub(crate) fn parse_new_pane_args_with_spec(args: &[String]) -> Result<ParsedNewPaneArgs, String> {
    let mut agent = false;
    let mut backend = None;
    let mut model = None;
    let mut profile = None;
    let mut project = None;
    let mut title_args = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" => {
                agent = true;
                index += 1;
            }
            "--project" => {
                if project.is_some() {
                    return Err("--project given more than once".to_string());
                }
                project = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--project requires a name".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--profile" => {
                if profile.is_some() {
                    return Err("--profile given more than once".to_string());
                }
                profile = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--profile requires a name".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--backend" => {
                if backend.is_some() {
                    return Err("--backend given more than once".to_string());
                }
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--backend requires claude or droid".to_string())?;
                backend = Some(match value.as_str() {
                    "claude" => AgentBackendKind::Claude,
                    "droid" => AgentBackendKind::Droid,
                    other => {
                        return Err(format!(
                            "unknown agent backend '{other}': expected claude or droid"
                        ))
                    }
                });
                agent = true;
                index += 2;
            }
            "--model" => {
                if model.is_some() {
                    return Err("--model given more than once".to_string());
                }
                model = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--model requires a model id".to_string())?
                        .clone(),
                );
                agent = true;
                index += 2;
            }
            "--name" | "-n" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--name requires a title".to_string())?;
                title_args.push(args[index].clone());
                title_args.push(value.clone());
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected pane option: {other}"));
            }
            _ => {
                // Preserve the long-standing `ctl new NAME` positional form.
                title_args.push(args[index].clone());
                index += 1;
            }
        }
    }
    let title = parse_name_option(&title_args)?;
    Ok(ParsedNewPaneArgs {
        agent,
        title,
        backend,
        model,
        profile,
        project,
    })
}

pub(crate) fn control_new_pane(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let mut parsed = parse_new_pane_args_with_spec(args)?;
    if let Some(profile_name) = parsed.profile.clone() {
        let config_value: Value = client.request(DaemonRequest::GetConfig)?;
        let config: Config = serde_json::from_value(config_value)
            .map_err(|error| format!("invalid config payload: {error}"))?;
        let profile = config
            .profile(&profile_name)
            .ok_or_else(|| format!("unknown profile '{profile_name}'"))?;
        if profile.is_agent_profile() {
            parsed.agent = true;
            if parsed.backend.is_none() {
                parsed.backend = match profile.agent_backend.as_deref() {
                    Some("claude") => Some(AgentBackendKind::Claude),
                    Some("droid") => Some(AgentBackendKind::Droid),
                    _ => None,
                };
            }
            if parsed.model.is_none() {
                parsed.model = profile.agent_model.clone();
            }
        }
        if parsed.title.is_none() {
            parsed.title = Some(profile_name);
        }
    }
    let pane: Pane = if parsed.agent {
        client.request(DaemonRequest::CreateAgentPaneWithSpec {
            title: parsed.title,
            backend: parsed.backend,
            model: parsed.model,
        })?
    } else {
        client.request(DaemonRequest::CreatePane {
            title: parsed.title,
            profile: parsed.profile,
        })?
    };
    if let Some(project) = parsed.project {
        client.request::<Value>(DaemonRequest::ProjectAssign {
            name: project,
            pane_id: pane.id.clone(),
        })?;
    }
    if json_output {
        write_json_stdout(&pane)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{}\t{}", pane.id, pane.title)
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn control_pane_status(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let pane_ref = args
        .first()
        .map(String::as_str)
        .unwrap_or("active")
        .to_string();
    let pane_id = resolve_pane_ref(client, &pane_ref)?;
    let status: PaneStatus = client.request(DaemonRequest::PaneStatus { pane_id })?;
    if json_output {
        write_json_stdout(&status)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(
            stdout,
            "{}\t{}\t{:?}",
            status.pane.id, status.pane.title, status.state
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Format the human-readable `ctl status --verbose` output (non-JSON form).
/// Includes all editable config fields (VAL-CFG-001): `shell`, `shell_args`,
/// `env` (key names only, values suppressed per VAL-SEC-010), `scrub_env`
/// (variable names by definition), `font_family`, `font_size`, `theme`,
/// `idle_shutdown_secs`, `restore_policy`, `agent_permission_mode`, and the
/// provider binary overrides. Extracted as a pure function so the
/// human-readable form is unit-testable directly.
pub(crate) fn format_status_verbose_human(status: &VerboseStatus) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "subscribers\t{}", status.subscribers);
    let _ = writeln!(out, "uptime\t{}s", status.uptime_secs);
    let _ = writeln!(out, "cwd\t{}", status.cwd);
    for pane in &status.panes {
        let _ = writeln!(
            out,
            "pane\t{}\t{}\t{:?}",
            pane.pane.id, pane.pane.title, pane.state
        );
    }
    let cfg = &status.config;
    // `env` is summarized by `Config::summary` as a map of key→null (values
    // suppressed, VAL-SEC-010). Render its key names only so the human-readable
    // form lists the configured env var names without echoing any values.
    let env_keys = env_key_names_csv(&cfg["env"]);
    let _ = writeln!(
        out,
        "config\tshell={}\tshell_args={}\tenv={}\tscrub_env={}\tfont_family={}\tfont_size={}\ttheme={}\tidle_shutdown_secs={}\trestore_policy={}\tagent_permission_mode={}\tagent_claude_bin={}\tagent_droid_bin={}",
        cfg["shell"],
        cfg["shell_args"],
        env_keys,
        cfg["scrub_env"],
        cfg["font_family"],
        cfg["font_size"],
        cfg["theme"],
        cfg["idle_shutdown_secs"],
        cfg["restore_policy"],
        cfg["agent_permission_mode"],
        cfg["agent_claude_bin"],
        cfg["agent_droid_bin"],
    );
    out
}

/// Render the `env` config-summary value as a comma-separated list of key names.
/// `Config::summary` emits `env` as a JSON object mapping each key to `null`
/// (values suppressed, VAL-SEC-010). This extracts just the key names for the
/// human-readable `status --verbose` line; if the field is absent or not an
/// object, renders `null` to mirror the JSON form. Returns an empty string for
/// an empty env map (no configured env vars).
pub(crate) fn env_key_names_csv(env: &Value) -> String {
    let Some(obj) = env.as_object() else {
        return "null".to_string();
    };
    if obj.is_empty() {
        return String::new();
    }
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

pub(crate) fn control_status_verbose(
    client: &DaemonClient,
    json_output: bool,
) -> Result<(), String> {
    let status: VerboseStatus = client.request(DaemonRequest::StatusVerbose)?;
    if json_output {
        return write_json_stdout(&status);
    }
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format_status_verbose_human(&status).as_bytes())
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    Ok(())
}

// ----- ctl: keyboard lease and ledger (docs/design/keyboard-lease-and-ledger.md) -----

/// The holder label `ctl` attributes its writes and lease claims to:
/// (M6) The holder this process acts as: with a client credential in the
/// environment, the credential's holder (the daemon would refuse any other);
/// else the self-declared default. One `whoami` round trip when credentialed.
pub(crate) fn effective_holder(client: &DaemonClient) -> String {
    if client_token_from_env().is_some() {
        if let Ok(identity) = client.request::<Value>(DaemonRequest::Whoami) {
            if let Some(holder) = identity["holder"].as_str().filter(|h| !h.is_empty()) {
                return holder.to_string();
            }
        }
    }
    default_holder()
}

/// `$SGIAN_HOLDER` when set and valid, else `user@host`.
pub(crate) fn default_holder() -> String {
    if let Ok(configured) = std::env::var("SGIAN_HOLDER") {
        if let Ok(holder) = validate_holder(&configured) {
            return holder;
        }
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "operator".to_string());
    let host = local_hostname();
    let short_host = host.split('.').next().unwrap_or("local");
    validate_holder(&format!("{user}@{short_host}")).unwrap_or_else(|_| "operator".to_string())
}

#[cfg(unix)]
pub(crate) fn local_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into a buffer we
    // own for the duration of the call and NUL-terminates on success; a
    // non-zero return leaves the contents unspecified, so we only read the
    // buffer when it returns 0 and stop at the first NUL.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "local".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() {
        "local".to_string()
    } else {
        name
    }
}

#[cfg(not(unix))]
pub(crate) fn local_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "local".to_string())
}

/// Pull `--as HOLDER` out of a send/broadcast argument list, stopping at `--`
/// like `parse_lf_flag` so a payload can still contain the literal text.
/// Pull `--generation N` out of a send argument list (before `--`).
pub(crate) fn parse_generation_flag(args: &[String]) -> Result<(Option<u64>, Vec<String>), String> {
    let mut generation = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if passthrough {
            remaining.push(arg.clone());
        } else if arg == "--generation" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--generation requires a number".to_string())?;
            generation = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --generation '{value}'"))?,
            );
            index += 1;
        } else {
            if arg == "--" {
                passthrough = true;
            }
            remaining.push(arg.clone());
        }
        index += 1;
    }
    Ok((generation, remaining))
}

pub(crate) fn parse_as_flag(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut holder = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if passthrough {
            remaining.push(arg.clone());
        } else if arg == "--as" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--as requires a HOLDER".to_string())?;
            holder = Some(validate_holder(value)?);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--as=") {
            holder = Some(validate_holder(value)?);
        } else {
            if arg == "--" {
                passthrough = true;
            }
            remaining.push(arg.clone());
        }
        index += 1;
    }
    Ok((holder, remaining))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaseVerb {
    Status,
    Take,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseArgs {
    pub(crate) verb: LeaseVerb,
    pub(crate) pane_ref: String,
    pub(crate) holder: Option<String>,
    pub(crate) force: bool,
    pub(crate) why: Option<String>,
    pub(crate) note: Option<String>,
    /// `--generation N`: refuse the release if the lease changed hands.
    pub(crate) generation: Option<u64>,
}

/// `lease [status|take|release] [PANE] [--as HOLDER] [--force --why REASON] [-m NOTE]`.
pub(crate) fn parse_lease_args(args: &[String]) -> Result<LeaseArgs, String> {
    let mut parsed = LeaseArgs {
        verb: LeaseVerb::Status,
        pane_ref: "active".to_string(),
        holder: None,
        force: false,
        why: None,
        note: None,
        generation: None,
    };
    let mut index = 0;
    let mut pane_seen = false;
    if let Some(first) = args.first() {
        match first.as_str() {
            "status" => {
                parsed.verb = LeaseVerb::Status;
                index = 1;
            }
            "take" => {
                parsed.verb = LeaseVerb::Take;
                index = 1;
            }
            "release" => {
                parsed.verb = LeaseVerb::Release;
                index = 1;
            }
            _ => {}
        }
    }
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |index: usize, flag: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg {
            "--as" => {
                parsed.holder = Some(validate_holder(&take_value(index, "--as")?)?);
                index += 1;
            }
            "--force" => parsed.force = true,
            "--why" => {
                parsed.why = Some(take_value(index, "--why")?);
                index += 1;
            }
            "-m" | "--note" => {
                parsed.note = Some(take_value(index, "-m/--note")?);
                index += 1;
            }
            "--generation" => {
                let value = take_value(index, "--generation")?;
                parsed.generation = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --generation '{value}'"))?,
                );
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for lease: {other}"));
            }
            other => {
                if pane_seen {
                    return Err(format!("unexpected argument for lease: {other}"));
                }
                parsed.pane_ref = other.to_string();
                pane_seen = true;
            }
        }
        index += 1;
    }
    if parsed.verb != LeaseVerb::Take && (parsed.force || parsed.why.is_some()) {
        return Err("--force/--why apply to `lease take`".to_string());
    }
    if parsed.verb == LeaseVerb::Release && parsed.note.is_none() {
        return Err("lease release requires -m NOTE (the hand-back note is mandatory)".to_string());
    }
    if parsed.verb != LeaseVerb::Release && parsed.note.is_some() {
        return Err("-m/--note applies to `lease release`".to_string());
    }
    Ok(parsed)
}

pub(crate) fn format_held_for(held_ms: Option<u64>) -> String {
    match held_ms {
        None => "-".to_string(),
        Some(ms) => {
            let secs = ms / 1000;
            if secs >= 3600 {
                format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
            } else if secs >= 60 {
                format!("{}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            }
        }
    }
}

pub(crate) fn print_lease_info(info: &LeaseInfo, json_output: bool) -> Result<(), String> {
    if json_output {
        return write_json_stdout(info);
    }
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "{}\t{}\t{}\t{}\twrites={}\trefused={}\tgen={}",
        info.pane_id,
        info.policy,
        info.holder.as_deref().unwrap_or("-"),
        format_held_for(info.held_ms),
        info.writes,
        info.refused_writes,
        info.generation
            .map(|generation| generation.to_string())
            .unwrap_or_else(|| "-".to_string())
    )
    .map_err(|error| format!("failed to write stdout: {error}"))
}

/// `ctl lease …` — show, take, or release a pane's keyboard lease. The
/// dispatcher parses first (pure) so status can use a read-only connection.
pub(crate) fn control_lease(
    client: &DaemonClient,
    parsed: LeaseArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let holder = parsed
        .holder
        .clone()
        .unwrap_or_else(|| effective_holder(client));
    let info: LeaseInfo = match parsed.verb {
        LeaseVerb::Status => client.request(DaemonRequest::LeaseStatus { pane_id })?,
        LeaseVerb::Take => client.request(DaemonRequest::TakeLease {
            pane_id,
            holder,
            force: parsed.force,
            why: parsed.why,
        })?,
        LeaseVerb::Release => client.request(DaemonRequest::ReleaseLease {
            pane_id,
            holder,
            note: parsed.note.unwrap_or_default(),
            generation: parsed.generation,
        })?,
    };
    print_lease_info(&info, json_output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LedgerArgs {
    pub(crate) pane_ref: String,
    pub(crate) limit: usize,
    pub(crate) verify: bool,
}

/// `ledger [PANE] [-n N] [--verify]`.
pub(crate) fn parse_ledger_args(args: &[String]) -> Result<LedgerArgs, String> {
    let mut parsed = LedgerArgs {
        pane_ref: "active".to_string(),
        limit: 0,
        verify: false,
    };
    let mut pane_seen = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--verify" => parsed.verify = true,
            "-n" | "--lines" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "-n requires a count".to_string())?;
                parsed.limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid -n count '{value}'"))?;
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for ledger: {other}"));
            }
            other => {
                if pane_seen {
                    return Err(format!("unexpected argument for ledger: {other}"));
                }
                parsed.pane_ref = other.to_string();
                pane_seen = true;
            }
        }
        index += 1;
    }
    Ok(parsed)
}

/// `ctl ledger …` — print or verify a pane's lease ledger. Reads the file
/// from the workspace data dir directly, so a CLOSED pane's ledger (its id
/// given literally) is still readable; only an open pane needs the daemon to
/// resolve a title.
pub(crate) fn control_ledger(
    client: &DaemonClient,
    parsed: LedgerArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = match resolve_pane_ref(client, &parsed.pane_ref) {
        Ok(pane_id) => pane_id,
        Err(error) => {
            if parsed.pane_ref.starts_with("pane-") {
                parsed.pane_ref.clone()
            } else {
                return Err(error);
            }
        }
    };
    let path = ledger_path(&client.data_dir.join(LEDGER_DIR), &pane_id);
    if parsed.verify {
        return match ledger_verify(&path) {
            Ok(summary) => {
                if json_output {
                    write_json_stdout(&json!({
                        "pane_id": pane_id,
                        "ok": true,
                        "records": summary.records,
                        "head": summary.head,
                    }))
                } else {
                    let mut stdout = std::io::stdout();
                    writeln!(
                        stdout,
                        "ok\t{}\t{} records\thead {}",
                        pane_id,
                        summary.records,
                        if summary.head.is_empty() {
                            "-"
                        } else {
                            summary.head.as_str()
                        }
                    )
                    .map_err(|error| format!("failed to write stdout: {error}"))
                }
            }
            Err(broken) => Err(format!(
                "ledger break in {} at line {}{}: {}",
                path.display(),
                broken.line,
                broken
                    .seq
                    .map(|seq| format!(" (seq {seq})"))
                    .unwrap_or_default(),
                broken.reason
            )),
        };
    }
    let records = read_ledger_tail(&path, parsed.limit);
    if json_output {
        return write_json_stdout(&records);
    }
    let mut stdout = std::io::stdout();
    for record in &records {
        writeln!(stdout, "{record}").map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchArgs {
    pub(crate) pane_ref: String,
    pub(crate) needle: String,
    pub(crate) ignore_case: bool,
    pub(crate) limit: usize,
}

/// `search <PANE> [-i] [-n N] [--] <NEEDLE...>` — flags before the needle;
/// `--` ends flag parsing so a needle can start with `-`.
pub(crate) fn parse_search_args(args: &[String]) -> Result<SearchArgs, String> {
    let mut parsed = SearchArgs {
        pane_ref: String::new(),
        needle: String::new(),
        ignore_case: false,
        limit: 0,
    };
    let mut index = 0;
    let mut needle_parts: Vec<String> = Vec::new();
    let mut passthrough = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if passthrough
            || !needle_parts.is_empty()
            || (arg != "--" && !arg.starts_with('-') && !parsed.pane_ref.is_empty())
        {
            needle_parts.push(arg.to_string());
        } else if arg == "--" {
            passthrough = true;
        } else if arg == "-i" || arg == "--ignore-case" {
            parsed.ignore_case = true;
        } else if arg == "-n" || arg == "--limit" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "-n requires a count".to_string())?;
            parsed.limit = value
                .parse::<usize>()
                .map_err(|_| format!("invalid -n count '{value}'"))?;
            index += 1;
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(format!("unexpected argument for search: {arg}"));
        } else {
            parsed.pane_ref = arg.to_string();
        }
        index += 1;
    }
    if parsed.pane_ref.is_empty() {
        return Err("search requires a pane and a needle".to_string());
    }
    parsed.needle = needle_parts.join(" ");
    if parsed.needle.trim().is_empty() {
        return Err("search requires a needle".to_string());
    }
    Ok(parsed)
}

pub(crate) fn control_search(
    client: &DaemonClient,
    parsed: SearchArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let result: Value = client.request(DaemonRequest::SearchScrollback {
        pane_id,
        needle: parsed.needle,
        ignore_case: parsed.ignore_case,
        limit: parsed.limit,
    })?;
    if json_output {
        return write_json_stdout(&result);
    }
    let mut stdout = std::io::stdout();
    for hit in result["matches"].as_array().into_iter().flatten() {
        writeln!(
            stdout,
            "{}\t{}",
            hit["line"].as_u64().unwrap_or(0),
            hit["text"].as_str().unwrap_or("")
        )
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    if result["truncated"].as_bool().unwrap_or(false) {
        writeln!(stdout, "(more hits; raise -n)")
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinesArgs {
    pub(crate) pane_ref: String,
    pub(crate) from: usize,
    pub(crate) to: usize,
}

/// `lines <PANE> <A>[:<B>]`.
pub(crate) fn parse_lines_args(args: &[String]) -> Result<LinesArgs, String> {
    let [pane_ref, range] = args else {
        return Err("lines requires a pane and a line range A[:B]".to_string());
    };
    let (from, to) = match range.split_once(':') {
        Some((a, b)) => (a, b),
        None => (range.as_str(), range.as_str()),
    };
    let from: usize = from
        .parse()
        .map_err(|_| format!("invalid line number '{from}'"))?;
    let to: usize = to
        .parse()
        .map_err(|_| format!("invalid line number '{to}'"))?;
    if from == 0 || to < from {
        return Err("line range must be 1-based with A <= B".to_string());
    }
    Ok(LinesArgs {
        pane_ref: pane_ref.clone(),
        from,
        to,
    })
}

pub(crate) fn control_lines(
    client: &DaemonClient,
    parsed: LinesArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let result: Value = client.request(DaemonRequest::ScrollbackLines {
        pane_id,
        from: parsed.from,
        to: parsed.to,
    })?;
    if json_output {
        return write_json_stdout(&result);
    }
    let mut stdout = std::io::stdout();
    let from = result["from"].as_u64().unwrap_or(1) as usize;
    for (offset, line) in result["lines"].as_array().into_iter().flatten().enumerate() {
        writeln!(stdout, "{}\t{}", from + offset, line.as_str().unwrap_or(""))
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectVerb {
    List,
    Show,
    New,
    Add,
    Rm,
    Delete,
    Ledger,
    Dossier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectArgs {
    pub(crate) verb: ProjectVerb,
    pub(crate) name: Option<String>,
    pub(crate) panes: Vec<String>,
    pub(crate) goal: Option<String>,
    pub(crate) repo: Option<String>,
    pub(crate) limit: usize,
    pub(crate) lines: usize,
    pub(crate) out: Option<String>,
}

/// `project list | show NAME | new NAME [--goal TEXT] [--repo PATH] |
/// add NAME PANE... | rm PANE... | delete NAME | ledger NAME [-n N] |
/// dossier NAME [--lines N] [--out FILE]`.
pub(crate) fn parse_project_args(args: &[String]) -> Result<ProjectArgs, String> {
    let verb = match args.first().map(String::as_str) {
        None | Some("list") => ProjectVerb::List,
        Some("show") => ProjectVerb::Show,
        Some("new") | Some("create") => ProjectVerb::New,
        Some("add") | Some("assign") => ProjectVerb::Add,
        Some("rm") | Some("remove") | Some("unassign") => ProjectVerb::Rm,
        Some("delete") => ProjectVerb::Delete,
        Some("ledger") => ProjectVerb::Ledger,
        Some("dossier") => ProjectVerb::Dossier,
        Some(other) => return Err(format!("unknown project command: {other}")),
    };
    let mut parsed = ProjectArgs {
        verb,
        name: None,
        panes: Vec::new(),
        goal: None,
        repo: None,
        limit: 0,
        lines: 0,
        out: None,
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut index = if args.is_empty() { 0 } else { 1 };
    while index < args.len() {
        match args[index].as_str() {
            "--goal" => {
                parsed.goal = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--goal requires TEXT".to_string())?,
                );
                index += 1;
            }
            "--repo" => {
                parsed.repo = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--repo requires PATH".to_string())?,
                );
                index += 1;
            }
            "-n" | "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "-n requires a count".to_string())?;
                parsed.limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid -n count '{value}'"))?;
                index += 1;
            }
            "--lines" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--lines requires a count".to_string())?;
                parsed.lines = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --lines count '{value}'"))?;
                if parsed.lines == 0 {
                    return Err("--lines must be at least 1".to_string());
                }
                index += 1;
            }
            "--out" => {
                parsed.out = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--out requires a FILE".to_string())?,
                );
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for project: {other}"));
            }
            other => positionals.push(other.to_string()),
        }
        index += 1;
    }
    match verb {
        ProjectVerb::List => {
            if !positionals.is_empty() {
                return Err("project list takes no arguments".to_string());
            }
        }
        ProjectVerb::Show
        | ProjectVerb::Delete
        | ProjectVerb::New
        | ProjectVerb::Ledger
        | ProjectVerb::Dossier => {
            if positionals.len() != 1 {
                return Err("expected exactly one project NAME".to_string());
            }
            parsed.name = positionals.pop();
        }
        ProjectVerb::Add => {
            if positionals.len() < 2 {
                return Err("project add needs a NAME and at least one PANE".to_string());
            }
            parsed.name = Some(positionals.remove(0));
            parsed.panes = positionals;
        }
        ProjectVerb::Rm => {
            if positionals.is_empty() {
                return Err("project rm needs at least one PANE".to_string());
            }
            parsed.panes = positionals;
        }
    }
    if verb != ProjectVerb::New && (parsed.goal.is_some() || parsed.repo.is_some()) {
        return Err("--goal/--repo apply to `project new`".to_string());
    }
    if verb != ProjectVerb::Ledger && parsed.limit != 0 {
        return Err("-n applies to `project ledger`".to_string());
    }
    if verb != ProjectVerb::Dossier && (parsed.lines != 0 || parsed.out.is_some()) {
        return Err("--lines/--out apply to `project dossier`".to_string());
    }
    Ok(parsed)
}

pub(crate) fn control_project(
    client: &DaemonClient,
    parsed: ProjectArgs,
    json_output: bool,
) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    match parsed.verb {
        ProjectVerb::List => {
            let summaries: Vec<ProjectSummary> = client.request(DaemonRequest::ProjectList)?;
            if json_output {
                return write_json_stdout(&summaries);
            }
            for summary in summaries {
                writeln!(
                    stdout,
                    "{}\tpanes={} live={} needs_input={} working={} idle={} unattended={} held={}{}",
                    summary.project.name,
                    summary.panes,
                    summary.live,
                    summary.needs_input,
                    summary.working,
                    summary.idle,
                    summary.unattended,
                    summary.held,
                    if summary.holders.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", summary.holders.join(", "))
                    }
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Show => {
            let name = parsed.name.unwrap_or_default();
            let detail: Value = client.request(DaemonRequest::ProjectShow { name })?;
            if json_output {
                return write_json_stdout(&detail);
            }
            let summary = &detail["summary"];
            writeln!(
                stdout,
                "{}\t{}",
                summary["project"]["name"].as_str().unwrap_or("-"),
                summary["project"]["goal"].as_str().unwrap_or("")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))?;
            for pane in detail["panes"].as_array().into_iter().flatten() {
                let agent = &pane["agent"];
                writeln!(
                    stdout,
                    "  {}\t{}\t{}\t{}\t{}\t{}{}",
                    pane["id"].as_str().unwrap_or("-"),
                    pane["title"].as_str().unwrap_or("-"),
                    pane["state"].as_str().unwrap_or("-"),
                    agent["agent"].as_str().unwrap_or("-"),
                    agent["attention"].as_str().unwrap_or("-"),
                    pane["holder"].as_str().unwrap_or("-"),
                    if agent["unattended"].as_bool().unwrap_or(false) {
                        "\tUNATTENDED"
                    } else {
                        ""
                    }
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::New => {
            let project: Project = client.request(DaemonRequest::ProjectCreate {
                name: parsed.name.unwrap_or_default(),
                goal: parsed.goal,
                repo: parsed.repo,
            })?;
            if json_output {
                return write_json_stdout(&project);
            }
            writeln!(stdout, "{}", project.name)
                .map_err(|error| format!("failed to write stdout: {error}"))
        }
        ProjectVerb::Add => {
            let name = parsed.name.unwrap_or_default();
            let mut results = Vec::new();
            for pane_ref in &parsed.panes {
                let pane_id = resolve_pane_ref(client, pane_ref)?;
                let result: Value = client.request(DaemonRequest::ProjectAssign {
                    name: name.clone(),
                    pane_id,
                })?;
                results.push(result);
            }
            if json_output {
                return write_json_stdout(&results);
            }
            for result in results {
                writeln!(
                    stdout,
                    "{}\t{}",
                    result["pane_id"].as_str().unwrap_or("-"),
                    result["project"].as_str().unwrap_or("-")
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Rm => {
            let mut results = Vec::new();
            for pane_ref in &parsed.panes {
                let pane_id = resolve_pane_ref(client, pane_ref)?;
                let result: Value = client.request(DaemonRequest::ProjectUnassign { pane_id })?;
                results.push(result);
            }
            if json_output {
                return write_json_stdout(&results);
            }
            for result in results {
                writeln!(
                    stdout,
                    "{}\t{}",
                    result["pane_id"].as_str().unwrap_or("-"),
                    result["project"].as_str().unwrap_or("-")
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Delete => {
            let project: Project = client.request(DaemonRequest::ProjectDelete {
                name: parsed.name.unwrap_or_default(),
            })?;
            if json_output {
                return write_json_stdout(&project);
            }
            writeln!(stdout, "{}\tdeleted", project.name)
                .map_err(|error| format!("failed to write stdout: {error}"))
        }
        ProjectVerb::Ledger => {
            let result: Value = client.request(DaemonRequest::ProjectLedger {
                name: parsed.name.unwrap_or_default(),
                limit: parsed.limit,
            })?;
            if json_output {
                return write_json_stdout(&result);
            }
            for record in result["records"].as_array().into_iter().flatten() {
                writeln!(stdout, "{record}")
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Dossier => {
            let result: Value = client.request(DaemonRequest::ProjectDossier {
                name: parsed.name.unwrap_or_default(),
                lines: parsed.lines,
            })?;
            match parsed.out {
                Some(path) => {
                    let pretty = serde_json::to_string_pretty(&result)
                        .map_err(|error| format!("failed to encode dossier: {error}"))?;
                    fs::write(&path, pretty.as_bytes())
                        .map_err(|error| format!("failed to write {path}: {error}"))?;
                    let panes = result["panes"].as_array().map_or(0, Vec::len);
                    let unverified = result["panes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|pane| pane["ledger"]["chain"]["verified"] == json!(false))
                        .count();
                    if json_output {
                        return write_json_stdout(&json!({
                            "project": result["summary"]["project"]["name"],
                            "file": path,
                            "panes": panes,
                            "unverified_ledgers": unverified,
                        }));
                    }
                    writeln!(
                        stdout,
                        "{}\t{}\t{} pane(s), {} ledger(s) failed verification",
                        result["summary"]["project"]["name"].as_str().unwrap_or("-"),
                        path,
                        panes,
                        unverified
                    )
                    .map_err(|error| format!("failed to write stdout: {error}"))
                }
                None => {
                    if json_output {
                        return write_json_stdout(&result);
                    }
                    let pretty = serde_json::to_string_pretty(&result)
                        .map_err(|error| format!("failed to encode dossier: {error}"))?;
                    writeln!(stdout, "{pretty}")
                        .map_err(|error| format!("failed to write stdout: {error}"))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KranzVerb {
    Status,
    Bind,
    Unbind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KranzArgs {
    pub(crate) verb: KranzVerb,
    pub(crate) pane_ref: String,
    pub(crate) repo: Option<String>,
}

/// `kranz [status|bind|unbind] [PANE] [--repo PATH]`.
pub(crate) fn parse_kranz_args(args: &[String]) -> Result<KranzArgs, String> {
    let mut parsed = KranzArgs {
        verb: KranzVerb::Status,
        pane_ref: "active".to_string(),
        repo: None,
    };
    let mut index = 0;
    match args.first().map(String::as_str) {
        Some("status") => index = 1,
        Some("bind") => {
            parsed.verb = KranzVerb::Bind;
            index = 1;
        }
        Some("unbind") => {
            parsed.verb = KranzVerb::Unbind;
            index = 1;
        }
        _ => {}
    }
    let mut pane_seen = false;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--repo requires a PATH".to_string())?;
                parsed.repo = Some(value.clone());
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for kranz: {other}"));
            }
            other => {
                if pane_seen {
                    return Err(format!("unexpected argument for kranz: {other}"));
                }
                parsed.pane_ref = other.to_string();
                pane_seen = true;
            }
        }
        index += 1;
    }
    if parsed.verb != KranzVerb::Bind && parsed.repo.is_some() {
        return Err("--repo applies to `kranz bind`".to_string());
    }
    if parsed.verb == KranzVerb::Status && pane_seen {
        return Err("kranz status lists every binding; it takes no PANE".to_string());
    }
    Ok(parsed)
}

pub(crate) fn control_kranz(
    client: &DaemonClient,
    parsed: KranzArgs,
    json_output: bool,
) -> Result<(), String> {
    let result: Value = match parsed.verb {
        KranzVerb::Status => client.request(DaemonRequest::KranzBindings)?,
        KranzVerb::Bind => {
            let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
            client.request(DaemonRequest::KranzBind {
                pane_id,
                repo: parsed.repo,
            })?
        }
        KranzVerb::Unbind => {
            let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
            client.request(DaemonRequest::KranzUnbind { pane_id })?
        }
    };
    if json_output {
        return write_json_stdout(&result);
    }
    let mut stdout = std::io::stdout();
    match parsed.verb {
        KranzVerb::Status => {
            let bindings: HashMap<String, KranzBinding> =
                serde_json::from_value(result).unwrap_or_default();
            let mut rows: Vec<_> = bindings.into_iter().collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            for (pane_id, binding) in rows {
                writeln!(
                    stdout,
                    "{pane_id}\t{}\t{}",
                    if binding.manual { "manual" } else { "auto" },
                    binding.repo
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        _ => writeln!(
            stdout,
            "{}\t{}",
            result["pane_id"].as_str().unwrap_or("-"),
            result["binding"]["repo"].as_str().unwrap_or("-")
        )
        .map_err(|error| format!("failed to write stdout: {error}")),
    }
}

pub(crate) fn control_send_input(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    // Help only as the FIRST token (L6): later positions are freeform payload
    // (`send active ls -h` must send "-h").
    if matches!(
        args.first().map(String::as_str),
        Some("--help") | Some("-h")
    ) {
        return print_control_help();
    }
    let (literal_lf, args) = parse_lf_flag(args);
    let (holder, args) = parse_as_flag(&args)?;
    let (generation, args) = parse_generation_flag(&args)?;
    if args.len() < 2 {
        return Err("send requires a pane and input".to_string());
    }

    // (T2) Route by pane kind: to an AGENT pane the text is a chat message
    // (SendAgentMessage — posted verbatim, with no Enter/CR translation and
    // no --lf effect); to a shell pane it stays PTY input.
    let status = resolve_pane_status(client, &args[0])?;
    if status.pane.kind == PaneKind::Agent {
        let text = args[1..].join(" ");
        client.request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: status.pane.id,
            text,
        })?;
        return Ok(());
    }
    let input = decode_cli_text(&args[1..].join(" "), literal_lf);
    match holder {
        Some(holder) => client.request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: status.pane.id,
            input,
            holder,
            generation,
        })?,
        None => client.request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: status.pane.id,
            input,
        })?,
    };
    Ok(())
}

/// (T2) `ctl interrupt <pane>` — interrupt an agent pane's current turn.
/// Shell panes have no turn to interrupt; signal them with `send` (Ctrl-C)
/// instead.
pub(crate) fn control_interrupt(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    if args.len() > 1 {
        return Err(format!("unexpected argument for interrupt: {}", args[1]));
    }
    let pane_ref = args.first().map(String::as_str).unwrap_or("active");
    let status = resolve_pane_status(client, pane_ref)?;
    if status.pane.kind != PaneKind::Agent {
        return Err(format!(
            "pane {} is not an agent pane; interrupt applies to agent panes \
             (for shells, send a Ctrl-C instead)",
            status.pane.id
        ));
    }
    client.request::<CommandOk>(DaemonRequest::InterruptAgent {
        pane_id: status.pane.id,
    })?;
    Ok(())
}

pub(crate) fn control_restart_pane(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let pane_ref = args
        .first()
        .map(String::as_str)
        .unwrap_or("active")
        .to_string();
    let pane_id = resolve_pane_ref(client, &pane_ref)?;
    let result: CommandOk = client.request(DaemonRequest::RestartPaneTerminal {
        pane_id: pane_id.clone(),
    })?;
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "restarted {pane_id}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Window of printed scrollback the attach dedupe anchors against. The
/// subscribe→file-read gap is milliseconds of output, so a small window is
/// ample and keeps the (one-time) anchor scan cheap.
pub(crate) const OVERLAP_WINDOW_BYTES: usize = 16 * 1024;
/// Minimum bytes an anchor match must cover before the skipper trusts it —
/// prevents a coincidental short match from suppressing real output.
pub(crate) const OVERLAP_MIN_ANCHOR_BYTES: usize = 32;

/// Dedupe the attach subscribe-then-read overlap (M9): output emitted between
/// the Subscribe registration and the GetScrollback file read is BOTH in the
/// printed scrollback and queued on the subscription, so the first streamed
/// chunks can repeat what was just printed. The skipper anchors the stream's
/// first chunk against a suffix of the printed tail (longest match, with a
/// minimum anchor length) and swallows the stream while it continues that
/// suffix byte-for-byte. Swallowed bytes are by construction identical to bytes
/// already printed, so a mis-anchor's worst case is suppressing output that is
/// indistinguishable from what is already on screen.
pub(crate) struct OverlapSkipper {
    /// Unmatched remainder of the printed tail; empty = dedupe finished.
    pub(crate) remaining: Vec<u8>,
    pub(crate) anchored: bool,
}

impl OverlapSkipper {
    pub(crate) fn new(printed: &[u8]) -> Self {
        let start = printed.len().saturating_sub(OVERLAP_WINDOW_BYTES);
        Self {
            remaining: printed[start..].to_vec(),
            anchored: false,
        }
    }

    /// Return the portion of `chunk` that should be printed.
    pub(crate) fn filter<'a>(&mut self, chunk: &'a [u8]) -> &'a [u8] {
        if self.remaining.is_empty() || chunk.is_empty() {
            self.remaining.clear();
            return chunk;
        }

        if !self.anchored {
            // Anchor: the duplicate stream is some SUFFIX of the printed tail.
            // Find the longest suffix the first chunk continues; require the
            // matched span to be long enough to not be a coincidence.
            for offset in 0..self.remaining.len() {
                let suffix = &self.remaining[offset..];
                let match_len = suffix.len().min(chunk.len());
                if match_len < OVERLAP_MIN_ANCHOR_BYTES {
                    break;
                }
                if suffix.len() >= chunk.len() {
                    if suffix.starts_with(chunk) {
                        self.anchored = true;
                        self.remaining = suffix[chunk.len()..].to_vec();
                        return &[];
                    }
                } else if chunk.starts_with(suffix) {
                    // The duplicate ends inside this chunk.
                    let skip = suffix.len();
                    self.remaining.clear();
                    return &chunk[skip..];
                }
            }
            // No trustworthy anchor: treat the stream as all-new from here on.
            self.remaining.clear();
            return chunk;
        }

        // Anchored: the stream must continue the remainder exactly; any
        // divergence ends the dedupe (print everything from here).
        if self.remaining.len() >= chunk.len() {
            if self.remaining.starts_with(chunk) {
                self.remaining.drain(..chunk.len());
                return &[];
            }
        } else if chunk.starts_with(self.remaining.as_slice()) {
            let skip = self.remaining.len();
            self.remaining.clear();
            return &chunk[skip..];
        }
        self.remaining.clear();
        chunk
    }
}

pub(crate) fn control_attach_pane(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    let pane_ref = args
        .first()
        .map(String::as_str)
        .unwrap_or("active")
        .to_string();
    let pane_id = resolve_pane_ref(client, &pane_ref)?;

    // Subscribe before the liveness check below so a pane that dies in between
    // still delivers its ended/closed event through the subscription (no missed-
    // event hang).
    let mut conn = client.connect()?;
    conn.write_request(&DaemonRequest::Subscribe)?;
    conn.await_subscribe_ack()?;
    // Attached output is legitimately sparse; drop the request read deadline.
    conn.set_read_timeout(None);

    // A dedicated request, not BootstrapWorkspace: bootstrap reads every pane's
    // scrollback and has side effects (first-boot shell spawning, persistence).
    let result: Value = client.request(DaemonRequest::GetScrollback {
        pane_id: pane_id.clone(),
    })?;
    let mut overlap = OverlapSkipper::new(&[]);
    if let Some(scrollback) = result["scrollback"].as_str().filter(|s| !s.is_empty()) {
        let mut stdout = std::io::stdout();
        stdout
            .write_all(scrollback.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("failed to write stdout: {error}"))?;
        // Output emitted between the Subscribe above and this file read is both
        // printed (in the scrollback) and queued (on the subscription) — dedupe
        // it against the printed tail instead of showing it twice (M9).
        overlap = OverlapSkipper::new(scrollback.as_bytes());
    }

    // A pane with no live session will never produce output or an ended event:
    // its scrollback is everything there is, so stop instead of blocking forever.
    let status: PaneStatus = client.request(DaemonRequest::PaneStatus {
        pane_id: pane_id.clone(),
    })?;
    if status.state != PaneRuntimeState::Live {
        return Ok(());
    }

    loop {
        match conn.read_event()? {
            Some(DaemonEvent::PtyOutput { pane_id: id, data }) if id == pane_id => {
                let printable = overlap.filter(data.as_bytes());
                if printable.is_empty() {
                    continue;
                }
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(printable)
                    .and_then(|_| stdout.flush())
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Some(DaemonEvent::PaneEnded { pane_id: id, .. })
            | Some(DaemonEvent::PaneClosed { pane_id: id })
                if id == pane_id =>
            {
                return Ok(());
            }
            Some(_) => {}
            None => return Ok(()),
        }
    }
}

/// The fully-parsed plan for `ctl logs` — options resolved up front. Pure (no
/// I/O, no daemon calls), so it is unit-testable in isolation like
/// `parse_exec_args`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LogsPlan {
    /// Limit output to the last N lines (`-n`/`--lines`). `None` = all lines.
    pub(crate) lines: Option<usize>,
    /// Stream new entries as they are written (`--follow`/`-f`).
    pub(crate) follow: bool,
}

/// Parse `ctl logs` flags into a `LogsPlan`. Pure: no daemon client, no I/O.
/// Flags: `-n N` / `--lines N` (limit to last N lines), `--follow` / `-f`
/// (stream new entries). No positional arguments are accepted.
pub(crate) fn parse_logs_args(args: &[String]) -> Result<LogsPlan, String> {
    let mut lines = None;
    let mut follow = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-n" | "--lines" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{} requires a line count", args[index]))?;
                lines = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid line count: {value}"))?,
                );
                index += 2;
            }
            "--follow" | "-f" => {
                follow = true;
                index += 1;
            }
            other => return Err(format!("unknown logs option: {other}")),
        }
    }
    Ok(LogsPlan { lines, follow })
}

/// Read the last `limit` lines (or all if `limit` is None) from a log file.
/// Returns lines in chronological order (oldest to newest among the tail).
/// Returns an empty vector if the file does not exist or is empty. Pure I/O
/// (no daemon interaction), so it is testable in isolation.
pub(crate) fn read_log_tail(log_path: &Path, limit: Option<usize>) -> Vec<String> {
    let content = fs::read_to_string(log_path).unwrap_or_default();
    let all_lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    let start = limit
        .map(|n| all_lines.len().saturating_sub(n))
        .unwrap_or(0);
    all_lines[start..].iter().map(|s| s.to_string()).collect()
}

/// The fully-parsed plan for `ctl exec` — every flag resolved up front so the
/// action phase never re-parses. Pure (no I/O, no daemon calls), so it is
/// unit-testable in isolation like `parse_control_options`.
#[derive(Debug)]
pub(crate) struct ExecPlan {
    pub(crate) create_new: bool,
    pub(crate) title: Option<String>,
    pub(crate) all: bool,
    pub(crate) panes_list: Option<String>,
    pub(crate) pane_ref: Option<String>,
    pub(crate) command: String,
}

/// Detect `--help` / `-h` only in the option-parsing window, i.e. tokens BEFORE
/// the first bare `--` separator. Tokens after `--` are the user's freeform
/// command payload and must never be intercepted as ctl flags.
pub(crate) fn has_help_flag(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            return false;
        }
        if arg == "--help" || arg == "-h" {
            return true;
        }
    }
    false
}

/// (07-19 CLI low) Validate a `--panes A,B` list: an explicitly-empty list
/// (`--panes ""`, `--panes ","`) splits to zero targets and would exit 0
/// vacuously — a usage error, not a success. Items are trimmed and empty items
/// dropped by the consumers, so at least one item must name a pane. Shared by
/// `parse_exec_args` and `parse_run_args`.
pub(crate) fn require_named_pane(list: &str) -> Result<(), String> {
    if list.split(',').any(|item| !item.trim().is_empty()) {
        Ok(())
    } else {
        Err("--panes requires at least one pane".to_string())
    }
}

/// Parse `ctl exec` flags into an `ExecPlan`. Pure: no daemon client, no I/O.
/// Flags: `--new`, `--all`, `--pane PANE`, `--panes A,B`, `--name NAME`/`-n NAME`,
/// `--` (end of options); the remaining tokens are the command, joined with spaces.
///
/// Targeting flags are mutually exclusive (the same contract `run` pins in
/// `parse_run_args`): `--all`/`--panes`/`--pane` reject each other, and
/// `--new`/`--name` — which only apply to the single-pane path — are rejected
/// under `--all`/`--panes` rather than silently dropped (07-19 CLI low).
pub(crate) fn parse_exec_args(args: &[String]) -> Result<ExecPlan, String> {
    let mut pane_ref: Option<String> = None;
    let mut create_new = false;
    let mut title: Option<String> = None;
    let mut all = false;
    let mut panes_list: Option<String> = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--new" => {
                create_new = true;
                index += 1;
            }
            "--all" => {
                all = true;
                index += 1;
            }
            "--pane" => {
                pane_ref = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--pane requires a pane".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--panes" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--panes requires a comma-separated list".to_string())?;
                require_named_pane(value)?;
                panes_list = Some(value.clone());
                index += 2;
            }
            "--name" | "-n" => {
                title = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--name requires a title".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown exec option: {value}"));
            }
            _ => break,
        }
    }

    // Reject conflicting targeting combinations (previously `--all` silently
    // overrode `--panes` overrode `--pane`, and `--new`/`--name` were silently
    // dropped under `--all`/`--panes`). Checked after the loop so the error is
    // order-independent.
    if all && panes_list.is_some() {
        return Err("--all cannot be combined with --panes".to_string());
    }
    if all && pane_ref.is_some() {
        return Err("--all cannot be combined with --pane".to_string());
    }
    if panes_list.is_some() && pane_ref.is_some() {
        return Err("--panes cannot be combined with --pane".to_string());
    }
    if (all || panes_list.is_some()) && create_new {
        return Err("--new cannot be combined with --all/--panes".to_string());
    }
    if (all || panes_list.is_some()) && title.is_some() {
        return Err("--name cannot be combined with --all/--panes".to_string());
    }

    if index >= args.len() {
        return Err("exec requires a command".to_string());
    }

    let command = args[index..].join(" ");
    Ok(ExecPlan {
        create_new,
        title,
        all,
        panes_list,
        pane_ref,
        command,
    })
}

pub(crate) fn control_exec(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let plan = parse_exec_args(args)?;
    let command = plan.command.clone();
    let line = format!("{command}\r");

    if plan.all {
        let result: Value = client.request(DaemonRequest::Broadcast { input: line })?;
        return report_exec_targets(&command, &result["panes"], json_output);
    }

    if let Some(list) = plan.panes_list {
        let mut targets = Vec::new();
        for pane_ref in list
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let pane_id = resolve_pane_ref(client, pane_ref)?;
            client.request::<CommandOk>(DaemonRequest::SendInput {
                pane_id: pane_id.clone(),
                input: line.clone(),
            })?;
            targets.push(pane_id);
        }
        return report_exec_targets(&command, &json!(targets), json_output);
    }

    let pane = if plan.create_new {
        let inferred_title = plan
            .title
            .or_else(|| command.split_whitespace().next().map(String::from));
        Some(client.request::<Pane>(DaemonRequest::CreatePane {
            title: inferred_title,
            profile: None,
        })?)
    } else {
        None
    };
    let pane_id = if let Some(pane) = pane.as_ref() {
        pane.id.clone()
    } else {
        resolve_pane_ref(client, plan.pane_ref.as_deref().unwrap_or("active"))?
    };

    client.request::<CommandOk>(DaemonRequest::SendInput {
        pane_id: pane_id.clone(),
        input: line,
    })?;

    if json_output {
        write_json_stdout(&json!({ "pane_id": pane_id, "command": command }))
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "sent to {pane_id}: {command}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn report_exec_targets(
    command: &str,
    panes: &Value,
    json_output: bool,
) -> Result<(), String> {
    if json_output {
        write_json_stdout(&json!({ "command": command, "panes": panes }))
    } else {
        let count = panes.as_array().map(|panes| panes.len()).unwrap_or(0);
        let mut stdout = std::io::stdout();
        writeln!(stdout, "sent to {count} pane(s): {command}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn control_broadcast(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    // Help only as the FIRST token (L6): later positions are freeform payload.
    if matches!(
        args.first().map(String::as_str),
        Some("--help") | Some("-h")
    ) {
        return print_control_help();
    }
    if args.is_empty() {
        return Err("broadcast requires text".to_string());
    }
    let (literal_lf, args) = parse_lf_flag(args);
    if args.is_empty() {
        return Err("broadcast requires text".to_string());
    }
    let input = decode_cli_text(&args.join(" "), literal_lf);
    let result: Value = client.request(DaemonRequest::Broadcast { input })?;
    let count = result["panes"]
        .as_array()
        .map(|panes| panes.len())
        .unwrap_or(0);
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "broadcast to {count} pane(s)")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn control_sync_input(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let enabled = match args.first().map(String::as_str) {
        Some("on") | Some("true") | Some("1") => true,
        Some("off") | Some("false") | Some("0") => false,
        _ => return Err("sync requires 'on' or 'off'".to_string()),
    };
    let result: Value = client.request(DaemonRequest::SetSyncInput { enabled })?;
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(
            stdout,
            "synchronize-input {}",
            if enabled { "on" } else { "off" }
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// The fully-parsed plan for `ctl run` — pure (no I/O, no daemon calls), unit-tested
/// like `parse_control_options` and `parse_exec_args`.
///
/// `all` and `panes_list` select the batched (waiting, per-pane exit-code-collecting)
/// variant: `--all` targets every live pane; `--panes A,B` targets a named subset.
/// These are mutually exclusive with each other and with `--pane`.
#[derive(Debug)]
pub(crate) struct RunPlan {
    pub(crate) pane_ref: String,
    /// The command tokens, RAW. Quoting happens only after the target shell's
    /// family is resolved (`quote_command_for`): POSIX and fish have different
    /// single-quote escape rules, and quoting for the wrong family corrupts
    /// backslash-bearing args or unbalances the wrapper entirely (H7).
    pub(crate) command_args: Vec<String>,
    pub(crate) all: bool,
    pub(crate) panes_list: Option<String>,
    /// Overall deadline for the run (`--timeout MS`). None = wait indefinitely
    /// (the historical contract); any marker miss then hangs, so scripts should
    /// pass one.
    pub(crate) timeout_ms: Option<u64>,
}

/// The shell family determines the status-variable idiom used in the `ctl run`
/// wrapper. POSIX shells (sh/bash/zsh/dash) use `$?`; fish uses `$status`.
/// Emitting the wrong idiom causes `ctl run` to hang (the marker never prints)
/// because the variable doesn't exist in the other family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellFamily {
    Posix,
    Fish,
}

/// Detect the shell family from the configured shell path. The check is based
/// on the basename: if it is `fish` (or starts with `fish`, e.g.
/// `/opt/homebrew/bin/fish`), the family is `Fish`; otherwise `Posix`. This is
/// a pure function with no I/O.
pub(crate) fn detect_shell_family(shell: &str) -> ShellFamily {
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell);
    if basename == "fish" || basename.starts_with("fish") {
        ShellFamily::Fish
    } else {
        ShellFamily::Posix
    }
}

/// Build the `ctl run` wrapper line that sends the command followed by a
/// unique marker carrying the shell's exit-status variable. The status
/// variable idiom depends on the shell family: POSIX (sh/bash/zsh) uses
/// `"$?"`, while Fish uses `$status` (fish has no `$?`; using it would hang
/// the pane). The marker is unique (pid + timestamp) so the command's own
/// output cannot cross-match it (VAL-ORCH-007). The `\r` at the end submits
/// the line.
pub(crate) fn build_run_wrapper(command: &str, marker: &str, family: ShellFamily) -> String {
    let status_var = match family {
        ShellFamily::Posix => "\"$?\"",
        ShellFamily::Fish => "$status",
    };
    format!("{command}; printf '\\n{marker}:%s\\n' {status_var}\r")
}

/// Characters that need no quoting in either shell family: letters, digits,
/// and punctuation with no special meaning. Deliberately excludes whitespace,
/// glob chars (*?[]), redirection (<>&|;), expansion ($`), quotes, comment (#),
/// and backslash.
///
/// `=` is safe only MID-word (`FOO=bar` keeps its literal assignment shape):
/// at word start zsh performs equals-expansion (`=foo` expands to the path of
/// the `foo` command), so a leading `=` must force quoting (07-19 CLI low).
pub(crate) fn shell_arg_is_safe(arg: &str) -> bool {
    !arg.is_empty()
        && arg.chars().enumerate().all(|(position, c)| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | ',' | '+')
                || (c == '=' && position > 0)
        })
}

/// Shell-quote a single argument for POSIX shells (sh/bash/zsh). Safe args are
/// returned verbatim; anything else is wrapped in single quotes with embedded
/// single quotes escaped via the standard `'\''` idiom. This preserves the
/// argument's identity when the wrapper line is re-parsed by the pane's shell.
pub(crate) fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if shell_arg_is_safe(arg) {
        return arg.to_string();
    }
    let escaped = arg.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Shell-quote a single argument for fish. Inside fish single quotes only `\`
/// and `'` are special, each escaped with a backslash. The POSIX `'\''` idiom
/// is wrong here — fish would interpret `\'` inside the quotes, silently
/// corrupting backslash-bearing args, and an arg ending in `\` would unbalance
/// the wrapper so the exit marker never prints (H7).
pub(crate) fn fish_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if shell_arg_is_safe(arg) {
        return arg.to_string();
    }
    let escaped = arg.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// Quote one argument for the resolved shell family.
pub(crate) fn shell_quote_for(family: ShellFamily, arg: &str) -> String {
    match family {
        ShellFamily::Posix => shell_quote(arg),
        ShellFamily::Fish => fish_quote(arg),
    }
}

/// Join raw command tokens into the wrapper's command string, quoting each for
/// the resolved shell family so the pane's shell re-parses the user's original
/// argument grouping intact (e.g. `run -- sh -c 'exit 7'` keeps `exit 7` as a
/// single argument to `-c`).
pub(crate) fn quote_command_for(family: ShellFamily, args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote_for(family, arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse `ctl run` flags into a `RunPlan`. Pure: no daemon client, no I/O.
/// Flags: `--pane PANE` (default `active`), `--all`, `--panes A,B`,
/// `--timeout MS` (overall deadline for the run; omitted = wait indefinitely),
/// `--` (end of options); the remaining tokens are the command, kept RAW here —
/// they are quoted for the resolved shell family later (`quote_command_for`),
/// since POSIX and fish quoting rules differ (H7).
///
/// `--all` and `--panes` select the batched variant (waits for each pane and
/// reports per-pane exit codes). They are mutually exclusive with `--pane` and
/// with each other.
pub(crate) fn parse_run_args(args: &[String]) -> Result<RunPlan, String> {
    let mut pane_ref = "active".to_string();
    let mut all = false;
    let mut panes_list: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                if all || panes_list.is_some() {
                    return Err("--pane cannot be combined with --all/--panes".to_string());
                }
                pane_ref = args
                    .get(index + 1)
                    .ok_or_else(|| "--pane requires a pane".to_string())?
                    .clone();
                index += 2;
            }
            "--all" => {
                if panes_list.is_some() || pane_ref != "active" {
                    return Err("--all cannot be combined with --pane/--panes".to_string());
                }
                all = true;
                index += 1;
            }
            "--panes" => {
                if all || pane_ref != "active" {
                    return Err("--panes cannot be combined with --pane/--all".to_string());
                }
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--panes requires a comma-separated list".to_string())?;
                require_named_pane(value)?;
                panes_list = Some(value.clone());
                index += 2;
            }
            "--timeout" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--timeout requires a value".to_string())?;
                timeout_ms = Some(value.parse::<u64>().map_err(|_| {
                    format!("--timeout requires a non-negative integer (ms), got {value:?}")
                })?);
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown run option: {value}"));
            }
            _ => break,
        }
    }
    if index >= args.len() {
        return Err("run requires a command".to_string());
    }
    Ok(RunPlan {
        pane_ref,
        command_args: args[index..].to_vec(),
        all,
        panes_list,
        timeout_ms,
    })
}

/// Cap on captured PTY output retained in a `PaneRunResult` (ENHANCEMENTS §3).
/// Large enough for a useful failure tail; small enough for batched JSON.
pub(crate) const RUN_RESULT_TAIL_BYTES: usize = 4096;

/// The result of running a command in a single pane within a batched run.
/// `exit_code` is `Some(code)` when the marker was captured; `None` means the
/// pane failed before a code could be collected (not live, ended, closed, or
/// the daemon dropped the stream). `success` is true only for exit code 0.
/// `timed_out` is true when the wait hit `--timeout`. `tail` is a bounded
/// capture of PTY output (UTF-8 trimmed) for automation; `elapsed_ms` is wall
/// time from subscribe through resolution.
#[derive(Debug, Clone)]
pub(crate) struct PaneRunResult {
    pub(crate) pane_id: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) success: bool,
    pub(crate) error: Option<String>,
    pub(crate) timed_out: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) tail: String,
}

impl PaneRunResult {
    pub(crate) fn ok(pane_id: &str, code: i32, elapsed_ms: u64, tail: String) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            exit_code: Some(code),
            success: code == 0,
            error: None,
            timed_out: false,
            elapsed_ms,
            tail,
        }
    }

    pub(crate) fn failed(pane_id: &str, reason: String, elapsed_ms: u64, tail: String) -> Self {
        let timed_out = reason.starts_with("timed out after ");
        Self {
            pane_id: pane_id.to_string(),
            exit_code: None,
            success: false,
            error: Some(reason),
            timed_out,
            elapsed_ms,
            tail,
        }
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "pane": self.pane_id,
            "exit_code": self.exit_code,
            "success": self.success,
            "timed_out": self.timed_out,
            "elapsed_ms": self.elapsed_ms,
            "tail": self.tail,
            "error": self.error,
        })
    }
}

/// Run a command in a single pane and collect its exit code (or failure reason).
/// Subscribes to the event stream, sends the wrapper line, and watches for the
/// marker / `PaneEnded` / `PaneClosed`. Returns a `PaneRunResult`; never hangs
/// past the pane's natural completion (the caller may wrap this in a timeout).
/// The `marker_suffix` disambiguates concurrent per-pane markers in the same
/// workspace (each pane gets a distinct marker so sync-input mirroring or
/// shared output cannot cross-match).
pub(crate) fn run_in_pane(
    client: &DaemonClient,
    pane_id: &str,
    command: &str,
    family: ShellFamily,
    marker_suffix: &str,
    timeout: Option<Duration>,
) -> PaneRunResult {
    let started = Instant::now();
    let elapsed_ms = || started.elapsed().as_millis() as u64;
    let deadline = timeout.map(|t| Instant::now() + t);
    let timed_out = |pane_id: &str, tail: String| {
        PaneRunResult::failed(
            pane_id,
            format!(
                "timed out after {}ms waiting for the exit marker",
                timeout.map(|t| t.as_millis()).unwrap_or_default()
            ),
            elapsed_ms(),
            tail,
        )
    };
    let setup_timed_out = || deadline.map(|d| Instant::now() >= d).unwrap_or(false);
    // Remaining time for a setup RPC. When the run has no overall timeout,
    // use the default client read deadline; otherwise use time left (failing
    // immediately if the deadline already elapsed).
    let setup_budget = || -> Result<Option<Duration>, PaneRunResult> {
        match deadline {
            None => Ok(Some(CLIENT_READ_TIMEOUT)),
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err(timed_out(pane_id, String::new()))
                } else {
                    Ok(Some(remaining))
                }
            }
        }
    };
    // Fail fast on a dead pane: otherwise we'd send into a void and wait
    // forever for a marker that never prints.
    let status: PaneStatus = match setup_budget().and_then(|budget| {
        client
            .request_with_timeout(
                DaemonRequest::PaneStatus {
                    pane_id: pane_id.to_string(),
                },
                budget,
            )
            .map_err(|error| {
                if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
                    timed_out(pane_id, String::new())
                } else {
                    PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new())
                }
            })
    }) {
        Ok(s) => s,
        Err(result) => return result,
    };
    if status.state != PaneRuntimeState::Live {
        return PaneRunResult::failed(
            pane_id,
            "pane is not live".to_string(),
            elapsed_ms(),
            String::new(),
        );
    }

    // Subscribe before sending so the marker can't be missed.
    let mut conn = match setup_budget().and_then(|budget| {
        client.connect_with_timeout(budget).map_err(|error| {
            if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
                timed_out(pane_id, String::new())
            } else {
                PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new())
            }
        })
    }) {
        Ok(c) => c,
        Err(result) => return result,
    };
    match setup_budget() {
        Ok(Some(remaining)) => conn.set_read_timeout(Some(remaining)),
        Ok(None) => {}
        Err(result) => return result,
    }
    if let Err(error) = conn.write_request(&DaemonRequest::Subscribe) {
        return PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new());
    }
    // Wait for the registration ack (when the daemon supports it) BEFORE the
    // SendInput below: the marker's output can then never be broadcast before
    // this subscriber joined (M8).
    if let Err(error) = conn.await_subscribe_ack() {
        if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
            return timed_out(pane_id, String::new());
        }
        return PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new());
    }
    // A command's output gaps are unbounded; the run deadline (if any) governs,
    // not the per-read request timeout.
    conn.set_read_timeout(None);

    // pid + timestamp + per-pane suffix: unique per invocation, so command
    // output, old scrollback, or a concurrent run cannot cross-match it (L1).
    let marker = format!(
        "__sgian_rc_{}_{}_{}_{}",
        std::process::id(),
        now_millis(),
        marker_suffix,
        pane_id
    );
    let prefix = format!("{marker}:");
    let wrapped = build_run_wrapper(command, &marker, family);
    if let Err(result) = setup_budget().and_then(|budget| {
        client
            .request_with_timeout::<CommandOk>(
                DaemonRequest::SendInput {
                    pane_id: pane_id.to_string(),
                    input: wrapped,
                },
                budget,
            )
            .map_err(|error| {
                if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
                    timed_out(pane_id, String::new())
                } else {
                    PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new())
                }
            })
            .map(|_| ())
    }) {
        return result;
    }

    // `output` is the marker-search window (kept tiny); `tail` is the bounded
    // capture returned to automation (ENHANCEMENTS §3).
    let mut output = String::new();
    let mut tail = String::new();
    loop {
        // Enforce the overall deadline (H7): bound each read by the remaining
        // time so a marker miss (foreign shell, foreground vim, quoting bug)
        // surfaces as a clean timeout instead of an infinite hang.
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return timed_out(pane_id, tail);
            }
            conn.set_read_timeout(Some(remaining));
        }
        match conn.read_event() {
            Ok(Some(DaemonEvent::PtyOutput { pane_id: id, data })) if id == pane_id => {
                output.push_str(&data);
                tail.push_str(&data);
                trim_to_tail(&mut tail, RUN_RESULT_TAIL_BYTES);
                if let Some(code) = parse_exit_marker(&output, &prefix) {
                    return PaneRunResult::ok(pane_id, code, elapsed_ms(), tail);
                }
                trim_to_tail(&mut output, prefix.len() + 64);
            }
            Ok(Some(DaemonEvent::PaneEnded { pane_id: id, .. })) if id == pane_id => {
                return PaneRunResult::failed(
                    pane_id,
                    "pane ended before the command finished".to_string(),
                    elapsed_ms(),
                    tail,
                );
            }
            Ok(Some(DaemonEvent::PaneClosed { pane_id: id })) if id == pane_id => {
                return PaneRunResult::failed(
                    pane_id,
                    "pane closed before the command finished".to_string(),
                    elapsed_ms(),
                    tail,
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return PaneRunResult::failed(
                    pane_id,
                    "daemon closed before the command finished".to_string(),
                    elapsed_ms(),
                    tail,
                )
            }
            Err(error) => {
                // Check the deadline FIRST: a read error at/after it is the
                // timeout firing, not a transport failure.
                if read_error_is_timeout(deadline, Instant::now()) {
                    return timed_out(pane_id, tail);
                }
                return PaneRunResult::failed(pane_id, error, elapsed_ms(), tail);
            }
        }
    }
}

/// Slack for early-firing read timers when mapping a read error onto the run
/// deadline (`read_error_is_timeout`).
pub(crate) const READ_TIMER_EPSILON: Duration = Duration::from_millis(5);

/// (07-19 CLI low) Should a read error in the `run_in_pane` loop surface as
/// the clean "timed out after Nms" deadline error? The per-read timeout is
/// armed to the time REMAINING to the deadline, but the OS timer can fire a
/// hair EARLY — a strict `now >= deadline` check would then surface a raw IO
/// error sub-milliseconds before the deadline passes. Compare with a small
/// epsilon so the clean timeout always wins in that window. A genuine IO
/// error with the deadline further than the epsilon away is reported raw.
/// Pure for tests.
pub(crate) fn read_error_is_timeout(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| now + READ_TIMER_EPSILON >= deadline)
}

/// Resolve the effective shell family from the daemon's config. Used by both
/// the single-pane and batched `ctl run` paths to pick the status-variable
/// idiom (`$?` vs `$status`).
pub(crate) fn resolve_shell_family(client: &DaemonClient) -> Result<ShellFamily, String> {
    let verbose: VerboseStatus = client.request(DaemonRequest::StatusVerbose)?;
    Ok(detect_shell_family(&status_shell_for_family(
        &verbose.config,
    )))
}

/// Pick the shell to family-detect from a status config payload (M10): prefer
/// the daemon-RESOLVED shell (what the daemon would actually spawn) — a ctl
/// whose environment differs from the daemon's must not re-resolve $SHELL
/// locally and diverge. Old daemons don't emit `resolved_shell`; fall back to
/// the raw configured `shell`, then the local default. Pure for tests.
pub(crate) fn status_shell_for_family(config: &Value) -> String {
    config
        .get("resolved_shell")
        .and_then(Value::as_str)
        .or_else(|| config.get("shell").and_then(Value::as_str))
        .map(ToString::to_string)
        .unwrap_or_else(default_shell)
}

/// Run a command in a pane and report its exit code (best-effort). Wraps the command so
/// the shell prints a unique marker plus the shell's status variable, then
/// watches the pane's output for it. Detects the shell family from the
/// daemon's effective config so non-POSIX shells (fish, which uses `$status`
/// instead of `$?`) report the correct code instead of hanging.
///
/// With `--all` or `--panes A,B`, dispatches to the batched variant that runs
/// the command in multiple panes concurrently, collects each pane's exit code,
/// and reports per-pane results (VAL-ORCH-008..013, 028, 030).
/// A parsed `ctl wait` invocation. `pane_ref` is resolved to a pane id by the caller;
/// `condition` is the single required wait condition; `timeout_ms` bounds the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WaitArgs {
    pub(crate) pane_ref: String,
    pub(crate) condition: WaitCondition,
    pub(crate) timeout_ms: Option<u64>,
}

/// Parse `ctl wait <pane> (--text S | --regex RE | --idle MS | --exit) [--timeout MS]`.
/// Pure: no daemon client, no I/O. Enforces EXACTLY one condition (a missing or
/// conflicting condition is a usage error, never an open-ended wait) and a numeric
/// `--idle`/`--timeout` (VAL-PRIM-016 / VAL-PRIM-017).
///
/// `--` escaping (07-19 CLI low): `--help` is otherwise intercepted as a flag
/// (by `parse_control_options` / `has_help_flag`), so there was no way to match
/// the literal text `--help`. A `--` directly after a value-taking flag escapes
/// that flag's value (`--text -- --help` matches the literal text); a
/// standalone `--` ends flag recognition so a later token is a literal pane
/// reference even when it starts with `-`.
pub(crate) fn parse_wait_args(args: &[String]) -> Result<WaitArgs, String> {
    pub(crate) fn set_condition(
        slot: &mut Option<WaitCondition>,
        cond: WaitCondition,
    ) -> Result<(), String> {
        if slot.is_some() {
            return Err("wait accepts only one of --text/--regex/--idle/--exit".to_string());
        }
        *slot = Some(cond);
        Ok(())
    }

    let mut pane_ref: Option<String> = None;
    let mut condition: Option<WaitCondition> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut index = 0;
    let mut literal = false;

    while index < args.len() {
        let arg = args[index].as_str();
        if literal {
            // After a standalone `--`: only the pane reference is still open.
            if pane_ref.is_some() {
                return Err(format!(
                    "wait accepts a single pane reference; unexpected argument: {arg}"
                ));
            }
            pane_ref = Some(arg.to_string());
            index += 1;
            continue;
        }
        match arg {
            "--" => {
                literal = true;
                index += 1;
            }
            "--text" | "--regex" | "--idle" | "--timeout" => {
                // A `--` in the value slot escapes the NEXT token as the value.
                let value_index = if args.get(index + 1).map(String::as_str) == Some("--") {
                    index + 2
                } else {
                    index + 1
                };
                let value = args
                    .get(value_index)
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                match arg {
                    "--text" => set_condition(&mut condition, WaitCondition::Text(value.clone()))?,
                    "--regex" => {
                        set_condition(&mut condition, WaitCondition::Regex(value.clone()))?
                    }
                    "--idle" => {
                        let ms = value.parse::<u64>().map_err(|_| {
                            format!("--idle requires a non-negative integer (ms), got {value:?}")
                        })?;
                        set_condition(&mut condition, WaitCondition::Idle(ms))?;
                    }
                    "--timeout" => {
                        timeout_ms = Some(value.parse::<u64>().map_err(|_| {
                            format!("--timeout requires a non-negative integer (ms), got {value:?}")
                        })?);
                    }
                    _ => unreachable!(),
                }
                index = value_index + 1;
            }
            "--exit" => {
                set_condition(&mut condition, WaitCondition::Exit)?;
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown wait option: {other}"));
            }
            other => {
                if pane_ref.is_some() {
                    return Err(format!(
                        "wait accepts a single pane reference; unexpected argument: {other}"
                    ));
                }
                pane_ref = Some(other.to_string());
                index += 1;
            }
        }
    }

    let pane_ref = pane_ref.ok_or_else(|| "wait requires a pane id".to_string())?;
    let condition =
        condition.ok_or_else(|| "wait requires one of --text/--regex/--idle/--exit".to_string())?;
    Ok(WaitArgs {
        pane_ref,
        condition,
        timeout_ms,
    })
}

/// `ctl wait <pane> (--text S | --regex RE | --idle MS | --exit) [--timeout MS] [--json]`
/// — block until the condition is met or the timeout elapses. Prints the result
/// (`--json` → the documented `{matched,reason,revision,exit_code?,elapsed_ms}` object;
/// otherwise a short human summary) and maps the outcome to the process exit code: a
/// match exits 0, a timeout exits non-zero, so scripts can branch on `$?`
/// (VAL-PRIM-012 / VAL-PRIM-013) — mirroring `ctl run`'s exit-code mapping.
pub(crate) fn control_wait(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_wait_args(args)?;
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    // A wait's response legitimately takes as long as the wait itself: scale the
    // read deadline to the wait's own timeout (plus the normal request margin),
    // or clear it entirely for an untimed wait (the documented block-forever
    // contract) — the default CLIENT_READ_TIMEOUT would cut long waits short.
    let mut conn = client.connect()?;
    conn.set_read_timeout(
        parsed
            .timeout_ms
            .map(|ms| Duration::from_millis(ms) + CLIENT_READ_TIMEOUT),
    );
    let response = conn.request(&DaemonRequest::Wait {
        pane_id,
        condition: parsed.condition,
        timeout_ms: parsed.timeout_ms,
    })?;
    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "daemon request failed".to_string()));
    }
    let outcome: Value = response.result;

    let matched = outcome
        .get("matched")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = outcome
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if json_output {
        write_json_stdout(&outcome)?;
    } else {
        let mut stdout = std::io::stdout();
        let line = if matched {
            match reason.as_str() {
                "exit" => match outcome.get("exit_code").and_then(Value::as_i64) {
                    Some(code) => format!("matched: exit (code {code})"),
                    None => "matched: exit".to_string(),
                },
                other => format!("matched: {other}"),
            }
        } else {
            "timeout".to_string()
        };
        writeln!(stdout, "{line}").map_err(|error| format!("failed to write stdout: {error}"))?;
    }

    // Scriptable process exit: a match exits 0, a timeout exits non-zero (VAL-PRIM-013).
    if matched {
        Ok(())
    } else {
        Err(format!("wait did not match (reason: {reason})"))
    }
}

/// A parsed `ctl snapshot` invocation: just the pane reference (the global `--json`
/// flag is consumed by `parse_control_options` before the args reach here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapshotArgs {
    pub(crate) pane_ref: String,
}

/// Parse `ctl snapshot <pane>`. Pure: no daemon client, no I/O. Requires exactly one
/// pane reference; a missing pane, an extra positional, or an unknown option is a usage
/// error. A standalone `--` ends flag recognition so a pane titled like a flag
/// stays addressable (`snapshot -- --titled`) (07-19 CLI low).
pub(crate) fn parse_snapshot_args(args: &[String]) -> Result<SnapshotArgs, String> {
    let mut pane_ref: Option<String> = None;
    let mut literal = false;
    for arg in args {
        match arg.as_str() {
            other if literal => {
                if pane_ref.is_some() {
                    return Err(format!(
                        "snapshot accepts a single pane reference; unexpected argument: {other}"
                    ));
                }
                pane_ref = Some(other.to_string());
            }
            "--" => literal = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown snapshot option: {other}"));
            }
            other => {
                if pane_ref.is_some() {
                    return Err(format!(
                        "snapshot accepts a single pane reference; unexpected argument: {other}"
                    ));
                }
                pane_ref = Some(other.to_string());
            }
        }
    }
    let pane_ref = pane_ref.ok_or_else(|| "snapshot requires a pane id".to_string())?;
    Ok(SnapshotArgs { pane_ref })
}

/// A parsed `ctl find` invocation: the optional metadata filters (the global `--json`
/// flag is consumed by `parse_control_options` before the args reach here).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FindArgs {
    pub(crate) command: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) state: Option<PaneRuntimeState>,
}

/// Parse `ctl find [--command S] [--title S] [--cwd S] [--state live|ended]`. Pure: no
/// daemon client, no I/O. Every filter is optional (a bare `find` matches all panes);
/// each value-taking flag requires its value; `--state` accepts only `live`/`ended`
/// (any other value is a usage error — VAL-PRIM-039); positionals and unknown flags are
/// usage errors.
///
/// `--` escaping (07-19 CLI low), same shape as `parse_wait_args`: a `--`
/// directly after a value-taking flag escapes that flag's value
/// (`--title -- --help` filters on the literal title "--help"); a standalone
/// `--` ends flag recognition (find takes no positionals, so later tokens are
/// usage errors either way).
pub(crate) fn parse_find_args(args: &[String]) -> Result<FindArgs, String> {
    let mut parsed = FindArgs::default();
    let mut literal = false;
    let mut index = 0;
    // Value for a flag at `flag_index`: a `--` in the value slot escapes the
    // NEXT token as the literal value. Returns (value, next_index).
    pub(crate) fn flag_value<'a>(
        args: &'a [String],
        flag_index: usize,
        flag: &str,
    ) -> Result<(&'a str, usize), String> {
        let value_index = if args.get(flag_index + 1).map(String::as_str) == Some("--") {
            flag_index + 2
        } else {
            flag_index + 1
        };
        args.get(value_index)
            .map(|value| (value.as_str(), value_index + 1))
            .ok_or_else(|| format!("{flag} requires a value"))
    }

    while index < args.len() {
        let arg = args[index].as_str();
        if literal {
            return Err(format!("find takes no positional arguments; got: {arg}"));
        }
        match arg {
            "--" => {
                literal = true;
                index += 1;
            }
            "--command" => {
                let (value, next) = flag_value(args, index, "--command")?;
                parsed.command = Some(value.to_string());
                index = next;
            }
            "--title" => {
                let (value, next) = flag_value(args, index, "--title")?;
                parsed.title = Some(value.to_string());
                index = next;
            }
            "--cwd" => {
                let (value, next) = flag_value(args, index, "--cwd")?;
                parsed.cwd = Some(value.to_string());
                index = next;
            }
            "--state" => {
                let (value, next) = flag_value(args, index, "--state")?;
                let state = match value {
                    "live" => PaneRuntimeState::Live,
                    "ended" => PaneRuntimeState::Ended,
                    other => {
                        return Err(format!(
                            "invalid --state '{other}': must be 'live' or 'ended'"
                        ));
                    }
                };
                parsed.state = Some(state);
                index = next;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown find option: {other}"));
            }
            other => {
                return Err(format!("find takes no positional arguments; got: {other}"));
            }
        }
    }
    Ok(parsed)
}

/// `ctl snapshot <pane> [--json]` — read a pane's rendered visible screen. With
/// `--json`, prints the documented snapshot struct (architecture §6.3); otherwise
/// prints the screen, one line per visible row, to stdout (VAL-PRIM-025). Reads an
/// existing pane only (never spawns a daemon).
pub(crate) fn control_snapshot(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_snapshot_args(args)?;
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let snapshot: Value = client.request(DaemonRequest::Snapshot { pane_id })?;

    if json_output {
        write_json_stdout(&snapshot)?;
    } else {
        let mut stdout = std::io::stdout();
        if let Some(lines) = snapshot.get("lines").and_then(Value::as_array) {
            for line in lines {
                writeln!(stdout, "{}", line.as_str().unwrap_or(""))
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
        }
    }
    Ok(())
}

/// `ctl find [--command S] [--title S] [--cwd S] [--state live|ended] [--json]` — query
/// the workspace's panes by metadata. Filters AND together and are substring matches; no
/// filters returns every pane (live and ended); no matches returns an empty set (exit 0,
/// not an error). With `--json`, prints the array of per-pane metadata; otherwise prints
/// a compact human line per matched pane. Reads existing panes only (never spawns a
/// daemon). (Architecture §6.3; VAL-PRIM-028..039/052/054.)
pub(crate) fn control_find(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_find_args(args)?;
    let result: Value = client.request(DaemonRequest::Find {
        command: parsed.command,
        title: parsed.title,
        cwd: parsed.cwd,
        state: parsed.state,
    })?;

    if json_output {
        write_json_stdout(&result)?;
    } else {
        let mut stdout = std::io::stdout();
        if let Some(entries) = result.as_array() {
            for entry in entries {
                let id = entry["id"].as_str().unwrap_or("");
                let state = entry["state"].as_str().unwrap_or("");
                let title = entry["title"].as_str().unwrap_or("");
                let command = entry["command"].as_str().unwrap_or("");
                writeln!(stdout, "{id}\t{state}\t{title}\t{command}")
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
        }
    }
    Ok(())
}

/// (T1) Parsed `ctl agent` arguments: the pane reference (defaults to the
/// active pane, like every PANE-taking command) and the requested operation.
#[derive(Debug)]
pub(crate) struct AgentArgs {
    pub(crate) pane_ref: String,
    /// `None`: query. `Some(Some("claude"))`: mark. `Some(None)`: unmark.
    pub(crate) mark: Option<Option<String>>,
    /// `--watch`: stream agent-state, lease and pane-end transitions
    /// (docs/design/keyboard-lease-and-ledger.md §6 M3). With no PANE the
    /// stream covers every pane; with one it ends when that pane closes.
    pub(crate) watch: bool,
    /// Whether a PANE was given (a bare `--watch` means every pane).
    pub(crate) pane_given: bool,
}

/// (T1) `ctl agent [PANE] [on|off]` — `on`/`off` as the FIRST positional is
/// the operation on the ACTIVE pane (L9: `ctl agent on` ≡ `ctl agent active
/// on`); any other first positional is the PANE reference and the optional
/// second positional is the operation.
pub(crate) fn parse_agent_args(args: &[String]) -> Result<AgentArgs, String> {
    let watch = args.iter().any(|arg| arg == "--watch");
    let args: Vec<String> = args
        .iter()
        .filter(|arg| *arg != "--watch")
        .cloned()
        .collect();
    let args = args.as_slice();
    let verb_first = matches!(args.first().map(String::as_str), Some("on" | "off"));
    let (pane_ref, op_index) = if verb_first {
        ("active".to_string(), 0)
    } else {
        (
            args.first()
                .cloned()
                .unwrap_or_else(|| "active".to_string()),
            1,
        )
    };
    let mark = match args.get(op_index).map(String::as_str) {
        None => None,
        Some("on") => Some(Some("claude".to_string())),
        Some("off") => Some(None),
        Some(other) => return Err(format!("unexpected argument for agent: {other}")),
    };
    if let Some(extra) = args.get(op_index + 1) {
        return Err(format!("unexpected argument for agent: {extra}"));
    }
    if watch && mark.is_some() {
        return Err("--watch cannot be combined with on/off".to_string());
    }
    let pane_given = !verb_first && !args.is_empty();
    Ok(AgentArgs {
        pane_ref,
        mark,
        watch,
        pane_given,
    })
}

/// One line for `ctl agent --watch`: the agent-state, lease and pane-end
/// transitions a script wants to react to; `None` for everything else.
/// JSON is the daemon's own event shape (`{"event":"agent_state",...}`).
pub(crate) fn format_watch_event(event: &DaemonEvent, json_output: bool) -> Option<String> {
    pub(crate) fn enum_name<T: Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "-".to_string())
    }
    let text = match event {
        DaemonEvent::AgentState {
            pane_id,
            agent,
            attention,
            mode,
        } => format!(
            "{pane_id}\tagent_state\t{}\t{}\t{}",
            agent.as_deref().unwrap_or("-"),
            attention
                .map(|value| enum_name(&value))
                .unwrap_or_else(|| "-".to_string()),
            mode.as_deref().unwrap_or("-")
        ),
        DaemonEvent::LeaseState {
            pane_id,
            transition,
            holder,
            ..
        } => format!(
            "{pane_id}\tlease_{}\t{}",
            enum_name(transition),
            holder.as_deref().unwrap_or("-")
        ),
        DaemonEvent::PaneEnded { pane_id, exit_code } => format!(
            "{pane_id}\tpane_ended\t{}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string())
        ),
        DaemonEvent::PaneClosed { pane_id } => format!("{pane_id}\tpane_closed"),
        DaemonEvent::OutputWarning { pane_id, total, .. } => format!(
            "{pane_id}\toutput_warning{}",
            format_output_warning(&serde_json::to_value(total).unwrap_or(Value::Null))
        ),
        DaemonEvent::AgentUsage { pane_id, usage } => format!(
            "{pane_id}\tagent_usage\t{}",
            usage.summary(now_millis() / 1000)
        ),
        _ => return None,
    };
    if json_output {
        serde_json::to_string(event).ok()
    } else {
        Some(text)
    }
}

/// `\tHIDDEN-OUTPUT conceal=2 clipboard=1` for a non-empty counter object, else "".
pub(crate) fn format_output_warning(tricks: &Value) -> String {
    let Some(map) = tricks.as_object() else {
        return String::new();
    };
    let parts: Vec<String> = map
        .iter()
        .filter(|(_, count)| count.as_u64().unwrap_or(0) > 0)
        .map(|(kind, count)| format!("{kind}={}", count.as_u64().unwrap_or(0)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("\tHIDDEN-OUTPUT {}", parts.join(" "))
    }
}

pub(crate) fn watch_event_pane(event: &DaemonEvent) -> Option<&str> {
    match event {
        DaemonEvent::OutputWarning { pane_id, .. }
        | DaemonEvent::AgentUsage { pane_id, .. }
        | DaemonEvent::AgentState { pane_id, .. }
        | DaemonEvent::LeaseState { pane_id, .. }
        | DaemonEvent::PaneEnded { pane_id, .. }
        | DaemonEvent::PaneClosed { pane_id } => Some(pane_id),
        _ => None,
    }
}

/// `ctl agent --watch [PANE]` — print the current agent state as a baseline,
/// then stream transitions until killed (or, with a PANE, until it closes).
pub(crate) fn control_agent_watch(
    client: &DaemonClient,
    pane_filter: Option<String>,
    json_output: bool,
) -> Result<(), String> {
    // Subscribe first so a transition between the baseline read and the loop
    // is queued rather than missed.
    let mut conn = client.connect()?;
    conn.write_request(&DaemonRequest::Subscribe)?;
    conn.await_subscribe_ack()?;
    conn.set_read_timeout(None);

    let entries: Value = client.request(DaemonRequest::Find {
        command: None,
        title: None,
        cwd: None,
        state: None,
    })?;
    let mut stdout = std::io::stdout();
    for entry in entries.as_array().into_iter().flatten() {
        let Some(pane_id) = entry["id"].as_str() else {
            continue;
        };
        if pane_filter
            .as_deref()
            .is_some_and(|wanted| wanted != pane_id)
        {
            continue;
        }
        let baseline = DaemonEvent::AgentState {
            pane_id: pane_id.to_string(),
            agent: entry["agent"].as_str().map(str::to_string),
            attention: serde_json::from_value(entry["attention"].clone()).ok(),
            mode: entry["mode"].as_str().map(str::to_string),
        };
        if let Some(line) = format_watch_event(&baseline, json_output) {
            writeln!(stdout, "{line}")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
    }
    stdout
        .flush()
        .map_err(|error| format!("failed to write stdout: {error}"))?;

    loop {
        let Some(event) = conn.read_event()? else {
            return Ok(());
        };
        let Some(pane_id) = watch_event_pane(&event) else {
            continue;
        };
        if pane_filter
            .as_deref()
            .is_some_and(|wanted| wanted != pane_id)
        {
            continue;
        }
        if let Some(line) = format_watch_event(&event, json_output) {
            writeln!(stdout, "{line}")
                .and_then(|_| stdout.flush())
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        if pane_filter.is_some() && matches!(event, DaemonEvent::PaneClosed { .. }) {
            return Ok(());
        }
    }
}

/// (T1) Read a pane's agent state via `find` (which — unlike `snapshot` —
/// answers for panes without a live screen model, e.g. restored-never-spawned
/// panes) and shape it like the SetPaneAgent response.
pub(crate) fn query_agent_state(client: &DaemonClient, pane_id: &str) -> Result<Value, String> {
    let result: Value = client.request(DaemonRequest::Find {
        command: None,
        title: None,
        cwd: None,
        state: None,
    })?;
    let entry = result.as_array().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry["id"].as_str() == Some(pane_id))
    });
    Ok(json!({
        "pane_id": pane_id,
        "agent": entry.and_then(|entry| entry.get("agent")).cloned().unwrap_or(Value::Null),
        "attention": entry.and_then(|entry| entry.get("attention")).cloned().unwrap_or(Value::Null),
        "mode": entry.and_then(|entry| entry.get("mode")).cloned().unwrap_or(Value::Null),
        "unattended": entry
            .and_then(|entry| entry.get("unattended"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "output_warnings": entry
            .and_then(|entry| entry.get("output_warnings"))
            .cloned()
            .unwrap_or(Value::Null),
        "usage": entry
            .and_then(|entry| entry.get("usage"))
            .cloned()
            .unwrap_or(Value::Null),
    }))
}

/// (T1) `ctl agent [PANE] [on|off]` — print a pane's agent + attention state,
/// or override it: `on` marks the pane as running Claude Code (persisted),
/// `off` clears the mark (auto-detection resumes). The current state is
/// printed after a set, consistent with other mutating commands.
pub(crate) fn control_agent(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_agent_args(args)?;
    if parsed.watch {
        let filter = if parsed.pane_given {
            Some(resolve_pane_ref(client, &parsed.pane_ref)?)
        } else {
            None
        };
        return control_agent_watch(client, filter, json_output);
    }
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let state = match parsed.mark {
        Some(agent) => client.request::<Value>(DaemonRequest::SetPaneAgent {
            pane_id: pane_id.clone(),
            agent,
        })?,
        None => query_agent_state(client, &pane_id)?,
    };

    if json_output {
        write_json_stdout(&state)
    } else {
        let mut stdout = std::io::stdout();
        let suffix = format!(
            "{}{}",
            if state["unattended"].as_bool().unwrap_or(false) {
                "\tUNATTENDED"
            } else {
                ""
            },
            format_output_warning(&state["output_warnings"])
        );
        let usage_text = serde_json::from_value::<AgentUsage>(state["usage"].clone())
            .ok()
            .filter(|usage| usage.updated_at_ms > 0)
            .map(|usage| format!("\t{}", usage.summary(now_millis() / 1000)))
            .unwrap_or_default();
        writeln!(
            stdout,
            "{}\t{}\t{}\t{}{}{}",
            pane_id,
            state["agent"].as_str().unwrap_or("-"),
            state["attention"].as_str().unwrap_or("-"),
            state["mode"].as_str().unwrap_or("-"),
            suffix,
            usage_text
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

pub(crate) fn control_run(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let plan = parse_run_args(args)?;

    if plan.all || plan.panes_list.is_some() {
        return control_run_batched(client, &plan, json_output);
    }

    let pane_id = resolve_pane_ref(client, &plan.pane_ref)?;
    let family = resolve_shell_family(client)?;
    // Quote only now that the target shell's family is known (H7).
    let command = quote_command_for(family, &plan.command_args);
    let timeout = plan.timeout_ms.map(Duration::from_millis);
    let result = run_in_pane(client, &pane_id, &command, family, "single", timeout);

    if json_output {
        // Prefer the durable per-pane object (same shape as batched) while
        // keeping `pane_id` for older consumers of the single-pane form.
        let mut payload = result.to_json();
        if let Some(object) = payload.as_object_mut() {
            object.insert("pane_id".to_string(), json!(pane_id));
        }
        write_json_stdout(&payload)?;
    } else if let Some(code) = result.exit_code {
        writeln!(std::io::stdout(), "exit {code}")
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }

    match result.exit_code {
        Some(0) => Ok(()),
        Some(code) => {
            // Mirror the command's own exit code as the CLI's process exit (L2),
            // clamped into 1..=255 so a failure can never read as 0 and >255
            // can't wrap. Failures without a captured code (timeout, dead pane)
            // keep the default exit 1.
            CLI_EXIT_CODE.store(code.clamp(1, 255), Ordering::SeqCst);
            Err(format!("command exited with code {code}"))
        }
        None => Err(result.error.unwrap_or_else(|| "run failed".to_string())),
    }
}

/// The batched (waiting, per-pane exit-code-collecting) variant of `ctl run`.
/// Runs the command in every live pane (`--all`) or a named subset
/// (`--panes A,B`) concurrently, collects each pane's exit code, and reports
/// per-pane results. The aggregate process exit is 0 only when every targeted
/// pane succeeded (VAL-ORCH-013). A not-live / dying pane is reported as a
/// failure for that pane without hanging or losing other panes' results
/// (VAL-ORCH-028). Zero live panes yields an empty result set and exit 0
/// (VAL-ORCH-030).
pub(crate) fn control_run_batched(
    client: &DaemonClient,
    plan: &RunPlan,
    json_output: bool,
) -> Result<(), String> {
    let results = collect_batched_results(client, plan)?;

    // Zero live panes: empty result set, vacuously all-success (VAL-ORCH-030).
    if results.is_empty() {
        if json_output {
            write_json_stdout(&json!([]))?;
        }
        return Ok(());
    }

    let any_failed = results.iter().any(|result| !result.success);

    if json_output {
        let json_results: Vec<Value> = results.iter().map(PaneRunResult::to_json).collect();
        write_json_stdout(&json!(json_results))?;
    } else {
        let mut stdout = std::io::stdout();
        for result in &results {
            if let Some(code) = result.exit_code {
                writeln!(
                    stdout,
                    "{} exit {code} ({}ms)",
                    result.pane_id, result.elapsed_ms
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            } else {
                let kind = if result.timed_out {
                    "timeout"
                } else {
                    "failed"
                };
                writeln!(
                    stdout,
                    "{} {kind}: {} ({}ms)",
                    result.pane_id,
                    result.error.as_deref().unwrap_or("unknown"),
                    result.elapsed_ms
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
        }
    }

    if any_failed {
        let failures = results.iter().filter(|r| !r.success).count();
        Err(format!("{failures} pane(s) failed"))
    } else {
        Ok(())
    }
}

/// Resolve the target pane set for a batched run and execute the command in
/// each pane concurrently, returning a sorted `Vec<PaneRunResult>`. This is
/// the testable core of `control_run_batched` (no stdout I/O). Returns an
/// error only for resolution failures (e.g. an unknown pane ref in
/// `--panes`); per-pane failures are represented in the result vector, not as
/// an error.
pub(crate) fn collect_batched_results(
    client: &DaemonClient,
    plan: &RunPlan,
) -> Result<Vec<PaneRunResult>, String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;

    // Resolve the target pane set.
    // --all: every LIVE pane (ended panes are excluded, not reported).
    // --panes A,B: the named panes (resolved via match_pane_ref; unknown refs
    //   fail fast with "pane not found" per VAL-ORCH-025). Named panes that
    //   turn out to be Ended are targeted and reported as failures (VAL-ORCH-028).
    let targets: Vec<String> = if plan.all {
        list.panes
            .iter()
            .filter(|status| status.state == PaneRuntimeState::Live)
            .map(|status| status.pane.id.clone())
            .collect()
    } else {
        let list_str = plan
            .panes_list
            .as_ref()
            .ok_or_else(|| "batched run requires --all or --panes".to_string())?;
        let mut ids = Vec::new();
        for pane_ref in list_str
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let id = match_pane_ref(&list, pane_ref)?;
            ids.push(id);
        }
        ids
    };

    let family = resolve_shell_family(client)?;

    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // Run each pane concurrently in scoped threads so a slow/dying pane does
    // not block the others (VAL-ORCH-028). Each thread opens its own
    // subscriber stream and watches only its pane's output.
    // Quote only now that the target shell's family is known (H7).
    let command = quote_command_for(family, &plan.command_args);
    let command = command.as_str();
    let timeout = plan.timeout_ms.map(Duration::from_millis);
    let mut results: Vec<PaneRunResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(idx, pane_id)| {
                let pane_id = pane_id.as_str();
                // Distinct suffix per pane so markers don't cross-match even
                // under sync-input mirroring.
                let suffix = format!("b{idx}");
                scope.spawn(move || run_in_pane(client, pane_id, command, family, &suffix, timeout))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    PaneRunResult::failed("?", "run thread panicked".to_string(), 0, String::new())
                })
            })
            .collect()
    });

    // Sort by pane id for deterministic output.
    results.sort_by(|left, right| left.pane_id.cmp(&right.pane_id));
    Ok(results)
}

/// Trim `buffer` from the front so at most `keep` bytes remain, respecting UTF-8
/// character boundaries.
pub(crate) fn trim_to_tail(buffer: &mut String, keep: usize) {
    if buffer.len() <= keep {
        return;
    }
    let mut start = buffer.len() - keep;
    while !buffer.is_char_boundary(start) {
        start += 1;
    }
    buffer.drain(..start);
}

/// Find `prefix` followed by digits in `buffer` (the command's output), ignoring the
/// echoed command line (which contains the literal `%s`, not digits). Digits that run
/// to the end of the buffer are not accepted: the rest of the code may still be in
/// flight (the marker line always ends with a newline), so wait for the terminator.
pub(crate) fn parse_exit_marker(buffer: &str, prefix: &str) -> Option<i32> {
    let mut search_from = 0;
    while let Some(found) = buffer[search_from..].find(prefix) {
        let start = search_from + found + prefix.len();
        let digits: String = buffer[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if !digits.is_empty() && start + digits.len() < buffer.len() {
            if let Ok(code) = digits.parse::<i32>() {
                return Some(code);
            }
        }
        search_from = start.max(search_from + 1);
    }
    None
}

/// `ctl logs` — tail the structured daemon log. Read-only: requires a running
/// daemon (the caller uses `connect_existing`), never spawns one.
///
/// Options (documented in `ctl help`):
///   -n, --lines N   Print the last N lines (default: all)
///   -f, --follow    Stream new entries as they are written (blocks until killed)
pub(crate) fn control_logs(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    let plan = parse_logs_args(args)?;
    let log_path = client.data_dir.join(LOG_FILE);

    // Print the tail (last N lines or all) of the current log file.
    let tail = read_log_tail(&log_path, plan.lines);
    let mut stdout = std::io::stdout();
    for line in &tail {
        writeln!(stdout, "{line}").map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    stdout
        .flush()
        .map_err(|error| format!("failed to write stdout: {error}"))?;

    if plan.follow {
        follow_log_file(&log_path)?;
    }

    Ok(())
}

/// Whether two metadata handles refer to the same underlying file. Unix compares
/// (device, inode); the Windows fallback says "same" (rotation-follow is
/// best-effort there, matching the transport's compiles-only bar).
#[cfg(unix)]
pub(crate) fn is_same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
pub(crate) fn is_same_file(_a: &fs::Metadata, _b: &fs::Metadata) -> bool {
    true
}

/// Stream new log entries as they are written to the file. Seeks to the current
/// end of the file, then polls for new content every 100ms, printing each new
/// line to stdout. Blocks until the process is killed (e.g. Ctrl+C).
///
/// Rotation-aware (M10): `BoundedFileWriter` rotates by renaming the active log
/// aside and creating a fresh file at the same path. The follower tracks the
/// PATH — when the path's identity changes (or the file shrinks), it reopens
/// from the start of the new file instead of reading the renamed inode's silence
/// forever.
pub(crate) fn follow_log_file(log_path: &Path) -> Result<(), String> {
    let file = File::open(log_path).map_err(|error| format!("failed to open log file: {error}"))?;
    let mut reader = BufReader::new(file);
    // Seek to end so only new entries (written after `ctl logs` started) are
    // streamed. The initial tail above already printed existing content.
    let mut position = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("failed to seek log file: {error}"))?;

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // No new content: check for rotation before waiting. A missing
                // path (mid-rotation window) just retries next tick.
                let rotated = match (fs::metadata(log_path), reader.get_ref().metadata()) {
                    (Ok(path_meta), Ok(open_meta)) => {
                        !is_same_file(&path_meta, &open_meta) || path_meta.len() < position
                    }
                    _ => false,
                };
                if rotated {
                    if let Ok(new_file) = File::open(log_path) {
                        reader = BufReader::new(new_file);
                        position = 0;
                        continue;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
            Ok(read) => {
                position += read as u64;
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(line.as_bytes())
                    .and_then(|_| stdout.flush())
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Err(error) => return Err(format!("failed to read log file: {error}")),
        }
    }
}

pub(crate) fn control_shutdown(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let result: CommandOk = client.request(DaemonRequest::Shutdown)?;
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "daemon stopped")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Non-PTY argv runner (ENHANCEMENTS §3): `ctl process [--cwd DIR] [--timeout MS] -- <argv...>`.
pub(crate) fn control_process(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let mut cwd = None;
    let mut timeout_ms = None;
    let mut argv = Vec::new();
    let mut index = 0;
    let mut passthrough = false;
    while index < args.len() {
        if passthrough {
            argv.push(args[index].clone());
            index += 1;
            continue;
        }
        match args[index].as_str() {
            "--" => {
                passthrough = true;
                index += 1;
            }
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--cwd requires a path".to_string())?;
                cwd = Some(value.clone());
                index += 2;
            }
            "--timeout" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--timeout requires milliseconds".to_string())?;
                timeout_ms = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --timeout value: {value}"))?,
                );
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected process option: {other}"));
            }
            _ => {
                // Allow `ctl process echo hi` without `--`.
                argv.push(args[index].clone());
                index += 1;
            }
        }
    }
    if argv.is_empty() {
        return Err("process requires a command (try: process -- echo hi)".to_string());
    }
    let result: Value = client.request(DaemonRequest::RunProcess {
        argv,
        cwd,
        timeout_ms,
    })?;
    if json_output {
        write_json_stdout(&result)?;
    } else {
        let code = result.get("exit_code");
        let timed_out = result
            .get("timed_out")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let elapsed = result
            .get("elapsed_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if timed_out {
            writeln!(std::io::stdout(), "timeout ({elapsed}ms)")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        } else if let Some(code) = code.and_then(|v| v.as_i64()) {
            writeln!(std::io::stdout(), "exit {code} ({elapsed}ms)")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        } else {
            writeln!(std::io::stdout(), "failed ({elapsed}ms)")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        if let Some(stdout) = result.get("stdout").and_then(|v| v.as_str()) {
            if !stdout.is_empty() {
                print!("{stdout}");
                if !stdout.ends_with('\n') {
                    println!();
                }
            }
        }
        if let Some(stderr) = result.get("stderr").and_then(|v| v.as_str()) {
            if !stderr.is_empty() && stderr != "timed out" {
                eprint!("{stderr}");
                if !stderr.ends_with('\n') {
                    eprintln!();
                }
            }
        }
    }
    let success = result
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if success {
        Ok(())
    } else if result
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        Err("process timed out".to_string())
    } else if let Some(code) = result.get("exit_code").and_then(|v| v.as_i64()) {
        CLI_EXIT_CODE.store((code as i32).clamp(1, 255), Ordering::SeqCst);
        Err(format!("process exited with code {code}"))
    } else {
        Err("process failed".to_string())
    }
}

/// Privacy-safe diagnostic bundle for support (ENHANCEMENTS §3).
/// Omits prompts, scrollback, tokens, env values, and agent message bodies.
pub(crate) fn control_diagnostic(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    if let Some(extra) = args.first() {
        return Err(format!("unexpected argument for diagnostic: {extra}"));
    }
    let status: VerboseStatus = client.request(DaemonRequest::StatusVerbose)?;
    let panes: PaneList = client.request(DaemonRequest::ListPanes)?;
    let found: Value = client.request(DaemonRequest::Find {
        command: None,
        title: None,
        cwd: None,
        state: None,
    })?;
    let log_path = client.data_dir.join(LOG_FILE);
    let log_tail = read_scrubbed_log_tail(&log_path, 200);
    let find_by_id: HashMap<String, &Value> = found
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(|id| id.as_str())
                .map(|id| (id.to_string(), entry))
        })
        .collect();
    let pane_summaries: Vec<Value> = panes
        .panes
        .iter()
        .map(|pane_status| {
            let meta = find_by_id.get(&pane_status.pane.id);
            json!({
                "id": pane_status.pane.id,
                "title": pane_status.pane.title,
                "kind": pane_status.pane.kind,
                "state": pane_status.state,
                "agent": meta.and_then(|m| m.get("agent")).cloned().unwrap_or(Value::Null),
                "attention": meta.and_then(|m| m.get("attention")).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at_ms": now_millis(),
        "cwd": status.cwd,
        "uptime_secs": status.uptime_secs,
        "subscribers": status.subscribers,
        "active_pane_id": status.active_pane_id,
        "config": status.config,
        "panes": pane_summaries,
        "daemon_log_tail": log_tail,
        "notes": [
            "env values omitted; profile env values omitted",
            "agent conversation logs and PTY scrollback are not included",
            "daemon_log_tail lines matching a secret-ish denylist are replaced with [redacted]; this is best-effort, not a guarantee",
        ],
    });
    // Always JSON — this is a machine/support artifact (`--json` is optional).
    let _ = json_output;
    write_json_stdout(&report)
}

/// Last `max_lines` of daemon.log with obvious secret-bearing lines redacted.
pub(crate) fn read_scrubbed_log_tail(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| scrub_diagnostic_log_line(line))
        .collect()
}

pub(crate) fn scrub_diagnostic_log_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let suspicious = [
        "token",
        "authorization",
        "password",
        "secret",
        "api_key",
        "apikey",
        "cookie",
        "credential",
        "bearer ",
        "private key",
        "begin rsa",
        "begin openssh",
        "aws_secret",
        "sig=",
        "x-api-key",
    ];
    if suspicious.iter().any(|needle| lower.contains(needle)) {
        "[redacted]".to_string()
    } else {
        line.to_string()
    }
}

/// Bounded pipe reader for `ctl process`: keep at most `cap` bytes (tail) while
/// draining so a chatty child cannot grow daemon memory unboundedly.
pub(crate) fn read_capped_pipe_tail(mut reader: impl std::io::Read, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > cap {
                    let drop = out.len() - cap;
                    out.drain(..drop);
                }
            }
            Err(_) => break,
        }
    }
    out
}

pub(crate) fn capped_utf8_tail(bytes: &[u8], cap: usize) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let start = if bytes.len() > cap {
        let mut i = bytes.len() - cap;
        while i < bytes.len() && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
            i += 1;
        }
        i
    } else {
        0
    };
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

pub(crate) fn pipe_reader_finished(handle: &Option<std::thread::JoinHandle<Vec<u8>>>) -> bool {
    handle.as_ref().map(|h| h.is_finished()).unwrap_or(true)
}

pub(crate) fn take_pipe_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> String {
    let Some(handle) = handle else {
        return String::new();
    };
    if !handle.is_finished() {
        // Detach: caller already killed the process group; hanging forever is worse.
        return String::new();
    }
    match handle.join() {
        Ok(bytes) => capped_utf8_tail(&bytes, 256 * 1024),
        Err(_) => String::new(),
    }
}

/// Wait up to `budget` for both pipe readers. If either is still blocked (a
/// descendant holding the pipe), kill the process group and grant a short grace
/// period so readers can unblock before we abandon them.
pub(crate) fn finalize_run_process_pipes(
    child: &mut std::process::Child,
    mut stdout_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    mut stderr_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    budget: Duration,
) -> (String, String) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if pipe_reader_finished(&stdout_handle) && pipe_reader_finished(&stderr_handle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !pipe_reader_finished(&stdout_handle) || !pipe_reader_finished(&stderr_handle) {
        kill_run_process_tree(child);
        let grace = Instant::now() + Duration::from_millis(500);
        while Instant::now() < grace {
            if pipe_reader_finished(&stdout_handle) && pipe_reader_finished(&stderr_handle) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    (
        take_pipe_reader(stdout_handle.take()),
        take_pipe_reader(stderr_handle.take()),
    )
}

pub(crate) fn kill_run_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        if pid > 0 {
            // SAFETY: negative pid kills the process group created in pre_exec.
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

pub(crate) fn exit_status_fields(status: &std::process::ExitStatus) -> (Value, Value) {
    if let Some(code) = status.code() {
        return (json!(code), Value::Null);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (Value::Null, json!(signal));
        }
    }
    (Value::Null, Value::Null)
}

pub(crate) fn parse_name_option(args: &[String]) -> Result<Option<String>, String> {
    let mut title: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let (value, consumed) = match args[index].as_str() {
            "--name" | "-n" => (
                args.get(index + 1)
                    .ok_or_else(|| "--name requires a title".to_string())?
                    .clone(),
                2,
            ),
            flag if flag.starts_with('-') => {
                return Err(format!("unexpected pane option: {flag}"));
            }
            value => (value.to_string(), 1),
        };
        // Exactly one title source (L7): `new --name a b` used to silently
        // create pane `b` while `new a --name b` errored — order-dependent
        // and lossy. Any second title is now an error regardless of order.
        if title.is_some() {
            return Err(
                "pane title given more than once (use one positional NAME or one --name)"
                    .to_string(),
            );
        }
        title = Some(value);
        index += consumed;
    }
    Ok(title)
}

pub(crate) fn resolve_pane_ref(client: &DaemonClient, pane_ref: &str) -> Result<String, String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;
    match_pane_ref(&list, pane_ref)
}

/// (T2) Resolve a pane reference to its full status (id AND kind), for
/// commands that route by pane kind (`send`, `interrupt`).
pub(crate) fn resolve_pane_status(
    client: &DaemonClient,
    pane_ref: &str,
) -> Result<PaneStatus, String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;
    match_pane_status(&list, pane_ref)
}

/// (T2) Same resolution rules as `match_pane_ref`, returning the matched
/// PaneStatus instead of just the id.
pub(crate) fn match_pane_status(list: &PaneList, pane_ref: &str) -> Result<PaneStatus, String> {
    let pane_id = match_pane_ref(list, pane_ref)?;
    list.panes
        .iter()
        .find(|status| status.pane.id == pane_id)
        .cloned()
        .ok_or_else(|| format!("pane not found: {pane_ref}"))
}

/// Resolve a pane reference: the keyword `active`, an exact pane id, or a unique
/// title. Exact ids win over titles so a pane *titled* like another pane's id
/// cannot shadow or ambiguate the real pane.
pub(crate) fn match_pane_ref(list: &PaneList, pane_ref: &str) -> Result<String, String> {
    if pane_ref == "active" {
        return list
            .active_pane_id
            .clone()
            .ok_or_else(|| "workspace has no active pane".to_string());
    }

    if let Some(status) = list.panes.iter().find(|status| status.pane.id == pane_ref) {
        return Ok(status.pane.id.clone());
    }

    let matches = list
        .panes
        .iter()
        .filter(|status| status.pane.title == pane_ref)
        .map(|status| status.pane.id.clone())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [pane_id] => Ok(pane_id.clone()),
        [] => Err(format!("pane not found: {pane_ref}")),
        _ => Err(format!("pane reference is ambiguous: {pane_ref}")),
    }
}

/// Decode CLI escape sequences in input text.
///
/// By default (`literal_lf = false`), `\n` and `\r` both map to a carriage
/// return (CR, 0x0D) — the "press Enter" semantics a terminal expects when a
/// user submits a line. With `literal_lf = true` (the `--lf`/`--raw` flag),
/// `\n` maps to a literal line-feed (LF, 0x0A) so callers can transmit a raw
/// LF byte to the PTY (e.g. for tools that read fixed-size binary frames).
/// `\r` always maps to CR in both modes.
pub(crate) fn decode_cli_text(input: &str, literal_lf: bool) -> String {
    let mut output = String::new();
    let mut chars = input.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            output.push(char);
            continue;
        }

        match chars.next() {
            Some('n') => output.push(if literal_lf { '\n' } else { '\r' }),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('\\') => output.push('\\'),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

/// Extract the `--lf`/`--raw` flag from a `send`/`broadcast` argument list.
///
/// Returns `(literal_lf, remaining)` where `remaining` preserves the order of
/// every non-flag argument. The flag may appear anywhere in the list (it is
/// pulled out before the pane/text split), so `send <pane> --lf <text>` and
/// `send --lf <pane> <text>` are both accepted.
pub(crate) fn parse_lf_flag(args: &[String]) -> (bool, Vec<String>) {
    let mut literal_lf = false;
    let mut remaining = Vec::with_capacity(args.len());
    let mut passthrough = false;
    for arg in args {
        if passthrough {
            remaining.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--lf" | "--raw" => literal_lf = true,
            // A `--` ends flag recognition so a payload that IS the literal
            // string `--lf`/`--raw`/`--` can still be transmitted (L6).
            "--" => passthrough = true,
            other => remaining.push(other.to_string()),
        }
    }
    (literal_lf, remaining)
}

pub(crate) fn write_json_stdout<T: Serialize>(value: &T) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    serde_json::to_writer_pretty(&mut stdout, value)
        .map_err(|error| format!("failed to write json: {error}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|error| format!("failed to write stdout: {error}"))
}

pub(crate) fn print_control_help() -> Result<(), String> {
    const HELP: &str = r#"Usage: sgian ctl [--workspace PATH] [--json] <command> [args]

Global options (may precede the command; --json/--workspace may also follow
commands that don't take free text. They are recognized even right after a
value-taking flag, so use `--` when a VALUE must literally be one of them):
  -w, --workspace PATH   Target a workspace directory (default: current dir)
      --json             Emit machine-readable JSON
  -h, --help             Show this help
      --                 End global-flag recognition; later tokens pass through

Commands (PANE is a pane id or title; defaults to the active pane):
  workspaces                    List known workspaces
  daemons                       List workspaces and whether their daemon is running
  ipc-endpoint                  Start/locate the daemon and print native-client
                                IPC discovery metadata (never prints the token)
  panes | list                  List panes in the workspace
  new [--agent] [--backend claude|droid] [--model ID] [--profile NAME] [--project NAME] [--name NAME]
                                Create a pane (alias: pane new).
                                  --agent defaults to Claude
                                  --backend selects the agent CLI
                                  --model selects that CLI's model id
                                  --profile applies a named config profile
                                  Provider/model are immutable for the pane.
  status [PANE]                 Show a pane's status   (alias: pane status)
  status --verbose              Show daemon-level runtime detail (subscribers,
                                pane states, uptime, effective config summary)
  restart [PANE]                Restart a pane's shell (alias: pane restart)
  send <PANE> [--lf|--raw] [--as HOLDER] [--generation N] [--] <TEXT...>
                                Send text to a pane    (alias: pane send)
                                  By default \n and \r submit a line as Enter/CR.
                                  --lf / --raw sends a literal LF (0x0A) instead of CR.
                                  --as HOLDER attributes the input to a lease holder
                                  -- ends flag parsing (send a literal "--lf" etc.)
                                  To an agent pane, send posts a chat message
                                  (verbatim; no Enter/CR translation, no --lf).
  agent --watch [PANE]          Stream agent-state, lease and pane-end transitions
                                  (one line each; --json prints the daemon events).
                                  No PANE watches every pane; with one, exits on close.
  hook [--event NAME] [--type NAME] [--pid N] [--no-stdin]
                                The command a Claude Code hook runs: reads the
                                  hook payload from stdin, finds the pane that
                                  owns the calling process (this workspace first,
                                  then every running daemon) and sets its badge
                                  from the hook (Notification → needs input,
                                  UserPromptSubmit/PreToolUse → working, Stop →
                                  idle). Always exits 0; --json prints the answer.
  statusline [--pid N] [--exec COMMAND [ARGS...]]
                                The command Claude Code's status line runs:
                                  reads the payload from stdin, records the
                                  session's model, context fill and rate-limit
                                  windows against the pane that owns the calling
                                  process, then prints your own status command's
                                  output (fed the same payload) or a compact
                                  default line. Always exits 0; --json prints
                                  the daemon's answer instead.
  identity [list]               Issued client credentials (id, holder, scopes)
  identity issue --holder NAME [--scope read,write,admin]
                                Issue a per-client credential; the token is
                                  printed once. The client exports it as
                                  SGIAN_CLIENT_TOKEN (or names a file in
                                  SGIAN_CLIENT_TOKEN_FILE); its holder is then
                                  fixed to NAME and --as anything else is refused.
                                  Default scope: read. Config `identity: required`
                                  makes every write need a credential.
  identity revoke <ID>          Revoke a credential (its connections are cut at
                                  their next request)
  whoami                        This connection's credential, holder, scopes
                                  and the daemon's identity policy
  lease [PANE]                  Show who holds a pane's keyboard (alias: lease status)
  lease take [PANE] [--as HOLDER] [--force --why REASON]
                                Claim the keyboard. While held, input from anyone
                                  else is refused. --force revokes another holder
                                  (REASON is ledgered). HOLDER defaults to
                                  $SGIAN_HOLDER or user@host.
  lease release [PANE] -m NOTE [--as HOLDER] [--generation N]
                                Hand the keyboard back; the note is mandatory.
                                  --generation N (from `lease take --json`) makes
                                  a late command from a previous holder fail as stale.
  project list                  Projects with their attention roll-up
                                  (panes, live, needs input, working, idle,
                                  unattended, keyboard holders)
  project show <NAME>           One project with every member pane's state
  project new <NAME> [--goal TEXT] [--repo PATH]
                                Create a project (a named group of panes
                                  serving one goal; persists with the workspace)
  project add <NAME> <PANE>...  Put panes in a project (a pane is in at most one)
  project rm <PANE>...          Take panes out of their project
  project delete <NAME>         Delete a project (its panes stay open)
  project ledger <NAME> [-n N]  Member panes' ledgers merged in time order
  project dossier <NAME> [--lines N] [--out FILE]
                                One JSON document for a reviewer or a Kranz
                                  gate: the roll-up, every member pane's state,
                                  its full ledger (chain verified) and the last
                                  N scrollback lines (default 40)
  kranz status                  List panes bound to Kranz missions (auto: a
                                  `kranz run` under the pane; manual: bind)
  kranz bind [PANE] [--repo PATH]
                                Bind a pane to the mission at PATH (default: the
                                  pane's cwd). Hand-back notes are mirrored into
                                  its inbox with `kranz msg`; `kranz status`
                                  drives the pane's badge.
  kranz unbind [PANE]           Remove a binding
  search <PANE> [-i] [-n N] [--] <NEEDLE...>
                                Substring search over a pane's whole scrollback
                                  (control sequences stripped): `line<TAB>text`.
                                  -i ignores case; -n caps hits (default 100).
  lines <PANE> <A>[:<B>]        Print scrollback lines A..B (1-based, inclusive):
                                  the range a ledger record or search hit cites.
                                  Numbers are relative to the current file; the
                                  16 MiB cap drops the oldest half and renumbers.
  ledger [PANE] [-n N] [--verify]
                                Print a pane's hash-chained lease ledger (JSONL).
                                  --verify walks the chain and names the first break.
                                  A closed pane's ledger is readable by literal id.
  interrupt [PANE]              Interrupt an agent pane's current turn
                                  (agent panes only; for shells send a Ctrl-C)
  attach [PANE]                 Stream a pane's output (alias: pane attach)
  logs [-n N] [--follow]        Tail the structured daemon log
                                  -n, --lines N   Print the last N lines (default: all)
                                  -f, --follow    Stream new entries (blocks until killed)
  diagnostic                    Privacy-safe support dump (always JSON)
  process [--cwd DIR] [--timeout MS] -- <ARGV...>
                                Non-PTY argv execution in the daemon
  exec [--new] [--all] [--pane PANE] [--panes A,B] [--name NAME] -- <COMMAND...>
                                Run a command in one pane, listed panes, or all
  broadcast [--lf|--raw] [--] <TEXT...>
                                Send text to every live pane
                                  By default \n and \r submit a line as Enter/CR.
                                  --lf / --raw sends a literal LF (0x0A) instead of CR.
                                  -- ends flag parsing (send a literal "--lf" etc.)
  run [--pane PANE] [--all | --panes A,B] [--timeout MS] -- <COMMAND...>
                                Run a command and report its exit code.
                                The ctl process exits with the command's own code
                                  (clamped to 1..255 on failure), so scripts can
                                  branch on $? directly.
                                --timeout MS bounds the wait for the exit marker
                                  (omitted: waits indefinitely).
                                With --all, run in every live pane and report EACH
                                  pane's exit code (waits for all; not fire-and-forget).
                                With --panes A,B, run in only the named subset.
                                Batched --json includes timed_out, elapsed_ms, tail, error.
                                Aggregate exit is 0 only if every targeted pane succeeded.
  wait <PANE> (--text S | --regex RE | --idle MS | --exit) [--timeout MS]
                                Block until a pane satisfies a condition.
                                  --text S / --regex RE  match the visible screen
                                  --idle MS              no new output for MS ms
                                  --exit                 the pane's process ends
                                  --timeout MS           bound the wait
                                  -- ends flag parsing (--text -- --help matches
                                  the literal text "--help")
                                Exits 0 on match, non-zero on timeout; --json prints
                                  {matched,reason,revision,exit_code?,elapsed_ms}.
  snapshot <PANE> [--json]      Read a pane's rendered visible screen.
                                Default prints the screen (one line per visible row);
                                  --json prints the full struct (pane_id, cols, rows,
                                  lines, title, revision, cursor, alive, exit_code?,
                                  command, cwd, ...); an ended pane returns its final
                                  screen + exit code.
  find [--command S] [--title S] [--cwd S] [--state live|ended]
                                Query panes by metadata (filters AND together;
                                  --command/--title/--cwd are substring matches).
                                  No filters lists every pane (live and ended); no
                                  matches is an empty set, not an error. --json prints
                                  per-pane metadata (id, title, command, cwd, state,
                                  exit_code, agent, attention, group, cols, rows,
                                  revision).
  agent [PANE] [on|off]         Show a pane's agent + attention state.
                                  on/off marks/unmarks a terminal pane as running an
                                  agent CLI (persisted); no PANE targets the active pane.
                                  --json prints {pane_id,agent,attention}.
  sync on|off                   Mirror typed input to every live pane
  write-config <FILE>|<JSON>
                                Persist a config to the workspace config.json
                                  (atomically; file-watch live-reloads it)
                                  Pass a file path or an inline JSON string starting with {
  shutdown [--all]              Stop this workspace's daemon (or every daemon)

Read-only commands (panes, status, attach, logs, wait, snapshot, find, agent, lease
status, ledger, shutdown)
need a running daemon; workspaces and daemons only inspect local state (they neither
need nor start one); the other commands start a daemon on demand.

A PANE reference is a pane id or the pane's registry title (the `new --name` /
rename label). `find --title` and snapshot's title field instead report the
terminal's OSC title, which can differ from the registry title.
"#;
    std::io::stdout()
        .write_all(HELP.as_bytes())
        .map_err(|error| format!("failed to write stdout: {error}"))
}

/// Outcome of a background updater check. Errors are intentional skips — the
/// app must stay usable when the feed is down, unsigned, or unreachable
/// (ENHANCEMENTS §5 fault injection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateCheckOutcome {
    Available {
        version: String,
        body: Option<String>,
    },
    UpToDate,
    Skipped(String),
}

pub(crate) fn classify_update_check(
    result: Result<Option<(String, Option<String>)>, String>,
) -> UpdateCheckOutcome {
    match result {
        Ok(Some((version, body))) => UpdateCheckOutcome::Available { version, body },
        Ok(None) => UpdateCheckOutcome::UpToDate,
        Err(error) => UpdateCheckOutcome::Skipped(error),
    }
}

/// (M12) Check the updater feed once in the background and, when an update is
/// available, emit `update-available` with `{version, body}` to the main
/// window (the frontend shows a one-click banner wired to `install_update`).
/// EVERY error — no network, dead DNS, an unprovisioned or unsigned feed — is
/// swallowed to debug-level tracing: the app must be unaffected when the feed
/// is down.
pub(crate) fn spawn_update_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let checked = async {
            let updater = app.updater().map_err(|error| error.to_string())?;
            let update = updater.check().await.map_err(|error| error.to_string())?;
            Ok(update.map(|update| (update.version.clone(), update.body.clone())))
        }
        .await;
        match classify_update_check(checked) {
            UpdateCheckOutcome::Available { version, body } => {
                let payload = json!({
                    "version": version,
                    "body": body,
                });
                if let Err(error) = app.emit_to("main", "update-available", payload) {
                    tracing::debug!("update-available emit failed: {error}");
                }
            }
            UpdateCheckOutcome::UpToDate => {}
            UpdateCheckOutcome::Skipped(error) => {
                tracing::debug!("update check skipped: {error}");
            }
        }
    });
}
