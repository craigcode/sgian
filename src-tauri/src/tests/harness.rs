use super::*;

// ----- In-process integration harness over a short /tmp socket -----

/// A daemon spawned in-process on a background thread over a SHORT /tmp socket,
/// driven through the real `run_daemon_with_config` path with an injected
/// `Config` (never reads the developer's real config.json).
pub(crate) struct TestDaemon {
    pub(crate) data_dir: tempfile::TempDir,
    pub(crate) cwd: PathBuf,
    pub(crate) _socket_dir: tempfile::TempDir,
    pub(crate) socket_path: PathBuf,
    pub(crate) token: String,
    pub(crate) join_handle: Option<thread::JoinHandle<Result<(), String>>>,
}

impl TestDaemon {
    pub(crate) fn spawn(config: Config) -> Self {
        Self::spawn_with_cwd(config, PathBuf::from("/tmp/sgian-itest"))
    }

    pub(crate) fn spawn_with_cwd(mut config: Config, cwd: PathBuf) -> Self {
        // (M3b) Never run the real `claude agents --json` from a test
        // daemon unless the test opts in explicitly.
        if config.agent_probe_interval_ms.is_none() {
            config.agent_probe_interval_ms = Some(0);
        }
        // Reset the process-global shutdown flag: run_daemon's loop checks it,
        // and a prior integration test (or signal) may have left it set. The
        // per-server `shutdown` AtomicBool drives the actual exit; this static
        // is only set by signal handlers but is process-global.
        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);

        let data_dir = tempfile::tempdir().expect("temp data dir");
        // The socket must live under a SHORT /tmp path (macOS sun_path ~104
        // bytes). Its parent dir is chmod'd 0700 by run_daemon, so it cannot
        // be /tmp itself — use a fresh temp dir under /tmp.
        let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
        let socket_path = socket_dir.path().join("d.sock");

        let join_handle = thread::spawn({
            let cwd = cwd.clone();
            let socket_path = socket_path.clone();
            let data_dir_path = data_dir.path().to_path_buf();
            move || run_daemon_with_config(cwd, socket_path, data_dir_path, config)
        });

        // The token file is written by DaemonServer::with_config before the
        // listener binds, so once the socket is up the token is on disk.
        let token_path = data_dir.path().join(TOKEN_FILE);
        let token = retry_read_token(&token_path);
        retry_until_ready(&socket_path, &token);

        Self {
            data_dir,
            cwd,
            _socket_dir: socket_dir,
            socket_path,
            token,
            join_handle: Some(join_handle),
        }
    }

    /// Build a DaemonClient pointing at this daemon (never spawns).
    pub(crate) fn client(&self) -> DaemonClient {
        DaemonClient {
            cwd: self.cwd.clone(),
            socket_path: self.socket_path.clone(),
            data_dir: self.data_dir.path().to_path_buf(),
            token: self.token.clone(),
            auto_spawn: false,
        }
    }

    /// Send Shutdown, join the daemon thread, and drop the temp dirs.
    pub(crate) fn shutdown(mut self) {
        let _ = self.client().request::<CommandOk>(DaemonRequest::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        // TempDirs drop here, removing data + socket dirs.
    }

    /// Shut down the current daemon, then spawn a fresh one that re-loads the
    /// same persisted workspace from the same data_dir. Used for restart /
    /// persistence-survival tests. The old socket dir is replaced with a new
    /// one (the old socket was unlinked on shutdown).
    pub(crate) fn restart(&mut self, config: Config) {
        let _ = self.client().request::<CommandOk>(DaemonRequest::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }

        SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
        let socket_path = socket_dir.path().join("d.sock");

        let join_handle = thread::spawn({
            let cwd = self.cwd.clone();
            let socket_path = socket_path.clone();
            let data_dir_path = self.data_dir.path().to_path_buf();
            move || run_daemon_with_config(cwd, socket_path, data_dir_path, config)
        });

        let token_path = self.data_dir.path().join(TOKEN_FILE);
        self.token = retry_read_token(&token_path);
        retry_until_ready(&socket_path, &self.token);

        self._socket_dir = socket_dir;
        self.socket_path = socket_path;
        self.join_handle = Some(join_handle);
    }

    /// Send Shutdown, join the daemon thread (which flushes the structured log
    /// via the WorkerGuard), then read the daemon log file. The TempDirs are
    /// still alive when the log is read (self is not yet dropped).
    pub(crate) fn shutdown_and_read_log(&mut self) -> String {
        let _ = self.client().request::<CommandOk>(DaemonRequest::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        let log_path = self.data_dir.path().join(LOG_FILE);
        fs::read_to_string(&log_path).unwrap_or_default()
    }
}

#[test]
fn bootstrap_reports_current_pty_sizes() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("initial bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::ResizePaneTerminal {
            pane_id: pane_id.clone(),
            cols: 101,
            rows: 31,
        })
        .expect("resize pane");

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after resize");
    assert_eq!(
        snapshot.sizes.get(&pane_id),
        Some(&PaneSize {
            cols: 101,
            rows: 31,
        })
    );
    daemon.shutdown();
}

pub(crate) fn retry_read_token(path: &Path) -> String {
    for _ in 0..400 {
        if let Ok(Some(token)) = read_token(path) {
            if !token.is_empty() {
                return token;
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("daemon token never appeared at {}", path.display());
}

pub(crate) fn retry_until_ready(socket_path: &Path, token: &str) {
    for _ in 0..400 {
        if let Ok(mut stream) = authenticate_stream_at(socket_path, token) {
            if write_json_line(&mut stream, &DaemonRequest::Ping).is_ok() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    if let Ok(response) = serde_json::from_str::<IpcResponse>(&line) {
                        if response.ok {
                            return;
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("daemon never became ready at {}", socket_path.display());
}

#[test]
fn run_daemon_integration_ping_create_subscribe_broadcast_shutdown() {
    // /bin/cat keeps the pane alive and echoes input, so a broadcast produces a
    // PtyOutput event the subscriber can observe on the event stream.
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Ping
    let ok: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping should succeed");
    assert!(ok.ok);

    // CreatePane — spawns /bin/cat in pane-2 (pane-1 exists but has no shell).
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    assert_eq!(pane.id, "pane-2");

    // Subscribe — the daemon converts this connection into an event stream
    // (no response written to the subscriber).
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");

    // Broadcast — write to every live pane. pane-2's cat echoes it back, which
    // the reader thread emits as a PtyOutput event to all subscribers.
    let broadcast_result: Value = client
        .request(DaemonRequest::Broadcast {
            input: "hello-sgian\n".to_string(),
        })
        .expect("broadcast should succeed");
    let targeted = broadcast_result["panes"]
        .as_array()
        .expect("broadcast should report targeted panes");
    assert!(
        targeted.iter().any(|p| p == &json!(pane.id)),
        "broadcast should target the created pane"
    );

    // Read the PtyOutput event off the subscriber stream (fan-out delivery).
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut received = false;
    for _ in 0..20 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PtyOutput { pane_id, data }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if pane_id == pane.id && data.contains("hello-sgian") {
                        received = true;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        received,
        "subscriber should receive the broadcast PtyOutput event"
    );

    // Shutdown — the daemon loop exits and the thread joins cleanly.
    daemon.shutdown();
}

/// Helper: spawn a daemon with the given config, subscribe, create a pane
/// whose shell prints the value of `var_name` as `V[<value>]`, and capture
/// the first PtyOutput event containing "V[". Returns the captured data
/// substring so the caller can assert on the printed value.
pub(crate) fn capture_pane_env_print(config: Config, var_name: &str) -> String {
    // The pane shell prints the variable then exits. Using /bin/sh -c keeps
    // the print deterministic and avoids interactive-prompt noise.
    let print_cmd = format!("printf 'V[%s]' \"${var_name}\"");
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        shell_args: Some(vec!["-c".to_string(), print_cmd]),
        ..config
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Subscribe BEFORE creating the pane so we capture the shell's immediate
    // printf output (the shell exits right after printing).
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");

    let _pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut captured = String::new();
    for _ in 0..40 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PtyOutput { data, .. }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if data.contains("V[") {
                        captured = data;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    daemon.shutdown();
    captured
}

/// VAL-SEC-003/004/007 integration: the real spawn_pane path scrubs inherited
/// env vars per the config scrub list, preserves inheritance with no scrub
/// list, and lets an explicit `env` value take precedence over the scrub list.
#[test]
fn env_scrubbing_integration_spawned_pane() {
    // Use a unique variable name so concurrent tests are unaffected. The var
    // is set in the test process env (inherited by the daemon thread) and
    // removed at the end. This is the only test that touches this name.
    let var = "SGIAN_TEST_SCRUB_VAR";
    std::env::set_var(var, "inherited");

    // VAL-SEC-003: scrub list removes the inherited value from the pane.
    let scrubbed = capture_pane_env_print(
        Config {
            scrub_env: vec![var.to_string()],
            ..Default::default()
        },
        var,
    );
    assert!(
        scrubbed.contains("V[]"),
        "scrubbed var should be empty in the pane, got: {scrubbed}"
    );
    assert!(
        !scrubbed.contains("inherited"),
        "scrubbed inherited value must not leak, got: {scrubbed}"
    );

    // VAL-SEC-004: with no scrub list, the inherited value is preserved.
    let inherited = capture_pane_env_print(Config::default(), var);
    assert!(
        inherited.contains("V[inherited]"),
        "default (no scrub) should preserve inherited value, got: {inherited}"
    );

    // VAL-SEC-007: an explicit `env` value takes precedence over the scrub
    // list for the same variable.
    let mut explicit_env = HashMap::new();
    explicit_env.insert(var.to_string(), "cfgval".to_string());
    let precedence = capture_pane_env_print(
        Config {
            scrub_env: vec![var.to_string()],
            env: explicit_env,
            ..Default::default()
        },
        var,
    );
    assert!(
        precedence.contains("V[cfgval]"),
        "explicit env value should win over scrub list, got: {precedence}"
    );

    std::env::remove_var(var);
}
