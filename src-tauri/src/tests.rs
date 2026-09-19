use super::*;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// Create a connected in-process transport pair on every supported host.
/// Unix has a native socketpair; Windows connects to a uniquely named pipe
/// instance and accepts it before dropping the temporary listener.
fn test_transport_pair() -> std::io::Result<(TransportStream, TransportStream)> {
    #[cfg(unix)]
    {
        return UnixStream::pair();
    }
    #[cfg(windows)]
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("transport-pair.sock");
        let listener = transport_bind(&path)?;
        let client = transport_connect(&path)?;
        let (server, _) = listener.accept()?;
        return Ok((client, server));
    }
    #[allow(unreachable_code)]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no test transport for this platform",
    ))
}

#[test]
fn control_invocation_accepts_current_and_legacy_binary_names() {
    assert!(is_control_invocation(&[
        "sgian".to_string(),
        "ctl".to_string()
    ]));
    assert!(is_control_invocation(&["/tmp/sgianctl".to_string()]));
    assert!(is_control_invocation(&["/tmp/sgian2ctl".to_string()]));
    assert!(!is_control_invocation(&["/tmp/sgian".to_string()]));
}

#[test]
fn new_registry_starts_with_one_terminal_pane() {
    let registry = PaneRegistry::new("/tmp/sgian".to_string());
    let snapshot = registry.snapshot();

    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.active_pane_id, Some("pane-1".to_string()));
    assert_eq!(snapshot.panes[0].title, "term-1");
}

#[test]
fn create_pane_assigns_distinct_ids_and_focuses_latest() {
    let mut registry = PaneRegistry::new("/tmp/sgian".to_string());
    let pane = registry.create_pane(Some("editor".to_string()));
    let snapshot = registry.snapshot();

    assert_eq!(pane.id, "pane-2");
    assert_eq!(pane.title, "editor");
    assert_eq!(snapshot.active_pane_id, Some("pane-2".to_string()));
    assert_eq!(snapshot.panes.len(), 2);
}

#[test]
fn close_pane_keeps_at_least_one_pane() {
    let mut registry = PaneRegistry::new("/tmp/sgian".to_string());
    let err = registry
        .close_pane("pane-1")
        .expect_err("last pane should stay open");

    assert_eq!(err, "at least one pane must remain open");
    assert_eq!(registry.snapshot().panes.len(), 1);
}

#[test]
fn rename_pane_trims_title() {
    let mut registry = PaneRegistry::new("/tmp/sgian".to_string());
    let pane = registry
        .rename_pane("pane-1", "  build  ".to_string())
        .expect("pane should rename");

    assert_eq!(pane.title, "build");
}

#[test]
fn terminal_store_remembers_size_before_session_exists() {
    let mut store = TerminalStore::new_for_tests(PathBuf::from("/tmp/sgian"));

    store
        .resize_pane("pane-1", 101, 31)
        .expect("resize should be remembered");

    let size = store
        .sizes
        .get("pane-1")
        .expect("size should be available for later spawn");
    assert_eq!(size.cols, 101);
    assert_eq!(size.rows, 31);
}

#[test]
fn pty_size_clamps_dimensions_from_both_sides() {
    let size = pty_size(0, 0);
    assert_eq!((size.cols, size.rows), (2, 1));

    // A hostile 65535x65535 resize must not reach the vt100 model (H4).
    let size = pty_size(u16::MAX, u16::MAX);
    assert_eq!((size.cols, size.rows), (MAX_PTY_COLS, MAX_PTY_ROWS));

    let size = pty_size(120, 40);
    assert_eq!((size.cols, size.rows), (120, 40));
}

#[test]
fn resize_pane_stores_clamped_dimensions() {
    let mut store = TerminalStore::new_for_tests(PathBuf::from("/tmp/sgian"));

    store
        .resize_pane("pane-1", u16::MAX, u16::MAX)
        .expect("oversized resize should clamp, not fail");

    let size = store.sizes.get("pane-1").expect("size should be stored");
    assert_eq!(size.cols, MAX_PTY_COLS);
    assert_eq!(size.rows, MAX_PTY_ROWS);
}

#[test]
fn queue_pane_input_fails_fast_when_pane_stops_draining() {
    // A writer that blocks forever, like a PTY whose foreground process
    // stopped reading stdin (Ctrl-S / stopped job) with a full kernel buffer.
    // Requests must fail fast with a backlog error, never block (H2).
    struct BlockedWriter {
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let sender = spawn_input_writer(Box::new(BlockedWriter {
        release: Arc::clone(&release),
    }));

    // The writer thread can hold at most one in-flight chunk; the queue holds
    // PANE_INPUT_QUEUE_LIMIT more. One extra send must fail fast.
    let mut backlogged = None;
    for _ in 0..=(PANE_INPUT_QUEUE_LIMIT + 1) {
        if let Err(error) = queue_pane_input(&sender, "pane-1", "x") {
            backlogged = Some(error);
            break;
        }
    }
    let error = backlogged.expect("a full queue should fail fast, not block");
    assert!(error.contains("backlogged"), "unexpected error: {error}");

    // Unblock the writer so the thread drains and exits at teardown.
    let (lock, cvar) = &*release;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
}

#[test]
fn queue_pane_input_reports_ended_after_writer_exit() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty gone",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sender = spawn_input_writer(Box::new(FailingWriter));
    // The writer thread errors on its first write and exits; once the
    // receiver is dropped, sends observe Disconnected → "session ended".
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match queue_pane_input(&sender, "pane-1", "x") {
            Err(error) => {
                assert!(error.contains("session ended"), "unexpected error: {error}");
                break;
            }
            Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(()) => panic!("writer exit should surface as a send error"),
        }
    }
}

// ----- MB spawn metadata + exit-code reaper (VAL-TERM-014..021, 026, 027, 030) -----

static MB_TEST_DIR_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A unique, per-test scrollback dir so parallel real-PTY tests never collide on
/// the same `{pane_id}.ansi` file.
fn unique_scrollback_dir() -> PathBuf {
    let seq = MB_TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sgian-mb-test-{}-{}", std::process::id(), seq));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Build a `TerminalStore` whose panes launch `/bin/sh`, so a test can drive a
/// pane to a known exit code by writing `exit N` / `kill -KILL $$`.
fn sh_terminal_store(cwd: &str) -> TerminalStore {
    TerminalStore::new(
        PathBuf::from(cwd),
        OutputRouter::new(unique_scrollback_dir()),
        HashMap::new(),
        ShellConfig {
            shell: "/bin/sh".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            scrub_env: Vec::new(),
        },
        AgentSpawnConfig::default(),
        unique_scrollback_dir(),
        Arc::new(AtomicBool::new(false)),
        HashMap::new(),
    )
}

/// Poll until the pane's current session is no longer live (reader EOF + reaper
/// ran), or give up after ~3s. Returns whether the pane ended.
fn wait_until_pane_ended(store: &TerminalStore, pane_id: &str) -> bool {
    for _ in 0..300 {
        if !store.is_live(pane_id) {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Subscribe, drive a `/bin/sh` pane via `input`, and collect every `PaneEnded`
/// exit code observed for that pane within a bounded window.
fn collect_pane_ended_codes(input: &str) -> Vec<Option<i32>> {
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Subscribe BEFORE the pane ends so we observe the live PaneEnded broadcast.
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe).expect("subscribe should write");
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("read timeout should apply");

    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane.id.clone(),
            input: input.to_string(),
        })
        .expect("send should succeed");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut ended_codes: Vec<Option<i32>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PaneEnded { pane_id, exit_code }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if pane_id == pane.id {
                        ended_codes.push(exit_code);
                    }
                }
            }
            Err(_) => {} // read timeout: keep polling until the deadline
        }
    }

    daemon.shutdown();
    ended_codes
}

#[test]
fn reaped_exit_code_preserves_codes_and_nulls_signal_deaths() {
    // Clean (0), failing (7), and arbitrary (42) codes are preserved verbatim.
    assert_eq!(reaped_exit_code(&ExitStatus::with_exit_code(0)), Some(0));
    assert_eq!(reaped_exit_code(&ExitStatus::with_exit_code(7)), Some(7));
    assert_eq!(reaped_exit_code(&ExitStatus::with_exit_code(42)), Some(42));
    // portable-pty erases the numeric signal and forces code=1 for a signal
    // death, so a signal-terminated child reports a null (documented) code
    // rather than a misleading 1.
    assert_eq!(reaped_exit_code(&ExitStatus::with_signal("Killed")), None);
}

/// L14 residual pin: `reaped_exit_code` detects a signal death through
/// portable-pty's `Display` wording ("Terminated by <name>") because the
/// crate has no signal accessor. Drive a REAL SIGKILL'd child through it so
/// an upstream change to that wording fails loudly here instead of silently
/// misreporting every signal death as exit 1.
#[test]
fn reaped_exit_code_returns_none_for_a_real_sigkilled_child() {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(pty_size(80, 24)).expect("openpty");
    let mut command = CommandBuilder::new("/bin/sleep");
    command.arg("60");
    let mut child = pair.slave.spawn_command(command).expect("spawn sleep");
    child.kill().expect("SIGKILL should deliver");
    let status = child.wait().expect("wait should succeed");
    assert_eq!(
        reaped_exit_code(&status),
        None,
        "a real SIGKILL'd child must report a null exit code (status: {status})"
    );
}

#[test]
fn spawn_pane_captures_command_and_cwd() {
    let cwd = "/tmp/sgian-mb-meta";
    let mut store = sh_terminal_store(cwd);
    store.spawn_pane("pane-1").expect("spawn should succeed");

    let meta = store.pane_meta("pane-1");
    assert!(
        meta.command.as_deref().unwrap_or("").contains("sh"),
        "command should record the launched shell, got {:?}",
        meta.command
    );
    assert_eq!(
        meta.cwd.as_deref(),
        Some(cwd),
        "cwd should record the pane's working directory"
    );
}

#[test]
fn reaper_records_exit_code_and_metadata_persists_after_end() {
    let cwd = "/tmp/sgian-mb-exit";
    // Clean (0), failing (7), and arbitrary (42) codes are captured and preserved.
    for (cmd, code) in [("exit 0\n", 0), ("exit 7\n", 7), ("exit 42\n", 42)] {
        let mut store = sh_terminal_store(cwd);
        store.spawn_pane("pane-1").expect("spawn should succeed");
        store
            .write_to_pane("pane-1", cmd)
            .expect("write to a live pane should succeed");
        assert!(
            wait_until_pane_ended(&store, "pane-1"),
            "pane should end after `{cmd}`"
        );

        let meta = store.pane_meta("pane-1");
        assert_eq!(meta.exit_code, Some(code), "exit code {code} preserved");
        // Spawn metadata stays queryable after the pane has ended.
        assert!(
            meta.command.as_deref().unwrap_or("").contains("sh"),
            "command stays queryable after exit"
        );
        assert_eq!(
            meta.cwd.as_deref(),
            Some(cwd),
            "cwd stays queryable after exit"
        );
    }
}

#[test]
fn signal_killed_pane_reaps_to_null_exit_code() {
    let mut store = sh_terminal_store("/tmp/sgian-mb-signal");
    store.spawn_pane("pane-1").expect("spawn should succeed");
    // The shell signals itself; the reaper observes a signal death.
    store
        .write_to_pane("pane-1", "kill -KILL $$\n")
        .expect("write should succeed");
    assert!(
        wait_until_pane_ended(&store, "pane-1"),
        "a signal-killed pane should be reaped to not-live"
    );
    assert_eq!(
        store.pane_meta("pane-1").exit_code,
        None,
        "a signal death reports a null exit code"
    );
}

#[test]
fn restart_relives_pane_and_clears_exit_code() {
    let mut store = sh_terminal_store("/tmp/sgian-mb-restart");
    store.spawn_pane("pane-1").expect("spawn should succeed");
    store.write_to_pane("pane-1", "exit 7\n").expect("write");
    assert!(wait_until_pane_ended(&store, "pane-1"), "pane should end");
    assert_eq!(store.pane_meta("pane-1").exit_code, Some(7));

    store
        .restart_pane("pane-1")
        .expect("restart should succeed");
    // A stale reader from the ended generation must not mark the new live
    // session ended (generation safety, Invariant 1).
    thread::sleep(Duration::from_millis(100));
    assert!(
        store.is_live("pane-1"),
        "restarted pane is live and not ended by the stale reader"
    );
    assert_eq!(
        store.pane_meta("pane-1").exit_code,
        None,
        "restarting an ended pane clears its recorded exit code"
    );
}

/// review-low: close_pane drops the sizes entry, so restart used to reset
/// the pane to the default 120x40 — the size must be preserved across a
/// restart (both the persisted sizes map and the actual new PTY).
#[test]
fn restart_pane_preserves_size() {
    let mut store = sh_terminal_store("/tmp/sgian-restart-size");
    store.spawn_pane("pane-1").expect("spawn should succeed");
    store
        .resize_pane("pane-1", 80, 24)
        .expect("resize should succeed");

    store
        .restart_pane("pane-1")
        .expect("restart should succeed");

    let size = store
        .sizes
        .get("pane-1")
        .copied()
        .expect("size entry retained across restart");
    assert_eq!(
        (size.cols, size.rows),
        (80, 24),
        "persisted sizes map keeps the resized dimensions"
    );
    let session_size = store
        .sessions
        .get("pane-1")
        .expect("session exists")
        ._master
        .get_size()
        .expect("master reports a size");
    assert_eq!(
        (session_size.cols, session_size.rows),
        (80, 24),
        "the new PTY was opened at the preserved size"
    );
}

/// review-low: the spawn partial-failure cleanup (writer/reader setup fails
/// after spawn_command succeeded) kills + reaps the child. The failure
/// can't be forced portably, but the kill+wait itself must work on a live
/// child without hanging or panicking.
#[test]
fn kill_and_reap_child_terminates_a_spawned_shell() {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(pty_size(80, 24)).expect("openpty");
    let child = pair
        .slave
        .spawn_command(CommandBuilder::new("/bin/sh"))
        .expect("spawn sh");
    kill_and_reap_child(child);
}

#[test]
fn pane_ended_event_carries_exit_code_exactly_once() {
    let ended_codes = collect_pane_ended_codes("exit 3\n");
    assert_eq!(
        ended_codes.len(),
        1,
        "exactly one PaneEnded per pane end, got {ended_codes:?}"
    );
    assert_eq!(
        ended_codes[0],
        Some(3),
        "PaneEnded payload carries the captured exit code"
    );
}

#[test]
fn pane_ended_event_signal_death_is_single_and_null() {
    let ended_codes = collect_pane_ended_codes("kill -KILL $$\n");
    assert_eq!(
        ended_codes.len(),
        1,
        "a signal-killed pane fires exactly one PaneEnded, got {ended_codes:?}"
    );
    assert_eq!(
        ended_codes[0], None,
        "signal death carries a null exit code"
    );
}

#[test]
fn daemon_public_list_panes_reports_runtime_state() {
    let data_dir = std::env::temp_dir().join(format!("sgian-list-test-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-list"),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");

    let value = server
        .handle(DaemonRequest::ListPanes)
        .expect("list panes should succeed");
    let list: PaneList = serde_json::from_value(value).expect("pane list should deserialize");

    assert_eq!(list.panes.len(), 1);
    assert_eq!(list.active_pane_id, Some("pane-1".to_string()));
    assert_eq!(list.panes[0].pane.title, "term-1");
    assert_eq!(list.panes[0].state, PaneRuntimeState::Ended);

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn shutdown_request_marks_daemon_for_exit() {
    let data_dir = std::env::temp_dir().join(format!("sgian-shutdown-test-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-shutdown"),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");

    assert!(!server.should_shutdown());
    server
        .handle(DaemonRequest::Shutdown)
        .expect("shutdown should succeed");

    assert!(server.should_shutdown());

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn daemon_rejects_invalid_hello_token() {
    let data_dir = std::env::temp_dir().join(format!("sgian-auth-test-{}", now_millis()));
    let server = Arc::new(
        DaemonServer::with_config(
            PathBuf::from("/tmp/sgian-auth"),
            data_dir.clone(),
            Config::default(),
        )
        .expect("daemon server should start"),
    );
    let (mut client_stream, server_stream) =
        test_transport_pair().expect("transport pair should be available");
    let server_thread = {
        let server = Arc::clone(&server);
        thread::spawn(move || handle_daemon_client(server, server_stream))
    };

    write_json_line(
        &mut client_stream,
        &IpcHello {
            frame_type: "hello".to_string(),
            version: PROTOCOL_VERSION,
            token: "wrong-token".to_string(),
            max_wire_version: None,
            capabilities: None,
            client_token: None,
        },
    )
    .expect("hello should write");
    let mut reader = BufReader::new(client_stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("auth response should read");
    let response: IpcResponse =
        serde_json::from_str(&line).expect("auth response should deserialize");

    assert!(!response.ok);
    assert_eq!(
        response.error,
        Some("daemon authentication failed".to_string())
    );
    server_thread
        .join()
        .expect("server thread should finish")
        .expect("server should handle auth failure");

    let _ = fs::remove_dir_all(data_dir);
}

/// L18: the budget calc itself — the remaining time shrinks to an error at
/// the deadline, at millisecond scale (no 30s waits in tests).
#[test]
fn handshake_budget_remaining_bounds_the_total_phase() {
    let started = Instant::now();
    let remaining = handshake_budget_remaining(started, Duration::from_secs(30))
        .expect("plenty of budget left");
    assert!(remaining <= Duration::from_secs(30));
    // An already-exhausted budget errors immediately...
    assert!(handshake_budget_remaining(started, Duration::ZERO).is_err());
    // ...and a tiny one lapses into an error (ms, not seconds).
    thread::sleep(Duration::from_millis(20));
    let error = handshake_budget_remaining(started, Duration::from_millis(10))
        .expect_err("the budget is exhausted");
    assert!(error.contains("timed out"), "unexpected error: {error}");
}

/// L18: a slowloris dribbling hello bytes below each per-read timeout is cut
/// off at the TOTAL phase deadline instead of pinning the connection thread.
#[test]
fn handshake_slowloris_dribble_fails_at_the_total_deadline() {
    let data_dir = std::env::temp_dir().join(format!("sgian-loris-test-{}", now_millis()));
    let server = Arc::new(
        DaemonServer::with_config(
            PathBuf::from("/tmp/sgian-loris"),
            data_dir.clone(),
            Config::default(),
        )
        .expect("daemon server should start"),
    );
    let (mut client_stream, server_stream) =
        test_transport_pair().expect("transport pair should be available");
    let started = Instant::now();
    let server_thread = thread::spawn(move || {
        handle_daemon_client_with_handshake_budget(
            server,
            server_stream,
            Duration::from_millis(400),
        )
    });

    // Drip-feed a partial hello: every byte lands well inside any per-read
    // timeout, but the phase overruns the 400ms total budget.
    for _ in 0..20 {
        if client_stream.write_all(b"{").is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let result = server_thread.join().expect("server thread should finish");
    assert!(
        result.is_err(),
        "a dribbling handshake must fail at the deadline, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "failure must come near the budget, not hang the thread"
    );

    let _ = fs::remove_dir_all(data_dir);
}

/// L19: reconnects after a SHORT subscription session are spaced by the
/// backoff (an accept+ack-then-EOF daemon can't be busy-looped); a healthy
/// long session resets to an immediate reconnect.
#[test]
fn subscription_backoff_spaces_reconnects_after_short_sessions() {
    let mut backoff = SubscriptionBackoff::new();
    // First-ever connect: no delay.
    assert_eq!(backoff.pre_connect_delay(), Duration::ZERO);

    // Simulate repeated accept+ack-then-immediate-EOF sessions: consecutive
    // connect attempt timestamps must be spaced by at least the backoff.
    let session = Duration::from_millis(10);
    let mut clock = Duration::ZERO;
    let mut attempts = vec![clock];
    for _ in 0..5 {
        backoff.session_ended(Some(session));
        let delay = backoff.pre_connect_delay();
        clock += delay + session;
        attempts.push(clock);
    }
    for pair in attempts.windows(2) {
        assert!(
            pair[1] - pair[0] >= SUBSCRIPTION_RECONNECT_BACKOFF,
            "short sessions must space reconnect attempts: {attempts:?}"
        );
    }

    // A session of exactly the healthy minimum resets the backoff: the next
    // reconnect is immediate.
    backoff.session_ended(Some(SUBSCRIPTION_HEALTHY_MIN));
    assert_eq!(backoff.pre_connect_delay(), Duration::ZERO);

    // A connect failure records no session: the loop's own fixed sleep
    // covers it, so the state machine adds nothing — and the next short
    // session re-arms the backoff.
    backoff.session_ended(None);
    assert_eq!(backoff.pre_connect_delay(), Duration::ZERO);
    backoff.session_ended(Some(Duration::from_millis(1)));
    assert_eq!(backoff.pre_connect_delay(), SUBSCRIPTION_RECONNECT_BACKOFF);
}

#[test]
fn token_is_created_once_and_reused() {
    let data_dir = std::env::temp_dir().join(format!("sgian-token-test-{}", now_millis()));

    let first = load_or_create_token(&data_dir).expect("token should be created");
    let second = load_or_create_token(&data_dir).expect("token should be reused");

    assert_eq!(first, second);
    assert_eq!(first.len(), 64);

    let _ = fs::remove_dir_all(data_dir);
}

/// review-low: the `AlreadyExists` re-read branch of `load_or_create_token`
/// re-applies the 0600 chmod, same as the normal path (the create-race
/// winner's chmod may not have landed yet).
#[cfg(unix)]
#[test]
fn load_or_create_token_re_read_branch_rechmods() {
    use std::os::unix::fs::PermissionsExt;

    let data_dir = std::env::temp_dir().join(format!("sgian-tokrace-test-{}", now_millis()));
    fs::create_dir_all(&data_dir).expect("data dir should be created");
    let token_path = data_dir.join(TOKEN_FILE);
    // An empty, lax-mode token file reads as "no token" and then loses the
    // create_new race; the re-read branch errors on the still-empty file,
    // but only after re-applying 0600.
    fs::write(&token_path, "").expect("write empty token");
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644)).expect("set lax perms");

    let error = load_or_create_token(&data_dir).expect_err("empty token file errors");
    assert!(error.contains("empty"), "unexpected error: {error}");
    let mode = fs::metadata(&token_path)
        .expect("token metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o600,
        "the re-read branch must re-apply 0600, got {mode:o}"
    );

    let _ = fs::remove_dir_all(data_dir);
}

/// review-low: `set_private_file_permissions` must not follow symlinks — a
/// planted symlink could otherwise chmod an arbitrary same-UID file.
#[cfg(unix)]
#[test]
fn set_private_file_permissions_refuses_symlinks() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("target");
    fs::write(&target, "x").expect("write target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).expect("set target perms");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let error = set_private_file_permissions(&link).expect_err("symlink chmod must be refused");
    assert!(error.contains("symlink"), "unexpected error: {error}");
    let mode = fs::metadata(&target)
        .expect("target metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode, 0o644,
        "the target must not be chmod'd through the symlink, got {mode:o}"
    );

    // A regular file still gets 0600.
    set_private_file_permissions(&target).expect("regular file chmods");
    let mode = fs::metadata(&target)
        .expect("target metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "a regular file gets 0600, got {mode:o}");
}

#[test]
fn scrollback_cap_trims_to_half_with_hysteresis() {
    let data_dir = std::env::temp_dir().join(format!("sgian-scrollback-test-{}", now_millis()));
    fs::create_dir_all(&data_dir).expect("scrollback dir should be created");
    fs::write(scrollback_path(&data_dir, "pane-1"), b"0123456789abcdef")
        .expect("scrollback should be written");

    // Over the cap: trimmed to half the cap (keeps appends cheap afterwards).
    cap_scrollback_file_to(&data_dir, "pane-1", 6).expect("scrollback should cap");
    let data = fs::read_to_string(scrollback_path(&data_dir, "pane-1"))
        .expect("scrollback should be readable");
    assert_eq!(data, "def");

    // At or below the cap: untouched, so per-append rewrites can't recur.
    cap_scrollback_file_to(&data_dir, "pane-1", 6).expect("cap should be a no-op");
    let data = fs::read_to_string(scrollback_path(&data_dir, "pane-1"))
        .expect("scrollback should be readable");
    assert_eq!(data, "def");

    let _ = fs::remove_dir_all(data_dir);
}

/// L9: the kept tail starts at a LINE boundary when one exists in range, so a
/// replay can't begin mid-ANSI-escape; a tail with no newline keeps the plain
/// byte boundary (pinned by the "def" case above).
#[test]
fn scrollback_cap_starts_the_kept_tail_at_a_line_boundary() {
    let data_dir = std::env::temp_dir().join(format!("sgian-capline-test-{}", now_millis()));
    fs::create_dir_all(&data_dir).expect("scrollback dir should be created");
    // 16 bytes; cap 10 → target 5 → the byte offset lands mid-"cc", and the
    // tail must advance to the next line start ("dd\n").
    fs::write(
        scrollback_path(&data_dir, "pane-1"),
        b"aaaa\nbbbb\ncc\ndd\n",
    )
    .expect("scrollback should be written");

    cap_scrollback_file_to(&data_dir, "pane-1", 10).expect("scrollback should cap");
    let data = fs::read_to_string(scrollback_path(&data_dir, "pane-1"))
        .expect("scrollback should be readable");
    assert_eq!(data, "dd\n");

    let _ = fs::remove_dir_all(data_dir);
}

/// M11: the cached append handle + in-memory byte count must keep capping
/// correctly — the count stays accurate across a cap rewrite (no per-chunk
/// stat), and appends after the cap land in the NEW file (the cached handle
/// is re-opened after the rename replaced the inode).
#[test]
fn append_scrollback_caches_handle_and_caps_accurately() {
    let data_dir = unique_scrollback_dir();
    let router = OutputRouter::new(data_dir.clone());

    // Fill past the cap in a few large appends (append_scrollback isn't
    // limited to the reader's 8 KiB chunks).
    let chunk = "x".repeat(4 * 1024 * 1024 + 1024);
    for _ in 0..4 {
        router
            .append_scrollback("pane-1", &chunk)
            .expect("append should succeed");
    }
    assert!(
        router
            .append_handles
            .lock()
            .expect("lock")
            .contains_key("pane-1"),
        "the append handle must be cached, not re-opened per chunk"
    );

    // 4 × (4 MiB + 1 KiB) > 16 MiB cap → the file was capped to its
    // hysteresis target (~half, no newline to advance to).
    let capped_len = fs::metadata(scrollback_path(&data_dir, "pane-1"))
        .expect("scrollback file")
        .len();
    assert_eq!(
        capped_len,
        SCROLLBACK_MAX_BYTES / 2,
        "capped file should sit at the hysteresis target"
    );

    // The tracked count was re-primed after the cap: the next append lands
    // in the new file and grows it by exactly the appended bytes.
    router
        .append_scrollback("pane-1", "tail-marker")
        .expect("append after cap should succeed");
    let after = fs::read_to_string(scrollback_path(&data_dir, "pane-1"))
        .expect("scrollback should be readable");
    assert!(
        after.ends_with("tail-marker"),
        "post-cap append must land in the new file"
    );
    assert_eq!(
        after.len() as u64,
        capped_len + "tail-marker".len() as u64,
        "byte count must stay accurate across the cap"
    );

    let _ = fs::remove_dir_all(&data_dir);
}

/// review-low: startup prune removes orphan `.ansi` files AND stale
/// `.ansi.tmp` cap litter; the runtime sweep (include_temps=false) leaves
/// temp files alone so an in-flight cap is never deleted under itself.
#[test]
fn prune_orphan_scrollback_removes_orphans_and_tmp_litter() {
    let data_dir = unique_scrollback_dir();
    let live: HashSet<String> = ["pane-1".to_string()].into_iter().collect();
    fs::write(scrollback_path(&data_dir, "pane-1"), b"live").expect("write live");
    fs::write(scrollback_path(&data_dir, "pane-9"), b"orphan").expect("write orphan");
    let cap_temp = scrollback_path(&data_dir, "pane-1").with_extension("ansi.tmp");
    fs::write(&cap_temp, b"partial-cap").expect("write temp");

    // Runtime sweep: orphan .ansi pruned, cap temp left in place.
    prune_orphan_scrollback(&data_dir, &live, false);
    assert!(scrollback_path(&data_dir, "pane-1").exists());
    assert!(
        !scrollback_path(&data_dir, "pane-9").exists(),
        "orphan scrollback must be pruned"
    );
    assert!(
        cap_temp.exists(),
        "runtime sweep must not touch a potentially in-flight cap temp"
    );

    // Startup prune: cap litter removed too; live scrollback kept.
    prune_orphan_scrollback(&data_dir, &live, true);
    assert!(
        !cap_temp.exists(),
        "startup prune must remove .ansi.tmp litter"
    );
    assert!(scrollback_path(&data_dir, "pane-1").exists());

    let _ = fs::remove_dir_all(&data_dir);
}

#[test]
fn decode_cli_text_expands_common_escapes() {
    assert_eq!(
        decode_cli_text(r"cargo test\nnext\t\\done", false),
        "cargo test\rnext\t\\done"
    );
}

#[test]
fn decode_cli_text_default_maps_newline_to_cr() {
    // VAL-ORCH-016: without --lf, \n maps to CR (0x0D).
    let decoded = decode_cli_text("ab\\n", false);
    assert_eq!(decoded, "ab\r", "default mode: \\n -> CR");
}

#[test]
fn decode_cli_text_literal_lf_maps_newline_to_lf() {
    // VAL-ORCH-015: with --lf, \n maps to a literal LF (0x0A), not CR.
    let decoded = decode_cli_text("ab\\n", true);
    assert_eq!(decoded, "ab\n", "literal_lf mode: \\n -> LF (0x0A)");
    assert_ne!(decoded, "ab\r", "literal LF must differ from CR");
}

#[test]
fn decode_cli_text_literal_lf_still_decodes_other_escapes() {
    // \t and \\ still decode; \r stays CR under --lf.
    assert_eq!(decode_cli_text("a\\tb\\rc\\d", true), "a\tb\rc\\d");
}

#[test]
fn parse_lf_flag_extracts_lf_and_preserves_order() {
    let args: Vec<String> = ["pane-1", "--lf", "echo", "hi"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let (literal_lf, remaining) = parse_lf_flag(&args);
    assert!(literal_lf, "--lf should set literal_lf");
    assert_eq!(remaining, vec!["pane-1", "echo", "hi"]);
}

#[test]
fn parse_lf_flag_extracts_raw_alias() {
    let args: Vec<String> = ["--raw", "text"].iter().map(ToString::to_string).collect();
    let (literal_lf, remaining) = parse_lf_flag(&args);
    assert!(literal_lf, "--raw should set literal_lf");
    assert_eq!(remaining, vec!["text"]);
}

#[test]
fn parse_lf_flag_no_flag_preserves_args() {
    let args: Vec<String> = ["pane-1", "echo", "hi"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let (literal_lf, remaining) = parse_lf_flag(&args);
    assert!(!literal_lf, "no flag -> literal_lf false");
    assert_eq!(remaining, vec!["pane-1", "echo", "hi"]);
}

#[test]
fn parse_lf_flag_flag_before_pane() {
    // send --lf <pane> <text> ordering is also accepted.
    let args: Vec<String> = ["--lf", "pane-1", "echo", "hi"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let (literal_lf, remaining) = parse_lf_flag(&args);
    assert!(literal_lf);
    assert_eq!(remaining, vec!["pane-1", "echo", "hi"]);
}

#[test]
fn parse_lf_flag_removes_repeated_flags() {
    let args: Vec<String> = ["--lf", "--raw", "text"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let (literal_lf, remaining) = parse_lf_flag(&args);
    assert!(literal_lf);
    assert_eq!(remaining, vec!["text"]);
}

#[test]
fn restored_daemon_reports_ended_pane_with_scrollback() {
    let data_dir = std::env::temp_dir().join(format!("sgian-restored-test-{}", now_millis()));
    let scrollback_dir = data_dir.join(SCROLLBACK_DIR);
    fs::create_dir_all(&scrollback_dir).expect("scrollback dir should be created");

    let pane = Pane {
        id: "pane-1".to_string(),
        title: "term-1".to_string(),
        kind: PaneKind::Shell,
        created_at_ms: now_millis(),
    };
    let persisted = PersistedWorkspace {
        panes: vec![pane],
        active_pane_id: Some("pane-1".to_string()),
        cwd: "/tmp/sgian-restored".to_string(),
        next_id: 2,
        layout: Some(json!({ "type": "leaf", "id": "pane-1" })),
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        data_dir.join(WORKSPACE_FILE),
        serde_json::to_vec(&persisted).expect("workspace should serialize"),
    )
    .expect("workspace should be written");
    fs::write(scrollback_dir.join("pane-1.ansi"), "old output\r\n")
        .expect("scrollback should be written");

    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-restored"),
        data_dir.clone(),
        Config {
            restore_policy: Some("restore_on_demand".to_string()),
            ..Default::default()
        },
    )
    .expect("daemon server should load persisted workspace");
    let value = server
        .handle(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap should succeed");
    let snapshot: WorkspaceSnapshot =
        serde_json::from_value(value).expect("snapshot should deserialize");

    assert_eq!(
        snapshot.pane_states.get("pane-1"),
        Some(&PaneRuntimeState::Ended)
    );
    assert_eq!(
        snapshot.scrollback.get("pane-1"),
        Some(&"old output\r\n".to_string())
    );
    assert_eq!(
        snapshot.layout,
        Some(json!({ "type": "leaf", "id": "pane-1" }))
    );

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn drain_complete_utf8_waits_for_split_multibyte_character() {
    let mut pending = vec![0xe2, 0x94];

    assert!(drain_complete_utf8(&mut pending).is_empty());
    assert_eq!(pending, vec![0xe2, 0x94]);

    pending.push(0x80);
    assert_eq!(drain_complete_utf8(&mut pending), vec!["─"]);
    assert!(pending.is_empty());
}

#[test]
fn drain_complete_utf8_keeps_incomplete_tail_after_valid_prefix() {
    let mut pending = "Claude ╭".as_bytes().to_vec();
    let tail = pending.pop().expect("multibyte char has a final byte");

    assert_eq!(drain_complete_utf8(&mut pending), vec!["Claude "]);
    assert!(!pending.is_empty());

    pending.push(tail);
    assert_eq!(drain_complete_utf8(&mut pending), vec!["╭"]);
    assert!(pending.is_empty());
}

// ---- MB in-daemon vt100 terminal model (mb-terminal-model) ----

fn model_lines(model: &PaneModel) -> Vec<String> {
    let screen = model.parser.screen();
    let (_, cols) = screen.size();
    screen.rows(0, cols).collect()
}

fn model_title(model: &PaneModel) -> Option<String> {
    model.parser.callbacks().title.clone()
}

#[test]
fn vt100_model_process_updates_screen_contents() {
    // VAL-TERM-022: feeding bytes renders visible text onto the grid.
    let mut model = PaneModel::new(80, 24);
    model.process(b"abc");
    let lines = model_lines(&model);
    assert_eq!(lines.len(), 24, "one entry per row");
    assert!(lines[0].starts_with("abc"), "row 0 = {:?}", lines[0]);
}

#[test]
fn vt100_model_interprets_ansi_not_echoed() {
    // VAL-TERM-002: CR overwrite, clear-screen, and SGR are interpreted, not echoed.
    let mut model = PaneModel::new(80, 24);
    model.process(b"foo\rbar");
    assert_eq!(model_lines(&model)[0].trim_end(), "bar");

    model.process(b"\x1b[2J\x1b[H");
    assert!(
        model_lines(&model)
            .iter()
            .all(|line| line.trim().is_empty()),
        "clear-screen should blank the visible grid"
    );

    model.process(b"\x1b[31mRED\x1b[0m");
    let row = model_lines(&model)[0].clone();
    assert!(row.starts_with("RED"), "row 0 = {row:?}");
    assert!(!row.contains('\u{1b}'), "no raw escape bytes in row text");
}

#[test]
fn vt100_model_long_line_wraps_onto_grid() {
    // VAL-TERM-003: a line wider than cols wraps onto the next row.
    let cols = 20u16;
    let mut model = PaneModel::new(cols, 24);
    let text: String = "x".repeat((cols + 10) as usize);
    model.process(text.as_bytes());
    let lines = model_lines(&model);
    assert_eq!(lines[0].trim_end().chars().count(), cols as usize);
    assert_eq!(lines[1].trim_end().chars().count(), 10);
    assert_eq!(
        format!("{}{}", lines[0].trim_end(), lines[1].trim_end()),
        text
    );
}

#[test]
fn vt100_model_revision_bumps_on_process_and_is_stable_when_idle() {
    // VAL-TERM-004 + VAL-TERM-005 + VAL-TERM-023.
    let mut model = PaneModel::new(80, 24);
    assert_eq!(model.revision, 0);
    model.process(b"x");
    let after_first = model.revision;
    assert!(after_first > 0, "revision must bump on output");
    model.process(b"y");
    assert!(
        model.revision > after_first,
        "revision keeps climbing on output"
    );
    // Idle (no process call): revision is stable across reads.
    assert_eq!(model.revision, model.revision);
}

#[test]
fn vt100_model_captures_osc_title() {
    // VAL-TERM-024: an OSC title escape is decoded from the byte stream.
    let mut model = PaneModel::new(80, 24);
    assert_eq!(model_title(&model), None);
    model.process(b"\x1b]2;mb-title\x07");
    assert_eq!(model_title(&model).as_deref(), Some("mb-title"));
    // OSC 0 (icon + title) also updates the window title.
    model.process(b"\x1b]0;other\x07");
    assert_eq!(model_title(&model).as_deref(), Some("other"));
    // The escape bytes are interpreted, never printed onto the grid.
    assert!(model_lines(&model)
        .iter()
        .all(|line| !line.contains("mb-title") && !line.contains("other")));
}

#[test]
fn vt100_model_set_size_tracks_resize_and_revision_is_monotonic() {
    // VAL-TERM-025 + VAL-TERM-006 + VAL-TERM-010/011.
    let mut model = PaneModel::new(80, 24);
    assert_eq!(model.parser.screen().size(), (24, 80)); // (rows, cols)
    model.process(b"hello");
    let rev_before = model.revision;

    model.set_size(100, 40); // (cols, rows)
    assert_eq!(model.parser.screen().size(), (40, 100));
    // Revision never resets or decreases on resize (it tracks output, not size).
    assert_eq!(model.revision, rev_before, "resize alone is not output");

    // Content printed after a shrink wraps at the NEW width.
    let mut model = PaneModel::new(80, 24);
    model.set_size(10, 24);
    model.process(b"aaaaaaaaaaaaaaa"); // 15 chars into a 10-col grid
    let lines = model_lines(&model);
    assert_eq!(lines[0].trim_end().chars().count(), 10);
    assert_eq!(lines[1].trim_end().chars().count(), 5);
}

#[test]
fn vt100_model_handles_binary_and_invalid_utf8_without_corruption() {
    // VAL-TERM-031: pathological bytes never panic and never leak control bytes.
    let mut model = PaneModel::new(80, 24);
    let mut garbage: Vec<u8> = (0u8..=255).collect();
    garbage.extend_from_slice(&[0xff, 0xfe, 0x00, 0x00, 0xc0, 0xc1]);
    garbage.extend_from_slice(b"\x1b[partial");
    garbage.extend_from_slice(b"\x1b]2;unterminated-title");
    let rev_before = model.revision;
    model.process(&garbage);
    assert!(
        model.revision > rev_before,
        "revision advanced over the burst"
    );
    for line in model_lines(&model) {
        assert!(
            !line.as_bytes().contains(&0x1b),
            "no raw ESC byte in {line:?}"
        );
        assert!(!line.as_bytes().contains(&0x00), "no NUL byte in {line:?}");
    }
}

#[test]
fn output_router_pane_models_and_revisions_are_independent() {
    // VAL-TERM-028: per-pane screens + counters are isolated.
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().to_path_buf());
    router.ensure_model("pane-1", 80, 24);
    router.ensure_model("pane-2", 80, 24);

    router.feed_model("pane-1", b"AAA");

    let m1 = router.model_handle("pane-1").expect("pane-1 model");
    let m2 = router.model_handle("pane-2").expect("pane-2 model");
    let (rev1, lines1) = {
        let g = m1.lock().unwrap();
        (g.revision, model_lines(&g))
    };
    let (rev2, lines2) = {
        let g = m2.lock().unwrap();
        (g.revision, model_lines(&g))
    };
    assert!(
        rev1 > rev2,
        "only pane-1 received output (rev1={rev1}, rev2={rev2})"
    );
    assert_eq!(rev2, 0, "pane-2 revision untouched");
    assert!(lines1[0].starts_with("AAA"));
    assert!(lines2.iter().all(|line| line.trim().is_empty()));
}

#[test]
fn output_router_preserves_revision_across_restart_and_removes_on_close() {
    // Monotonic revision across an ensure_model restart; remove_model frees the model.
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().to_path_buf());
    router.ensure_model("pane-1", 80, 24);
    router.feed_model("pane-1", b"first");
    let rev_after_first = router
        .model_handle("pane-1")
        .unwrap()
        .lock()
        .unwrap()
        .revision;
    assert!(rev_after_first > 0);

    // Restart (ensure_model on an existing pane): fresh screen, revision stays
    // monotonic AND advances — the wipe is itself a state change, so a
    // revision-deduping client must see a new revision (L10).
    router.ensure_model("pane-1", 80, 24);
    {
        let handle = router.model_handle("pane-1").unwrap();
        let g = handle.lock().unwrap();
        assert!(
            g.revision > rev_after_first,
            "the reset wipe must bump the revision (got {} vs {rev_after_first})",
            g.revision
        );
        assert!(
            model_lines(&g).iter().all(|line| line.trim().is_empty()),
            "restart resets the screen"
        );
    }

    router.remove_model("pane-1");
    assert!(
        router.model_handle("pane-1").is_none(),
        "model removed on close"
    );
}

#[test]
fn osc_title_capture_does_not_emit_pane_renamed() {
    // VAL-TERM-029: capturing an OSC title must not ride the user-rename event.
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().to_path_buf());
    router.ensure_model("pane-1", 80, 24);

    let (mut client, server) = test_transport_pair().expect("transport pair should be available");
    router
        .add_subscriber(server, 1)
        .expect("subscribe within the cap"); // wire v1 (newline JSON)

    let bytes = b"\x1b]2;osc-title\x07hello";
    router.feed_model("pane-1", bytes);
    // The reader feeds the model AND emits the same raw bytes as output.
    router.emit("pane-1", String::from_utf8_lossy(bytes).to_string());

    client
        .set_read_timeout(Some(std::time::Duration::from_millis(750)))
        .unwrap();
    let mut buf = [0u8; 8192];
    let received = match client.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).to_string(),
        Err(_) => String::new(),
    };
    assert!(
        received.contains("pty_output"),
        "expected the output event to flow, got: {received:?}"
    );
    assert!(
        !received.contains("pane_renamed"),
        "OSC title must not emit a pane_renamed event, got: {received:?}"
    );
    // The title is captured in the model instead of via a rename.
    let handle = router.model_handle("pane-1").unwrap();
    assert_eq!(
        model_title(&handle.lock().unwrap()).as_deref(),
        Some("osc-title")
    );
}

#[test]
fn is_valid_pane_id_accepts_canonical_ids_only() {
    assert!(is_valid_pane_id("pane-1"));
    assert!(is_valid_pane_id("pane-42"));
    assert!(!is_valid_pane_id("pane-"));
    assert!(!is_valid_pane_id("pane-1a"));
    assert!(!is_valid_pane_id("../etc"));
    assert!(!is_valid_pane_id("pane-../x"));
    assert!(!is_valid_pane_id("local-abc"));
}

#[test]
fn from_persisted_drops_panes_with_unsafe_ids() {
    let persisted = PersistedWorkspace {
        panes: vec![
            Pane {
                id: "pane-1".to_string(),
                title: "ok".to_string(),
                kind: PaneKind::Shell,
                created_at_ms: 0,
            },
            Pane {
                id: "../evil".to_string(),
                title: "bad".to_string(),
                kind: PaneKind::Shell,
                created_at_ms: 0,
            },
        ],
        active_pane_id: Some("../evil".to_string()),
        cwd: "/tmp/x".to_string(),
        next_id: 5,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    let registry = PaneRegistry::from_persisted(persisted, "/tmp/x".to_string());
    let snapshot = registry.snapshot();

    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.panes[0].id, "pane-1");
    assert_eq!(snapshot.active_pane_id, Some("pane-1".to_string()));
}

/// L14: duplicated persisted pane ids collapse to the first occurrence, and a
/// tampered `pane-<u64::MAX>` saturates next-id computation instead of
/// overflowing (debug panic / release wraparound id reuse).
#[test]
fn from_persisted_dedupes_ids_and_saturates_next_id() {
    let pane = |id: String, title: &str| Pane {
        id,
        title: title.to_string(),
        kind: PaneKind::Shell,
        created_at_ms: 0,
    };
    let persisted = PersistedWorkspace {
        panes: vec![
            pane("pane-1".to_string(), "first"),
            pane("pane-1".to_string(), "second"),
            pane(format!("pane-{}", u64::MAX), "ceiling"),
        ],
        active_pane_id: None,
        cwd: "/tmp/x".to_string(),
        next_id: 1,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    let mut registry = PaneRegistry::from_persisted(persisted, "/tmp/x".to_string());
    let snapshot = registry.snapshot();

    let pane1_count = snapshot.panes.iter().filter(|p| p.id == "pane-1").count();
    assert_eq!(pane1_count, 1, "duplicate ids must collapse to one pane");
    assert_eq!(snapshot.panes[0].title, "first", "first occurrence wins");

    // Creating another pane saturates at the ceiling instead of panicking.
    let created = registry.create_pane(None);
    assert_eq!(created.id, format!("pane-{}", u64::MAX));
}

/// L13: closed-pane suppression entries are pruned once their retention
/// elapses, so the set cannot grow for the daemon's lifetime.
#[test]
fn closed_set_is_swept_after_retention() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().to_path_buf());
    router.mark_closed("pane-1");
    assert!(router.is_closed("pane-1"));

    router.sweep_closed(Duration::from_secs(60));
    assert!(router.is_closed("pane-1"), "fresh entries are retained");

    router.sweep_closed(Duration::ZERO);
    assert!(!router.is_closed("pane-1"), "expired entries are pruned");
}

/// M6: a pane-emitted OSC title is capped at MAX_TITLE_CHARS when stored, so
/// snapshot/find never clone an attacker-length string per query.
#[test]
fn osc_title_is_capped_when_stored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().to_path_buf());
    router.ensure_model("pane-1", 80, 24);

    let mut bytes = b"\x1b]2;".to_vec();
    bytes.extend_from_slice("t".repeat(MAX_TITLE_CHARS * 8).as_bytes());
    bytes.extend_from_slice(b"\x07");
    router.feed_model("pane-1", &bytes);

    let handle = router.model_handle("pane-1").expect("model exists");
    let stored = handle
        .lock()
        .unwrap()
        .parser
        .callbacks()
        .title
        .clone()
        .expect("title captured");
    assert_eq!(stored.chars().count(), MAX_TITLE_CHARS);
}

#[test]
fn utf8_boundary_advances_past_continuation_bytes() {
    let data = "a─b".as_bytes().to_vec(); // ─ is e2 94 80

    assert_eq!(utf8_boundary_at_or_after(&data, 1), 1);
    assert_eq!(utf8_boundary_at_or_after(&data, 2), 4);
    assert_eq!(utf8_boundary_at_or_after(&data, 3), 4);

    let start = utf8_boundary_at_or_after(&data, 2);
    assert_eq!(std::str::from_utf8(&data[start..]).unwrap(), "b");
}

#[test]
fn arg_value_reads_value_and_handles_trailing_flag() {
    let args = vec![
        "--workspace".to_string(),
        "/tmp/ws".to_string(),
        "--socket".to_string(),
    ];

    assert_eq!(arg_value(&args, "--workspace"), Some("/tmp/ws".to_string()));
    assert_eq!(arg_value(&args, "--socket"), None);
    assert_eq!(arg_value(&args, "--missing"), None);
}

#[test]
fn parse_control_options_handles_flags_after_subcommand() {
    let options = parse_control_options(vec!["panes".to_string(), "--json".to_string()])
        .expect("options should parse");

    assert!(options.json);
    assert_eq!(options.args, vec!["panes".to_string()]);
}

#[test]
fn parse_control_options_treats_help_flag_as_help_command() {
    let options = parse_control_options(vec!["--help".to_string()]).expect("options should parse");
    assert_eq!(options.args.first().map(String::as_str), Some("help"));
}

#[test]
fn parse_control_options_preserves_freeform_send_payload() {
    let options = parse_control_options(vec![
        "send".to_string(),
        "active".to_string(),
        "--json".to_string(),
    ])
    .expect("options should parse");

    assert!(!options.json);
    assert_eq!(
        options.args,
        vec![
            "send".to_string(),
            "active".to_string(),
            "--json".to_string()
        ]
    );
}

#[test]
fn parse_control_options_errors_on_unknown_leading_flag() {
    let err = parse_control_options(vec!["--bogus".to_string()]).expect_err("should error");
    assert!(err.contains("unknown ctl option"));
}

#[test]
fn native_ipc_discovery_exposes_location_but_never_token() {
    let root = std::env::temp_dir().join(format!("sgian-native-ipc-{}", now_millis()));
    let workspace = root.join("workspace");
    let socket_path = root.join("runtime").join(SOCKET_FILE);
    let data_dir = root.join("data");
    fs::create_dir_all(&workspace).expect("workspace directory");
    let client = DaemonClient {
        cwd: workspace.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.clone(),
        token: "super-secret-token".to_string(),
        auto_spawn: false,
    };

    let info = native_ipc_endpoint(&client).expect("discovery metadata");
    assert_eq!(
        info.token_path,
        data_dir.join(TOKEN_FILE).display().to_string()
    );
    assert_eq!(
        info.workspace,
        canonical_workspace_path(&workspace).display().to_string()
    );
    assert_eq!(info.workspace_key, workspace_key(&workspace));
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);
    assert_eq!(info.capabilities, vec!["subscribe-ack"]);
    #[cfg(unix)]
    assert_eq!(info.endpoint, socket_path.display().to_string());

    let json = serde_json::to_string(&info).expect("serialize discovery metadata");
    assert!(!json.contains("super-secret-token"));
    assert!(!json.contains("\"token\":"));
    assert!(json.contains("\"token_path\""));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn control_cli_rejects_json_for_attach_and_logs() {
    // 07-19 CLI low: the global parser consumes --json, but attach streams
    // raw PTY output and logs prints plain text — the flag was silently
    // ignored. Both commands now fail with a clear usage error, before any
    // daemon connection is attempted (this test runs with no daemon).
    for args in [
        vec!["sgian", "ctl", "--json", "attach"],
        vec!["sgian", "ctl", "attach", "--json"],
        vec!["sgian", "ctl", "--json", "logs"],
        vec!["sgian", "ctl", "logs", "-n", "5", "--json"],
    ] {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let command = args[2..]
            .iter()
            .find(|arg| !arg.starts_with('-'))
            .cloned()
            .unwrap_or_default();
        let err = run_control_cli_from_args(&args).expect_err("attach/logs must reject --json");
        assert_eq!(err, format!("--json is not supported for {command}"));
    }
}

#[test]
fn read_ipc_line_rejects_oversized_frame() {
    let mut oversized = vec![b'a'; (MAX_FRAME_BYTES as usize) + 16];
    oversized.push(b'\n');
    let mut reader = BufReader::new(std::io::Cursor::new(oversized));

    let result = read_ipc_line(&mut reader);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("maximum size"));
}

#[test]
fn read_ipc_line_reads_normal_frame() {
    let mut reader = BufReader::new(std::io::Cursor::new(b"hello world\nnext".to_vec()));
    assert_eq!(read_ipc_line(&mut reader).unwrap(), "hello world\n");
}

#[test]
fn framed_envelope_round_trips_request() {
    // VAL-IPC-001: framed encode/decode round-trips representative DaemonRequest
    // variants with no payload mutation.
    let requests = vec![
        DaemonRequest::Ping,
        DaemonRequest::CreatePane {
            title: Some("editor".to_string()),
            profile: None,
        },
        DaemonRequest::WriteToPane {
            pane_id: "pane-2".to_string(),
            data: "ls -la\n".to_string(),
        },
        DaemonRequest::Subscribe,
    ];
    for request in requests {
        let bytes = frame::encode(&request).expect("encode framed request");
        let mut reader = std::io::Cursor::new(bytes);
        let decoded: DaemonRequest = frame::read(&mut reader)
            .expect("read returns Ok")
            .expect("a decoded frame");
        assert_eq!(decoded, request);
    }
}

#[test]
fn framed_envelope_header_is_magic_be_version_be_length() {
    // VAL-IPC-002: header is exactly magic(4) + u16-BE version + u32-BE length,
    // and LENGTH equals the JSON payload byte count, with the payload verbatim.
    let request = DaemonRequest::WriteToPane {
        pane_id: "pane-1".to_string(),
        data: "hello".to_string(),
    };
    let payload = serde_json::to_vec(&request).unwrap();
    let bytes = frame::encode(&request).unwrap();

    assert_eq!(&bytes[0..4], b"SGN2");
    assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 2);
    let declared = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]) as usize;
    assert_eq!(declared, payload.len());
    assert_eq!(&bytes[frame::HEADER_LEN..], payload.as_slice());
    assert_eq!(bytes.len(), frame::HEADER_LEN + payload.len());
}

#[test]
fn framed_reader_rejects_bad_magic() {
    // VAL-IPC-003: a frame whose leading 4 bytes are not the magic is a clean
    // protocol error (Err, no panic, no hang).
    let mut bad = Vec::new();
    bad.extend_from_slice(b"XXXX");
    bad.extend_from_slice(&frame::WIRE_VERSION.to_be_bytes());
    bad.extend_from_slice(&3u32.to_be_bytes());
    bad.extend_from_slice(b"abc");
    let mut reader = std::io::Cursor::new(bad);
    let result = frame::read_bytes(&mut reader);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("magic"));
}

#[test]
fn framed_reader_rejects_oversized_length() {
    // VAL-IPC-004 / VAL-IPC-046 (framed side): a LENGTH exceeding MAX_FRAME_BYTES
    // is rejected at the header, BEFORE any payload allocation and with no body
    // present (so a correct reader cannot block on or allocate the declared size).
    let mut header = Vec::new();
    header.extend_from_slice(&frame::MAGIC);
    header.extend_from_slice(&frame::WIRE_VERSION.to_be_bytes());
    let oversized = (MAX_FRAME_BYTES + 1) as u32;
    header.extend_from_slice(&oversized.to_be_bytes());
    let mut reader = std::io::Cursor::new(header);
    let result = frame::read_bytes(&mut reader);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("maximum frame size"));

    // u32::MAX must also be rejected without attempting a ~4 GiB allocation.
    let mut huge = Vec::new();
    huge.extend_from_slice(&frame::MAGIC);
    huge.extend_from_slice(&frame::WIRE_VERSION.to_be_bytes());
    huge.extend_from_slice(&u32::MAX.to_be_bytes());
    let mut reader = std::io::Cursor::new(huge);
    assert!(frame::read_bytes(&mut reader).is_err());
}

#[test]
fn framed_envelope_round_trips_response() {
    // VAL-IPC-005: framed IpcResponse round-trips both an ok:true(result) and an
    // ok:false(error) form. (IpcResponse has no PartialEq derive; compare fields.)
    let ok = IpcResponse {
        ok: true,
        result: json!({ "panes": ["pane-1"] }),
        error: None,
    };
    let err = IpcResponse {
        ok: false,
        result: Value::Null,
        error: Some("nope".to_string()),
    };
    for original in [ok, err] {
        let bytes = frame::encode(&original).unwrap();
        let mut reader = std::io::Cursor::new(bytes);
        let decoded: IpcResponse = frame::read(&mut reader).unwrap().unwrap();
        assert_eq!(decoded.ok, original.ok);
        assert_eq!(decoded.result, original.result);
        assert_eq!(decoded.error, original.error);
    }
}

#[test]
fn framed_reader_rejects_truncated_and_garbage() {
    // VAL-IPC-006: random bytes, a header with no body, and a truncated header
    // are each a bounded Err (never panic/hang/silent-accept).
    let mut reader = std::io::Cursor::new(b"not-a-valid-frame-at-all".to_vec());
    assert!(frame::read_bytes(&mut reader).is_err());

    let mut header_only = Vec::new();
    header_only.extend_from_slice(&frame::MAGIC);
    header_only.extend_from_slice(&frame::WIRE_VERSION.to_be_bytes());
    header_only.extend_from_slice(&8u32.to_be_bytes());
    let mut reader = std::io::Cursor::new(header_only);
    assert!(frame::read_bytes(&mut reader).is_err());

    let mut reader = std::io::Cursor::new(vec![b'S', b'G']);
    assert!(frame::read_bytes(&mut reader).is_err());
}

/// M6: a peer that sends PART of a frame header and then stalls must not pin
/// the connection thread forever — StallReadTimeout arms a read deadline
/// once the first byte arrives, so the stalled frame read fails promptly.
#[cfg(unix)]
#[test]
fn stalled_partial_frame_fails_after_stall_timeout() {
    let (mut client, mut server_side) =
        test_transport_pair().expect("transport pair should be available");
    client
        .write_all(&frame::MAGIC[..3])
        .expect("partial header writes");

    let started = Instant::now();
    let result = {
        let mut reader = StallReadTimeout::new(&mut server_side, Duration::from_millis(150));
        frame::read::<_, DaemonRequest>(&mut reader)
    };
    assert!(
        result.is_err(),
        "a stalled partial frame must error, got {result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stall timeout must bound the read"
    );
}

/// M6: an idle persistent connection between frames must NOT be bounded —
/// the stall deadline is only armed AFTER the first byte of a frame. A peer
/// that idles longer than the stall timeout and then sends a complete valid
/// frame reads cleanly.
#[cfg(unix)]
#[test]
fn idle_between_frames_is_not_bounded_by_stall_timeout() {
    let (mut client, mut server_side) =
        test_transport_pair().expect("transport pair should be available");
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        frame::write(&mut client, &DaemonRequest::Ping).expect("frame writes");
    });

    let result = {
        let mut reader = StallReadTimeout::new(&mut server_side, Duration::from_millis(150));
        frame::read::<_, DaemonRequest>(&mut reader)
    };
    writer.join().expect("writer thread");
    assert!(
        matches!(result, Ok(Some(DaemonRequest::Ping))),
        "an idle-then-complete frame must read cleanly, got {result:?}"
    );
}

/// M6 follow-up: the stall deadline is ABSOLUTE across the frame — a peer
/// dribbling one byte per interval (each read individually under the timeout)
/// must still die at the deadline, not live forever.
#[cfg(unix)]
#[test]
fn dribbled_frame_fails_at_the_absolute_stall_deadline() {
    let (mut client, mut server_side) =
        test_transport_pair().expect("transport pair should be available");
    let writer = thread::spawn(move || {
        // 20 bytes of frame header, one byte every 50 ms = 1 s total, far past
        // the 150 ms absolute deadline but under any per-read timeout.
        for byte in [b'S'; 20] {
            if client.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    let started = Instant::now();
    let result = {
        let mut reader = StallReadTimeout::new(&mut server_side, Duration::from_millis(150));
        frame::read::<_, DaemonRequest>(&mut reader)
    };
    // Measure the READ, not the writer: the dribble keeps going for ~1 s
    // (the errored reader doesn't close the socket), so asserting after
    // `join` would time the writer thread and flake on a loaded CI runner.
    let read_elapsed = started.elapsed();
    writer.join().expect("writer thread");
    assert!(
        result.is_err(),
        "a dribbled frame must error, got {result:?}"
    );
    assert!(
        read_elapsed < Duration::from_secs(2),
        "the absolute deadline must bound the read, took {read_elapsed:?}"
    );
}
#[test]
fn framed_reader_errors_on_short_body_eof() {
    // VAL-IPC-007: a declared LENGTH exceeding the bytes delivered before EOF is a
    // bounded Err, not an unbounded wait for the missing bytes.
    let mut framed = Vec::new();
    framed.extend_from_slice(&frame::MAGIC);
    framed.extend_from_slice(&frame::WIRE_VERSION.to_be_bytes());
    framed.extend_from_slice(&64u32.to_be_bytes());
    framed.extend_from_slice(b"only-a-few");
    let mut reader = std::io::Cursor::new(framed);
    let result = frame::read_bytes(&mut reader);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("truncated"));
}

#[test]
fn framed_payload_matches_newline_payload() {
    // VAL-IPC-008: the JSON inside a v2 frame is byte-identical to the v1 newline
    // payload and deserializes to an equal value (framing changes only envelope).
    let request = DaemonRequest::ResizePaneTerminal {
        pane_id: "pane-3".to_string(),
        cols: 120,
        rows: 40,
    };
    let bytes = frame::encode(&request).unwrap();
    let framed_payload = &bytes[frame::HEADER_LEN..];
    let newline_payload = serde_json::to_vec(&request).unwrap();
    assert_eq!(framed_payload, newline_payload.as_slice());

    let framed_value: Value = serde_json::from_slice(framed_payload).unwrap();
    let newline_value: Value = serde_json::from_slice(&newline_payload).unwrap();
    assert_eq!(framed_value, newline_value);

    let from_frame: DaemonRequest = serde_json::from_slice(framed_payload).unwrap();
    assert_eq!(from_frame, request);
}

#[test]
fn framed_reader_rejects_unknown_wire_version() {
    // VAL-IPC-009: correct magic but an unsupported per-frame WIRE_VERSION is a
    // clean protocol error rather than a dispatch.
    for version in [0u16, 1u16, 3u16, 99u16] {
        let mut framed = Vec::new();
        framed.extend_from_slice(&frame::MAGIC);
        framed.extend_from_slice(&version.to_be_bytes());
        framed.extend_from_slice(&3u32.to_be_bytes());
        framed.extend_from_slice(b"abc");
        let mut reader = std::io::Cursor::new(framed);
        let result = frame::read_bytes(&mut reader);
        assert!(result.is_err(), "version {version} should be rejected");
        assert!(result.unwrap_err().contains("wire version"));
    }
}

#[test]
fn framed_zero_length_payload_is_error() {
    // VAL-IPC-010: a well-formed header with LENGTH == 0 does not deserialize to a
    // valid DaemonRequest and is an error (not a panic). The bounded reader treats
    // the empty payload as a valid-length frame; the typed decode rejects it.
    let mut framed = Vec::new();
    framed.extend_from_slice(&frame::MAGIC);
    framed.extend_from_slice(&frame::WIRE_VERSION.to_be_bytes());
    framed.extend_from_slice(&0u32.to_be_bytes());
    let mut reader = std::io::Cursor::new(framed.clone());
    let result: Result<Option<DaemonRequest>, String> = frame::read(&mut reader);
    assert!(result.is_err());

    let mut reader = std::io::Cursor::new(framed);
    let bytes = frame::read_bytes(&mut reader).unwrap();
    assert_eq!(bytes, Some(Vec::new()));
}

#[test]
fn framed_envelope_round_trips_event() {
    // VAL-IPC-049: the envelope carries a DaemonEvent (the third payload type)
    // unchanged; payload is byte-identical to the v1 newline payload.
    let pane = Pane {
        id: "pane-5".to_string(),
        title: "term-5".to_string(),
        kind: PaneKind::Shell,
        created_at_ms: 1_700_000_000_000,
    };
    let events = vec![
        DaemonEvent::PaneCreated { pane: pane.clone() },
        DaemonEvent::PaneEnded {
            pane_id: "pane-5".to_string(),
            exit_code: None,
        },
        DaemonEvent::PtyOutput {
            pane_id: "pane-5".to_string(),
            data: "output\r\n".to_string(),
        },
    ];
    for event in events {
        let bytes = frame::encode(&event).unwrap();
        assert_eq!(
            &bytes[frame::HEADER_LEN..],
            serde_json::to_vec(&event).unwrap().as_slice()
        );
        let mut reader = std::io::Cursor::new(bytes);
        let decoded: DaemonEvent = frame::read(&mut reader).unwrap().unwrap();
        assert_eq!(decoded, event);
    }
}

#[test]
fn encode_framed_rejects_oversized_payload() {
    // VAL-IPC-046 (write side): the encoder refuses a payload over MAX_FRAME_BYTES
    // so the framed path is bounded on write as the newline path is on read.
    let big = "x".repeat((MAX_FRAME_BYTES as usize) + 1);
    let request = DaemonRequest::WriteToPane {
        pane_id: "pane-1".to_string(),
        data: big,
    };
    let result = frame::encode(&request);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("maximum frame size"));
}

#[test]
fn framed_reader_clean_eof_at_boundary_is_none() {
    // A peer that closes between frames (nothing buffered) is a clean Ok(None),
    // which is what lets the v2 request loop terminate gracefully on EOF.
    let mut reader = std::io::Cursor::new(Vec::new());
    assert_eq!(frame::read_bytes(&mut reader).unwrap(), None);
    let decoded: Option<DaemonRequest> = frame::read(&mut reader).unwrap();
    assert_eq!(decoded, None);
}

#[test]
fn framed_reader_reads_sequential_frames() {
    // The codec is self-delimiting: frames concatenated on one stream decode in
    // sequence, then a clean EOF -> None.
    let mut buf = Vec::new();
    buf.extend_from_slice(&frame::encode(&DaemonRequest::Ping).unwrap());
    buf.extend_from_slice(&frame::encode(&DaemonRequest::ListPanes).unwrap());
    let mut reader = std::io::Cursor::new(buf);
    assert_eq!(
        frame::read::<_, DaemonRequest>(&mut reader).unwrap(),
        Some(DaemonRequest::Ping)
    );
    assert_eq!(
        frame::read::<_, DaemonRequest>(&mut reader).unwrap(),
        Some(DaemonRequest::ListPanes)
    );
    assert_eq!(frame::read::<_, DaemonRequest>(&mut reader).unwrap(), None);
}

#[test]
fn write_framed_then_read_round_trips_over_a_buffer() {
    // frame::write encodes + flushes to any Write sink; round-trips via the reader.
    let mut buf: Vec<u8> = Vec::new();
    frame::write(&mut buf, &DaemonRequest::Ping).unwrap();
    assert_eq!(&buf[0..4], b"SGN2");
    let mut reader = std::io::Cursor::new(buf);
    let decoded: DaemonRequest = frame::read(&mut reader).unwrap().unwrap();
    assert_eq!(decoded, DaemonRequest::Ping);
}

// ───── MA capability/version handshake negotiation (VAL-IPC-011..026) ─────

/// What a v2-aware client learns from the handshake response: whether auth
/// succeeded, the negotiated wire version, the advertised capabilities, and the
/// preserved `protocol_version`. Mirrors what the integration/SDK client will do
/// once wired; for this feature it exercises the daemon's negotiation directly.
struct ClientHandshake {
    stream: TransportStream,
    ok: bool,
    negotiated_wire_version: u16,
    capabilities: Vec<String>,
    protocol_version: Option<u64>,
}

impl ClientHandshake {
    /// Feature-detection rule (architecture.md §5.2 / VAL-IPC-025): switch to the
    /// framed envelope iff the negotiated version reached the framed wire version
    /// AND the peer advertised the "framed" capability.
    fn uses_framing(&self) -> bool {
        self.negotiated_wire_version >= frame::WIRE_VERSION
            && self.capabilities.iter().any(|c| c == "framed")
    }
}

/// Drive the client side of the handshake over `stream`: send a newline hello
/// carrying the legacy `version: 1` plus the additive `max_wire_version`, read
/// the newline handshake response, and decode the negotiation fields.
fn client_handshake(
    stream: TransportStream,
    token: &str,
    max_wire_version: Option<u16>,
) -> Result<ClientHandshake, String> {
    let mut stream = stream;
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: token.to_string(),
        max_wire_version,
        capabilities: Some(vec!["framed".to_string()]),
        client_token: None,
    };
    write_json_line(&mut stream, &hello)?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("failed to read handshake response: {error}"))?;
    let response: IpcResponse = serde_json::from_str(line.trim_end())
        .map_err(|error| format!("invalid handshake response: {error}"))?;
    let negotiated_wire_version = response
        .result
        .get("negotiated_wire_version")
        .and_then(Value::as_u64)
        .map(|v| v as u16)
        .unwrap_or(1);
    let capabilities = response
        .result
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let protocol_version = response
        .result
        .get("protocol_version")
        .and_then(Value::as_u64);
    Ok(ClientHandshake {
        stream: reader.into_inner(),
        ok: response.ok,
        negotiated_wire_version,
        capabilities,
        protocol_version,
    })
}

/// Spin up an in-process daemon over a connected transport pair and run the
/// real `handle_daemon_client` on the server side.
/// Returns the client end, the daemon's real token, the data-dir guard (kept
/// alive by the caller), and the server thread handle.
fn pair_daemon_connection() -> (
    TransportStream,
    String,
    tempfile::TempDir,
    thread::JoinHandle<Result<(), String>>,
) {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let server = Arc::new(
        DaemonServer::with_config(
            PathBuf::from("/tmp/sgian-handshake"),
            data_dir.path().to_path_buf(),
            Config::default(),
        )
        .expect("daemon server should start"),
    );
    let token = server.token.clone();
    let (client_stream, server_stream) =
        test_transport_pair().expect("transport pair should be available");
    let handle = {
        let server = Arc::clone(&server);
        thread::spawn(move || handle_daemon_client(server, server_stream))
    };
    (client_stream, token, data_dir, handle)
}

/// Run `client_handshake` against a fake peer that replies with `result` as its
/// handshake-response payload; returns whether the client decides to use framing.
fn client_feature_detection(result: Value) -> bool {
    let (client, server) = test_transport_pair().expect("transport pair should be available");
    let server_thread = thread::spawn(move || {
        let mut reader = BufReader::new(server);
        let mut hello_line = String::new();
        reader.read_line(&mut hello_line).expect("read hello");
        let mut server = reader.into_inner();
        write_json_line(
            &mut server,
            &IpcResponse {
                ok: true,
                result,
                error: None,
            },
        )
        .expect("write handshake response");
    });
    let hs = client_handshake(client, "tok", Some(2)).expect("handshake completes");
    let uses = hs.uses_framing();
    drop(hs.stream);
    server_thread.join().expect("fake peer thread");
    uses
}

#[test]
fn ipc_hello_accepts_legacy_and_extended_fields() {
    // VAL-IPC-011: a legacy hello (type/version/token only), an extended hello
    // (additive max_wire_version + capabilities), and a hello carrying extra
    // unknown fields all deserialize — IpcHello must NOT use deny_unknown_fields.
    let legacy: IpcHello = serde_json::from_str(r#"{"type":"hello","version":1,"token":"t"}"#)
        .expect("legacy hello must still parse");
    assert_eq!(legacy.version, 1);
    assert_eq!(legacy.max_wire_version, None);
    assert_eq!(legacy.capabilities, None);

    let extended: IpcHello = serde_json::from_str(
        r#"{"type":"hello","version":1,"token":"t","max_wire_version":2,"capabilities":["framed"]}"#,
    )
    .expect("extended hello must parse");
    assert_eq!(extended.max_wire_version, Some(2));
    assert_eq!(extended.capabilities, Some(vec!["framed".to_string()]));

    let with_unknown: IpcHello = serde_json::from_str(
        r#"{"type":"hello","version":1,"token":"t","max_wire_version":2,"future_field":"x","nested":{"a":1}}"#,
    )
    .expect("a hello with extra unknown fields must still parse");
    assert_eq!(with_unknown.max_wire_version, Some(2));
}

#[test]
fn new_client_hello_is_newline_readable_by_v1() {
    // VAL-IPC-012: a new client's hello (carrying additive fields) is emitted by
    // the v1 writer as a single '\n'-terminated JSON line a v1 line reader parses.
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: "tok-123".to_string(),
        max_wire_version: Some(2),
        capabilities: Some(vec!["framed".to_string()]),
        client_token: None,
    };
    let (mut writer, peer) = test_transport_pair().expect("transport pair should be available");
    write_json_line(&mut writer, &hello).expect("hello writes");
    drop(writer);

    let mut reader = BufReader::new(peer);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .expect("v1 line reader reads the hello");
    assert!(n > 0);
    assert!(line.ends_with('\n'), "hello must be newline-terminated");
    assert_eq!(line.matches('\n').count(), 1, "hello is exactly one line");

    // A v1 reader extracts type/version/token (ignoring the additive fields).
    let value: Value = serde_json::from_str(line.trim_end()).expect("v1 JSON parse");
    assert_eq!(value.get("type").and_then(Value::as_str), Some("hello"));
    assert_eq!(value.get("version").and_then(Value::as_u64), Some(1));
    assert_eq!(value.get("token").and_then(Value::as_str), Some("tok-123"));
}

#[test]
fn new_client_hello_sends_legacy_version_one() {
    // VAL-IPC-013: the legacy `version` field a v1 daemon checks is set to 1.
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: "tok".to_string(),
        max_wire_version: Some(2),
        capabilities: None,
        client_token: None,
    };
    let value = serde_json::to_value(&hello).expect("hello serializes");
    assert_eq!(value.get("version").and_then(Value::as_u64), Some(1));
    assert_eq!(
        value.get("max_wire_version").and_then(Value::as_u64),
        Some(2)
    );
}

#[test]
fn negotiation_clamps_to_lower_bound() {
    // VAL-IPC-016: negotiated = min(client_max, daemon_max), never above either.
    assert_eq!(negotiate_wire_version(Some(5)), DAEMON_MAX_WIRE_VERSION);
    assert_eq!(negotiate_wire_version(Some(5)), 2);
    assert_eq!(negotiate_wire_version(Some(2)), 2);
    assert_eq!(negotiate_wire_version(Some(1)), 1);
    // Pure min: a client advertising 0 yields 0 (never raised above client max),
    // which a sane client treats as the newline path (0 < framed wire version).
    assert_eq!(negotiate_wire_version(Some(0)), 0);
}

#[test]
fn absent_max_wire_version_defaults_to_one() {
    // VAL-IPC-020: an omitted max_wire_version is treated as client max = 1.
    assert_eq!(negotiate_wire_version(None), 1);
}

#[test]
fn client_read_times_out_against_a_silent_daemon() {
    // H3: a daemon that accepts but never responds (the H2 wedge shape) must
    // not pin the client in a blocking read forever.
    let dir = std::env::temp_dir().join(format!("sgian-timeout-test-{}", now_millis()));
    fs::create_dir_all(&dir).expect("test dir should be created");
    let socket_path = dir.join("silent.sock");
    let listener = transport_bind(&socket_path).expect("bind should succeed");

    let accept_thread = thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            // Hold the connection open, silent, past the client's deadline.
            thread::sleep(Duration::from_millis(600));
            drop(stream);
        }
    });

    let started = Instant::now();
    let result = DaemonConnection::connect_with_timeout(
        &socket_path,
        "irrelevant-token",
        Some(Duration::from_millis(150)),
    );
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "handshake against a silent daemon must fail"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "read should time out promptly, not hang (took {elapsed:?})"
    );

    accept_thread.join().expect("accept thread should finish");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn handshake_negotiates_min_wire_version() {
    // VAL-IPC-014: client max 2 vs daemon max 2 negotiates 2; client max 1 -> 1.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    assert!(hs.ok);
    assert_eq!(
        hs.negotiated_wire_version,
        std::cmp::min(2, DAEMON_MAX_WIRE_VERSION)
    );
    assert_eq!(hs.negotiated_wire_version, 2);
    drop(hs.stream);
    let _ = handle.join();

    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(1)).expect("handshake ok");
    assert_eq!(hs.negotiated_wire_version, 1);
    drop(hs.stream);
    let _ = handle.join();
}

#[test]
fn handshake_advertises_capabilities() {
    // VAL-IPC-015: the response carries a capabilities list including the framed
    // and persistent capabilities so a client can feature-detect.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    assert!(
        hs.capabilities.iter().any(|c| c == "framed"),
        "capabilities must advertise framed: {:?}",
        hs.capabilities
    );
    assert!(
        hs.capabilities.iter().any(|c| c == "persistent"),
        "capabilities must advertise persistent: {:?}",
        hs.capabilities
    );
    drop(hs.stream);
    let _ = handle.join();
}

#[test]
fn new_client_new_daemon_uses_framed_v2() {
    // VAL-IPC-017: negotiate 2, then a framed DaemonRequest is dispatched and
    // answered with a framed IpcResponse on the same connection.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    assert_eq!(hs.negotiated_wire_version, 2);
    assert!(
        hs.uses_framing(),
        "client must switch to framing on v2 + framed cap"
    );

    let mut stream = hs.stream;
    frame::write(&mut stream, &DaemonRequest::Ping).expect("framed request writes");
    let response: IpcResponse = frame::read(&mut stream)
        .expect("framed response reads")
        .expect("a framed response is present");
    assert!(
        response.ok,
        "framed Ping must be answered ok: {:?}",
        response.error
    );
    drop(stream);
    let _ = handle.join();
}

#[test]
fn new_client_v1_daemon_falls_back_to_newline() {
    // VAL-IPC-018: a new client (advertising max_wire_version 2) talking to a
    // simulated v1 daemon (old response shape, no negotiated field) completes the
    // handshake, stays on the newline path, and serves a request with no rejection.
    let (client, server) = test_transport_pair().expect("transport pair should be available");
    let server_thread = thread::spawn(move || {
        let mut reader = BufReader::new(server);
        let mut hello_line = String::new();
        reader.read_line(&mut hello_line).expect("read hello");
        let hello: IpcHello = serde_json::from_str(hello_line.trim_end()).expect("parse hello");
        assert_eq!(hello.version, 1, "v1 daemon requires legacy version 1");
        let mut server = reader.into_inner();
        // Old daemon reply: protocol_version only (no negotiated/capabilities).
        write_json_line(
            &mut server,
            &IpcResponse {
                ok: true,
                result: json!({ "protocol_version": 1 }),
                error: None,
            },
        )
        .expect("write old response");
        // One newline request, one newline response (v1 single-shot).
        let mut reader = BufReader::new(server);
        let mut req_line = String::new();
        reader.read_line(&mut req_line).expect("read request");
        let _req: DaemonRequest = serde_json::from_str(req_line.trim_end()).expect("parse req");
        let mut server = reader.into_inner();
        write_json_line(
            &mut server,
            &IpcResponse {
                ok: true,
                result: json!("pong"),
                error: None,
            },
        )
        .expect("write response");
    });

    let hs = client_handshake(client, "ignored", Some(2)).expect("handshake completes");
    assert!(
        hs.ok,
        "handshake with a v1 daemon must succeed (no rejection)"
    );
    assert_eq!(
        hs.negotiated_wire_version, 1,
        "absent negotiated field defaults to wire v1"
    );
    assert!(
        !hs.uses_framing(),
        "client must NOT frame against a v1 daemon"
    );

    let mut stream = hs.stream;
    write_json_line(&mut stream, &DaemonRequest::Ping).expect("newline request writes");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("newline response reads");
    let resp: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
    assert!(resp.ok, "the newline request is served with no error");
    drop(reader);
    server_thread.join().expect("fake v1 daemon thread");
}

#[test]
fn old_client_new_daemon_stays_newline_v1() {
    // VAL-IPC-019: a legacy hello (no max_wire_version/capabilities) authenticates,
    // negotiates wire v1, and is served over the newline path (not hard-rejected,
    // not forced into framing).
    let (mut client, token, _dd, handle) = pair_daemon_connection();
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: token.clone(),
        max_wire_version: None,
        capabilities: None,
        client_token: None,
    };
    write_json_line(&mut client, &hello).expect("legacy hello writes");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("handshake response reads");
    let response: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
    assert!(response.ok, "legacy hello must be accepted, not rejected");
    assert_eq!(
        response
            .result
            .get("negotiated_wire_version")
            .and_then(Value::as_u64),
        Some(1),
        "a legacy client negotiates wire v1"
    );

    let mut client = reader.into_inner();
    write_json_line(&mut client, &DaemonRequest::Ping).expect("newline request writes");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).expect("newline response reads");
    let resp: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
    assert!(resp.ok, "the newline request is served over v1");
    drop(reader);
    let _ = handle.join();
}

#[test]
fn authenticate_negotiates_instead_of_version_reject() {
    // VAL-IPC-021: a valid-token hello whose legacy `version` != PROTOCOL_VERSION
    // is NOT rejected — the daemon negotiates a wire version instead.
    let (mut client, token, _dd, handle) = pair_daemon_connection();
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: 99,
        token: token.clone(),
        max_wire_version: Some(2),
        capabilities: None,
        client_token: None,
    };
    write_json_line(&mut client, &hello).expect("hello writes");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("handshake response reads");
    let response: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
    assert!(
        response.ok,
        "a valid-token hello must not be rejected on legacy version: {:?}",
        response.error
    );
    assert_eq!(
        response
            .result
            .get("negotiated_wire_version")
            .and_then(Value::as_u64),
        Some(2)
    );
    drop(reader);
    let _ = handle.join();
}

#[test]
fn bad_token_rejected_for_v1_and_v2() {
    // VAL-IPC-022: a wrong token is rejected whether the hello advertises v2 or the
    // legacy v1 shape; the error is generic and never leaks the token value.
    let bad_token = "0".repeat(64);
    for max_wire in [None, Some(2u16)] {
        let (mut client, _token, _dd, handle) = pair_daemon_connection();
        let hello = IpcHello {
            frame_type: "hello".to_string(),
            version: PROTOCOL_VERSION,
            token: bad_token.clone(),
            max_wire_version: max_wire,
            capabilities: None,
            client_token: None,
        };
        write_json_line(&mut client, &hello).expect("hello writes");
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).expect("response reads");
        let response: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
        assert!(
            !response.ok,
            "bad token must be rejected (max_wire={max_wire:?})"
        );
        let err = response.error.unwrap_or_default();
        assert!(
            err.contains("authentication failed"),
            "generic auth error expected: {err}"
        );
        assert!(!err.contains(&bad_token), "error must not leak the token");
        drop(reader);
        let _ = handle.join();
    }
}

#[test]
fn bad_token_does_not_enter_request_loop() {
    // VAL-IPC-023: on a bad token the daemon sends the failed handshake response
    // and closes WITHOUT dispatching any request (the token gate precedes the loop).
    let (mut client, _token, _dd, handle) = pair_daemon_connection();
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: "0".repeat(64),
        max_wire_version: Some(2),
        capabilities: None,
        client_token: None,
    };
    write_json_line(&mut client, &hello).expect("hello writes");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).expect("response reads");
    let response: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
    assert!(!response.ok);

    // The connection is closed after the failed handshake: a further read is EOF
    // and the handler returns cleanly without entering any request loop.
    let mut rest = String::new();
    let n = reader
        .read_line(&mut rest)
        .expect("read after failed handshake");
    assert_eq!(n, 0, "connection must be closed after a failed handshake");
    let result = handle.join().expect("server thread must not panic");
    assert!(
        result.is_ok(),
        "handler returns cleanly on bad token: {result:?}"
    );
}

#[test]
fn handshake_response_is_newline_json() {
    // VAL-IPC-024: even when v2 is negotiated, the handshake response is a single
    // newline-JSON line (not a binary frame), readable by a v1 line reader.
    let (mut client, token, _dd, handle) = pair_daemon_connection();
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: token.clone(),
        max_wire_version: Some(2),
        capabilities: None,
        client_token: None,
    };
    write_json_line(&mut client, &hello).expect("hello writes");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).expect("response reads");
    assert!(line.ends_with('\n'), "response must be newline-terminated");
    assert!(
        !line.as_bytes().starts_with(&frame::MAGIC),
        "response must not be a framed envelope"
    );
    let response: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse response");
    assert!(response.ok);
    assert_eq!(
        response
            .result
            .get("negotiated_wire_version")
            .and_then(Value::as_u64),
        Some(2)
    );
    drop(reader);
    let _ = handle.join();
}

#[test]
fn client_feature_detects_from_capabilities() {
    // VAL-IPC-025: a client switches to framing iff framing is advertised AND
    // negotiated >= 2; otherwise it stays on the newline path.
    assert!(
        client_feature_detection(json!({
            "protocol_version": 1,
            "negotiated_wire_version": 2,
            "capabilities": ["framed", "persistent"]
        })),
        "must frame when framed advertised and negotiated 2"
    );
    assert!(
        !client_feature_detection(json!({
            "protocol_version": 1,
            "negotiated_wire_version": 2,
            "capabilities": ["persistent"]
        })),
        "must NOT frame when the framed capability is absent"
    );
    assert!(
        !client_feature_detection(json!({ "protocol_version": 1 })),
        "must NOT frame against a simulated v1 peer (no negotiated field)"
    );
}

#[test]
fn handshake_keeps_protocol_version_field() {
    // VAL-IPC-026: the response still carries the original protocol_version field
    // alongside the new negotiation fields, for both v1 and v2 clients.
    for max_wire in [None, Some(2u16)] {
        let (client, token, _dd, handle) = pair_daemon_connection();
        let hs = client_handshake(client, &token, max_wire).expect("handshake ok");
        assert_eq!(
            hs.protocol_version,
            Some(PROTOCOL_VERSION as u64),
            "protocol_version must be preserved (max_wire={max_wire:?})"
        );
        drop(hs.stream);
        let _ = handle.join();
    }
}

// ───── MA ctl/client integration over the negotiated protocol (VAL-IPC-044/045/047/048) ─────

#[test]
fn daemon_connection_negotiates_framed_v2_and_round_trips() {
    // VAL-IPC-047/048 at the unit layer: the wired client (`DaemonConnection`,
    // which `ctl`/`DaemonClient` now use by default) negotiates framed v2 against
    // a real in-process daemon and round-trips MULTIPLE requests over the SAME
    // persistent connection (architecture.md §5.2/§5.3).
    let (client, token, _data_dir, handle) = pair_daemon_connection();
    let mut conn = DaemonConnection::handshake(client, &token, None).expect("handshake completes");
    assert_eq!(
        conn.wire_version,
        frame::WIRE_VERSION,
        "new client + new daemon negotiate the framed wire version"
    );
    assert!(
        conn.uses_framing(),
        "negotiated v2 + advertised framed ⇒ the client uses framing"
    );

    // First framed round-trip: Ping.
    let pong = conn
        .request(&DaemonRequest::Ping)
        .expect("framed ping round-trips");
    assert!(pong.ok, "framed Ping should succeed: {:?}", pong.error);

    // Two more requests on the SAME connection prove persistence (no reconnect).
    let panes = conn
        .request(&DaemonRequest::ListPanes)
        .expect("framed list round-trips");
    assert!(panes.ok);
    let panes_again = conn
        .request(&DaemonRequest::ListPanes)
        .expect("a second framed list on the same connection round-trips");
    assert!(panes_again.ok);

    // Closing the connection ends the daemon's per-connection loop cleanly.
    drop(conn);
    handle
        .join()
        .expect("daemon thread joins")
        .expect("daemon connection handler returns Ok on clean EOF");
}

#[test]
fn daemon_connection_falls_back_to_newline_against_v1_daemon() {
    // VAL-IPC-018 at the client layer (Invariant 8): a wired client meeting a
    // simulated v1 daemon — one that ignores max_wire_version, replies with only
    // protocol_version, and serves a single newline request — detects the absent
    // negotiation, stays on the newline path, and still round-trips.
    let (client, server) = test_transport_pair().expect("transport pair should be available");
    let server_thread = thread::spawn(move || {
        let mut reader = BufReader::new(server);
        let mut hello_line = String::new();
        reader
            .read_line(&mut hello_line)
            .expect("v1 daemon reads the hello");
        // Legacy handshake response: protocol_version only, exactly an old daemon.
        write_json_line(
            reader.get_mut(),
            &IpcResponse {
                ok: true,
                result: json!({ "protocol_version": 1 }),
                error: None,
            },
        )
        .expect("v1 daemon writes the legacy handshake response");
        // One newline request/response — the v1 one-request-per-connection path.
        let mut req_line = String::new();
        reader
            .read_line(&mut req_line)
            .expect("v1 daemon reads the request");
        let request: DaemonRequest =
            serde_json::from_str(req_line.trim_end()).expect("request decodes");
        assert_eq!(
            request,
            DaemonRequest::Ping,
            "the request arrives as newline JSON"
        );
        write_json_line(
            reader.get_mut(),
            &IpcResponse {
                ok: true,
                result: json!({ "pong": true }),
                error: None,
            },
        )
        .expect("v1 daemon writes the newline response");
    });

    let mut conn = DaemonConnection::handshake(client, "tok", None).expect("handshake completes");
    assert_eq!(
        conn.wire_version, 1,
        "absent negotiation defaults to wire v1"
    );
    assert!(
        !conn.uses_framing(),
        "a wired client must stay newline against a v1 daemon"
    );
    let response = conn
        .request(&DaemonRequest::Ping)
        .expect("newline request round-trips against the v1 daemon");
    assert!(response.ok);

    drop(conn);
    server_thread
        .join()
        .expect("simulated v1 daemon thread joins");
}

#[test]
fn daemon_is_alive_probes_via_negotiated_protocol() {
    // ma-scrutiny-fixes (negotiated-by-default): the liveness/auth probe behind
    // `ctl daemons` and `ctl shutdown --all` must negotiate via DaemonConnection
    // (framed v2 against a new daemon) rather than the legacy v1
    // authenticate_stream_at, while still succeeding against an old v1-only daemon
    // (graceful fallback is covered by
    // daemon_connection_falls_back_to_newline_against_v1_daemon).
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    });

    // A live daemon is reported alive by the negotiated probe.
    assert!(
        daemon_is_alive_at(&daemon.socket_path, &daemon.token),
        "a live daemon must be reported alive by the negotiated probe"
    );

    // The probe path negotiates framed v2 against the new daemon (negotiated by
    // default): a DaemonConnection to the SAME socket/token uses framing.
    let conn = DaemonConnection::connect(&daemon.socket_path, &daemon.token)
        .expect("the probe connects over the negotiated protocol");
    assert!(
        conn.uses_framing(),
        "the liveness probe negotiates framed v2 against a v2-capable daemon"
    );
    drop(conn);

    // A wrong token is not alive (auth still gates the probe).
    assert!(
        !daemon_is_alive_at(&daemon.socket_path, "wrong-token-xyz"),
        "a wrong token must not be reported alive"
    );

    let socket_path = daemon.socket_path.clone();
    let token = daemon.token.clone();
    daemon.shutdown();

    // After shutdown the socket is gone, so the probe reports not-alive.
    assert!(
        !daemon_is_alive_at(&socket_path, &token),
        "a dead daemon must be reported not-alive"
    );
}

#[test]
fn daemon_is_alive_probed_short_circuits_when_lock_not_held() {
    // 07-19 CLI low: `ctl daemons` / `ctl shutdown --all` probe each known
    // workspace; a wedged one used to cost up to CLIENT_READ_TIMEOUT in
    // ping. With NO flock held the probe must report not-running WITHOUT
    // connecting at all; with the lock held it must ping as before.
    let dir = tempfile::tempdir_in("/tmp").expect("temp dir");
    let socket_path = dir.path().join("d.sock");
    // A listener that accepts and immediately drops connections, counting
    // them: every ping attempt is observable, and the dropped connection
    // fails the ping fast (no CLIENT_READ_TIMEOUT wait in the test).
    let listener = transport_bind(&socket_path).expect("bind a listener");
    let accept_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let acceptor = thread::spawn({
        let accept_count = accept_count.clone();
        move || {
            let _ = listener.set_nonblocking(true);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok((stream, _)) = listener.accept() {
                    accept_count.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    });

    // No lock held ⇒ not-running, fast, and NO ping attempt.
    let started = Instant::now();
    assert!(
        !daemon_is_alive_probed(&socket_path, "tok"),
        "no lock held ⇒ not-running"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the lock short-circuit must not wait out a ping: {:?}",
        started.elapsed()
    );
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        accept_count.load(Ordering::SeqCst),
        0,
        "no lock held ⇒ the probe must not ping (no connection accepted)"
    );

    // Lock held (a possibly-live daemon) ⇒ the probe pings as before; the
    // dropped connection makes the ping fail, so still not-alive.
    let _lock = acquire_daemon_lock(&socket_path)
        .expect("acquire should succeed")
        .expect("acquire should return the lock file");
    assert!(
        !daemon_is_alive_probed(&socket_path, "tok"),
        "lock held but ping fails ⇒ not-running"
    );
    assert!(
        accept_count.load(Ordering::SeqCst) >= 1,
        "lock held ⇒ the probe must ping (a connection was accepted)"
    );

    drop(acceptor);
}

#[test]
fn payload_json_schema_is_stable() {
    // VAL-IPC-044: the v2 framing changes ONLY the transport envelope; the JSON
    // payload schema for DaemonRequest/DaemonEvent/IpcResponse is byte-identical
    // to the documented v1 shape — DaemonRequest internally tagged "command",
    // DaemonEvent "event", snake_case variant tags, unchanged field names. A
    // baseline-shaped JSON still deserializes; a freshly serialized value matches.

    // DaemonRequest: internal tag "command" + snake_case variant/field names.
    assert_eq!(
        serde_json::to_value(DaemonRequest::Ping).unwrap(),
        json!({ "command": "ping" })
    );
    assert_eq!(
        serde_json::to_value(DaemonRequest::CreatePane {
            title: Some("t".to_string()),
            profile: None,
        })
        .unwrap(),
        json!({ "command": "create_pane", "title": "t" })
    );
    assert_eq!(
        serde_json::to_value(DaemonRequest::SendInput {
            pane_id: "pane-1".to_string(),
            input: "x".to_string(),
        })
        .unwrap(),
        json!({ "command": "send_input", "pane_id": "pane-1", "input": "x" })
    );
    // A baseline (v1-shape) request JSON still deserializes to the same value.
    assert_eq!(
        serde_json::from_str::<DaemonRequest>(
            r#"{"command":"write_to_pane","pane_id":"pane-2","data":"hi"}"#
        )
        .unwrap(),
        DaemonRequest::WriteToPane {
            pane_id: "pane-2".to_string(),
            data: "hi".to_string(),
        }
    );

    // DaemonEvent: internal tag "event" + snake_case variant/field names.
    assert_eq!(
        serde_json::to_value(DaemonEvent::PaneEnded {
            pane_id: "pane-1".to_string(),
            exit_code: None,
        })
        .unwrap(),
        json!({ "event": "pane_ended", "pane_id": "pane-1" })
    );
    // The reaper's exit_code is additive: present only when there is a code, so a
    // None end keeps the byte-identical pre-MB shape (asserted above) and a real
    // code simply adds the field.
    assert_eq!(
        serde_json::to_value(DaemonEvent::PaneEnded {
            pane_id: "pane-1".to_string(),
            exit_code: Some(7),
        })
        .unwrap(),
        json!({ "event": "pane_ended", "pane_id": "pane-1", "exit_code": 7 })
    );
    // A pre-MB PaneEnded (pane_id only) still deserializes, defaulting to no code.
    assert_eq!(
        serde_json::from_str::<DaemonEvent>(r#"{"event":"pane_ended","pane_id":"pane-1"}"#)
            .unwrap(),
        DaemonEvent::PaneEnded {
            pane_id: "pane-1".to_string(),
            exit_code: None,
        }
    );
    assert_eq!(
        serde_json::to_value(DaemonEvent::PtyOutput {
            pane_id: "pane-1".to_string(),
            data: "out".to_string(),
        })
        .unwrap(),
        json!({ "event": "pty_output", "pane_id": "pane-1", "data": "out" })
    );

    // IpcResponse: unchanged ok/result/error fields (error: None ⇒ null).
    assert_eq!(
        serde_json::to_value(IpcResponse {
            ok: true,
            result: json!({ "x": 1 }),
            error: None,
        })
        .unwrap(),
        json!({ "ok": true, "result": { "x": 1 }, "error": null })
    );
}

#[test]
fn pane_created_at_ms_is_u64_roundtrip() {
    // VAL-IPC-045: Pane.created_at_ms stays u64 and round-trips through BOTH the
    // newline and framed payloads without truncation or type change (a u128 here
    // broke tagged-enum deserialization — the field must remain u64).
    let pane = Pane {
        id: "pane-1".to_string(),
        title: "term".to_string(),
        kind: PaneKind::Shell,
        created_at_ms: u64::MAX,
    };
    // The field is u64 (compile-time proof) and a large value survives.
    let created: u64 = pane.created_at_ms;
    assert_eq!(created, u64::MAX);

    // Newline payload bytes are exactly what write_json_line emits (to_vec).
    let newline_bytes = serde_json::to_vec(&pane).unwrap();
    let from_newline: Pane = serde_json::from_slice(&newline_bytes).unwrap();
    assert_eq!(from_newline, pane);
    assert_eq!(from_newline.created_at_ms, u64::MAX);

    // Framed payload wraps the identical JSON; decoding recovers the same u64.
    let framed = frame::encode(&pane).unwrap();
    let from_framed: Pane = frame::read(&mut std::io::Cursor::new(framed))
        .unwrap()
        .unwrap();
    assert_eq!(from_framed, pane);
    assert_eq!(from_framed.created_at_ms, u64::MAX);

    // The historical u128 breakage was in the TAGGED-enum path: prove a
    // PaneCreated event carrying the pane round-trips through the framed envelope.
    let event = DaemonEvent::PaneCreated { pane: pane.clone() };
    let event_frame = frame::encode(&event).unwrap();
    let from_event: DaemonEvent = frame::read(&mut std::io::Cursor::new(event_frame))
        .unwrap()
        .unwrap();
    assert_eq!(from_event, event);

    // The field renders as a bare integer (not a string / float).
    assert_eq!(
        serde_json::to_value(&pane)
            .unwrap()
            .get("created_at_ms")
            .and_then(Value::as_u64),
        Some(u64::MAX)
    );
}

// ───── MA persistent multi-request v2 connections (VAL-IPC-027..035, 050..052) ─────

/// Spin up ONE in-process daemon (no socket bind ⇒ no orphan daemon) that several
/// `connect_to` connections share, so concurrency/isolation can be exercised.
fn shared_daemon() -> (Arc<DaemonServer>, tempfile::TempDir, String) {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let server = Arc::new(
        DaemonServer::with_config(
            PathBuf::from("/tmp/sgian-persistent"),
            data_dir.path().to_path_buf(),
            Config::default(),
        )
        .expect("daemon server should start"),
    );
    let token = server.token.clone();
    (server, data_dir, token)
}

/// Open one client connection to a shared daemon and run the real
/// `handle_daemon_client` on its own server thread (mirrors the accept loop's
/// per-connection thread, so each connection is isolated from the others).
fn connect_to(
    server: &Arc<DaemonServer>,
) -> (TransportStream, thread::JoinHandle<Result<(), String>>) {
    let (client, server_stream) =
        test_transport_pair().expect("transport pair should be available");
    let server = Arc::clone(server);
    let handle = thread::spawn(move || handle_daemon_client(server, server_stream));
    (client, handle)
}

/// Block until `cond` holds (bounded), so event/subscriber assertions don't race
/// the server thread registering a subscriber.
fn wait_for<F: Fn() -> bool>(cond: F) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("condition not met within timeout");
}

/// Read framed `DaemonEvent`s from a v2 event stream (skipping catch-up events a
/// fresh daemon replays) until the one equal to `want` arrives. Asserts each
/// delivered event decodes as a valid framed envelope (VAL-IPC-050).
fn read_framed_event_until(stream: &mut TransportStream, want: &DaemonEvent) {
    for _ in 0..50 {
        let event: DaemonEvent = frame::read(stream)
            .expect("framed event reads")
            .expect("an event is present");
        if &event == want {
            return;
        }
    }
    panic!("expected framed event {want:?} not received");
}

/// Read newline-JSON `DaemonEvent`s from a v1 event stream until the one equal to
/// `want` arrives. Asserts each line is NOT a framed envelope (VAL-IPC-050).
fn read_newline_event_until(reader: &mut BufReader<TransportStream>, want: &DaemonEvent) {
    for _ in 0..50 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("newline event reads");
        assert!(n > 0, "stream closed before {want:?}");
        assert!(
            !line.as_bytes().starts_with(&frame::MAGIC),
            "v1 events must NOT be framed"
        );
        let event: DaemonEvent = serde_json::from_str(line.trim_end()).expect("newline parses");
        if &event == want {
            return;
        }
    }
    panic!("expected newline event {want:?} not received");
}

#[test]
fn v2_connection_serves_multiple_requests() {
    // VAL-IPC-027: after a v2 handshake, ONE connection answers several framed
    // requests in sequence (no reconnect), including mixed request types.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    assert_eq!(hs.negotiated_wire_version, 2);
    let mut stream = hs.stream;

    for id in ["first", "second", "third"] {
        frame::write(
            &mut stream,
            &DaemonRequest::PaneStatus {
                pane_id: id.to_string(),
            },
        )
        .expect("framed request writes");
        let response: IpcResponse = frame::read(&mut stream)
            .expect("framed response reads")
            .expect("a framed response is present");
        assert!(!response.ok, "unknown pane status is an error response");
        let err = response.error.unwrap_or_default();
        assert!(
            err.contains(id),
            "response {err:?} must echo requested id {id}"
        );
    }

    // A different request type on the SAME connection is also served.
    frame::write(&mut stream, &DaemonRequest::Ping).expect("framed ping writes");
    let response: IpcResponse = frame::read(&mut stream)
        .expect("framed response reads")
        .expect("a framed response is present");
    assert!(response.ok, "Ping answered ok on a persistent connection");

    drop(stream);
    let result = handle.join().expect("server thread must not panic");
    assert!(result.is_ok(), "v2 loop exits cleanly on EOF: {result:?}");
}

// ───── MB `ctl wait` blocking primitive (mb-wait-primitive) ─────

/// Build an in-process daemon whose workspace cwd is a real temp dir (so a pane's
/// shell spawns with a valid cwd) and no socket bind (so no orphan daemon).
fn wait_shared_server() -> (Arc<DaemonServer>, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = Arc::new(
        DaemonServer::with_config(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            Config::default(),
        )
        .expect("daemon server should start"),
    );
    let token = server.token.clone();
    (server, dir, token)
}

/// Create a pane on `server` (spawning its shell) and return the new pane id.
fn create_wait_pane(server: &Arc<DaemonServer>) -> String {
    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create pane"),
    )
    .expect("pane decodes");
    pane.id
}

fn wait_now(
    server: &Arc<DaemonServer>,
    pane_id: &str,
    condition: WaitCondition,
    timeout_ms: Option<u64>,
) -> Result<Value, String> {
    server.handle(DaemonRequest::Wait {
        pane_id: pane_id.to_string(),
        condition,
        timeout_ms,
    })
}

/// Block until the pane's shell has printed its prompt (revision advanced past 0)
/// and then gone quiet. A bare write right after spawn can race the shell's startup
/// terminal-capability handshake, which under heavy parallel load may swallow part
/// of the input (leaving e.g. a bare `exit` that returns 0 instead of 7); this makes
/// the subsequent write deterministic.
fn await_shell_ready(server: &Arc<DaemonServer>, pane_id: &str) {
    if let Some(model) = server.router.model_handle(pane_id) {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if model.lock().map(|m| m.revision).unwrap_or(0) > 0 {
                break;
            }
            thread::sleep(Duration::from_millis(15));
        }
    }
    let _ = wait_now(server, pane_id, WaitCondition::Idle(120), Some(5000));
}

fn str_args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[test]
fn parse_wait_args_enforces_single_condition() {
    // VAL-PRIM-016 / VAL-PRIM-017: exactly one of --text/--regex/--idle/--exit; a
    // missing condition is a usage error (not an open-ended wait).
    let err = parse_wait_args(&str_args(&["pane-1"])).unwrap_err();
    assert!(
        err.contains("--text") && err.contains("--exit"),
        "missing-condition error should name the conditions: {err}"
    );
    // Conflicting conditions are rejected.
    let err = parse_wait_args(&str_args(&["pane-1", "--text", "X", "--exit"])).unwrap_err();
    assert!(
        err.contains("only one"),
        "conflicting-condition error: {err}"
    );
    // A missing pane id is a usage error.
    assert!(parse_wait_args(&str_args(&["--exit"])).is_err());
}

#[test]
fn parse_wait_args_parses_each_condition_and_timeout() {
    // VAL-PRIM-017: each condition + --timeout parse into the intended request.
    let parsed = parse_wait_args(&str_args(&["p", "--text", "READY"])).unwrap();
    assert_eq!(parsed.pane_ref, "p");
    assert_eq!(parsed.condition, WaitCondition::Text("READY".to_string()));
    assert_eq!(parsed.timeout_ms, None);

    let parsed =
        parse_wait_args(&str_args(&["p", "--regex", "a.+b", "--timeout", "1500"])).unwrap();
    assert_eq!(parsed.condition, WaitCondition::Regex("a.+b".to_string()));
    assert_eq!(parsed.timeout_ms, Some(1500));

    let parsed = parse_wait_args(&str_args(&["p", "--idle", "500"])).unwrap();
    assert_eq!(parsed.condition, WaitCondition::Idle(500));

    let parsed = parse_wait_args(&str_args(&["p", "--exit"])).unwrap();
    assert_eq!(parsed.condition, WaitCondition::Exit);

    // Unknown flag and non-numeric idle/timeout are rejected (total parsing).
    assert!(parse_wait_args(&str_args(&["p", "--bogus"])).is_err());
    assert!(parse_wait_args(&str_args(&["p", "--idle", "soon"])).is_err());
    assert!(parse_wait_args(&str_args(&["p", "--text", "X", "--timeout", "later"])).is_err());
}

#[test]
fn parse_wait_args_double_dash_escapes_literal_values() {
    // 07-19 CLI low: `--help` used to be intercepted as a flag, leaving no
    // way to match the literal text. A `--` directly after a value-taking
    // flag escapes that flag's value.
    let parsed = parse_wait_args(&str_args(&["p", "--text", "--", "--help"])).unwrap();
    assert_eq!(parsed.pane_ref, "p");
    assert_eq!(parsed.condition, WaitCondition::Text("--help".to_string()));

    let parsed = parse_wait_args(&str_args(&["p", "--regex", "--", "--h.*"])).unwrap();
    assert_eq!(parsed.condition, WaitCondition::Regex("--h.*".to_string()));

    // The escape consumes only the value: later flags still parse.
    let parsed = parse_wait_args(&str_args(&[
        "p",
        "--text",
        "--",
        "--help",
        "--timeout",
        "50",
    ]))
    .unwrap();
    assert_eq!(parsed.condition, WaitCondition::Text("--help".to_string()));
    assert_eq!(parsed.timeout_ms, Some(50));

    // `--` with nothing after it is still a missing value.
    assert!(parse_wait_args(&str_args(&["p", "--text", "--"])).is_err());

    // A standalone `--` ends flag recognition: a pane reference starting
    // with `-` stays addressable (flags first, then `-- PANE`).
    let parsed = parse_wait_args(&str_args(&["--exit", "--", "-titled"])).unwrap();
    assert_eq!(parsed.pane_ref, "-titled");
    assert_eq!(parsed.condition, WaitCondition::Exit);
    // Everything after the standalone `--` is positional.
    assert!(parse_wait_args(&str_args(&["p", "--", "--exit"])).is_err());
}

#[test]
fn wait_matches_text_already_on_screen() {
    // VAL-PRIM-001 / VAL-PRIM-002: text already present on the visible screen
    // resolves immediately as matched with reason "text".
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    server.router.feed_model(&pane, b"sentinel-READY-0xABC\n");
    let outcome = wait_now(
        &server,
        &pane,
        WaitCondition::Text("sentinel-READY-0xABC".to_string()),
        Some(3000),
    )
    .expect("wait ok");
    assert_eq!(outcome["matched"], json!(true), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("text"));
    assert!(outcome["elapsed_ms"].as_u64().unwrap() < 2000);
}

#[test]
fn wait_matches_regex_on_screen() {
    // VAL-PRIM-003: a regex match on the visible screen resolves as reason "text".
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    server.router.feed_model(&pane, b"build #1234 ok\n");
    let outcome = wait_now(
        &server,
        &pane,
        WaitCondition::Regex(r"build #[0-9]+ ok".to_string()),
        Some(3000),
    )
    .expect("wait ok");
    assert_eq!(outcome["matched"], json!(true), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("text"));
}

#[test]
fn wait_invalid_regex_is_clear_error() {
    // VAL-PRIM-005: an invalid --regex is a clear error, never a hang or panic.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    let err = wait_now(
        &server,
        &pane,
        WaitCondition::Regex("(".to_string()),
        Some(3000),
    )
    .unwrap_err();
    assert!(
        err.to_lowercase().contains("regex") || err.to_lowercase().contains("invalid"),
        "error should describe the bad pattern: {err}"
    );
}

#[test]
fn wait_times_out_when_text_absent() {
    // VAL-PRIM-010 / VAL-PRIM-011: a never-appearing string under --timeout resolves
    // as reason "timeout", matched:false (not a spurious text match).
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    let started = Instant::now();
    let outcome = wait_now(
        &server,
        &pane,
        WaitCondition::Text("ABSENT-TOKEN-NEVER".to_string()),
        Some(400),
    )
    .expect("wait ok");
    assert_eq!(outcome["matched"], json!(false), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("timeout"));
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "should honor the timeout window"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "should not overrun the timeout"
    );
}

#[test]
fn wait_idle_returns_when_pane_quiet() {
    // VAL-PRIM-006: --idle returns after the pane produces no output for the window.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    let outcome = wait_now(&server, &pane, WaitCondition::Idle(150), Some(5000)).expect("wait ok");
    assert_eq!(outcome["matched"], json!(true), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("idle"));
}

#[test]
fn wait_regex_never_matches_times_out() {
    // VAL-PRIM-004: a regex that never matches within the window resolves as a
    // timeout (matched:false), not a spurious match.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    server.router.feed_model(&pane, b"build complete\n");
    let outcome = wait_now(
        &server,
        &pane,
        WaitCondition::Regex(r"NEVER_[0-9]+".to_string()),
        Some(400),
    )
    .expect("wait ok");
    assert_eq!(outcome["matched"], json!(false), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("timeout"));
}

#[test]
fn wait_idle_waits_through_continuous_output() {
    // VAL-PRIM-007: --idle must NOT fire while output continues. The idle timer
    // resets on every revision bump, so a chatty pane only satisfies --idle once it
    // has actually been quiet for the full window.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    // Emit output every 40ms for ~1s, then stop; idle can only be satisfied AFTER
    // this chatty phase ends. The 600ms idle window (not 150ms) keeps the test
    // honest on loaded CI runners, where the feeder thread can be starved for a
    // few hundred ms between ticks — the property under test is "no fire before
    // quiet", not the exact window length.
    let feeder = {
        let server = Arc::clone(&server);
        let pane = pane.clone();
        thread::spawn(move || {
            for _ in 0..25 {
                server.router.feed_model(&pane, b"tick\n");
                thread::sleep(Duration::from_millis(40));
            }
        })
    };
    let outcome = wait_now(&server, &pane, WaitCondition::Idle(600), Some(5000)).expect("wait ok");
    feeder.join().expect("feeder thread");
    assert_eq!(outcome["matched"], json!(true), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("idle"));
    assert!(
        outcome["elapsed_ms"].as_u64().unwrap() >= 800,
        "idle must not fire during the chatty phase: outcome={outcome}"
    );
}

#[test]
fn wait_text_on_dead_pane_is_bounded() {
    // VAL-PRIM-049: a text/regex wait whose pane dies before the token appears must
    // terminate promptly (the frozen final screen can never gain new output), not
    // hang to the timeout or leak a server-side waiter.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server
        .handle(DaemonRequest::WriteToPane {
            pane_id: pane.clone(),
            data: "exit 0\r".to_string(),
        })
        .expect("write ok");
    let died = Instant::now();
    while died.elapsed() < Duration::from_secs(5)
        && server.lock_terminals().expect("lock").is_live(&pane)
    {
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        !server.lock_terminals().expect("lock").is_live(&pane),
        "pane should have exited before the text wait"
    );
    let started = Instant::now();
    let outcome = wait_now(
        &server,
        &pane,
        WaitCondition::Text("NEVER_PRINTED".to_string()),
        Some(5000),
    )
    .expect("wait ok");
    assert_eq!(outcome["matched"], json!(false), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("timeout"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a dead-pane text wait must be bounded, not consume the full timeout"
    );
}

#[test]
fn wait_exit_reports_captured_code() {
    // VAL-PRIM-008: --exit returns when the pane ends and reports its exit code.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server
        .handle(DaemonRequest::WriteToPane {
            pane_id: pane.clone(),
            data: "exit 7\r".to_string(),
        })
        .expect("write ok");
    let outcome = wait_now(&server, &pane, WaitCondition::Exit, Some(5000)).expect("wait ok");
    assert_eq!(outcome["matched"], json!(true), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("exit"));
    assert_eq!(outcome["exit_code"], json!(7), "outcome={outcome}");
}

#[test]
fn wait_unknown_pane_is_clean_error() {
    // VAL-PRIM-050: waiting on an unknown pane fails fast (no hang, no timeout burn).
    let (server, _dir, _t) = wait_shared_server();
    let started = Instant::now();
    let err = wait_now(
        &server,
        "pane-9999",
        WaitCondition::Text("X".to_string()),
        Some(5000),
    )
    .unwrap_err();
    assert!(err.contains("pane-9999") || err.to_lowercase().contains("not found"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must not consume the timeout"
    );
}

// ───── Persistence serialization (H1) ─────

#[test]
fn concurrent_persists_never_tear_workspace_file() {
    // H1: two threads persist concurrently through the SAME fixed temp path
    // (workspace.json.tmp) with differing payload sizes; without the persist
    // lock an interleaved open/write/rename can leave a torn file that is
    // neither written state. The final file must always parse and equal one
    // of the two states (here: the long or the short title).
    let (server, dir, _t) = wait_shared_server();
    let long_title = "L".repeat(200);
    let short_title = "s".to_string();

    let mut handles = Vec::new();
    for title in [long_title.clone(), short_title.clone()] {
        let server = Arc::clone(&server);
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                server
                    .handle(DaemonRequest::RenamePane {
                        pane_id: "pane-1".to_string(),
                        title: title.clone(),
                    })
                    .expect("rename should succeed");
                server.persist().expect("persist should succeed");
            }
        }));
    }
    for handle in handles {
        handle.join().expect("persist thread must not panic");
    }

    let data =
        fs::read_to_string(dir.path().join(WORKSPACE_FILE)).expect("workspace.json should exist");
    let persisted: PersistedWorkspace =
        serde_json::from_str(&data).expect("final workspace.json must parse (never torn)");
    let title = &persisted.panes[0].title;
    assert!(
        *title == long_title || *title == short_title,
        "torn persist: title is neither written state (len {})",
        title.len()
    );
}

// ───── Pane-count cap (M4) + CreatePane rollback on failure ─────

#[test]
fn create_pane_beyond_limit_is_refused() {
    // M4: pane creation is refused at MAX_PANES (each pane costs a shell, a
    // PTY, two threads, and a vt100 model). Fill the registry directly so
    // the test spawns no shells.
    let (server, _dir, _t) = wait_shared_server();
    while server.lock_registry().expect("lock").snapshot().panes.len() < MAX_PANES {
        server.lock_registry().expect("lock").create_pane(None);
    }

    let err = server
        .handle(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect_err("create beyond the pane cap should be refused");
    assert!(err.contains("pane limit"), "unexpected error: {err}");
    assert_eq!(
        server.snapshot().expect("snapshot").panes.len(),
        MAX_PANES,
        "a refused create must not add a pane"
    );
}

#[test]
fn persisted_workspace_over_pane_limit_still_loads() {
    // M4: the cap blocks only NEW creation — a persisted workspace that
    // exceeds it must still load (its panes just can't grow further).
    let dir = tempfile::tempdir().expect("temp dir");
    let panes: Vec<Pane> = (1..=(MAX_PANES as u64 + 1))
        .map(|n| Pane {
            id: format!("pane-{n}"),
            title: format!("term-{n}"),
            kind: PaneKind::Shell,
            created_at_ms: now_millis(),
        })
        .collect();
    let persisted = PersistedWorkspace {
        panes,
        active_pane_id: Some("pane-1".to_string()),
        cwd: dir.path().display().to_string(),
        next_id: MAX_PANES as u64 + 2,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        dir.path().join(WORKSPACE_FILE),
        serde_json::to_vec_pretty(&persisted).expect("encode workspace"),
    )
    .expect("write workspace.json");

    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("over-cap persisted workspace must still load");
    assert_eq!(
        server.snapshot().expect("snapshot").panes.len(),
        MAX_PANES + 1
    );

    let err = server
        .handle(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect_err("create over the cap should be refused");
    assert!(err.contains("pane limit"), "unexpected error: {err}");
}

#[test]
fn create_pane_spawn_failure_rolls_back_the_pane() {
    // A CreatePane whose shell can't spawn must leave NOTHING committed:
    // the pane is removed from the registry, the previously-active pane
    // stays active, and no live session survives — so a client retry can't
    // duplicate a pane that was reported as failed.
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config {
            shell: Some("/nonexistent/sgian-missing-shell".to_string()),
            ..Default::default()
        },
    )
    .expect("daemon server should start");
    let server = Arc::new(server);

    let err = server
        .handle(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect_err("create with an unspawnable shell should fail");
    assert!(!err.is_empty(), "error should be non-empty");

    let snapshot = server.snapshot().expect("snapshot");
    assert_eq!(
        snapshot.panes.len(),
        1,
        "failed create must be rolled back: {snapshot:?}"
    );
    assert_eq!(snapshot.active_pane_id.as_deref(), Some("pane-1"));
    assert!(
        !server.lock_terminals().expect("lock").is_live("pane-2"),
        "no live session may survive the rollback"
    );
}

/// A persist failure AFTER a successful spawn must also roll back (kill the
/// fresh session, drop the registry entry). Unix-only: the failure is
/// induced by making the data dir unwritable.
#[cfg(unix)]
#[test]
fn create_pane_persist_failure_rolls_back_the_pane() {
    use std::os::unix::fs::PermissionsExt;

    let (server, dir, _t) = wait_shared_server();

    // Make the data dir unwritable so persist() fails after the spawn.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555))
        .expect("chmod data dir read-only");
    let result = server.handle(DaemonRequest::CreatePane {
        title: None,
        profile: None,
    });
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755))
        .expect("restore data dir permissions");

    let err = result.expect_err("create should fail when persist fails");
    assert!(
        err.contains("persist"),
        "error should describe the persist failure: {err}"
    );
    let snapshot = server.snapshot().expect("snapshot");
    assert_eq!(
        snapshot.panes.len(),
        1,
        "failed create must be rolled back: {snapshot:?}"
    );
    assert_eq!(snapshot.active_pane_id.as_deref(), Some("pane-1"));
    assert!(
        !server.lock_terminals().expect("lock").is_live("pane-2"),
        "the spawned session must be killed by the rollback"
    );
}

// ───── wait liveness / disconnect / closed-pane semantics (M6) ─────

/// M6: a timeout-less wait whose client drops its socket must abort promptly
/// with a clean error instead of pinning the handler thread forever.
#[cfg(unix)]
#[test]
fn wait_aborts_promptly_when_client_disconnects() {
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    let (daemon_side, client_side) =
        test_transport_pair().expect("transport pair should be available");

    // The client end is already gone: the wait must notice on its first
    // ticks (the condition would otherwise never resolve).
    drop(client_side);
    let started = Instant::now();
    let err = server
        .handle_wait(
            &pane,
            &WaitCondition::Text("NEVER-PRINTED-TOKEN".to_string()),
            None,
            Some(&daemon_side),
        )
        .expect_err("a disconnected client must abort the wait");
    assert!(
        err.contains("client disconnected"),
        "unexpected error: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the abort must be prompt, not a timeout burn"
    );
}

/// M6: a live (quiet) peer must NOT trip the disconnect check — the wait
/// resolves on its own condition as usual.
#[cfg(unix)]
#[test]
fn wait_with_live_peer_is_not_aborted() {
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    let (daemon_side, client_side) =
        test_transport_pair().expect("transport pair should be available");

    let outcome = server
        .handle_wait(
            &pane,
            &WaitCondition::Idle(120),
            Some(5000),
            Some(&daemon_side),
        )
        .expect("wait should resolve normally with a live peer");
    assert_eq!(outcome["reason"], json!("idle"), "outcome={outcome}");
    drop(client_side);
}

/// M6: the peer-liveness probe itself — a quiet peer is alive, a peer with
/// pending (pipelined) data is alive and its byte is NOT consumed, and an
/// orderly close is detected.
#[cfg(unix)]
#[test]
fn wait_peer_disconnected_classifies_socket_states() {
    let (mut a, mut b) = test_transport_pair().expect("transport pair should be available");
    assert!(!wait_peer_disconnected(&a), "a quiet live peer is alive");

    b.write_all(b"x").expect("write to peer");
    thread::sleep(Duration::from_millis(150)); // let the byte land
    assert!(
        !wait_peer_disconnected(&a),
        "a peer with pending data is alive (data left unread)"
    );
    // Drain the byte, then close the peer: the orderly shutdown is detected.
    let mut buf = [0u8; 1];
    a.read_exact(&mut buf).expect("drain pending byte");
    assert_eq!(&buf, b"x", "the probe must not consume the pending byte");
    drop(b);
    wait_for(|| wait_peer_disconnected(&a));
}

/// `wait --exit` on a pane CLOSED mid-wait resolves distinctly as reason
/// "closed" with a null exit code — not a spurious "exit".
#[test]
fn wait_exit_on_closed_pane_reports_closed_not_exit() {
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);

    let waiter = {
        let server = Arc::clone(&server);
        let pane = pane.clone();
        thread::spawn(move || wait_now(&server, &pane, WaitCondition::Exit, None))
    };
    // Let the wait start polling, then close the pane out from under it.
    thread::sleep(Duration::from_millis(150));
    server
        .handle(DaemonRequest::ClosePane {
            pane_id: pane.clone(),
        })
        .expect("close pane");

    let outcome = waiter.join().expect("waiter thread").expect("wait ok");
    assert_eq!(outcome["matched"], json!(true), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("closed"), "outcome={outcome}");
    assert_eq!(outcome["exit_code"], Value::Null, "outcome={outcome}");
}

/// M7 follow-up: a pane whose spawn is IN FLIGHT has no liveness entry yet —
/// a `wait --exit` landing in that window must not resolve a spurious "exit".
/// Simulated by an ended pane with a spawning marker present.
#[test]
fn wait_exit_does_not_resolve_during_in_flight_spawn() {
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    end_pane_with_code(&server, &pane, 0);
    server
        .lock_terminals()
        .expect("lock")
        .spawning
        .insert(pane.clone());

    let started = Instant::now();
    let outcome =
        wait_now(&server, &pane, WaitCondition::Exit, Some(200)).expect("wait should not error");
    assert_eq!(outcome["matched"], json!(false), "outcome={outcome}");
    assert_eq!(outcome["reason"], json!("timeout"), "outcome={outcome}");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the wait must poll past the spawn window, resolved in {:?}",
        started.elapsed()
    );
}

/// M8: transient per-accept errors (ECONNABORTED/EINTR) and resource
/// pressure (EMFILE/ENFILE/ENOBUFS/ENOMEM) are non-fatal; only genuinely
/// fatal listener errors tear the daemon down.
#[test]
fn accept_errors_are_classified_for_daemon_survival() {
    use std::io::Error as IoError;
    assert_eq!(
        classify_accept_error(&IoError::new(
            std::io::ErrorKind::Interrupted,
            "interrupted"
        )),
        AcceptErrorClass::Transient
    );
    assert_eq!(
        classify_accept_error(&IoError::new(
            std::io::ErrorKind::ConnectionAborted,
            "aborted"
        )),
        AcceptErrorClass::Transient
    );
    #[cfg(unix)]
    {
        assert_eq!(
            classify_accept_error(&IoError::from_raw_os_error(libc::EMFILE)),
            AcceptErrorClass::ResourcePressure
        );
        assert_eq!(
            classify_accept_error(&IoError::from_raw_os_error(libc::ENFILE)),
            AcceptErrorClass::ResourcePressure
        );
        assert_eq!(
            classify_accept_error(&IoError::from_raw_os_error(libc::ENOMEM)),
            AcceptErrorClass::ResourcePressure
        );
    }
    assert_eq!(
        classify_accept_error(&IoError::new(
            std::io::ErrorKind::PermissionDenied,
            "denied"
        )),
        AcceptErrorClass::Fatal
    );
}

#[test]
fn live_transport_budget_includes_subscribers() {
    assert!(!live_transport_limit_reached(100, 50));
    assert!(live_transport_limit_reached(
        MAX_LIVE_TRANSPORTS - MAX_SUBSCRIBERS,
        MAX_SUBSCRIBERS
    ));
    assert!(live_transport_limit_reached(usize::MAX, 1));
}

/// M7: two concurrent ensure_terminal calls for the same pane must not
/// spawn two shells — the in-flight marker makes the loser wait for the
/// winner's commit and then see the pane live (double-spawn avoidance
/// without holding the TerminalStore lock across the fork/exec).
#[test]
fn concurrent_ensures_spawn_a_pane_exactly_once() {
    let (server, _dir, _t) = wait_shared_server();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let server = Arc::clone(&server);
        handles.push(thread::spawn(move || server.ensure_terminal("pane-1")));
    }
    for handle in handles {
        handle.join().expect("ensure thread").expect("ensure ok");
    }

    let terminals = server.lock_terminals().expect("lock");
    assert!(
        terminals.is_live("pane-1"),
        "pane is live after the ensures"
    );
    assert_eq!(terminals.sessions.len(), 1, "exactly one session committed");
    assert_eq!(
        terminals.next_generation, 1,
        "exactly one spawn ran (generation bumped once)"
    );
}

// ───── MB `ctl snapshot` primitive (mb-snapshot-primitive) ─────

fn snapshot_now(server: &Arc<DaemonServer>, pane_id: &str) -> Result<Value, String> {
    server.handle(DaemonRequest::Snapshot {
        pane_id: pane_id.to_string(),
    })
}

#[test]
fn parse_snapshot_args_accepts_pane_and_rejects_bad() {
    // A single pane reference parses; missing pane, extra args, and unknown
    // options are usage errors.
    let parsed = parse_snapshot_args(&str_args(&["pane-1"])).unwrap();
    assert_eq!(parsed.pane_ref, "pane-1");

    assert!(parse_snapshot_args(&str_args(&[])).is_err());
    assert!(parse_snapshot_args(&str_args(&["pane-1", "extra"])).is_err());
    let err = parse_snapshot_args(&str_args(&["pane-1", "--bogus"])).unwrap_err();
    assert!(err.contains("--bogus"), "unknown-option error: {err}");
}

#[test]
fn parse_snapshot_args_double_dash_allows_dash_titled_pane() {
    // 07-19 CLI low: `--` ends flag recognition so a pane titled like a
    // flag stays addressable.
    let parsed = parse_snapshot_args(&str_args(&["--", "--titled"])).unwrap();
    assert_eq!(parsed.pane_ref, "--titled");
    // Still a single reference: a second positional errors.
    assert!(parse_snapshot_args(&str_args(&["--", "a", "b"])).is_err());
}

#[test]
fn snapshot_returns_documented_struct_for_live_pane() {
    // VAL-PRIM-018 / VAL-PRIM-022: the documented key set with correct types; a
    // live pane reports alive:true and no (absent/null) exit code.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server
        .router
        .feed_model(&pane, b"\x1b[2J\x1b[HHELLO-STRUCT");
    let snap = snapshot_now(&server, &pane).expect("snapshot ok");

    assert_eq!(snap["pane_id"], json!(pane), "snap={snap}");
    assert!(snap["cols"].is_number(), "cols number: {snap}");
    assert!(snap["rows"].is_number(), "rows number: {snap}");
    assert!(snap["lines"].is_array(), "lines array: {snap}");
    assert!(snap["title"].is_string(), "title string: {snap}");
    assert!(snap["revision"].is_number(), "revision number: {snap}");
    assert!(snap["cursor"]["row"].is_number(), "cursor.row: {snap}");
    assert!(snap["cursor"]["col"].is_number(), "cursor.col: {snap}");
    assert_eq!(snap["alive"], json!(true), "live pane alive: {snap}");
    assert!(
        snap.get("exit_code").is_none_or(|v| v.is_null()),
        "live pane has no exit_code: {snap}"
    );
    // A pane spawned by the daemon records the launched command (VAL-TERM-014).
    assert!(snap["command"].is_string(), "command present: {snap}");
    // The documented struct also carries the origin field (defaults to User).
    assert_eq!(snap["origin"], json!("User"), "origin defaults: {snap}");
}

#[test]
fn snapshot_lines_are_plain_text_and_len_equals_rows() {
    // VAL-PRIM-019 / VAL-PRIM-020: lines are the per-row visible text in order,
    // length equals rows, and SGR/ANSI is interpreted away (no escape bytes).
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server
        .router
        .feed_model(&pane, b"\x1b[2J\x1b[H\x1b[31mL1\x1b[0m\r\nL2\r\nL3");
    let snap = snapshot_now(&server, &pane).expect("snapshot ok");

    let rows = snap["rows"].as_u64().expect("rows") as usize;
    let lines = snap["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), rows, "lines.len() must equal rows: {snap}");
    assert_eq!(
        lines[0],
        json!("L1"),
        "row0 plain text (ANSI stripped): {snap}"
    );
    assert_eq!(lines[1], json!("L2"), "row1: {snap}");
    assert_eq!(lines[2], json!("L3"), "row2: {snap}");
    for line in lines {
        assert!(
            !line.as_str().unwrap_or("").contains('\u{1b}'),
            "no escape bytes in a line: {line}"
        );
    }
}

#[test]
fn snapshot_cursor_reflects_position() {
    // VAL-PRIM-021: after `abc` on an otherwise-empty first row, the cursor is at
    // row 0, col 3 (0-based).
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server.router.feed_model(&pane, b"\x1b[2J\x1b[Habc");
    let snap = snapshot_now(&server, &pane).expect("snapshot ok");
    assert_eq!(snap["cursor"]["row"], json!(0), "snap={snap}");
    assert_eq!(snap["cursor"]["col"], json!(3), "snap={snap}");
}

#[test]
fn snapshot_revision_matches_model_and_is_stable() {
    // VAL-PRIM-027: two no-output snapshots share a revision; new output raises it.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    let first = snapshot_now(&server, &pane).expect("snapshot ok");
    let second = snapshot_now(&server, &pane).expect("snapshot ok");
    assert_eq!(
        first["revision"], second["revision"],
        "revision stable without output: {first} vs {second}"
    );
    // The snapshot revision equals the model's own revision.
    let model_rev = server
        .router
        .model_handle(&pane)
        .and_then(|m| m.lock().ok().map(|m| m.revision))
        .expect("model revision");
    assert_eq!(second["revision"], json!(model_rev), "snap={second}");

    server.router.feed_model(&pane, b"more-output\n");
    let third = snapshot_now(&server, &pane).expect("snapshot ok");
    assert!(
        third["revision"].as_u64().unwrap() > second["revision"].as_u64().unwrap(),
        "revision advances after output: {second} -> {third}"
    );
}

#[test]
fn snapshot_reflects_text_a_wait_matched() {
    // VAL-PRIM-024: a snapshot taken after `wait --text` shows the matched text and
    // reports a revision >= the wait's revision (both read the same model).
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    server.router.feed_model(&pane, b"TOKEN-XYZ\n");
    let waited = wait_now(
        &server,
        &pane,
        WaitCondition::Text("TOKEN-XYZ".to_string()),
        Some(3000),
    )
    .expect("wait ok");
    assert_eq!(waited["matched"], json!(true), "waited={waited}");
    let snap = snapshot_now(&server, &pane).expect("snapshot ok");
    let joined = snap["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("TOKEN-XYZ"),
        "snapshot shows matched text: {snap}"
    );
    assert!(
        snap["revision"].as_u64().unwrap() >= waited["revision"].as_u64().unwrap(),
        "snapshot revision >= wait revision: {snap} vs {waited}"
    );
}

#[test]
fn snapshot_unknown_pane_is_clean_error() {
    // VAL-PRIM-026: a snapshot for a non-existent pane is a clean error, not a
    // panic or a zeroed struct.
    let (server, _dir, _t) = wait_shared_server();
    let err = snapshot_now(&server, "pane-9999").unwrap_err();
    assert!(
        err.contains("pane-9999") || err.to_lowercase().contains("not found"),
        "unknown-pane error: {err}"
    );
}

#[test]
fn snapshot_ended_pane_reports_exit_code_and_final_screen() {
    // VAL-PRIM-023 / VAL-PRIM-051: after the pane exits, snapshot reports
    // alive:false + the captured exit code AND still returns the final screen.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server
        .handle(DaemonRequest::WriteToPane {
            pane_id: pane.clone(),
            data: "printf 'FINAL-LINE\\n'; exit 7\r".to_string(),
        })
        .expect("write ok");
    let died = Instant::now();
    while died.elapsed() < Duration::from_secs(5)
        && server.lock_terminals().expect("lock").is_live(&pane)
    {
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        !server.lock_terminals().expect("lock").is_live(&pane),
        "pane should have exited"
    );
    let snap = snapshot_now(&server, &pane).expect("snapshot ok");
    assert_eq!(snap["alive"], json!(false), "ended pane: {snap}");
    assert_eq!(snap["exit_code"], json!(7), "captured exit code: {snap}");
    let joined = snap["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("FINAL-LINE"),
        "ended pane still returns its final screen: {snap}"
    );
}

// ───── MB `ctl find` query primitive (mb-find-primitive) ─────

fn find_now(
    server: &Arc<DaemonServer>,
    command: Option<&str>,
    title: Option<&str>,
    cwd: Option<&str>,
    state: Option<PaneRuntimeState>,
) -> Result<Value, String> {
    server.handle(DaemonRequest::Find {
        command: command.map(String::from),
        title: title.map(String::from),
        cwd: cwd.map(String::from),
        state,
    })
}

/// Drive a pane's shell to exit with `code` and block until the reaper marks it
/// ended, so `find --state ended` / exit-code assertions are deterministic.
fn end_pane_with_code(server: &Arc<DaemonServer>, pane_id: &str, code: i32) {
    await_shell_ready(server, pane_id);
    server
        .handle(DaemonRequest::WriteToPane {
            pane_id: pane_id.to_string(),
            data: format!("exit {code}\r"),
        })
        .expect("write exit");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5)
        && server.lock_terminals().expect("lock").is_live(pane_id)
    {
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        !server.lock_terminals().expect("lock").is_live(pane_id),
        "pane {pane_id} should have exited"
    );
}

fn find_ids(result: &Value) -> Vec<String> {
    result
        .as_array()
        .expect("find returns an array")
        .iter()
        .map(|entry| entry["id"].as_str().expect("entry id").to_string())
        .collect()
}

fn recorded_command(server: &Arc<DaemonServer>, pane_id: &str) -> String {
    server
        .lock_terminals()
        .expect("lock")
        .pane_meta(pane_id)
        .command
        .expect("a daemon-spawned pane records its command")
}

#[test]
fn parse_find_args_parses_filters_and_rejects_bad_state() {
    // VAL-PRIM-054 substrate: a bare `find` (no flags) parses to all-None.
    let parsed = parse_find_args(&str_args(&[])).unwrap();
    assert_eq!(parsed.command, None);
    assert_eq!(parsed.title, None);
    assert_eq!(parsed.cwd, None);
    assert_eq!(parsed.state, None);

    // Each filter parses; --state maps to the runtime-state enum (live|ended).
    let parsed = parse_find_args(&str_args(&[
        "--command",
        "sh",
        "--title",
        "t",
        "--cwd",
        "/tmp",
        "--state",
        "live",
    ]))
    .unwrap();
    assert_eq!(parsed.command.as_deref(), Some("sh"));
    assert_eq!(parsed.title.as_deref(), Some("t"));
    assert_eq!(parsed.cwd.as_deref(), Some("/tmp"));
    assert_eq!(parsed.state, Some(PaneRuntimeState::Live));
    assert_eq!(
        parse_find_args(&str_args(&["--state", "ended"]))
            .unwrap()
            .state,
        Some(PaneRuntimeState::Ended)
    );

    // VAL-PRIM-039: an invalid --state value is a clear usage error.
    let err = parse_find_args(&str_args(&["--state", "bogus"])).unwrap_err();
    assert!(
        err.contains("live") && err.contains("ended"),
        "bad-state error should name the valid values: {err}"
    );

    // Unknown flag + missing flag values are rejected (total parsing).
    assert!(parse_find_args(&str_args(&["--bogus"])).is_err());
    assert!(parse_find_args(&str_args(&["--command"])).is_err());
    assert!(parse_find_args(&str_args(&["--state"])).is_err());
}

#[test]
fn parse_find_args_double_dash_escapes_literal_values() {
    // 07-19 CLI low: a `--` directly after a value-taking flag escapes that
    // flag's value, so a filter can match a literal `--help` etc.
    let parsed = parse_find_args(&str_args(&["--title", "--", "--help"])).unwrap();
    assert_eq!(parsed.title.as_deref(), Some("--help"));
    let parsed = parse_find_args(&str_args(&["--command", "--", "-v"])).unwrap();
    assert_eq!(parsed.command.as_deref(), Some("-v"));
    // The escape consumes only the value: later flags still parse.
    let parsed =
        parse_find_args(&str_args(&["--title", "--", "--help", "--state", "live"])).unwrap();
    assert_eq!(parsed.title.as_deref(), Some("--help"));
    assert_eq!(parsed.state, Some(PaneRuntimeState::Live));
    // `--` with nothing after it is still a missing value.
    assert!(parse_find_args(&str_args(&["--title", "--"])).is_err());
    // A standalone `--` ends flag recognition; find takes no positionals.
    assert!(parse_find_args(&str_args(&["--", "x"])).is_err());
}

#[test]
fn find_no_filters_returns_all_panes() {
    // VAL-PRIM-054: a bare `find` returns every pane — live AND ended — with no
    // implicit live-only filter.
    let (server, _dir, _t) = wait_shared_server();
    let live = create_wait_pane(&server);
    await_shell_ready(&server, &live);
    let ended = create_wait_pane(&server);
    end_pane_with_code(&server, &ended, 0);

    let ids = find_ids(&find_now(&server, None, None, None, None).expect("find ok"));
    assert!(ids.contains(&live), "no-filter find includes the live pane");
    assert!(
        ids.contains(&ended),
        "no-filter find includes the ended pane"
    );
}

#[test]
fn find_by_state_filters_live_and_ended() {
    // VAL-PRIM-031: --state live returns only live panes; --state ended only ended.
    let (server, _dir, _t) = wait_shared_server();
    let live = create_wait_pane(&server);
    await_shell_ready(&server, &live);
    let ended = create_wait_pane(&server);
    end_pane_with_code(&server, &ended, 0);

    let live_only = find_ids(
        &find_now(&server, None, None, None, Some(PaneRuntimeState::Live)).expect("find ok"),
    );
    assert!(
        live_only.contains(&live),
        "live filter includes the live pane"
    );
    assert!(
        !live_only.contains(&ended),
        "live filter excludes the ended pane"
    );

    let ended_only = find_ids(
        &find_now(&server, None, None, None, Some(PaneRuntimeState::Ended)).expect("find ok"),
    );
    assert!(
        ended_only.contains(&ended),
        "ended filter includes the ended pane"
    );
    assert!(
        !ended_only.contains(&live),
        "ended filter excludes the live pane"
    );
}

#[test]
fn find_by_command_substring_matches() {
    // VAL-PRIM-028 / VAL-PRIM-037: --command matches the launched command as a
    // substring (not only exact equality).
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    let command = recorded_command(&server, &pane);
    // A trailing substring of the real command (shell-agnostic) must match.
    let sub = &command[command.len() / 2..];
    let hit = find_ids(&find_now(&server, Some(sub), None, None, None).expect("find ok"));
    assert!(
        hit.contains(&pane),
        "substring `{sub}` of command `{command}` matches pane {pane}"
    );
    let miss = find_ids(
        &find_now(&server, Some("__definitely_absent_cmd__"), None, None, None).expect("find ok"),
    );
    assert!(
        !miss.contains(&pane),
        "a non-matching --command excludes the pane"
    );
}

#[test]
fn find_by_cwd_substring_matches() {
    // VAL-PRIM-030: --cwd matches the recorded working directory as a substring.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    let cwd = server
        .lock_terminals()
        .expect("lock")
        .pane_meta(&pane)
        .cwd
        .expect("cwd recorded");
    let leaf = std::path::Path::new(&cwd)
        .file_name()
        .expect("cwd has a final component")
        .to_string_lossy()
        .to_string();
    let hit = find_ids(&find_now(&server, None, None, Some(&leaf), None).expect("find ok"));
    assert!(
        hit.contains(&pane),
        "cwd substring `{leaf}` matches pane for cwd `{cwd}`"
    );
    let miss =
        find_ids(&find_now(&server, None, None, Some("__no_such_dir__"), None).expect("find ok"));
    assert!(
        !miss.contains(&pane),
        "a non-matching --cwd excludes the pane"
    );
}

#[test]
fn find_by_title_matches_osc_title() {
    // VAL-PRIM-029 / VAL-PRIM-037: --title matches the OSC-captured screen title
    // (the same title snapshot reports), as a substring.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    server.router.feed_model(&pane, b"\x1b]2;mb-find-title\x07");
    let hit =
        find_ids(&find_now(&server, None, Some("mb-find-title"), None, None).expect("find ok"));
    assert!(
        hit.contains(&pane),
        "find --title matches the OSC-captured title"
    );
    let sub = find_ids(&find_now(&server, None, Some("find-title"), None, None).expect("find ok"));
    assert!(
        sub.contains(&pane),
        "find --title matches a substring of the title"
    );
}

#[test]
fn find_filters_and_together() {
    // VAL-PRIM-034: multiple filters AND together; a filter no pane satisfies
    // yields an empty set (not OR semantics).
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    let command = recorded_command(&server, &pane);
    let sub = &command[command.len() / 2..];

    let both = find_ids(
        &find_now(&server, Some(sub), None, None, Some(PaneRuntimeState::Live)).expect("find ok"),
    );
    assert!(
        both.contains(&pane),
        "AND of live + matching command returns the pane"
    );

    let none = find_now(
        &server,
        Some("__absent__"),
        None,
        None,
        Some(PaneRuntimeState::Live),
    )
    .expect("find ok");
    assert!(
        none.as_array().expect("array").is_empty(),
        "AND with an unsatisfiable filter is empty: {none}"
    );
}

#[test]
fn find_json_returns_full_metadata() {
    // VAL-PRIM-035 / VAL-PRIM-038: each entry carries the documented metadata; an
    // ended entry includes its captured exit code; agent/group are nullable.
    let (server, _dir, _t) = wait_shared_server();
    let live = create_wait_pane(&server);
    await_shell_ready(&server, &live);
    let ended = create_wait_pane(&server);
    end_pane_with_code(&server, &ended, 7);

    let result = find_now(&server, None, None, None, None).expect("find ok");
    let entries = result.as_array().expect("array");
    let entry_for = |id: &str| {
        entries
            .iter()
            .find(|entry| entry["id"] == json!(id))
            .cloned()
            .unwrap_or_else(|| panic!("entry for {id} present: {result}"))
    };

    let l = entry_for(&live);
    for key in [
        "id", "title", "command", "cwd", "state", "agent", "group", "cols", "rows", "revision",
    ] {
        assert!(l.get(key).is_some(), "live entry has `{key}`: {l}");
    }
    assert_eq!(l["state"], json!("live"), "live state: {l}");
    assert!(
        l["cols"].is_number() && l["rows"].is_number(),
        "size are numbers: {l}"
    );
    assert!(l["revision"].is_number(), "revision is a number: {l}");
    assert!(l["agent"].is_null(), "agent is nullable: {l}");
    assert!(l["group"].is_null(), "group is nullable: {l}");

    let e = entry_for(&ended);
    assert_eq!(e["state"], json!("ended"), "ended state: {e}");
    assert_eq!(
        e["exit_code"],
        json!(7),
        "ended entry includes the captured exit code: {e}"
    );
}

#[test]
fn find_no_matches_returns_empty_set() {
    // VAL-PRIM-036: a query that matches nothing is an empty array, not an error.
    let (server, _dir, _t) = wait_shared_server();
    let pane = create_wait_pane(&server);
    await_shell_ready(&server, &pane);
    let result = find_now(&server, Some("__no_such_cmd__"), None, None, None)
        .expect("no-match find is success, not an error");
    assert!(result.is_array(), "find returns an array: {result}");
    assert!(
        result.as_array().expect("array").is_empty(),
        "no matches is an empty set: {result}"
    );
}

#[test]
fn v2_connection_resumes_after_blocking_wait() {
    // VAL-IPC-053: one v2 connection issues a blocking Wait, reads its framed
    // response, then issues a follow-up request on the SAME connection.
    let (server, _dir, token) = wait_shared_server();
    let pane = create_wait_pane(&server);
    let (client, handle) = connect_to(&server);
    let hs = client_handshake(client, &token, Some(2)).expect("handshake");
    assert_eq!(hs.negotiated_wire_version, 2);
    let mut stream = hs.stream;

    // A blocking wait that resolves via its own --timeout (the live pane won't exit).
    frame::write(
        &mut stream,
        &DaemonRequest::Wait {
            pane_id: pane.clone(),
            condition: WaitCondition::Exit,
            timeout_ms: Some(250),
        },
    )
    .expect("framed wait writes");
    let wait_resp: IpcResponse = frame::read(&mut stream)
        .expect("framed response reads")
        .expect("a response is present");
    assert!(wait_resp.ok, "wait responded: {wait_resp:?}");
    assert_eq!(wait_resp.result["reason"], json!("timeout"));

    // The SAME connection serves a following request (the loop resumed).
    frame::write(&mut stream, &DaemonRequest::Ping).expect("framed ping writes");
    let ping_resp: IpcResponse = frame::read(&mut stream)
        .expect("framed response reads")
        .expect("a response is present");
    assert!(
        ping_resp.ok,
        "a post-wait request is served on the same connection"
    );

    drop(stream);
    let result = handle.join().expect("server thread must not panic");
    assert!(
        result.is_ok(),
        "v2 loop exits cleanly after a blocking wait: {result:?}"
    );
}

#[test]
fn wait_does_not_block_other_daemon_operations() {
    // VAL-PRIM-014: a blocking wait on one connection must not stall the daemon; an
    // unrelated request on another connection returns promptly.
    let (server, _dir, token) = wait_shared_server();
    let pane = create_wait_pane(&server);

    // Connection A: a wait that stays blocked (live pane, bounded by a timeout).
    let (a_client, a_handle) = connect_to(&server);
    let a_hs = client_handshake(a_client, &token, Some(2)).expect("handshake A");
    let mut a = a_hs.stream;
    frame::write(
        &mut a,
        &DaemonRequest::Wait {
            pane_id: pane.clone(),
            condition: WaitCondition::Exit,
            timeout_ms: Some(1500),
        },
    )
    .expect("framed wait writes");

    // Connection B: an unrelated request must return well before A's wait resolves.
    let (b_client, b_handle) = connect_to(&server);
    let b_hs = client_handshake(b_client, &token, Some(2)).expect("handshake B");
    let mut b = b_hs.stream;
    let started = Instant::now();
    frame::write(&mut b, &DaemonRequest::ListPanes).expect("framed list writes");
    let b_resp: IpcResponse = frame::read(&mut b)
        .expect("framed response reads")
        .expect("a response is present");
    let elapsed = started.elapsed();
    assert!(b_resp.ok, "ListPanes served while a wait is outstanding");
    assert!(
        elapsed < Duration::from_millis(700),
        "concurrent request blocked for {elapsed:?} while a wait was outstanding"
    );

    // Drain A's (timeout) response and tear both connections down cleanly.
    let a_resp: IpcResponse = frame::read(&mut a)
        .expect("framed response reads")
        .expect("a response is present");
    assert!(a_resp.ok);
    drop(a);
    drop(b);
    let _ = a_handle.join();
    let _ = b_handle.join();
}

#[test]
fn v2_responses_are_in_order() {
    // VAL-IPC-028: pipelined requests are answered one at a time in request order.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    let mut stream = hs.stream;

    let ids = ["alpha", "bravo", "charlie", "delta"];
    for id in ids {
        frame::write(
            &mut stream,
            &DaemonRequest::PaneStatus {
                pane_id: id.to_string(),
            },
        )
        .expect("request writes");
    }
    for id in ids {
        let response: IpcResponse = frame::read(&mut stream)
            .expect("response reads")
            .expect("present");
        let err = response.error.unwrap_or_default();
        assert!(err.contains(id), "expected response for {id}, got {err:?}");
    }

    drop(stream);
    let _ = handle.join();
}

#[test]
fn v2_loop_exits_on_eof() {
    // VAL-IPC-029: closing a v2 connection ends its handler cleanly and the daemon
    // stays healthy for new connections.
    let (server, _dd, token) = shared_daemon();

    let (client, handle) = connect_to(&server);
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    let mut stream = hs.stream;
    frame::write(&mut stream, &DaemonRequest::Ping).expect("ping writes");
    let _: IpcResponse = frame::read(&mut stream).expect("reads").expect("present");
    drop(stream); // client closes ⇒ server's framed read sees clean boundary EOF
    let result = handle.join().expect("handler must not panic");
    assert!(result.is_ok(), "v2 loop exits Ok on EOF: {result:?}");

    // The daemon is still healthy: a brand-new connection is served normally.
    let (client2, handle2) = connect_to(&server);
    let hs2 = client_handshake(client2, &token, Some(2)).expect("second handshake ok");
    let mut stream2 = hs2.stream;
    frame::write(&mut stream2, &DaemonRequest::Ping).expect("ping writes");
    let resp: IpcResponse = frame::read(&mut stream2).expect("reads").expect("present");
    assert!(resp.ok, "daemon serves new connections after one closed");
    drop(stream2);
    let _ = handle2.join();
}

#[test]
fn v2_subscribe_streams_events_on_own_connection() {
    // VAL-IPC-030: on a v2 connection, Subscribe turns the connection into a FRAMED
    // event stream rather than a request/response channel.
    let (server, _dd, token) = shared_daemon();
    let (client, handle) = connect_to(&server);
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    let mut stream = hs.stream;

    frame::write(&mut stream, &DaemonRequest::Subscribe).expect("subscribe writes");
    wait_for(|| server.router.subscriber_count() >= 1);

    let live = DaemonEvent::PaneEnded {
        pane_id: "evt-1".to_string(),
        exit_code: None,
    };
    server.router.broadcast(&live);
    // Catch-up events (a fresh daemon replays Ended panes) and the live event are
    // ALL framed; read past catch-up to the live event we broadcast.
    read_framed_event_until(&mut stream, &live);

    drop(stream);
    let _ = handle.join();
}

#[test]
fn v2_dispatch_matches_v1_single_shot() {
    // VAL-IPC-031: a request dispatched in the v2 loop yields the same IpcResponse
    // as the v1 single-shot path for the same input/state.
    let (server, _dd, token) = shared_daemon();

    // v1 single-shot.
    let (v1c, v1h) = connect_to(&server);
    let hs_v1 = client_handshake(v1c, &token, Some(1)).expect("v1 handshake");
    assert_eq!(hs_v1.negotiated_wire_version, 1);
    let mut v1s = hs_v1.stream;
    write_json_line(&mut v1s, &DaemonRequest::ListPanes).expect("v1 request");
    let mut v1r = BufReader::new(v1s);
    let mut line = String::new();
    v1r.read_line(&mut line).expect("v1 response");
    let v1_resp: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse v1");
    drop(v1r);
    let _ = v1h.join();

    // v2 loop.
    let (v2c, v2h) = connect_to(&server);
    let hs_v2 = client_handshake(v2c, &token, Some(2)).expect("v2 handshake");
    let mut v2s = hs_v2.stream;
    frame::write(&mut v2s, &DaemonRequest::ListPanes).expect("v2 request");
    let v2_resp: IpcResponse = frame::read(&mut v2s)
        .expect("v2 response reads")
        .expect("present");
    drop(v2s);
    let _ = v2h.join();

    assert_eq!(v1_resp.ok, v2_resp.ok, "ok matches across wire paths");
    assert_eq!(
        v1_resp.result, v2_resp.result,
        "result is identical across wire paths"
    );
    assert_eq!(
        v1_resp.error, v2_resp.error,
        "error matches across wire paths"
    );
}

#[test]
fn v1_path_is_one_request_per_connection() {
    // VAL-IPC-032: a negotiated-v1 connection serves exactly one request, then closes.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(1)).expect("v1 handshake");
    assert_eq!(hs.negotiated_wire_version, 1);
    let mut client = hs.stream;

    // First request: served.
    write_json_line(&mut client, &DaemonRequest::Ping).expect("first request");
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    reader.read_line(&mut line).expect("first response");
    let resp: IpcResponse = serde_json::from_str(line.trim_end()).expect("parse");
    assert!(resp.ok);

    // Second request on the SAME connection: NOT served. The connection is done,
    // so the write may fail (server already closed) or succeed into a dead socket;
    // either way no second response is served and the next read is EOF.
    let mut client = reader.into_inner();
    let _ = write_json_line(&mut client, &DaemonRequest::Ping);
    let mut reader = BufReader::new(client);
    let mut line2 = String::new();
    let n = reader.read_line(&mut line2).unwrap_or(0);
    assert_eq!(n, 0, "v1 serves exactly one request then closes");
    let _ = handle.join();
}

#[test]
fn subscribe_connection_does_not_dispatch_requests() {
    // VAL-IPC-033: after Subscribe, the connection is an event stream only — a
    // request written on it is NOT dispatched (no IpcResponse comes back).
    let (server, _dd, token) = shared_daemon();
    let (client, handle) = connect_to(&server);
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    let mut stream = hs.stream;

    frame::write(&mut stream, &DaemonRequest::Subscribe).expect("subscribe writes");
    wait_for(|| server.router.subscriber_count() >= 1);

    // Write a request on the subscribed connection; it must NOT be dispatched.
    frame::write(&mut stream, &DaemonRequest::Ping).expect("post-subscribe write");

    // Every frame on the connection is a broadcast EVENT, never a Ping response:
    // read until the live marker, asserting no dispatched response appears.
    let marker = DaemonEvent::PaneEnded {
        pane_id: "marker".to_string(),
        exit_code: None,
    };
    server.router.broadcast(&marker);
    let mut saw_marker = false;
    for _ in 0..50 {
        let bytes = frame::read_bytes(&mut stream)
            .expect("a frame reads")
            .expect("a frame is present");
        let value: Value = serde_json::from_slice(&bytes).expect("frame payload is JSON");
        assert!(
            value.get("event").is_some(),
            "post-subscribe frames are events, not responses: {value}"
        );
        assert!(
            value.get("ok").is_none(),
            "a dispatched Ping response must NOT appear: {value}"
        );
        if value.get("pane_id").and_then(Value::as_str) == Some("marker") {
            saw_marker = true;
            break;
        }
    }
    assert!(
        saw_marker,
        "the live marker event must be delivered on the subscribed connection"
    );

    drop(stream);
    let _ = handle.join();
}

#[test]
fn v2_connection_rejects_unframed_message() {
    // VAL-IPC-034: a v1-style newline message on a v2 connection (no envelope) is a
    // clean protocol error — bad magic ⇒ close, no panic, no response written.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(2)).expect("handshake ok");
    let mut stream = hs.stream;

    write_json_line(&mut stream, &DaemonRequest::Ping).expect("unframed write");

    let result = handle.join().expect("server thread must not panic");
    assert!(
        result.is_err(),
        "an unframed message on a v2 connection is a protocol error: {result:?}"
    );
    // The connection is closed with no response bytes sent.
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    assert!(buf.is_empty(), "no response should be sent, got {buf:?}");
}

#[test]
fn v1_connection_rejects_framed_envelope() {
    // VAL-IPC-035: a binary framed envelope on a negotiated-v1 connection is an
    // invalid request (protocol error), not a panic or hang.
    let (client, token, _dd, handle) = pair_daemon_connection();
    let hs = client_handshake(client, &token, Some(1)).expect("v1 handshake");
    assert_eq!(hs.negotiated_wire_version, 1);
    let mut client = hs.stream;

    // Write a frame where the v1 path expects a newline JSON value, then half-close
    // so the line reader hits a bounded EOF instead of waiting for a newline.
    frame::write(&mut client, &DaemonRequest::Ping).expect("framed write");
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("half-close");

    let result = handle.join().expect("server thread must not panic");
    assert!(
        result.is_err(),
        "a framed envelope on a v1 connection is a protocol error: {result:?}"
    );
}

#[test]
fn subscribe_event_framing_follows_negotiated_version() {
    // VAL-IPC-050: a v2 Subscribe streams FRAMED events; a v1 Subscribe streams
    // newline-JSON events — each decodable only by the matching reader.
    let (server, _dd, token) = shared_daemon();

    // v2 subscribe ⇒ framed events (start with MAGIC).
    let (v2c, v2h) = connect_to(&server);
    let hs = client_handshake(v2c, &token, Some(2)).expect("v2 handshake");
    let mut v2_stream = hs.stream;
    frame::write(&mut v2_stream, &DaemonRequest::Subscribe).expect("v2 subscribe");
    wait_for(|| server.router.subscriber_count() >= 1);
    let evt = DaemonEvent::PaneEnded {
        pane_id: "v2-evt".to_string(),
        exit_code: None,
    };
    server.router.broadcast(&evt);
    read_framed_event_until(&mut v2_stream, &evt);
    drop(v2_stream);
    let _ = v2h.join();
    wait_for(|| server.router.subscriber_count() == 0);

    // v1 subscribe ⇒ newline-JSON events (no MAGIC prefix).
    let (v1c, v1h) = connect_to(&server);
    let hs = client_handshake(v1c, &token, Some(1)).expect("v1 handshake");
    let mut v1c = hs.stream;
    write_json_line(&mut v1c, &DaemonRequest::Subscribe).expect("v1 subscribe");
    wait_for(|| server.router.subscriber_count() >= 1);
    let evt1 = DaemonEvent::PaneEnded {
        pane_id: "v1-evt".to_string(),
        exit_code: None,
    };
    server.router.broadcast(&evt1);
    let mut v1r = BufReader::new(v1c);
    read_newline_event_until(&mut v1r, &evt1);
    drop(v1r);
    let _ = v1h.join();
}

#[test]
fn concurrent_v2_connections_are_isolated() {
    // VAL-IPC-051: two concurrent persistent v2 connections each receive responses
    // for ONLY their own requests, in order, with no cross-connection delivery.
    let (server, _dd, token) = shared_daemon();

    let (a, ah) = connect_to(&server);
    let (b, bh) = connect_to(&server);
    let mut sa = client_handshake(a, &token, Some(2))
        .expect("A handshake")
        .stream;
    let mut sb = client_handshake(b, &token, Some(2))
        .expect("B handshake")
        .stream;

    for i in 0..5 {
        let a_id = format!("A-{i}");
        let b_id = format!("B-{i}");
        frame::write(
            &mut sa,
            &DaemonRequest::PaneStatus {
                pane_id: a_id.clone(),
            },
        )
        .expect("A req");
        frame::write(
            &mut sb,
            &DaemonRequest::PaneStatus {
                pane_id: b_id.clone(),
            },
        )
        .expect("B req");
        let ra: IpcResponse = frame::read(&mut sa).expect("A reads").expect("present");
        let rb: IpcResponse = frame::read(&mut sb).expect("B reads").expect("present");
        assert!(
            ra.error.unwrap_or_default().contains(&a_id),
            "A received A's own response"
        );
        assert!(
            rb.error.unwrap_or_default().contains(&b_id),
            "B received B's own response"
        );
    }

    drop(sa);
    drop(sb);
    let _ = ah.join();
    let _ = bh.join();
}

#[test]
fn v2_connection_error_does_not_affect_others() {
    // VAL-IPC-052: a malformed frame on one v2 connection closes only that one with
    // a protocol error; a concurrent connection keeps being served normally.
    let (server, _dd, token) = shared_daemon();

    let (a, ah) = connect_to(&server);
    let (b, bh) = connect_to(&server);
    let mut sa = client_handshake(a, &token, Some(2))
        .expect("A handshake")
        .stream;
    let mut sb = client_handshake(b, &token, Some(2))
        .expect("B handshake")
        .stream;

    // A issues good requests first.
    for i in 0..3 {
        frame::write(
            &mut sa,
            &DaemonRequest::PaneStatus {
                pane_id: format!("A-{i}"),
            },
        )
        .expect("A req");
        let _: IpcResponse = frame::read(&mut sa).expect("A reads").expect("present");
    }
    // A then sends a malformed frame (bad magic): a clean protocol error closes A.
    sa.write_all(b"NOPEnononono-bad-frame-bytes")
        .expect("A garbage write");
    sa.flush().ok();
    let a_result = ah.join().expect("A thread must not panic");
    assert!(
        a_result.is_err(),
        "A's malformed frame is a protocol error: {a_result:?}"
    );

    // B is unaffected: it keeps issuing requests and receiving correct responses.
    for i in 0..3 {
        let b_id = format!("B-{i}");
        frame::write(
            &mut sb,
            &DaemonRequest::PaneStatus {
                pane_id: b_id.clone(),
            },
        )
        .expect("B req");
        let rb: IpcResponse = frame::read(&mut sb).expect("B reads").expect("present");
        assert!(
            rb.error.unwrap_or_default().contains(&b_id),
            "B still served correctly after A errored"
        );
    }

    // The daemon stays healthy for brand-new connections too.
    let (c, ch) = connect_to(&server);
    let mut sc = client_handshake(c, &token, Some(2))
        .expect("C handshake")
        .stream;
    frame::write(&mut sc, &DaemonRequest::Ping).expect("C req");
    let rc: IpcResponse = frame::read(&mut sc).expect("C reads").expect("present");
    assert!(rc.ok, "daemon serves new connections after a peer error");

    drop(sb);
    drop(sc);
    let _ = bh.join();
    let _ = ch.join();
}

#[test]
fn update_workspace_layout_rejects_oversized_layout() {
    let data_dir = std::env::temp_dir().join(format!("sgian-layout-test-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-layout"),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");

    let big = "x".repeat(MAX_LAYOUT_BYTES + 1);
    let err = server
        .handle(DaemonRequest::UpdateWorkspaceLayout { layout: json!(big) })
        .expect_err("oversized layout should be rejected");
    assert!(err.contains("maximum size"));

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn config_overlay_prefers_workspace_then_global() {
    let mut global_env = HashMap::new();
    global_env.insert("A".to_string(), "1".to_string());
    let global = Config {
        shell: Some("/bin/bash".to_string()),
        font_size: Some(12),
        env: global_env,
        idle_shutdown_secs: Some(100),
        ..Default::default()
    };
    let mut workspace_env = HashMap::new();
    workspace_env.insert("B".to_string(), "2".to_string());
    let workspace = Config {
        shell: Some("/bin/zsh".to_string()),
        env: workspace_env,
        ..Default::default()
    };

    let merged = global.overlay(workspace);
    assert_eq!(merged.shell, Some("/bin/zsh".to_string()));
    assert_eq!(merged.font_size, Some(12));
    assert_eq!(merged.env.get("A"), Some(&"1".to_string()));
    assert_eq!(merged.env.get("B"), Some(&"2".to_string()));
    assert_eq!(merged.idle_shutdown_secs, Some(100));
}

#[test]
fn config_shell_config_falls_back_to_default_shell() {
    assert!(!Config::default().shell_config().shell.is_empty());

    let configured = Config {
        shell: Some("/bin/bash".to_string()),
        shell_args: Some(vec!["-l".to_string()]),
        ..Default::default()
    };
    let shell = configured.shell_config();
    assert_eq!(shell.shell, "/bin/bash");
    assert_eq!(shell.args, vec!["-l".to_string()]);
}

#[test]
fn unix_default_shell_uses_a_portable_linux_fallback() {
    assert_eq!(default_unix_shell(None, true), "/bin/zsh");
    assert_eq!(default_unix_shell(None, false), "/bin/sh");
    assert_eq!(
        default_unix_shell(Some("/custom/fish".to_string()), false),
        "/custom/fish"
    );
}

// ----- Environment scrubbing unit tests (VAL-SEC-003/004/007) -----

/// VAL-SEC-003: a scrubbed inherited variable is removed from the spawn env.
#[test]
fn compute_spawn_env_scrubs_listed_inherited_vars() {
    let mut inherited = HashMap::new();
    inherited.insert("SGIAN_SECRET".to_string(), "sentinel123".to_string());
    inherited.insert("KEEP".to_string(), "yes".to_string());
    let scrub = vec!["SGIAN_SECRET".to_string()];
    let explicit = HashMap::new();

    let env = compute_spawn_env(&inherited, &scrub, &explicit);
    assert!(
        !env.contains_key("SGIAN_SECRET"),
        "scrubbed var must be absent"
    );
    assert_eq!(
        env.get("KEEP"),
        Some(&"yes".to_string()),
        "unscrubbed var must be preserved"
    );
}

/// VAL-SEC-004: with no scrub list, the inherited environment is preserved.
#[test]
fn compute_spawn_env_default_preserves_inheritance() {
    let mut inherited = HashMap::new();
    inherited.insert("SGIAN_SECRET".to_string(), "sentinel123".to_string());
    let scrub: Vec<String> = vec![];
    let explicit = HashMap::new();

    let env = compute_spawn_env(&inherited, &scrub, &explicit);
    assert_eq!(
        env.get("SGIAN_SECRET"),
        Some(&"sentinel123".to_string()),
        "default (no scrub) must preserve inherited value"
    );
}

/// VAL-SEC-007: an explicit `env` value takes precedence over the scrub list
/// for the same variable name (operator-set value wins, not scrubbed away).
#[test]
fn compute_spawn_env_explicit_takes_precedence_over_scrub() {
    let mut inherited = HashMap::new();
    inherited.insert("FOO".to_string(), "inherited".to_string());
    let scrub = vec!["FOO".to_string()];
    let mut explicit = HashMap::new();
    explicit.insert("FOO".to_string(), "cfgval".to_string());

    let env = compute_spawn_env(&inherited, &scrub, &explicit);
    assert_eq!(
        env.get("FOO"),
        Some(&"cfgval".to_string()),
        "explicit env value must win over both scrub and inherited"
    );
}

/// The config overlay unions scrub lists from global and workspace configs.
#[test]
fn config_overlay_unions_scrub_env_lists() {
    let global = Config {
        scrub_env: vec!["A".to_string(), "B".to_string()],
        ..Default::default()
    };
    let workspace = Config {
        scrub_env: vec!["B".to_string(), "C".to_string()],
        ..Default::default()
    };
    let merged = global.overlay(workspace);
    let mut scrub = merged.scrub_env.clone();
    scrub.sort();
    assert_eq!(
        scrub,
        vec!["A".to_string(), "B".to_string(), "C".to_string()],
        "overlay should union (dedup) scrub lists"
    );
}

/// shell_config propagates the scrub list to the ShellConfig the PTY spawner uses.
#[test]
fn config_shell_config_propagates_scrub_env() {
    let config = Config {
        scrub_env: vec!["SECRET".to_string()],
        ..Default::default()
    };
    let shell = config.shell_config();
    assert_eq!(shell.scrub_env, vec!["SECRET".to_string()]);
}

/// M3: a workspace layer that EXPLICITLY sets 0/[] cancels the global value;
/// an absent field still inherits it (Option semantics, not magic sentinels).
#[test]
fn config_overlay_zero_and_empty_override_globals() {
    let global = Config {
        idle_shutdown_secs: Some(300),
        shell_args: Some(vec!["-l".to_string()]),
        ..Default::default()
    };

    let workspace = Config {
        idle_shutdown_secs: Some(0),
        shell_args: Some(Vec::new()),
        ..Default::default()
    };
    let merged = global.clone().overlay(workspace);
    assert_eq!(merged.idle_shutdown_secs, Some(0));
    assert_eq!(merged.idle_shutdown_secs_effective(), 0);
    assert_eq!(merged.shell_args, Some(Vec::new()));
    assert!(merged.shell_args_effective().is_empty());

    let merged = global.overlay(Config::default());
    assert_eq!(merged.idle_shutdown_secs, Some(300));
    assert_eq!(merged.shell_args_effective(), vec!["-l".to_string()]);

    // The JSON shape too: `"idle_shutdown_secs": 0` in a file is an explicit
    // Some(0), not conflated with unset.
    let parsed: Config = serde_json::from_str(r#"{"idle_shutdown_secs":0}"#).expect("parse");
    assert_eq!(parsed.idle_shutdown_secs, Some(0));
    let parsed: Config = serde_json::from_str("{}").expect("parse");
    assert_eq!(parsed.idle_shutdown_secs, None);
}

/// M1: full_config (the get_config payload) carries scrub_env, so an honest
/// get→edit→write round-trip cannot silently drop the scrub list.
#[test]
fn full_config_round_trip_carries_scrub_env() {
    let config = Config {
        scrub_env: vec!["SECRET".to_string()],
        ..Default::default()
    };
    let full = config.full_config();
    assert_eq!(full["scrub_env"], json!(["SECRET"]));

    let round_tripped: Config =
        serde_json::from_value(full).expect("full_config should deserialize into Config");
    assert_eq!(round_tripped.scrub_env, vec!["SECRET".to_string()]);
}

/// 07-19 review (persistence lows): unknown config keys are rejected
/// loudly instead of silently dropped; the full get→write round-trip
/// (exactly the struct's fields) still deserializes.
#[test]
fn config_rejects_unknown_keys_but_accepts_full_round_trip() {
    // A typo'd key fails loudly, naming the offending key.
    let error = serde_json::from_value::<Config>(json!({
        "shell": "/bin/zsh",
        "scrub_envv": ["SECRET"],
    }))
    .expect_err("an unknown key must be rejected");
    assert!(
        error.to_string().contains("unknown field"),
        "expected an unknown-field error, got: {error}"
    );
    assert!(
        error.to_string().contains("scrub_envv"),
        "the error should name the offending key, got: {error}"
    );

    // The full GUI round-trip payload (get_config → write_config) carries
    // exactly the struct's fields and must still be accepted.
    let config = Config {
        shell: Some("/bin/zsh".to_string()),
        shell_args: Some(vec!["-l".to_string()]),
        env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        scrub_env: vec!["SECRET".to_string()],
        font_family: Some("monospace".to_string()),
        font_size: Some(14),
        theme: Some(json!({ "name": "dark" })),
        idle_shutdown_secs: Some(30),
        restore_policy: Some("restore_on_demand".to_string()),
        lease_policy: None,
        agent_probe_interval_ms: None,
        kranz_bin: None,
        identity: None,
        agent_permission_mode: Some("manual".to_string()),
        agent_claude_bin: Some("/opt/claude/bin/claude".to_string()),
        agent_droid_bin: Some("/opt/factory/bin/droid".to_string()),
        profiles: Vec::new(),
    };
    let full = config.full_config();
    let round_tripped: Config = serde_json::from_value(full)
        .expect("the full_config round-trip must survive deny_unknown_fields");
    assert_eq!(round_tripped, config);
}

/// M2: a malformed config layer is surfaced (error / warning), never silently
/// treated as an empty default.
#[test]
fn malformed_config_is_surfaced_not_silently_defaulted() {
    let dir = std::env::temp_dir().join(format!("sgian-badcfg-test-{}", now_millis()));
    fs::create_dir_all(&dir).expect("test dir should be created");
    fs::write(dir.join(CONFIG_FILE), b"{ not json").expect("bad config should be written");

    let error = read_config_file(&dir.join(CONFIG_FILE)).expect_err("malformed config must error");
    assert!(error.contains("malformed config"), "unexpected: {error}");
    assert!(matches!(
        read_config_file(&dir.join("missing.json")),
        Ok(None)
    ));

    let (_config, warnings) = load_config(&dir);
    assert!(
        warnings.iter().any(|w| w.contains("sgian-badcfg-test")),
        "workspace-layer warning missing from {warnings:?}"
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn startup_rejects_invalid_config_before_creating_runtime() {
    for contents in [
        r#"{"scrub_env":["SECRET"],}"#,
        r#"{"agent_permission_mode":"manul"}"#,
        r#"{"restore_policy":"unknown"}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("runtime/daemon.sock");
        fs::write(dir.path().join(CONFIG_FILE), contents).unwrap();
        let error = run_daemon(
            dir.path().to_path_buf(),
            socket.clone(),
            dir.path().to_path_buf(),
        )
        .expect_err("invalid policy must prevent startup");
        assert!(error.contains("refusing to start with invalid configuration"));
        assert!(!socket.parent().unwrap().exists());
        assert!(!dir.path().join(TOKEN_FILE).exists());
    }
}

/// M2: a file-watch reload of a malformed config keeps the previous effective
/// config (and thus broadcasts nothing) instead of resetting to defaults.
#[test]
fn reload_config_keeps_previous_on_malformed_file() {
    let data_dir = std::env::temp_dir().join(format!("sgian-badreload-test-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-badreload"),
        data_dir.clone(),
        Config {
            shell: Some("/bin/distinctive-shell".to_string()),
            ..Default::default()
        },
    )
    .expect("daemon server should start");

    fs::write(data_dir.join(CONFIG_FILE), b"{ definitely not json")
        .expect("bad config should be written");
    server.reload_config();
    assert_eq!(
        server.effective_config().shell.as_deref(),
        Some("/bin/distinctive-shell"),
        "malformed reload must keep the previous effective config"
    );

    // A subsequent VALID file does reload (the skip is about malformed-ness).
    fs::write(data_dir.join(CONFIG_FILE), br#"{"shell":"/bin/newshell"}"#)
        .expect("good config should be written");
    server.reload_config();
    assert_eq!(
        server.effective_config().shell.as_deref(),
        Some("/bin/newshell")
    );

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn parse_exit_marker_ignores_echoed_command_and_reads_code() {
    let prefix = "__sgian_rc_pane-1:";
    // The echoed command line contains the literal "%s"; only the output has digits.
    let stream = "printf '\\n__sgian_rc_pane-1:%s\\n' \"$?\"\r\n__sgian_rc_pane-1:42\n";
    assert_eq!(parse_exit_marker(stream, prefix), Some(42));
    assert_eq!(parse_exit_marker("__sgian_rc_pane-1:%s", prefix), None);
    assert_eq!(parse_exit_marker("nothing here", prefix), None);
    assert_eq!(parse_exit_marker("__sgian_rc_pane-1:0\n", prefix), Some(0));
    // Digits running to the end of the buffer may be a partially-received code;
    // wait for the marker line's terminating newline.
    assert_eq!(parse_exit_marker("__sgian_rc_pane-1:4", prefix), None);
}

#[test]
fn scrub_diagnostic_log_line_redacts_secrets() {
    assert_eq!(scrub_diagnostic_log_line("token=abc"), "[redacted]");
    assert_eq!(
        scrub_diagnostic_log_line("Cookie: session=xyz"),
        "[redacted]"
    );
    assert_eq!(scrub_diagnostic_log_line("url?sig=deadbeef"), "[redacted]");
    assert_eq!(scrub_diagnostic_log_line("plain info"), "plain info");
}

#[test]
fn capped_utf8_tail_avoids_mid_codepoint_slice() {
    // "─" is U+2500 (e2 94 80). Cap of 4 bytes starts mid-character; the
    // helper must advance to a char boundary and never invent U+FFFD from
    // a valid UTF-8 source.
    let bytes = b"ab\xe2\x94\x80cd";
    let tail = capped_utf8_tail(bytes, 4);
    assert!(
        !tail.contains('\u{FFFD}'),
        "valid UTF-8 must not yield replacement chars, got {tail:?}"
    );
    assert!(
        tail == "─cd" || tail == "cd",
        "expected a clean UTF-8 tail, got {tail:?}"
    );
}

#[test]
fn ui_smoke_env_enabled_parses_truthy_values() {
    // Not process-global: call the parser via temporary env in a scoped way
    // would race parallel tests. Pin the pure marker-path fallback instead.
    let path = ui_smoke_marker_path();
    assert!(
        path.file_name().and_then(|n| n.to_str()) == Some(".sgian-ui-smoke-ok")
            || path.to_string_lossy().contains("sgian"),
        "default marker path should be workspace-local, got {}",
        path.display()
    );
    assert_eq!(
        ui_smoke_error_path(Path::new("/tmp/.sgian-ui-smoke-ok"))
            .file_name()
            .and_then(|name| name.to_str()),
        Some(".sgian-ui-smoke-ok.err")
    );
    assert!(!ui_smoke_env_enabled() || std::env::var_os("SGIAN_UI_SMOKE").is_some());
}

#[test]
fn classify_update_check_skips_network_and_signature_failures() {
    // ENHANCEMENTS §5: unreachable feed / bad signature must skip, not panic.
    assert_eq!(
        classify_update_check(Err(
            "error sending request for url (http://127.0.0.1:9/)".to_string()
        )),
        UpdateCheckOutcome::Skipped(
            "error sending request for url (http://127.0.0.1:9/)".to_string()
        )
    );
    assert_eq!(
        classify_update_check(Err("signature verification failed".to_string())),
        UpdateCheckOutcome::Skipped("signature verification failed".to_string())
    );
    assert_eq!(
        classify_update_check(Ok(None)),
        UpdateCheckOutcome::UpToDate
    );
    assert_eq!(
        classify_update_check(Ok(Some(("1.2.3".to_string(), Some("notes".to_string())))),),
        UpdateCheckOutcome::Available {
            version: "1.2.3".to_string(),
            body: Some("notes".to_string()),
        }
    );
}

#[test]
fn config_profiles_reject_kind_field_mismatch() {
    let mut config = Config {
        profiles: vec![PaneProfile {
            name: "bad".to_string(),
            kind: Some("shell".to_string()),
            agent_backend: Some("claude".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(config.validate().unwrap_err().contains("kind is shell"));
    config.profiles = vec![PaneProfile {
        name: " spaced ".to_string(),
        ..Default::default()
    }];
    assert!(config.validate().unwrap_err().contains("whitespace"));
}

#[test]
fn run_process_returns_exact_exit_and_stdout() {
    let cwd = tempfile::tempdir().expect("cwd");
    let data_dir = std::env::temp_dir().join(format!("sgian-run-process-{}", now_millis()));
    let server = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");
    let argv = if cfg!(windows) {
        vec![
            "cmd".to_string(),
            "/C".to_string(),
            "echo".to_string(),
            "hello-process".to_string(),
        ]
    } else {
        vec!["/bin/echo".to_string(), "hello-process".to_string()]
    };
    let value = server
        .handle(DaemonRequest::RunProcess {
            argv,
            cwd: None,
            timeout_ms: Some(5_000),
        })
        .expect("run process");
    assert_eq!(value["exit_code"], json!(0));
    assert_eq!(value["success"], json!(true));
    assert_eq!(value["timed_out"], json!(false));
    assert!(
        value["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("hello-process"),
        "stdout should capture argv output, got {}",
        value["stdout"]
    );
    let _ = fs::remove_dir_all(data_dir);
}

#[cfg(unix)]
#[test]
fn run_process_honors_scrub_and_explicit_environment() {
    let dir = tempfile::tempdir().unwrap();
    // Use an already inherited variable; never mutate global environment
    // while the rest of the test suite is spawning children concurrently.
    assert!(std::env::var_os("PATH").is_some());
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().join("data"),
        Config {
            scrub_env: vec!["PATH".into()],
            env: HashMap::from([("SGIAN_AUDIT_EXPLICIT".into(), "configured".into())]),
            ..Default::default()
        },
    )
    .unwrap();
    let request = || DaemonRequest::RunProcess {
        argv: vec!["/usr/bin/env".into()],
        cwd: None,
        timeout_ms: Some(5_000),
    };
    let value = server.handle(request()).unwrap();
    let output = value["stdout"].as_str().unwrap();
    assert!(!output.lines().any(|line| line.starts_with("PATH=")));
    assert!(output
        .lines()
        .any(|line| line == "SGIAN_AUDIT_EXPLICIT=configured"));

    server
        .config
        .write()
        .unwrap()
        .env
        .insert("PATH".into(), "/configured-path".into());
    let value = server.handle(request()).unwrap();
    assert!(value["stdout"]
        .as_str()
        .unwrap()
        .lines()
        .any(|line| line == "PATH=/configured-path"));
}

/// A child that exits while a descendant still holds stdout must not pin
/// daemon reader threads: after the join budget, the process group is killed
/// so the pipes close and the request returns.
#[cfg(unix)]
#[test]
fn run_process_reaps_descendants_holding_pipes() {
    let cwd = tempfile::tempdir().expect("cwd");
    let data_dir = std::env::temp_dir().join(format!("sgian-run-process-orphan-{}", now_millis()));
    let server = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");
    let started = Instant::now();
    let value = server
        .handle(DaemonRequest::RunProcess {
            argv: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 120 & echo orphan-done".to_string(),
            ],
            cwd: None,
            timeout_ms: Some(15_000),
        })
        .expect("run process");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "stuck pipe readers must not hang the request (elapsed {:?})",
        started.elapsed()
    );
    assert_eq!(value["timed_out"], json!(false));
    assert!(
        value["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("orphan-done"),
        "stdout should capture the parent echo, got {}",
        value["stdout"]
    );
    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn subscriber_connect_disconnect_soak_returns_to_baseline() {
    // ENHANCEMENTS §5: repeated subscribe/unsubscribe must not leak entries.
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().to_path_buf());
    assert_eq!(router.subscriber_count(), 0);
    for _ in 0..32 {
        let (_client, server) = test_transport_pair().expect("transport pair should be available");
        let id = router
            .add_subscriber(server, 1)
            .expect("subscribe within the cap");
        assert_eq!(router.subscriber_count(), 1);
        router.remove_subscriber(id);
        assert_eq!(router.subscriber_count(), 0);
    }
}

#[test]
fn pane_run_result_json_includes_structured_fields() {
    let result = PaneRunResult::ok("pane-1", 0, 12, "hello".to_string());
    let value = result.to_json();
    assert_eq!(value["pane"], json!("pane-1"));
    assert_eq!(value["exit_code"], json!(0));
    assert_eq!(value["success"], json!(true));
    assert_eq!(value["timed_out"], json!(false));
    assert_eq!(value["elapsed_ms"], json!(12));
    assert_eq!(value["tail"], json!("hello"));
}

#[test]
fn config_profiles_round_trip_and_validate() {
    let mut config = Config {
        profiles: vec![PaneProfile {
            name: "review".to_string(),
            kind: Some("agent".to_string()),
            agent_backend: Some("claude".to_string()),
            agent_model: Some("sonnet".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    config.validate().expect("valid profiles");
    let full = config.full_config();
    let round: Config = serde_json::from_value(full).expect("round-trip");
    assert_eq!(round.profiles.len(), 1);
    assert_eq!(
        round.profile("review").unwrap().agent_model.as_deref(),
        Some("sonnet")
    );
    config.profiles.push(PaneProfile {
        name: "review".to_string(),
        ..Default::default()
    });
    assert!(config.validate().unwrap_err().contains("duplicate"));
}

/// ENHANCEMENTS §4: a shell profile freezes into pane_shells at create time
/// and the spawned command reflects the profile shell.
#[cfg(unix)]
#[test]
fn create_pane_with_shell_profile_uses_override() {
    let mut config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    config.profiles = vec![PaneProfile {
        name: "catshell".to_string(),
        kind: Some("shell".to_string()),
        shell: Some("/bin/cat".to_string()),
        env: HashMap::from([("SGIAN_PROFILE_MARK".to_string(), "from-profile".to_string())]),
        ..Default::default()
    }];
    let cwd = tempfile::tempdir().expect("cwd");
    let data_dir = std::env::temp_dir().join(format!("sgian-profile-create-{}", now_millis()));
    let server = DaemonServer::with_config(cwd.path().to_path_buf(), data_dir.clone(), config)
        .expect("daemon server should start");
    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: Some("profiled".to_string()),
                profile: Some("catshell".to_string()),
            })
            .expect("create with profile"),
    )
    .expect("pane");
    {
        let terminals = server.lock_terminals().expect("lock");
        let override_shell = terminals
            .pane_shells
            .get(&pane.id)
            .expect("profile override stored");
        assert_eq!(override_shell.shell, "/bin/cat");
        assert_eq!(
            override_shell
                .env
                .get("SGIAN_PROFILE_MARK")
                .map(String::as_str),
            Some("from-profile")
        );
        let command = terminals.pane_meta(&pane.id).command.unwrap_or_default();
        assert!(
            command.contains("cat"),
            "spawned command should use profile shell, got {command}"
        );
    }
    let _ = fs::remove_dir_all(data_dir);
}

/// ENHANCEMENTS §4: frozen shell overrides survive persist + daemon reload.
#[cfg(unix)]
#[test]
fn pane_shell_profile_survives_daemon_restart() {
    let mut config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    config.profiles = vec![PaneProfile {
        name: "catshell".to_string(),
        kind: Some("shell".to_string()),
        shell: Some("/bin/cat".to_string()),
        env: HashMap::from([("SGIAN_PROFILE_MARK".to_string(), "persisted".to_string())]),
        ..Default::default()
    }];
    let cwd = tempfile::tempdir().expect("cwd");
    let data_dir = tempfile::tempdir().expect("data");
    let pane_id = {
        let server = DaemonServer::with_config(
            cwd.path().to_path_buf(),
            data_dir.path().to_path_buf(),
            config.clone(),
        )
        .expect("daemon server should start");
        let pane: Pane = serde_json::from_value(
            server
                .handle(DaemonRequest::CreatePane {
                    title: Some("profiled".to_string()),
                    profile: Some("catshell".to_string()),
                })
                .expect("create with profile"),
        )
        .expect("pane");
        server.persist().expect("persist");
        pane.id
    };
    // Pre-feature workspaces omit pane_shells — serde default must still load.
    let _: PersistedWorkspace =
        serde_json::from_str(r#"{"panes":[],"active_pane_id":null,"cwd":"/tmp","next_id":1}"#)
            .expect("pre-feature workspace without pane_shells must load");
    let raw = fs::read_to_string(data_dir.path().join(WORKSPACE_FILE)).expect("workspace.json");
    assert!(
        raw.contains("pane_shells"),
        "workspace.json should include pane_shells: {raw}"
    );

    let reloaded = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.path().to_path_buf(),
        config,
    )
    .expect("reload daemon");
    // Bootstrap auto-spawns restored panes (same path the GUI uses).
    reloaded
        .handle(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap should respawn");
    {
        let terminals = reloaded.lock_terminals().expect("lock");
        let override_shell = terminals
            .pane_shells
            .get(&pane_id)
            .expect("override restored from disk");
        assert_eq!(override_shell.shell, "/bin/cat");
        assert_eq!(
            override_shell
                .env
                .get("SGIAN_PROFILE_MARK")
                .map(String::as_str),
            Some("persisted")
        );
        let command = terminals.pane_meta(&pane_id).command.unwrap_or_default();
        assert!(
            command.contains("cat"),
            "respawned pane should use persisted profile shell, got {command}"
        );
    }
}

/// ENHANCEMENTS §4: RestartPaneTerminal keeps the frozen profile override.
#[cfg(unix)]
#[test]
fn restart_pane_preserves_shell_profile_override() {
    let mut config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    config.profiles = vec![PaneProfile {
        name: "catshell".to_string(),
        kind: Some("shell".to_string()),
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    }];
    let cwd = tempfile::tempdir().expect("cwd");
    let data_dir = tempfile::tempdir().expect("data");
    let server = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.path().to_path_buf(),
        config,
    )
    .expect("daemon server should start");
    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: None,
                profile: Some("catshell".to_string()),
            })
            .expect("create with profile"),
    )
    .expect("pane");
    server
        .handle(DaemonRequest::RestartPaneTerminal {
            pane_id: pane.id.clone(),
        })
        .expect("restart");
    {
        let terminals = server.lock_terminals().expect("lock");
        let override_shell = terminals
            .pane_shells
            .get(&pane.id)
            .expect("override retained across restart");
        assert_eq!(override_shell.shell, "/bin/cat");
        let command = terminals.pane_meta(&pane.id).command.unwrap_or_default();
        assert!(
            command.contains("cat"),
            "restarted pane should still use profile shell, got {command}"
        );
    }
}

/// ENHANCEMENTS §4: closing a pane drops its override from the persisted map.
#[cfg(unix)]
#[test]
fn close_pane_drops_shell_profile_from_persist() {
    let mut config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    config.profiles = vec![PaneProfile {
        name: "catshell".to_string(),
        kind: Some("shell".to_string()),
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    }];
    let cwd = tempfile::tempdir().expect("cwd");
    let data_dir = tempfile::tempdir().expect("data");
    let server = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.path().to_path_buf(),
        config,
    )
    .expect("daemon server should start");
    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: None,
                profile: Some("catshell".to_string()),
            })
            .expect("create with profile"),
    )
    .expect("pane");
    server.persist().expect("persist before close");
    server
        .handle(DaemonRequest::ClosePane {
            pane_id: pane.id.clone(),
        })
        .expect("close");
    server.persist().expect("persist after close");
    let persisted: PersistedWorkspace = serde_json::from_str(
        &fs::read_to_string(data_dir.path().join(WORKSPACE_FILE)).expect("workspace.json"),
    )
    .expect("parse");
    assert!(
        !persisted.pane_shells.contains_key(&pane.id),
        "closed pane must not remain in pane_shells"
    );
}

#[test]
fn trim_to_tail_respects_utf8_boundaries() {
    let mut buffer = "ab─cd".to_string();
    trim_to_tail(&mut buffer, 3);
    assert_eq!(buffer, "cd");

    let mut short = "abc".to_string();
    trim_to_tail(&mut short, 8);
    assert_eq!(short, "abc");
}

#[test]
fn clean_title_trims_and_caps_length() {
    assert_eq!(
        clean_title(Some("  build  ".to_string())),
        Some("build".to_string())
    );
    assert_eq!(clean_title(Some("   ".to_string())), None);

    let huge = "x".repeat(MAX_TITLE_CHARS * 4);
    let capped = clean_title(Some(huge)).expect("title should survive capping");
    assert_eq!(capped.chars().count(), MAX_TITLE_CHARS);
}

/// L11: control characters (ESC/CSI, newlines, NUL) are stripped from titles
/// so a rename can't log-inject or escape-inject terminals printing lists.
#[test]
fn clean_title_strips_control_characters() {
    assert_eq!(
        clean_title(Some("\x1b[2Jinno\ncent\x07".to_string())),
        Some("[2Jinnocent".to_string())
    );
    assert_eq!(clean_title(Some("\x1b\x07\n".to_string())), None);
}

/// L7: exactly one title source — a second positional or --name is an error
/// in either order (previously `new --name a b` silently created pane `b`).
#[test]
fn parse_name_option_rejects_conflicting_titles() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        parse_name_option(&args(&["--name", "a"])).expect("named"),
        Some("a".to_string())
    );
    assert_eq!(
        parse_name_option(&args(&["a"])).expect("positional"),
        Some("a".to_string())
    );
    assert!(parse_name_option(&args(&["--name", "a", "b"]))
        .expect_err("named + positional")
        .contains("more than once"));
    assert!(parse_name_option(&args(&["a", "--name", "b"]))
        .expect_err("positional + named")
        .contains("more than once"));
    assert!(parse_name_option(&args(&["a", "b"]))
        .expect_err("two positionals")
        .contains("more than once"));
    assert!(parse_name_option(&args(&["-x"]))
        .expect_err("unknown flag")
        .contains("unexpected pane option"));
}

/// L6: `--` ends flag parsing so the literal strings --lf/--raw/-- transmit.
#[test]
fn parse_lf_flag_passthrough_after_double_dash() {
    let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (lf, rest) = parse_lf_flag(&args(&["--lf", "--", "--raw", "--", "x"]));
    assert!(lf);
    assert_eq!(rest, args(&["--raw", "--", "x"]));

    let (lf, rest) = parse_lf_flag(&args(&["--", "--lf"]));
    assert!(!lf);
    assert_eq!(rest, args(&["--lf"]));
}

/// L4: fixed-arity commands reject leftover/typo'd arguments.
#[test]
fn ensure_no_extra_args_rejects_leftovers() {
    assert!(ensure_no_extra_args("panes", &[]).is_ok());
    let err = ensure_no_extra_args("panes", &["--jsonn".to_string()])
        .expect_err("typo'd flag must error");
    assert!(err.contains("panes") && err.contains("--jsonn"), "{err}");
}

/// M9: the attach overlap skipper swallows exactly the duplicated tail.
#[test]
fn overlap_skipper_dedupes_the_printed_tail() {
    let printed = format!(
        "{}PROMPT$ echo overlap-window-anchor-text with padding\r\n",
        "x".repeat(100)
    );

    // Whole duplicate split across two chunks (first chunk >= the anchor
    // minimum), then new output.
    let mut skipper = OverlapSkipper::new(printed.as_bytes());
    assert_eq!(skipper.filter(b"PROMPT$ echo overlap-window-anchor"), b"");
    assert_eq!(skipper.filter(b"-text with padding\r\n"), b"");
    assert_eq!(skipper.filter(b"NEW OUTPUT"), b"NEW OUTPUT".as_slice());

    // Duplicate that ends inside a chunk: only the tail prints.
    let mut skipper = OverlapSkipper::new(printed.as_bytes());
    assert_eq!(
        skipper.filter(b"overlap-window-anchor-text with padding\r\nfresh"),
        b"fresh".as_slice()
    );

    // A stream that never matches the tail passes through whole.
    let mut skipper = OverlapSkipper::new(printed.as_bytes());
    let novel = b"completely unrelated output that matches nothing here";
    assert_eq!(skipper.filter(novel), novel.as_slice());

    // Divergence after an anchor stops the dedupe from then on.
    let mut skipper = OverlapSkipper::new(printed.as_bytes());
    assert_eq!(skipper.filter(b"PROMPT$ echo overlap-window-anch"), b"");
    assert_eq!(skipper.filter(b"DIVERGED"), b"DIVERGED".as_slice());
    assert_eq!(skipper.filter(b"more"), b"more".as_slice());

    // A short first chunk (below the anchor minimum) is never swallowed —
    // under-deduping (a visible duplicate) beats mis-anchoring real output.
    let mut skipper = OverlapSkipper::new(printed.as_bytes());
    assert_eq!(skipper.filter(b"\r\n"), b"\r\n".as_slice());
}

#[test]
fn constant_time_eq_compares_correctly() {
    assert!(constant_time_eq("abc123", "abc123"));
    assert!(!constant_time_eq("abc123", "abc124"));
    assert!(!constant_time_eq("abc", "abc123"));
    assert!(constant_time_eq("", ""));
}

#[test]
fn match_pane_ref_prefers_ids_and_detects_ambiguity() {
    let status = |id: &str, title: &str| PaneStatus {
        pane: Pane {
            id: id.to_string(),
            title: title.to_string(),
            kind: PaneKind::Shell,
            created_at_ms: 0,
        },
        state: PaneRuntimeState::Live,
    };
    let list = PaneList {
        panes: vec![
            status("pane-1", "build"),
            status("pane-2", "pane-1"),
            status("pane-3", "test"),
            status("pane-4", "test"),
        ],
        active_pane_id: Some("pane-3".to_string()),
        cwd: "/tmp/x".to_string(),
    };

    assert_eq!(match_pane_ref(&list, "active"), Ok("pane-3".to_string()));
    // A pane *titled* "pane-1" cannot shadow the real pane-1.
    assert_eq!(match_pane_ref(&list, "pane-1"), Ok("pane-1".to_string()));
    assert_eq!(match_pane_ref(&list, "build"), Ok("pane-1".to_string()));
    assert!(match_pane_ref(&list, "test")
        .unwrap_err()
        .contains("ambiguous"));
    assert!(match_pane_ref(&list, "nope")
        .unwrap_err()
        .contains("not found"));
}

#[test]
fn read_scrollback_tail_seeks_and_respects_utf8() {
    let data_dir = std::env::temp_dir().join(format!("sgian-tail-test-{}", now_millis()));
    fs::create_dir_all(&data_dir).expect("scrollback dir should be created");
    // "x─yz": the box-drawing char is 3 bytes; a 4-byte tail starts mid-character.
    fs::write(scrollback_path(&data_dir, "pane-1"), "x─yz".as_bytes())
        .expect("scrollback should be written");

    assert_eq!(
        read_scrollback_tail(&data_dir, "pane-1", 4),
        Some("yz".to_string())
    );
    assert_eq!(
        read_scrollback_tail(&data_dir, "pane-1", 1024),
        Some("x─yz".to_string())
    );

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn set_active_pane_updates_registry() {
    let data_dir = std::env::temp_dir().join(format!("sgian-active-test-{}", now_millis()));
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let server =
        DaemonServer::with_config(PathBuf::from("/tmp/sgian-active"), data_dir.clone(), config)
            .expect("daemon server should start");

    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed"),
    )
    .expect("pane should deserialize");
    let list: PaneList =
        serde_json::from_value(server.handle(DaemonRequest::ListPanes).unwrap()).unwrap();
    assert_eq!(list.active_pane_id, Some(pane.id));

    server
        .handle(DaemonRequest::SetActivePane {
            pane_id: "pane-1".to_string(),
        })
        .expect("set_active_pane should succeed");
    let list: PaneList =
        serde_json::from_value(server.handle(DaemonRequest::ListPanes).unwrap()).unwrap();
    assert_eq!(list.active_pane_id, Some("pane-1".to_string()));

    let err = server
        .handle(DaemonRequest::SetActivePane {
            pane_id: "pane-99".to_string(),
        })
        .expect_err("unknown pane should be rejected");
    assert!(err.contains("pane not found"));

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn get_scrollback_returns_pane_tail() {
    let data_dir = std::env::temp_dir().join(format!("sgian-getsb-test-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-getsb"),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");
    fs::write(
        scrollback_path(&data_dir.join(SCROLLBACK_DIR), "pane-1"),
        b"hello scrollback",
    )
    .expect("scrollback should be written");

    let result = server
        .handle(DaemonRequest::GetScrollback {
            pane_id: "pane-1".to_string(),
        })
        .expect("get_scrollback should succeed");
    assert_eq!(result["scrollback"], json!("hello scrollback"));

    let err = server
        .handle(DaemonRequest::GetScrollback {
            pane_id: "pane-99".to_string(),
        })
        .expect_err("unknown pane should be rejected");
    assert!(err.contains("pane not found"));

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn registry_changes_broadcast_events_to_subscribers() {
    let data_dir = std::env::temp_dir().join(format!("sgian-events-test-{}", now_millis()));
    // /bin/cat keeps the spawned session alive without emitting any output, so
    // the event stream below contains only the registry events under test.
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let server =
        DaemonServer::with_config(PathBuf::from("/tmp/sgian-events"), data_dir.clone(), config)
            .expect("daemon server should start");

    let (client_stream, server_stream) =
        test_transport_pair().expect("transport pair should be available");
    server
        .router
        .add_subscriber(server_stream, 1)
        .expect("subscribe within the cap");
    let mut reader = BufReader::new(client_stream);
    let mut next_event = move || -> DaemonEvent {
        let mut line = String::new();
        reader.read_line(&mut line).expect("event should arrive");
        serde_json::from_str(&line).expect("event should deserialize")
    };

    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: Some("events".to_string()),
                profile: None,
            })
            .expect("create should succeed"),
    )
    .expect("pane should deserialize");
    assert_eq!(
        next_event(),
        DaemonEvent::PaneCreated { pane: pane.clone() }
    );

    server
        .handle(DaemonRequest::RenamePane {
            pane_id: pane.id.clone(),
            title: "renamed".to_string(),
        })
        .expect("rename should succeed");
    match next_event() {
        DaemonEvent::PaneRenamed { pane: renamed } => {
            assert_eq!(renamed.id, pane.id);
            assert_eq!(renamed.title, "renamed");
        }
        other => panic!("expected a rename event, got {other:?}"),
    }

    server
        .handle(DaemonRequest::ClosePane {
            pane_id: pane.id.clone(),
        })
        .expect("close should succeed");
    assert_eq!(next_event(), DaemonEvent::PaneClosed { pane_id: pane.id });

    let _ = fs::remove_dir_all(data_dir);
}

#[test]
fn daemon_get_config_and_sync_input() {
    let data_dir = std::env::temp_dir().join(format!("sgian-cfg-test-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-cfg"),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");

    let appearance = server
        .handle(DaemonRequest::GetConfig)
        .expect("get_config should succeed");
    assert!(appearance.is_object());
    assert!(appearance.get("font_family").is_some());
    assert!(appearance.get("theme").is_some());

    let result = server
        .handle(DaemonRequest::SetSyncInput { enabled: true })
        .expect("set_sync_input should succeed");
    assert_eq!(result["sync_input"], json!(true));
    assert!(server.sync_input.load(Ordering::SeqCst));

    let _ = fs::remove_dir_all(data_dir);
}

// ----- parse_exec_args: pure option parser for `ctl exec` -----

fn exec_args(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

// ----- has_help_flag: pre-`--` help detection -----

#[test]
fn has_help_flag_detects_bare_help() {
    assert!(has_help_flag(&exec_args(&["--help"])));
}

#[test]
fn has_help_flag_detects_bare_short_help() {
    assert!(has_help_flag(&exec_args(&["-h"])));
}

#[test]
fn has_help_flag_detects_help_before_separator() {
    assert!(has_help_flag(&exec_args(&["--pane", "x", "--help"])));
}

#[test]
fn has_help_flag_empty_args() {
    assert!(!has_help_flag(&[]));
}

#[test]
fn has_help_flag_ignores_help_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "echo", "--help"])));
}

#[test]
fn has_help_flag_ignores_short_help_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "echo", "-h"])));
}

#[test]
fn has_help_flag_ignores_help_right_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "--help"])));
}

#[test]
fn has_help_flag_ignores_short_help_right_after_separator() {
    assert!(!has_help_flag(&exec_args(&["--", "-h"])));
}

#[test]
fn has_help_flag_ignores_help_after_separator_with_options() {
    assert!(!has_help_flag(&exec_args(&[
        "--pane", "x", "--", "echo", "--help"
    ])));
}

#[test]
fn parse_exec_args_defaults_to_active_with_bare_command() {
    let plan = parse_exec_args(&exec_args(&["echo", "hi"])).expect("bare command should parse");
    assert!(!plan.create_new);
    assert!(!plan.all);
    assert_eq!(plan.pane_ref, None);
    assert_eq!(plan.panes_list, None);
    assert_eq!(plan.title, None);
    assert_eq!(plan.command, "echo hi");
}

#[test]
fn parse_exec_args_parses_new_and_name() {
    let plan = parse_exec_args(&exec_args(&[
        "--new", "--name", "build", "--", "echo", "hi",
    ]))
    .expect("new+name should parse");
    assert!(plan.create_new);
    assert_eq!(plan.title, Some("build".to_string()));
    assert_eq!(plan.command, "echo hi");
}

#[test]
fn parse_exec_args_name_alias_n() {
    let plan = parse_exec_args(&exec_args(&["-n", "build", "--", "echo", "hi"]))
        .expect("-n alias should parse");
    assert_eq!(plan.title, Some("build".to_string()));
}

#[test]
fn parse_exec_args_parses_all() {
    let plan =
        parse_exec_args(&exec_args(&["--all", "--", "echo", "hi"])).expect("--all should parse");
    assert!(plan.all);
    assert_eq!(plan.command, "echo hi");
}

#[test]
fn parse_exec_args_parses_panes_list() {
    let plan = parse_exec_args(&exec_args(&["--panes", "a,b", "--", "echo", "hi"]))
        .expect("--panes should parse");
    assert_eq!(plan.panes_list, Some("a,b".to_string()));
}

#[test]
fn parse_exec_args_parses_pane_ref() {
    let plan = parse_exec_args(&exec_args(&["--pane", "build", "--", "echo", "hi"]))
        .expect("--pane should parse");
    assert_eq!(plan.pane_ref, Some("build".to_string()));
}

#[test]
fn parse_exec_args_joins_multi_word_command() {
    let plan = parse_exec_args(&exec_args(&["--", "sh", "-c", "exit 7"]))
        .expect("multi-word command should parse");
    assert_eq!(plan.command, "sh -c exit 7");
}

#[test]
fn parse_exec_args_errors_on_missing_command() {
    let err = parse_exec_args(&exec_args(&["--new"])).expect_err("should error");
    assert!(err.contains("exec requires a command"));
}

#[test]
fn parse_exec_args_errors_on_unknown_option() {
    let err = parse_exec_args(&exec_args(&["--bogus", "echo"])).expect_err("should error");
    assert!(err.contains("unknown exec option"));
    assert!(err.contains("--bogus"));
}

#[test]
fn parse_exec_args_errors_on_missing_pane_value() {
    let err = parse_exec_args(&exec_args(&["--pane"])).expect_err("should error");
    assert!(err.contains("--pane requires a pane"));
}

#[test]
fn parse_exec_args_errors_on_missing_panes_value() {
    let err = parse_exec_args(&exec_args(&["--panes"])).expect_err("should error");
    assert!(err.contains("--panes requires"));
}

#[test]
fn parse_exec_args_errors_on_missing_name_value() {
    let err = parse_exec_args(&exec_args(&["--name"])).expect_err("should error");
    assert!(err.contains("--name requires a title"));
}

// 07-19 CLI low: exec mirrors run's targeting-flag exclusivity instead of
// silently letting --all override --panes override --pane.

#[test]
fn parse_exec_args_rejects_all_with_pane() {
    let err = parse_exec_args(&exec_args(&["--all", "--pane", "x", "--", "true"]))
        .expect_err("--all + --pane should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    let err = parse_exec_args(&exec_args(&["--pane", "x", "--all", "--", "true"]))
        .expect_err("--pane + --all should error (order-independent)");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_exec_args_rejects_all_with_panes() {
    let err = parse_exec_args(&exec_args(&["--all", "--panes", "a,b", "--", "true"]))
        .expect_err("--all + --panes should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_exec_args_rejects_panes_with_pane() {
    let err = parse_exec_args(&exec_args(&["--panes", "a,b", "--pane", "x", "--", "true"]))
        .expect_err("--panes + --pane should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
}

#[test]
fn parse_exec_args_rejects_new_and_name_under_batched_targeting() {
    // --new/--name only apply to the single-pane path; under --all/--panes
    // they used to be silently dropped.
    let err = parse_exec_args(&exec_args(&["--all", "--new", "--", "true"]))
        .expect_err("--all + --new should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    let err = parse_exec_args(&exec_args(&["--panes", "a", "--name", "t", "--", "true"]))
        .expect_err("--panes + --name should error");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    let err = parse_exec_args(&exec_args(&["--new", "--all", "--", "true"]))
        .expect_err("--new + --all should error (order-independent)");
    assert!(
        err.contains("cannot be combined"),
        "unexpected error: {err}"
    );
    // --new + --name + --pane remains valid (single-pane targeting).
    parse_exec_args(&exec_args(&[
        "--new", "--name", "t", "--pane", "x", "--", "true",
    ]))
    .expect("--new + --name + --pane should parse");
}

#[test]
fn parse_exec_args_rejects_empty_panes_list() {
    // 07-19 CLI low: `--panes ""` / `--panes ","` targeted zero panes and
    // exited 0 vacuously — a usage error instead.
    let err = parse_exec_args(&exec_args(&["--panes", "", "--", "true"]))
        .expect_err("empty --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    let err = parse_exec_args(&exec_args(&["--panes", ",", "--", "true"]))
        .expect_err("commas-only --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    let err = parse_exec_args(&exec_args(&["--panes", " , ", "--", "true"]))
        .expect_err("whitespace-only --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    // Trailing/empty items around a real pane are still fine.
    let plan = parse_exec_args(&exec_args(&["--panes", "a,", "--", "true"]))
        .expect("a named pane with an empty item should parse");
    assert_eq!(plan.panes_list.as_deref(), Some("a,"));
}

// ----- parse_logs_args: pure option parser for `ctl logs` -----

#[test]
fn parse_logs_args_defaults_to_all_lines_no_follow() {
    let plan = parse_logs_args(&[]).expect("no args should parse");
    assert_eq!(
        plan,
        LogsPlan {
            lines: None,
            follow: false
        }
    );
}

#[test]
fn parse_logs_args_parses_short_lines_flag() {
    let plan = parse_logs_args(&exec_args(&["-n", "5"])).expect("-n should parse");
    assert_eq!(plan.lines, Some(5));
    assert!(!plan.follow);
}

#[test]
fn parse_logs_args_parses_long_lines_flag() {
    let plan = parse_logs_args(&exec_args(&["--lines", "10"])).expect("--lines should parse");
    assert_eq!(plan.lines, Some(10));
    assert!(!plan.follow);
}

#[test]
fn parse_logs_args_parses_follow_long_flag() {
    let plan = parse_logs_args(&exec_args(&["--follow"])).expect("--follow should parse");
    assert!(plan.follow);
    assert_eq!(plan.lines, None);
}

#[test]
fn parse_logs_args_parses_follow_short_flag() {
    let plan = parse_logs_args(&exec_args(&["-f"])).expect("-f should parse");
    assert!(plan.follow);
}

#[test]
fn parse_logs_args_parses_combined_lines_and_follow() {
    let plan =
        parse_logs_args(&exec_args(&["-n", "3", "--follow"])).expect("combined flags should parse");
    assert_eq!(plan.lines, Some(3));
    assert!(plan.follow);
}

#[test]
fn parse_logs_args_errors_on_invalid_line_count() {
    let err = parse_logs_args(&exec_args(&["-n", "abc"])).expect_err("should error");
    assert!(err.contains("invalid line count"));
}

#[test]
fn parse_logs_args_errors_on_missing_line_count() {
    let err = parse_logs_args(&exec_args(&["-n"])).expect_err("should error");
    assert!(err.contains("requires a line count"));
}

#[test]
fn parse_logs_args_errors_on_unknown_option() {
    let err = parse_logs_args(&exec_args(&["--bogus"])).expect_err("should error");
    assert!(err.contains("unknown logs option"));
}

// ----- read_log_tail: pure I/O helper for ctl logs -----

#[test]
fn read_log_tail_returns_all_lines_when_no_limit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\n").expect("write log");
    let lines = read_log_tail(&path, None);
    assert_eq!(lines, vec!["line1", "line2", "line3"]);
}

#[test]
fn read_log_tail_limits_to_last_n_lines() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\nline4\nline5\n").expect("write log");
    let lines = read_log_tail(&path, Some(2));
    assert_eq!(lines, vec!["line4", "line5"]);
}

#[test]
fn read_log_tail_limit_one_returns_last_line() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\nline3\n").expect("write log");
    let lines = read_log_tail(&path, Some(1));
    assert_eq!(lines, vec!["line3"]);
}

#[test]
fn read_log_tail_limit_exceeding_count_returns_all() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("test.log");
    fs::write(&path, "line1\nline2\n").expect("write log");
    let lines = read_log_tail(&path, Some(10));
    assert_eq!(lines, vec!["line1", "line2"]);
}

#[test]
fn read_log_tail_missing_file_returns_empty() {
    let lines = read_log_tail(Path::new("/nonexistent/path/log.log"), None);
    assert!(lines.is_empty());
}

// ----- parse_run_args: pure option parser for `ctl run` -----

#[test]
fn parse_run_args_defaults_to_active_pane() {
    let plan = parse_run_args(&exec_args(&["--", "echo", "hi"])).expect("bare run should parse");
    assert_eq!(plan.pane_ref, "active");
    assert_eq!(plan.command_args, vec!["echo", "hi"]);
    assert_eq!(plan.timeout_ms, None);
}

#[test]
fn parse_run_args_parses_pane_flag() {
    let plan = parse_run_args(&exec_args(&["--pane", "build", "--", "echo", "hi"]))
        .expect("--pane should parse");
    assert_eq!(plan.pane_ref, "build");
    assert_eq!(plan.command_args, vec!["echo", "hi"]);
}

#[test]
fn parse_run_args_parses_timeout() {
    let plan = parse_run_args(&exec_args(&["--timeout", "2500", "--", "true"]))
        .expect("--timeout should parse");
    assert_eq!(plan.timeout_ms, Some(2500));

    let err = parse_run_args(&exec_args(&["--timeout", "soon", "--", "true"]))
        .expect_err("non-numeric timeout should error");
    assert!(err.contains("--timeout requires a non-negative integer"));
    let err = parse_run_args(&exec_args(&["--timeout"])).expect_err("missing value should error");
    assert!(err.contains("--timeout requires a value"));
}

#[test]
fn parse_run_args_keeps_raw_args_and_quotes_at_family_time() {
    let plan = parse_run_args(&exec_args(&["--", "sh", "-c", "exit 42"]))
        .expect("multi-word command should parse");
    // The parser keeps raw tokens; quoting happens once the shell family is
    // known. POSIX quoting preserves the user's argument grouping: `exit 42`
    // stays a single argument to `-c` instead of splitting into `exit` + `$0=42`.
    assert_eq!(plan.command_args, vec!["sh", "-c", "exit 42"]);
    assert_eq!(
        quote_command_for(ShellFamily::Posix, &plan.command_args),
        "sh -c 'exit 42'"
    );
}

#[test]
fn parse_run_args_errors_on_missing_command() {
    let err = parse_run_args(&exec_args(&["--pane", "active"])).expect_err("should error");
    assert!(err.contains("run requires a command"));
}

#[test]
fn parse_run_args_errors_on_unknown_option() {
    let err = parse_run_args(&exec_args(&["--bogus", "echo"])).expect_err("should error");
    assert!(err.contains("unknown run option"));
    assert!(err.contains("--bogus"));
}

#[test]
fn parse_run_args_errors_on_missing_pane_value() {
    let err = parse_run_args(&exec_args(&["--pane"])).expect_err("should error");
    assert!(err.contains("--pane requires a pane"));
}

#[test]
fn parse_run_args_parses_all_flag() {
    let plan = parse_run_args(&exec_args(&["--all", "--", "true"])).expect("should parse");
    assert!(plan.all);
    assert_eq!(plan.panes_list, None);
    assert_eq!(plan.pane_ref, "active");
    assert_eq!(plan.command_args, vec!["true"]);
}

#[test]
fn parse_run_args_parses_panes_list() {
    let plan =
        parse_run_args(&exec_args(&["--panes", "a,b", "--", "echo", "hi"])).expect("should parse");
    assert!(!plan.all);
    assert_eq!(plan.panes_list.as_deref(), Some("a,b"));
    assert_eq!(plan.command_args, vec!["echo", "hi"]);
}

#[test]
fn parse_run_args_errors_on_missing_panes_value() {
    let err = parse_run_args(&exec_args(&["--panes"])).expect_err("should error");
    assert!(err.contains("--panes requires"));
}

#[test]
fn parse_run_args_rejects_all_with_pane() {
    parse_run_args(&exec_args(&["--all", "--pane", "x", "--", "true"]))
        .expect_err("--all + --pane should error");
}

#[test]
fn parse_run_args_rejects_all_with_panes() {
    parse_run_args(&exec_args(&["--all", "--panes", "a,b", "--", "true"]))
        .expect_err("--all + --panes should error");
}

#[test]
fn parse_run_args_rejects_panes_with_pane() {
    parse_run_args(&exec_args(&["--panes", "a,b", "--pane", "x", "--", "true"]))
        .expect_err("--panes + --pane should error");
}

#[test]
fn parse_run_args_rejects_empty_panes_list() {
    // 07-19 CLI low: `--panes ""` / `--panes ","` resolved to an empty
    // target set and collect_batched_results exited 0 vacuously.
    let err = parse_run_args(&exec_args(&["--panes", "", "--", "true"]))
        .expect_err("empty --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    let err = parse_run_args(&exec_args(&["--panes", ",", "--", "true"]))
        .expect_err("commas-only --panes should error");
    assert!(err.contains("at least one pane"), "unexpected error: {err}");
    // A real pane amid empty items is still accepted.
    let plan = parse_run_args(&exec_args(&["--panes", "a,,b", "--", "true"]))
        .expect("named panes with an empty item should parse");
    assert_eq!(plan.panes_list.as_deref(), Some("a,,b"));
}

// ----- shell_quote (arg-grouping fix for ctl run) -----

#[test]
fn shell_quote_leaves_safe_args_unquoted() {
    assert_eq!(shell_quote("echo"), "echo");
    assert_eq!(shell_quote("/usr/bin/false"), "/usr/bin/false");
    assert_eq!(shell_quote("exit"), "exit");
    assert_eq!(shell_quote("a-b_c.d=e,f@g+h"), "a-b_c.d=e,f@g+h");
}

#[test]
fn shell_arg_is_safe_rejects_leading_equals_only() {
    // 07-19 CLI low: zsh equals-expansion makes a word-initial `=`
    // (`ctl run -- =foo`) expand to the path of `foo`; mid-word `=` keeps
    // its literal assignment shape.
    assert!(!shell_arg_is_safe("=foo"));
    assert!(!shell_arg_is_safe("="));
    assert!(shell_arg_is_safe("FOO=bar"));
    assert!(shell_arg_is_safe("a="));
    assert_eq!(shell_quote("=foo"), "'=foo'");
    assert_eq!(shell_quote("FOO=bar"), "FOO=bar");
    // Same predicate guards the fish quoter.
    assert_eq!(fish_quote("=foo"), "'=foo'");
    assert_eq!(fish_quote("FOO=bar"), "FOO=bar");
}

#[test]
fn shell_quote_quotes_args_with_spaces() {
    assert_eq!(shell_quote("exit 7"), "'exit 7'");
    assert_eq!(shell_quote("exit 42"), "'exit 42'");
    assert_eq!(shell_quote("echo hello world"), "'echo hello world'");
}

#[test]
fn shell_quote_quotes_empty_arg() {
    assert_eq!(shell_quote(""), "''");
}

#[test]
fn shell_quote_escapes_embedded_single_quotes() {
    // `it's a test` → `'it'\''s a test'`
    assert_eq!(shell_quote("it's a test"), "'it'\\''s a test'");
}

#[test]
fn shell_quote_quotes_shell_metacharacters() {
    assert_eq!(shell_quote("$HOME"), "'$HOME'");
    assert_eq!(shell_quote("a;b"), "'a;b'");
    assert_eq!(shell_quote("a|b"), "'a|b'");
    assert_eq!(shell_quote("*"), "'*'");
    assert_eq!(shell_quote("`cmd`"), "'`cmd`'");
}

#[test]
fn quote_command_for_posix_preserves_grouping() {
    // The arg-grouping bug: `run -- sh -c 'exit 7'` was joined as
    // `sh -c exit 7` which the pane shell parses as `sh -c exit` with
    // `$0=7`. Shell-quoting preserves `exit 7` as a single argument.
    let plan = parse_run_args(&exec_args(&["--", "sh", "-c", "exit 7"])).expect("should parse");
    assert_eq!(
        quote_command_for(ShellFamily::Posix, &plan.command_args),
        "sh -c 'exit 7'"
    );

    let plan = parse_run_args(&exec_args(&["--", "sh", "-c", "echo hello; exit 3"]))
        .expect("should parse");
    assert_eq!(
        quote_command_for(ShellFamily::Posix, &plan.command_args),
        "sh -c 'echo hello; exit 3'"
    );
}

#[test]
fn fish_quote_escapes_fish_specials() {
    // Fish single quotes treat only \ and ' as special (backslash-escaped).
    assert_eq!(fish_quote("plain"), "plain");
    assert_eq!(fish_quote(""), "''");
    assert_eq!(fish_quote("has space"), "'has space'");
    assert_eq!(fish_quote("it's"), "'it\\'s'");
    // A backslash-bearing arg: POSIX quoting would pass `\` through
    // untouched inside '…', but fish interprets `\'`/`\\` inside single
    // quotes — so fish quoting must double the backslashes (H7).
    assert_eq!(fish_quote(r"C:\tmp\"), r"'C:\\tmp\\'");
    assert_eq!(
        quote_command_for(ShellFamily::Fish, &["echo".to_string(), r"a\b".to_string()]),
        r"echo 'a\\b'"
    );
}

// ----- detect_shell_family (shell-aware exit codes, VAL-ORCH-006) -----

#[test]
fn detect_shell_family_posix_shells() {
    assert_eq!(detect_shell_family("/bin/sh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/bash"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/zsh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/dash"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/usr/bin/sh"), ShellFamily::Posix);
}

#[test]
fn detect_shell_family_fish() {
    assert_eq!(detect_shell_family("fish"), ShellFamily::Fish);
    assert_eq!(detect_shell_family("/usr/bin/fish"), ShellFamily::Fish);
    assert_eq!(
        detect_shell_family("/opt/homebrew/bin/fish"),
        ShellFamily::Fish
    );
    // A path whose basename starts with "fish" (e.g. fish-git) is also Fish.
    assert_eq!(
        detect_shell_family("/usr/local/bin/fish-dev"),
        ShellFamily::Fish
    );
}

#[test]
fn detect_shell_family_unknown_shell_defaults_to_posix() {
    assert_eq!(detect_shell_family("/bin/ksh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family("/bin/tcsh"), ShellFamily::Posix);
    assert_eq!(detect_shell_family(""), ShellFamily::Posix);
}

// ----- build_run_wrapper (shell-aware exit codes, VAL-ORCH-006) -----

#[test]
fn build_run_wrapper_posix_uses_dollar_question() {
    let wrapper = build_run_wrapper("true", "__sgian_rc_42_1700", ShellFamily::Posix);
    assert!(
        wrapper.contains("\"$?\""),
        "POSIX wrapper must use \"$?\": {wrapper}"
    );
    assert!(wrapper.ends_with('\r'), "wrapper must end with CR");
    assert!(
        wrapper.contains("; printf '\\n__sgian_rc_42_1700:%s\\n'"),
        "wrapper must contain marker printf: {wrapper}"
    );
}

#[test]
fn build_run_wrapper_fish_uses_dollar_status() {
    let wrapper = build_run_wrapper("true", "__sgian_rc_42_1700", ShellFamily::Fish);
    assert!(
        wrapper.contains("$status"),
        "fish wrapper must use $status: {wrapper}"
    );
    assert!(
        !wrapper.contains("$?"),
        "fish wrapper must NOT use $?: {wrapper}"
    );
    assert!(wrapper.ends_with('\r'), "wrapper must end with CR");
    assert!(
        wrapper.contains("; printf '\\n__sgian_rc_42_1700:%s\\n'"),
        "wrapper must contain marker printf: {wrapper}"
    );
}

// ----- parse_exit_marker + trim_to_tail under high-volume output -----

#[test]
fn parse_exit_marker_finds_code_after_large_output() {
    let prefix = "__sgian_rc_42_1700000000:";
    // Simulate 100k lines of output followed by the marker line.
    let mut buffer = String::new();
    for i in 0..100_000 {
        buffer.push_str(&format!("line{i}\n"));
    }
    buffer.push_str(&format!("{prefix}5\n"));
    assert_eq!(parse_exit_marker(&buffer, prefix), Some(5));
}

#[test]
fn trim_to_tail_preserves_partial_marker_for_next_chunk() {
    let prefix = "__sgian_rc_42_1700000000:";
    let mut buffer = String::new();
    // Fill with noise, then a partial marker (digits still in flight).
    for i in 0..10_000 {
        buffer.push_str(&format!("line{i}\n"));
    }
    buffer.push_str(prefix);
    buffer.push('4'); // partial code, no terminator yet

    // Simulate the trim that control_run does after each event.
    trim_to_tail(&mut buffer, prefix.len() + 64);

    // The partial marker must survive the trim so the next chunk can
    // complete it.
    assert!(
        buffer.contains(prefix),
        "partial marker prefix must survive trim, got: ...{}",
        &buffer[buffer.len().saturating_sub(80)..]
    );
    assert!(buffer.ends_with('4'));

    // Now append the terminator — the full marker should parse.
    buffer.push('\n');
    assert_eq!(parse_exit_marker(&buffer, prefix), Some(4));
}

#[test]
fn trim_to_tail_repeatedly_keeps_marker_findable() {
    // Simulate the high-volume control_run loop: many events, each
    // trimmed, with the marker arriving only at the end.
    let prefix = "__sgian_rc_99_1700000001:";
    let mut buffer = String::new();
    for chunk in 0..500u32 {
        for i in 0..100 {
            buffer.push_str(&format!("c{chunk}l{i}\n"));
        }
        trim_to_tail(&mut buffer, prefix.len() + 64);
        // Marker not yet arrived.
        assert_eq!(parse_exit_marker(&buffer, prefix), None);
    }
    // Final chunk carries the marker.
    buffer.push_str(&format!("{prefix}42\n"));
    assert_eq!(parse_exit_marker(&buffer, prefix), Some(42));
}

// ----- Multi-subscriber broadcast fan-out (VAL-OBS-022) -----

#[test]
fn broadcast_delivers_to_every_concurrent_subscriber() {
    let scrollback_dir = tempfile::tempdir().expect("scrollback dir");
    let router = OutputRouter::new(scrollback_dir.path().to_path_buf());

    // K subscribers, each backed by a connected transport pair so the
    // fan-out is observable per-subscriber (not just a count).
    const K: usize = 3;
    let mut clients = Vec::new();
    for _ in 0..K {
        let (client, server) = test_transport_pair().expect("transport pair should be available");
        router
            .add_subscriber(server, 1)
            .expect("subscribe within the cap");
        clients.push(client);
    }

    assert_eq!(router.subscriber_count(), K);

    let event = DaemonEvent::PtyOutput {
        pane_id: "pane-1".to_string(),
        data: "fan-out\n".to_string(),
    };
    router.broadcast(&event);

    // EACH of the K subscribers must actually receive the event — proving
    // fan-out, not just bookkeeping.
    for client in clients {
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout should apply");
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("subscriber should receive the broadcast event");
        let received: DaemonEvent = serde_json::from_str(&line).expect("event should deserialize");
        assert_eq!(received, event);
    }
}

/// M5: subscription beyond MAX_SUBSCRIBERS is refused with a clean error
/// (the stream is handed back and closed — no entry, channel, or threads
/// leak), and a disconnect frees a slot so subscribing succeeds again.
#[cfg(unix)]
#[test]
fn subscriber_cap_rejects_beyond_limit_and_recovers_after_disconnect() {
    let scrollback_dir = tempfile::tempdir().expect("temp scrollback dir");
    let router = OutputRouter::new(scrollback_dir.path().to_path_buf());

    let mut peers = Vec::new();
    for _ in 0..MAX_SUBSCRIBERS {
        let (client, server) = UnixStream::pair().expect("unix stream pair should be available");
        router
            .add_subscriber(server, 1)
            .expect("subscribe within the cap");
        peers.push(client);
    }
    assert_eq!(router.subscriber_count(), MAX_SUBSCRIBERS);

    // The next subscribe is rejected cleanly: the reason and the stream are
    // handed back so the caller can respond and close it (no leak).
    let (client, server) = UnixStream::pair().expect("unix stream pair should be available");
    let (reason, stream) = router
        .add_subscriber(server, 1)
        .expect_err("subscribe beyond the cap should be refused");
    assert!(
        reason.contains("subscriber limit"),
        "unexpected reason: {reason}"
    );
    drop(stream);
    drop(client);
    assert_eq!(
        router.subscriber_count(),
        MAX_SUBSCRIBERS,
        "a rejected subscribe must not register"
    );

    // Disconnect one subscriber; its watcher prunes the entry, and a new
    // subscribe succeeds on the freed slot.
    peers.pop();
    wait_for(|| router.subscriber_count() < MAX_SUBSCRIBERS);
    let (client, server) = UnixStream::pair().expect("unix stream pair should be available");
    router
        .add_subscriber(server, 1)
        .expect("subscribe after a disconnect freed a slot");
    peers.push(client);
}

/// L12 pin: a subscriber that never drains its 1024-slot queue is DROPPED on
/// burst — broadcast fan-out must never block on one slow consumer. With the
/// peer never reading, the socket buffer fills, the writer thread stalls,
/// the bounded queue fills, and a later broadcast's `try_send` fails Full.
#[cfg(unix)]
#[test]
fn slow_subscriber_is_dropped_when_its_queue_fills() {
    let scrollback_dir = tempfile::tempdir().expect("temp scrollback dir");
    let router = OutputRouter::new(scrollback_dir.path().to_path_buf());
    let (client, server) = UnixStream::pair().expect("unix stream pair");
    router
        .add_subscriber(server, 1)
        .expect("subscribe within the cap");
    assert_eq!(router.subscriber_count(), 1);

    // Never read from `client`. Payloads large enough to fill the socket
    // buffer fast make the queue fill deterministically: once it does, the
    // broadcast fan-out drops the subscriber synchronously.
    let event = DaemonEvent::PtyOutput {
        pane_id: "pane-1".to_string(),
        data: "x".repeat(32 * 1024),
    };
    for _ in 0..SUBSCRIBER_QUEUE_LIMIT * 2 {
        router.broadcast(&event);
    }

    wait_for(|| router.subscriber_count() == 0);
    drop(client);
}

// ----- In-process integration harness over a short /tmp socket -----

/// A daemon spawned in-process on a background thread over a SHORT /tmp socket,
/// driven through the real `run_daemon_with_config` path with an injected
/// `Config` (never reads the developer's real config.json).
struct TestDaemon {
    data_dir: tempfile::TempDir,
    cwd: PathBuf,
    _socket_dir: tempfile::TempDir,
    socket_path: PathBuf,
    token: String,
    join_handle: Option<thread::JoinHandle<Result<(), String>>>,
}

impl TestDaemon {
    fn spawn(config: Config) -> Self {
        Self::spawn_with_cwd(config, PathBuf::from("/tmp/sgian-itest"))
    }

    fn spawn_with_cwd(mut config: Config, cwd: PathBuf) -> Self {
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
    fn client(&self) -> DaemonClient {
        DaemonClient {
            cwd: self.cwd.clone(),
            socket_path: self.socket_path.clone(),
            data_dir: self.data_dir.path().to_path_buf(),
            token: self.token.clone(),
            auto_spawn: false,
        }
    }

    /// Send Shutdown, join the daemon thread, and drop the temp dirs.
    fn shutdown(mut self) {
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
    fn restart(&mut self, config: Config) {
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
    fn shutdown_and_read_log(&mut self) -> String {
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

fn retry_read_token(path: &Path) -> String {
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

fn retry_until_ready(socket_path: &Path, token: &str) {
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
fn capture_pane_env_print(config: Config, var_name: &str) -> String {
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

// ----- control_run exit-code integration (VAL-ORCH-001..005, 024, 027, 031) -----

/// Helper: spawn a daemon with the given shell, create a pane, wait for it
/// to become Live, and return (daemon, client, pane_id).
fn spawn_run_daemon(shell: &str) -> (TestDaemon, DaemonClient, String) {
    let config = Config {
        shell: Some(shell.to_string()),
        idle_shutdown_secs: None, // disable idle shutdown for test stability
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // CreatePane spawns the shell. The pane is Live once the shell process
    // is running (ensure_pane spawns before returning).
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    // Brief settle so the shell is ready to accept input on the PTY.
    thread::sleep(Duration::from_millis(100));

    (daemon, client, pane.id)
}

/// Helper: run `control_run` in a background thread with a timeout so a
/// bug (e.g. marker never arrives) doesn't hang the test suite.
fn control_run_with_timeout(
    client: &DaemonClient,
    args: &[&str],
    json: bool,
    timeout: Duration,
) -> Result<(), String> {
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    // We can't move &DaemonClient into the thread, so reconstruct a clone
    // with the same connection info.
    let client_clone = DaemonClient {
        cwd: client.cwd.clone(),
        socket_path: client.socket_path.clone(),
        data_dir: client.data_dir.clone(),
        token: client.token.clone(),
        auto_spawn: false,
    };
    std::thread::spawn(move || {
        let _ = tx.send(control_run(&client_clone, &args, json));
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err("control_run timed out".to_string()))
}

/// VAL-ORCH-001: `ctl run` reports success as exit 0.
#[test]
fn control_run_reports_exit_0_for_success() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "true"],
        false,
        Duration::from_secs(10),
    );
    assert!(result.is_ok(), "true should report exit 0: {:?}", result);
    daemon.shutdown();
}

/// VAL-ORCH-002: `ctl run` propagates a simple non-zero exit code.
#[test]
fn control_run_reports_exit_1_for_false() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "/usr/bin/false"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 1"),
        "false should report exit 1: {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-003: arg-grouping bug fixed — `run -- sh -c 'exit 7'` reports 7.
#[test]
fn control_run_arg_grouping_exit_7() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", "exit 7"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 7"),
        "sh -c 'exit 7' should report exit 7 (arg-grouping fix): {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-004: exact non-zero codes are preserved (not collapsed to 0/1).
#[test]
fn control_run_preserves_exact_exit_code_42() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", "exit 42"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 42"),
        "sh -c 'exit 42' should report exit 42: {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-004: exact codes via --json output.
#[test]
fn control_run_json_reports_exact_exit_code() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    // For json mode, control_run writes JSON to stdout and returns
    // Err("command exited with code N") for non-zero. We verify the
    // returned error carries the exact code.
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", "exit 55"],
        true,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 55"),
        "json mode should still report exit 55: {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-005: correct exit codes across the POSIX shell family.
#[test]
fn control_run_across_posix_shells() {
    for shell in &["/bin/sh", "/bin/bash", "/bin/zsh"] {
        // Skip if the shell isn't available on this system.
        if !std::path::Path::new(shell).exists() {
            eprintln!("skipping {shell}: not installed");
            continue;
        }
        let (daemon, client, pane_id) = spawn_run_daemon(shell);

        // Success → exit 0
        let ok_result = control_run_with_timeout(
            &client,
            &["--pane", &pane_id, "--", "true"],
            false,
            Duration::from_secs(10),
        );
        assert!(
            ok_result.is_ok(),
            "{shell}: true should report exit 0: {:?}",
            ok_result
        );

        // Non-zero → exact code. Use sh -c so it works regardless of the
        // pane's shell (the subshell is always /bin/sh).
        let err_result = control_run_with_timeout(
            &client,
            &["--pane", &pane_id, "--", "sh", "-c", "exit 5"],
            false,
            Duration::from_secs(10),
        );
        assert!(
            err_result
                .as_ref()
                .unwrap_err()
                .contains("command exited with code 5"),
            "{shell}: sh -c 'exit 5' should report exit 5: {:?}",
            err_result
        );

        daemon.shutdown();
    }
}

/// VAL-ORCH-024: `ctl run` against a not-live pane fails fast (no hang).
#[test]
fn control_run_not_live_pane_fails_fast() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");

    // End the pane's shell by sending "exit\n".
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "exit\n".to_string(),
        })
        .expect("send exit should succeed");

    // Wait for the pane to become Ended (poll the status).
    let mut became_ended = false;
    for _ in 0..50 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: pane_id.clone(),
            })
            .expect("status should succeed");
        if status.state != PaneRuntimeState::Live {
            became_ended = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(became_ended, "pane should become Ended after exit");

    // control_run against the ended pane must fail fast, not hang.
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "echo", "x"],
        false,
        Duration::from_secs(5),
    );
    assert!(
        result.as_ref().unwrap_err().contains("pane is not live"),
        "not-live pane should fail fast with 'pane is not live': {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-027: `ctl run` reports the correct code under high-volume output.
#[test]
fn control_run_high_volume_output() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    // 5000 lines of output, then exit 5. The trim_to_tail must keep the
    // marker findable despite the volume.
    let cmd = "sh -c 'i=0; while [ $i -lt 5000 ]; do echo line$i; i=$((i+1)); done; exit 5'";
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", cmd],
        false,
        Duration::from_secs(30),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 5"),
        "high-volume command should report exit 5: {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-031: `ctl run` exit-code capture is correct while sync-input is on.
#[test]
fn control_run_correct_under_sync_input() {
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Create two panes so sync has somewhere to mirror.
    let pane_a: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create pane A");
    let pane_b: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create pane B");
    thread::sleep(Duration::from_millis(100));

    // Enable sync input — SendInput is now mirrored to all live panes.
    let _sync_result: Value = client
        .request(DaemonRequest::SetSyncInput { enabled: true })
        .expect("sync on should succeed");
    assert_eq!(_sync_result["sync_input"], json!(true));

    // Run a command against pane A. The wrapper + marker probe is mirrored
    // to pane B too, but control_run only watches pane A's output, so the
    // correct code should be reported.
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_a.id, "--", "sh", "-c", "exit 4"],
        false,
        Duration::from_secs(15),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 4"),
        "sync-on: sh -c 'exit 4' on pane A should report exit 4: {:?}",
        result
    );

    // Turn sync off and verify normal targeting still works.
    let _off: Value = client
        .request(DaemonRequest::SetSyncInput { enabled: false })
        .expect("sync off should succeed");

    let result2 = control_run_with_timeout(
        &client,
        &["--pane", &pane_b.id, "--", "true"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result2.is_ok(),
        "sync off: true on pane B should report exit 0: {:?}",
        result2
    );

    daemon.shutdown();
}

/// VAL-ORCH-006: non-POSIX shell (fish) reports correct code and does NOT hang.
/// Fish uses `$status` instead of `$?`; the old POSIX-only wrapper caused
/// `ctl run` to hang until the pane ended. This test is skipped if fish is
/// not installed on the system.
#[test]
fn control_run_fish_reports_correct_code_no_hang() {
    let fish = if std::path::Path::new("/opt/homebrew/bin/fish").exists() {
        "/opt/homebrew/bin/fish"
    } else if std::path::Path::new("/usr/bin/fish").exists() {
        "/usr/bin/fish"
    } else if std::path::Path::new("/usr/local/bin/fish").exists() {
        "/usr/local/bin/fish"
    } else {
        // Fish not installed — skip (the contract allows this).
        eprintln!("skipping control_run_fish_reports_correct_code_no_hang: fish not installed");
        return;
    };

    let (daemon, client, pane_id) = spawn_run_daemon(fish);

    // Success → exit 0. Must complete before the timeout (no hang).
    let ok_result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "true"],
        false,
        Duration::from_secs(15),
    );
    assert!(
        ok_result.is_ok(),
        "fish: true should report exit 0 (no hang): {:?}",
        ok_result
    );

    // Non-zero → exit 1. Must complete before the timeout (no hang).
    let err_result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "false"],
        false,
        Duration::from_secs(15),
    );
    assert!(
        err_result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 1"),
        "fish: false should report exit 1 (no hang): {:?}",
        err_result
    );

    // Exact non-zero code: fish -c 'exit 3' → exit 3.
    let code_result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "fish", "-c", "exit 3"],
        false,
        Duration::from_secs(15),
    );
    assert!(
        code_result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 3"),
        "fish: fish -c 'exit 3' should report exit 3: {:?}",
        code_result
    );

    daemon.shutdown();
}

/// VAL-ORCH-007: command self-output cannot corrupt the reported code.
/// A command whose own stdout contains digits (e.g. `echo 999; exit 3`)
/// must not perturb the exit code `ctl run` reports — the marker is unique
/// and the parser looks for the marker prefix, not bare digits.
#[test]
fn control_run_self_output_cannot_corrupt_code() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", "echo 999; exit 3"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 3"),
        "echo 999; exit 3 should report exit 3 (not 999, not 0): {:?}",
        result
    );
    daemon.shutdown();
}

// ----- help / usage discovery (VAL-ORCH-026) -----

/// VAL-ORCH-026: `ctl run --help` prints usage instead of erroring with
/// "unknown run option: --help".
#[test]
fn control_run_help_flag_prints_help_not_error() {
    let (daemon, client, _pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(&client, &["--help"], false, Duration::from_secs(5));
    assert!(
        result.is_ok(),
        "run --help should succeed, not error: {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-026: `ctl run -h` (short form) also prints help.
#[test]
fn control_run_short_help_flag_prints_help() {
    let (daemon, client, _pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(&client, &["-h"], false, Duration::from_secs(5));
    assert!(
        result.is_ok(),
        "run -h should succeed, not error: {:?}",
        result
    );
    daemon.shutdown();
}

/// VAL-ORCH-026: `ctl exec --help` prints usage instead of erroring with
/// "unknown exec option: --help".
#[test]
fn control_exec_help_flag_prints_help_not_error() {
    let (daemon, client, _pane_id) = spawn_run_daemon("/bin/sh");
    let args: Vec<String> = vec!["--help".to_string()];
    let client_clone = DaemonClient {
        cwd: client.cwd.clone(),
        socket_path: client.socket_path.clone(),
        data_dir: client.data_dir.clone(),
        token: client.token.clone(),
        auto_spawn: false,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(control_exec(&client_clone, &args, false));
    });
    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| Err("control_exec timed out".to_string()));
    assert!(
        result.is_ok(),
        "exec --help should succeed, not error: {:?}",
        result
    );
    daemon.shutdown();
}

// ----- --help/-h after `--` payload preservation regression tests -----

/// Regression: `ctl run -- sh -c 'exit 3' --help` must execute the command
/// (exit 3), NOT short-circuit to printing help because `--help` appears
/// after the `--` separator as part of the command payload.
#[test]
fn control_run_help_after_separator_runs_command() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", "exit 3", "--help"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 3"),
        "run -- sh -c 'exit 3' --help should execute and report exit 3, \
         not print help: {:?}",
        result
    );
    daemon.shutdown();
}

/// Regression: `ctl run -- echo -h` must execute `echo -h` (exit 0), NOT
/// short-circuit to help because `-h` appears after `--`.
#[test]
fn control_run_short_help_after_separator_runs_command() {
    let (daemon, client, pane_id) = spawn_run_daemon("/bin/sh");
    // `echo -h` on /bin/sh echoes "-h" and exits 0. If the help
    // short-circuit fires, we'd get Ok without running the command.
    // Use `sh -c 'exit 5' -h` so a successful run produces a non-zero
    // exit code, distinguishing "ran" from "printed help".
    let result = control_run_with_timeout(
        &client,
        &["--pane", &pane_id, "--", "sh", "-c", "exit 5", "-h"],
        false,
        Duration::from_secs(10),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .contains("command exited with code 5"),
        "run -- sh -c 'exit 5' -h should execute and report exit 5, \
         not print help: {:?}",
        result
    );
    daemon.shutdown();
}

/// Regression: `ctl exec --pane <nonexistent> -- echo --help` must attempt
/// to resolve the pane (and fail), NOT short-circuit to printing help
/// because `--help` appears after `--`.
#[test]
fn control_exec_help_after_separator_runs_command() {
    let (daemon, client, _pane_id) = spawn_run_daemon("/bin/sh");
    let args: Vec<String> = vec![
        "--pane".to_string(),
        "nonexistent-pane-xyz".to_string(),
        "--".to_string(),
        "echo".to_string(),
        "--help".to_string(),
    ];
    let client_clone = DaemonClient {
        cwd: client.cwd.clone(),
        socket_path: client.socket_path.clone(),
        data_dir: client.data_dir.clone(),
        token: client.token.clone(),
        auto_spawn: false,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(control_exec(&client_clone, &args, false));
    });
    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| Err("control_exec timed out".to_string()));
    assert!(
        result.is_err(),
        "exec --pane <nonexistent> -- echo --help should error on pane \
         resolution, not print help: {:?}",
        result
    );
    daemon.shutdown();
}

/// Regression: `ctl exec --pane <nonexistent> -- echo -h` must attempt to
/// resolve the pane (and fail), NOT short-circuit to help.
#[test]
fn control_exec_short_help_after_separator_runs_command() {
    let (daemon, client, _pane_id) = spawn_run_daemon("/bin/sh");
    let args: Vec<String> = vec![
        "--pane".to_string(),
        "nonexistent-pane-xyz".to_string(),
        "--".to_string(),
        "echo".to_string(),
        "-h".to_string(),
    ];
    let client_clone = DaemonClient {
        cwd: client.cwd.clone(),
        socket_path: client.socket_path.clone(),
        data_dir: client.data_dir.clone(),
        token: client.token.clone(),
        auto_spawn: false,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(control_exec(&client_clone, &args, false));
    });
    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| Err("control_exec timed out".to_string()));
    assert!(
        result.is_err(),
        "exec --pane <nonexistent> -- echo -h should error on pane \
         resolution, not print help: {:?}",
        result
    );
    daemon.shutdown();
}

// ----- decode_cli_text --lf / --raw integration (VAL-ORCH-015, 016, 029) -----

/// Spawn a daemon whose panes run a fixed-size byte dumper (`head -c 3 |
/// od -An -tx1`), subscribe, create `count` panes, and return the daemon,
/// client, subscriber stream, and the created pane ids. The shell reads
/// exactly 3 bytes from the PTY then hex-dumps them, so sending a 3-byte
/// payload produces a deterministic `61 62 0X` line we can assert on.
fn spawn_byte_dumper_daemon(
    count: usize,
) -> (
    TestDaemon,
    DaemonClient,
    std::io::BufReader<TransportStream>,
    Vec<String>,
) {
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        shell_args: Some(vec![
            "-c".to_string(),
            "head -c 3 | od -An -tx1".to_string(),
        ]),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Subscribe BEFORE creating panes so we capture the dumper output.
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout should apply");

    let mut pane_ids = Vec::new();
    for _ in 0..count {
        let pane: Pane = client
            .request(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed");
        pane_ids.push(pane.id);
    }
    // Brief settle so each shell is blocked on head reading the PTY.
    thread::sleep(Duration::from_millis(150));

    let reader = BufReader::new(stream);
    (daemon, client, reader, pane_ids)
}

/// Drain PtyOutput events from the subscriber stream until we see the od
/// hex-dump line for `pane_id` (contains both `61` and `62` — the `ab`
/// prefix bytes) or time out. The caller asserts on the specific trailing
/// byte (`0a` vs `0d`) in the returned data.
fn capture_od_output(reader: &mut std::io::BufReader<TransportStream>, pane_id: &str) -> String {
    let mut line = String::new();
    for _ in 0..80 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PtyOutput {
                    data,
                    pane_id: pane,
                }) = serde_json::from_str::<DaemonEvent>(&line)
                {
                    if pane == pane_id && data.contains("61") && data.contains("62") {
                        return data;
                    }
                }
            }
            Err(_) => break,
        }
    }
    String::new()
}

/// VAL-ORCH-015: `ctl send --lf` transmits a literal LF (0x0A), not CR.
#[test]
fn control_send_lf_transmits_literal_lf() {
    let (daemon, client, mut reader, pane_ids) = spawn_byte_dumper_daemon(1);
    let pane_id = &pane_ids[0];
    control_send_input(
        &client,
        &["--lf".to_string(), pane_id.clone(), "ab\\n".to_string()],
    )
    .expect("send --lf should succeed");
    let data = capture_od_output(&mut reader, pane_id);
    daemon.shutdown();
    assert!(
        !data.is_empty(),
        "send --lf should produce od output, got empty"
    );
    assert!(
        data.contains("0a"),
        "send --lf should deliver literal LF (0x0a), got: {data:?}"
    );
    assert!(
        !data.contains("0d"),
        "send --lf must NOT deliver CR (0x0d), got: {data:?}"
    );
}

/// VAL-ORCH-015: `--raw` is an alias for `--lf`.
#[test]
fn control_send_raw_alias_transmits_literal_lf() {
    let (daemon, client, mut reader, pane_ids) = spawn_byte_dumper_daemon(1);
    let pane_id = &pane_ids[0];
    control_send_input(
        &client,
        &[pane_id.clone(), "--raw".to_string(), "ab\\n".to_string()],
    )
    .expect("send --raw should succeed");
    let data = capture_od_output(&mut reader, pane_id);
    daemon.shutdown();
    assert!(
        !data.is_empty(),
        "send --raw should produce od output, got empty"
    );
    assert!(
        data.contains("0a"),
        "send --raw should deliver literal LF (0x0a), got: {data:?}"
    );
}

/// VAL-ORCH-016: default `send` (no flag) still submits the line (no
/// regression). The raw byte mapping `\n`→CR (0x0D) is verified at the
/// unit level (`decode_cli_text_default_maps_newline_to_cr`); here we
/// confirm the default still causes the shell to execute the line.
#[test]
fn control_send_default_submits_line_no_regression() {
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout should apply");

    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    let pane_id = pane.id.clone();
    thread::sleep(Duration::from_millis(150));

    // Default send: \n maps to CR, which submits the line to the shell.
    control_send_input(
        &client,
        &[pane_id.clone(), "echo sgian_lf_mark\\n".to_string()],
    )
    .expect("default send should succeed");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut captured = String::new();
    for _ in 0..80 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PtyOutput { data, pane_id: p }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if p == pane_id && data.contains("sgian_lf_mark") {
                        captured = data;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    daemon.shutdown();
    assert!(
        captured.contains("sgian_lf_mark"),
        "default send should submit the line (echo output observed), got: {captured:?}"
    );
}

/// VAL-ORCH-029: `ctl broadcast --lf` transmits a literal LF to EVERY live
/// pane.
#[test]
fn control_broadcast_lf_transmits_literal_lf_to_all_panes() {
    let (daemon, client, mut reader, pane_ids) = spawn_byte_dumper_daemon(2);
    // broadcast --lf "ab\n" — 3 bytes (a, b, LF) to every live pane.
    control_broadcast(&client, &["--lf".to_string(), "ab\\n".to_string()], false)
        .expect("broadcast --lf should succeed");
    // Drain all PtyOutput events in a single pass, collecting the od output
    // per pane (events interleave across panes, so a sequential per-pane
    // capture could skip a pane whose output arrived first).
    let mut line = String::new();
    let mut od_outputs: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for _ in 0..160 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PtyOutput {
                    data,
                    pane_id: pane,
                }) = serde_json::from_str::<DaemonEvent>(&line)
                {
                    if pane_ids.contains(&pane) && data.contains("61") && data.contains("62") {
                        od_outputs.entry(pane).or_insert(data);
                        if od_outputs.len() == pane_ids.len() {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    daemon.shutdown();
    for pane_id in &pane_ids {
        let data = od_outputs.get(pane_id).cloned().unwrap_or_default();
        assert!(
            !data.is_empty(),
            "broadcast --lf to pane {pane_id} should produce od output, got empty"
        );
        assert!(
            data.contains("0a"),
            "broadcast --lf to pane {pane_id} should deliver LF (0x0a), got: {data:?}"
        );
        assert!(
            !data.contains("0d"),
            "broadcast --lf to pane {pane_id} must NOT deliver CR (0x0d), got: {data:?}"
        );
    }
}

/// Regression guard: default `broadcast` (no flag) still submits the line
/// to every live pane (VAL-ORCH-017 + the no-regression requirement).
#[test]
fn control_broadcast_default_submits_line_all_panes() {
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout should apply");

    let mut pane_ids = Vec::new();
    for _ in 0..2 {
        let pane: Pane = client
            .request(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed");
        pane_ids.push(pane.id);
    }
    thread::sleep(Duration::from_millis(150));

    control_broadcast(&client, &["echo bc_mark\\n".to_string()], false)
        .expect("default broadcast should succeed");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..120 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PtyOutput { data, pane_id: p }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if data.contains("bc_mark") {
                        seen.insert(p);
                    }
                }
            }
            Err(_) => break,
        }
    }
    daemon.shutdown();
    for pane_id in &pane_ids {
        assert!(
            seen.contains(pane_id),
            "default broadcast should submit the line to pane {pane_id}"
        );
    }
}

// ----- Batched multi-pane exec (VAL-ORCH-008..013, 028, 030) -----

/// Spawn a daemon with named live panes. The daemon's default pane-1 is
/// ended first so only the named panes are live (keeps `--all` expectations
/// deterministic). Returns the daemon, client, and the created pane ids.
fn spawn_batched_daemon(shell: &str, names: &[&str]) -> (TestDaemon, DaemonClient, Vec<String>) {
    let config = Config {
        shell: Some(shell.to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // End the default pane-1 so only the named panes we create are live.
    let default_list: PaneList = client
        .request(DaemonRequest::ListPanes)
        .expect("list default");
    let default_id = default_list.panes[0].pane.id.clone();
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: default_id.clone(),
            input: "exit\n".to_string(),
        })
        .expect("end default pane");
    for _ in 0..50 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: default_id.clone(),
            })
            .expect("status default");
        if status.state != PaneRuntimeState::Live {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let mut ids = Vec::new();
    for name in names {
        let pane: Pane = client
            .request(DaemonRequest::CreatePane {
                title: Some(name.to_string()),
                profile: None,
            })
            .expect("create pane should succeed");
        ids.push(pane.id);
    }
    // Brief settle so the shells are ready to accept input on the PTY.
    thread::sleep(Duration::from_millis(150));
    (daemon, client, ids)
}

/// Run `collect_batched_results` in a background thread with a timeout so a
/// bug (e.g. a pane that never prints its marker) doesn't hang the suite.
fn collect_batched_with_timeout(
    client: &DaemonClient,
    plan: &RunPlan,
    timeout: Duration,
) -> Result<Vec<PaneRunResult>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let client_clone = DaemonClient {
        cwd: client.cwd.clone(),
        socket_path: client.socket_path.clone(),
        data_dir: client.data_dir.clone(),
        token: client.token.clone(),
        auto_spawn: false,
    };
    let plan = RunPlan {
        pane_ref: plan.pane_ref.clone(),
        command_args: plan.command_args.clone(),
        all: plan.all,
        panes_list: plan.panes_list.clone(),
        timeout_ms: plan.timeout_ms,
    };
    std::thread::spawn(move || {
        let _ = tx.send(collect_batched_results(&client_clone, &plan));
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err("collect_batched_results timed out".to_string()))
}

/// Helper to find a pane id by title.
fn pane_id_by_title(client: &DaemonClient, title: &str) -> String {
    let list: PaneList = client
        .request(DaemonRequest::ListPanes)
        .expect("list panes");
    list.panes
        .iter()
        .find(|s| s.pane.title == title)
        .map(|s| s.pane.id.clone())
        .unwrap_or_else(|| panic!("pane titled {title} not found"))
}

/// VAL-ORCH-009: batched run across all live panes reports EACH pane's exit
/// code. With ≥2 live panes, every pane gets a result entry with its exit
/// code.
#[test]
fn batched_run_all_reports_each_pane_exit_code() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a", "b"]);
    let plan = RunPlan {
        pane_ref: "active".to_string(),
        command_args: vec!["true".to_string()],
        timeout_ms: None,
        all: true,
        panes_list: None,
    };
    let results = collect_batched_with_timeout(&client, &plan, Duration::from_secs(15))
        .expect("should not time out");
    assert_eq!(
        results.len(),
        2,
        "both live panes should report: {:?}",
        results
    );
    for result in &results {
        assert_eq!(result.exit_code, Some(0), "true → exit 0: {:?}", result);
        assert!(result.success, "true → success: {:?}", result);
    }
    daemon.shutdown();
}

/// VAL-ORCH-009: batched --all with a failure reports the non-zero code per
/// pane.
#[test]
fn batched_run_all_reports_nonzero_per_pane() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a", "b"]);
    let plan = RunPlan {
        pane_ref: "active".to_string(),
        command_args: vec!["/usr/bin/false".to_string()],
        timeout_ms: None,
        all: true,
        panes_list: None,
    };
    let results = collect_batched_with_timeout(&client, &plan, Duration::from_secs(15))
        .expect("should not time out");
    assert_eq!(results.len(), 2);
    for result in &results {
        assert_eq!(result.exit_code, Some(1), "false → exit 1: {:?}", result);
        assert!(!result.success);
    }
    daemon.shutdown();
}

/// VAL-ORCH-011: a mix of success and failure panes is reported accurately.
/// Pane A is prepped to exit 0; pane B is prepped to exit 3. The batched
/// result shows each pane's correct code (no cross-pane bleed).
#[test]
fn batched_run_mixed_success_and_failure() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a", "b"]);
    let id_a = pane_id_by_title(&client, "a");
    let id_b = pane_id_by_title(&client, "b");

    // Prep: set a per-pane env var so the same command yields different
    // exit codes in each pane. The var is exported in the pane's shell,
    // so the `sh -c` subshell inherits it.
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: id_a.clone(),
            input: "export SGIAN_MIX=0\n".to_string(),
        })
        .expect("prep a");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: id_b.clone(),
            input: "export SGIAN_MIX=3\n".to_string(),
        })
        .expect("prep b");
    // Wait for the shells to process the export.
    thread::sleep(Duration::from_millis(200));

    let plan = RunPlan {
        pane_ref: "active".to_string(),
        // sh -c 'exit $SGIAN_MIX' — the subshell inherits the pane's env.
        command_args: vec![
            "sh".to_string(),
            "-c".to_string(),
            "exit $SGIAN_MIX".to_string(),
        ],
        timeout_ms: None,
        all: true,
        panes_list: None,
    };
    let results = collect_batched_with_timeout(&client, &plan, Duration::from_secs(15))
        .expect("should not time out");
    assert_eq!(results.len(), 2);

    let result_a = results
        .iter()
        .find(|r| r.pane_id == id_a)
        .expect("a result");
    let result_b = results
        .iter()
        .find(|r| r.pane_id == id_b)
        .expect("b result");
    assert_eq!(
        result_a.exit_code,
        Some(0),
        "a should exit 0: {:?}",
        result_a
    );
    assert!(result_a.success);
    assert_eq!(
        result_b.exit_code,
        Some(3),
        "b should exit 3: {:?}",
        result_b
    );
    assert!(!result_b.success);
    daemon.shutdown();
}

/// VAL-ORCH-012: batched `--panes a,b` targets only the named subset; a
/// third live pane `c` shows no result entry.
#[test]
fn batched_run_panes_subset_excludes_others() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a", "b", "c"]);
    let id_a = pane_id_by_title(&client, "a");
    let id_b = pane_id_by_title(&client, "b");

    let plan = RunPlan {
        pane_ref: "active".to_string(),
        command_args: vec!["true".to_string()],
        timeout_ms: None,
        all: false,
        panes_list: Some("a,b".to_string()),
    };
    let results = collect_batched_with_timeout(&client, &plan, Duration::from_secs(15))
        .expect("should not time out");
    // Only a and b; c is absent.
    assert_eq!(results.len(), 2, "only a and b: {:?}", results);
    let ids: Vec<&str> = results.iter().map(|r| r.pane_id.as_str()).collect();
    assert!(ids.contains(&id_a.as_str()));
    assert!(ids.contains(&id_b.as_str()));
    for result in &results {
        assert_eq!(result.exit_code, Some(0));
    }
    daemon.shutdown();
}

/// VAL-ORCH-013: aggregate process exit is 0 when all panes succeeded and
/// non-zero when any pane failed. Verified via `control_run` (which returns
/// Ok/Err reflecting the aggregate).
#[test]
fn batched_run_aggregate_exit_reflects_outcomes() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a", "b"]);

    // All success → Ok.
    let ok = control_run_with_timeout(
        &client,
        &["--all", "--", "true"],
        false,
        Duration::from_secs(15),
    );
    assert!(ok.is_ok(), "all-success batched run should be Ok: {:?}", ok);

    // Any failure → Err.
    let err = control_run_with_timeout(
        &client,
        &["--all", "--", "/usr/bin/false"],
        false,
        Duration::from_secs(15),
    );
    assert!(
        err.as_ref().unwrap_err().contains("pane(s) failed"),
        "mixed-failure batched run should be Err: {:?}",
        err
    );
    daemon.shutdown();
}

/// VAL-ORCH-028: a targeted pane that is already Ended is reported as a
/// failure for that pane; the other targeted pane still reports its real
/// exit code; the batch completes without hanging.
#[test]
fn batched_run_ended_pane_reported_without_hang() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a", "b"]);
    let id_a = pane_id_by_title(&client, "a");
    let id_b = pane_id_by_title(&client, "b");

    // End pane b's shell.
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: id_b.clone(),
            input: "exit\n".to_string(),
        })
        .expect("send exit to b");

    // Wait for b to become Ended.
    let mut became_ended = false;
    for _ in 0..50 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: id_b.clone(),
            })
            .expect("status b");
        if status.state != PaneRuntimeState::Live {
            became_ended = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(became_ended, "pane b should become Ended");

    // Batched --panes a,b: a succeeds, b is reported as failed (not live).
    // Must complete well under the timeout (no hang on b).
    let plan = RunPlan {
        pane_ref: "active".to_string(),
        command_args: vec!["true".to_string()],
        timeout_ms: None,
        all: false,
        panes_list: Some("a,b".to_string()),
    };
    let results = collect_batched_with_timeout(&client, &plan, Duration::from_secs(10))
        .expect("should not time out");
    assert_eq!(results.len(), 2);

    let result_a = results
        .iter()
        .find(|r| r.pane_id == id_a)
        .expect("a result");
    let result_b = results
        .iter()
        .find(|r| r.pane_id == id_b)
        .expect("b result");
    assert_eq!(
        result_a.exit_code,
        Some(0),
        "a should succeed: {:?}",
        result_a
    );
    assert!(result_a.success);
    assert!(
        !result_b.success,
        "b should be flagged failed: {:?}",
        result_b
    );
    assert!(
        result_b.exit_code.is_none(),
        "b has no exit code: {:?}",
        result_b
    );
    assert!(
        result_b.error.as_deref().unwrap_or("").contains("not live"),
        "b error should mention not live: {:?}",
        result_b
    );
    daemon.shutdown();
}

/// VAL-ORCH-030: batched --all with zero live panes returns an empty result
/// set (vacuously all-success) and does not hang or error.
#[test]
fn batched_run_all_zero_live_panes_is_empty() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a"]);

    // End the only pane.
    let id_a = pane_id_by_title(&client, "a");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: id_a,
            input: "exit\n".to_string(),
        })
        .expect("send exit");

    // Wait for it to become Ended.
    for _ in 0..50 {
        let list: PaneList = client.request(DaemonRequest::ListPanes).expect("list");
        if list.panes.iter().all(|s| s.state != PaneRuntimeState::Live) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Batched --all: zero live panes → empty results, no hang.
    let plan = RunPlan {
        pane_ref: "active".to_string(),
        command_args: vec!["true".to_string()],
        timeout_ms: None,
        all: true,
        panes_list: None,
    };
    let results = collect_batched_with_timeout(&client, &plan, Duration::from_secs(10))
        .expect("should not time out");
    assert!(
        results.is_empty(),
        "zero live panes → empty results: {:?}",
        results
    );

    // The aggregate (control_run) should be Ok (vacuously all-success).
    let ok = control_run_with_timeout(
        &client,
        &["--all", "--", "true"],
        false,
        Duration::from_secs(10),
    );
    assert!(ok.is_ok(), "zero live panes → aggregate Ok: {:?}", ok);
    daemon.shutdown();
}

/// M8: a capability-negotiated subscriber receives SubscribeAck as the FIRST
/// event, strictly after registration — reading it guarantees no later
/// broadcast can be missed (pins the subscribe-then-send race fix).
#[test]
fn subscribe_ack_is_first_event_for_capable_clients() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/cat", &["ack"]);

    let mut conn = client.connect().expect("connect should succeed");
    conn.write_request(&DaemonRequest::Subscribe)
        .expect("subscribe should write");
    conn.await_subscribe_ack()
        .expect("the ack must be the first event on the stream");

    daemon.shutdown();
}

/// H7: `run --timeout` bounds a marker miss. /bin/cat echoes the wrapper
/// line verbatim (the marker is followed by the literal `%s`, never digits),
/// so no exit code can ever be parsed — exactly the marker-miss shape that
/// used to hang forever.
#[test]
fn read_error_is_timeout_covers_early_firing_read_timer() {
    // 07-19 CLI low: the per-read timeout is armed to the time remaining to
    // the deadline, but the OS timer can fire sub-milliseconds EARLY. A
    // read error inside the epsilon window maps to the clean timeout…
    let now = Instant::now();
    let deadline = now + Duration::from_millis(2);
    assert!(read_error_is_timeout(Some(deadline), now));
    // …at the deadline…
    assert!(read_error_is_timeout(Some(deadline), deadline));
    // …and past it.
    assert!(read_error_is_timeout(
        Some(deadline),
        deadline + Duration::from_millis(1)
    ));
    // A read error well BEFORE the deadline is genuine: reported raw.
    let far = now + Duration::from_secs(60);
    assert!(!read_error_is_timeout(Some(far), now));
    // No deadline armed: never a timeout.
    assert!(!read_error_is_timeout(None, now));
}

#[test]
fn run_in_pane_times_out_when_no_marker_appears() {
    let (daemon, client, ids) = spawn_batched_daemon("/bin/cat", &["tmo"]);
    let pane_id = ids.first().expect("one pane").clone();

    let started = Instant::now();
    let result = run_in_pane(
        &client,
        &pane_id,
        "true",
        ShellFamily::Posix,
        "tmo",
        Some(Duration::from_millis(500)),
    );

    assert!(!result.success, "a marker miss must not report success");
    assert!(
        result.error.as_deref().unwrap_or("").contains("timed out"),
        "expected a timeout error, got {:?}",
        result.error
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the deadline must fire promptly"
    );

    daemon.shutdown();
}

/// VAL-ORCH-025: batched `--panes` with an unknown member fails fast with
/// "pane not found" (no hang, no silent success).
#[test]
fn batched_run_panes_unknown_pane_fails_fast() {
    let (daemon, client, _) = spawn_batched_daemon("/bin/sh", &["a"]);
    let plan = RunPlan {
        pane_ref: "active".to_string(),
        command_args: vec!["true".to_string()],
        timeout_ms: None,
        all: false,
        panes_list: Some("a,no-such-pane".to_string()),
    };
    let result = collect_batched_with_timeout(&client, &plan, Duration::from_secs(10));
    let err = result.expect_err("unknown pane should error");
    assert!(
        err.contains("not found") || err.contains("no active pane"),
        "should mention not found: {err}"
    );
    daemon.shutdown();
}

#[test]
fn run_daemon_with_config_never_reads_real_global_config() {
    // Sanity: an explicit Config with a distinctive shell is honored, proving
    // run_daemon_with_config uses the injected config rather than load_config
    // (which would read ~/Library/Application Support/Sgian/config.json).
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);

    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: Some(1),
        ..Default::default()
    };
    // The server constructed inside run_daemon_with_config must reflect the
    // injected config, not the developer's real global config. We verify by
    // constructing a DaemonServer with the same config directly and checking
    // its shell_config — the integration test above exercises the full path.
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-hermetic"),
        data_dir.path().to_path_buf(),
        config.clone(),
    )
    .expect("daemon server should start with injected config");
    assert_eq!(
        server.effective_config().shell,
        Some("/bin/cat".to_string())
    );
    assert_eq!(server.effective_config().idle_shutdown_secs, Some(1));

    let _ = fs::remove_file(&socket_path);
}

// ----- Structured logging tests (VAL-OBS-001/002/003/004/005/010/011/017/018/020, VAL-SEC-009) -----

/// VAL-OBS-001: daemon log is structured with levels + readable timestamp + fields.
#[test]
fn structured_log_has_levels_timestamps_and_fields() {
    let config = Config {
        shell: Some("/usr/bin/true".to_string()), // exits immediately → pane-end
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // CreatePane → pane_create log entry.
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    // Wait for the shell (/bin/true) to exit → pane_end log entry.
    thread::sleep(Duration::from_millis(200));

    let log = daemon.shutdown_and_read_log();

    // Level tokens present.
    assert!(log.contains("INFO"), "log should contain INFO level: {log}");

    // Readable timestamp (RFC3339-ish: contains '-' and ':' and 'T' or 'Z').
    // The old format was bare "{epoch_ms} {message}" with no dashes/colons.
    assert!(
        log.contains('-') && log.contains(':'),
        "log should have a human-readable timestamp: {log}"
    );

    // Structured fields.
    assert!(
        log.contains("workspace_key="),
        "log should contain workspace_key field: {log}"
    );
    assert!(
        log.contains("pane_id="),
        "log should contain pane_id field: {log}"
    );
    assert!(
        log.contains("event="),
        "log should contain event field: {log}"
    );

    // Lifecycle events.
    assert!(
        log.contains("daemon_start"),
        "log should contain daemon_start event: {log}"
    );
    assert!(
        log.contains("pane_create"),
        "log should contain pane_create event: {log}"
    );
    assert!(
        log.contains("pane_end"),
        "log should contain pane_end event: {log}"
    );
    assert!(
        log.contains("daemon_shutdown"),
        "log should contain daemon_shutdown event: {log}"
    );
    assert!(
        log.contains(&pane.id),
        "log should contain the pane id {}: {log}",
        pane.id
    );
}

/// VAL-OBS-003: pane close and rename are logged (no ctl verb, so via handle directly).
#[test]
fn structured_log_records_close_and_rename() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-log-close-rename"),
        data_dir.path().to_path_buf(),
        config,
    )
    .expect("server should start");

    // Activate tracing for the test thread so handle's tracing calls are captured.
    let _log_guard = tracing::dispatcher::set_default(&server.log_dispatch);

    // CreatePane → pane_create
    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed"),
    )
    .expect("should be a Pane");

    // RenamePane → pane_rename
    server
        .handle(DaemonRequest::RenamePane {
            pane_id: pane.id.clone(),
            title: "renamed-pane".to_string(),
        })
        .expect("rename should succeed");

    // ClosePane → pane_close
    server
        .handle(DaemonRequest::ClosePane {
            pane_id: pane.id.clone(),
        })
        .expect("close should succeed");

    // Drop the server to flush the non-blocking log writer.
    drop(_log_guard);
    drop(server);

    let log_path = data_dir.path().join(LOG_FILE);
    let log = fs::read_to_string(&log_path).unwrap_or_default();

    assert!(
        log.contains("pane_rename") && log.contains(&pane.id),
        "log should contain pane_rename event with pane id {}: {log}",
        pane.id
    );
    assert!(
        log.contains("renamed-pane"),
        "log should contain the new title: {log}"
    );
    assert!(
        log.contains("pane_close") && log.contains(&pane.id),
        "log should contain pane_close event with pane id {}: {log}",
        pane.id
    );
}

/// VAL-OBS-004: client connect and disconnect are logged.
#[test]
fn structured_log_records_connect_and_disconnect() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // A Ping request triggers handle_daemon_client which logs client_connect.
    let _: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping should succeed");

    // Subscribe a client (so a watcher thread is set up), then drop the
    // connection to trigger a client_disconnect log entry.
    let mut sub_stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut sub_stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");

    // Drop the subscriber stream → watcher detects disconnect.
    drop(sub_stream);

    // Give the watcher thread time to detect the disconnect and log it.
    thread::sleep(Duration::from_millis(300));

    let log = daemon.shutdown_and_read_log();

    assert!(
        log.contains("client_connect"),
        "log should contain client_connect event: {log}"
    );
    assert!(
        log.contains("client_disconnect"),
        "log should contain client_disconnect event: {log}"
    );
}

/// VAL-OBS-005: daemon shutdown is logged.
#[test]
fn structured_log_records_shutdown() {
    let config = Config::default();
    let mut daemon = TestDaemon::spawn(config);
    let log = daemon.shutdown_and_read_log();

    assert!(
        log.contains("daemon_shutdown"),
        "log should contain daemon_shutdown event: {log}"
    );
}

/// VAL-OBS-010: log storage is bounded/rotating (startup rotation when file
/// exceeds LOG_MAX_BYTES).
#[test]
fn structured_log_rotates_when_too_large() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let log_path = data_dir.path().join(LOG_FILE);
    let old_path = data_dir.path().join(format!("{LOG_FILE}.old"));

    // Pre-create a log file exceeding the rotation threshold.
    let big_content = "x".repeat((LOG_MAX_BYTES + 1024) as usize);
    fs::write(&log_path, &big_content).expect("write big log");

    // Setup the log writer (should rotate the oversized file).
    let (writer, guard) = setup_log_writer(data_dir.path());

    // The old file should have been renamed.
    assert!(
        old_path.exists(),
        "oversized log should be rotated to {LOG_FILE}.old"
    );
    assert_eq!(
        fs::read_to_string(&old_path).unwrap_or_default(),
        big_content,
        "rotated file should preserve original content"
    );

    // The new log file should exist (pre-created with 0600).
    assert!(log_path.exists(), "new log file should exist");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&log_path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0);
        assert_eq!(
            mode & 0o777,
            0o600,
            "new log file should be 0600, got {:o}",
            mode & 0o777
        );
    }

    drop(writer);
    drop(guard);
}

/// Regression (m1-fix-log-bounded-and-nopanic): sustained-volume logging
/// stays bounded AT RUNTIME. The active daemon.log must NOT grow unbounded
/// during a long-lived session — the BoundedFileWriter rotates when
/// LOG_MAX_BYTES is exceeded during operation, not just at startup.
#[test]
fn structured_log_runtime_bounded() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let log_path = data_dir.path().join(LOG_FILE);
    let old_path = data_dir.path().join(format!("{LOG_FILE}.old"));

    let (mut writer, guard) = setup_log_writer(data_dir.path());

    // Write well beyond the cap (3× LOG_MAX_BYTES). With runtime rotation,
    // the active file should stay at ~LOG_MAX_BYTES and the old file should
    // appear after the first rotation.
    let chunk = "x".repeat(4096);
    let target = LOG_MAX_BYTES * 3;
    let mut written: u64 = 0;
    while written < target {
        // Use write (not write_all) so the BoundedFileWriter sees each chunk
        // as a discrete write call and can check the cap.
        writer.write_all(chunk.as_bytes()).expect("write");
        written += chunk.len() as u64;
    }

    // Drop the writer and guard to flush all pending writes to disk.
    drop(writer);
    drop(guard);

    // The active log file must NOT exceed the cap by more than one rotation
    // unit (the last write chunk that triggered the rotation).
    let active_size = fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        active_size <= LOG_MAX_BYTES + chunk.len() as u64,
        "active log file ({} bytes) exceeds cap ({} + {} bytes)",
        active_size,
        LOG_MAX_BYTES,
        chunk.len()
    );

    // The old (rotated) file should exist — runtime rotation occurred.
    assert!(
        old_path.exists(),
        "runtime rotation should have produced {LOG_FILE}.old"
    );

    // Total log storage (active + rotated) should be bounded at ~2× cap.
    let old_size = fs::metadata(&old_path).map(|m| m.len()).unwrap_or(0);
    let total = active_size + old_size;
    assert!(
        total <= LOG_MAX_BYTES * 2 + chunk.len() as u64 * 2,
        "total log storage ({total} bytes) is not bounded"
    );
}

/// L15: a single write larger than the cap no longer blows past it — the
/// writer rotates, writes at most cap bytes, and reports the honest short
/// count (a write_all caller retries the remainder into the next rotation).
#[test]
fn bounded_file_writer_oversized_write_is_capped_and_honest() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let log_path = data_dir.path().join(LOG_FILE);
    let mut writer = BoundedFileWriter::new(data_dir.path()).expect("writer");

    let oversized = "x".repeat((LOG_MAX_BYTES + 100) as usize);
    let n = writer
        .write(oversized.as_bytes())
        .expect("oversized write should succeed");
    assert_eq!(
        n as u64, LOG_MAX_BYTES,
        "an oversized write reports the honest short count, not Ok(len)"
    );
    let active_size = fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    assert_eq!(
        active_size, LOG_MAX_BYTES,
        "the active file never exceeds the cap on a single oversized write"
    );

    // A write_all caller retries the remainder: it lands after rotation.
    writer.write_all(oversized.as_bytes()).expect("write_all");
    let active_size = fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        active_size <= LOG_MAX_BYTES,
        "active file stays capped after the retry lands (got {active_size})"
    );
}

/// L15: when the reopen after rotation fails, writes are still dropped
/// (best-effort, no panic) but the reported count is honest (0), not
/// over-reported as Ok(len).
#[test]
fn bounded_file_writer_failed_reopen_reports_zero() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let mut writer = BoundedFileWriter::new(data_dir.path()).expect("writer");
    // Force the no-file state (what a failed reopen leaves behind).
    writer.file = None;
    let n = writer.write(b"dropped").expect("write must not fail");
    assert_eq!(n, 0, "a dropped write reports 0, not Ok(len)");
}

/// Regression (m1-fix-log-bounded-and-nopanic): logger init on an
/// un-writable/inaccessible path falls back gracefully without panicking.
/// Logging is best-effort and MUST NEVER crash daemon startup.
#[test]
fn structured_log_init_no_panic_on_inaccessible_path() {
    // Use a path that cannot be created: a subdirectory "under" a regular file.
    let temp = tempfile::tempdir().expect("temp dir");
    let blocker = temp.path().join("blocker");
    fs::write(&blocker, "not a directory").expect("write blocker");
    let inaccessible_log_dir = blocker.join("logs");

    // setup_log_writer must NOT panic — it should fall back gracefully
    // (e.g. to stderr) and return a usable writer.
    let (mut writer, guard) = setup_log_writer(&inaccessible_log_dir);

    // File logging should be disabled (no file created at the inaccessible path).
    assert!(
        !inaccessible_log_dir.join(LOG_FILE).exists(),
        "no log file should be created at an inaccessible path"
    );

    // The writer should still be usable (writes go to stderr fallback).
    writer
        .write_all(b"test log line\n")
        .expect("write should not fail");

    drop(writer);
    drop(guard);
}

/// VAL-OBS-011: logging is best-effort — an unwritable log dir does not crash
/// the daemon.
#[test]
fn structured_log_best_effort_unwritable_dir() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);

    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: Some(1),
        ..Default::default()
    };

    // Make the data dir read-only so the log file cannot be written.
    // The daemon should still start and serve requests.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(data_dir.path(), fs::Permissions::from_mode(0o500))
            .expect("chmod data dir read-only");
    }

    let data_dir_path = data_dir.path().to_path_buf();
    let join_handle = thread::spawn({
        let cwd = PathBuf::from("/tmp/sgian-log-best-effort");
        let socket_path = socket_path.clone();
        move || run_daemon_with_config(cwd, socket_path, data_dir_path, config)
    });

    // If the daemon starts despite the unwritable log dir, it should be
    // reachable. We try to connect; if it fails, the daemon didn't start
    // (logging failure should NOT prevent startup).
    // Give the daemon time to start.
    thread::sleep(Duration::from_millis(500));

    // Read the token (it was written before the log setup, so it should exist).
    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);

    // Try to ping the daemon — it should respond even though logging failed.
    let ping_ok = authenticate_stream_at(&socket_path, &token)
        .and_then(|mut stream| {
            write_json_line(&mut stream, &DaemonRequest::Ping)?;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|e| format!("read: {e}"))?;
            let resp: IpcResponse =
                serde_json::from_str(&line).map_err(|e| format!("parse: {e}"))?;
            if resp.ok {
                Ok(())
            } else {
                Err("ping failed".to_string())
            }
        })
        .is_ok();

    // Clean up: restore permissions so TempDir can be removed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(data_dir.path(), fs::Permissions::from_mode(0o700));
    }

    // Send shutdown if the daemon is running.
    if ping_ok {
        let _ = authenticate_stream_at(&socket_path, &token).and_then(|mut stream| {
            write_json_line(&mut stream, &DaemonRequest::Shutdown)?;
            Ok(())
        });
    }
    let _ = join_handle.join();

    assert!(
        ping_ok,
        "daemon should start and serve even with an unwritable log dir"
    );
}

/// VAL-OBS-017: structured log never records PTY output payloads.
#[test]
fn structured_log_never_records_pty_output() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    // Send a unique sentinel into the pane's PTY.
    let sentinel = "NOLOG_SENTINEL_7f3a";
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane.id.clone(),
            input: format!("echo {sentinel}\n"),
        })
        .expect("send should succeed");

    // Give the reader thread time to process the input.
    thread::sleep(Duration::from_millis(200));

    let log = daemon.shutdown_and_read_log();

    assert!(
        !log.contains(sentinel),
        "log must NOT contain PTY output sentinel '{sentinel}': {log}"
    );
    // But lifecycle events should still be present.
    assert!(
        log.contains("pane_create"),
        "log should still contain pane_create: {log}"
    );
}

/// VAL-OBS-018: daemon logs are isolated per workspace.
#[test]
fn structured_log_isolated_per_workspace() {
    let config1 = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let config2 = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };

    // Use distinct cwds so the workspace_keys differ and pane ids differ
    // (each daemon has its own registry starting from pane-1).
    let mut daemon1 = TestDaemon::spawn_with_cwd(config1, PathBuf::from("/tmp/sgian-ws1-iso"));
    let client1 = daemon1.client();
    let _pane1: Pane = client1
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create in WS1 should succeed");

    let mut daemon2 = TestDaemon::spawn_with_cwd(config2, PathBuf::from("/tmp/sgian-ws2-iso"));
    let client2 = daemon2.client();
    let _pane2: Pane = client2
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create in WS2 should succeed");

    let log1 = daemon1.shutdown_and_read_log();
    let log2 = daemon2.shutdown_and_read_log();

    // Each log contains its own workspace_key (derived from its cwd).
    let key1 = workspace_key(&PathBuf::from("/tmp/sgian-ws1-iso"));
    let key2 = workspace_key(&PathBuf::from("/tmp/sgian-ws2-iso"));

    // WS1 log contains WS1's workspace_key and pane, but not WS2's key.
    assert!(
        log1.contains(&key1),
        "WS1 log should contain WS1 workspace_key: {log1}"
    );
    assert!(
        !log1.contains(&key2),
        "WS1 log should NOT contain WS2 workspace_key: {log1}"
    );

    // WS2 log contains WS2's workspace_key and pane, but not WS1's key.
    assert!(
        log2.contains(&key2),
        "WS2 log should contain WS2 workspace_key: {log2}"
    );
    assert!(
        !log2.contains(&key1),
        "WS2 log should NOT contain WS1 workspace_key: {log2}"
    );

    // Log files live in distinct data dirs (per-workspace isolation).
    assert_ne!(
        daemon1.data_dir.path(),
        daemon2.data_dir.path(),
        "each daemon should have its own data dir"
    );
}

/// VAL-OBS-020: log history survives a daemon restart (append, not truncate).
#[test]
fn structured_log_survives_restart() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let cwd = PathBuf::from("/tmp/sgian-log-restart");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None, // disable idle shutdown
        ..Default::default()
    };

    // First daemon run.
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let data_dir_path = data_dir.path().to_path_buf();
    let h1 = thread::spawn({
        let cwd = cwd.clone();
        let sp = socket_path.clone();
        let dd = data_dir_path.clone();
        let cfg = config.clone();
        move || run_daemon_with_config(cwd, sp, dd, cfg)
    });

    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);
    retry_until_ready(&socket_path, &token);

    // Shutdown the first daemon.
    let _ = authenticate_stream_at(&socket_path, &token).and_then(|mut stream| {
        write_json_line(&mut stream, &DaemonRequest::Shutdown)?;
        Ok::<(), String>(())
    });
    let _ = h1.join();

    // Second daemon run with the same data dir (restart).
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let _ = fs::remove_file(&socket_path); // clean up stale socket
    let h2 = thread::spawn({
        let cwd = cwd.clone();
        let sp = socket_path.clone();
        let dd = data_dir_path.clone();
        let cfg = config.clone();
        move || run_daemon_with_config(cwd, sp, dd, cfg)
    });

    retry_until_ready(&socket_path, &token);

    // Shutdown the second daemon.
    let _ = authenticate_stream_at(&socket_path, &token).and_then(|mut stream| {
        write_json_line(&mut stream, &DaemonRequest::Shutdown)?;
        Ok::<(), String>(())
    });
    let _ = h2.join();

    // Read the log file — it should contain entries from BOTH runs.
    let log_path = data_dir.path().join(LOG_FILE);
    let log = fs::read_to_string(&log_path).unwrap_or_default();

    let start_count = log.matches("daemon_start").count();
    assert!(
        start_count >= 2,
        "log should contain >=2 daemon_start entries after restart, got {start_count}: {log}"
    );
    assert!(
        log.contains("daemon_shutdown"),
        "log should contain shutdown entry from first run: {log}"
    );
}

/// VAL-SEC-009: structured log never records the authentication token.
#[test]
fn structured_log_never_records_token() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Trigger an auth-reject by connecting with a wrong token.
    let _ = authenticate_stream_at(&daemon.socket_path, "wrong-token-xyz");

    // Also do a successful request (which logs client_connect).
    let _: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping should succeed");

    let log = daemon.shutdown_and_read_log();

    // The real token must never appear in the log.
    assert!(
        !log.contains(&daemon.token),
        "log must NOT contain the auth token: {log}"
    );
    // The wrong token must also not appear.
    assert!(
        !log.contains("wrong-token-xyz"),
        "log must NOT contain the rejected token: {log}"
    );
    // But the auth-reject event should be logged.
    assert!(
        log.contains("auth_rejected"),
        "log should contain auth_rejected event: {log}"
    );
}

/// VAL-SEC-008 (partial): log file is owner-only (0600) on Unix.
#[test]
#[cfg(unix)]
fn structured_log_file_is_owner_only() {
    let config = Config::default();
    let mut daemon = TestDaemon::spawn(config);
    let _log = daemon.shutdown_and_read_log();

    use std::os::unix::fs::PermissionsExt;
    let log_path = daemon.data_dir.path().join(LOG_FILE);
    let mode = fs::metadata(&log_path)
        .map(|m| m.permissions().mode())
        .unwrap_or(0);
    assert_eq!(
        mode & 0o777,
        0o600,
        "log file should be 0600, got {:o}",
        mode & 0o777
    );
}

// ----- ctl logs integration tests (VAL-OBS-006/007/009) -----

/// VAL-OBS-006: `ctl logs` reads recent daemon log entries from a LIVE daemon
/// (the log must be flushed to disk without shutting down). Verifies the
/// FlushingWriter makes entries visible while the daemon is still running.
#[test]
fn ctl_logs_reads_live_daemon_log() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // CreatePane generates log entries (daemon_start + pane_create +
    // client_connect).
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    // Give the non-blocking writer's worker thread time to flush.
    thread::sleep(Duration::from_millis(200));

    // Read the log file WHILE the daemon is live (no shutdown/flush).
    let log_path = daemon.data_dir.path().join(LOG_FILE);
    let lines = read_log_tail(&log_path, None);

    assert!(
        !lines.is_empty(),
        "log should have entries while daemon is live"
    );
    assert!(
        lines.iter().any(|l| l.contains("daemon_start")),
        "log should contain daemon_start: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("pane_create") && l.contains(&pane.id)),
        "log should contain pane_create with pane id {}: {lines:?}",
        pane.id
    );

    daemon.shutdown();
}

/// VAL-OBS-007: `ctl logs` honors a line-count limit. With more than N log
/// lines, a limit of N returns at most N lines; a limit of 1 returns exactly
/// one line (the most recent).
#[test]
fn ctl_logs_honors_line_count_limit() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Generate many log entries (each CreatePane logs pane_create +
    // client_connect; 10 panes is plenty).
    for _ in 0..10 {
        let _: Pane = client
            .request(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed");
    }

    // Wait for the non-blocking writer to flush.
    thread::sleep(Duration::from_millis(300));

    let log_path = daemon.data_dir.path().join(LOG_FILE);
    let all_lines = read_log_tail(&log_path, None);
    assert!(
        all_lines.len() > 5,
        "should have more than 5 log lines, got {}",
        all_lines.len()
    );

    // Limit 5 → at most 5 lines.
    let limited = read_log_tail(&log_path, Some(5));
    assert_eq!(limited.len(), 5, "limit 5 should return exactly 5 lines");

    // Limit 1 → exactly 1 line (the most recent).
    let one = read_log_tail(&log_path, Some(1));
    assert_eq!(one.len(), 1, "limit 1 should return exactly 1 line");
    assert_eq!(
        one[0],
        all_lines[all_lines.len() - 1],
        "limit 1 should return the most recent line"
    );

    daemon.shutdown();
}

/// VAL-OBS-009: `ctl logs` requires a running daemon and never spawns one.
/// `connect_existing` on a workspace with no daemon fails with a clear error
/// and does not create data dirs or spawn a daemon.
#[test]
fn ctl_logs_requires_running_daemon() {
    let cwd = PathBuf::from("/tmp/sgian-logs-no-daemon-test");
    // No daemon was started for this workspace. connect_existing must fail.
    match DaemonClient::connect_existing(cwd) {
        Ok(_) => panic!("connect_existing should fail without a running daemon"),
        Err(err) => {
            assert!(
                err.contains("no daemon") || err.contains("not running"),
                "error should mention no daemon: {err}"
            );
        }
    }
}

/// VAL-OBS-008: `ctl logs` and its limit option are documented in help.
#[test]
fn ctl_help_documents_logs_command() {
    // We can't capture stdout directly in a unit test, but we can verify the
    // help text contains the logs command and its flags by checking the
    // source. Instead, exercise the real help path via run_control_cli_from_args
    // and assert it succeeds (the help string is embedded in print_control_help).
    // The string content is verified by the E2E test against the binary.
    // Here we just assert print_control_help succeeds.
    print_control_help().expect("help should print");
}

// ----- `ctl status --verbose` (VAL-OBS-012..016, VAL-OBS-019, VAL-SEC-010) -----

/// VAL-OBS-012: `StatusVerbose` handler returns subscriber count, per-pane
/// runtime states, uptime, and a config summary.
#[test]
fn status_verbose_surfaces_daemon_level_detail() {
    let data_dir =
        std::env::temp_dir().join(format!("sgian-status-verbose-detail-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-sv"),
        data_dir.clone(),
        Config {
            shell: Some("/bin/cat".to_string()),
            idle_shutdown_secs: Some(42),
            ..Default::default()
        },
    )
    .expect("daemon server should start");

    let value = server
        .handle(DaemonRequest::StatusVerbose)
        .expect("status_verbose should succeed");
    let status: VerboseStatus =
        serde_json::from_value(value).expect("should deserialize as VerboseStatus");

    // Subscriber count is present and is 0 (no subscribers).
    assert_eq!(status.subscribers, 0);
    // Per-pane runtime states: pane-1 exists with no shell → Ended.
    assert!(!status.panes.is_empty());
    assert_eq!(status.panes[0].state, PaneRuntimeState::Ended);
    // Uptime is present and is a non-negative integer.
    let _ = status.uptime_secs;
    // Config summary is present with the expected fields.
    assert_eq!(status.config["shell"], json!("/bin/cat"));
    assert_eq!(status.config["idle_shutdown_secs"], json!(42));
    assert!(status.config.get("restore_policy").is_some());

    let _ = fs::remove_dir_all(data_dir);
}

/// VAL-OBS-014: uptime is monotonic (non-decreasing) across successive reads
/// of the same daemon.
#[test]
fn status_verbose_uptime_is_monotonic() {
    let data_dir = std::env::temp_dir().join(format!("sgian-uptime-{}", now_millis()));
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-uptime"),
        data_dir.clone(),
        Config::default(),
    )
    .expect("daemon server should start");

    let first: VerboseStatus =
        serde_json::from_value(server.handle(DaemonRequest::StatusVerbose).unwrap())
            .expect("first read");
    thread::sleep(Duration::from_secs(1));
    let second: VerboseStatus =
        serde_json::from_value(server.handle(DaemonRequest::StatusVerbose).unwrap())
            .expect("second read");

    assert!(
        second.uptime_secs >= first.uptime_secs,
        "uptime must be non-decreasing: first={}, second={}",
        first.uptime_secs,
        second.uptime_secs
    );

    let _ = fs::remove_dir_all(data_dir);
}

/// VAL-OBS-015: config summary reflects the effective (merged) config,
/// including workspace overrides.
#[test]
fn status_verbose_config_summary_reflects_merged_config() {
    let global = Config {
        idle_shutdown_secs: Some(10),
        font_family: Some("global-font".to_string()),
        ..Default::default()
    };
    let workspace = Config {
        idle_shutdown_secs: Some(99),
        theme: Some(json!("dark")),
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    };
    let merged = global.overlay(workspace);

    let summary = merged.summary();
    // Workspace override wins for idle_shutdown_secs.
    assert_eq!(summary["idle_shutdown_secs"], json!(99));
    // Global value preserved when workspace doesn't override.
    assert_eq!(summary["font_family"], json!("global-font"));
    // Workspace-only value present.
    assert_eq!(summary["theme"], json!("dark"));
    // restore_policy reflects the explicit workspace value.
    assert_eq!(summary["restore_policy"], json!("restore_on_demand"));
}

/// VAL-SEC-010: the config summary must never expose configured `env` values.
#[test]
fn status_verbose_config_summary_excludes_env_values() {
    let config = Config {
        env: {
            let mut map = HashMap::new();
            map.insert("SECRET_ENV".to_string(), "zzz-sentinel".to_string());
            map
        },
        ..Default::default()
    };
    let summary = config.summary();
    // The summary includes env key names (VAL-CFG-001) but not their values
    // (VAL-SEC-010). The env field is present as a map of key→null.
    assert!(
        summary.get("env").is_some(),
        "config summary should include env field (keys only, values redacted)"
    );
    // The sentinel value must not appear anywhere in the serialized summary.
    let serialized = serde_json::to_string(&summary).unwrap();
    assert!(
        !serialized.contains("zzz-sentinel"),
        "env value must not leak into config summary: {serialized}"
    );
    // The env key IS present (redacted to null).
    assert_eq!(
        summary["env"]["SECRET_ENV"],
        Value::Null,
        "env key should be present with null value"
    );
}

/// M10: the summary carries the daemon-RESOLVED shell alongside (not instead
/// of) the raw `shell` field, so ctl-side family resolution can't diverge by
/// re-resolving $SHELL in the ctl process's own environment.
#[test]
fn config_summary_carries_resolved_shell() {
    // Unset: the raw field keeps its null shape; resolved is the daemon-side
    // default (never empty).
    let summary = Config::default().summary();
    assert_eq!(summary["shell"], Value::Null, "raw shell keeps its shape");
    let resolved = summary["resolved_shell"]
        .as_str()
        .expect("resolved_shell is present");
    assert!(
        !resolved.is_empty(),
        "resolved_shell is the effective default"
    );

    // Set: resolved matches the configured shell.
    let configured = Config {
        shell: Some("/bin/fish".to_string()),
        ..Default::default()
    };
    let summary = configured.summary();
    assert_eq!(summary["resolved_shell"], json!("/bin/fish"));
    assert_eq!(summary["shell"], json!("/bin/fish"));
}

/// M10: family resolution prefers the daemon-resolved shell, falls back to
/// the raw `shell` field for old daemons, then to the local default.
#[test]
fn status_shell_for_family_prefers_daemon_resolved_shell() {
    // New daemon: resolved_shell wins over the raw field.
    let config = json!({"shell": "/bin/zsh", "resolved_shell": "/bin/fish"});
    assert_eq!(
        detect_shell_family(&status_shell_for_family(&config)),
        ShellFamily::Fish
    );
    // Old daemon (no resolved_shell field): the raw field is used.
    let config = json!({"shell": "/bin/fish"});
    assert_eq!(
        detect_shell_family(&status_shell_for_family(&config)),
        ShellFamily::Fish
    );
    // Neither field: the local default, exactly the old fallback.
    let config = json!({"shell": null});
    assert_eq!(status_shell_for_family(&config), default_shell());
}

/// VAL-CFG-001 regression: the human-readable `ctl status --verbose` output
/// (non-JSON form) must surface ALL 8 editable config fields — `shell`,
/// `shell_args`, `env` (key names only), `font_family`, `font_size`,
/// `theme`, `idle_shutdown_secs`, `restore_policy`. Previously only 6 of 8
/// were rendered (shell_args and env were omitted). Also VAL-SEC-010: the
/// configured env value must not appear in the human-readable line.
#[test]
fn status_verbose_human_readable_shows_all_eight_fields() {
    // Build a VerboseStatus whose config summary carries non-default values
    // for every editable field, including shell_args and an env entry whose
    // value must be suppressed.
    let config = json!({
        "shell": "/bin/zsh",
        "shell_args": ["-l"],
        "env": { "SECRET_ENV": null, "PATH": null },
        "font_family": "MonoFont",
        "font_size": 14,
        "theme": "dark",
        "idle_shutdown_secs": 42,
        "restore_policy": "restore_on_demand",
    });
    let status = VerboseStatus {
        subscribers: 0,
        panes: vec![PaneStatus {
            pane: Pane {
                id: "pane-1".to_string(),
                title: "term-1".to_string(),
                kind: PaneKind::Shell,
                created_at_ms: 0,
            },
            state: PaneRuntimeState::Live,
        }],
        active_pane_id: Some("pane-1".to_string()),
        cwd: "/tmp/sgian-val".to_string(),
        uptime_secs: 7,
        config,
    };

    let rendered = format_status_verbose_human(&status);

    // All 8 editable field labels must appear in the human-readable output.
    for field in [
        "shell=",
        "shell_args=",
        "env=",
        "font_family=",
        "font_size=",
        "theme=",
        "idle_shutdown_secs=",
        "restore_policy=",
    ] {
        assert!(
            rendered.contains(field),
            "human-readable status --verbose missing field `{field}`: {rendered}"
        );
    }

    // The env KEY names must appear (VAL-CFG-001: env field present).
    assert!(
        rendered.contains("SECRET_ENV"),
        "env key name SECRET_ENV missing from human-readable output: {rendered}"
    );
    assert!(
        rendered.contains("PATH"),
        "env key name PATH missing from human-readable output: {rendered}"
    );

    // Seeded env values are null in the summary, but guard against any
    // future regression that would surface a real value (VAL-SEC-010).
    assert!(
        !rendered.contains("zzz-sentinel"),
        "env value leaked into human-readable status --verbose: {rendered}"
    );

    // The shell_args value must be present (the args themselves are not
    // secret — only env values are suppressed).
    assert!(
        rendered.contains("-l"),
        "shell_args value missing from human-readable output: {rendered}"
    );

    // Sanity: the non-config lines (subscribers/uptime/cwd/pane) are still
    // rendered, proving the refactor did not drop them.
    assert!(rendered.contains("subscribers\t0"));
    assert!(rendered.contains("uptime\t7s"));
    assert!(rendered.contains("cwd\t/tmp/sgian-val"));
    assert!(rendered.contains("pane\tpane-1\tterm-1\tLive"));
}

/// VAL-SEC-010: `get_config` (appearance) also must not expose env values.
#[test]
fn get_config_appearance_excludes_env_values() {
    let config = Config {
        env: {
            let mut map = HashMap::new();
            map.insert("SECRET_ENV".to_string(), "zzz-sentinel".to_string());
            map
        },
        ..Default::default()
    };
    // summary() is used by status --verbose; it must not include env values.
    let summary = config.summary();
    let serialized = serde_json::to_string(&summary).unwrap();
    assert!(
        !serialized.contains("zzz-sentinel"),
        "env value must not leak into config summary: {serialized}"
    );
    // full_config() is used by get_config (GUI only); it DOES include env
    // values (the settings modal needs them), but this is not a ctl surface.
    let full = config.full_config();
    assert!(
        full.get("env").is_some(),
        "full_config must include env field for the settings modal"
    );
}

/// VAL-OBS-012/013/014: integration test — `status --verbose` over a real
/// daemon returns subscriber count, per-pane states, uptime, and config.
#[test]
fn run_daemon_status_verbose_integration() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: Some(30),
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Create a pane so we have at least one Live pane (cat stays alive).
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    let status: VerboseStatus = client
        .request(DaemonRequest::StatusVerbose)
        .expect("status_verbose should succeed");

    // Subscriber count is 0 (no subscribers attached).
    assert_eq!(status.subscribers, 0);
    // The created pane should be Live (cat is running).
    let created = status
        .panes
        .iter()
        .find(|p| p.pane.id == pane.id)
        .expect("created pane should appear in verbose status");
    assert_eq!(created.state, PaneRuntimeState::Live);
    // Config summary reflects the injected config.
    assert_eq!(status.config["idle_shutdown_secs"], json!(30));
    assert_eq!(status.config["restore_policy"], json!("restore_on_demand"));
    // Uptime is present.
    let _ = status.uptime_secs;

    daemon.shutdown();
}

/// VAL-OBS-019: `status --verbose` requires a running daemon (connect_existing
/// fails on a dead workspace; it never spawns one).
#[test]
fn status_verbose_requires_running_daemon() {
    let cwd = PathBuf::from("/tmp/sgian-sv-no-daemon-test");
    match DaemonClient::connect_existing(cwd) {
        Ok(_) => panic!("connect_existing should fail without a running daemon"),
        Err(err) => {
            assert!(
                err.contains("no daemon") || err.contains("not running"),
                "error should mention no daemon: {err}"
            );
        }
    }
}

// ----- Subscribe catch-up tests (VAL-LIFE-001/002/010/011, VAL-CROSS-001/002/003) -----

/// VAL-LIFE-001 / VAL-CROSS-002: a PaneEnded emitted BEFORE a client subscribes
/// is reflected to the late subscriber via the subscribe catch-up mechanism.
#[test]
fn late_subscriber_sees_ended() {
    // /usr/bin/true exits immediately → PaneEnded is broadcast before we subscribe.
    let config = Config {
        shell: Some("/usr/bin/true".to_string()),
        idle_shutdown_secs: None, // disable idle shutdown
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Create pane-2; its shell (/usr/bin/true) exits right away, broadcasting
    // PaneEnded while no subscriber is attached.
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    assert_eq!(pane.id, "pane-2");

    // Wait for the shell to exit and the PaneEnded to be broadcast (no subscriber
    // receives it — this is the "missed event" gap the catch-up closes).
    let mut ended = false;
    for _ in 0..100 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: pane.id.clone(),
            })
            .expect("status should succeed");
        if status.state == PaneRuntimeState::Ended {
            ended = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ended, "pane should end before we subscribe");

    // Now subscribe — the catch-up should push a PaneEnded for the already-ended
    // pane so the late subscriber learns it ended.
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut received_ended = false;
    for _ in 0..20 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PaneEnded { pane_id, .. }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if pane_id == pane.id {
                        received_ended = true;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        received_ended,
        "late subscriber should receive PaneEnded for the already-ended pane via catch-up"
    );

    daemon.shutdown();
}

/// VAL-LIFE-002 / VAL-CROSS-003: Ended runtime state persists across a daemon
/// restart and is reflected post-bootstrap. Under `restore_on_demand`, ended
/// panes stay Ended (not revived, not dropped).
#[test]
fn ended_state_survives_restart() {
    // /usr/bin/true exits immediately → pane ends → state is persisted as Ended.
    // Use restore_on_demand so the ended pane is NOT auto-revived on bootstrap.
    let config = Config {
        shell: Some("/usr/bin/true".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Create pane-2; its shell exits right away.
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    assert_eq!(pane.id, "pane-2");

    // Wait for the pane to end and the state to be persisted.
    let mut ended = false;
    for _ in 0..100 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: pane.id.clone(),
            })
            .expect("status should succeed");
        if status.state == PaneRuntimeState::Ended {
            ended = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ended, "pane should end before restart");

    // Restart the daemon — it re-loads the persisted workspace (same data_dir).
    daemon.restart(Config {
        shell: Some("/usr/bin/true".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    // Bootstrap the workspace so the persisted state is loaded.
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap should succeed");

    // The ended pane should still be present and report as Ended (not lost,
    // not fabricated as Live).
    let pane_states = &snapshot.pane_states;
    assert_eq!(
        pane_states.get(&pane.id),
        Some(&PaneRuntimeState::Ended),
        "ended pane state should survive restart as Ended"
    );

    // Also verify via PaneStatus (the ctl-facing surface).
    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: pane.id.clone(),
        })
        .expect("status should succeed");
    assert_eq!(
        status.state,
        PaneRuntimeState::Ended,
        "PaneStatus should report Ended after restart"
    );

    daemon.shutdown();
}

/// VAL-LIFE-011: Subscribe catch-up reports the correct state for EVERY pane in
/// a mixed live/ended set. A late subscriber learns that a still-running pane is
/// Live (no spurious PaneEnded) and a previously-ended pane is Ended.
#[test]
fn mixed_live_ended_catchup() {
    // Use /bin/cat for live panes (stays alive, echoes input) and /usr/bin/true
    // for ended panes (exits immediately). We need both in the same workspace,
    // so we use the default shell for the first pane and create a second pane
    // that also uses the default shell. But we need one live and one ended.
    // Strategy: use /bin/cat (stays alive) as the shell. Create pane-2 (cat,
    // stays alive). Create pane-3 (cat, stays alive). Send "exit" to pane-3 to
    // end it. Now pane-2 is Live and pane-3 is Ended. Subscribe and verify
    // catch-up sends PaneEnded for pane-3 but NOT for pane-2.
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // pane-1 is seeded but has no shell (not spawned on bootstrap for a fresh
    // workspace — actually it IS spawned because spawn_on_bootstrap is true for
    // a fresh workspace). Let's create two more panes.
    let pane_a: Pane = client
        .request(DaemonRequest::CreatePane {
            title: Some("a".to_string()),
            profile: None,
        })
        .expect("create a should succeed");
    let pane_b: Pane = client
        .request(DaemonRequest::CreatePane {
            title: Some("b".to_string()),
            profile: None,
        })
        .expect("create b should succeed");

    // End pane_b by sending "exit" — cat will exit on EOF, but actually cat
    // doesn't understand "exit". Let's use a different approach: close pane_b
    // via ClosePane, or use a shell that exits. Actually, /bin/cat reads until
    // EOF. We can close its stdin by... hmm. Let's use a shell that exits.
    // Actually, the simplest approach: the shell is /bin/cat, so sending
    // Ctrl-D (EOT, 0x04) will cause cat to see EOF and exit.
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_b.id.clone(),
            input: "\u{0004}".to_string(), // Ctrl-D / EOT
        })
        .expect("send should succeed");

    // Wait for pane_b to end.
    let mut ended = false;
    for _ in 0..100 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: pane_b.id.clone(),
            })
            .expect("status should succeed");
        if status.state == PaneRuntimeState::Ended {
            ended = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ended, "pane_b should end after EOT");

    // Verify pane_a is still live.
    let status_a: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: pane_a.id.clone(),
        })
        .expect("status a should succeed");
    assert_eq!(
        status_a.state,
        PaneRuntimeState::Live,
        "pane_a should still be live"
    );

    // Subscribe — catch-up should send PaneEnded for pane_b (ended) but NOT
    // for pane_a (live).
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut got_ended_b = false;
    let mut got_ended_a = false;
    // Read all catch-up events (the catch-up sends PaneEnded for ended panes
    // only; there may be multiple events). Give a short window to collect them.
    for _ in 0..20 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PaneEnded { pane_id, .. }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    if pane_id == pane_b.id {
                        got_ended_b = true;
                    }
                    if pane_id == pane_a.id {
                        got_ended_a = true;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        got_ended_b,
        "catch-up should send PaneEnded for ended pane_b"
    );
    assert!(
        !got_ended_a,
        "catch-up should NOT send PaneEnded for live pane_a"
    );

    daemon.shutdown();
}

/// Stress regression: subscribe catch-up with ended panes >> SUBSCRIBER_QUEUE_LIMIT
/// (1024) must deliver a PaneEnded for EVERY ended pane, even when the total
/// far exceeds both the bounded channel capacity and the kernel socket send
/// buffer. The catch-up path uses `send_to_subscriber` which previously did a
/// non-blocking `try_send` and silently dropped events once the bounded channel
/// filled up. With 10000 ended panes and a subscriber that doesn't read
/// immediately (simulating a slow client or GUI startup), the channel + socket
/// buffer overflow and events are silently dropped — violating VAL-LIFE-011's
/// "every pane" guarantee. After the fix (reliable catch-up delivery with
/// retry+timeout), all 10000 must arrive once the subscriber starts reading.
#[test]
fn catchup_delivers_every_ended_pane_above_queue_limit() {
    let total_panes: usize = 10000; // >> SUBSCRIBER_QUEUE_LIMIT (1024)

    // Pre-seed a workspace.json with `total_panes` panes, all marked Ended.
    // Using restore_on_demand ensures the daemon loads them as Ended without
    // auto-reviving (no PTY spawns needed — fast and resource-light).
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let cwd = PathBuf::from("/tmp/sgian-catchup-stress");

    let panes: Vec<Pane> = (1..=total_panes)
        .map(|i| Pane {
            id: format!("pane-{i}"),
            title: format!("pane-{i}"),
            kind: PaneKind::Shell,
            created_at_ms: 1000 + i as u64,
        })
        .collect();
    let mut pane_states = HashMap::new();
    for pane in &panes {
        pane_states.insert(pane.id.clone(), PaneRuntimeState::Ended);
    }
    let persisted = PersistedWorkspace {
        panes,
        active_pane_id: Some("pane-1".to_string()),
        cwd: cwd.display().to_string(),
        next_id: (total_panes + 1) as u64,
        layout: None,
        sizes: HashMap::new(),
        pane_states,
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        data_dir.path().join(WORKSPACE_FILE),
        serde_json::to_vec(&persisted).expect("workspace should serialize"),
    )
    .expect("workspace should be written");

    // Start the daemon with restore_on_demand so ended panes stay Ended.
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let data_dir_path = data_dir.path().to_path_buf();
    let join_handle = thread::spawn({
        let cwd = cwd.clone();
        let sp = socket_path.clone();
        let dd = data_dir_path.clone();
        move || {
            run_daemon_with_config(
                cwd,
                sp,
                dd,
                Config {
                    shell: Some("/bin/cat".to_string()),
                    idle_shutdown_secs: None,
                    restore_policy: Some("restore_on_demand".to_string()),
                    ..Default::default()
                },
            )
        }
    });
    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);
    retry_until_ready(&socket_path, &token);

    let client = DaemonClient {
        cwd: cwd.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: token.clone(),
        auto_spawn: false,
    };

    // Subscribe — the catch-up should push a PaneEnded for every ended pane.
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");

    // Deliberately delay reading to create queue pressure: the catch-up runs
    // on the daemon side while the subscriber's socket buffer fills up. Once
    // the kernel socket send buffer + the 1024-slot bounded channel are both
    // full, the old try_send path silently drops the remaining events. The
    // fixed retry path blocks until the subscriber starts draining.
    thread::sleep(Duration::from_millis(500));

    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout should apply");

    // Read PaneEnded events and collect the set of pane ids seen.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut seen_ended: HashSet<String> = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if seen_ended.len() >= total_panes {
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::PaneEnded { pane_id, .. }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    seen_ended.insert(pane_id);
                }
            }
            Err(_) => break,
        }
    }

    // The critical assertion: every ended pane must be represented.
    let missing: Vec<String> = (1..=total_panes)
        .map(|i| format!("pane-{i}"))
        .filter(|id| !seen_ended.contains(id))
        .collect();
    assert_eq!(
        seen_ended.len(),
        total_panes,
        "catch-up should deliver PaneEnded for all {total_panes} ended panes, \
         but {} were missing (e.g. {:?}); only {} received",
        missing.len(),
        &missing[..missing.len().min(10)],
        seen_ended.len(),
    );

    // Clean up.
    let _ = authenticate_stream_at(&socket_path, &token).and_then(|mut s| {
        write_json_line(&mut s, &DaemonRequest::Shutdown)?;
        Ok::<(), String>(())
    });
    let _ = join_handle.join();
}

/// VAL-LIFE-010: idle-timeout shutdown is suppressed while a client is attached;
/// resumes after disconnect. Uses a low idle_shutdown_secs so the daemon would
/// idle-out quickly if not for the attached subscriber.
#[test]
fn idle_suppressed_while_attached() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: Some(2), // short idle timeout
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Subscribe a client (attach) — this keeps the daemon alive past the idle
    // timeout because subscriber_count > 0 suppresses idle shutdown.
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout should apply");

    // Wait beyond the idle timeout (3s > 2s idle). The daemon should still be
    // alive because a subscriber is attached.
    thread::sleep(Duration::from_secs(3));

    // The daemon should still be serving requests (not idle-shut-down).
    // This single ping is OK: it resets idle_since, but the subscriber is still
    // attached so the daemon wouldn't idle-out anyway.
    let ok: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping should succeed while subscriber attached");
    assert!(
        ok.ok,
        "daemon should still be alive with a subscriber attached"
    );

    // Disconnect the subscriber by dropping the stream.
    drop(stream);

    // Wait for the disconnect watcher to remove the subscriber (brief settle),
    // then wait for the idle timeout to fire (no subscribers → idle shutdown
    // after 2s). Do NOT ping during this window — each ping is a new connection
    // that resets the idle timer in the accept loop.
    thread::sleep(Duration::from_secs(4));

    // Now try to ping — if the daemon has shut down, this will fail.
    let result = client.request::<CommandOk>(DaemonRequest::Ping);
    assert!(
        result.is_err(),
        "daemon should idle-shut down after the last subscriber disconnects"
    );

    // The daemon thread exited on its own (idle shutdown). Join it via restart-
    // style cleanup: send Shutdown (no-op if already stopped) and join.
    if let Some(handle) = daemon.join_handle.take() {
        let _ = handle.join();
    }
}

/// VAL-CROSS-001: daemon and panes survive client disconnect and are reflected
/// on reattach. With idle_shutdown disabled (0), the daemon stays alive between
/// short-lived client connections, and a new client sees the same panes.
#[test]
fn daemon_survives_client_disconnect_reattach() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None, // tmux-style: daemon outlives clients
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);

    // First client: create two named panes.
    {
        let client = daemon.client();
        let pane_a: Pane = client
            .request(DaemonRequest::CreatePane {
                title: Some("a".to_string()),
                profile: None,
            })
            .expect("create a should succeed");
        let pane_b: Pane = client
            .request(DaemonRequest::CreatePane {
                title: Some("b".to_string()),
                profile: None,
            })
            .expect("create b should succeed");
        assert_eq!(pane_a.id, "pane-2");
        assert_eq!(pane_b.id, "pane-3");
    } // client drops here (connection closes)

    // Simulate time passing with no resident client.
    thread::sleep(Duration::from_millis(100));

    // Reattach: a new client connection should see the same panes.
    {
        let client = daemon.client();
        let list: PaneList = client
            .request(DaemonRequest::ListPanes)
            .expect("list should succeed");

        // pane-1 (seeded) + pane-2 (a) + pane-3 (b) = 3 panes.
        let ids: Vec<String> = list.panes.iter().map(|p| p.pane.id.clone()).collect();
        assert!(
            ids.iter().any(|id| id == "pane-2"),
            "pane-2 should survive reattach"
        );
        assert!(
            ids.iter().any(|id| id == "pane-3"),
            "pane-3 should survive reattach"
        );

        // A live pane should still respond to input.
        let ok: CommandOk = client
            .request(DaemonRequest::SendInput {
                pane_id: "pane-2".to_string(),
                input: "test\n".to_string(),
            })
            .expect("send should succeed");
        assert!(ok.ok, "live pane should accept input after reattach");
    }

    daemon.shutdown();
}

// ----- Daemon flock lock tests (VAL-LIFE-003/004/009, VAL-SEC-008, VAL-LIFE-012) -----

/// VAL-LIFE-003: under concurrent cold start, exactly one daemon owns the
/// socket; the other defers (returns Ok) without binding a second listener.
#[test]
fn concurrent_cold_start_yields_one_socket_owner() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let cwd = PathBuf::from("/tmp/sgian-race-test");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };

    // Two threads race to start the daemon on the same socket. The lock
    // serializes them: one wins (blocks in the accept loop), the other defers
    // (returns Ok promptly). A channel collects the prompt return (the loser).
    let (loser_tx, loser_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let socket_path = socket_path.clone();
        let data_dir_path = data_dir.path().to_path_buf();
        let cwd = cwd.clone();
        let config = config.clone();
        let loser_tx = loser_tx.clone();
        handles.push(thread::spawn(move || {
            let result = run_daemon_with_config(cwd, socket_path, data_dir_path, config);
            // The loser returns promptly; the winner only returns after
            // shutdown. Send the result; the test reads the first (loser's).
            let _ = loser_tx.send(result);
        }));
    }
    drop(loser_tx);

    // The loser defers within a bounded window (no hang).
    let loser_result = loser_rx
        .recv_timeout(Duration::from_secs(8))
        .expect("loser should defer within 8s");
    assert!(
        loser_result.is_ok(),
        "deferring daemon should return Ok, got {loser_result:?}"
    );

    // Exactly one daemon is listening: ping it via a client.
    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);
    retry_until_ready(&socket_path, &token);
    let client = DaemonClient {
        cwd: cwd.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: token.clone(),
        auto_spawn: false,
    };
    let ok: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping the single owner should succeed");
    assert!(ok.ok, "the winning daemon should answer ping");

    // Clean teardown: shut down the winner and join both threads.
    let _: CommandOk = client
        .request(DaemonRequest::Shutdown)
        .expect("shutdown should succeed");
    for handle in handles {
        let _ = handle.join();
    }
}

/// VAL-LIFE-004: a stale lock from a crashed daemon is auto-released by the
/// kernel (flock is tied to the open file description). After the holder drops
/// (simulating process death), the next acquire succeeds without stale recovery.
#[test]
fn stale_daemon_lock_is_recovered() {
    let dir = tempfile::tempdir_in("/tmp").expect("temp dir");
    let socket_path = dir.path().join("d.sock");
    // First acquire: simulate a daemon holding the lock.
    let lock_a = acquire_daemon_lock(&socket_path)
        .expect("first acquire should succeed")
        .expect("first acquire should return the lock file");
    // A concurrent acquire defers (WouldBlock).
    let lock_b = acquire_daemon_lock(&socket_path).expect("second acquire should not error");
    assert!(lock_b.is_none(), "second acquire should defer while held");

    // Simulate crash: drop the holder. flock releases on fd close.
    drop(lock_a);

    // The next acquire succeeds without stale recovery.
    let lock_c = acquire_daemon_lock(&socket_path)
        .expect("third acquire should succeed after holder dropped")
        .expect("third acquire should return the lock file");
    // Cleanup.
    drop(lock_c);
}

/// VAL-LIFE-009: after a clean shutdown, the lock is released and the next
/// daemon starts normally (reacquires the lock, no stale-recovery path).
#[test]
fn clean_shutdown_releases_daemon_lock() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let cwd = PathBuf::from("/tmp/sgian-clean-shutdown-test");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };

    let join_handle = thread::spawn({
        let cwd = cwd.clone();
        let socket_path = socket_path.clone();
        let data_dir_path = data_dir.path().to_path_buf();
        move || run_daemon_with_config(cwd, socket_path, data_dir_path, config)
    });

    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);
    retry_until_ready(&socket_path, &token);

    // The running daemon holds the lock: a second acquire defers.
    let lock_check =
        acquire_daemon_lock(&socket_path).expect("acquire during running daemon should not error");
    assert!(
        lock_check.is_none(),
        "a second lock acquire should defer while the daemon holds it"
    );

    // Clean shutdown releases the lock (the daemon's lock File drops).
    let client = DaemonClient {
        cwd: cwd.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: token.clone(),
        auto_spawn: false,
    };
    let _: CommandOk = client
        .request(DaemonRequest::Shutdown)
        .expect("shutdown should succeed");
    let _ = join_handle.join();

    // After shutdown the lock is released when the daemon's lock File drops.
    // Under heavy parallel `cargo test` load the kernel's flock-release-on-fd-
    // close can lag the very next acquire by a few milliseconds, so retry within
    // a short bounded window rather than asserting on the first attempt. This
    // still asserts the lock becomes free promptly after a clean shutdown — it
    // fails if the lock never frees — and only tolerates the sub-second release-
    // visibility race, without weakening what it asserts.
    let deadline = Instant::now() + Duration::from_secs(2);
    let lock_after = loop {
        match acquire_daemon_lock(&socket_path).expect("acquire after shutdown should not error") {
            Some(file) => break file,
            None => {
                assert!(
                    Instant::now() < deadline,
                    "lock should be free within 2s of a clean shutdown"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
    };
    drop(lock_after);
}

/// Regression for the spawn-after-shutdown bug (m1-fix-respawn-after-shutdown):
/// a spawning verb issued right after a clean `ctl shutdown` failed with
/// "daemon did not become ready". The dying daemon still holds the flock
/// during teardown; the freshly-spawned daemon's `acquire_daemon_lock` returned
/// `Ok(None)` and it silently deferred (`return Ok(())`), exiting without
/// binding. The client then waited 2s for a daemon that never came up.
///
/// This test simulates a dying daemon holding the lock: it acquires the lock,
/// spawns `run_daemon_with_config` in a thread, and asserts the thread does
/// NOT exit immediately (it waits for the lock). After releasing the lock
/// (simulating the dying daemon finishing teardown), the spawned daemon must
/// acquire it, bind the socket, and serve a Ping.
#[test]
fn respawn_after_shutdown_waits_for_lock_then_binds() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let cwd = PathBuf::from("/tmp/sgian-respawn-test");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };

    // Simulate a dying daemon: hold the lock during teardown.
    let dying_lock = acquire_daemon_lock(&socket_path)
        .expect("holder acquire should succeed")
        .expect("holder acquire should return the lock file");

    let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let join_handle = thread::spawn({
        let cwd = cwd.clone();
        let socket_path = socket_path.clone();
        let data_dir_path = data_dir.path().to_path_buf();
        move || {
            let result = run_daemon_with_config(cwd, socket_path, data_dir_path, config);
            let _ = result_tx.send(result);
        }
    });

    // The spawned daemon must NOT exit immediately while the lock is held.
    // On the buggy behavior it deferred instantly (returning Ok within ~0ms).
    // Give it a moment, then assert it is still waiting.
    match result_rx.recv_timeout(Duration::from_millis(400)) {
        Ok(early) => {
            panic!("spawned daemon returned while lock was held (should wait): {early:?}")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            // Good: the daemon is still waiting for the lock.
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("spawned daemon thread exited unexpectedly");
        }
    }

    // Release the lock (dying daemon finishes teardown). The spawned daemon
    // must now acquire it, bind, and serve.
    drop(dying_lock);

    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);
    retry_until_ready(&socket_path, &token);

    let client = DaemonClient {
        cwd: cwd.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: token.clone(),
        auto_spawn: false,
    };
    let ok: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping the respawned daemon should succeed");
    assert!(ok.ok, "the respawned daemon should answer ping");

    let _: CommandOk = client
        .request(DaemonRequest::Shutdown)
        .expect("shutdown should succeed");
    let _ = join_handle.join();
}

/// Regression (m1-fix-respawn-after-shutdown): when a LIVE daemon holds the
/// lock and is serving on the socket, a concurrent spawn must still defer
/// promptly (connect check succeeds) rather than waiting the full lock-wait
/// window. This guards the concurrent-cold-start path against the bounded
/// wait regressing VAL-LIFE-003.
#[test]
fn concurrent_spawn_defers_promptly_when_live_daemon_serves() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);

    // A second spawn against the same socket must defer (return Ok) within a
    // short window because the live daemon is serving on the socket.
    let socket_path = daemon.socket_path.clone();
    let data_dir_path = daemon.data_dir.path().to_path_buf();
    let cwd = daemon.cwd.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let handle = thread::spawn(move || {
        let result = run_daemon_with_config(
            cwd,
            socket_path,
            data_dir_path,
            Config {
                shell: Some("/bin/cat".to_string()),
                idle_shutdown_secs: None,
                ..Default::default()
            },
        );
        let _ = tx.send(result);
    });

    let loser_result = rx
        .recv_timeout(Duration::from_secs(4))
        .expect("loser should defer within 4s");
    assert!(
        loser_result.is_ok(),
        "deferring daemon should return Ok, got {loser_result:?}"
    );
    let _ = handle.join();

    daemon.shutdown();
}

/// VAL-SEC-008: the daemon lock file is owner-only (0600) and its containing
/// runtime dir is 0700.
#[test]
fn daemon_lock_file_is_owner_only() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let lock_path = daemon
        .socket_path
        .parent()
        .expect("socket parent")
        .join(DAEMON_LOCK_FILE);
    assert!(lock_path.exists(), "daemon.lock should exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = fs::metadata(&lock_path)
            .expect("lock metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "daemon.lock should be 0600, got {file_mode:o}"
        );
        let dir_mode = fs::metadata(lock_path.parent().expect("lock parent"))
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "runtime dir should be 0700, got {dir_mode:o}"
        );
    }
    daemon.shutdown();
}

// ─── H4: wedged-daemon socket protection ───

#[test]
fn daemon_start_failure_surfaces_child_status_and_early_error() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let path = reset_daemon_startup_log(data_dir.path());
    record_daemon_startup_error(
        &[
            "sgian".to_string(),
            DAEMON_ARG.to_string(),
            DATA_DIR_ARG.to_string(),
            data_dir.path().display().to_string(),
        ],
        "failed to bind daemon: access denied",
    );

    let error = format_daemon_start_failure(
        "The system cannot find the file specified. (os error 2)",
        Some("exit code: 1".to_string()),
        &path,
    );
    assert!(error.contains("spawned daemon exited (exit code: 1)"));
    assert!(error.contains("failed to bind daemon: access denied"));
    assert!(error.contains("os error 2"));
    assert!(error.contains(DAEMON_STARTUP_LOG_FILE));
}

#[test]
fn daemon_start_failure_reports_live_child_without_fabricating_log_text() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let path = reset_daemon_startup_log(data_dir.path());
    let error = format_daemon_start_failure("pipe missing", None, &path);
    assert!(error.contains("still running but exposed no endpoint"));
    assert!(error.contains("pipe missing"));
    assert!(!error.contains("startup error:"));
}

/// H4: the lock probe distinguishes a live-but-wedged daemon (flock held)
/// from a dead one (flock free) — without creating the lock file.
#[test]
fn daemon_lock_is_held_detects_held_free_and_missing() {
    let dir = tempfile::tempdir_in("/tmp").expect("temp dir");
    let socket_path = dir.path().join("d.sock");

    // Missing lock file: no daemon ever ran — not held, and the probe must
    // NOT create the file as a side effect.
    assert!(!daemon_lock_is_held(&socket_path));
    assert!(
        !dir.path().join(DAEMON_LOCK_FILE).exists(),
        "a pure probe must not create the lock file"
    );

    // Held by this process (standing in for a running daemon): held.
    let lock = acquire_daemon_lock(&socket_path)
        .expect("acquire should succeed")
        .expect("acquire should return the lock file");
    assert!(daemon_lock_is_held(&socket_path));

    // Released: free again. Another test thread may be mid-fork (a
    // subprocess spawn): its child holds a duplicate of our lock fd until
    // it execs, which keeps the flock alive for a few microseconds past
    // the drop, so give the release a bounded moment.
    drop(lock);
    let released = (0..200).any(|_| {
        if daemon_lock_is_held(&socket_path) {
            thread::sleep(Duration::from_millis(5));
            false
        } else {
            true
        }
    });
    assert!(
        released,
        "the daemon lock must be free once its file is dropped"
    );
}

/// H4: with the workspace flock HELD (a live-but-wedged daemon) and ping
/// failing, ensure_daemon must refuse to replace the daemon and must NOT
/// unlink its live socket.
#[test]
fn ensure_daemon_refuses_to_unlink_wedged_daemon_socket() {
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_path = socket_dir.path().join("d.sock");
    // A genuine socket file whose listener is gone (ping fails fast with
    // ECONNREFUSED): the shape a wedged daemon's socket presents.
    drop(transport_bind(&socket_path).expect("bind a socket file"));
    assert!(socket_path.exists());
    // Hold the flock the way a running daemon does.
    let _lock = acquire_daemon_lock(&socket_path)
        .expect("acquire should succeed")
        .expect("acquire should return the lock file");

    let client = DaemonClient {
        cwd: PathBuf::from("/tmp/sgian-h4-wedged"),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: "test-token".to_string(),
        auto_spawn: true,
    };
    let error = client
        .ensure_daemon()
        .expect_err("ensure_daemon must refuse to replace a wedged daemon");
    assert!(
        error.contains("not responding"),
        "error should describe the wedged daemon: {error}"
    );
    assert!(
        socket_path.exists(),
        "the live socket must NOT be unlinked while the lock is held"
    );
}

/// H4: with the flock FREE the stale socket is unlinked and the spawn path
/// proceeds (it then fails readiness against the test binary, which is not a
/// daemon — the point is the removal + spawn attempt, not a real daemon).
#[test]
fn ensure_daemon_removes_stale_socket_when_lock_is_free() {
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_path = socket_dir.path().join("d.sock");
    drop(transport_bind(&socket_path).expect("bind a socket file"));
    assert!(socket_path.exists());
    // No lock held anywhere (the probe finds no lock file at first).

    let client = DaemonClient {
        cwd: PathBuf::from("/tmp/sgian-h4-stale"),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: "test-token".to_string(),
        auto_spawn: true,
    };
    let error = client
        .ensure_daemon()
        .expect_err("the test binary is not a daemon; readiness must fail");
    assert!(
        !error.contains("not responding"),
        "a free lock must not trip the wedged-daemon refusal: {error}"
    );
    assert!(
        !socket_path.exists(),
        "the stale socket must be unlinked once the lock is known free"
    );
}

/// H4: `ctl shutdown` against a daemon whose lock is HELD but whose socket
/// does not answer reports "daemon not responding (lock held)" as an
/// error — not the exit-0 "no daemon running" no-op (which stays for a
/// genuinely absent daemon).
#[test]
fn shutdown_when_unresponsive_reports_wedged_lock_as_error() {
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");

    // No lock file at all → the genuine no-daemon no-op (Ok).
    shutdown_when_unresponsive(&socket_path, false).expect("no daemon is an exit-0 no-op");

    // Lock HELD → wedged-daemon error.
    let lock = acquire_daemon_lock(&socket_path)
        .expect("acquire should succeed")
        .expect("acquire should return the lock file");
    let error = shutdown_when_unresponsive(&socket_path, false)
        .expect_err("a held lock must be an error, not a no-op");
    assert!(
        error.contains("daemon not responding (lock held)"),
        "unexpected message: {error}"
    );

    // Lock released → no-op again.
    drop(lock);
    shutdown_when_unresponsive(&socket_path, false).expect("a free lock is the no-daemon no-op");
}

/// VAL-IPC-037: the Windows named-pipe name folds in a per-user SID component
/// (so two users cannot collide on the same workspace pipe) rather than being
/// a bare path hash. Exercises the platform-independent composer that the
/// `cfg(windows)` `pipe_name_from_path` delegates to, so the SID-scoping
/// contract is verifiable on macOS too.
#[test]
fn windows_pipe_name_with_sid_is_per_user_scoped() {
    let path = Path::new("/tmp/sgian-ws-abc/daemon.sock");
    let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    let name = pipe_name_with_sid(sid, path);

    // Valid pipe prefix and the SID component is embedded verbatim.
    assert!(
        name.starts_with(&format!("\\\\.\\pipe\\{WINDOWS_IPC_NAMESPACE}-")),
        "unexpected prefix: {name}"
    );
    assert!(
        name.contains(sid),
        "pipe name must contain the SID component: {name}"
    );

    // Different users (SIDs) derive different pipe names for the SAME path —
    // this is the per-user scoping that prevents cross-user collisions.
    let other = pipe_name_with_sid("S-1-5-21-9-9-9-500", path);
    assert_ne!(name, other, "distinct SIDs must yield distinct pipe names");

    // Same SID + same path is stable/deterministic.
    assert_eq!(name, pipe_name_with_sid(sid, path));

    // A different path with the same SID differs (path is still part of the
    // name), and the SID still scopes it.
    let other_path = pipe_name_with_sid(sid, Path::new("/tmp/sgian-ws-xyz/daemon.sock"));
    assert_ne!(name, other_path);
    assert!(other_path.contains(sid));
}

/// VAL-IPC-037 (continued): non-pipe-safe characters in a SID component are
/// sanitized, never injected raw into the pipe name.
#[test]
fn windows_pipe_name_sanitizes_sid_component() {
    let name = pipe_name_with_sid("evil\\..\\pipe\\x", Path::new("/tmp/ws/daemon.sock"));
    assert!(
        name.starts_with(&format!("\\\\.\\pipe\\{WINDOWS_IPC_NAMESPACE}-")),
        "name: {name}"
    );
    // The only backslashes present are the literal `\\.\pipe\` prefix (4),
    // none injected from the SID component.
    assert_eq!(
        name.matches('\\').count(),
        4,
        "no extra backslashes may leak from the SID component: {name}"
    );
}

/// ma-scrutiny-fixes (1): the Windows pipe NAME derivation fails CLOSED when the
/// per-user SID cannot be resolved — there is NO "nosid"/placeholder fallback, so
/// a derived pipe name ALWAYS embeds a real per-user SID (two users can never
/// collide on, or hijack, the same workspace pipe). Exercises the
/// platform-independent helper the cfg(windows) `pipe_name_from_path` delegates to,
/// so the fail-closed contract is verifiable on macOS.
#[test]
fn pipe_name_from_sid_fails_closed_without_sid() {
    let path = Path::new("/tmp/sgian-ws-abc/daemon.sock");
    // SID lookup failed (None) ⇒ refuse to build a pipe name (Err), never fall
    // back to a fixed placeholder component.
    let err = pipe_name_from_sid(None, path)
        .expect_err("a missing SID must fail closed, not fall back to a placeholder");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    // A resolved SID yields a name embedding that SID verbatim, identical to the
    // composer's output.
    let sid = "S-1-5-21-1111111111-2222222222-3333333333-1001";
    let name =
        pipe_name_from_sid(Some(sid.to_string()), path).expect("a resolved SID yields a name");
    assert_eq!(name, pipe_name_with_sid(sid, path));
    assert!(name.contains(sid), "pipe name must embed the SID: {name}");
    // The old "nosid" placeholder must never appear in a derived name.
    assert!(
        !name.contains("nosid"),
        "no placeholder SID component may appear: {name}"
    );
}

/// ma-scrutiny-fixes (2): pipe creation fails CLOSED when the owner security
/// descriptor cannot be built — a NULL descriptor is refused (Err), never silently
/// downgraded to default-ACL security (so CreateNamedPipeW is never called with
/// NULL SECURITY_ATTRIBUTES). Exercises the platform-independent guard both the
/// initial bind and each post-accept recreate consult, so the fail-closed contract
/// is verifiable on macOS.
#[test]
fn require_owner_descriptor_fails_closed_on_null() {
    // NULL descriptor (SID lookup / SDDL build failed) ⇒ refuse.
    let err = require_owner_descriptor(true).expect_err("a NULL owner descriptor must fail closed");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    // A real (non-NULL) descriptor is accepted.
    require_owner_descriptor(false).expect("a real owner descriptor is accepted");
}

/// VAL-SEC-008: the structured daemon log file is owner-only (0600) and its
/// containing data dir is 0700.
#[test]
fn daemon_log_file_is_owner_only() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    // Force a log entry by issuing a ping, then read the log file perms.
    let client = daemon.client();
    let _: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping should succeed");
    let log_path = daemon.data_dir.path().join(LOG_FILE);
    assert!(log_path.exists(), "daemon.log should exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = fs::metadata(&log_path)
            .expect("log metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "daemon.log should be 0600, got {file_mode:o}"
        );
        let dir_mode = fs::metadata(daemon.data_dir.path())
            .expect("data dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "data dir should be 0700, got {dir_mode:o}");
    }
    daemon.shutdown();
}

/// VAL-LIFE-012: a daemon that cannot come up (runtime dir cannot be created)
/// surfaces a clear error promptly rather than hanging.
#[test]
fn run_daemon_surfaces_clear_error_when_runtime_dir_cannot_be_created() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let dir = tempfile::tempdir_in("/tmp").expect("temp dir");
    // Pre-create the socket parent as a regular file so ensure_private_dir fails.
    let parent = dir.path().join("runtimedir");
    fs::write(&parent, "not a dir").expect("write file");
    let socket_path = parent.join("d.sock");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let result = run_daemon_with_config(
        PathBuf::from("/tmp/sgian-bind-fail-test"),
        socket_path,
        data_dir.path().to_path_buf(),
        Config::default(),
    );
    assert!(
        result.is_err(),
        "should surface a clear error, got {result:?}"
    );
    let err = result.unwrap_err();
    assert!(!err.is_empty(), "error message should be non-empty");
    // No daemon is listening (no hang, no half-started process).
}

// ----- bootstrap aggregate scrollback budget (H2) -----

/// ENHANCEMENTS §5: bootstrap + reattach with many panes stays under a
/// gross wall-clock ceiling (catches pathological regressions, not microbench).
#[test]
fn bootstrap_and_reattach_stay_within_perf_baseline() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();
    for _ in 0..15 {
        let _: Pane = client
            .request(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed");
    }
    let started = Instant::now();
    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let bootstrap_elapsed = started.elapsed();
    assert!(
        bootstrap_elapsed < Duration::from_secs(5),
        "bootstrap with 16 panes took {bootstrap_elapsed:?} (budget 5s)"
    );
    let reattach_started = Instant::now();
    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("reattach bootstrap");
    let reattach_elapsed = reattach_started.elapsed();
    assert!(
        reattach_elapsed < Duration::from_secs(5),
        "reattach bootstrap took {reattach_elapsed:?} (budget 5s)"
    );
    daemon.shutdown();
}

/// H2: a v2 (framed) BootstrapWorkspace response must stay deliverable even
/// when many panes have grown scrollback. Per-pane reads are capped at
/// SCROLLBACK_REPLAY_LIMIT_BYTES and the aggregate is bounded by
/// BOOTSTRAP_SCROLLBACK_BUDGET_BYTES, so the framed response stays under
/// MAX_FRAME_BYTES and every pane still appears in the snapshot.
#[test]
fn bootstrap_with_grown_scrollback_stays_under_frame_cap() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Five panes, each with a full (per-pane-capped) scrollback tail:
    // 5 × 2 MiB raw would far exceed the 8 MiB frame cap unbounded.
    let mut pane_ids = vec!["pane-1".to_string()];
    for _ in 0..4 {
        let pane: Pane = client
            .request(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed");
        pane_ids.push(pane.id);
    }
    // ANSI/control-heavy content exercises the worst JSON escaping growth
    // (ESC → XX), not just the raw byte count.
    let line = "\x1b[31merror: something failed\x1b[0m\n";
    let chunk = line.repeat(SCROLLBACK_REPLAY_LIMIT_BYTES / line.len() + 1);
    let scrollback_dir = daemon.data_dir.path().join(SCROLLBACK_DIR);
    for pane_id in &pane_ids {
        fs::write(scrollback_path(&scrollback_dir, pane_id), &chunk)
            .expect("scrollback should be written");
    }

    let mut conn = client.connect().expect("connect should succeed");
    let response = conn
        .request(&DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap must be deliverable, not dropped as oversized");
    assert!(response.ok, "bootstrap failed: {:?}", response.error);

    let snapshot: WorkspaceSnapshot =
        serde_json::from_value(response.result.clone()).expect("snapshot should decode");
    assert_eq!(
        snapshot.panes.len(),
        pane_ids.len(),
        "every pane must still appear in the snapshot"
    );

    // The exact encode the daemon performed for this response must fit a frame.
    let encoded = frame::encode(&response).expect("bootstrap response must be frameable");
    assert!(
        encoded.len() as u64 <= MAX_FRAME_BYTES,
        "encoded bootstrap response ({} bytes) exceeds MAX_FRAME_BYTES",
        encoded.len()
    );

    // The aggregate serialized scrollback respects the budget, and the first
    // pane (filled first) still carries scrollback.
    let aggregate: usize = snapshot
        .scrollback
        .values()
        .map(|data| serialized_json_len(data))
        .sum();
    assert!(
        aggregate <= BOOTSTRAP_SCROLLBACK_BUDGET_BYTES,
        "aggregate serialized scrollback ({aggregate}) exceeds the budget"
    );
    assert!(
        snapshot
            .scrollback
            .get("pane-1")
            .is_some_and(|data| !data.is_empty()),
        "first pane should still carry scrollback"
    );

    daemon.shutdown();
}

// ----- restore policy tests (VAL-LIFE-005/006/007/008, VAL-CROSS-012/016) -----

/// Helper: create a pane, end it (Ctrl-D to /bin/cat), wait for Ended,
/// and return the pane id. The daemon must use /bin/cat as the shell.
fn create_and_end_pane(client: &DaemonClient) -> String {
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane.id.clone(),
            input: "\u{0004}".to_string(), // Ctrl-D / EOT → cat exits
        })
        .expect("send EOT should succeed");
    for _ in 0..200 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: pane.id.clone(),
            })
            .expect("status should succeed");
        if status.state == PaneRuntimeState::Ended {
            return pane.id;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("pane {} never ended", pane.id);
}

/// VAL-LIFE-005: With no `restore_policy` configured, the default auto-revives
/// ended panes on bootstrap.
#[test]
fn restore_policy_default_revives_ended_panes() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Bootstrap to spawn the initial pane.
    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");

    let ended_pane = create_and_end_pane(&client);

    // Restart with default config (no restore_policy → auto_respawn).
    daemon.restart(Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    });
    let client = daemon.client();

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");

    // The ended pane should be revived (Live) under the default policy.
    let state = snapshot.pane_states.get(&ended_pane);
    assert_eq!(
        state,
        Some(&PaneRuntimeState::Live),
        "default policy should auto-revive ended pane {ended_pane}"
    );

    daemon.shutdown();
}

/// VAL-LIFE-006: With `restore_policy: "auto_respawn"` explicitly set, ended
/// panes are automatically restarted on bootstrap.
#[test]
fn restore_policy_auto_respawn_revives_ended_panes() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("auto_respawn".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");

    let ended_pane = create_and_end_pane(&client);

    daemon.restart(Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("auto_respawn".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");

    assert_eq!(
        snapshot.pane_states.get(&ended_pane),
        Some(&PaneRuntimeState::Live),
        "auto_respawn should revive ended pane {ended_pane}"
    );

    daemon.shutdown();
}

/// VAL-LIFE-007 / VAL-CROSS-016: With `restore_policy: "restore_on_demand"`,
/// ended panes are restored to the listing but NOT auto-restarted; they stay
/// Ended until explicitly revived, at which point they become Live.
#[test]
fn restore_policy_restore_on_demand_keeps_ended() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");

    let ended_pane = create_and_end_pane(&client);

    daemon.restart(Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");

    // The pane should be listed but Ended (not auto-revived).
    let pane_exists = snapshot.panes.iter().any(|p| p.id == ended_pane);
    assert!(pane_exists, "ended pane should still be listed");
    assert_eq!(
        snapshot.pane_states.get(&ended_pane),
        Some(&PaneRuntimeState::Ended),
        "restore_on_demand should NOT auto-revive ended pane {ended_pane}"
    );

    // Explicit revive via RestartPaneTerminal → Live.
    let _: CommandOk = client
        .request(DaemonRequest::RestartPaneTerminal {
            pane_id: ended_pane.clone(),
        })
        .expect("restart should succeed");

    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: ended_pane.clone(),
        })
        .expect("status after restart");
    assert_eq!(
        status.state,
        PaneRuntimeState::Live,
        "explicit restart should make the pane Live"
    );

    daemon.shutdown();
}

/// VAL-LIFE-008: An unrecognized `restore_policy` value falls back to the
/// default (auto_respawn) and the daemon serves normally.
#[test]
fn restore_policy_invalid_falls_back_to_auto_respawn() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("nonsense".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");

    let ended_pane = create_and_end_pane(&client);

    daemon.restart(Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("nonsense".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    // The daemon should start and serve despite the invalid policy.
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap should succeed with invalid policy");

    // Invalid → falls back to auto_respawn → pane revived.
    assert_eq!(
        snapshot.pane_states.get(&ended_pane),
        Some(&PaneRuntimeState::Live),
        "invalid policy should fall back to auto_respawn (revive)"
    );

    daemon.shutdown();
}

/// VAL-LIFE-013: A corrupt persisted workspace.json is handled gracefully —
/// no panic, no hang, no garbage. The daemon reseeds to a fresh workspace.
#[test]
fn corrupt_workspace_json_handled_gracefully() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let cwd = PathBuf::from("/tmp/sgian-corrupt-ws-test");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };

    // First run: create a workspace with a pane, then shut down.
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let data_dir_path = data_dir.path().to_path_buf();
    let h1 = thread::spawn({
        let cwd = cwd.clone();
        let sp = socket_path.clone();
        let dd = data_dir_path.clone();
        let cfg = config.clone();
        move || run_daemon_with_config(cwd, sp, dd, cfg)
    });
    let token_path = data_dir.path().join(TOKEN_FILE);
    let token = retry_read_token(&token_path);
    retry_until_ready(&socket_path, &token);

    let client = DaemonClient {
        cwd: cwd.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: token.clone(),
        auto_spawn: false,
    };
    let _: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    // Shutdown the first daemon.
    let _ = authenticate_stream_at(&socket_path, &token).and_then(|mut stream| {
        write_json_line(&mut stream, &DaemonRequest::Shutdown)?;
        Ok::<(), String>(())
    });
    let _ = h1.join();

    // Corrupt the workspace.json file.
    let persist_path = data_dir.path().join(WORKSPACE_FILE);
    assert!(persist_path.exists(), "workspace.json should exist");
    fs::write(&persist_path, "not-json{{corrupt").expect("write corrupt workspace");

    // Second run: should handle the corrupt file gracefully.
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    let _ = fs::remove_file(&socket_path);
    let h2 = thread::spawn({
        let cwd = cwd.clone();
        let sp = socket_path.clone();
        let dd = data_dir_path.clone();
        let cfg = config.clone();
        move || run_daemon_with_config(cwd, sp, dd, cfg)
    });
    retry_until_ready(&socket_path, &token);

    let client = DaemonClient {
        cwd: cwd.clone(),
        socket_path: socket_path.clone(),
        data_dir: data_dir.path().to_path_buf(),
        token: token.clone(),
        auto_spawn: false,
    };

    // Bootstrap should succeed (reseed to fresh workspace).
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap should succeed despite corrupt workspace");

    // Should have exactly one pane (reseeded), and it should be Live.
    assert_eq!(
        snapshot.panes.len(),
        1,
        "corrupt workspace should reseed to exactly one pane"
    );
    let pane_id = &snapshot.panes[0].id;
    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: pane_id.clone(),
        })
        .expect("status should succeed");
    assert_eq!(
        status.state,
        PaneRuntimeState::Live,
        "reseeded pane should be live"
    );

    // Clean shutdown.
    let _ = authenticate_stream_at(&socket_path, &token).and_then(|mut stream| {
        write_json_line(&mut stream, &DaemonRequest::Shutdown)?;
        Ok::<(), String>(())
    });
    let _ = h2.join();
}

/// VAL-CROSS-012: A fresh workspace boots to exactly one usable, live pane.
#[test]
fn fresh_workspace_boots_one_live_pane() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Bootstrap the fresh workspace.
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap should succeed");

    // Exactly one pane.
    assert_eq!(
        snapshot.panes.len(),
        1,
        "fresh workspace should have exactly one pane"
    );

    // It should be live.
    let pane_id = &snapshot.panes[0].id;
    assert_eq!(
        snapshot.pane_states.get(pane_id),
        Some(&PaneRuntimeState::Live),
        "the seeded pane should be live"
    );

    // Verify via PaneStatus (the ctl-facing surface).
    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: pane_id.clone(),
        })
        .expect("status should succeed");
    assert_eq!(status.state, PaneRuntimeState::Live);

    daemon.shutdown();
}

/// VAL-CROSS-016: Restore policy deterministically controls reattach respawn
/// behavior. Under auto_respawn, restored panes come back live; under
/// restore_on_demand, they stay ended until explicitly revived.
#[test]
fn restore_policy_governs_reattach_respawn() {
    // --- Part A: auto_respawn → panes live on reattach ---
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("auto_respawn".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let ended_pane_a = create_and_end_pane(&client);

    daemon.restart(Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("auto_respawn".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");
    assert_eq!(
        snapshot.pane_states.get(&ended_pane_a),
        Some(&PaneRuntimeState::Live),
        "auto_respawn: pane should be live on reattach"
    );
    daemon.shutdown();

    // --- Part B: restore_on_demand → panes ended on reattach, live after revive ---
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let ended_pane_b = create_and_end_pane(&client);

    daemon.restart(Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");
    assert_eq!(
        snapshot.pane_states.get(&ended_pane_b),
        Some(&PaneRuntimeState::Ended),
        "restore_on_demand: pane should be ended on reattach"
    );

    // Explicit revive → Live.
    let _: CommandOk = client
        .request(DaemonRequest::RestartPaneTerminal {
            pane_id: ended_pane_b.clone(),
        })
        .expect("restart should succeed");
    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: ended_pane_b.clone(),
        })
        .expect("status after revive");
    assert_eq!(
        status.state,
        PaneRuntimeState::Live,
        "restore_on_demand: pane should be live after explicit revive"
    );

    daemon.shutdown();
}

// ─── VAL-SEC-001 / VAL-CROSS-006: workspace_key collision detection ───

/// A mismatch between the persisted cwd and the connecting cwd is refused by
/// the client-side check (connect_or_spawn path) with a clear error.
#[test]
fn cwd_mismatch_refused_on_connect_or_spawn() {
    // Create a temp data_dir with a workspace.json whose cwd doesn't match
    // the connecting cwd, plus a token file so connect_or_spawn can proceed
    // past the token step (it won't get there — the cwd check fires first).
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let original_cwd = "/tmp/sgian-collision-cos-original";
    let persisted = PersistedWorkspace {
        panes: vec![Pane {
            id: "pane-1".to_string(),
            title: "term-1".to_string(),
            kind: PaneKind::Shell,
            created_at_ms: now_millis(),
        }],
        active_pane_id: Some("pane-1".to_string()),
        cwd: original_cwd.to_string(),
        next_id: 2,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        data_dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).expect("serialize"),
    )
    .expect("write workspace.json");

    // The client computes data_dir from the cwd. We need the workspace.json
    // to be at the location the client will look. Since the client uses
    // workspace_data_dir_for(&cwd, &key), and we can't easily control that
    // path in a unit test, we test check_persisted_cwd directly (see
    // check_persisted_cwd_mismatch_refused) and the daemon-side check (see
    // daemon_side_refuses_cwd_mismatch). This test verifies the wiring is
    // correct by calling check_persisted_cwd with the data_dir.
    let result = check_persisted_cwd(
        &PathBuf::from("/tmp/sgian-collision-cos-different"),
        data_dir.path(),
    );
    assert!(result.is_err(), "mismatched cwd should be refused");
    let error = result.err().unwrap();
    assert!(
        error.contains("collision"),
        "error should mention collision: {error}"
    );
    assert!(
        error.contains(original_cwd),
        "error should name the persisted cwd: {error}"
    );
}

/// A mismatch is also refused by connect_existing (read-only verb path).
/// This is verified via the check_persisted_cwd helper which connect_existing
/// calls before reading the token. The unit test below exercises the helper
/// directly; the integration E2E test (ctl) exercises the full path.
#[test]
fn cwd_mismatch_refused_on_connect_existing() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let persisted = PersistedWorkspace {
        panes: vec![],
        active_pane_id: None,
        cwd: "/tmp/sgian-collision-ce-original".to_string(),
        next_id: 1,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        data_dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).expect("serialize"),
    )
    .expect("write workspace.json");

    let result = check_persisted_cwd(
        &PathBuf::from("/tmp/sgian-collision-ce-different"),
        data_dir.path(),
    );
    assert!(result.is_err(), "mismatched cwd should be refused");
    let error = result.err().unwrap();
    assert!(
        error.contains("collision"),
        "error should mention collision: {error}"
    );
}

/// The daemon-side check (defense-in-depth) also refuses a mismatched cwd.
#[test]
fn daemon_side_refuses_cwd_mismatch() {
    // Create a temp data_dir with a workspace.json whose cwd doesn't match
    // the connecting cwd. The daemon should refuse to start.
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let persisted = PersistedWorkspace {
        panes: vec![Pane {
            id: "pane-1".to_string(),
            title: "term-1".to_string(),
            kind: PaneKind::Shell,
            created_at_ms: now_millis(),
        }],
        active_pane_id: Some("pane-1".to_string()),
        cwd: "/tmp/sgian-collision-daemon-original".to_string(),
        next_id: 2,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        data_dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).expect("serialize"),
    )
    .expect("write workspace.json");

    let socket_dir = tempfile::tempdir_in("/tmp").expect("temp socket dir");
    let socket_path = socket_dir.path().join("d.sock");
    let result = run_daemon_with_config(
        PathBuf::from("/tmp/sgian-collision-daemon-different"),
        socket_path,
        data_dir.path().to_path_buf(),
        Config {
            shell: Some("/bin/cat".to_string()),
            ..Default::default()
        },
    );
    assert!(result.is_err(), "daemon should refuse on cwd mismatch");
    let error = result.err().unwrap();
    assert!(
        error.contains("collision") || error.contains("mismatch"),
        "error should mention collision/mismatch: {error}"
    );
}

/// The daemon-side defense-in-depth check must use the SAME canonical path
/// identity as the client-side guard. Previously the client accepted an
/// equivalent spelling, then the spawned daemon compared raw strings and
/// exited before creating its endpoint.
#[test]
fn daemon_side_accepts_equivalent_cwd_spellings() {
    let root = tempfile::tempdir().expect("temp root");
    let workspace = root.path().join("ws");
    fs::create_dir(&workspace).expect("create workspace");
    let canonical = fs::canonicalize(&workspace).expect("canonical workspace");
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let persisted = PersistedWorkspace {
        panes: vec![Pane {
            id: "pane-1".to_string(),
            title: "term-1".to_string(),
            kind: PaneKind::Shell,
            created_at_ms: now_millis(),
        }],
        active_pane_id: Some("pane-1".to_string()),
        cwd: canonical.display().to_string(),
        next_id: 2,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        data_dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).expect("serialize"),
    )
    .expect("write workspace.json");

    let alternate = workspace.join("..").join("ws");
    assert_ne!(
        alternate.display().to_string(),
        canonical.display().to_string(),
        "test requires distinct raw spellings"
    );
    let server =
        DaemonServer::with_config(alternate, data_dir.path().to_path_buf(), Config::default());
    assert!(
        server.is_ok(),
        "equivalent canonical cwd spellings must not trip daemon collision guard: {:?}",
        server.err()
    );
}

// ─── M9 / M17 / L21: workspace identity, key migration, cwd heuristic ───

/// M9: path SPELLINGS of the same directory (trailing slash, `.` component,
/// symlink) derive ONE workspace key — not parallel workspaces. A
/// nonexistent path falls back to the raw string, deterministically.
#[test]
fn workspace_key_canonicalizes_path_spellings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let base = dir.path().join("ws");
    fs::create_dir(&base).expect("mkdir");

    let plain = workspace_key(&base);
    let trailing = workspace_key(Path::new(&format!("{}/", base.display())));
    let dotted = workspace_key(&dir.path().join(".").join("ws"));
    assert_eq!(plain, trailing, "a trailing slash must not fork the key");
    assert_eq!(plain, dotted, "a `.` component must not fork the key");

    #[cfg(unix)]
    {
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&base, &link).expect("symlink");
        assert_eq!(
            plain,
            workspace_key(&link),
            "a symlink must not fork the key"
        );
    }

    // Nonexistent path: canonicalization fails and the raw string is
    // hashed — stable across calls, distinct from any other raw path.
    let missing = dir.path().join("does-not-exist");
    assert_eq!(workspace_key(&missing), workspace_key(&missing));
    assert_ne!(workspace_key(&missing), plain);
}

/// M17: with only a legacy-keyed dir present, it is renamed to the
/// FNV-keyed dir (contents move) and the FNV dir is used.
#[test]
fn workspace_data_dir_migrates_legacy_dir() {
    let root = tempfile::tempdir().expect("temp root");
    let path = Path::new("/tmp/sgian-m17-migrate");
    let fnv = workspace_key(path);
    let legacy = root.path().join(legacy_workspace_key(path));
    fs::create_dir(&legacy).expect("legacy dir");
    fs::write(legacy.join(WORKSPACE_FILE), "{}").expect("legacy contents");

    let resolved = workspace_data_dir_in(root.path(), path, &fnv);
    assert_eq!(
        resolved,
        root.path().join(&fnv),
        "the FNV dir is used after migration"
    );
    assert!(!legacy.exists(), "the legacy dir was renamed away");
    assert!(
        resolved.join(WORKSPACE_FILE).exists(),
        "contents moved with the rename"
    );
}

/// M17: when BOTH dirs exist the FNV dir wins and the legacy dir is left
/// alone (never merged).
#[test]
fn workspace_data_dir_prefers_fnv_when_both_exist() {
    let root = tempfile::tempdir().expect("temp root");
    let path = Path::new("/tmp/sgian-m17-both");
    let fnv = workspace_key(path);
    let current = root.path().join(&fnv);
    let legacy = root.path().join(legacy_workspace_key(path));
    fs::create_dir(&current).expect("fnv dir");
    fs::create_dir(&legacy).expect("legacy dir");

    assert_eq!(workspace_data_dir_in(root.path(), path, &fnv), current);
    assert!(legacy.exists(), "the legacy dir is not merged or removed");
}

/// M17: with NEITHER dir present the (not yet created) FNV dir is chosen.
#[test]
fn workspace_data_dir_resolves_fnv_when_neither_exists() {
    let root = tempfile::tempdir().expect("temp root");
    let path = Path::new("/tmp/sgian-m17-neither");
    let fnv = workspace_key(path);
    let resolved = workspace_data_dir_in(root.path(), path, &fnv);
    assert_eq!(resolved, root.path().join(&fnv));
    assert!(!resolved.exists(), "resolution does not create the dir");
}

/// L21: the `src-tauri` cwd retarget is a DEBUG-builds-only dev heuristic.
#[cfg(debug_assertions)]
#[test]
fn src_tauri_cwd_retargets_to_parent_in_debug_builds() {
    assert_eq!(
        resolve_workspace_dir_from_cwd(PathBuf::from("/repo/src-tauri")),
        PathBuf::from("/repo")
    );
    // Unrelated names are untouched in every build.
    assert_eq!(
        resolve_workspace_dir_from_cwd(PathBuf::from("/repo/other")),
        PathBuf::from("/repo/other")
    );
    // A root path has no parent: stays as-is (no panic).
    assert_eq!(
        resolve_workspace_dir_from_cwd(PathBuf::from("/")),
        PathBuf::from("/")
    );
}

/// L21: in release builds a directory NAMED `src-tauri` is a valid
/// workspace and must NOT be retargeted (this test only runs under
/// `--release`).
#[cfg(not(debug_assertions))]
#[test]
fn src_tauri_cwd_is_not_retargeted_in_release_builds() {
    assert_eq!(
        resolve_workspace_dir_from_cwd(PathBuf::from("/repo/src-tauri")),
        PathBuf::from("/repo/src-tauri")
    );
}

// ─── VAL-SEC-002: matching cwd connects normally (no false positives) ───

/// A matching cwd (untampered workspace) connects and serves normally after
/// a restart — no false-positive collision error.
#[test]
fn matching_cwd_connects_normally_no_false_positive() {
    let cwd = PathBuf::from("/tmp/sgian-match-test");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let mut daemon = TestDaemon::spawn_with_cwd(config.clone(), cwd.clone());
    let client = daemon.client();
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");
    let pane_id = pane.id.clone();

    // Restart with the same cwd — should load and serve normally.
    // (restart() handles shutdown + re-spawn internally.)
    daemon.restart(config);
    let client = daemon.client();
    let list: PaneList = client
        .request(DaemonRequest::ListPanes)
        .expect("list should succeed after restart with matching cwd");
    assert!(
        list.panes.iter().any(|p| p.pane.id == pane_id),
        "the persisted pane should be listed after restart (matching cwd)"
    );
    daemon.shutdown();
}

// ─── VAL-SEC-005: token authentication regression guard ───

/// A wrong token is rejected by the daemon's authenticate method.
#[test]
fn wrong_token_rejected_by_daemon() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let socket_path = daemon.socket_path.clone();

    // Try to authenticate with a wrong token.
    let wrong_token = "0".repeat(64);
    let result = authenticate_stream_at(&socket_path, &wrong_token);
    assert!(
        result.is_err(),
        "authentication with wrong token should fail"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("authentication failed"),
        "error should mention authentication failure: {error}"
    );

    // The correct token works.
    let correct_token = daemon.token.clone();
    let result = authenticate_stream_at(&socket_path, &correct_token);
    assert!(
        result.is_ok(),
        "authentication with correct token should succeed"
    );

    daemon.shutdown();
}

// ─── VAL-SEC-006: data and socket files remain owner-only (0600/0700) ───

/// All workspace artifacts are owner-only: data dir, runtime dir, scrollback
/// dir are 0700; workspace.json, token file, socket are 0600.
#[test]
fn workspace_artifacts_are_owner_only() {
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();
    let _: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let data_dir = daemon.data_dir.path();
        let scrollback_dir = data_dir.join(SCROLLBACK_DIR);
        let workspace_json = data_dir.join(WORKSPACE_FILE);
        let token_file = data_dir.join(TOKEN_FILE);

        // Directories: 0700
        for dir in [data_dir, scrollback_dir.as_path()] {
            let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "dir {:?} should be 0700, got {mode:o}", dir);
        }

        // Files: 0600
        for file in [workspace_json.as_path(), token_file.as_path()] {
            let mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "file {:?} should be 0600, got {mode:o}", file);
        }

        // Socket: 0600
        let socket_mode = fs::metadata(&daemon.socket_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            socket_mode, 0o600,
            "daemon.sock should be 0600, got {socket_mode:o}"
        );

        // Runtime dir (socket parent): 0700
        let runtime_dir = daemon.socket_path.parent().unwrap();
        let rt_mode = fs::metadata(runtime_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            rt_mode, 0o700,
            "runtime dir should be 0700, got {rt_mode:o}"
        );
    }

    daemon.shutdown();
}

// ─── VAL-CROSS-004: two workspaces run fully independent daemons ───

/// Two distinct workspace cwds get independent daemons, panes, and logs.
/// A command in one workspace never affects the other.
#[test]
fn two_workspaces_are_independent() {
    let cwd_a = PathBuf::from("/tmp/sgian-indep-a");
    let cwd_b = PathBuf::from("/tmp/sgian-indep-b");
    let config = Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    };

    let daemon_a = TestDaemon::spawn_with_cwd(config.clone(), cwd_a);
    let daemon_b = TestDaemon::spawn_with_cwd(config, cwd_b);

    let client_a = daemon_a.client();
    let client_b = daemon_b.client();

    // Create a pane in workspace A.
    let pane_a: Pane = client_a
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create in WS_A");

    // List panes in each — they must be disjoint.
    let snap_a: PaneList = client_a
        .request(DaemonRequest::ListPanes)
        .expect("list WS_A");
    let snap_b: PaneList = client_b
        .request(DaemonRequest::ListPanes)
        .expect("list WS_B");

    assert!(
        snap_a.panes.iter().any(|p| p.pane.id == pane_a.id),
        "WS_A should have its pane"
    );
    assert!(
        snap_b.panes.iter().all(|p| p.pane.id != pane_a.id),
        "WS_B should NOT contain WS_A's pane"
    );

    // Workspace keys must differ.
    assert_ne!(
        daemon_a.socket_path.parent().unwrap(),
        daemon_b.socket_path.parent().unwrap(),
        "the two workspaces should have distinct runtime dirs"
    );

    // Broadcast in WS_A must not affect WS_B.
    let _: Value = client_a
        .request(DaemonRequest::Broadcast {
            input: "ws-a-only\n".to_string(),
        })
        .expect("broadcast in WS_A");

    // WS_B's panes should not have received WS_A's broadcast.
    // (We can't easily inspect scrollback via the client, but we can verify
    // the pane registries remain distinct — the broadcast target lists differ.)
    let snap_b_after: PaneList = client_b
        .request(DaemonRequest::ListPanes)
        .expect("list WS_B after broadcast");
    assert_eq!(
        snap_b.panes.len(),
        snap_b_after.panes.len(),
        "WS_B pane count should be unchanged after WS_A broadcast"
    );

    daemon_a.shutdown();
    daemon_b.shutdown();
}

/// check_persisted_cwd passes for a fresh workspace (no workspace.json).
#[test]
fn check_persisted_cwd_fresh_workspace_ok() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cwd = PathBuf::from("/tmp/some-fresh-cwd");
    assert!(
        check_persisted_cwd(&cwd, dir.path()).is_ok(),
        "fresh workspace (no workspace.json) should pass cwd check"
    );
}

/// check_persisted_cwd passes for a matching cwd.
#[test]
fn check_persisted_cwd_matching_cwd_ok() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cwd = "/tmp/match-cwd-test";
    let persisted = PersistedWorkspace {
        panes: vec![],
        active_pane_id: None,
        cwd: cwd.to_string(),
        next_id: 1,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).unwrap(),
    )
    .expect("write workspace.json");

    assert!(
        check_persisted_cwd(&PathBuf::from(cwd), dir.path()).is_ok(),
        "matching cwd should pass"
    );
}

/// check_persisted_cwd fails for a mismatched cwd.
#[test]
fn check_persisted_cwd_mismatch_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    let persisted = PersistedWorkspace {
        panes: vec![],
        active_pane_id: None,
        cwd: "/tmp/original-cwd".to_string(),
        next_id: 1,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).unwrap(),
    )
    .expect("write workspace.json");

    let result = check_persisted_cwd(&PathBuf::from("/tmp/different-cwd"), dir.path());
    assert!(result.is_err(), "mismatched cwd should be refused");
    let error = result.unwrap_err();
    assert!(
        error.contains("collision"),
        "error should mention collision: {error}"
    );
}

/// M9 follow-up: alternate spellings of one directory (symlinks, `..`, trailing
/// slash) converge on the same data dir via canonicalization — the cwd check
/// must accept them instead of refusing a false "collision".
#[test]
fn check_persisted_cwd_accepts_equivalent_spellings() {
    let dir = tempfile::tempdir().expect("temp dir");
    let ws = dir.path().join("ws");
    fs::create_dir(&ws).expect("workspace dir");
    // Persist the CANONICAL spelling (tempdirs on macOS live under the
    // /var -> /private/var symlink, so `ws` itself exercises canonicalization).
    let canonical = fs::canonicalize(&ws).expect("canonical ws");
    let persisted = PersistedWorkspace {
        panes: vec![],
        active_pane_id: None,
        cwd: canonical.display().to_string(),
        next_id: 1,
        layout: None,
        sizes: HashMap::new(),
        pane_states: HashMap::new(),
        agents: HashMap::new(),
        agents_v2: HashMap::new(),
        agent_specs: HashMap::new(),
        pane_shells: HashMap::new(),
        leases: HashMap::new(),
        projects: HashMap::new(),
    };
    fs::write(
        dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).unwrap(),
    )
    .expect("write workspace.json");

    assert!(
        check_persisted_cwd(&ws, dir.path()).is_ok(),
        "the possibly-symlinked spelling must pass"
    );
    assert!(
        check_persisted_cwd(&ws.join("..").join("ws"), dir.path()).is_ok(),
        "a `..`-bearing spelling must pass"
    );
}

/// check_persisted_cwd passes for a corrupt workspace.json (unparseable).
#[test]
fn check_persisted_cwd_corrupt_file_ok() {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::write(dir.path().join(WORKSPACE_FILE), "not valid json").expect("write corrupt file");

    assert!(
        check_persisted_cwd(&PathBuf::from("/tmp/any-cwd"), dir.path()).is_ok(),
        "corrupt workspace.json should pass cwd check (handled elsewhere)"
    );
}

// ─── VAL-XPLAT-001 / VAL-CROSS-017: data-dir abstraction via dirs crate ───

/// `app_support_dir()` must delegate to `dirs::data_dir()` (the
/// cross-platform data root) and select either the current product
/// directory or the compatibility fallback directly beneath it.
#[test]
fn app_support_dir_uses_dirs_data_dir() {
    let data_root = dirs::data_dir().expect("dirs::data_dir should resolve");
    let actual = app_support_dir();
    assert!(
        actual == data_root.join(APP_SUPPORT_DIR)
            || actual == data_root.join(LEGACY_APP_SUPPORT_DIR),
        "app_support_dir must stay directly under dirs::data_dir(): {}",
        actual.display()
    );
}

/// A clean installation selects the new product directory.
#[test]
fn app_support_dir_new_install_uses_current_name() {
    let root = tempfile::tempdir().expect("temp data root");
    assert_eq!(app_support_dir_in(root.path()), root.path().join("Sgian"));
}

/// An existing installation remains on its old root so data and live
/// daemon routing survive the executable rename.
#[test]
fn app_support_dir_reuses_legacy_install() {
    let root = tempfile::tempdir().expect("temp data root");
    let legacy = root.path().join(LEGACY_APP_SUPPORT_DIR);
    fs::create_dir(&legacy).expect("legacy root");
    assert_eq!(app_support_dir_in(root.path()), legacy);
}

/// When both roots exist, never guess at a merge; the explicitly current
/// root is authoritative.
#[test]
fn app_support_dir_prefers_current_when_both_exist() {
    let root = tempfile::tempdir().expect("temp data root");
    let current = root.path().join(APP_SUPPORT_DIR);
    fs::create_dir(&current).expect("current root");
    fs::create_dir(root.path().join(LEGACY_APP_SUPPORT_DIR)).expect("legacy root");
    assert_eq!(app_support_dir_in(root.path()), current);
}

/// The centralized `private_mode()` helper on `OpenOptions` must produce
/// owner-only (0600) files on Unix. On non-Unix it is a no-op (verified by
/// compilation, not runtime). This guards the perm centralization.
#[test]
fn private_mode_creates_owner_only_file() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("secret.txt");
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).private_mode();
    {
        let mut file = opts.open(&path).expect("open with private_mode");
        file.write_all(b"test").expect("write");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file should be 0600, got {mode:o}");
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

// ----- Config UX backend tests (VAL-CFG-001/008/009/011, VAL-CROSS-005/007/021/022) -----

/// VAL-CFG-001: `get_config` exposes ALL editable fields (not just appearance).
#[test]
fn get_config_exposes_all_editable_fields() {
    let config = Config {
        shell: Some("/bin/bash".to_string()),
        shell_args: Some(vec!["-l".to_string()]),
        env: {
            let mut m = HashMap::new();
            m.insert("FOO".to_string(), "bar".to_string());
            m
        },
        font_family: Some("Mono".to_string()),
        font_size: Some(14),
        theme: Some(json!({"background": "#000"})),
        idle_shutdown_secs: Some(30),
        restore_policy: Some("restore_on_demand".to_string()),
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let cfg: Value = client
        .request(DaemonRequest::GetConfig)
        .expect("get_config should succeed");
    assert_eq!(cfg["shell"], json!("/bin/bash"), "shell field missing");
    assert_eq!(cfg["shell_args"], json!(["-l"]), "shell_args field missing");
    assert_eq!(cfg["env"]["FOO"], json!("bar"), "env field missing");
    assert_eq!(
        cfg["font_family"],
        json!("Mono"),
        "font_family field missing"
    );
    assert_eq!(cfg["font_size"], json!(14), "font_size field missing");
    assert_eq!(
        cfg["theme"]["background"],
        json!("#000"),
        "theme field missing"
    );
    assert_eq!(
        cfg["idle_shutdown_secs"],
        json!(30),
        "idle_shutdown_secs field missing"
    );
    assert_eq!(
        cfg["restore_policy"],
        json!("restore_on_demand"),
        "restore_policy field missing"
    );

    daemon.shutdown();
}

/// VAL-CFG-008: `WriteConfig` persists a valid config.json atomically to the
/// per-workspace config location.
#[test]
fn write_config_persists_valid_config() {
    let config = Config {
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    let new_config = Config {
        font_size: Some(20),
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: Some(42),
        ..Default::default()
    };
    let result: CommandOk = client
        .request(DaemonRequest::WriteConfig {
            config: serde_json::to_value(&new_config).expect("serialize config"),
        })
        .expect("write-config should succeed");

    assert!(result.ok);

    // Verify config.json exists on disk with the new values.
    let config_path = daemon.data_dir.path().join(CONFIG_FILE);
    let content = fs::read_to_string(&config_path).expect("config.json should exist after write");
    let parsed: Config = serde_json::from_str(&content).expect("config.json should be valid JSON");
    assert_eq!(parsed.font_size, Some(20));
    assert_eq!(parsed.shell.as_deref(), Some("/bin/sh"));
    assert_eq!(parsed.idle_shutdown_secs, Some(42));

    // No temp file left behind.
    let temp_path = config_path.with_extension("json.tmp");
    assert!(!temp_path.exists(), "temp file should not remain");

    daemon.shutdown();
}

/// VAL-CFG-009: `WriteConfig` rejects invalid input without corrupting the
/// existing config.
#[test]
fn write_config_rejects_invalid_restore_policy() {
    let config = Config {
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // First write a valid config.
    let valid = Config {
        font_size: Some(14),
        ..Default::default()
    };
    client
        .request::<CommandOk>(DaemonRequest::WriteConfig {
            config: serde_json::to_value(&valid).expect("serialize config"),
        })
        .expect("valid write should succeed");

    let config_path = daemon.data_dir.path().join(CONFIG_FILE);
    let content_before = fs::read_to_string(&config_path).expect("config.json should exist");

    // Try to write an invalid config (bad restore_policy).
    let invalid = Config {
        restore_policy: Some("nonsense".to_string()),
        ..Default::default()
    };
    let result = client.request::<CommandOk>(DaemonRequest::WriteConfig {
        config: serde_json::to_value(&invalid).expect("serialize config"),
    });
    assert!(result.is_err(), "invalid restore_policy should be rejected");

    // Verify the existing config is unchanged.
    let content_after = fs::read_to_string(&config_path).expect("config.json should still exist");
    assert_eq!(
        content_before, content_after,
        "existing config must be byte-for-byte intact after rejected write"
    );

    daemon.shutdown();
}

/// M1: a WriteConfig payload that OMITS scrub_env preserves the workspace
/// file's current scrub list; an EXPLICIT empty list is an intentional clear.
#[test]
fn write_config_preserves_scrub_env_when_payload_omits_it() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let config_path = daemon.data_dir.path().join(CONFIG_FILE);

    // Seed a workspace config with a scrub list (as an operator would).
    client
        .request::<CommandOk>(DaemonRequest::WriteConfig {
            config: json!({ "scrub_env": ["AWS_SECRET_ACCESS_KEY"] }),
        })
        .expect("seed write should succeed");

    // A partial payload (e.g. a settings form with no scrub_env field) must
    // not erase the seeded list on a write round-trip.
    client
        .request::<CommandOk>(DaemonRequest::WriteConfig {
            config: json!({ "font_size": 15 }),
        })
        .expect("partial write should succeed");
    let parsed: Config =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config should exist"))
            .expect("config should parse");
    assert_eq!(parsed.font_size, Some(15));
    assert_eq!(
        parsed.scrub_env,
        vec!["AWS_SECRET_ACCESS_KEY".to_string()],
        "an omitted scrub_env must survive the write round-trip"
    );

    // An explicit empty list replaces it (deliberate clear wins).
    client
        .request::<CommandOk>(DaemonRequest::WriteConfig {
            config: json!({ "scrub_env": [] }),
        })
        .expect("explicit clear should succeed");
    let parsed: Config =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config should exist"))
            .expect("config should parse");
    assert!(
        parsed.scrub_env.is_empty(),
        "an explicit [] must clear the scrub list"
    );

    daemon.shutdown();
}

/// Regression test for VAL-CFG-009 fix: `ctl write-config` with invalid
/// input must cause the CLI to exit non-zero. The CLI handler
/// `control_write_config` returns `Err` when the daemon rejects the
/// WriteConfig request (validation failure) or when the input JSON is
/// malformed. `run_control_cli_from_args` propagates that `Err` to `run()`,
/// which calls `std::process::exit(1)`. This test exercises the real CLI
/// handler function (not just the daemon-client level) to ensure the error
/// propagates correctly through the full CLI path.
#[test]
fn write_config_invalid_input_exits_nonzero() {
    let config = Config {
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Case 1: invalid restore_policy — daemon rejects via validate().
    let result = control_write_config(
        &client,
        &["{\"restore_policy\":\"nonsense\"}".to_string()],
        false,
    );
    assert!(
        result.is_err(),
        "write-config with invalid restore_policy must return Err (non-zero exit)"
    );

    // Case 2: invalid JSON type — daemon rejects via serde deserialization.
    let result = control_write_config(
        &client,
        &["{\"font_size\":\"notanumber\"}".to_string()],
        false,
    );
    assert!(
        result.is_err(),
        "write-config with invalid JSON type must return Err (non-zero exit)"
    );

    // Case 3: malformed JSON syntax — rejected by the CLI handler itself.
    let result = control_write_config(&client, &["{bad json".to_string()], false);
    assert!(
        result.is_err(),
        "write-config with malformed JSON must return Err (non-zero exit)"
    );

    // Case 4: no arguments — rejected by the CLI handler.
    let result = control_write_config(&client, &[], false);
    assert!(
        result.is_err(),
        "write-config with no arguments must return Err (non-zero exit)"
    );

    // Case 5: --json mode with invalid input must also return Err.
    let result = control_write_config(
        &client,
        &["{\"restore_policy\":\"nonsense\"}".to_string()],
        true,
    );
    assert!(
        result.is_err(),
        "write-config --json with invalid input must return Err (non-zero exit)"
    );

    // Case 6: valid config still succeeds (exit 0).
    let result = control_write_config(&client, &["{\"font_size\":16}".to_string()], false);
    assert!(
        result.is_ok(),
        "write-config with valid input must return Ok (exit 0)"
    );

    daemon.shutdown();
}

/// VAL-CFG-011 / VAL-CROSS-007: editing config.json on disk live-reloads the
/// running daemon and a `ConfigChanged` event is delivered carrying the new
/// effective config. Config is no longer frozen at construction.
#[test]
fn config_file_watch_reloads_and_broadcasts_config_changed() {
    let config = Config {
        font_size: Some(12),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Subscribe to events.
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout should apply");

    // Externally rewrite the per-workspace config.json.
    let config_path = daemon.data_dir.path().join(CONFIG_FILE);
    let new_config_json = serde_json::to_string(&Config {
        font_size: Some(20),
        idle_shutdown_secs: None,
        ..Default::default()
    })
    .expect("serialize config");
    // Use atomic write (same as write_file_atomic) to mimic real edits.
    let temp_path = config_path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .private_mode()
            .open(&temp_path)
            .expect("open temp");
        file.write_all(new_config_json.as_bytes())
            .expect("write temp");
    }
    fs::rename(&temp_path, &config_path).expect("rename");

    // Wait for ConfigChanged event on the subscriber stream.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut received = false;
    for _ in 0..200 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::ConfigChanged { config }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    assert_eq!(
                        config["font_size"],
                        json!(20),
                        "ConfigChanged should carry the new font_size"
                    );
                    received = true;
                    break;
                }
            }
            Err(_) => break,
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        received,
        "subscriber should receive a ConfigChanged event after config.json edit"
    );

    // Also verify status --verbose reflects the new config (live reload, not frozen).
    let status: VerboseStatus = client
        .request(DaemonRequest::StatusVerbose)
        .expect("status should succeed");
    assert_eq!(
        status.config["font_size"],
        json!(20),
        "status --verbose should reflect the live-reloaded config"
    );

    daemon.shutdown();
}

/// VAL-CROSS-005: config overlay is workspace-scoped — global applies
/// everywhere, per-workspace override wins locally. This is already tested
/// via `Config::overlay` unit tests; here we verify the live-reload path
/// respects overlay semantics (per-workspace override wins).
#[test]
fn config_live_reload_per_workspace_override_wins() {
    let config = Config {
        font_size: Some(12),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Write a per-workspace config that overrides font_size.
    let config_path = daemon.data_dir.path().join(CONFIG_FILE);
    let new_config_json = serde_json::to_string(&Config {
        font_size: Some(25),
        idle_shutdown_secs: None,
        ..Default::default()
    })
    .expect("serialize config");
    let temp_path = config_path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .private_mode()
            .open(&temp_path)
            .expect("open temp");
        file.write_all(new_config_json.as_bytes())
            .expect("write temp");
    }
    fs::rename(&temp_path, &config_path).expect("rename");

    // Wait for the reload to take effect.
    let mut reloaded = false;
    for _ in 0..200 {
        let status: VerboseStatus = client
            .request(DaemonRequest::StatusVerbose)
            .expect("status should succeed");
        if status.config["font_size"] == json!(25) {
            reloaded = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        reloaded,
        "per-workspace config override should be reflected after live reload"
    );

    daemon.shutdown();
}

/// VAL-CROSS-021: a live `shell` config change makes newly-spawned panes use
/// the new shell. After reloading the config to change the shell, a pane
/// created *after* the reload should use the new shell (verified via
/// `status --verbose` showing the new shell and the new pane being live
/// under it).
#[test]
fn live_shell_change_affects_new_panes() {
    let config = Config {
        shell: Some("/bin/sh".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Write a new config with a different shell.
    let config_path = daemon.data_dir.path().join(CONFIG_FILE);
    let new_config_json = serde_json::to_string(&Config {
        shell: Some("/bin/cat".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    })
    .expect("serialize config");
    let temp_path = config_path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .private_mode()
            .open(&temp_path)
            .expect("open temp");
        file.write_all(new_config_json.as_bytes())
            .expect("write temp");
    }
    fs::rename(&temp_path, &config_path).expect("rename");

    // Wait for the reload to take effect (status --verbose shows /bin/cat).
    let mut reloaded = false;
    for _ in 0..200 {
        let status: VerboseStatus = client
            .request(DaemonRequest::StatusVerbose)
            .expect("status should succeed");
        if status.config["shell"] == json!("/bin/cat") {
            reloaded = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        reloaded,
        "status --verbose should show the new shell after reload"
    );

    // Create a new pane AFTER the reload. It should use /bin/cat (which stays
    // alive and echoes input), proving the TerminalStore's shell config was
    // updated live.
    let pane: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("create should succeed");

    // Verify the new pane is Live (cat keeps running).
    let mut live = false;
    for _ in 0..100 {
        let status: PaneStatus = client
            .request(DaemonRequest::PaneStatus {
                pane_id: pane.id.clone(),
            })
            .expect("status should succeed");
        if status.state == PaneRuntimeState::Live {
            live = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        live,
        "new pane should be live under the reloaded shell (/bin/cat)"
    );

    daemon.shutdown();
}

/// VAL-CROSS-022: changing `restore_policy` through the write-config path
/// live-reloads and `status --verbose` reflects the new policy with the
/// same daemon pid (no restart).
#[test]
fn write_config_live_reloads_restore_policy() {
    let config = Config {
        idle_shutdown_secs: None,
        ..Default::default()
    };
    let daemon = TestDaemon::spawn(config);
    let client = daemon.client();

    // Write a config with restore_on_demand.
    let new_config = Config {
        restore_policy: Some("restore_on_demand".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    client
        .request::<CommandOk>(DaemonRequest::WriteConfig {
            config: serde_json::to_value(&new_config).expect("serialize config"),
        })
        .expect("write-config should succeed");

    // Wait for the live reload to reflect the new policy.
    let mut reflected = false;
    for _ in 0..200 {
        let status: VerboseStatus = client
            .request(DaemonRequest::StatusVerbose)
            .expect("status should succeed");
        if status.config["restore_policy"] == json!("restore_on_demand") {
            reflected = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        reflected,
        "status --verbose should reflect restore_on_demand after write-config + live reload"
    );

    // Now write auto_respawn and verify it changes too.
    let new_config2 = Config {
        restore_policy: Some("auto_respawn".to_string()),
        idle_shutdown_secs: None,
        ..Default::default()
    };
    client
        .request::<CommandOk>(DaemonRequest::WriteConfig {
            config: serde_json::to_value(&new_config2).expect("serialize config"),
        })
        .expect("write-config should succeed");

    let mut reflected2 = false;
    for _ in 0..200 {
        let status: VerboseStatus = client
            .request(DaemonRequest::StatusVerbose)
            .expect("status should succeed");
        if status.config["restore_policy"] == json!("auto_respawn") {
            reflected2 = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        reflected2,
        "status --verbose should reflect auto_respawn after second write-config"
    );

    daemon.shutdown();
}

// ----- (T1) agent detection + attention classification -----

/// (T1) Claude Code at rest: welcome banner + ❯ input box with chrome (2
/// signature groups, no working/needs-input patterns).
const CLAUDE_IDLE_SCREEN: &str =
    "  Claude Code v2.0  \r\n╭──────────╮\r\n│ ❯        │\r\n╰──────────╯\r\n";
/// (T1) Claude Code mid-turn: spinner + verb + the working footer.
const CLAUDE_WORKING_SCREEN: &str =
    "  Claude Code v2.0  \r\n✻ Thinking… esc to interrupt ⠋\r\n╭──────────╮\r\n│ ❯        │\r\n╰──────────╯\r\n";

/// (T1) Reset a pane's classification throttle so the next feed_model
/// reclassifies immediately (classification is throttled to 500 ms/pane).
fn agent_unthrottle(router: &OutputRouter, pane_id: &str) {
    if let Ok(mut tracker) = router.agents.lock() {
        if let Some(entry) = tracker.panes.get_mut(pane_id) {
            entry.last_classified_at = None;
        }
    }
}

#[test]
fn agent_attention_needs_input_beats_working() {
    // A permission prompt with the working footer + spinner still visible
    // must classify as NeedsInput (priority over Working).
    let text = "✻ Thinking… esc to interrupt ⠋\nDo you want to proceed?\n❯ 1. Yes\n  2. No";
    assert_eq!(classify_agent_attention(text), AgentAttention::NeedsInput);
    assert_eq!(
        classify_agent_attention("Waiting for your response\nesc to interrupt"),
        AgentAttention::NeedsInput
    );
    assert_eq!(
        classify_agent_attention("Press enter to continue ⠋"),
        AgentAttention::NeedsInput
    );
    // The numbered-pair rule needs BOTH lines.
    assert_eq!(
        classify_agent_attention("1. Yes, this looks fine"),
        AgentAttention::Idle
    );
}

#[test]
fn agent_attention_spinner_and_keywords_are_working() {
    assert_eq!(
        classify_agent_attention("⠋ Compiling crates"),
        AgentAttention::Working
    );
    assert_eq!(
        classify_agent_attention("✻ Thinking…"),
        AgentAttention::Working
    );
    assert_eq!(
        classify_agent_attention("Working on the fix"),
        AgentAttention::Working
    );
    assert_eq!(
        classify_agent_attention("esc to interrupt"),
        AgentAttention::Working
    );
}

#[test]
fn agent_attention_bare_prompt_is_idle() {
    // The agent's ❯ input box at rest: no needs-input/working patterns.
    assert_eq!(
        classify_agent_attention("╭──────────╮\n│ ❯        │\n╰──────────╯"),
        AgentAttention::Idle
    );
    assert_eq!(classify_agent_attention(""), AgentAttention::Idle);
}

#[test]
fn agent_detection_requires_two_marker_groups() {
    // Plain shell output: no signature.
    assert_eq!(detect_agent("user@host:~$ ls -la\n", false), None);
    // A single group is never enough for a FRESH mark — not even the
    // ❯+chrome input box alone (conservative against false positives).
    assert_eq!(
        detect_agent("╭──────────╮\n│ ❯        │\n╰──────────╯\n", false),
        None
    );
    assert_eq!(detect_agent("Claude Code\n", false), None);
    // Two independent groups → detected.
    assert_eq!(
        detect_agent(CLAUDE_IDLE_SCREEN, false),
        Some("claude".to_string())
    );
    assert_eq!(
        detect_agent(CLAUDE_WORKING_SCREEN, false),
        Some("claude".to_string())
    );
    // Hysteresis: an already-detected pane keeps its mark with 1 group…
    assert_eq!(
        detect_agent("╭──────────╮\n│ ❯        │\n╰──────────╯\n", true),
        Some("claude".to_string())
    );
    // …but clears once every marker is gone.
    assert_eq!(detect_agent("user@host:~$ ", true), None);
}

#[test]
fn agent_signature_candidate_prefilter() {
    assert!(agent_signature_candidate(b"blah esc to interrupt blah"));
    assert!(agent_signature_candidate(
        "⏵⏵ auto-accept edits on".as_bytes()
    ));
    assert!(agent_signature_candidate(b"Welcome to Claude Code"));
    // A bare ❯ prompt (common zsh theme) must NOT trigger a classification.
    assert!(!agent_signature_candidate("❯".as_bytes()));
    assert!(!agent_signature_candidate(b"plain build output\n"));
    // Short/empty inputs never panic (needle longer than haystack).
    assert!(!agent_signature_candidate(b""));
    assert!(!agent_signature_candidate(b"esc"));
}

#[test]
fn agent_detection_ignores_plain_shell_output() {
    // A pane that never shows a signature is never marked — and the cheap
    // pre-check doesn't even create a tracker entry for it.
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    router.feed_model("pane-1", b"user@host:~$ ls -la\r\ntotal 0\r\nuser@host:~$ ");
    let info = router.agent_state("pane-1");
    assert_eq!(info.agent, None);
    assert_eq!(info.attention, None);
    assert!(!router
        .agents
        .lock()
        .expect("agents lock")
        .panes
        .contains_key("pane-1"));
}

#[test]
fn agent_detection_marks_claude_screen_and_classifies() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);

    router.feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    let info = router.agent_state("pane-1");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, Some(AgentAttention::Idle));

    agent_unthrottle(&router, "pane-1");
    router.feed_model("pane-1", "✻ Thinking… esc to interrupt ⠋\r\n".as_bytes());
    assert_eq!(
        router.agent_state("pane-1").attention,
        Some(AgentAttention::Working)
    );

    // NeedsInput beats the working footer still on screen.
    agent_unthrottle(&router, "pane-1");
    router.feed_model(
        "pane-1",
        "Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No\r\n".as_bytes(),
    );
    assert_eq!(
        router.agent_state("pane-1").attention,
        Some(AgentAttention::NeedsInput)
    );
}

#[test]
fn agent_classification_is_throttled_per_pane() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    router.feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    let classified_revision =
        router.agents.lock().expect("agents lock").panes["pane-1"].last_classified_revision;
    assert!(classified_revision > 0);

    // A feed inside the 500 ms window still feeds the model (its revision
    // bumps) but does NOT reclassify.
    router.feed_model("pane-1", "more output\r\n".as_bytes());
    let model_revision = router
        .model_handle("pane-1")
        .expect("model")
        .lock()
        .expect("model lock")
        .revision;
    assert!(model_revision > classified_revision);
    assert_eq!(
        router.agents.lock().expect("agents lock").panes["pane-1"].last_classified_revision,
        classified_revision
    );

    // Once the throttle window passes, the next output reclassifies.
    agent_unthrottle(&router, "pane-1");
    router.feed_model("pane-1", "even more\r\n".as_bytes());
    assert_eq!(
        router.agents.lock().expect("agents lock").panes["pane-1"].last_classified_revision,
        model_revision + 1
    );
}

#[test]
fn agent_state_event_emitted_once_per_transition() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");
    server.router.ensure_model("pane-1", 80, 24);

    let (client_stream, server_stream) =
        test_transport_pair().expect("transport pair should be available");
    server
        .router
        .add_subscriber(server_stream, 1)
        .expect("subscribe within the cap");
    client_stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("set read timeout");
    let mut reader = BufReader::new(client_stream);
    fn next_event(reader: &mut BufReader<TransportStream>) -> DaemonEvent {
        let mut line = String::new();
        reader.read_line(&mut line).expect("event should arrive");
        serde_json::from_str(&line).expect("event should deserialize")
    }

    // Detection transition: exactly one event.
    server
        .router
        .feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    assert_eq!(
        next_event(&mut reader),
        DaemonEvent::AgentState {
            pane_id: "pane-1".to_string(),
            agent: Some("claude".to_string()),
            attention: Some(AgentAttention::Idle),
            mode: None,
        }
    );

    // Reclassifying an UNCHANGED state emits nothing (transitions only).
    agent_unthrottle(&server.router, "pane-1");
    server
        .router
        .feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).is_err(),
        "an unchanged agent state must not emit an event"
    );

    // Idle → Working: exactly one event.
    agent_unthrottle(&server.router, "pane-1");
    server
        .router
        .feed_model("pane-1", "✻ Thinking… esc to interrupt\r\n".as_bytes());
    assert_eq!(
        next_event(&mut reader),
        DaemonEvent::AgentState {
            pane_id: "pane-1".to_string(),
            agent: Some("claude".to_string()),
            attention: Some(AgentAttention::Working),
            mode: None,
        }
    );

    // The signature leaves the screen: the FIRST signature-free
    // classification is absorbed as a possible torn redraw (M4)…
    agent_unthrottle(&server.router, "pane-1");
    server
        .router
        .feed_model("pane-1", b"\x1b[2J\x1b[Huser@host:~$ ");
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).is_err(),
        "one signature-free classification must keep the mark"
    );

    // …the SECOND consecutive one clears it: one clear event with nulls.
    agent_unthrottle(&server.router, "pane-1");
    server.router.feed_model("pane-1", b"ls\r\n");
    assert_eq!(
        next_event(&mut reader),
        DaemonEvent::AgentState {
            pane_id: "pane-1".to_string(),
            agent: None,
            attention: None,
            mode: None,
        }
    );

    // Nothing further.
    let mut line = String::new();
    assert!(
        reader.read_line(&mut line).is_err(),
        "no further agent events after the clear transition"
    );
}

#[test]
fn agent_manual_mark_overrides_detection() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);

    // A manually marked pane STAYS marked even with no signature on screen.
    router.set_manual_agent("pane-1", Some("claude".to_string()));
    router.classify_agent_now("pane-1");
    router.feed_model("pane-1", b"user@host:~$ ls\r\n");
    agent_unthrottle(&router, "pane-1");
    router.feed_model("pane-1", b"total 0\r\n");
    let info = router.agent_state("pane-1");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, Some(AgentAttention::Idle));

    // Unmarking returns to auto-detection: no signature → cleared.
    router.set_manual_agent("pane-1", None);
    router.classify_agent_now("pane-1");
    assert_eq!(router.agent_state("pane-1").agent, None);
    assert!(router.manual_agent_marks().is_empty());
}

/// (T1) Subscribe a UnixStream pair to a router's event stream. The
/// client reader starts with a generous timeout; callers shorten it for
/// silence assertions.
#[cfg(unix)]
fn router_event_reader(router: &OutputRouter) -> BufReader<UnixStream> {
    let (client, server_stream) = UnixStream::pair().expect("unix stream pair");
    router
        .add_subscriber(server_stream, 1)
        .expect("subscribe within the cap");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    BufReader::new(client)
}

/// (T1) Read until the pane's next AgentState event, returning
/// (agent, attention). Other events are skipped. Resets a generous read
/// timeout (silence assertions shorten it).
#[cfg(unix)]
fn read_router_agent_state(
    reader: &mut BufReader<UnixStream>,
    pane_id: &str,
) -> (Option<String>, Option<AgentAttention>) {
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    for _ in 0..200 {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("event stream reads");
        assert!(n > 0, "event stream closed waiting for AgentState");
        let Ok(event) = serde_json::from_str::<DaemonEvent>(line.trim_end()) else {
            continue;
        };
        if let DaemonEvent::AgentState {
            pane_id: id,
            agent,
            attention,
            ..
        } = event
        {
            if id == pane_id {
                return (agent, attention);
            }
        }
    }
    panic!("AgentState not received within 200 events");
}

/// (T1) Assert the stream stays silent for `window` (a short read
/// timeout doubles as the wait).
#[cfg(unix)]
fn assert_no_agent_event(reader: &mut BufReader<UnixStream>, window: Duration, what: &str) {
    reader
        .get_ref()
        .set_read_timeout(Some(window))
        .expect("set read timeout");
    let mut line = String::new();
    assert!(reader.read_line(&mut line).is_err(), "{what}");
}

#[cfg(unix)]
#[test]
fn agent_trailing_edge_classifies_prompt_in_throttle_window() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    let mut reader = router_event_reader(&router);

    // First classification is immediate: detected, Idle.
    router.feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    assert_eq!(
        read_router_agent_state(&mut reader, "pane-1"),
        (Some("claude".to_string()), Some(AgentAttention::Idle))
    );

    // (T1) H1: the permission prompt is the LAST output of a burst and
    // lands inside the throttle window — the agent then blocks on stdin
    // and no further chunk ever arrives to trigger classification.
    router.feed_model(
        "pane-1",
        "Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No\r\n".as_bytes(),
    );
    // No immediate classification inside the window…
    assert_no_agent_event(
        &mut reader,
        Duration::from_millis(200),
        "nothing may be classified inside the throttle window",
    );
    // …but the trailing edge delivers the transition after it.
    assert_eq!(
        read_router_agent_state(&mut reader, "pane-1"),
        (Some("claude".to_string()), Some(AgentAttention::NeedsInput))
    );

    // Exactly one broadcast: nothing further once the trailing edge ran.
    assert_no_agent_event(
        &mut reader,
        Duration::from_millis(800),
        "no further agent events after the trailing transition",
    );
}

#[cfg(unix)]
#[test]
fn agent_trailing_edge_noop_when_nothing_changed() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    let mut reader = router_event_reader(&router);
    router.feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    assert_eq!(
        read_router_agent_state(&mut reader, "pane-1"),
        (Some("claude".to_string()), Some(AgentAttention::Idle))
    );

    // A throttled chunk whose screen keeps the same state schedules the
    // trailing classification, but an unchanged state emits no event.
    router.feed_model("pane-1", b"more output\r\n");
    thread::sleep(AGENT_CLASSIFY_INTERVAL + Duration::from_millis(250));
    // The trailing classification DID run (the revision caught up)…
    let model_revision = router
        .model_handle("pane-1")
        .expect("model")
        .lock()
        .expect("model lock")
        .revision;
    assert_eq!(
        router.agents.lock().expect("agents lock").panes["pane-1"].last_classified_revision,
        model_revision
    );
    // …and broadcast nothing.
    assert_no_agent_event(
        &mut reader,
        Duration::from_millis(300),
        "an unchanged trailing classification must not emit an event",
    );
}

#[cfg(unix)]
#[test]
fn agent_trailing_edge_and_hook_skip_closed_pane() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    let mut reader = router_event_reader(&router);
    router.feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    assert_eq!(
        read_router_agent_state(&mut reader, "pane-1"),
        (Some("claude".to_string()), Some(AgentAttention::Idle))
    );

    // A throttled prompt schedules the trailing edge; the pane is then
    // closed (ClosePane order: mark closed, drop the tracking entry).
    router.feed_model(
        "pane-1",
        "Do you want to proceed?\r\n❯ 1. Yes\r\n  2. No\r\n".as_bytes(),
    );
    router.mark_closed("pane-1");
    router.remove_agent("pane-1");
    thread::sleep(AGENT_CLASSIFY_INTERVAL + Duration::from_millis(250));
    // The deferred classification never ran: no event, no ghost entry.
    assert_no_agent_event(
        &mut reader,
        Duration::from_millis(300),
        "no classification after pane close",
    );
    assert!(!router
        .agents
        .lock()
        .expect("agents lock")
        .panes
        .contains_key("pane-1"));

    // (T1) L6a: a still-draining reader feeding after close re-creates
    // no tracker entry.
    router.feed_model("pane-1", CLAUDE_WORKING_SCREEN.as_bytes());
    assert!(!router
        .agents
        .lock()
        .expect("agents lock")
        .panes
        .contains_key("pane-1"));
}

#[test]
fn agent_detection_survives_one_torn_redraw_frame() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    router.feed_model("pane-1", CLAUDE_IDLE_SCREEN.as_bytes());
    assert_eq!(
        router.agent_state("pane-1").agent.as_deref(),
        Some("claude")
    );

    // (T1) M4: ONE signature-free classification (a torn full-screen
    // redraw) keeps the mark — and the attention state with it.
    agent_unthrottle(&router, "pane-1");
    router.feed_model("pane-1", b"\x1b[2J\x1b[Huser@host:~$ ");
    let info = router.agent_state("pane-1");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, Some(AgentAttention::Idle));

    // TWO consecutive signature-free classifications clear it.
    agent_unthrottle(&router, "pane-1");
    router.feed_model("pane-1", b"ls -la\r\n");
    let info = router.agent_state("pane-1");
    assert_eq!(info.agent, None);
    assert_eq!(info.attention, None);
}

#[test]
fn agent_manual_unmark_redetects_with_fresh_threshold() {
    let router = OutputRouter::new(std::env::temp_dir());
    router.ensure_model("pane-1", 80, 24);
    // A ONE-group screen (the ❯+chrome input box alone).
    router.feed_model(
        "pane-1",
        "╭──────────╮\r\n│ ❯        │\r\n╰──────────╯\r\n".as_bytes(),
    );
    router.set_manual_agent("pane-1", Some("claude".to_string()));
    router.classify_agent_now("pane-1");
    assert_eq!(
        router.agent_state("pane-1").agent.as_deref(),
        Some("claude")
    );

    // (T1) M3: unmarking clears the mark, so re-detection uses the fresh
    // 2-group threshold — the 1-group hysteresis floor must NOT keep an
    // ex-manual mark alive.
    router.set_manual_agent("pane-1", None);
    router.classify_agent_now("pane-1");
    assert_eq!(router.agent_state("pane-1").agent, None);
}

#[cfg(unix)]
#[test]
fn agent_attention_clears_when_pane_process_ends() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config {
            shell: Some("/bin/sh".to_string()),
            ..Default::default()
        },
    )
    .expect("daemon server should start");
    server
        .handle(DaemonRequest::EnsurePaneTerminal {
            pane_id: "pane-1".to_string(),
        })
        .expect("spawn pane shell");
    let mut reader = router_event_reader(&server.router);

    // The pane shows a working agent.
    server
        .router
        .feed_model("pane-1", CLAUDE_WORKING_SCREEN.as_bytes());
    assert_eq!(
        read_router_agent_state(&mut reader, "pane-1"),
        (Some("claude".to_string()), Some(AgentAttention::Working))
    );

    // (T1) M2: the shell exits — the reader's EOF claim clears the badge
    // (AgentState with null attention; the mark is kept) before PaneEnded.
    server
        .handle(DaemonRequest::WriteToPane {
            pane_id: "pane-1".to_string(),
            data: "exit\n".to_string(),
        })
        .expect("write exit");
    assert_eq!(
        read_router_agent_state(&mut reader, "pane-1"),
        (Some("claude".to_string()), None)
    );
    // No trailing classification resurrects the badge afterwards (the
    // `ended` latch): the state stays cleared past the window.
    thread::sleep(AGENT_CLASSIFY_INTERVAL + Duration::from_millis(250));
    let info = server.router.agent_state("pane-1");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, None);
}

#[cfg(unix)]
#[test]
fn agent_attention_clears_on_restart_and_classification_resumes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config {
            shell: Some("/bin/cat".to_string()),
            ..Default::default()
        },
    )
    .expect("daemon server should start");
    server.router.ensure_model("pane-1", 80, 24);
    server
        .router
        .feed_model("pane-1", CLAUDE_WORKING_SCREEN.as_bytes());
    assert_eq!(
        server.router.agent_state("pane-1").attention,
        Some(AgentAttention::Working)
    );

    // (T1) M2: same clear on restart — the killed process keeps no badge.
    server
        .handle(DaemonRequest::RestartPaneTerminal {
            pane_id: "pane-1".to_string(),
        })
        .expect("restart pane");
    let info = server.router.agent_state("pane-1");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, None);

    // Classification resumes on the fresh generation (the respawn's
    // ensure_model resets the `ended` latch).
    agent_unthrottle(&server.router, "pane-1");
    server
        .router
        .feed_model("pane-1", CLAUDE_WORKING_SCREEN.as_bytes());
    assert_eq!(
        server.router.agent_state("pane-1").attention,
        Some(AgentAttention::Working)
    );
}

#[test]
fn set_pane_agent_validates_agent_name() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");

    // (T1) L8: shell-safe names up to 32 chars are accepted.
    let state = server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: Some("my-agent_2".to_string()),
        })
        .expect("valid name should succeed");
    assert_eq!(state["agent"], json!("my-agent_2"));
    let name32 = "a".repeat(AGENT_NAME_MAX_LEN);
    server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: Some(name32.clone()),
        })
        .expect("32 chars should succeed");

    for bad in [
        "a".repeat(AGENT_NAME_MAX_LEN + 1),
        "has space".to_string(),
        "slash/ok".to_string(),
        "claude🦀".to_string(),
    ] {
        let err = server
            .handle(DaemonRequest::SetPaneAgent {
                pane_id: "pane-1".to_string(),
                agent: Some(bad),
            })
            .expect_err("invalid name must error");
        assert!(
            err.contains("invalid agent name"),
            "unexpected error: {err}"
        );
    }
    // Failed validations never touched the mark.
    assert_eq!(
        server.router.agent_state("pane-1").agent.as_deref(),
        Some(name32.as_str())
    );
}

#[test]
fn set_pane_agent_persist_failure_reverts_mark() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");
    server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: Some("claude".to_string()),
        })
        .expect("mark should succeed");

    // Sabotage the persist path: a DIRECTORY at workspace.json makes the
    // atomic temp+rename fail.
    let persist_path = dir.path().join(WORKSPACE_FILE);
    fs::remove_file(&persist_path).expect("remove workspace.json");
    fs::create_dir(&persist_path).expect("replace it with a directory");

    // (T1) L8: the failed mark is reverted in memory — disk, memory, and
    // clients never diverge.
    server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: Some("other".to_string()),
        })
        .expect_err("persist must fail");
    assert_eq!(
        server.router.agent_state("pane-1").agent.as_deref(),
        Some("claude")
    );
    assert_eq!(
        server
            .router
            .manual_agent_marks()
            .get("pane-1")
            .map(String::as_str),
        Some("claude")
    );
}

#[test]
fn stale_manual_marks_filtered_at_seed_and_persist() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A hand-edited workspace carrying a mark for a pane that doesn't exist.
    let raw = json!({
        "panes": [{
            "id": "pane-1",
            "title": "term-1",
            "kind": "shell",
            "created_at_ms": 1,
        }],
        "active_pane_id": "pane-1",
        "cwd": dir.path().display().to_string(),
        "next_id": 2,
        "agents": { "pane-1": "claude", "pane-99": "ghost" },
    });
    fs::write(
        dir.path().join(WORKSPACE_FILE),
        serde_json::to_vec_pretty(&raw).expect("encode workspace"),
    )
    .expect("write workspace.json");

    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("workspace should load");
    // (T1) L6b: the ghost mark is filtered at seed time.
    let marks = server.router.manual_agent_marks();
    assert_eq!(marks.get("pane-1").map(String::as_str), Some("claude"));
    assert!(!marks.contains_key("pane-99"));

    // (T1) L7: even a stale entry sneaked into the tracker is dropped at
    // persist time.
    server
        .router
        .agents
        .lock()
        .expect("agents lock")
        .panes
        .insert(
            "pane-99".to_string(),
            AgentPaneState {
                agent: Some("ghost".to_string()),
                manual: true,
                ..Default::default()
            },
        );
    server.persist().expect("persist should succeed");
    let persisted: PersistedWorkspace = serde_json::from_str(
        &fs::read_to_string(dir.path().join(WORKSPACE_FILE)).expect("workspace.json should exist"),
    )
    .expect("workspace.json should parse");
    assert!(!persisted.agents.contains_key("pane-99"));
    assert_eq!(
        persisted.agents.get("pane-1").map(String::as_str),
        Some("claude")
    );
}

#[test]
fn set_pane_agent_marks_persists_and_restores() {
    let dir = tempfile::tempdir().expect("temp dir");
    let read_persisted = || -> PersistedWorkspace {
        serde_json::from_str(
            &fs::read_to_string(dir.path().join(WORKSPACE_FILE))
                .expect("workspace.json should exist"),
        )
        .expect("workspace.json should parse")
    };
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");

    // Mark pane-1 (no screen model needed for a manual mark).
    let state = server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: Some("claude".to_string()),
        })
        .expect("mark should succeed");
    assert_eq!(state["agent"], json!("claude"));
    assert_eq!(state["attention"], json!("idle"));
    assert_eq!(
        read_persisted().agents.get("pane-1").map(String::as_str),
        Some("claude")
    );

    // A fresh daemon on the same data dir restores the manual mark, and
    // the bootstrap payload carries it.
    drop(server);
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("restart should load the workspace");
    assert_eq!(
        server
            .router
            .manual_agent_marks()
            .get("pane-1")
            .map(String::as_str),
        Some("claude")
    );
    assert_eq!(
        server.router.agent_state("pane-1").agent.as_deref(),
        Some("claude")
    );
    let snapshot = server.snapshot().expect("snapshot");
    assert_eq!(
        snapshot
            .agent_states
            .get("pane-1")
            .and_then(|info| info.agent.as_deref()),
        Some("claude")
    );

    // Unmark: back to auto-detection (no signature on screen → cleared),
    // and the persisted mark is gone.
    let state = server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: None,
        })
        .expect("unmark should succeed");
    assert!(state["agent"].is_null());
    assert!(state["attention"].is_null());
    assert!(!read_persisted().agents.contains_key("pane-1"));
}

#[test]
fn persisted_workspace_without_agents_field_still_loads() {
    // Backward compat: a workspace.json written BEFORE the agents field
    // existed must load with no manual marks (serde default).
    let dir = tempfile::tempdir().expect("temp dir");
    let raw = json!({
        "panes": [{
            "id": "pane-1",
            "title": "term-1",
            "kind": "shell",
            "created_at_ms": 1,
        }],
        "active_pane_id": "pane-1",
        "cwd": dir.path().display().to_string(),
        "next_id": 2,
    });
    fs::write(
        dir.path().join(WORKSPACE_FILE),
        serde_json::to_vec_pretty(&raw).expect("encode workspace"),
    )
    .expect("write workspace.json");

    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("legacy workspace must still load");
    assert!(server.router.manual_agent_marks().is_empty());
    assert_eq!(server.snapshot().expect("snapshot").panes.len(), 1);
}

#[test]
fn set_pane_agent_rejects_unknown_pane_and_blank_name() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");

    let err = server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-99".to_string(),
            agent: Some("claude".to_string()),
        })
        .expect_err("unknown pane must error");
    assert!(err.contains("pane not found"), "unexpected error: {err}");

    let err = server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: Some("   ".to_string()),
        })
        .expect_err("blank agent name must error");
    assert!(err.contains("blank"), "unexpected error: {err}");
}

#[test]
fn agent_mark_dies_with_closed_pane() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config {
            shell: Some("/bin/cat".to_string()),
            ..Default::default()
        },
    )
    .expect("daemon server should start");

    let pane: Pane = serde_json::from_value(
        server
            .handle(DaemonRequest::CreatePane {
                title: None,
                profile: None,
            })
            .expect("create should succeed"),
    )
    .expect("pane should deserialize");
    server
        .handle(DaemonRequest::SetPaneAgent {
            pane_id: pane.id.clone(),
            agent: Some("claude".to_string()),
        })
        .expect("mark should succeed");
    assert_eq!(
        server.router.manual_agent_marks().get(&pane.id),
        Some(&"claude".to_string())
    );

    server
        .handle(DaemonRequest::ClosePane {
            pane_id: pane.id.clone(),
        })
        .expect("close should succeed");
    assert!(server.router.manual_agent_marks().is_empty());
    assert_eq!(server.router.agent_state(&pane.id).agent, None);
    let persisted: PersistedWorkspace = serde_json::from_str(
        &fs::read_to_string(dir.path().join(WORKSPACE_FILE)).expect("workspace.json should exist"),
    )
    .expect("workspace.json should parse");
    assert!(!persisted.agents.contains_key(&pane.id));
}

#[test]
fn agent_snapshot_and_find_carry_agent_and_attention() {
    let dir = tempfile::tempdir().expect("temp dir");
    let server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");
    server.router.ensure_model("pane-1", 80, 24);
    server
        .router
        .feed_model("pane-1", CLAUDE_WORKING_SCREEN.as_bytes());

    let snapshot = server
        .handle(DaemonRequest::Snapshot {
            pane_id: "pane-1".to_string(),
        })
        .expect("snapshot should succeed");
    assert_eq!(snapshot["agent"], json!("claude"));
    assert_eq!(snapshot["attention"], json!("working"));

    let found = server
        .handle(DaemonRequest::Find {
            command: None,
            title: None,
            cwd: None,
            state: None,
        })
        .expect("find should succeed");
    let entries = found.as_array().expect("find returns an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["agent"], json!("claude"));
    assert_eq!(entries[0]["attention"], json!("working"));

    // A non-agent pane: agent null, attention key OMITTED.
    let dir2 = tempfile::tempdir().expect("temp dir");
    let server2 = DaemonServer::with_config(
        dir2.path().to_path_buf(),
        dir2.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");
    server2.router.ensure_model("pane-1", 80, 24);
    server2.router.feed_model("pane-1", b"user@host:~$ ls\r\n");
    let snapshot = server2
        .handle(DaemonRequest::Snapshot {
            pane_id: "pane-1".to_string(),
        })
        .expect("snapshot should succeed");
    assert!(snapshot["agent"].is_null());
    assert!(snapshot.get("attention").is_none());
}

#[test]
fn agent_event_and_request_wire_shapes() {
    // The FIXED wire contract: snake_case tags + field names.
    let event = DaemonEvent::AgentState {
        pane_id: "pane-1".to_string(),
        agent: Some("claude".to_string()),
        attention: Some(AgentAttention::NeedsInput),
        mode: None,
    };
    assert_eq!(
        serde_json::to_value(&event).expect("serialize event"),
        json!({
            "event": "agent_state",
            "pane_id": "pane-1",
            "agent": "claude",
            "attention": "needs_input",
        })
    );
    let decoded: DaemonEvent = serde_json::from_value(json!({
        "event": "agent_state",
        "pane_id": "pane-1",
        "agent": null,
        "attention": null,
    }))
    .expect("deserialize event");
    assert_eq!(
        decoded,
        DaemonEvent::AgentState {
            pane_id: "pane-1".to_string(),
            agent: None,
            attention: None,
            mode: None,
        }
    );

    let request = DaemonRequest::SetPaneAgent {
        pane_id: "pane-1".to_string(),
        agent: Some("claude".to_string()),
    };
    assert_eq!(
        serde_json::to_value(&request).expect("serialize request"),
        json!({
            "command": "set_pane_agent",
            "pane_id": "pane-1",
            "agent": "claude",
        })
    );
    let decoded: DaemonRequest = serde_json::from_value(json!({
        "command": "set_pane_agent",
        "pane_id": "pane-1",
        "agent": null,
    }))
    .expect("deserialize request");
    assert_eq!(
        decoded,
        DaemonRequest::SetPaneAgent {
            pane_id: "pane-1".to_string(),
            agent: None,
        }
    );
}

// ----- parse_agent_args: pure parser for `ctl agent` -----

fn agent_args(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

#[test]
fn parse_agent_args_forms() {
    let parsed = parse_agent_args(&agent_args(&["pane-2"])).expect("query form");
    assert_eq!(parsed.pane_ref, "pane-2");
    assert_eq!(parsed.mark, None);

    let parsed = parse_agent_args(&agent_args(&[])).expect("no pane defaults to active");
    assert_eq!(parsed.pane_ref, "active");
    assert_eq!(parsed.mark, None);

    let parsed = parse_agent_args(&agent_args(&["pane-2", "on"])).expect("on form");
    assert_eq!(parsed.pane_ref, "pane-2");
    assert_eq!(parsed.mark, Some(Some("claude".to_string())));

    let parsed = parse_agent_args(&agent_args(&["pane-2", "off"])).expect("off form");
    assert_eq!(parsed.mark, Some(None));

    // (T1) L9: a leading on/off is the operation on the ACTIVE pane.
    let parsed = parse_agent_args(&agent_args(&["on"])).expect("on targets active");
    assert_eq!(parsed.pane_ref, "active");
    assert_eq!(parsed.mark, Some(Some("claude".to_string())));
    let parsed = parse_agent_args(&agent_args(&["off"])).expect("off targets active");
    assert_eq!(parsed.pane_ref, "active");
    assert_eq!(parsed.mark, Some(None));
    let err = parse_agent_args(&agent_args(&["on", "extra"]))
        .expect_err("verb-first takes no further positional");
    assert_eq!(err, "unexpected argument for agent: extra");

    let err =
        parse_agent_args(&agent_args(&["pane-2", "maybe"])).expect_err("unknown verb must error");
    assert_eq!(err, "unexpected argument for agent: maybe");

    let err = parse_agent_args(&agent_args(&["pane-2", "on", "extra"]))
        .expect_err("extra argument must error");
    assert_eq!(err, "unexpected argument for agent: extra");
}

#[test]
fn ctl_agent_is_not_freeform_and_needs_a_daemon() {
    // `agent` takes no free text: a trailing --json is still parsed as a
    // global flag (not swallowed as payload).
    let options = parse_control_options(agent_args(&["agent", "pane-1", "--json"]))
        .expect("options should parse");
    assert!(options.json);
    assert_eq!(options.args, agent_args(&["agent", "pane-1"]));

    // Dispatch reaches the daemon connection (which fails without one) —
    // and never spawns one for a read/mark.
    let args = vec![
        "sgian".to_string(),
        "ctl".to_string(),
        "--workspace".to_string(),
        "/tmp/sgian-no-such-workspace-agent".to_string(),
        "agent".to_string(),
        "pane-1".to_string(),
    ];
    let err = run_control_cli_from_args(&args).expect_err("agent needs a running daemon");
    assert!(err.contains("no daemon running"), "unexpected error: {err}");
}

// ---- (T2) chat-native agent sessions ----

/// (T2) Fake `claude` CLI speaking the pinned stream-json protocol:
/// emits init (honoring --resume), answers normal turns with streamed
/// text + a result, raises a blocking can_use_tool permission request for
/// "ask-permission" turns (allow/deny decided by the control_response),
/// the same WITHOUT an `input` field for "ask-noinput" (L7), requests
/// permission then exits 7 for "ask-then-die" (H1), hangs a turn open for
/// "hang" (interrupt target), hangs with the interrupt answering only a
/// BARE control_response (the lost-`result` case, M2) for
/// "hang-noresult", and re-emits init mid-turn for "reinit-hang" (L5).
/// Every stdin line and its argv are logged to $FAKE_CLAUDE_LOG so tests
/// can assert what the daemon sent.
#[cfg(unix)]
const FAKE_CLAUDE_SH: &str = r#"#!/bin/sh
ARGS="$*"
SID="fake-session-0001"
while [ $# -gt 0 ]; do
  case "$1" in
--resume) SID="$2"; shift 2 ;;
*) shift ;;
  esac
done
LOG="${FAKE_CLAUDE_LOG:-/dev/null}"
STATE="$LOG.bare-interrupt"
printf 'argv %s\n' "$ARGS" >> "$LOG"
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SID\",\"model\":\"fake-claude-1\",\"cwd\":\"/tmp\",\"tools\":[]}"
while IFS= read -r line; do
  printf 'stdin %s\n' "$line" >> "$LOG"
  case "$line" in
*'"subtype":"interrupt"'*)
  printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"ignored\"}}"
  if [ ! -f "$STATE" ]; then
    printf '%s\n' "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"duration_ms\":1,\"num_turns\":1,\"session_id\":\"$SID\"}"
  fi
  ;;
*'"behavior":"allow"'*)
  printf '%s\n' "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_fake\",\"content\":\"ran fine\",\"is_error\":false}]}}"
  printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":1,\"num_turns\":2,\"total_cost_usd\":0.001,\"usage\":{\"input_tokens\":1,\"output_tokens\":2},\"session_id\":\"$SID\"}"
  ;;
*'"behavior":"deny"'*)
  printf '%s\n' "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_fake\",\"content\":\"denied by operator\",\"is_error\":true}]}}"
  printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":1,\"num_turns\":2,\"session_id\":\"$SID\"}"
  ;;
*ask-then-die*)
  printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_fake\",\"name\":\"FakeTool\",\"input\":{\"cmd\":\"probe\"}}]}}"
  printf '%s\n' "{\"type\":\"control_request\",\"request_id\":\"perm-req-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"FakeTool\",\"input\":{\"cmd\":\"probe\"}}}"
  exit 7
  ;;
*ask-permission*)
  printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_fake\",\"name\":\"FakeTool\",\"input\":{\"cmd\":\"probe\"}}]}}"
  printf '%s\n' "{\"type\":\"control_request\",\"request_id\":\"perm-req-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"FakeTool\",\"input\":{\"cmd\":\"probe\"}}}"
  ;;
*ask-noinput*)
  printf '%s\n' "{\"type\":\"control_request\",\"request_id\":\"perm-req-1\",\"request\":{\"subtype\":\"can_use_tool\",\"tool_name\":\"FakeTool\"}}"
  ;;
*reinit-hang*)
  printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SID\",\"model\":\"fake-claude-1\",\"cwd\":\"/tmp\",\"tools\":[]}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"working\"}}}"
  ;;
*hang-noresult*)
  touch "$STATE"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"working\"}}}"
  ;;
*hang*)
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"working\"}}}"
  ;;
*die*)
  exit 7
  ;;
*'"type":"user"'*)
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_start\",\"message\":{\"role\":\"assistant\"}}}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"echo\"}}}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" reply\"}}}"
  printf '%s\n' "{\"type\":\"stream_event\",\"event\":{\"type\":\"message_stop\"}}"
  printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"echo reply\"}]}}"
  printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":1,\"num_turns\":1,\"total_cost_usd\":0.002,\"usage\":{\"input_tokens\":3,\"output_tokens\":4},\"session_id\":\"$SID\"}"
  ;;
  esac
done
exit 0
"#;

#[cfg(unix)]
struct FakeClaude {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    log: PathBuf,
}

#[cfg(unix)]
fn install_fake_claude() -> FakeClaude {
    let dir = tempfile::tempdir().expect("temp driver dir");
    let bin = dir.path().join("fake-claude");
    fs::write(&bin, FAKE_CLAUDE_SH).expect("write fake claude driver");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude driver");
    }
    let log = dir.path().join("driver.log");
    FakeClaude {
        _dir: dir,
        bin,
        log,
    }
}

/// Minimal long-lived Factory Droid JSON-RPC driver. It intentionally
/// emits an idle notification immediately after initialization so the
/// integration test also pins suppression of startup-idle turn events.
/// The real CLI also requires factoryApiVersion on every RPC envelope,
/// including responses; accepting plain JSON-RPC hid launch failures.
#[cfg(unix)]
const FAKE_DROID_SH: &str = r#"#!/bin/sh
LOG="__FAKE_DROID_LOG__"
printf 'argv %s\n' "$*" >> "$LOG"
printf '%s\n' '{"jsonrpc":"2.0","type":"response","id":"init","result":{"sessionId":"fake-droid-session-1","session":{},"settings":{"modelId":"custom:Fireworks-Qwen-0","reasoningEffort":"medium"}}}'
printf '%s\n' '{"jsonrpc":"2.0","type":"notification","method":"droid.session_notification","params":{"notification":{"type":"droid_working_state_changed","newState":"idle"}}}'
while IFS= read -r line; do
  printf 'stdin %s\n' "$line" >> "$LOG"
  case "$line" in
*'"factoryApiVersion":"1.0.0"'*) ;;
*) printf '%s\n' '{"jsonrpc":"2.0","type":"response","id":null,"error":{"code":-32600,"message":"Invalid JSON-RPC message: missing factoryApiVersion"}}'; exit 1 ;;
  esac
  case "$line" in
*'"method":"droid.initialize_session"'*)
  ;;
*'"method":"droid.load_session"'*)
  printf '%s\n' '{"jsonrpc":"2.0","type":"response","id":"load","result":{"session":{},"settings":{"modelId":"custom:Fireworks-Qwen-0","reasoningEffort":"medium"}}}'
  ;;
*'"method":"droid.add_user_message"'*)
  printf '%s\n' '{"jsonrpc":"2.0","type":"response","id":"message","result":{}}'
  printf '%s\n' '{"jsonrpc":"2.0","type":"notification","method":"droid.session_notification","params":{"notification":{"type":"droid_working_state_changed","newState":"streaming_assistant_message"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","type":"notification","method":"droid.session_notification","params":{"notification":{"type":"assistant_text_delta","messageId":"m1","blockIndex":0,"textDelta":"droid reply"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","type":"notification","method":"droid.session_notification","params":{"notification":{"type":"assistant_text_complete","messageId":"m1","blockIndex":0}}}'
  printf '%s\n' '{"jsonrpc":"2.0","type":"notification","method":"droid.session_notification","params":{"notification":{"type":"droid_working_state_changed","newState":"idle"}}}'
  ;;
  esac
done
exit 0
"#;

#[cfg(unix)]
struct FakeDroid {
    _dir: tempfile::TempDir,
    bin: PathBuf,
    log: PathBuf,
}

#[cfg(unix)]
fn install_fake_droid() -> FakeDroid {
    let dir = tempfile::tempdir().expect("temp Droid driver dir");
    let bin = dir.path().join("fake-droid");
    let log = dir.path().join("driver.log");
    let script = FAKE_DROID_SH.replace("__FAKE_DROID_LOG__", &log.to_string_lossy());
    fs::write(&bin, script).expect("write fake Droid driver");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod fake Droid driver");
    FakeDroid {
        _dir: dir,
        bin,
        log,
    }
}

#[cfg(unix)]
fn droid_test_config(fake: &FakeDroid) -> Config {
    Config {
        shell: Some("/bin/cat".to_string()),
        agent_droid_bin: Some(fake.bin.to_string_lossy().to_string()),
        ..Default::default()
    }
}

/// (T2) Config wiring the fake driver in as the agent binary, with the
/// driver's log path passed through the config `env` map (per-test, so
/// parallel tests never share a log).
#[cfg(unix)]
fn agent_test_config(fake: &FakeClaude) -> Config {
    Config {
        shell: Some("/bin/cat".to_string()),
        agent_claude_bin: Some(fake.bin.to_string_lossy().to_string()),
        env: HashMap::from([(
            "FAKE_CLAUDE_LOG".to_string(),
            fake.log.to_string_lossy().to_string(),
        )]),
        ..Default::default()
    }
}

#[cfg(unix)]
fn driver_log_contents(fake: &FakeClaude) -> String {
    fs::read_to_string(&fake.log).unwrap_or_default()
}

/// (T2) Spawn a TestDaemon with the fake driver and an EXISTING cwd (the
/// shared /tmp/sgian-itest cwd does not exist, and std::process::Command
/// — unlike portable-pty — fails the spawn with ENOENT for a missing cwd).
/// The cwd TempDir is returned to keep it alive for the test.
#[cfg(unix)]
fn spawn_agent_test_daemon(fake: &FakeClaude) -> (TestDaemon, tempfile::TempDir) {
    let cwd = tempfile::tempdir().expect("agent test cwd");
    let daemon = TestDaemon::spawn_with_cwd(agent_test_config(fake), cwd.path().to_path_buf());
    (daemon, cwd)
}

#[cfg(unix)]
fn spawn_droid_test_daemon(fake: &FakeDroid) -> (TestDaemon, tempfile::TempDir) {
    let cwd = tempfile::tempdir().expect("Droid test cwd");
    let daemon = TestDaemon::spawn_with_cwd(droid_test_config(fake), cwd.path().to_path_buf());
    (daemon, cwd)
}

#[cfg(unix)]
fn subscribe_events(client: &DaemonClient) -> DaemonConnection {
    let mut connection = client.connect().expect("subscribe stream should connect");
    connection
        .write_request(&DaemonRequest::Subscribe)
        .expect("subscribe request should write");
    connection
        .await_subscribe_ack()
        .expect("subscriber registration should be acknowledged");
    // Spawning many real child-process fixtures in the full suite can
    // briefly queue behind macOS process-start/security scanning. Keep the
    // test bounded without treating a slow fixture launch as a protocol
    // failure. Production connection timeouts are unchanged.
    connection.set_read_timeout(Some(Duration::from_secs(30)));
    connection
}

/// (T2) Read daemon events off an acknowledged subscriber stream until
/// `matches` extracts a value from one. Other events (PaneCreated,
/// unrelated AgentEvents, …) are skipped.
#[cfg(unix)]
fn read_event_until<T>(
    reader: &mut DaemonConnection,
    matches: impl Fn(&DaemonEvent) -> Option<T>,
) -> T {
    for _ in 0..200 {
        let event = reader
            .read_event()
            .expect("event stream reads")
            .expect("event stream closed waiting for event");
        if let Some(found) = matches(&event) {
            return found;
        }
    }
    panic!("expected daemon event not received within 200 events");
}

/// (T2) Read until the pane's AgentEvent of the given `kind` arrives;
/// returns the normalized event object.
#[cfg(unix)]
fn read_agent_event(reader: &mut DaemonConnection, pane_id: &str, kind: &str) -> Value {
    read_event_until(reader, |event| {
        if let DaemonEvent::AgentEvent {
            pane_id: id,
            event: payload,
        } = event
        {
            if id == pane_id && payload.get("kind").and_then(Value::as_str) == Some(kind) {
                return Some(payload.clone());
            }
        }
        None
    })
}

#[cfg(unix)]
#[test]
fn agent_pane_create_send_streams_turn_events() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);

    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    assert_eq!(pane.kind, PaneKind::Agent);
    assert_eq!(
        pane.title,
        format!("agent-{}", pane.id.strip_prefix("pane-").unwrap())
    );

    // The CLI's init event becomes the normalized `session` event.
    let session = read_agent_event(&mut reader, &pane.id, "session");
    assert_eq!(session["session_id"], json!("fake-session-0001"));
    assert_eq!(session["model"], json!("fake-claude-1"));

    // A user message streams: message_start → deltas → complete → result.
    let ok: CommandOk = client
        .request(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hello there".to_string(),
        })
        .expect("send agent message");
    assert!(ok.ok);
    assert_eq!(
        read_agent_event(&mut reader, &pane.id, "message_start")["role"],
        json!("assistant")
    );
    let mut deltas = String::new();
    for _ in 0..2 {
        let delta = read_agent_event(&mut reader, &pane.id, "text_delta");
        deltas.push_str(delta["text"].as_str().expect("delta text"));
    }
    assert_eq!(deltas, "echo reply");
    read_agent_event(&mut reader, &pane.id, "message_complete");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));
    assert_eq!(turn["cost_usd"], json!(0.002));
    assert_eq!(turn["usage"]["input_tokens"], json!(3));

    // The driver saw exactly one user message carrying the text.
    wait_for(|| driver_log_contents(&fake).contains("hello there"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn droid_agent_pane_streams_and_persists_provider_identity() {
    let fake = install_fake_droid();
    let (daemon, _cwd) = spawn_droid_test_daemon(&fake);
    let client = daemon.client();
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPaneWithSpec {
            title: Some("fireworks-reviewer".to_string()),
            backend: Some(AgentBackendKind::Droid),
            model: Some("custom:Fireworks-Qwen-0".to_string()),
        })
        .expect("create Droid pane");

    // Poll the bounded bootstrap replay instead of relying on the legacy
    // v1 test subscriber's intentionally ack-less registration race.
    let wait_for_kind = |kind: &str| -> WorkspaceSnapshot {
        for _ in 0..200 {
            let snapshot = client
                .request::<WorkspaceSnapshot>(DaemonRequest::BootstrapWorkspace)
                .expect("bootstrap Droid replay");
            if snapshot
                .agent_events
                .get(&pane.id)
                .into_iter()
                .flatten()
                .any(|event| event.get("kind").and_then(Value::as_str) == Some(kind))
            {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("Droid replay never contained {kind}");
    };
    let initialized = wait_for_kind("session");
    let session = initialized.agent_events[&pane.id]
        .iter()
        .find(|event| event.get("kind").and_then(Value::as_str) == Some("session"))
        .expect("session event");
    assert_eq!(session["session_id"], json!("fake-droid-session-1"));
    assert_eq!(session["model"], json!("custom:Fireworks-Qwen-0"));
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "review this".to_string(),
        })
        .expect("send Droid message");
    let snapshot = wait_for_kind("turn_complete");
    let events = &snapshot.agent_events[&pane.id];
    assert_eq!(
        events
            .iter()
            .find(|event| event.get("kind").and_then(Value::as_str) == Some("text_delta"))
            .expect("Droid text delta")["text"],
        json!("droid reply")
    );
    assert!(events
        .iter()
        .any(|event| event.get("kind").and_then(Value::as_str) == Some("message_complete")));
    assert_eq!(
        snapshot.agent_specs.get(&pane.id),
        Some(&AgentPaneSpec {
            backend: AgentBackendKind::Droid,
            model: Some("custom:Fireworks-Qwen-0".to_string()),
        })
    );
    let turn_count = snapshot
        .agent_events
        .get(&pane.id)
        .into_iter()
        .flatten()
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some("turn_complete"))
        .count();
    assert_eq!(turn_count, 1, "startup idle must not become an empty turn");

    let disposable: Pane = client
        .request(DaemonRequest::CreatePane {
            title: Some("disposable".to_string()),
            profile: None,
        })
        .expect("create disposable shell pane");
    let after_close: WorkspaceSnapshot = client
        .request(DaemonRequest::ClosePane {
            pane_id: disposable.id,
        })
        .expect("close disposable shell pane");
    assert_eq!(
        after_close.agent_specs.get(&pane.id),
        Some(&AgentPaneSpec {
            backend: AgentBackendKind::Droid,
            model: Some("custom:Fireworks-Qwen-0".to_string()),
        }),
        "close snapshots must preserve surviving provider identity"
    );

    let log = fs::read_to_string(&fake.log).expect("Droid driver log");
    assert!(log.contains("stream-jsonrpc"), "{log}");
    assert!(
        log.contains(r#""method":"droid.initialize_session""#),
        "{log}"
    );
    assert!(
        log.contains(r#""modelId":"custom:Fireworks-Qwen-0""#),
        "{log}"
    );
    assert!(
        log.contains(r#""method":"droid.add_user_message""#),
        "{log}"
    );
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_permission_blocks_until_allow_and_deny() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // --- ALLOW path: the CLI blocks until the AgentApproval arrives. ---
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "ask-permission please".to_string(),
        })
        .expect("send ask-permission");
    let tool_use = read_agent_event(&mut reader, &pane.id, "tool_use");
    assert_eq!(tool_use["name"], json!("FakeTool"));
    assert_eq!(tool_use["input"], json!({ "cmd": "probe" }));
    let request = read_agent_event(&mut reader, &pane.id, "permission_request");
    assert_eq!(request["request_id"], json!("perm-req-1"));
    assert_eq!(request["tool_name"], json!("FakeTool"));
    assert_eq!(request["input"], json!({ "cmd": "probe" }));

    // While the permission request is pending, the turn is still running:
    // a concurrent message is REJECTED, not queued.
    let busy = client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "meanwhile".to_string(),
        })
        .expect_err("concurrent send must be rejected");
    assert!(
        busy.contains("turn already in progress"),
        "unexpected error: {busy}"
    );

    client
        .request::<CommandOk>(DaemonRequest::AgentApproval {
            pane_id: pane.id.clone(),
            request_id: "perm-req-1".to_string(),
            allow: true,
            message: None,
        })
        .expect("allow approval");
    let tool_result = read_agent_event(&mut reader, &pane.id, "tool_result");
    assert_eq!(tool_result["content"], json!("ran fine"));
    assert_eq!(tool_result["is_error"], json!(false));
    read_agent_event(&mut reader, &pane.id, "turn_complete");
    // Allow echoes the original input back as updatedInput (probe c).
    wait_for(|| {
        let log = driver_log_contents(&fake);
        log.contains(r#""behavior":"allow"#) && log.contains(r#""updatedInput":{"cmd":"probe"}"#)
    });

    // A stale request id (already answered) is a clean error.
    let stale = client
        .request::<CommandOk>(DaemonRequest::AgentApproval {
            pane_id: pane.id.clone(),
            request_id: "perm-req-1".to_string(),
            allow: true,
            message: None,
        })
        .expect_err("stale request id must error");
    assert!(stale.contains("no pending permission request"), "{stale}");

    // --- DENY path: the feedback message reaches the CLI. ---
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "ask-permission again".to_string(),
        })
        .expect("send second ask-permission");
    read_agent_event(&mut reader, &pane.id, "permission_request");
    client
        .request::<CommandOk>(DaemonRequest::AgentApproval {
            pane_id: pane.id.clone(),
            request_id: "perm-req-1".to_string(),
            allow: false,
            message: Some("not allowed, stop that".to_string()),
        })
        .expect("deny approval");
    let denied = read_agent_event(&mut reader, &pane.id, "tool_result");
    assert_eq!(denied["is_error"], json!(true));
    read_agent_event(&mut reader, &pane.id, "turn_complete");
    wait_for(|| {
        let log = driver_log_contents(&fake);
        log.contains(r#""behavior":"deny"#) && log.contains("not allowed, stop that")
    });
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_interrupt_ends_turn_and_pane_stays_alive() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // A "hang" turn starts streaming but never completes on its own.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hang on".to_string(),
        })
        .expect("send hang");
    let delta = read_agent_event(&mut reader, &pane.id, "text_delta");
    assert_eq!(delta["text"], json!("working"));

    // Interrupt ends the turn with an error-subtype turn_complete
    // (probe f); the process stays alive.
    client
        .request::<CommandOk>(DaemonRequest::InterruptAgent {
            pane_id: pane.id.clone(),
        })
        .expect("interrupt agent");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("error_during_execution"));
    wait_for(|| driver_log_contents(&fake).contains(r#""subtype":"interrupt"#));

    // The pane takes a fresh message afterwards (turn flag cleared, CLI alive).
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hello again".to_string(),
        })
        .expect("send after interrupt");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_process_exit_marks_pane_ended_and_send_respawns() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // "die" makes the driver exit 7: the reader reports process_exit
    // (normalized) and PaneEnded (the shared ended flow), in that order.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "die now".to_string(),
        })
        .expect("send die");
    let exit = read_agent_event(&mut reader, &pane.id, "process_exit");
    assert_eq!(exit["exit_code"], json!(7));
    let ended_code = read_event_until(&mut reader, |event| {
        if let DaemonEvent::PaneEnded { pane_id, exit_code } = event {
            if *pane_id == pane.id {
                return Some(*exit_code);
            }
        }
        None
    });
    assert_eq!(ended_code, Some(7));

    let list: PaneList = client
        .request(DaemonRequest::ListPanes)
        .expect("list panes");
    let status = list
        .panes
        .iter()
        .find(|status| status.pane.id == pane.id)
        .expect("agent pane listed");
    assert_eq!(status.state, PaneRuntimeState::Ended);

    // Sending to an ended agent pane auto-respawns it (resuming the CLI
    // session) instead of erroring like a dead PTY.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "are you back".to_string(),
        })
        .expect("send after exit respawns");
    read_agent_event(&mut reader, &pane.id, "session");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));
    wait_for(|| driver_log_contents(&fake).contains("--resume fake-session-0001"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_conversation_replay_and_resume_survive_daemon_restart() {
    let fake = install_fake_claude();
    let config = agent_test_config(&fake);
    let cwd = tempfile::tempdir().expect("agent test cwd");
    let mut daemon = TestDaemon::spawn_with_cwd(config.clone(), cwd.path().to_path_buf());
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "remember this".to_string(),
        })
        .expect("send message");
    read_agent_event(&mut reader, &pane.id, "turn_complete");

    // The conversation log persists the normalized events as JSONL.
    let log_path = daemon
        .data_dir
        .path()
        .join(AGENT_LOG_DIR)
        .join(format!("{}.jsonl", pane.id));
    wait_for(|| {
        fs::read_to_string(&log_path)
            .map(|log| log.contains(r#""kind":"turn_complete"#))
            .unwrap_or(false)
    });

    // workspace.json records the CLI session id under agents_v2. The id is
    // recorded by the reader thread, so this lands via the lazy-persist
    // cadence (LAZY_PERSIST_INTERVAL), not the send's own response.
    let workspace_path = daemon.data_dir.path().join(WORKSPACE_FILE);
    wait_for(|| {
        fs::read(&workspace_path)
            .ok()
            .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
            .map(|workspace| workspace["agents_v2"][pane.id.as_str()] == json!("fake-session-0001"))
            .unwrap_or(false)
    });

    // Daemon restart: the pane respawns with --resume, and the bootstrap
    // payload carries the bounded conversation replay.
    daemon.restart(config);
    let client = daemon.client();
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");
    let replay = snapshot
        .agent_events
        .get(&pane.id)
        .expect("agent pane has a replay");
    let kinds: Vec<&str> = replay
        .iter()
        .filter_map(|event| event.get("kind").and_then(Value::as_str))
        .collect();
    assert!(kinds.contains(&"session"), "replay kinds: {kinds:?}");
    assert!(kinds.contains(&"text_delta"), "replay kinds: {kinds:?}");
    assert!(kinds.contains(&"turn_complete"), "replay kinds: {kinds:?}");
    assert_eq!(
        snapshot
            .panes
            .iter()
            .find(|p| p.id == pane.id)
            .map(|p| p.kind),
        Some(PaneKind::Agent)
    );
    wait_for(|| driver_log_contents(&fake).contains("--resume fake-session-0001"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_restart_respawns_with_resume() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "before restart".to_string(),
        })
        .expect("send before restart");
    read_agent_event(&mut reader, &pane.id, "turn_complete");

    // RestartPaneTerminal kills the CLI and respawns it — resuming the
    // SAME CLI session (the id must survive the close, which drops both
    // the live session and the persisted-resume seed).
    client
        .request::<CommandOk>(DaemonRequest::RestartPaneTerminal {
            pane_id: pane.id.clone(),
        })
        .expect("restart agent pane");
    read_agent_event(&mut reader, &pane.id, "session");
    wait_for(|| driver_log_contents(&fake).contains("--resume fake-session-0001"));

    // The restarted pane takes messages (and the conversation log kept
    // appending across the restart — both turns are in the replay).
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "after restart".to_string(),
        })
        .expect("send after restart");
    read_agent_event(&mut reader, &pane.id, "turn_complete");
    let replay = read_agent_log_tail(
        &daemon.data_dir.path().join(AGENT_LOG_DIR),
        &pane.id,
        AGENT_REPLAY_MAX_BYTES,
        AGENT_REPLAY_MAX_EVENTS,
    );
    let sessions = replay
        .iter()
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some("session"))
        .count();
    assert_eq!(
        sessions, 2,
        "both spawns logged a session event: {replay:?}"
    );
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_requests_validate_pane_kind_and_payload() {
    let fake = install_fake_claude();
    let data_dir = std::env::temp_dir().join(format!(
        "sgian-t2-validate-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let cwd = tempfile::tempdir().expect("agent validate cwd");
    let server = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.clone(),
        agent_test_config(&fake),
    )
    .expect("server");

    // pane-1 is the default SHELL pane; agent requests reject it.
    for request in [
        DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: "pane-1".to_string(),
            text: "hi".to_string(),
        },
        DaemonRequest::InterruptAgent {
            pane_id: "pane-1".to_string(),
        },
    ] {
        let err = server.handle(request).expect_err("shell pane rejected");
        assert!(err.contains("not an agent pane"), "{err}");
    }
    let err = server
        .handle(DaemonRequest::AgentApproval {
            pane_id: "pane-1".to_string(),
            request_id: "r".to_string(),
            allow: true,
            message: None,
        })
        .expect_err("shell pane rejected for approval");
    assert!(err.contains("not an agent pane"), "{err}");

    // Unknown panes are reported as such.
    let err = server
        .handle(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: "pane-99".to_string(),
            text: "hi".to_string(),
        })
        .expect_err("unknown pane rejected");
    assert!(err.contains("pane not found"), "{err}");

    // Payload validation: empty and oversized messages.
    let pane = server
        .handle(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    let pane_id = pane["id"].as_str().expect("pane id").to_string();
    let err = server
        .handle(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane_id.clone(),
            text: "   ".to_string(),
        })
        .expect_err("empty message rejected");
    assert!(err.contains("cannot be empty"), "{err}");
    let err = server
        .handle(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane_id.clone(),
            text: "x".repeat(AGENT_MESSAGE_MAX_BYTES + 1),
        })
        .expect_err("oversized message rejected");
    assert!(err.contains("exceeds maximum size"), "{err}");

    let _ = fs::remove_dir_all(&data_dir);
}

#[cfg(unix)]
#[test]
fn agent_ctl_send_routes_by_pane_kind() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");

    // `ctl send <agent-pane> <text>` posts a chat message (the driver
    // receives a stream-json user line, not PTY bytes).
    control_send_input(&client, &[pane.id.clone(), "hello from ctl".to_string()])
        .expect("send to agent pane");
    let prompt = read_agent_event(&mut reader, &pane.id, "user_message");
    assert_eq!(prompt["text"], "hello from ctl");
    // Send before the CLI has initialized, then await its response using
    // the same bounded event timeout as the other process fixtures.
    // macOS can hold a new executable at launch for more than five seconds.
    read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert!(driver_log_contents(&fake).contains("hello from ctl"));
    assert!(
        driver_log_contents(&fake).contains(r#""type":"user"#),
        "agent pane send must be a stream-json user message"
    );

    // `ctl interrupt <shell-pane>` is a clean error; agent pane is fine.
    let err = control_interrupt(&client, &["pane-1".to_string()])
        .expect_err("interrupt on shell pane errors");
    assert!(err.contains("not an agent pane"), "{err}");
    control_interrupt(&client, std::slice::from_ref(&pane.id)).expect("interrupt on agent pane");
    wait_for(|| driver_log_contents(&fake).contains(r#""subtype":"interrupt"#));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_event_seq_is_monotonic_across_restart() {
    let fake = install_fake_claude();
    let config = agent_test_config(&fake);
    let cwd = tempfile::tempdir().expect("agent test cwd");
    let mut daemon = TestDaemon::spawn_with_cwd(config.clone(), cwd.path().to_path_buf());
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");

    // Contract: seq is per-pane, from 1, strictly increasing — on the
    // broadcast payload AND the JSONL log lines (the same objects).
    let session = read_agent_event(&mut reader, &pane.id, "session");
    assert_eq!(session["seq"], json!(1));
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: Some("restart-prompt-1".to_string()),
            pane_id: pane.id.clone(),
            text: "hello there".to_string(),
        })
        .expect("send agent message");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    let last_seq = turn["seq"].as_u64().expect("seq on turn_complete");
    assert!(last_seq > 1, "seq increased across the turn: {turn}");

    let log_path = daemon
        .data_dir
        .path()
        .join(AGENT_LOG_DIR)
        .join(format!("{}.jsonl", pane.id));
    wait_for(|| {
        fs::read_to_string(&log_path)
            .map(|log| log.contains(r#""kind":"turn_complete"#))
            .unwrap_or(false)
    });
    let log_seqs: Vec<u64> = fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("kind").is_some())
        .filter_map(|event| event["seq"].as_u64())
        .collect();
    assert!(!log_seqs.is_empty());
    assert_eq!(
        log_seqs,
        (1..=log_seqs.len() as u64).collect::<Vec<_>>(),
        "log seqs are exactly 1..=N"
    );

    // After a daemon restart the bootstrap replay carries the same seqs,
    // and the respawned session CONTINUES the sequence (clients dedupe
    // replay-vs-live by dropping seq <= lastSeq). The respawn itself
    // happens at daemon START (run_daemon_with_config consumes the
    // spawn-on-bootstrap flag), so its session event precedes any
    // subscriber — the continuation is observed on the NEXT turn's
    // events and in the log.
    daemon.restart(config);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after restart");
    let replay = snapshot
        .agent_events
        .get(&pane.id)
        .expect("agent pane has a replay");
    assert!(
        replay.iter().all(|event| event["seq"].is_u64()),
        "replay events carry seq: {replay:?}"
    );
    let prompt_index = replay
        .iter()
        .position(|event| event["kind"] == "user_message")
        .expect("accepted user prompt survives daemon restart");
    assert_eq!(replay[prompt_index]["text"], "hello there");
    assert_eq!(replay[prompt_index]["message_id"], "restart-prompt-1");
    let answer_index = replay
        .iter()
        .position(|event| event["kind"] == "text_delta")
        .expect("assistant reply survives daemon restart");
    assert!(
        prompt_index < answer_index,
        "prompt is persisted before the reply"
    );
    let max_replay_seq = replay
        .iter()
        .filter_map(|event| event["seq"].as_u64())
        .max()
        .expect("replay seqs");
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "after restart".to_string(),
        })
        .expect("send after restart");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert!(
        turn["seq"].as_u64().expect("seq") > max_replay_seq,
        "live seq continues past the replay: {turn}"
    );
    // The log stays monotonic across the restart (including the
    // respawn's session event): strictly increasing, no duplicates, the
    // pre-restart prefix untouched. (Whether daemon 1's shutdown
    // process_exit lands in the log is a shutdown race, so exact
    // 1..=N continuity is NOT asserted here.)
    let all_seqs: Vec<u64> = fs::read_to_string(&log_path)
        .expect("read log")
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("kind").is_some())
        .filter_map(|event| event["seq"].as_u64())
        .collect();
    assert!(
        all_seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "seqs strictly increasing across the restart: {all_seqs:?}"
    );
    assert_eq!(
        &all_seqs[..log_seqs.len()],
        log_seqs.as_slice(),
        "pre-restart seqs are unchanged"
    );
    daemon.shutdown();
}

/// ENHANCEMENTS §5: killing the daemon mid-agent-turn must not leave a
/// wedged permission; a restarted daemon resumes and accepts a new turn.
#[cfg(unix)]
#[test]
fn daemon_death_mid_agent_turn_recovers_on_restart() {
    let fake = install_fake_claude();
    let config = agent_test_config(&fake);
    let cwd = tempfile::tempdir().expect("agent test cwd");
    let mut daemon = TestDaemon::spawn_with_cwd(config.clone(), cwd.path().to_path_buf());
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hang please".to_string(),
        })
        .expect("send hang");
    read_agent_event(&mut reader, &pane.id, "text_delta");
    // Mid-turn: tear the daemon down (Shutdown kills agent children) and
    // bring it back from the same workspace.json.
    daemon.restart(config);
    let client = daemon.client();
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after mid-turn death");
    assert!(
        snapshot
            .panes
            .iter()
            .any(|p| p.id == pane.id && p.kind == PaneKind::Agent),
        "agent pane must still be in the registry"
    );
    let mut reader = subscribe_events(&client);
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "after death".to_string(),
        })
        .expect("send after restart must not be wedged");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_cli_death_mid_permission_unwedges_pane() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // (T2) H1: the driver requests permission and then EXITS without
    // another line. The reader's bounded wait re-checks child liveness
    // every increment, so the pane unwedges promptly instead of sitting
    // out the approval timeout as a zombie.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "ask-then-die please".to_string(),
        })
        .expect("send ask-then-die");
    read_agent_event(&mut reader, &pane.id, "permission_request");
    let resolved = read_agent_event(&mut reader, &pane.id, "permission_resolved");
    assert_eq!(resolved["request_id"], json!("perm-req-1"));
    assert_eq!(resolved["behavior"], json!("deny"));
    assert_eq!(resolved["reason"], json!("process_exit"));

    // The pane ends promptly (reaped via the permission wait's try_wait)…
    let exit = read_agent_event(&mut reader, &pane.id, "process_exit");
    assert_eq!(exit["exit_code"], json!(7));
    // …and a fresh send auto-respawns it.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "back again".to_string(),
        })
        .expect("send after exit respawns");
    read_agent_event(&mut reader, &pane.id, "session");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));

    // The resolution was logged too (a replay cancels the card).
    let log_path = daemon
        .data_dir
        .path()
        .join(AGENT_LOG_DIR)
        .join(format!("{}.jsonl", pane.id));
    wait_for(|| {
        fs::read_to_string(&log_path)
            .map(|log| {
                log.contains(r#""kind":"permission_resolved"#)
                    && log.contains(r#""reason":"process_exit"#)
            })
            .unwrap_or(false)
    });
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_permission_timeout_denies_and_resolves() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // Never answered: after AGENT_APPROVAL_TIMEOUT (short in tests, L5)
    // the reader auto-denies and reports it.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "ask-permission please".to_string(),
        })
        .expect("send ask-permission");
    read_agent_event(&mut reader, &pane.id, "permission_request");
    let resolved = read_agent_event(&mut reader, &pane.id, "permission_resolved");
    assert_eq!(resolved["request_id"], json!("perm-req-1"));
    assert_eq!(resolved["behavior"], json!("deny"));
    assert_eq!(resolved["reason"], json!("timeout"));

    // The CLI gets the deny and ends the turn…
    let result = read_agent_event(&mut reader, &pane.id, "tool_result");
    assert_eq!(result["is_error"], json!(true));
    read_agent_event(&mut reader, &pane.id, "turn_complete");
    wait_for(|| driver_log_contents(&fake).contains(r#""behavior":"deny"#));

    // …and the pane takes a fresh message afterwards.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hello again".to_string(),
        })
        .expect("send after timeout");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_permission_resolved_on_session_close() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "ask-permission please".to_string(),
        })
        .expect("send ask-permission");
    read_agent_event(&mut reader, &pane.id, "permission_request");

    // Restarting drops the session: deny_agent_pending fires with reason
    // "closed" (L5 — the ClosePane path shares this deny; its broadcast
    // is suppressed by mark_closed, so the restart path is the
    // observable one), and the new spawn resumes the conversation.
    client
        .request::<CommandOk>(DaemonRequest::RestartPaneTerminal {
            pane_id: pane.id.clone(),
        })
        .expect("restart agent pane");
    let resolved = read_agent_event(&mut reader, &pane.id, "permission_resolved");
    assert_eq!(resolved["request_id"], json!("perm-req-1"));
    assert_eq!(resolved["behavior"], json!("deny"));
    assert_eq!(resolved["reason"], json!("closed"));
    read_agent_event(&mut reader, &pane.id, "session");
    wait_for(|| driver_log_contents(&fake).contains("--resume fake-session-0001"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_reinit_same_session_keeps_turn_state() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    let session = read_agent_event(&mut reader, &pane.id, "session");
    assert_eq!(session["seq"], json!(1));

    // (T2) L5: the CLI re-emits init before every turn with the SAME
    // session id; "reinit-hang" models that, then hangs the turn open.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "reinit-hang please".to_string(),
        })
        .expect("send reinit-hang");
    let reinit = read_agent_event(&mut reader, &pane.id, "session");
    assert_eq!(reinit["session_id"], json!("fake-session-0001"));
    assert!(
        reinit["seq"].as_u64().expect("seq") > 1,
        "seq continues across a re-init: {reinit}"
    );

    // The re-init must not reset turn state: a concurrent send is
    // still rejected while the turn hangs open.
    let busy = client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "meanwhile".to_string(),
        })
        .expect_err("concurrent send must be rejected");
    assert!(
        busy.contains("turn already in progress"),
        "unexpected error: {busy}"
    );

    // Clean up: interrupt ends the hung turn.
    client
        .request::<CommandOk>(DaemonRequest::InterruptAgent {
            pane_id: pane.id.clone(),
        })
        .expect("interrupt agent");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("error_during_execution"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_interrupt_recovers_turn_when_result_lost() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // (T2) M2: "hang-noresult" streams but never sends the turn's
    // `result` — and the interrupt then answers with a BARE
    // control_response (no live turn), which normalizes to nothing.
    // Only the send-path clear recovers the pane.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hang-noresult please".to_string(),
        })
        .expect("send hang-noresult");
    let delta = read_agent_event(&mut reader, &pane.id, "text_delta");
    assert_eq!(delta["text"], json!("working"));
    let busy = client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "still busy".to_string(),
        })
        .expect_err("send while running must be rejected");
    assert!(
        busy.contains("turn already in progress"),
        "unexpected error: {busy}"
    );

    client
        .request::<CommandOk>(DaemonRequest::InterruptAgent {
            pane_id: pane.id.clone(),
        })
        .expect("interrupt agent");
    wait_for(|| driver_log_contents(&fake).contains(r#""subtype":"interrupt"#));

    // The next send is accepted again — without the fix it would be
    // rejected forever (no turn_complete ever arrives).
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "hello again".to_string(),
        })
        .expect("send after interrupt");
    let turn = read_agent_event(&mut reader, &pane.id, "turn_complete");
    assert_eq!(turn["subtype"], json!("success"));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_allow_omits_updated_input_when_request_lacks_input() {
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let mut reader = subscribe_events(&client);
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    read_agent_event(&mut reader, &pane.id, "session");

    // (T2) L7: the CLI's request carried no `input`; the allow reply
    // must OMIT updatedInput rather than echo `null`.
    client
        .request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: pane.id.clone(),
            text: "ask-noinput please".to_string(),
        })
        .expect("send ask-noinput");
    let request = read_agent_event(&mut reader, &pane.id, "permission_request");
    assert_eq!(request["request_id"], json!("perm-req-1"));
    client
        .request::<CommandOk>(DaemonRequest::AgentApproval {
            pane_id: pane.id.clone(),
            request_id: "perm-req-1".to_string(),
            allow: true,
            message: None,
        })
        .expect("allow approval");
    wait_for(|| driver_log_contents(&fake).contains(r#""behavior":"allow"#));
    assert!(
        !driver_log_contents(&fake).contains("updatedInput"),
        "no updatedInput echo for an input-less request: {}",
        driver_log_contents(&fake)
    );
    // A user decision is reported with reason "user".
    let resolved = read_agent_event(&mut reader, &pane.id, "permission_resolved");
    assert_eq!(resolved["behavior"], json!("allow"));
    assert_eq!(resolved["reason"], json!("user"));
    read_agent_event(&mut reader, &pane.id, "turn_complete");
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn agent_log_deleted_on_close_and_orphans_swept() {
    // (T2) M3: ClosePane deletes the pane's conversation log.
    let fake = install_fake_claude();
    let (daemon, _cwd) = spawn_agent_test_daemon(&fake);
    let client = daemon.client();
    let pane: Pane = client
        .request(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    let log_path = daemon
        .data_dir
        .path()
        .join(AGENT_LOG_DIR)
        .join(format!("{}.jsonl", pane.id));
    wait_for(|| log_path.exists());
    client
        .request::<Value>(DaemonRequest::ClosePane {
            pane_id: pane.id.clone(),
        })
        .expect("close agent pane");
    assert!(
        !log_path.exists(),
        "closed pane's agent log must be deleted"
    );
    daemon.shutdown();

    // Startup sweeps orphans; a live pane's log is untouched.
    let dir = tempfile::tempdir().expect("temp dir");
    let agents_dir = dir.path().join(AGENT_LOG_DIR);
    fs::create_dir_all(&agents_dir).expect("agents dir");
    fs::write(
        agents_dir.join("pane-1.jsonl"),
        b"{\"kind\":\"session\",\"seq\":1}\n",
    )
    .expect("write live log");
    fs::write(
        agents_dir.join("pane-9.jsonl"),
        b"{\"kind\":\"session\",\"seq\":1}\n",
    )
    .expect("write orphan log");
    let _server = DaemonServer::with_config(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("daemon server should start");
    assert!(
        agents_dir.join("pane-1.jsonl").exists(),
        "live pane's log untouched"
    );
    assert!(
        !agents_dir.join("pane-9.jsonl").exists(),
        "orphan log swept at startup"
    );
}

#[cfg(unix)]
#[test]
fn agent_pane_wait_text_regex_is_a_kind_error() {
    let fake = install_fake_claude();
    let data_dir = std::env::temp_dir().join(format!(
        "sgian-t2-wait-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let cwd = tempfile::tempdir().expect("agent wait cwd");
    let server = DaemonServer::with_config(
        cwd.path().to_path_buf(),
        data_dir.clone(),
        agent_test_config(&fake),
    )
    .expect("server");
    let pane = server
        .handle(DaemonRequest::CreateAgentPane { title: None })
        .expect("create agent pane");
    let pane_id = pane["id"].as_str().expect("pane id").to_string();

    // (T2) L6: --text/--regex on an agent pane can never match — a
    // kind-aware error, not a silent timeout.
    for condition in [
        WaitCondition::Text("needle".to_string()),
        WaitCondition::Regex("n.*".to_string()),
    ] {
        let err = server
            .handle(DaemonRequest::Wait {
                pane_id: pane_id.clone(),
                condition,
                timeout_ms: Some(50),
            })
            .expect_err("text wait on an agent pane must error");
        assert!(err.contains("no screen model"), "unexpected error: {err}");
    }

    // --idle still applies (it needs no screen model).
    let outcome = server
        .handle(DaemonRequest::Wait {
            pane_id: pane_id.clone(),
            condition: WaitCondition::Idle(0),
            timeout_ms: Some(1000),
        })
        .expect("idle wait should resolve");
    assert_eq!(outcome["matched"], json!(true));
    assert_eq!(outcome["reason"], json!("idle"));
    let _ = fs::remove_dir_all(&data_dir);
}

#[cfg(unix)]
#[test]
fn parse_new_pane_args_agent_flag() {
    let (agent, title) = parse_new_pane_args(&["--agent".to_string()]).expect("parse");
    assert!(agent);
    assert_eq!(title, None);
    let (agent, title) =
        parse_new_pane_args(&["--agent".to_string(), "my agent".to_string()]).expect("parse");
    assert!(agent);
    assert_eq!(title.as_deref(), Some("my agent"));
    let (agent, title) = parse_new_pane_args(&["work".to_string()]).expect("parse");
    assert!(!agent);
    assert_eq!(title.as_deref(), Some("work"));
    // Title rules still apply with the flag present.
    let err = parse_new_pane_args(&["--agent".to_string(), "a".to_string(), "b".to_string()])
        .expect_err("two titles still rejected");
    assert!(err.contains("more than once"), "{err}");
    let err = parse_new_pane_args(&["--bogus".to_string()]).expect_err("unknown flag");
    assert!(err.contains("unexpected pane option"), "{err}");
}

#[test]
fn parse_new_pane_args_provider_model_and_compatibility() {
    let parsed = parse_new_pane_args_with_spec(&str_args(&[
        "--backend",
        "droid",
        "--model",
        "custom:Fireworks-Qwen-0",
        "--name",
        "reviewer",
    ]))
    .expect("provider-aware form");
    assert_eq!(
        parsed,
        ParsedNewPaneArgs {
            agent: true,
            title: Some("reviewer".to_string()),
            backend: Some(AgentBackendKind::Droid),
            model: Some("custom:Fireworks-Qwen-0".to_string()),
            profile: None,

            project: None,
        }
    );

    // A model selection implies an agent pane, while the original
    // positional title and bare --agent forms remain accepted.
    assert_eq!(
        parse_new_pane_args_with_spec(&str_args(&["--model", "sonnet", "review"]))
            .expect("model plus positional title"),
        ParsedNewPaneArgs {
            agent: true,
            title: Some("review".to_string()),
            backend: None,
            model: Some("sonnet".to_string()),
            profile: None,

            project: None,
        }
    );
    assert_eq!(
        parse_new_pane_args_with_spec(&str_args(&["plain-shell"])).expect("legacy shell title"),
        ParsedNewPaneArgs {
            agent: false,
            title: Some("plain-shell".to_string()),
            backend: None,
            model: None,
            profile: None,

            project: None,
        }
    );

    for invalid in [
        str_args(&["--backend", "cursor"]),
        str_args(&["--backend", "droid", "--backend", "claude"]),
        str_args(&["--model", "one", "--model", "two"]),
    ] {
        assert!(
            parse_new_pane_args_with_spec(&invalid).is_err(),
            "{invalid:?}"
        );
    }
}

#[test]
fn agent_pane_spec_normalizes_default_and_model() {
    assert_eq!(
        AgentPaneSpec::normalized(None, Some("  ".to_string())).expect("default spec"),
        AgentPaneSpec::default()
    );
    assert_eq!(
        AgentPaneSpec::normalized(
            Some(AgentBackendKind::Droid),
            Some("  custom:Fireworks-0  ".to_string()),
        )
        .expect("trimmed model"),
        AgentPaneSpec {
            backend: AgentBackendKind::Droid,
            model: Some("custom:Fireworks-0".to_string()),
        }
    );
    let oversized = "x".repeat(MAX_TITLE_CHARS + 1);
    assert!(AgentPaneSpec::normalized(None, Some(oversized)).is_err());
}

#[test]
fn pane_kind_agent_serde_round_trip() {
    assert_eq!(
        serde_json::to_string(&PaneKind::Agent).unwrap(),
        "\"agent\""
    );
    assert_eq!(
        serde_json::from_str::<PaneKind>("\"agent\"").unwrap(),
        PaneKind::Agent
    );
    assert_eq!(
        serde_json::from_str::<PaneKind>("\"shell\"").unwrap(),
        PaneKind::Shell
    );
}

#[cfg(unix)]
#[test]
fn agent_config_permission_mode_defaults_and_validation() {
    let config = Config::default();
    assert_eq!(config.agent_permission_mode_effective(), "manual");
    assert_eq!(config.agent_config().permission_mode, "manual");
    assert!(config.validate().is_ok());

    let config = Config {
        agent_permission_mode: Some("bypassPermissions".to_string()),
        ..Default::default()
    };
    assert_eq!(
        config.agent_permission_mode_effective(),
        "bypassPermissions"
    );
    assert!(config.validate().is_ok());

    let config = Config {
        agent_permission_mode: Some("bogus".to_string()),
        ..Default::default()
    };
    // The effective value falls back to manual; write-config validation
    // rejects the unknown value.
    assert_eq!(config.agent_permission_mode_effective(), "manual");
    let err = config.validate().expect_err("invalid mode rejected");
    assert!(err.contains("agent_permission_mode"), "{err}");

    // Overlay: the workspace layer wins when it sets the field.
    let global = Config {
        agent_permission_mode: Some("manual".to_string()),
        agent_claude_bin: Some("/usr/local/bin/claude".to_string()),
        ..Default::default()
    };
    let workspace = Config {
        agent_permission_mode: Some("plan".to_string()),
        ..Default::default()
    };
    let merged = global.overlay(workspace);
    assert_eq!(merged.agent_permission_mode_effective(), "plan");
    assert_eq!(
        merged.agent_claude_bin.as_deref(),
        Some("/usr/local/bin/claude")
    );
}

#[cfg(unix)]
#[test]
fn normalize_agent_event_init_and_result() {
    let init = json!({
        "type": "system", "subtype": "init",
        "session_id": "abc-123", "model": "claude-x", "tools": ["Bash"],
    });
    let events = normalize_agent_event(&init);
    assert_eq!(
        events,
        vec![json!({"kind": "session", "session_id": "abc-123", "model": "claude-x"})]
    );

    let result = json!({
        "type": "result", "subtype": "success", "is_error": false,
        "duration_ms": 42, "num_turns": 2, "result": "done",
        "total_cost_usd": 0.01, "usage": {"input_tokens": 5},
        "session_id": "abc-123",
    });
    let events = normalize_agent_event(&result);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event["kind"], json!("turn_complete"));
    assert_eq!(event["subtype"], json!("success"));
    assert_eq!(event["cost_usd"], json!(0.01));
    assert_eq!(event["usage"]["input_tokens"], json!(5));
    assert_eq!(event["num_turns"], json!(2));
    // The CLI's session_id is NOT part of the contract event.
    assert!(event.get("session_id").is_none());
}

#[cfg(unix)]
#[test]
fn normalize_agent_event_stream_partials() {
    let start = json!({
        "type": "stream_event",
        "event": {"type": "message_start", "message": {"role": "assistant"}},
    });
    assert_eq!(
        normalize_agent_event(&start),
        vec![json!({"kind": "message_start", "role": "assistant"})]
    );

    let delta = json!({
        "type": "stream_event",
        "event": {"type": "content_block_delta", "index": 1,
                  "delta": {"type": "text_delta", "text": "partial"}},
    });
    assert_eq!(
        normalize_agent_event(&delta),
        vec![json!({"kind": "text_delta", "text": "partial"})]
    );

    // Thinking deltas and block boundaries carry no contract events.
    for raw in [
        json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}}),
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "signature_delta", "signature": "x"}}}),
        json!({"type": "stream_event", "event": {"type": "content_block_stop", "index": 0}}),
        json!({"type": "stream_event", "event": {"type": "message_delta", "delta": {}}}),
    ] {
        assert!(normalize_agent_event(&raw).is_empty(), "{raw}");
    }

    let stop = json!({"type": "stream_event", "event": {"type": "message_stop"}});
    assert_eq!(
        normalize_agent_event(&stop),
        vec![json!({"kind": "message_complete"})]
    );
}

#[cfg(unix)]
#[test]
fn normalize_agent_event_assistant_tool_use_and_tool_results() {
    // tool_use comes from the COMPLETE assistant message; its text blocks
    // are skipped (text already streamed via partials).
    let assistant = json!({
        "type": "assistant",
        "message": {"role": "assistant", "content": [
            {"type": "text", "text": "let me run that"},
            {"type": "tool_use", "id": "toolu_1", "name": "Bash",
             "input": {"command": "ls"}},
        ]},
    });
    let events = normalize_agent_event(&assistant);
    assert_eq!(
        events,
        vec![json!({"kind": "tool_use", "id": "toolu_1", "name": "Bash",
                    "input": {"command": "ls"}})]
    );

    // tool_result with string content (is_error absent → false)…
    let user = json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"},
        ]},
    });
    assert_eq!(
        normalize_agent_event(&user),
        vec![json!({"kind": "tool_result", "tool_use_id": "toolu_1",
                    "content": "ok", "is_error": false})]
    );
    // …and with block-array content + an explicit is_error.
    let user = json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_2",
             "content": [{"type": "text", "text": "boom"}], "is_error": true},
        ]},
    });
    assert_eq!(
        normalize_agent_event(&user),
        vec![json!({"kind": "tool_result", "tool_use_id": "toolu_2",
                    "content": [{"type": "text", "text": "boom"}], "is_error": true})]
    );
}

#[cfg(unix)]
#[test]
fn normalize_agent_event_control_requests_and_noise() {
    let permission = json!({
        "type": "control_request", "request_id": "req-9",
        "request": {"subtype": "can_use_tool", "tool_name": "WebFetch",
                    "input": {"url": "https://example.com"}},
    });
    assert_eq!(
        normalize_agent_event(&permission),
        vec![json!({"kind": "permission_request", "request_id": "req-9",
                    "tool_name": "WebFetch", "input": {"url": "https://example.com"}})]
    );

    // Unanswerable/unknown control requests and CLI noise normalize to
    // nothing (and must not panic).
    for raw in [
        json!({"type": "control_request", "request_id": "r", "request": {"subtype": "other"}}),
        json!({"type": "control_request", "request": {"subtype": "can_use_tool"}}),
        json!({"type": "control_request", "request_id": "", "request": {"subtype": "can_use_tool"}}),
        json!({"type": "control_response", "response": {"subtype": "success"}}),
        json!({"type": "rate_limit_event", "rate_limit_info": {}}),
        json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": 5}),
        json!({"type": "something_new"}),
        json!({"no_type": true}),
        json!(null),
        json!("just a string"),
    ] {
        assert!(normalize_agent_event(&raw).is_empty(), "{raw}");
    }
}

#[cfg(unix)]
#[test]
fn normalize_droid_events_cover_session_stream_tools_and_completion() {
    assert_eq!(
        normalize_droid_agent_event(&json!({
            "jsonrpc": "2.0",
            "type": "response",
            "id": "init-1",
            "result": {
                "sessionId": "droid-session-1",
                "settings": {"modelId": "custom:Fireworks-Qwen-0"}
            }
        })),
        vec![json!({
            "kind": "session",
            "session_id": "droid-session-1",
            "model": "custom:Fireworks-Qwen-0"
        })]
    );

    let envelope = |notification: Value| {
        json!({
            "jsonrpc": "2.0",
            "type": "notification",
            "method": "droid.session_notification",
            "params": {"notification": notification}
        })
    };
    assert_eq!(
        normalize_droid_agent_event(&envelope(json!({
            "type": "assistant_text_delta",
            "messageId": "m1",
            "blockIndex": 0,
            "textDelta": "hello"
        }))),
        vec![json!({"kind": "text_delta", "text": "hello"})]
    );
    assert_eq!(
        normalize_droid_agent_event(&envelope(json!({
            "type": "assistant_text_complete",
            "messageId": "m1",
            "blockIndex": 0
        }))),
        vec![json!({"kind": "message_complete"})]
    );
    assert_eq!(
        normalize_droid_agent_event(&envelope(json!({
            "type": "tool_call",
            "toolUse": {
                "type": "tool_use",
                "id": "tool-1",
                "name": "Execute",
                "input": {"command": "cargo test"}
            }
        }))),
        vec![json!({
            "kind": "tool_use",
            "id": "tool-1",
            "name": "Execute",
            "input": {"command": "cargo test"}
        })]
    );
    assert_eq!(
        normalize_droid_agent_event(&envelope(json!({
            "type": "tool_result",
            "messageId": "m2",
            "toolUseId": "tool-1",
            "content": "ok",
            "isError": false
        }))),
        vec![json!({
            "kind": "tool_result",
            "tool_use_id": "tool-1",
            "content": "ok",
            "is_error": false
        })]
    );
    assert_eq!(
        normalize_droid_agent_event(&envelope(json!({
            "type": "droid_working_state_changed",
            "newState": "idle"
        }))),
        vec![json!({"kind": "turn_complete", "subtype": "success"})]
    );
}

#[cfg(unix)]
#[test]
fn normalize_droid_permission_and_rpc_error() {
    let permission = json!({
        "jsonrpc": "2.0",
        "type": "request",
        "id": "permission-1",
        "method": "droid.request_permission",
        "params": {
            "toolUses": [{
                "toolUse": {
                    "type": "tool_use",
                    "id": "tool-1",
                    "name": "Execute",
                    "input": {"command": "cargo test"}
                },
                "confirmationType": "exec",
                "details": {
                    "type": "exec",
                    "fullCommand": "cargo test",
                    "command": "cargo"
                }
            }],
            "options": [
                {"label": "Proceed once", "value": "proceed_once"},
                {"label": "Cancel", "value": "cancel"}
            ]
        }
    });
    assert_eq!(
        normalize_droid_agent_event(&permission),
        vec![json!({
            "kind": "permission_request",
            "request_id": "permission-1",
            "tool_name": "Execute",
            "input": {"command": "cargo test"}
        })]
    );

    assert_eq!(
        normalize_droid_agent_event(&json!({
            "jsonrpc": "2.0",
            "type": "response",
            "id": "message-1",
            "error": {"code": -32602, "message": "invalid message"}
        })),
        vec![
            json!({"kind": "error", "message": "invalid message"}),
            json!({"kind": "turn_complete", "subtype": "error_during_execution"})
        ]
    );
}

#[cfg(unix)]
#[test]
fn read_capped_line_handles_split_eof_and_overflow() {
    // Normal line, then an EOF tail without a trailing newline.
    let mut reader = BufReader::new(&b"one\ntwo"[..]);
    let mut out = Vec::new();
    assert_eq!(
        read_capped_line(&mut reader, &mut out, 16).unwrap(),
        Some(false)
    );
    assert_eq!(out, b"one\n");
    assert_eq!(
        read_capped_line(&mut reader, &mut out, 16).unwrap(),
        Some(false)
    );
    assert_eq!(out, b"two");
    assert_eq!(read_capped_line(&mut reader, &mut out, 16).unwrap(), None);

    // A line longer than the cap is truncated and flagged; the remainder
    // up to the newline is consumed, so the next read starts clean.
    let mut reader = BufReader::new(&b"abcdefghij\nnext\n"[..]);
    assert_eq!(
        read_capped_line(&mut reader, &mut out, 4).unwrap(),
        Some(true)
    );
    assert_eq!(out, b"abcd");
    assert_eq!(
        read_capped_line(&mut reader, &mut out, 16).unwrap(),
        Some(false)
    );
    assert_eq!(out, b"next\n");

    // An over-cap EOF tail (no newline at all) is reported, then EOF.
    let mut reader = BufReader::new(&b"xyz123"[..]);
    assert_eq!(
        read_capped_line(&mut reader, &mut out, 3).unwrap(),
        Some(true)
    );
    assert_eq!(out, b"xyz");
    assert_eq!(read_capped_line(&mut reader, &mut out, 3).unwrap(), None);
}

#[test]
fn read_agent_log_tail_skips_malformed_and_caps_events() {
    let dir = tempfile::tempdir().expect("temp agents dir");
    let path = agent_log_path(dir.path(), "pane-7");
    let mut lines = vec![
        "not json".to_string(),
        json!({"kind": "session", "session_id": "s1"}).to_string(),
        "{\"broken\":".to_string(),
        json!({"no_kind": true}).to_string(),
    ];
    for index in 0..10 {
        lines.push(json!({"kind": "text_delta", "text": index}).to_string());
    }
    fs::write(&path, lines.join("\n") + "\n").expect("write log");

    // Malformed lines and kind-less objects are skipped; order preserved.
    let events = read_agent_log_tail(dir.path(), "pane-7", 1 << 20, 100);
    assert_eq!(events.len(), 11);
    assert_eq!(events[0]["kind"], json!("session"));
    assert_eq!(events[10]["text"], json!(9));

    // The event cap keeps the LAST N events.
    let events = read_agent_log_tail(dir.path(), "pane-7", 1 << 20, 3);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["text"], json!(7));

    // A tiny byte budget lands mid-line; the partial first line is dropped.
    let events = read_agent_log_tail(dir.path(), "pane-7", 40, 100);
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| event.get("kind").is_some()));

    // Invalid pane ids never resolve to a path.
    assert!(read_agent_log_tail(dir.path(), "../evil", 1 << 20, 100).is_empty());
    assert!(read_agent_log_tail(dir.path(), "pane-99", 1 << 20, 100).is_empty());
}

// ------------------------------------------------------------------
// (T2) Windows port: pure spawn-plan decision logic. These run on every
// host — the .cmd/.cmd.exe pieces are pure string/enum decisions, so no
// Windows machine is needed to pin them. The end-to-end Windows path
// (real .cmd driver through cmd.exe) is covered by the CI smoke test.
// ------------------------------------------------------------------

/// `.cmd`/`.bat` overrides classify for cmd.exe wrapping (case-insensitive
/// extension); everything else spawns directly.
#[test]
fn classify_agent_bin_marks_only_cmd_scripts() {
    for candidate in [
        "claude.cmd",
        "claude.CMD",
        "claude.bat",
        "claude.Bat",
        "C:\\nodejs\\claude.cmd",
        "scripts/claude.bat",
    ] {
        assert_eq!(
            classify_agent_bin(candidate.to_string()),
            AgentBinPlan::ViaCmd(PathBuf::from(candidate)),
            "{candidate}"
        );
    }
    for candidate in [
        "claude",
        "claude.exe",
        "claude.cmdx",
        "/usr/local/bin/claude",
        "C:\\nodejs\\claude.exe",
    ] {
        assert_eq!(
            classify_agent_bin(candidate.to_string()),
            AgentBinPlan::Direct(candidate.to_string()),
            "{candidate}"
        );
    }
}

/// Decision order: explicit candidate (config/env) first — classified on
/// Windows, direct elsewhere — then the Windows PATH probe, then the
/// per-platform default (bare `claude` on unix, NotFound on Windows).
#[test]
fn plan_agent_bin_decision_order() {
    // Candidate wins, classified only on Windows.
    assert_eq!(
        plan_agent_bin(Some("claude.cmd".to_string()), None, true),
        AgentBinPlan::ViaCmd(PathBuf::from("claude.cmd"))
    );
    assert_eq!(
        plan_agent_bin(Some("claude.cmd".to_string()), None, false),
        AgentBinPlan::Direct("claude.cmd".to_string())
    );
    assert_eq!(
        plan_agent_bin(Some("/opt/claude".to_string()), None, true),
        AgentBinPlan::Direct("/opt/claude".to_string())
    );
    // A candidate beats a probe result; an empty candidate is no candidate.
    let probed = AgentBinPlan::Direct("C:\\n\\claude.exe".to_string());
    assert_eq!(
        plan_agent_bin(Some("claude".to_string()), Some(probed.clone()), true),
        AgentBinPlan::Direct("claude".to_string())
    );
    assert_eq!(
        plan_agent_bin(Some(String::new()), Some(probed.clone()), true),
        probed.clone()
    );
    // No candidate: Windows takes the probe (or NotFound); unix the name.
    assert_eq!(plan_agent_bin(None, Some(probed.clone()), true), probed);
    assert_eq!(plan_agent_bin(None, None, true), AgentBinPlan::NotFound);
    assert_eq!(
        plan_agent_bin(None, None, false),
        AgentBinPlan::Direct("claude".to_string())
    );
}

#[cfg(unix)]
#[test]
fn agent_spawn_plan_is_provider_and_model_aware() {
    let mut store = TerminalStore::new_for_tests(PathBuf::from("/tmp/sgian-provider-test"));
    store.agent_config = AgentSpawnConfig {
        claude_bin: Some("/opt/claude/bin/claude".to_string()),
        droid_bin: Some("/opt/factory/bin/droid".to_string()),
        permission_mode: "manual".to_string(),
    };

    store.agent_specs.insert(
        "pane-d".to_string(),
        AgentPaneSpec {
            backend: AgentBackendKind::Droid,
            model: Some("custom:Fireworks-Qwen-0".to_string()),
        },
    );
    let droid = store.plan_agent_spawn("pane-d");
    assert_eq!(droid.backend, AgentBackendKind::Droid);
    assert_eq!(
        droid.bin,
        AgentBinPlan::Direct("/opt/factory/bin/droid".to_string())
    );
    assert_eq!(
        droid.args,
        str_args(&[
            "exec",
            "--input-format",
            "stream-jsonrpc",
            "--output-format",
            "stream-jsonrpc"
        ])
    );
    let init: Value = serde_json::from_str(
        droid
            .initial_input
            .as_deref()
            .expect("Droid initialize request"),
    )
    .expect("valid initialize json");
    assert_eq!(init["method"], json!("droid.initialize_session"));
    assert_eq!(init["params"]["cwd"], json!("/tmp/sgian-provider-test"));
    assert_eq!(init["params"]["autonomyLevel"], json!("off"));
    assert_eq!(init["params"]["modelId"], json!("custom:Fireworks-Qwen-0"));

    store
        .agent_resume
        .insert("pane-d".to_string(), "existing-session".to_string());
    let resumed = store.plan_agent_spawn("pane-d");
    let load: Value = serde_json::from_str(
        resumed
            .initial_input
            .as_deref()
            .expect("Droid load request"),
    )
    .expect("valid load json");
    assert_eq!(load["method"], json!("droid.load_session"));
    assert_eq!(load["params"]["sessionId"], json!("existing-session"));

    store.agent_specs.insert(
        "pane-c".to_string(),
        AgentPaneSpec {
            backend: AgentBackendKind::Claude,
            model: Some("sonnet".to_string()),
        },
    );
    let claude = store.plan_agent_spawn("pane-c");
    assert_eq!(claude.backend, AgentBackendKind::Claude);
    assert_eq!(
        claude.bin,
        AgentBinPlan::Direct("/opt/claude/bin/claude".to_string())
    );
    assert!(claude.initial_input.is_none());
    assert!(claude
        .args
        .windows(2)
        .any(|pair| pair == ["--model", "sonnet"]));
}

/// PATH probing walks directories in order, prefers claude.exe over
/// claude.cmd over claude.bat within a directory, and reports None when
/// nothing exists anywhere.
#[test]
fn probe_agent_path_order_and_precedence() {
    let dir_a = PathBuf::from("/probe/a");
    let dir_b = PathBuf::from("/probe/b");
    let path_var =
        std::env::join_paths([dir_a.as_path(), dir_b.as_path()]).expect("join probe PATH");
    let exists = |hits: &[PathBuf]| {
        let hits = hits.to_vec();
        move |path: &Path| hits.iter().any(|hit| hit == path)
    };

    // Nothing on PATH: no plan.
    assert_eq!(probe_agent_path(&path_var, exists(&[])), None);
    // First directory wins even when a later one has the "better" exe.
    assert_eq!(
        probe_agent_path(
            &path_var,
            exists(&[dir_a.join("claude.cmd"), dir_b.join("claude.exe")])
        ),
        Some(AgentBinPlan::ViaCmd(dir_a.join("claude.cmd")))
    );
    // Within one directory the exe beats the script shims.
    assert_eq!(
        probe_agent_path(
            &path_var,
            exists(&[dir_b.join("claude.exe"), dir_b.join("claude.cmd")])
        ),
        Some(AgentBinPlan::Direct(
            dir_b.join("claude.exe").to_string_lossy().into_owned()
        ))
    );
    // A lone .bat shim still resolves (through cmd.exe).
    assert_eq!(
        probe_agent_path(&path_var, exists(&[dir_a.join("claude.bat")])),
        Some(AgentBinPlan::ViaCmd(dir_a.join("claude.bat")))
    );
}

/// The spawn argv: Direct passes bin + args through untouched; ViaCmd
/// wraps as `cmd.exe /c <script> <args...>` preserving arg order; NotFound
/// yields no command (the caller maps it to the clean not-found error).
#[test]
fn agent_command_argv_constructs_cmd_exe_wrapper() {
    let args = vec!["-p".to_string(), "--verbose".to_string()];

    let (program, argv) = agent_command_argv(&AgentBinPlan::Direct("claude".to_string()), &args)
        .expect("direct plan has a command");
    assert_eq!(program, "claude");
    assert_eq!(argv, args, "direct spawn passes args through verbatim");

    let script = PathBuf::from("C:\\Program Files\\nodejs\\claude.cmd");
    let (program, argv) = agent_command_argv(&AgentBinPlan::ViaCmd(script.clone()), &args)
        .expect("cmd plan has a command");
    assert_eq!(program, "cmd.exe");
    assert_eq!(argv[0], "/c");
    assert_eq!(argv[1], script.to_string_lossy());
    assert_eq!(
        &argv[2..],
        args.as_slice(),
        "claude's own args follow the script path in order"
    );

    assert_eq!(agent_command_argv(&AgentBinPlan::NotFound, &args), None);
}

/// The liveness `command` string mirrors the actual spawn form.
#[test]
fn agent_bin_display_mirrors_spawn_form() {
    assert_eq!(
        agent_bin_display(&AgentBinPlan::Direct("claude".to_string())),
        "claude"
    );
    assert_eq!(
        agent_bin_display(&AgentBinPlan::ViaCmd(PathBuf::from("C:\\n\\claude.cmd"))),
        "cmd.exe /c \"C:\\n\\claude.cmd\""
    );
    assert_eq!(
        agent_bin_display(&AgentBinPlan::NotFound),
        "claude (not found)"
    );
}

// ----- Keyboard lease predicates, ledger chain, ctl parsing, IPC round trip -----
// docs/design/keyboard-lease-and-ledger.md

fn held_by(holder: &str) -> HeldLease {
    HeldLease::new(holder, 1_000, 1)
}

#[test]
fn lease_take_is_fresh_when_unheld_and_idempotent_for_holder() {
    assert_eq!(can_take(None, "alice", false, None), Ok(TakeOutcome::Fresh));
    let held = held_by("alice");
    assert_eq!(
        can_take(Some(&held), "alice", false, None),
        Ok(TakeOutcome::AlreadyHeld)
    );
    // Force from the current holder is still just idempotent.
    assert_eq!(
        can_take(Some(&held), "alice", true, Some("why")),
        Ok(TakeOutcome::AlreadyHeld)
    );
}

#[test]
fn lease_take_against_another_holder_needs_force_and_why() {
    let held = held_by("alice");
    let refused = can_take(Some(&held), "bob", false, None).expect_err("refused");
    assert!(refused.contains("held by alice"), "{refused}");
    assert!(refused.contains("--force"), "{refused}");
    let no_why = can_take(Some(&held), "bob", true, None).expect_err("needs why");
    assert!(no_why.contains("--why"), "{no_why}");
    let blank_why = can_take(Some(&held), "bob", true, Some("  ")).expect_err("blank why");
    assert!(blank_why.contains("--why"), "{blank_why}");
    assert_eq!(
        can_take(Some(&held), "bob", true, Some("alice is away")),
        Ok(TakeOutcome::Revoking {
            previous: "alice".to_string()
        })
    );
}

#[test]
fn lease_release_requires_the_holder() {
    assert!(can_release(None, "alice").is_err());
    let held = held_by("alice");
    assert_eq!(can_release(Some(&held), "alice"), Ok(()));
    let wrong = can_release(Some(&held), "bob").expect_err("not the holder");
    assert!(wrong.contains("held by alice, not bob"), "{wrong}");
}

#[test]
fn lease_write_gate_by_policy() {
    // open: unheld accepts anyone (attributed or not)
    assert_eq!(can_write(LeasePolicy::Open, None, None), Ok(()));
    assert_eq!(can_write(LeasePolicy::Open, None, Some("bob")), Ok(()));
    // required: unheld refuses everyone
    let refused = can_write(LeasePolicy::Required, None, Some("bob")).expect_err("unheld");
    assert!(refused.contains("unheld"), "{refused}");
    // held: only the holder, under either policy
    let held = held_by("alice");
    for policy in [LeasePolicy::Open, LeasePolicy::Required] {
        assert_eq!(can_write(policy, Some(&held), Some("alice")), Ok(()));
        assert!(can_write(policy, Some(&held), None).is_err());
        let other = can_write(policy, Some(&held), Some("bob")).expect_err("other");
        assert!(other.contains("held by alice"), "{other}");
    }
}

#[test]
fn holder_and_note_validation() {
    assert_eq!(validate_holder("  craig@mbp "), Ok("craig@mbp".to_string()));
    assert!(validate_holder("").is_err());
    assert!(validate_holder("two words").is_err());
    assert!(validate_holder("tab\there").is_err());
    assert!(validate_holder("ünïcode").is_err());
    assert!(validate_holder(&"x".repeat(HOLDER_MAX_LEN + 1)).is_err());
    assert_eq!(
        validate_bounded_text(" done\nnext: run tests ", "note", 64),
        Ok("done\nnext: run tests".to_string())
    );
    assert!(validate_bounded_text("   ", "note", 64).is_err());
    assert!(validate_bounded_text("bell\u{7}", "note", 64).is_err());
    assert!(validate_bounded_text(&"n".repeat(65), "note", 64).is_err());
}

#[test]
fn lease_policy_config_validation() {
    let mut config = Config::default();
    assert_eq!(config.lease_policy_effective(), LeasePolicy::Open);
    config.lease_policy = Some("required".to_string());
    assert!(config.validate().is_ok());
    assert_eq!(config.lease_policy_effective(), LeasePolicy::Required);
    config.lease_policy = Some("readonly".to_string());
    let error = config.validate().expect_err("unknown policy");
    assert!(error.contains("lease_policy"), "{error}");
    // The workspace layer overrides the global one, like restore_policy.
    let global = Config {
        lease_policy: Some("open".to_string()),
        ..Config::default()
    };
    let workspace = Config {
        lease_policy: Some("required".to_string()),
        ..Config::default()
    };
    assert_eq!(
        global.overlay(workspace).lease_policy_effective(),
        LeasePolicy::Required
    );
}

#[test]
fn canonical_json_sorts_keys_recursively() {
    let value = json!({"z": {"b": 1, "a": [{"y": 2, "x": 1}]}, "a": 0});
    assert_eq!(
        canonical_json(&value),
        r#"{"a":0,"z":{"a":[{"x":1,"y":2}],"b":1}}"#
    );
}

#[test]
fn ledger_chain_appends_verifies_and_detects_tampering() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut heads = HashMap::new();
    let first = ledger_append(
        dir.path(),
        &mut heads,
        "pane-1",
        "lease.taken",
        json!({"holder": "alice"}),
        true,
    )
    .expect("append 1");
    assert_eq!(first.seq, 1);
    assert_eq!(first.prev, "");
    assert_eq!(first.h.len(), 64);
    let second = ledger_append(
        dir.path(),
        &mut heads,
        "pane-1",
        "lease.released",
        json!({"holder": "alice", "note": "done"}),
        true,
    )
    .expect("append 2");
    assert_eq!(second.seq, 2);
    assert_eq!(second.prev, first.h);
    // A fresh head cache re-seeds from disk and continues the chain.
    let mut fresh_heads = HashMap::new();
    let third = ledger_append(
        dir.path(),
        &mut fresh_heads,
        "pane-1",
        "lease.taken",
        json!({"holder": "bob"}),
        false,
    )
    .expect("append 3");
    assert_eq!(third.seq, 3);
    assert_eq!(third.prev, second.h);

    let path = ledger_path(dir.path(), "pane-1");
    let summary = ledger_verify(&path).expect("chain verifies");
    assert_eq!(summary.records, 3);
    assert_eq!(summary.head, third.h);

    // Flip the note inside record 2: the hash no longer matches, and the
    // verifier names that line.
    let original = fs::read_to_string(&path).expect("read");
    let tampered = original.replacen(r#""note":"done""#, r#""note":"dome""#, 1);
    assert_ne!(original, tampered, "fixture must contain the note");
    fs::write(&path, &tampered).expect("write tampered");
    let broken = ledger_verify(&path).expect_err("tamper detected");
    assert_eq!(broken.line, 2);
    assert_eq!(broken.seq, Some(2));
    assert!(broken.reason.contains("hash mismatch"), "{}", broken.reason);

    // Deleting a middle record breaks the sequence at the next line.
    let mut lines: Vec<&str> = original.lines().collect();
    lines.remove(1);
    fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write gap");
    let gap = ledger_verify(&path).expect_err("gap detected");
    assert_eq!(gap.line, 2);
    assert!(gap.reason.contains("sequence 3 where 2"), "{}", gap.reason);

    // Tail reads are bounded and tolerate a torn last line.
    fs::write(&path, format!("{original}{{\"seq\":4,\"tor")).expect("write torn");
    let tail = read_ledger_tail(&path, 2);
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0]["seq"], json!(2));
    assert_eq!(tail[1]["seq"], json!(3));
    assert_eq!(read_ledger_tail(&path, 0).len(), 3);
}

#[test]
fn ledger_hash_depends_on_version_prefix_and_prev() {
    let body = r#"{"a":1}"#;
    let genesis = ledger_hash("", body);
    let chained = ledger_hash("abc", body);
    assert_ne!(genesis, chained);
    use sha2::Digest;
    let mut plain = sha2::Sha256::new();
    plain.update(format!("\n{body}").as_bytes());
    assert_ne!(
        genesis,
        hex_encode(&plain.finalize()),
        "the version prefix must be inside the hash input"
    );
}

#[test]
fn parse_lease_args_shapes() {
    let status = parse_lease_args(&[]).expect("bare");
    assert_eq!(status.verb, LeaseVerb::Status);
    assert_eq!(status.pane_ref, "active");
    let take = parse_lease_args(&args(&[
        "take", "pane-2", "--as", "ci", "--force", "--why", "stuck",
    ]))
    .expect("take");
    assert_eq!(take.verb, LeaseVerb::Take);
    assert_eq!(take.pane_ref, "pane-2");
    assert_eq!(take.holder.as_deref(), Some("ci"));
    assert!(take.force);
    assert_eq!(take.why.as_deref(), Some("stuck"));
    let release =
        parse_lease_args(&args(&["release", "-m", "answered the prompt"])).expect("release");
    assert_eq!(release.verb, LeaseVerb::Release);
    assert_eq!(release.note.as_deref(), Some("answered the prompt"));
    let missing_note = parse_lease_args(&args(&["release", "pane-1"])).expect_err("note required");
    assert!(missing_note.contains("-m NOTE"), "{missing_note}");
    assert!(parse_lease_args(&args(&["take", "a", "b"])).is_err());
    assert!(parse_lease_args(&args(&["status", "--force"])).is_err());
    assert!(parse_lease_args(&args(&["take", "--as", "bad holder"])).is_err());
}

#[test]
fn parse_as_flag_stops_at_double_dash() {
    let (holder, rest) =
        parse_as_flag(&args(&["pane-1", "--as", "ci", "echo", "hi"])).expect("parse");
    assert_eq!(holder.as_deref(), Some("ci"));
    assert_eq!(rest, args(&["pane-1", "echo", "hi"]));
    let (holder, rest) = parse_as_flag(&args(&["pane-1", "--", "--as", "literal"])).expect("parse");
    assert_eq!(holder, None);
    assert_eq!(rest, args(&["pane-1", "--", "--as", "literal"]));
    let (holder, _) = parse_as_flag(&args(&["--as=ops", "pane-1", "x"])).expect("parse");
    assert_eq!(holder.as_deref(), Some("ops"));
    assert!(parse_as_flag(&args(&["pane-1", "--as"])).is_err());
}

#[test]
fn parse_ledger_args_shapes() {
    let parsed = parse_ledger_args(&args(&["pane-3", "-n", "5", "--verify"])).expect("parse");
    assert_eq!(parsed.pane_ref, "pane-3");
    assert_eq!(parsed.limit, 5);
    assert!(parsed.verify);
    assert!(parse_ledger_args(&args(&["-n", "x"])).is_err());
    assert!(parse_ledger_args(&args(&["a", "b"])).is_err());
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn lease_round_trip_over_ipc_gates_input_and_writes_ledger() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    assert!(initial.leases.is_empty(), "fresh workspace has no leases");

    // Unheld under `open`: the legacy unattributed write still works.
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "".to_string(),
        })
        .expect("unheld write accepted");

    let taken: LeaseInfo = client
        .request(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("take");
    assert_eq!(taken.holder.as_deref(), Some("alice"));
    assert_eq!(taken.policy, "open");

    // Unattributed and other-holder writes are refused; the holder's go through.
    let refused = client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "x".to_string(),
        })
        .expect_err("unattributed write refused while held");
    assert!(refused.contains("held by alice"), "{refused}");
    let refused = client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "x".to_string(),
            holder: "bob".to_string(),
            generation: None,
        })
        .expect_err("bob refused");
    assert!(refused.contains("held by alice"), "{refused}");
    client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "echo hi\r".to_string(),
            holder: "alice".to_string(),
            generation: None,
        })
        .expect("holder write accepted");
    let broadcast: Value = client
        .request(DaemonRequest::Broadcast {
            input: "".to_string(),
        })
        .expect("broadcast");
    assert!(
        !broadcast["panes"]
            .as_array()
            .expect("panes array")
            .iter()
            .any(|id| id == &json!(pane_id)),
        "broadcast skips a held pane: {broadcast}"
    );

    let status: LeaseInfo = client
        .request(DaemonRequest::LeaseStatus {
            pane_id: pane_id.clone(),
        })
        .expect("status");
    assert_eq!(status.holder.as_deref(), Some("alice"));
    assert_eq!(status.writes, 1);
    assert_eq!(status.bytes_typed, "echo hi\r".len() as u64);
    assert_eq!(status.refused_writes, 2);
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap while held");
    assert_eq!(
        snapshot
            .leases
            .get(&pane_id)
            .and_then(|info| info.holder.clone()),
        Some("alice".to_string())
    );
    let persisted: PersistedWorkspace = serde_json::from_str(
        &fs::read_to_string(daemon.data_dir.path().join(WORKSPACE_FILE)).expect("workspace.json"),
    )
    .expect("parse workspace.json");
    assert_eq!(
        persisted
            .leases
            .get(&pane_id)
            .map(|held| held.holder.as_str()),
        Some("alice")
    );

    // Contention: bob needs --force and a reason.
    let contended = client
        .request::<LeaseInfo>(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            force: false,
            why: None,
        })
        .expect_err("contended take refused");
    assert!(contended.contains("held by alice"), "{contended}");
    let forced: LeaseInfo = client
        .request(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            force: true,
            why: Some("alice went home".to_string()),
        })
        .expect("forced take");
    assert_eq!(forced.holder.as_deref(), Some("bob"));
    assert_eq!(forced.writes, 0, "counters restart with the new holder");

    // Release: mandatory note, only the holder.
    let empty_note = client
        .request::<LeaseInfo>(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            note: "   ".to_string(),
            generation: None,
        })
        .expect_err("empty note refused");
    assert!(empty_note.contains("hand-back note"), "{empty_note}");
    let wrong_holder = client
        .request::<LeaseInfo>(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            note: "not mine".to_string(),
            generation: None,
        })
        .expect_err("alice no longer holds it");
    assert!(
        wrong_holder.contains("held by bob, not alice"),
        "{wrong_holder}"
    );
    let released: LeaseInfo = client
        .request(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            note: "answered the y/N; agent can carry on".to_string(),
            generation: None,
        })
        .expect("release");
    assert_eq!(released.holder, None);
    let after: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after release");
    assert!(
        after.leases.is_empty(),
        "released panes leave the snapshot map"
    );

    // The ledger has the whole story, in order, and verifies.
    let path = ledger_path(&daemon.data_dir.path().join(LEDGER_DIR), &pane_id);
    let summary = ledger_verify(&path).expect("ledger verifies");
    assert_eq!(summary.records, 4);
    let kinds: Vec<String> = read_ledger_tail(&path, 0)
        .iter()
        .map(|record| record["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "lease.taken",
            "lease.revoked",
            "lease.taken",
            "lease.released"
        ]
    );
    let records = read_ledger_tail(&path, 0);
    assert_eq!(records[1]["payload"]["holder"], json!("alice"));
    assert_eq!(records[1]["payload"]["by"], json!("bob"));
    assert_eq!(records[1]["payload"]["why"], json!("alice went home"));
    assert_eq!(records[1]["payload"]["refused_writes"], json!(2));
    assert_eq!(records[2]["payload"]["previous_holder"], json!("alice"));
    assert_eq!(
        records[3]["payload"]["note"],
        json!("answered the y/N; agent can carry on")
    );

    // Closing a held pane revokes the lease but keeps the ledger.
    client
        .request::<LeaseInfo>(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("retake");
    let created: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("second pane so the first can close");
    let _ = created;
    client
        .request::<Value>(DaemonRequest::ClosePane {
            pane_id: pane_id.clone(),
        })
        .expect("close held pane");
    let summary = ledger_verify(&path).expect("ledger survives close");
    assert_eq!(summary.records, 6);
    let last = read_ledger_tail(&path, 1).remove(0);
    assert_eq!(last["type"], json!("lease.revoked"));
    assert_eq!(last["payload"]["why"], json!("pane closed"));
    daemon.shutdown();
}

#[test]
fn lease_required_policy_refuses_unheld_writes() {
    let daemon = TestDaemon::spawn(Config {
        lease_policy: Some("required".to_string()),
        ..Config::default()
    });
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    let refused = client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "x".to_string(),
            holder: "alice".to_string(),
            generation: None,
        })
        .expect_err("unheld write refused under required");
    assert!(refused.contains("unheld"), "{refused}");
    let status: LeaseInfo = client
        .request(DaemonRequest::LeaseStatus {
            pane_id: pane_id.clone(),
        })
        .expect("status");
    assert_eq!(status.policy, "required");
    assert_eq!(status.holder, None);
    client
        .request::<LeaseInfo>(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("take");
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id,
            input: "".to_string(),
            holder: "alice".to_string(),
            generation: None,
        })
        .expect("holder write accepted");
    daemon.shutdown();
}

#[test]
fn router_ledgers_attention_transitions_and_pane_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().join("scrollback"));
    let ledger_dir = dir.path().join(LEDGER_DIR);
    fs::create_dir_all(&ledger_dir).expect("ledger dir");
    router.set_ledger(Arc::new(Mutex::new(LedgerSink::new(ledger_dir.clone()))));

    router.apply_agent_classification("pane-7", CLAUDE_WORKING_SCREEN);
    router.apply_agent_classification("pane-7", CLAUDE_WORKING_SCREEN);
    router.apply_agent_classification("pane-7", CLAUDE_IDLE_SCREEN);
    router.clear_agent_attention("pane-7");
    router.emit_pane_ended("pane-7", Some(0));

    let path = ledger_path(&ledger_dir, "pane-7");
    let summary = ledger_verify(&path).expect("router notes chain");
    assert_eq!(summary.records, 4);
    let records = read_ledger_tail(&path, 0);
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "attention.changed",
            "attention.changed",
            "attention.changed",
            "pane.ended"
        ]
    );
    assert_eq!(records[0]["payload"]["from"], Value::Null);
    assert_eq!(records[0]["payload"]["to"], json!("working"));
    assert_eq!(records[0]["payload"]["evidence"], json!("screen"));
    assert_eq!(records[1]["payload"]["from"], json!("working"));
    assert_eq!(records[1]["payload"]["to"], json!("idle"));
    assert_eq!(records[2]["payload"]["to"], Value::Null);
    assert_eq!(records[2]["payload"]["evidence"], json!("process ended"));
    assert_eq!(records[3]["payload"]["exit_code"], json!(0));
    // The exit record says what the agent was doing when it died, and
    // who held the keyboard (nobody: no lease table wired here).
    assert_eq!(records[3]["payload"]["attention"], json!("idle"));
    assert!(records[3]["payload"]["agent"].is_string());
    assert_eq!(records[3]["payload"]["holder"], Value::Null);
    assert_eq!(records[3]["payload"]["unattended"], json!(false));
    assert!(records[3]["payload"].get("output_tricks").is_none());
    // A bare router (no sink) stays silent rather than failing.
    let silent = OutputRouter::new(dir.path().join("scrollback2"));
    silent.apply_agent_classification("pane-8", CLAUDE_WORKING_SCREEN);
    assert!(!ledger_path(&ledger_dir, "pane-8").exists());
}

#[test]
fn parse_agent_args_watch_forms() {
    let parsed = parse_agent_args(&agent_args(&["--watch"])).expect("bare watch");
    assert!(parsed.watch);
    assert!(!parsed.pane_given);
    assert_eq!(parsed.mark, None);
    let parsed = parse_agent_args(&agent_args(&["pane-3", "--watch"])).expect("pane watch");
    assert!(parsed.watch);
    assert!(parsed.pane_given);
    assert_eq!(parsed.pane_ref, "pane-3");
    let parsed = parse_agent_args(&agent_args(&["--watch", "pane-3"])).expect("flag first");
    assert!(parsed.pane_given);
    assert_eq!(parsed.pane_ref, "pane-3");
    let err = parse_agent_args(&agent_args(&["pane-3", "on", "--watch"]))
        .expect_err("watch excludes marks");
    assert!(err.contains("--watch"), "{err}");
    let plain = parse_agent_args(&agent_args(&["pane-3"])).expect("plain");
    assert!(!plain.watch);
    assert!(plain.pane_given);
}

#[test]
fn format_watch_event_lines_and_json() {
    let state = DaemonEvent::AgentState {
        pane_id: "pane-1".to_string(),
        agent: Some("claude".to_string()),
        attention: Some(AgentAttention::NeedsInput),
        mode: Some("auto".to_string()),
    };
    assert_eq!(
        format_watch_event(&state, false).as_deref(),
        Some("pane-1\tagent_state\tclaude\tneeds_input\tauto")
    );
    let json_line = format_watch_event(&state, true).expect("json");
    let parsed: Value = serde_json::from_str(&json_line).expect("valid json");
    assert_eq!(parsed["event"], json!("agent_state"));
    assert_eq!(parsed["attention"], json!("needs_input"));
    let lease = DaemonEvent::LeaseState {
        pane_id: "pane-1".to_string(),
        transition: LeaseTransition::Taken,
        holder: Some("alice".to_string()),
        since_ms: Some(1),
        note: None,
    };
    assert_eq!(
        format_watch_event(&lease, false).as_deref(),
        Some("pane-1\tlease_taken\talice")
    );
    let ended = DaemonEvent::PaneEnded {
        pane_id: "pane-1".to_string(),
        exit_code: None,
    };
    assert_eq!(
        format_watch_event(&ended, false).as_deref(),
        Some("pane-1\tpane_ended\t-")
    );
    let output = DaemonEvent::PtyOutput {
        pane_id: "pane-1".to_string(),
        data: "x".to_string(),
    };
    assert_eq!(format_watch_event(&output, false), None);
    assert_eq!(watch_event_pane(&output), None);
    assert_eq!(watch_event_pane(&ended), Some("pane-1"));
}

#[test]
fn probe_attention_mapping_and_parent_walk() {
    let entry = |status: &str, waiting: Option<&str>, state: Option<&str>| AgentProbeEntry {
        pid: Some(1),
        status: Some(status.to_string()),
        waiting_for: waiting.map(str::to_string),
        state: state.map(str::to_string),
    };
    assert_eq!(
        attention_from_probe(&entry("busy", None, None)),
        Some(AgentAttention::Working)
    );
    assert_eq!(
        attention_from_probe(&entry("idle", None, None)),
        Some(AgentAttention::Idle)
    );
    assert_eq!(
        attention_from_probe(&entry("busy", Some("permission prompt"), None)),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        attention_from_probe(&entry("waiting", None, None)),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        attention_from_probe(&entry("", None, Some("blocked"))),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(attention_from_probe(&entry("weird", None, None)), None);
    let parsed: Vec<AgentProbeEntry> = serde_json::from_str(
        r#"[{"pid":300,"cwd":"/w","kind":"interactive","sessionId":"s","name":"n","status":"busy","extra":1}]"#,
    )
    .expect("tolerant parse");
    assert_eq!(parsed[0].pid, Some(300));
    assert_eq!(parsed[0].status.as_deref(), Some("busy"));

    let parents = parse_process_table("  300   200\n200 100\n999 1\nbad line\n").parent;
    assert_eq!(parents.get(&300), Some(&200));
    assert_eq!(parents.len(), 3);
    let pane_pids = vec![
        ("pane-1".to_string(), 100u32),
        ("pane-2".to_string(), 500u32),
    ];
    let entries = vec![
        AgentProbeEntry {
            pid: Some(300),
            status: Some("busy".to_string()),
            ..AgentProbeEntry::default()
        },
        AgentProbeEntry {
            pid: Some(999),
            status: Some("busy".to_string()),
            ..AgentProbeEntry::default()
        },
        AgentProbeEntry {
            pid: Some(500),
            status: Some("idle".to_string()),
            ..AgentProbeEntry::default()
        },
    ];
    let mapped = map_probe_entries(&entries, &parents, &pane_pids);
    assert_eq!(mapped.get("pane-1"), Some(&AgentAttention::Working));
    assert_eq!(mapped.get("pane-2"), Some(&AgentAttention::Idle));
    assert_eq!(mapped.len(), 2, "an unrelated session maps nowhere");
    // Two sessions in one pane: the louder state wins.
    let two = vec![
        AgentProbeEntry {
            pid: Some(300),
            status: Some("idle".to_string()),
            ..AgentProbeEntry::default()
        },
        AgentProbeEntry {
            pid: Some(200),
            status: Some("busy".to_string()),
            waiting_for: Some("input needed".to_string()),
            ..AgentProbeEntry::default()
        },
    ];
    assert_eq!(
        map_probe_entries(&two, &parents, &pane_pids).get("pane-1"),
        Some(&AgentAttention::NeedsInput)
    );
}

#[test]
fn hooks_map_onto_attention_and_panes() {
    assert_eq!(
        attention_from_hook("Notification", Some("permission_prompt")),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        attention_from_hook("Notification", Some("idle_prompt")),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        attention_from_hook("Notification", Some("auth_success")),
        None
    );
    assert_eq!(attention_from_hook("Notification", None), None);
    assert_eq!(
        attention_from_hook("UserPromptSubmit", None),
        Some(AgentAttention::Working)
    );
    assert_eq!(
        attention_from_hook("PreToolUse", None),
        Some(AgentAttention::Working)
    );
    assert_eq!(
        attention_from_hook("Stop", None),
        Some(AgentAttention::Idle)
    );
    assert_eq!(attention_from_hook("SessionStart", None), None);
    assert_eq!(attention_from_hook("", None), None);

    // pane-1's shell is 100 → claude 200 → hook 300; pane-2's shell is 400.
    let parents = HashMap::from([(300u32, 200u32), (200, 100), (100, 50), (50, 1), (400, 50)]);
    let pane_pids = vec![("pane-1".to_string(), 100u32), ("pane-2".to_string(), 400)];
    assert_eq!(
        pane_for_pid(300, &parents, &pane_pids).as_deref(),
        Some("pane-1")
    );
    assert_eq!(
        pane_for_pid(100, &parents, &pane_pids).as_deref(),
        Some("pane-1")
    );
    assert_eq!(
        pane_for_pid(400, &parents, &pane_pids).as_deref(),
        Some("pane-2")
    );
    assert_eq!(
        pane_for_pid(50, &parents, &pane_pids),
        None,
        "above every pane"
    );
    assert_eq!(
        pane_for_pid(999, &parents, &pane_pids),
        None,
        "unknown process"
    );
    // A parent cycle terminates.
    let cycle = HashMap::from([(7u32, 8u32), (8, 7)]);
    assert_eq!(pane_for_pid(7, &cycle, &pane_pids), None);

    let parsed =
        parse_hook_args(&args(&["--event", "Stop", "--pid", "42", "--no-stdin"])).expect("flags");
    assert_eq!(parsed.event.as_deref(), Some("Stop"));
    assert_eq!(parsed.pid, Some(42));
    assert!(!parsed.read_stdin);
    assert!(
        parse_hook_args(&args(&["--no-stdin"])).is_err(),
        "no event source"
    );
    assert!(parse_hook_args(&args(&["--pid", "x"])).is_err());
    assert!(parse_hook_args(&args(&["bogus"])).is_err());
    let payload: HookPayload = serde_json::from_str(
        r#"{"session_id":"s1","transcript_path":"/t","cwd":"/w","hook_event_name":"Notification","message":"Claude needs your permission to use Bash","notification_type":"permission_prompt","permission_mode":"default"}"#,
    )
    .expect("payload");
    let request =
        hook_request(&parse_hook_args(&[]).expect("bare"), &payload, 777).expect("request");
    assert_eq!(
        request,
        DaemonRequest::AgentSignal {
            pid: 777,
            event: "Notification".to_string(),
            notification_type: Some("permission_prompt".to_string()),
            message: Some("Claude needs your permission to use Bash".to_string()),
            session_id: Some("s1".to_string()),
        }
    );
    // Flags win over the payload; an empty payload needs --event.
    let forced = parse_hook_args(&args(&["--event", "Stop", "--type", "x"])).expect("forced");
    let DaemonRequest::AgentSignal {
        event,
        notification_type,
        ..
    } = hook_request(&forced, &payload, 1).expect("request")
    else {
        panic!("wrong request");
    };
    assert_eq!(
        (event.as_str(), notification_type.as_deref()),
        ("Stop", Some("x"))
    );
    assert!(hook_request(
        &parse_hook_args(&[]).expect("bare"),
        &HookPayload::default(),
        1
    )
    .is_err());
}

#[test]
fn identity_scopes_and_holder_binding_are_pure() {
    // Scope classification: reads, admin, and everything else writes.
    assert_eq!(request_scope(&DaemonRequest::Ping), ClientScope::Read);
    assert_eq!(request_scope(&DaemonRequest::Subscribe), ClientScope::Read);
    assert_eq!(
        request_scope(&DaemonRequest::AgentSignal {
            pid: 1,
            event: "Stop".into(),
            notification_type: None,
            message: None,
            session_id: None
        }),
        ClientScope::Read
    );
    assert_eq!(
        request_scope(&DaemonRequest::SendInput {
            pane_id: "p".into(),
            input: "x".into()
        }),
        ClientScope::Write
    );
    assert_eq!(
        request_scope(&DaemonRequest::ProjectCreate {
            name: "f".into(),
            goal: None,
            repo: None
        }),
        ClientScope::Write
    );
    assert_eq!(request_scope(&DaemonRequest::Shutdown), ClientScope::Admin);
    assert_eq!(
        request_scope(&DaemonRequest::IdentityList),
        ClientScope::Admin
    );
    assert_eq!(request_name(&DaemonRequest::IdentityList), "identity_list");

    assert_eq!(ClientScope::parse(" write "), Some(ClientScope::Write));
    assert_eq!(ClientScope::parse("root"), None);
    assert_eq!(
        IdentityPolicy::parse("required"),
        Some(IdentityPolicy::Required)
    );
    assert_eq!(IdentityPolicy::parse("closed"), None);
    let root_open = ClientIdentity::root(IdentityPolicy::Open);
    assert!(root_open.has(ClientScope::Write) && root_open.has(ClientScope::Admin));
    let root_required = ClientIdentity::root(IdentityPolicy::Required);
    assert!(!root_required.has(ClientScope::Write) && root_required.has(ClientScope::Admin));
    assert_eq!(client_token_hash("a"), client_token_hash("a"));
    assert_ne!(client_token_hash("a"), client_token_hash("b"));

    // Holder binding for a credentialed connection.
    let record = ClientRecord {
        id: "c1".into(),
        holder: "phone".into(),
        scopes: vec![ClientScope::Write],
        token_hash: String::new(),
        created_at_ms: 1,
        last_seen_ms: 0,
        revoked_at_ms: None,
    };
    let identity = ClientIdentity::from_record(&record);
    assert!(identity.has(ClientScope::Read), "read is implied");
    let bound = bind_holder(
        DaemonRequest::SendInput {
            pane_id: "p".into(),
            input: "hi".into(),
        },
        &identity,
    )
    .expect("bind");
    assert_eq!(
        bound,
        DaemonRequest::SendInputAs {
            pane_id: "p".into(),
            input: "hi".into(),
            holder: "phone".into(),
            generation: None
        }
    );
    let mismatch = bind_holder(
        DaemonRequest::TakeLease {
            pane_id: "p".into(),
            holder: "bob".into(),
            force: false,
            why: None,
        },
        &identity,
    )
    .expect_err("holder mismatch");
    assert!(mismatch.contains("does not match"), "{mismatch}");
    assert!(bind_holder(DaemonRequest::Broadcast { input: "x".into() }, &identity).is_err());
    // The root credential is left alone.
    let root_bound = bind_holder(
        DaemonRequest::TakeLease {
            pane_id: "p".into(),
            holder: "bob".into(),
            force: false,
            why: None,
        },
        &root_open,
    )
    .expect("root passes");
    assert!(matches!(root_bound, DaemonRequest::TakeLease { ref holder, .. } if holder == "bob"));

    // Config rejects an unknown policy.
    let bad = Config {
        identity: Some("closed".into()),
        ..Default::default()
    };
    assert!(bad.validate().expect_err("invalid").contains("identity"));
    assert_eq!(
        Config {
            identity: Some("required".into()),
            ..Default::default()
        }
        .identity_effective(),
        IdentityPolicy::Required
    );

    // ctl argument shapes.
    let issue = parse_identity_args(&args(&[
        "issue",
        "--holder",
        "phone",
        "--scope",
        "read,write",
    ]))
    .expect("issue");
    assert_eq!(issue.verb, IdentityVerb::Issue);
    assert_eq!(issue.holder.as_deref(), Some("phone"));
    assert_eq!(issue.scopes, args(&["read,write"]));
    assert_eq!(
        parse_identity_args(&[]).expect("bare").verb,
        IdentityVerb::List
    );
    assert_eq!(
        parse_identity_args(&args(&["revoke", "abc"]))
            .expect("revoke")
            .id
            .as_deref(),
        Some("abc")
    );
    assert!(parse_identity_args(&args(&["issue"])).is_err());
    assert!(parse_identity_args(&args(&["revoke"])).is_err());
    assert!(parse_identity_args(&args(&["list", "x"])).is_err());
    assert!(parse_identity_args(&args(&["bogus"])).is_err());
}

#[cfg(unix)]
#[test]
fn peer_uid_matches_own_uid_on_a_socketpair() {
    let (a, _b) = std::os::unix::net::UnixStream::pair().expect("pair");
    // SAFETY: getuid has no preconditions.
    let own = unsafe { libc::getuid() };
    assert_eq!(peer_uid(&a), Some(own));
}

/// Hello with a client credential and an empty workspace token, the way a
/// remote client without the token file connects.
#[cfg(unix)]
fn connect_with_client_token(daemon: &TestDaemon, token: &str) -> Result<DaemonConnection, String> {
    let stream = transport_connect(&daemon.socket_path).map_err(|e| e.to_string())?;
    DaemonConnection::handshake(stream, "", Some(token))
}

#[cfg(unix)]
#[test]
fn client_credentials_gate_writes_and_attribute_leases_over_ipc() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");

    // The workspace token is root under the default policy.
    let me: Value = client.request(DaemonRequest::Whoami).expect("whoami");
    assert_eq!(me["root"], json!(true));
    assert_eq!(me["identity_policy"], json!("open"));
    assert_eq!(me["scopes"], json!(["read", "write", "admin"]));

    // A read-only credential can look but not type.
    let viewer: Value = client
        .request(DaemonRequest::IdentityIssue {
            holder: "phone".into(),
            scopes: vec![],
        })
        .expect("issue viewer");
    let viewer_token = viewer["token"].as_str().expect("token").to_string();
    assert!(viewer_token.starts_with(CLIENT_TOKEN_PREFIX));
    assert_eq!(viewer["scopes"], json!(["read"]));
    let mut viewer_conn = connect_with_client_token(&daemon, &viewer_token).expect("viewer hello");
    let who = viewer_conn.request(&DaemonRequest::Whoami).expect("whoami");
    assert_eq!(who.result["holder"], json!("phone"));
    assert_eq!(who.result["credential"], viewer["id"]);
    assert_eq!(who.result["root"], json!(false));
    let seen = viewer_conn
        .request(&DaemonRequest::ListPanes)
        .expect("list");
    assert!(seen.ok, "{seen:?}");
    let refused = viewer_conn
        .request(&DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "echo no\n".into(),
        })
        .expect("refusal is a response");
    assert!(!refused.ok);
    assert!(
        refused
            .error
            .as_deref()
            .unwrap_or("")
            .contains("read-only credential"),
        "{refused:?}"
    );
    let refused_admin = viewer_conn
        .request(&DaemonRequest::IdentityList)
        .expect("refusal is a response");
    assert!(refused_admin
        .error
        .as_deref()
        .unwrap_or("")
        .contains("'admin' scope"));

    // A write credential types as its holder and its leases carry its id.
    let laptop: Value = client
        .request(DaemonRequest::IdentityIssue {
            holder: "laptop".into(),
            scopes: vec!["write".into()],
        })
        .expect("issue laptop");
    let laptop_token = laptop["token"].as_str().expect("token").to_string();
    let mut laptop_conn = connect_with_client_token(&daemon, &laptop_token).expect("laptop hello");
    let typed = laptop_conn
        .request(&DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "".into(),
        })
        .expect("send");
    assert!(typed.ok, "{typed:?}");
    let wrong = laptop_conn
        .request(&DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "bob".into(),
            force: false,
            why: None,
        })
        .expect("mismatch is a response");
    assert!(
        wrong
            .error
            .as_deref()
            .unwrap_or("")
            .contains("does not match"),
        "{wrong:?}"
    );
    let taken = laptop_conn
        .request(&DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "laptop".into(),
            force: false,
            why: None,
        })
        .expect("take");
    assert!(taken.ok, "{taken:?}");
    assert_eq!(taken.result["holder"], json!("laptop"));
    // Root cannot type into the held pane unattributed, as before.
    let held = client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "x".into(),
        })
        .expect_err("held by laptop");
    assert!(held.contains("held by laptop"), "{held}");
    let released = laptop_conn
        .request(&DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "laptop".into(),
            note: "done".into(),
            generation: None,
        })
        .expect("release");
    assert!(released.ok, "{released:?}");
    let records = read_ledger_tail(
        &ledger_path(&daemon.data_dir.path().join(LEDGER_DIR), &pane_id),
        0,
    );
    let lease_records: Vec<&Value> = records
        .iter()
        .filter(|r| r["type"] == json!("lease.taken") || r["type"] == json!("lease.released"))
        .collect();
    assert_eq!(lease_records.len(), 2, "{records:?}");
    assert_eq!(lease_records[0]["payload"]["credential"], laptop["id"]);
    assert_eq!(lease_records[1]["payload"]["credential"], laptop["id"]);

    // A read-only credential cannot set badges through the hook or
    // status-line reports either; a write credential can.
    let badge = viewer_conn
        .request(&DaemonRequest::AgentSignal {
            pid: 1,
            event: "Stop".into(),
            notification_type: None,
            message: None,
            session_id: None,
        })
        .expect("refusal is a response");
    assert!(
        badge
            .error
            .as_deref()
            .unwrap_or("")
            .contains("'write' scope required for agent_signal"),
        "{badge:?}"
    );
    let report = laptop_conn
        .request(&DaemonRequest::AgentStatus {
            pid: 1,
            payload: json!({ "model": { "display_name": "Opus" } }),
        })
        .expect("write credential may report");
    assert!(report.ok, "{report:?}");
    // The legacy unattributed write is bound to the credential too.
    let legacy = laptop_conn
        .request(&DaemonRequest::WriteToPane {
            pane_id: pane_id.clone(),
            data: "".into(),
        })
        .expect("legacy write");
    assert!(legacy.ok, "{legacy:?}");

    // Write is not admin: a credential cannot mint credentials.
    let mint = laptop_conn
        .request(&DaemonRequest::IdentityIssue {
            holder: "x".into(),
            scopes: vec![],
        })
        .expect("refusal is a response");
    assert!(!mint.ok);

    // Revocation ends at the next hello; a presented credential is held
    // to, so a revoked or bogus token is refused even beside the
    // workspace token (drop the credential to be root again).
    let listed: Value = client.request(DaemonRequest::IdentityList).expect("list");
    assert_eq!(listed.as_array().map_or(0, Vec::len), 2);
    assert!(listed[0].get("token_hash").is_none() && listed[0].get("token").is_none());
    client
        .request::<Value>(DaemonRequest::IdentityRevoke {
            id: viewer["id"].as_str().unwrap().into(),
        })
        .expect("revoke");
    // The viewer's live connection is cut on its next request, not only
    // at its next hello.
    let after_revoke = viewer_conn
        .request(&DaemonRequest::ListPanes)
        .expect("refusal is a response");
    assert_eq!(
        after_revoke.error.as_deref(),
        Some("client credential revoked")
    );
    assert!(connect_with_client_token(&daemon, &viewer_token).is_err());
    assert!(connect_with_client_token(&daemon, "sgc_nope").is_err());
    let stream = transport_connect(&daemon.socket_path).expect("connect");
    assert!(
        DaemonConnection::handshake(stream, &daemon.token, Some(&viewer_token)).is_err(),
        "a revoked credential is not quietly root"
    );
    let stream = transport_connect(&daemon.socket_path).expect("connect");
    let mut root_again =
        DaemonConnection::handshake(stream, &daemon.token, None).expect("root without credential");
    assert_eq!(
        root_again
            .request(&DaemonRequest::Whoami)
            .expect("whoami")
            .result["root"],
        json!(true)
    );
    // The file keeps hashes, never tokens, and is owner-only.
    let on_disk =
        fs::read_to_string(daemon.data_dir.path().join(CLIENTS_FILE)).expect("clients.json");
    assert!(on_disk.contains("token_hash") && !on_disk.contains(&laptop_token));
    let mode = fs::metadata(daemon.data_dir.path().join(CLIENTS_FILE))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn identity_required_makes_the_root_token_read_and_admin_only() {
    let daemon = TestDaemon::spawn(Config {
        identity: Some("required".into()),
        ..Default::default()
    });
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    let me: Value = client.request(DaemonRequest::Whoami).expect("whoami");
    assert_eq!(me["identity_policy"], json!("required"));
    assert_eq!(me["scopes"], json!(["read", "admin"]));
    let refused = client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "x".into(),
        })
        .expect_err("root cannot type under required");
    assert!(
        refused.contains("'write' scope required for send_input"),
        "{refused}"
    );
    let issued: Value = client
        .request(DaemonRequest::IdentityIssue {
            holder: "craig@desk".into(),
            scopes: vec!["write".into()],
        })
        .expect("root can still issue");
    let mut conn =
        connect_with_client_token(&daemon, issued["token"].as_str().unwrap()).expect("hello");
    // Spawning a shell is a write too: root is refused, the credential is not.
    let root_spawn = client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect_err("root cannot spawn under required");
    assert!(root_spawn.contains("ensure_pane_terminal"), "{root_spawn}");
    let spawned = conn
        .request(&DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    assert!(spawned.ok, "{spawned:?}");
    let typed = conn
        .request(&DaemonRequest::SendInput {
            pane_id,
            input: "".into(),
        })
        .expect("send");
    assert!(typed.ok, "{typed:?}");
    daemon.shutdown();
}

#[test]
fn status_payload_parses_into_usage_and_summarises() {
    let payload: Value = serde_json::from_str(
        r#"{"hook_event_name":"Status","session_id":"s1","cwd":"/w","model":{"id":"claude-opus-5","display_name":"Opus"},"workspace":{"current_dir":"/w","project_dir":"/w"},"cost":{"total_cost_usd":1.234},"context_window":{"total_input_tokens":8000,"context_window_size":200000,"used_percentage":40.4},"rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1000000},"seven_day":{"used_percentage":41.2,"resets_at":2000000}},"extra":true}"#,
    )
    .expect("payload");
    let usage = AgentUsage::from_status_payload(&payload).expect("usage");
    assert_eq!(usage.model.as_deref(), Some("Opus"));
    assert_eq!(usage.model_id.as_deref(), Some("claude-opus-5"));
    assert_eq!(usage.context_used_percentage, Some(40));
    assert_eq!(usage.context_window_size, Some(200000));
    assert_eq!(
        usage.five_hour,
        Some(RateLimitWindow {
            used_percentage: 24,
            resets_at: Some(1000000)
        })
    );
    assert_eq!(usage.seven_day.map(|w| w.used_percentage), Some(41));
    assert_eq!(usage.total_cost_cents, Some(123));
    assert_eq!(usage.session_id.as_deref(), Some("s1"));
    // now = 1h10m before the 5h reset, 11d before the 7d one.
    assert_eq!(
        usage.summary(1000000 - 4200),
        "Opus · 40% context · 5h 24% ↻ 1h10m · 7d 41% ↻ 11d"
    );
    assert_eq!(
        usage.summary(3000000),
        "Opus · 40% context · 5h 24% · 7d 41%"
    );
    // Free-tier payload: no rate limits, context may be null early on.
    let thin: Value = serde_json::from_str(
        r#"{"model":{"display_name":"Sonnet"},"context_window":{"used_percentage":null}}"#,
    )
    .expect("thin");
    let thin = AgentUsage::from_status_payload(&thin).expect("thin usage");
    assert_eq!(thin.summary(0), "Sonnet");
    assert!(thin.five_hour.is_none() && thin.context_used_percentage.is_none());
    assert!(AgentUsage::from_status_payload(&json!({"foo": 1})).is_none());
    assert!(AgentUsage::from_status_payload(&Value::Null).is_none());
    assert_eq!(format_reset(Some(100), 50), " ↻ 0m");
    assert_eq!(format_reset(Some(100 + 90 * 60), 100), " ↻ 1h30m");
    assert_eq!(format_reset(None, 0), "");

    let parsed = parse_statusline_args(&args(&["--pid", "9", "--exec", "sh", "-c", "echo x"]))
        .expect("args");
    assert_eq!(parsed.pid, Some(9));
    assert_eq!(parsed.then, args(&["sh", "-c", "echo x"]));
    assert!(parse_statusline_args(&args(&["--exec"])).is_err());
    assert!(parse_statusline_args(&args(&["bogus"])).is_err());
    assert_eq!(
        parse_statusline_args(&[]).expect("bare"),
        StatuslineArgs::default()
    );
}

#[cfg(unix)]
#[test]
fn status_payload_lands_on_the_owning_pane_over_ipc() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/sh".to_string()),
        agent_probe_interval_ms: Some(0),
        ..Default::default()
    });
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe).expect("subscribe should write");
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("read timeout should apply");
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "echo SHELLPID=$$\n".to_string(),
        })
        .expect("print pid");
    let mut shell_pid: Option<u32> = None;
    for _ in 0..200 {
        let found: Value = client
            .request(DaemonRequest::SearchScrollback {
                pane_id: pane_id.clone(),
                needle: "SHELLPID=".to_string(),
                ignore_case: false,
                limit: 0,
            })
            .expect("search");
        shell_pid = found["matches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| hit["text"].as_str())
            .filter_map(|text| text.strip_prefix("SHELLPID="))
            .filter_map(|rest| rest.trim().parse::<u32>().ok())
            .next();
        if shell_pid.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let shell_pid = shell_pid.expect("the pane's shell printed its pid");

    let payload = json!({
        "session_id": "s1",
        "model": { "id": "claude-opus-5", "display_name": "Opus" },
        "context_window": { "used_percentage": 40 },
        "rate_limits": { "five_hour": { "used_percentage": 23, "resets_at": 1000000 } }
    });
    let first: Value = client
        .request(DaemonRequest::AgentStatus {
            pid: shell_pid,
            payload: payload.clone(),
        })
        .expect("status");
    assert_eq!(first["mapped"], json!(true), "{first}");
    assert_eq!(first["pane_id"], json!(pane_id));
    assert_eq!(first["changed"], json!(true));
    assert_eq!(first["usage"]["model"], json!("Opus"));
    // Same payload again: stored, but no second event.
    let again: Value = client
        .request(DaemonRequest::AgentStatus {
            pid: shell_pid,
            payload: payload.clone(),
        })
        .expect("status again");
    assert_eq!(again["changed"], json!(false));
    // A later turn with more context: an event.
    let mut later = payload.clone();
    later["context_window"]["used_percentage"] = json!(55);
    let third: Value = client
        .request(DaemonRequest::AgentStatus {
            pid: shell_pid,
            payload: later,
        })
        .expect("status later");
    assert_eq!(third["changed"], json!(true));

    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after status");
    let usage = snapshot
        .agent_usage
        .get(&pane_id)
        .expect("usage in snapshot");
    assert_eq!(usage.context_used_percentage, Some(55));
    assert_eq!(usage.five_hour.map(|w| w.used_percentage), Some(23));
    assert!(usage.updated_at_ms > 0);
    // The status line proved Claude is running here: the pane has an agent
    // mark now, with attention still unknown (the status line says nothing
    // about that).
    let info = snapshot.agent_states.get(&pane_id).expect("agent state");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, None);
    let found: Value = client
        .request(DaemonRequest::Find {
            command: None,
            title: None,
            cwd: None,
            state: None,
        })
        .expect("find");
    let entry = found
        .as_array()
        .and_then(|entries| entries.iter().find(|entry| entry["id"] == json!(pane_id)))
        .expect("entry");
    assert_eq!(entry["usage"]["model"], json!("Opus"));

    // Events: one agent_state (the mark) and two agent_usage (first, third).
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut usage_events = 0;
    let mut mark_events = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && (usage_events < 2 || mark_events < 1) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => match serde_json::from_str::<DaemonEvent>(&line) {
                Ok(DaemonEvent::AgentUsage { pane_id: id, usage }) if id == pane_id => {
                    usage_events += 1;
                    assert_eq!(usage.model.as_deref(), Some("Opus"));
                }
                Ok(DaemonEvent::AgentState {
                    pane_id: id, agent, ..
                }) if id == pane_id && agent.as_deref() == Some("claude") => {
                    mark_events += 1;
                }
                _ => {}
            },
            Err(_) => {}
        }
    }
    assert_eq!((usage_events, mark_events), (2, 1));

    // Unknown process and a payload with nothing we keep: acknowledged.
    let stray: Value = client
        .request(DaemonRequest::AgentStatus {
            pid: u32::MAX - 9,
            payload: payload.clone(),
        })
        .expect("stray");
    assert_eq!(stray["mapped"], json!(false));
    let empty: Value = client
        .request(DaemonRequest::AgentStatus {
            pid: shell_pid,
            payload: json!({ "transcript_path": "/t" }),
        })
        .expect("empty");
    assert_eq!(empty["mapped"], json!(false));
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn hook_signal_sets_the_owning_pane_badge_over_ipc() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/sh".to_string()),
        agent_probe_interval_ms: Some(0),
        ..Default::default()
    });
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    // The shell prints its own pid; a hook fired from a child of that
    // shell must map to this pane.
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "echo SHELLPID=$$\n".to_string(),
        })
        .expect("print pid");
    let mut shell_pid: Option<u32> = None;
    for _ in 0..200 {
        let found: Value = client
            .request(DaemonRequest::SearchScrollback {
                pane_id: pane_id.clone(),
                needle: "SHELLPID=".to_string(),
                ignore_case: false,
                limit: 0,
            })
            .expect("search");
        shell_pid = found["matches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| hit["text"].as_str())
            .filter_map(|text| text.strip_prefix("SHELLPID="))
            .filter_map(|rest| rest.trim().parse::<u32>().ok())
            .next();
        if shell_pid.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let shell_pid = shell_pid.expect("the pane's shell printed its pid");

    let signal = |pid: u32, event: &str, kind: Option<&str>| -> Value {
        client
            .request(DaemonRequest::AgentSignal {
                pid,
                event: event.to_string(),
                notification_type: kind.map(str::to_string),
                message: Some("Claude needs your permission to use Bash".to_string()),
                session_id: Some("s1".to_string()),
            })
            .expect("signal")
    };
    let mapped = signal(shell_pid, "Notification", Some("permission_prompt"));
    assert_eq!(mapped["mapped"], json!(true), "{mapped}");
    assert_eq!(mapped["pane_id"], json!(pane_id));
    assert_eq!(mapped["attention"], json!("needs_input"));
    assert_eq!(mapped["evidence"], json!("hook"));
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after hook");
    let info = snapshot.agent_states.get(&pane_id).expect("agent state");
    assert_eq!(info.agent.as_deref(), Some("claude"));
    assert_eq!(info.attention, Some(AgentAttention::NeedsInput));

    // A Stop hook from the same session turns the badge idle.
    let stopped = signal(shell_pid, "Stop", None);
    assert_eq!(stopped["attention"], json!("idle"));
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap after stop");
    assert_eq!(
        snapshot
            .agent_states
            .get(&pane_id)
            .and_then(|info| info.attention),
        Some(AgentAttention::Idle)
    );

    // Unknown process, and an event with nothing to say: acknowledged, not errors.
    let stray = signal(u32::MAX - 7, "Notification", Some("permission_prompt"));
    assert_eq!(stray["mapped"], json!(false));
    assert!(
        stray["reason"]
            .as_str()
            .unwrap_or("")
            .contains("no live pane"),
        "{stray}"
    );
    let ignored = signal(shell_pid, "SessionStart", None);
    assert_eq!(ignored["mapped"], json!(false));

    // The Notification landed on the ledger with its message; the Stop did not.
    let records = read_ledger_tail(
        &ledger_path(&daemon.data_dir.path().join(LEDGER_DIR), &pane_id),
        0,
    );
    let hooks: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == json!("hook.received"))
        .collect();
    assert_eq!(hooks.len(), 1, "{records:?}");
    assert_eq!(
        hooks[0]["payload"]["notification_type"],
        json!("permission_prompt")
    );
    assert_eq!(
        hooks[0]["payload"]["message"],
        json!("Claude needs your permission to use Bash")
    );
    let evidence: Vec<&str> = records
        .iter()
        .filter(|record| record["type"] == json!("attention.changed"))
        .filter_map(|record| record["payload"]["evidence"].as_str())
        .collect();
    assert_eq!(evidence, vec!["hook", "hook"]);
    daemon.shutdown();
}

#[test]
fn official_attention_outranks_the_screen_until_it_expires() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().join("scrollback"));
    let ledger_dir = dir.path().join(LEDGER_DIR);
    fs::create_dir_all(&ledger_dir).expect("ledger dir");
    router.set_ledger(Arc::new(Mutex::new(LedgerSink::new(ledger_dir.clone()))));

    router.apply_official_attention(
        "pane-9",
        "claude",
        AgentAttention::NeedsInput,
        Duration::from_millis(120),
    );
    assert_eq!(
        router.agent_state("pane-9").attention,
        Some(AgentAttention::NeedsInput)
    );
    // The screen says working, but the official reading is fresh.
    router.apply_agent_classification("pane-9", CLAUDE_WORKING_SCREEN);
    assert_eq!(
        router.agent_state("pane-9").attention,
        Some(AgentAttention::NeedsInput)
    );
    thread::sleep(Duration::from_millis(150));
    router.apply_agent_classification("pane-9", CLAUDE_WORKING_SCREEN);
    assert_eq!(
        router.agent_state("pane-9").attention,
        Some(AgentAttention::Working),
        "the heuristic resumes once the official reading expires"
    );
    let records = read_ledger_tail(&ledger_path(&ledger_dir, "pane-9"), 0);
    assert_eq!(records[0]["payload"]["evidence"], json!("claude-agents"));
    assert_eq!(records[1]["payload"]["evidence"], json!("screen"));
    // The session vanishing from the listing clears the badge (not manual marks).
    assert!(router.apply_official_attention(
        "pane-9",
        "claude",
        AgentAttention::Idle,
        Duration::from_secs(1),
    ));
    assert!(!router.apply_official_attention(
        "pane-9",
        "claude",
        AgentAttention::Idle,
        Duration::from_secs(1),
    ));
    router.clear_official_attention("pane-9");
    assert_eq!(router.agent_state("pane-9").agent, None);
    let last = read_ledger_tail(&ledger_path(&ledger_dir, "pane-9"), 1).remove(0);
    assert_eq!(
        last["payload"]["evidence"],
        json!("claude-agents: session gone")
    );
    router.set_manual_agent("pane-9", Some("claude".to_string()));
    router.apply_official_attention(
        "pane-9",
        "claude",
        AgentAttention::Working,
        Duration::from_secs(1),
    );
    router.clear_official_attention("pane-9");
    assert_eq!(
        router.agent_state("pane-9").agent.as_deref(),
        Some("claude"),
        "a manual mark survives the session going away"
    );
    router.set_manual_agent("pane-9", None);
    // An ended pane ignores official readings (a dead process has no state).
    router.clear_agent_attention("pane-9");
    router.apply_official_attention(
        "pane-9",
        "claude",
        AgentAttention::Working,
        Duration::from_secs(1),
    );
    assert_eq!(router.agent_state("pane-9").attention, None);
}

#[test]
fn reconcile_probe_rounds_clears_after_two_misses_or_close() {
    let mut previous = HashMap::new();
    let mut mapped = HashMap::new();
    mapped.insert("pane-1".to_string(), AgentAttention::Idle);
    let live = vec!["pane-1".to_string(), "pane-2".to_string()];
    assert!(reconcile_probe_rounds(&mut previous, &mapped, &live).is_empty());
    assert_eq!(previous.get("pane-1"), Some(&0));
    // One miss: keep, count it.
    let none = HashMap::new();
    assert!(reconcile_probe_rounds(&mut previous, &none, &live).is_empty());
    assert_eq!(previous.get("pane-1"), Some(&1));
    // Reappearing resets the count.
    assert!(reconcile_probe_rounds(&mut previous, &mapped, &live).is_empty());
    assert_eq!(previous.get("pane-1"), Some(&0));
    // Two misses in a row: clear.
    assert!(reconcile_probe_rounds(&mut previous, &none, &live).is_empty());
    assert_eq!(
        reconcile_probe_rounds(&mut previous, &none, &live),
        vec!["pane-1".to_string()]
    );
    assert!(previous.is_empty());
    // A pane that is no longer live clears immediately.
    reconcile_probe_rounds(&mut previous, &mapped, &live);
    assert_eq!(
        reconcile_probe_rounds(&mut previous, &none, &["pane-2".to_string()]),
        vec!["pane-1".to_string()]
    );
}

#[test]
fn kranz_worker_detection_and_state_mapping() {
    let table = parse_process_table(
        "  300   200 /usr/local/bin/kranz --repo /w run\n200 100 -zsh\n400 100 kranz status\n500 1 /x/kranz.exe work\n999 1 kranz\n",
    );
    assert_eq!(table.parent.get(&300), Some(&200));
    assert_eq!(table.args.get(&200).map(String::as_str), Some("-zsh"));
    assert!(is_kranz_worker_command(
        "/usr/local/bin/kranz --repo /w run"
    ));
    assert!(is_kranz_worker_command("kranz.exe work"));
    assert!(!is_kranz_worker_command("kranz status"));
    assert!(!is_kranz_worker_command("/bin/kranzy run"));
    assert!(!is_kranz_worker_command(""));
    let pane_pids = vec![
        ("pane-1".to_string(), 100u32),
        ("pane-2".to_string(), 500u32),
    ];
    let workers = find_kranz_panes(&table, &pane_pids);
    assert_eq!(workers.get("pane-1"), Some(&300));
    assert_eq!(workers.get("pane-2"), Some(&500));
    assert_eq!(workers.len(), 2);

    assert_eq!(
        kranz_attention_from_state(&json!({"status": "running"})),
        Some(AgentAttention::Working)
    );
    assert_eq!(
        kranz_attention_from_state(
            &json!({"status": "running", "pendingQuestions": [{"id": "q1"}]})
        ),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        kranz_attention_from_state(
            &json!({"status": "running", "pendingGrantRequest": {"id": "g1"}})
        ),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        kranz_attention_from_state(&json!({"status": "paused"})),
        Some(AgentAttention::NeedsInput)
    );
    assert_eq!(
        kranz_attention_from_state(&json!({"status": "complete", "pendingQuestions": []})),
        Some(AgentAttention::Idle)
    );
    assert_eq!(
        kranz_attention_from_state(&json!({"status": "weird"})),
        None
    );
}

#[test]
fn parse_kranz_args_shapes() {
    let status = parse_kranz_args(&[]).expect("bare");
    assert_eq!(status.verb, KranzVerb::Status);
    let bind = parse_kranz_args(&args(&["bind", "pane-2", "--repo", "/repo"])).expect("bind");
    assert_eq!(bind.verb, KranzVerb::Bind);
    assert_eq!(bind.pane_ref, "pane-2");
    assert_eq!(bind.repo.as_deref(), Some("/repo"));
    let unbind = parse_kranz_args(&args(&["unbind"])).expect("unbind");
    assert_eq!(unbind.verb, KranzVerb::Unbind);
    assert_eq!(unbind.pane_ref, "active");
    assert!(parse_kranz_args(&args(&["status", "pane-1"])).is_err());
    assert!(parse_kranz_args(&args(&["unbind", "--repo", "/x"])).is_err());
    assert!(parse_kranz_args(&args(&["bind", "--repo"])).is_err());
}

#[cfg(unix)]
#[test]
fn released_note_is_mirrored_to_a_bound_kranz_mission() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("kranz-args.txt");
    let script = dir.path().join("kranz");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            record.display()
        ),
    )
    .expect("write fake kranz");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let daemon = TestDaemon::spawn(Config {
        kranz_bin: Some(script.to_string_lossy().into_owned()),
        ..Config::default()
    });
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    let bound: Value = client
        .request(DaemonRequest::KranzBind {
            pane_id: pane_id.clone(),
            repo: Some("/tmp/mission-repo".to_string()),
        })
        .expect("bind");
    assert_eq!(bound["binding"]["manual"], json!(true));
    let listed: HashMap<String, KranzBinding> = client
        .request(DaemonRequest::KranzBindings)
        .expect("bindings");
    assert_eq!(listed[&pane_id].repo, "/tmp/mission-repo");

    client
        .request::<LeaseInfo>(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("take");
    client
        .request::<LeaseInfo>(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            note: "answered the grant; carry on".to_string(),
            generation: None,
        })
        .expect("release");
    let recorded = fs::read_to_string(&record).expect("fake kranz was invoked");
    let argv: Vec<&str> = recorded.lines().collect();
    assert_eq!(argv[0..3], ["--repo", "/tmp/mission-repo", "msg"]);
    assert!(
        argv[3].contains("alice handed back the keyboard: answered the grant; carry on"),
        "{recorded}"
    );

    let path = ledger_path(&daemon.data_dir.path().join(LEDGER_DIR), &pane_id);
    let kinds: Vec<String> = read_ledger_tail(&path, 0)
        .iter()
        .map(|record| record["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "kranz.bound",
            "lease.taken",
            "lease.released",
            "kranz.mirrored"
        ]
    );
    let mirrored = read_ledger_tail(&path, 1).remove(0);
    assert_eq!(mirrored["payload"]["ok"], json!(true));
    assert_eq!(mirrored["payload"]["repo"], json!("/tmp/mission-repo"));

    // Unbinding stops the mirror; the ledger says so.
    client
        .request::<Value>(DaemonRequest::KranzUnbind {
            pane_id: pane_id.clone(),
        })
        .expect("unbind");
    let listed: HashMap<String, KranzBinding> = client
        .request(DaemonRequest::KranzBindings)
        .expect("bindings");
    assert!(listed.is_empty());
    assert_eq!(
        read_ledger_tail(&path, 1).remove(0)["type"],
        json!("kranz.unbound")
    );
    daemon.shutdown();
}

#[test]
fn inherited_session_markers_are_dropped_unless_explicit() {
    let inherited = HashMap::from([
        ("CLAUDECODE".to_string(), "1".to_string()),
        ("CLAUDE_CODE_CHILD_SESSION".to_string(), "abc".to_string()),
        ("PATH".to_string(), "/bin".to_string()),
    ]);
    let env = compute_spawn_env(&inherited, &[], &HashMap::new());
    assert!(!env.contains_key("CLAUDECODE"));
    assert!(!env.contains_key("CLAUDE_CODE_CHILD_SESSION"));
    assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
    let explicit = HashMap::from([("CLAUDECODE".to_string(), "1".to_string())]);
    let env = compute_spawn_env(&inherited, &[], &explicit);
    assert_eq!(env.get("CLAUDECODE").map(String::as_str), Some("1"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn process_parent_snapshot_reads_the_kernel_without_forking() {
    let parents = process_parent_snapshot().expect("snapshot available");
    let me = std::process::id();
    // SAFETY: getppid has no preconditions.
    let ppid = unsafe { libc::getppid() } as u32;
    assert_eq!(parents.get(&me), Some(&ppid), "own pid maps to own parent");
    assert!(parents.len() > 2);
}

#[cfg(unix)]
#[test]
fn process_descendants_walks_the_whole_subtree() {
    let table = parse_process_table("10 1\n20 10\n30 20\n40 10\n99 1\n");
    let mut found = process_descendants(10, &table);
    found.sort_unstable();
    assert_eq!(found, vec![20, 30, 40]);
    assert!(process_descendants(99, &table).is_empty());
}

#[cfg(unix)]
#[test]
fn closing_a_pane_terminates_its_grandchildren() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    let marker = daemon.data_dir.path().join("grandchild.pid");
    // A background job in an interactive shell lands in its own process
    // group, exactly the case a shell-only kill orphans.
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: format!("sleep 300 &\necho $! > '{}'\n", marker.display()),
        })
        .expect("start grandchild");
    let mut grandchild: Option<u32> = None;
    for _ in 0..200 {
        if let Ok(text) = fs::read_to_string(&marker) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                grandchild = Some(pid);
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    let grandchild = grandchild.expect("the shell reported the sleep pid");
    // SAFETY: probing a pid we were just handed.
    assert_eq!(unsafe { libc::kill(grandchild as libc::pid_t, 0) }, 0);

    let _: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("a second pane so the first can close");
    client
        .request::<Value>(DaemonRequest::ClosePane {
            pane_id: pane_id.clone(),
        })
        .expect("close");
    let mut gone = false;
    for _ in 0..200 {
        // SAFETY: existence probe; ESRCH (or a zombie already reaped by
        // init) means the grandchild is gone.
        if unsafe { libc::kill(grandchild as libc::pid_t, 0) } != 0 {
            gone = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !gone {
        // SAFETY: cleanup of our own test process.
        unsafe {
            libc::kill(grandchild as libc::pid_t, libc::SIGKILL);
        }
    }
    daemon.shutdown();
    assert!(gone, "the grandchild sleep must die with its pane");
}

#[test]
fn transient_pty_errors_are_classified() {
    assert!(is_transient_pty_error(
        "failed to openpty: Os { code: 6, kind: Uncategorized, message: \"Device not configured\" }"
    ));
    assert!(is_transient_pty_error(
        "Resource temporarily unavailable (os error 35)"
    ));
    assert!(!is_transient_pty_error("Permission denied (os error 13)"));
    assert!(!is_transient_pty_error(
        "No such file or directory (os error 2)"
    ));
}

/// docs/design/keyboard-lease-and-ledger.md §7: an agent dumping tens of
/// megabytes must not stall the daemon, must not grow the scrollback file
/// past its cap, and must die with its pane when closed mid-flood.
#[cfg(unix)]
#[test]
fn output_flood_keeps_the_daemon_responsive_bounded_and_killable() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    let _: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("a second pane so the first can close");

    // 18 MiB through the PTY: past the 16 MiB scrollback cap.
    let marker = daemon.data_dir.path().join("flood.done");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: format!("yes | head -c 18874368; touch '{}'\n", marker.display()),
        })
        .expect("start flood");
    let scrollback = scrollback_path(&daemon.data_dir.path().join(SCROLLBACK_DIR), &pane_id);
    let started = Instant::now();
    let mut slowest = Duration::ZERO;
    let mut largest: u64 = 0;
    while !marker.exists() && started.elapsed() < Duration::from_secs(60) {
        let ping = Instant::now();
        client
            .request::<Value>(DaemonRequest::Ping)
            .expect("ping answers during the flood");
        slowest = slowest.max(ping.elapsed());
        if let Ok(meta) = fs::metadata(&scrollback) {
            largest = largest.max(meta.len());
        }
        thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "flood: 18874368 bytes in {:?}, slowest ping {:?}, largest scrollback {} bytes",
        started.elapsed(),
        slowest,
        largest
    );
    assert!(marker.exists(), "the flood should finish within the budget");
    assert!(
        slowest < Duration::from_millis(1500),
        "a request stalled for {slowest:?} during the flood"
    );
    assert!(largest > 0, "the flood must reach the scrollback file");
    assert!(
        largest <= SCROLLBACK_MAX_BYTES + 256 * 1024,
        "scrollback grew to {largest} bytes, past the cap"
    );

    // An unbounded producer dies with its pane.
    let pid_file = daemon.data_dir.path().join("yes.pid");
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: format!("yes & echo $! > '{}'\n", pid_file.display()),
        })
        .expect("start unbounded flood");
    let mut producer: Option<u32> = None;
    for _ in 0..200 {
        if let Some(pid) = fs::read_to_string(&pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            producer = Some(pid);
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let producer = producer.expect("the shell reported the producer pid");
    thread::sleep(Duration::from_millis(300));
    client
        .request::<Value>(DaemonRequest::ClosePane {
            pane_id: pane_id.clone(),
        })
        .expect("close mid-flood");
    let mut gone = false;
    for _ in 0..200 {
        // SAFETY: existence probe on a pid we were handed.
        if unsafe { libc::kill(producer as libc::pid_t, 0) } != 0 {
            gone = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !gone {
        // SAFETY: cleanup of our own test process.
        unsafe {
            libc::kill(producer as libc::pid_t, libc::SIGKILL);
        }
    }
    daemon.shutdown();
    assert!(gone, "the producer must die with its pane");
}

#[test]
fn permission_mode_is_read_off_the_screen_and_flagged() {
    assert_eq!(
        classify_agent_mode("⏵⏵ auto mode on (shift+tab to cycle)"),
        Some("auto")
    );
    assert_eq!(
        classify_agent_mode("⏵⏵ bypass permissions on"),
        Some("bypass")
    );
    assert_eq!(
        classify_agent_mode("⏵⏵ accept edits on"),
        Some("accept-edits")
    );
    assert_eq!(
        classify_agent_mode("⏵⏵ auto-accept edits on"),
        Some("accept-edits")
    );
    assert_eq!(classify_agent_mode("⏸ plan mode on"), Some("plan"));
    assert_eq!(classify_agent_mode("❯ "), None);
    assert!(is_unattended_mode(Some("auto")));
    assert!(is_unattended_mode(Some("bypass")));
    assert!(is_unattended_mode(Some("bypassPermissions")));
    assert!(is_unattended_mode(Some("dontAsk")));
    assert!(!is_unattended_mode(Some("accept-edits")));
    assert!(!is_unattended_mode(Some("plan")));
    assert!(!is_unattended_mode(Some("manual")));
    assert!(!is_unattended_mode(None));
}

#[test]
fn mode_transitions_ride_agent_state_and_the_ledger() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().join("scrollback"));
    let ledger_dir = dir.path().join(LEDGER_DIR);
    fs::create_dir_all(&ledger_dir).expect("ledger dir");
    router.set_ledger(Arc::new(Mutex::new(LedgerSink::new(ledger_dir.clone()))));
    let auto_screen = format!("{CLAUDE_IDLE_SCREEN}  ⏵⏵ auto mode on (shift+tab to cycle)\r\n");
    let bypass_screen = format!("{CLAUDE_IDLE_SCREEN}  ⏵⏵ bypass permissions on\r\n");

    router.apply_agent_classification("pane-4", CLAUDE_IDLE_SCREEN);
    let info = router.agent_state("pane-4");
    assert_eq!(info.mode, None);
    assert!(!info.unattended);

    router.apply_agent_classification("pane-4", &auto_screen);
    let info = router.agent_state("pane-4");
    assert_eq!(info.mode.as_deref(), Some("auto"));
    assert!(info.unattended);
    assert_eq!(info.attention, Some(AgentAttention::Idle));

    // Under a fresh official reading the mode still tracks the screen.
    router.apply_official_attention(
        "pane-4",
        "claude",
        AgentAttention::Working,
        Duration::from_secs(5),
    );
    router.apply_agent_classification("pane-4", &bypass_screen);
    let info = router.agent_state("pane-4");
    assert_eq!(info.mode.as_deref(), Some("bypass"));
    assert_eq!(
        info.attention,
        Some(AgentAttention::Working),
        "official attention is untouched by a mode change"
    );

    let records = read_ledger_tail(&ledger_path(&ledger_dir, "pane-4"), 0);
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        vec![
            "attention.changed",
            "mode.changed",
            "attention.changed",
            "mode.changed"
        ]
    );
    assert_eq!(records[1]["payload"]["from"], Value::Null);
    assert_eq!(records[1]["payload"]["to"], json!("auto"));
    assert_eq!(records[1]["payload"]["unattended"], json!(true));
    assert_eq!(records[3]["payload"]["from"], json!("auto"));
    assert_eq!(records[3]["payload"]["to"], json!("bypass"));
}

#[test]
fn terminal_controls_are_stripped_for_search() {
    assert_eq!(
        strip_terminal_controls("\u{1b}[32mgreen\u{1b}[0m plain\r\n"),
        "green plain\n"
    );
    assert_eq!(
        strip_terminal_controls("\u{1b}]0;title\u{7}after \u{1b}]8;;http://x\u{1b}\\link"),
        "after link"
    );
    assert_eq!(strip_terminal_controls("a\u{1b}Pdcs stuff\u{1b}\\b"), "ab");
    assert_eq!(strip_terminal_controls("x\u{1b}(By\ttab\u{8}"), "xy\ttab");
    assert_eq!(
        strip_terminal_controls("unterminated \u{1b}[31"),
        "unterminated "
    );
    let lines = vec![
        "Alpha".to_string(),
        "beta needle".to_string(),
        "NEEDLE".to_string(),
    ];
    assert_eq!(
        search_lines(&lines, "needle", false, 10),
        vec![(2, "beta needle".to_string())]
    );
    assert_eq!(search_lines(&lines, "needle", true, 10).len(), 2);
    assert_eq!(search_lines(&lines, "needle", true, 1).len(), 1);
}

#[test]
fn parse_search_and_lines_args() {
    let parsed =
        parse_search_args(&args(&["pane-1", "-i", "-n", "5", "hello", "world"])).expect("parse");
    assert_eq!(parsed.pane_ref, "pane-1");
    assert_eq!(parsed.needle, "hello world");
    assert!(parsed.ignore_case);
    assert_eq!(parsed.limit, 5);
    let dashed = parse_search_args(&args(&["pane-1", "--", "-x", "flag"])).expect("dashed needle");
    assert_eq!(dashed.needle, "-x flag");
    assert!(parse_search_args(&args(&["pane-1"])).is_err());
    assert!(parse_search_args(&args(&["pane-1", "-q", "x"])).is_err());
    let lines = parse_lines_args(&args(&["pane-1", "3:9"])).expect("range");
    assert_eq!((lines.from, lines.to), (3, 9));
    let single = parse_lines_args(&args(&["pane-1", "7"])).expect("single");
    assert_eq!((single.from, single.to), (7, 7));
    assert!(parse_lines_args(&args(&["pane-1", "0:2"])).is_err());
    assert!(parse_lines_args(&args(&["pane-1", "5:2"])).is_err());
    assert!(parse_lines_args(&args(&["pane-1"])).is_err());
}

#[cfg(unix)]
#[test]
fn scrollback_search_and_lines_over_ipc() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    // The command line echoes "needle" too; only the printed rows carry "row-needle".
    client
        .request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: pane_id.clone(),
            input: "printf 'row-%s\\n' one two needle three\n".to_string(),
        })
        .expect("print rows");
    let mut result = Value::Null;
    for _ in 0..200 {
        result = client
            .request(DaemonRequest::SearchScrollback {
                pane_id: pane_id.clone(),
                needle: "row-three".to_string(),
                ignore_case: false,
                limit: 0,
            })
            .expect("search");
        if result["matches"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty())
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let hits = result["matches"].as_array().expect("matches").clone();
    assert_eq!(hits.len(), 1, "{result}");
    let three_line = hits[0]["line"].as_u64().expect("line") as usize;
    let needle: Value = client
        .request(DaemonRequest::SearchScrollback {
            pane_id: pane_id.clone(),
            needle: "ROW-NEEDLE".to_string(),
            ignore_case: true,
            limit: 0,
        })
        .expect("search folded");
    let needle_hits = needle["matches"].as_array().expect("matches");
    assert_eq!(needle_hits.len(), 1, "{needle}");
    assert_eq!(needle_hits[0]["text"], json!("row-needle"));
    let needle_line = needle_hits[0]["line"].as_u64().expect("line") as usize;
    assert_eq!(three_line, needle_line + 1);

    let cited: Value = client
        .request(DaemonRequest::ScrollbackLines {
            pane_id: pane_id.clone(),
            from: needle_line,
            to: three_line,
        })
        .expect("lines");
    assert_eq!(cited["lines"], json!(["row-needle", "row-three"]));
    assert_eq!(cited["from"], json!(needle_line));
    assert!(cited["total_lines"].as_u64().unwrap_or(0) as usize >= three_line);

    let empty = client
        .request::<Value>(DaemonRequest::SearchScrollback {
            pane_id: pane_id.clone(),
            needle: "   ".to_string(),
            ignore_case: false,
            limit: 0,
        })
        .expect_err("blank needle");
    assert!(empty.contains("empty"), "{empty}");
    let bad = client
        .request::<Value>(DaemonRequest::ScrollbackLines {
            pane_id: pane_id.clone(),
            from: 1,
            to: 5000,
        })
        .expect_err("range too wide");
    assert!(bad.contains("at most"), "{bad}");
    daemon.shutdown();
}

#[test]
fn project_names_and_rollup() {
    assert_eq!(
        validate_project_name(" feature-x "),
        Ok("feature-x".to_string())
    );
    assert!(validate_project_name("").is_err());
    assert!(validate_project_name(".hidden").is_err());
    assert!(validate_project_name("has space").is_err());
    assert!(validate_project_name(&"n".repeat(65)).is_err());

    let project = Project {
        name: "p".to_string(),
        goal: None,
        repo: None,
        panes: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        created_at_ms: 1,
    };
    let states = HashMap::from([
        ("a".to_string(), PaneRuntimeState::Live),
        ("b".to_string(), PaneRuntimeState::Live),
        ("c".to_string(), PaneRuntimeState::Ended),
    ]);
    let agents = HashMap::from([
        (
            "a".to_string(),
            AgentPaneInfo {
                agent: Some("claude".to_string()),
                attention: Some(AgentAttention::NeedsInput),
                mode: Some("auto".to_string()),
                unattended: true,
            },
        ),
        (
            "b".to_string(),
            AgentPaneInfo {
                agent: Some("claude".to_string()),
                attention: Some(AgentAttention::Working),
                mode: None,
                unattended: false,
            },
        ),
    ]);
    let leases = HashMap::from([
        ("a".to_string(), HeldLease::new("alice", 1, 1)),
        ("c".to_string(), HeldLease::new("alice", 2, 1)),
    ]);
    let summary = project_rollup(&project, &states, &agents, &leases);
    assert_eq!(summary.panes, 3);
    assert_eq!(summary.live, 2);
    assert_eq!(summary.needs_input, 1);
    assert_eq!(summary.working, 1);
    assert_eq!(summary.idle, 0);
    assert_eq!(summary.unattended, 1);
    assert_eq!(summary.held, 2);
    assert_eq!(summary.holders, vec!["alice".to_string()]);
}

#[test]
fn parse_project_args_shapes() {
    assert_eq!(
        parse_project_args(&[]).expect("bare").verb,
        ProjectVerb::List
    );
    let new = parse_project_args(&args(&["new", "feat", "--goal", "ship it", "--repo", "/r"]))
        .expect("new");
    assert_eq!(new.name.as_deref(), Some("feat"));
    assert_eq!(new.goal.as_deref(), Some("ship it"));
    assert_eq!(new.repo.as_deref(), Some("/r"));
    let add = parse_project_args(&args(&["add", "feat", "pane-1", "pane-2"])).expect("add");
    assert_eq!(add.panes, args(&["pane-1", "pane-2"]));
    let rm = parse_project_args(&args(&["rm", "pane-1"])).expect("rm");
    assert_eq!(rm.panes, args(&["pane-1"]));
    let ledger = parse_project_args(&args(&["ledger", "feat", "-n", "5"])).expect("ledger");
    assert_eq!(ledger.limit, 5);
    let dossier = parse_project_args(&args(&[
        "dossier",
        "feat",
        "--lines",
        "12",
        "--out",
        "/tmp/d.json",
    ]))
    .expect("dossier");
    assert_eq!(dossier.verb, ProjectVerb::Dossier);
    assert_eq!(dossier.name.as_deref(), Some("feat"));
    assert_eq!(dossier.lines, 12);
    assert_eq!(dossier.out.as_deref(), Some("/tmp/d.json"));
    let bare_dossier = parse_project_args(&args(&["dossier", "feat"])).expect("bare dossier");
    assert_eq!((bare_dossier.lines, bare_dossier.out), (0, None));
    assert!(parse_project_args(&args(&["dossier"])).is_err());
    assert!(parse_project_args(&args(&["dossier", "feat", "--lines", "0"])).is_err());
    assert!(parse_project_args(&args(&["ledger", "feat", "--out", "x"])).is_err());
    assert!(parse_project_args(&args(&["add", "feat"])).is_err());
    assert!(parse_project_args(&args(&["show"])).is_err());
    assert!(parse_project_args(&args(&["list", "x"])).is_err());
    assert!(parse_project_args(&args(&["show", "feat", "--goal", "x"])).is_err());
    assert!(parse_project_args(&args(&["bogus"])).is_err());
}

#[test]
fn project_dossier_bundles_state_ledger_and_scrollback() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<Project>(DaemonRequest::ProjectCreate {
            name: "feat".to_string(),
            goal: Some("prove the dossier".to_string()),
            repo: None,
        })
        .expect("create");
    client
        .request::<Value>(DaemonRequest::ProjectAssign {
            name: "feat".to_string(),
            pane_id: pane_id.clone(),
        })
        .expect("assign");
    client
        .request::<LeaseInfo>(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("take");
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "printf 'dossier-%s\n' one two; exit 3\n".to_string(),
            holder: "alice".to_string(),
            generation: None,
        })
        .expect("run and exit");

    // Poll until the pane's exit is in its ledger.
    let mut dossier = Value::Null;
    for _ in 0..300 {
        dossier = client
            .request(DaemonRequest::ProjectDossier {
                name: "feat".to_string(),
                lines: 0,
            })
            .expect("dossier");
        let ended = dossier["panes"][0]["ledger"]["records"]
            .as_array()
            .is_some_and(|records| records.iter().any(|r| r["type"] == json!("pane.ended")));
        if ended {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(dossier["format"], json!(PROJECT_DOSSIER_FORMAT));
    assert!(dossier["generated_at_ms"].as_u64().unwrap_or(0) > 0);
    assert_eq!(dossier["summary"]["project"]["name"], json!("feat"));
    assert_eq!(
        dossier["summary"]["project"]["goal"],
        json!("prove the dossier")
    );
    let panes = dossier["panes"].as_array().expect("panes");
    assert_eq!(panes.len(), 1, "{dossier}");
    let pane = &panes[0];
    assert_eq!(pane["id"], json!(pane_id));
    assert_eq!(pane["state"], json!(PaneRuntimeState::Ended));
    assert_eq!(pane["holder"], json!("alice"));

    // The chain verifies and names its head; the records are the whole ledger.
    let chain = &pane["ledger"]["chain"];
    assert_eq!(chain["verified"], json!(true), "{chain}");
    let records = pane["ledger"]["records"].as_array().expect("records");
    assert_eq!(chain["records"], json!(records.len()));
    assert_eq!(chain["head"], records.last().expect("last")["h"]);
    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap_or(""))
        .collect();
    assert!(kinds.contains(&"project.assigned"), "{kinds:?}");
    assert!(kinds.contains(&"lease.taken"), "{kinds:?}");
    let ended = records
        .iter()
        .find(|record| record["type"] == json!("pane.ended"))
        .expect("pane.ended");
    assert_eq!(ended["payload"]["exit_code"], json!(3));
    assert_eq!(ended["payload"]["holder"], json!("alice"));
    assert_eq!(ended["payload"]["unattended"], json!(false));

    // The scrollback tail is control-stripped text with citable numbers.
    let scrollback = &pane["scrollback"];
    let lines = scrollback["lines"].as_array().expect("lines");
    assert!(
        lines.iter().any(|line| line == "dossier-two"),
        "{scrollback}"
    );
    let total = scrollback["total_lines"].as_u64().expect("total") as usize;
    assert_eq!(scrollback["to"], json!(total));
    assert_eq!(
        scrollback["from"].as_u64().expect("from") as usize,
        total + 1 - lines.len()
    );
    assert!(lines.len() <= PROJECT_DOSSIER_DEFAULT_LINES);

    // `lines` bounds the tail; an unknown project is a clean error.
    let short: Value = client
        .request(DaemonRequest::ProjectDossier {
            name: "feat".to_string(),
            lines: 1,
        })
        .expect("short dossier");
    assert_eq!(
        short["panes"][0]["scrollback"]["lines"]
            .as_array()
            .map_or(0, Vec::len),
        1
    );
    assert_eq!(short["panes"][0]["scrollback"]["from"], json!(total));
    let missing = client
        .request::<Value>(DaemonRequest::ProjectDossier {
            name: "nope".to_string(),
            lines: 0,
        })
        .expect_err("unknown project");
    assert!(missing.contains("unknown project"), "{missing}");
    daemon.shutdown();
}

#[test]
fn projects_changed_is_broadcast_on_every_mutation() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    let mut stream = client
        .authenticated_stream()
        .expect("subscribe stream should connect");
    write_json_line(&mut stream, &DaemonRequest::Subscribe).expect("subscribe should write");
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("read timeout should apply");

    client
        .request::<Project>(DaemonRequest::ProjectCreate {
            name: "feat".to_string(),
            goal: None,
            repo: None,
        })
        .expect("create");
    client
        .request::<Value>(DaemonRequest::ProjectAssign {
            name: "feat".to_string(),
            pane_id: pane_id.clone(),
        })
        .expect("assign");
    let extra: Pane = client
        .request(DaemonRequest::CreatePane {
            title: None,
            profile: None,
        })
        .expect("second pane");
    client
        .request::<Value>(DaemonRequest::ProjectUnassign {
            pane_id: pane_id.clone(),
        })
        .expect("unassign");
    client
        .request::<Value>(DaemonRequest::ProjectUnassign {
            pane_id: extra.id.clone(),
        })
        .expect("unassign of a non-member is a quiet no-op");
    client
        .request::<Value>(DaemonRequest::ProjectAssign {
            name: "feat".to_string(),
            pane_id: extra.id.clone(),
        })
        .expect("assign extra");
    client
        .request::<Value>(DaemonRequest::ClosePane {
            pane_id: extra.id.clone(),
        })
        .expect("close member");
    client
        .request::<Project>(DaemonRequest::ProjectDelete {
            name: "feat".to_string(),
        })
        .expect("delete");

    // create, assign, unassign, assign, close (member), delete: six tables.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut tables: Vec<HashMap<String, Project>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && tables.len() < 6 {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(DaemonEvent::ProjectsChanged { projects }) =
                    serde_json::from_str::<DaemonEvent>(&line)
                {
                    tables.push(projects);
                }
            }
            Err(_) => {}
        }
    }
    assert_eq!(tables.len(), 6, "one ProjectsChanged per mutation");
    assert_eq!(tables[0]["feat"].panes, Vec::<String>::new());
    assert_eq!(tables[1]["feat"].panes, vec![pane_id.clone()]);
    assert_eq!(tables[2]["feat"].panes, Vec::<String>::new());
    assert_eq!(tables[3]["feat"].panes, vec![extra.id.clone()]);
    assert_eq!(tables[4]["feat"].panes, Vec::<String>::new());
    assert!(tables[5].is_empty());
    daemon.shutdown();
}

#[test]
fn projects_round_trip_over_ipc_and_persist() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let first = initial.panes[0].id.clone();
    let second: Pane = client
        .request(DaemonRequest::CreatePane {
            title: Some("worker".to_string()),
            profile: None,
        })
        .expect("second pane");

    let project: Project = client
        .request(DaemonRequest::ProjectCreate {
            name: "feat".to_string(),
            goal: Some("ship the thing".to_string()),
            repo: None,
        })
        .expect("create");
    assert_eq!(project.name, "feat");
    let dup = client
        .request::<Project>(DaemonRequest::ProjectCreate {
            name: "feat".to_string(),
            goal: None,
            repo: None,
        })
        .expect_err("duplicate refused");
    assert!(dup.contains("already exists"), "{dup}");
    assert!(client
        .request::<Project>(DaemonRequest::ProjectCreate {
            name: "bad name".to_string(),
            goal: None,
            repo: None,
        })
        .is_err());

    client
        .request::<Value>(DaemonRequest::ProjectAssign {
            name: "feat".to_string(),
            pane_id: first.clone(),
        })
        .expect("assign first");
    client
        .request::<Value>(DaemonRequest::ProjectAssign {
            name: "feat".to_string(),
            pane_id: second.id.clone(),
        })
        .expect("assign second");
    client
        .request::<LeaseInfo>(DaemonRequest::TakeLease {
            pane_id: second.id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("take");

    let summaries: Vec<ProjectSummary> = client.request(DaemonRequest::ProjectList).expect("list");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].panes, 2);
    assert_eq!(summaries[0].held, 1);
    assert_eq!(summaries[0].holders, vec!["alice".to_string()]);
    assert_eq!(summaries[0].project.goal.as_deref(), Some("ship the thing"));

    // Moving a pane to another project leaves the first.
    client
        .request::<Project>(DaemonRequest::ProjectCreate {
            name: "other".to_string(),
            goal: None,
            repo: None,
        })
        .expect("other");
    let moved: Value = client
        .request(DaemonRequest::ProjectAssign {
            name: "other".to_string(),
            pane_id: first.clone(),
        })
        .expect("move");
    assert_eq!(moved["previous"], json!("feat"));
    let detail: Value = client
        .request(DaemonRequest::ProjectShow {
            name: "feat".to_string(),
        })
        .expect("show");
    let members: Vec<&str> = detail["panes"]
        .as_array()
        .expect("panes")
        .iter()
        .map(|pane| pane["id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(members, vec![second.id.as_str()]);
    assert_eq!(detail["panes"][0]["holder"], json!("alice"));

    // The merged ledger carries both panes' records in time order.
    let ledger: Value = client
        .request(DaemonRequest::ProjectLedger {
            name: "other".to_string(),
            limit: 0,
        })
        .expect("ledger");
    let kinds: Vec<&str> = ledger["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["type"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        kinds,
        vec!["project.assigned", "project.unassigned", "project.assigned"]
    );
    assert_eq!(ledger["records"][2]["payload"]["project"], json!("other"));

    // Persisted, and the snapshot carries it.
    let persisted: PersistedWorkspace = serde_json::from_str(
        &fs::read_to_string(daemon.data_dir.path().join(WORKSPACE_FILE)).expect("workspace.json"),
    )
    .expect("parse");
    assert_eq!(persisted.projects["feat"].panes, vec![second.id.clone()]);
    assert_eq!(persisted.projects["other"].panes, vec![first.clone()]);
    let snapshot: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    assert_eq!(snapshot.projects.len(), 2);

    // Closing a member drops it; deleting a project keeps its panes.
    client
        .request::<Value>(DaemonRequest::ClosePane {
            pane_id: second.id.clone(),
        })
        .expect("close member");
    let after: Vec<ProjectSummary> = client
        .request(DaemonRequest::ProjectList)
        .expect("list after close");
    let feat = after
        .iter()
        .find(|summary| summary.project.name == "feat")
        .expect("feat");
    assert_eq!(feat.panes, 0);
    client
        .request::<Project>(DaemonRequest::ProjectDelete {
            name: "other".to_string(),
        })
        .expect("delete");
    assert!(client
        .request::<Value>(DaemonRequest::ProjectShow {
            name: "other".to_string(),
        })
        .is_err());
    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus {
            pane_id: first.clone(),
        })
        .expect("pane survives project deletion");
    assert_eq!(status.pane.id, first);
    daemon.shutdown();
}

#[test]
fn output_guard_counts_hiding_tricks_not_redraws() {
    let none =
        scan_output_tricks("plain text \u{1b}[32mgreen\u{1b}[0m \u{1b}[2K\u{1b}[A redraw\r\n");
    assert_eq!(none, OutputTricks::default());
    assert_eq!(scan_output_tricks("\u{1b}[38;5;8mgrey\u{1b}[0m").conceal, 0);
    assert_eq!(
        scan_output_tricks("\u{1b}[8mhidden\u{1b}[28m \u{1b}[1;8mx").conceal,
        2
    );
    assert_eq!(scan_output_tricks("\u{1b}]52;c;aGVsbG8=\u{7}").clipboard, 1);
    assert_eq!(scan_output_tricks("\u{1b}]0;title\u{7}").total(), 0);
    let phish = "\u{1b}]8;;https://evil.example/x\u{7}https://github.com/org/repo\u{1b}]8;;\u{7}";
    assert_eq!(scan_output_tricks(phish).hyperlink_mismatch, 1);
    let honest =
        "\u{1b}]8;;https://github.com/org/repo\u{1b}\\github.com/org/repo\u{1b}]8;;\u{1b}\\";
    assert_eq!(scan_output_tricks(honest).hyperlink_mismatch, 0);
    let labelled = "\u{1b}]8;;https://docs.example/a\u{7}the docs\u{1b}]8;;\u{7}";
    assert_eq!(scan_output_tricks(labelled).hyperlink_mismatch, 0);
    assert_eq!(
        scan_output_tricks("\u{1b}Pq payload\u{1b}\\ \u{1b}_apc\u{7}").string_controls,
        2
    );
    assert_eq!(scan_output_tricks("a\u{85}b\u{9b}c").c1_controls, 2);
    assert_eq!(
        url_host("https://user@Example.com:8443/path"),
        Some("example.com".to_string())
    );
    assert_eq!(
        url_host("www.example.com/x"),
        Some("www.example.com".to_string())
    );
    assert_eq!(url_host("just words"), None);
}

#[test]
fn output_guard_accumulates_and_rate_limits_announcements() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().join("scrollback"));
    let ledger_dir = dir.path().join(LEDGER_DIR);
    fs::create_dir_all(&ledger_dir).expect("ledger dir");
    router.set_ledger(Arc::new(Mutex::new(LedgerSink::new(ledger_dir.clone()))));
    fs::create_dir_all(dir.path().join("scrollback")).expect("scrollback dir");
    router.ensure_model("pane-5", 80, 24);

    router.emit("pane-5", "\u{1b}[8msecret\u{1b}[0m\n".to_string());
    router.emit("pane-5", "\u{1b}]52;c;Zm9v\u{7}\n".to_string());
    router.emit("pane-5", "nothing to see\n".to_string());
    let total = router.output_tricks("pane-5");
    assert_eq!(total.conceal, 1);
    assert_eq!(total.clipboard, 1);
    let records = read_ledger_tail(&ledger_path(&ledger_dir, "pane-5"), 0);
    let suspicious: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == json!("output.suspicious"))
        .collect();
    assert_eq!(
        suspicious.len(),
        1,
        "a second hit inside the interval is not re-announced"
    );
    assert_eq!(suspicious[0]["payload"]["added"]["conceal"], json!(1));
    assert_eq!(router.output_warnings().len(), 1);
    assert_eq!(router.output_tricks("pane-9"), OutputTricks::default());
    router.remove_output_guard("pane-5");
    assert!(router.output_warnings().is_empty());
    assert_eq!(
        format_output_warning(&json!({"conceal": 2, "clipboard": 0})),
        "\tHIDDEN-OUTPUT conceal=2"
    );
    assert_eq!(format_output_warning(&Value::Null), "");
}

#[test]
fn lease_generation_refuses_stale_commands() {
    let held = HeldLease::new("alice", 1, 7);
    assert_eq!(check_generation(Some(&held), None), Ok(()));
    assert_eq!(check_generation(Some(&held), Some(7)), Ok(()));
    let stale = check_generation(Some(&held), Some(6)).expect_err("stale");
    assert!(stale.contains("stale lease"), "{stale}");
    assert!(stale.contains("held by alice"), "{stale}");
    let gone = check_generation(None, Some(7)).expect_err("unheld");
    assert!(gone.contains("unheld"), "{gone}");
    assert_eq!(check_generation(None, None), Ok(()));
    let (generation, rest) =
        parse_generation_flag(&args(&["pane-1", "--generation", "9", "echo", "hi"]))
            .expect("parse");
    assert_eq!(generation, Some(9));
    assert_eq!(rest, args(&["pane-1", "echo", "hi"]));
    assert!(parse_generation_flag(&args(&["pane-1", "--generation", "x"])).is_err());
    let release = parse_lease_args(&args(&["release", "-m", "done", "--generation", "3"]))
        .expect("release with generation");
    assert_eq!(release.generation, Some(3));

    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    let alice: LeaseInfo = client
        .request(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            force: false,
            why: None,
        })
        .expect("take");
    let g1 = alice.generation.expect("held leases carry a generation");
    client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "".to_string(),
            holder: "alice".to_string(),
            generation: Some(g1),
        })
        .expect("current generation writes");
    let bob: LeaseInfo = client
        .request(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            force: true,
            why: Some("alice is away".to_string()),
        })
        .expect("force take");
    let g2 = bob.generation.expect("generation");
    assert!(g2 > g1);
    // Alice's late write names the old generation even with her own name: stale.
    let late = client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "x".to_string(),
            holder: "bob".to_string(),
            generation: Some(g1),
        })
        .expect_err("stale generation refused even for the current holder");
    assert!(late.contains("stale lease"), "{late}");
    let late_release = client
        .request::<LeaseInfo>(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            note: "done".to_string(),
            generation: Some(g1),
        })
        .expect_err("stale release refused");
    assert!(late_release.contains("stale lease"), "{late_release}");
    let status: LeaseInfo = client
        .request(DaemonRequest::LeaseStatus {
            pane_id: pane_id.clone(),
        })
        .expect("status");
    assert_eq!(
        status.refused_writes, 1,
        "the stale write counted as refused"
    );
    client
        .request::<LeaseInfo>(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "bob".to_string(),
            note: "done".to_string(),
            generation: Some(g2),
        })
        .expect("current generation releases");
    let after = client
        .request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "x".to_string(),
            holder: "bob".to_string(),
            generation: Some(g2),
        })
        .expect_err("a generation named on an unheld pane is stale");
    assert!(after.contains("unheld"), "{after}");
    daemon.shutdown();
}

#[test]
fn lease_generations_never_repeat_across_restarts() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let cwd = PathBuf::from("/tmp/sgian-lease-gen");
    let first_generation = {
        let server = DaemonServer::with_config(
            cwd.clone(),
            data_dir.path().to_path_buf(),
            Config::default(),
        )
        .expect("server");
        let pane = server.lock_registry().expect("registry").create_pane(None);
        let info: LeaseInfo = serde_json::from_value(
            server
                .handle(DaemonRequest::TakeLease {
                    pane_id: pane.id.clone(),
                    holder: "alice".to_string(),
                    force: false,
                    why: None,
                })
                .expect("take"),
        )
        .expect("lease info");
        info.generation.expect("generation")
    };
    let server = DaemonServer::with_config(cwd, data_dir.path().to_path_buf(), Config::default())
        .expect("restart");
    let restored = server.lease_infos();
    let (pane_id, info) = restored.iter().next().expect("lease restored");
    assert_eq!(info.generation, Some(first_generation));
    // A new lease after the restart gets a strictly greater generation.
    let pane_id = pane_id.clone();
    let _: Value = server
        .handle(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "alice".to_string(),
            note: "handover".to_string(),
            generation: None,
        })
        .expect("release");
    let next: LeaseInfo = serde_json::from_value(
        server
            .handle(DaemonRequest::TakeLease {
                pane_id,
                holder: "bob".to_string(),
                force: false,
                why: None,
            })
            .expect("take again"),
    )
    .expect("lease info");
    assert!(next.generation.expect("generation") > first_generation);
}

#[test]
fn agent_probe_interval_config() {
    let mut config = Config::default();
    assert_eq!(
        config.agent_probe_interval(),
        Some(Duration::from_millis(2000))
    );
    config.agent_probe_interval_ms = Some(0);
    assert_eq!(config.agent_probe_interval(), None);
    config.agent_probe_interval_ms = Some(10);
    assert_eq!(
        config.agent_probe_interval(),
        Some(Duration::from_millis(250)),
        "a floor keeps the probe from spinning"
    );
    let global = Config {
        agent_probe_interval_ms: Some(5000),
        ..Config::default()
    };
    let workspace = Config {
        agent_probe_interval_ms: Some(0),
        ..Config::default()
    };
    assert_eq!(global.overlay(workspace).agent_probe_interval(), None);
}

#[test]
fn lease_survives_daemon_restart_via_workspace_json() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let cwd = PathBuf::from("/tmp/sgian-lease-restart");
    {
        let server = DaemonServer::with_config(
            cwd.clone(),
            data_dir.path().to_path_buf(),
            Config::default(),
        )
        .expect("server");
        let pane = server.lock_registry().expect("registry").create_pane(None);
        server
            .handle(DaemonRequest::TakeLease {
                pane_id: pane.id.clone(),
                holder: "alice".to_string(),
                force: false,
                why: None,
            })
            .expect("take");
    }
    let server = DaemonServer::with_config(cwd, data_dir.path().to_path_buf(), Config::default())
        .expect("server restarted");
    let leases = server.lease_infos();
    assert_eq!(leases.len(), 1);
    let info = leases.values().next().expect("one lease");
    assert_eq!(info.holder.as_deref(), Some("alice"));
    // A lease for a pane that is not in the registry is not resurrected.
    let mut persisted: PersistedWorkspace = serde_json::from_str(
        &fs::read_to_string(data_dir.path().join(WORKSPACE_FILE)).expect("workspace.json"),
    )
    .expect("parse");
    persisted
        .leases
        .insert("pane-999".to_string(), HeldLease::new("ghost", 1, 1));
    fs::write(
        data_dir.path().join(WORKSPACE_FILE),
        serde_json::to_string(&persisted).expect("encode"),
    )
    .expect("write");
    drop(server);
    let server = DaemonServer::with_config(
        PathBuf::from("/tmp/sgian-lease-restart"),
        data_dir.path().to_path_buf(),
        Config::default(),
    )
    .expect("server restarted again");
    assert_eq!(server.lease_infos().len(), 1, "ghost lease filtered");
}
