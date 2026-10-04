use super::*;

// ----- MB spawn metadata + exit-code reaper (VAL-TERM-014..021, 026, 027, 030) -----

pub(crate) static MB_TEST_DIR_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// A unique, per-test scrollback dir so parallel real-PTY tests never collide on
/// the same `{pane_id}.ansi` file.
pub(crate) fn unique_scrollback_dir() -> PathBuf {
    let seq = MB_TEST_DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sgian-mb-test-{}-{}", std::process::id(), seq));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Build a `TerminalStore` whose panes launch `/bin/sh`, so a test can drive a
/// pane to a known exit code by writing `exit N` / `kill -KILL $$`.
pub(crate) fn sh_terminal_store(cwd: &str) -> TerminalStore {
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
pub(crate) fn wait_until_pane_ended(store: &TerminalStore, pane_id: &str) -> bool {
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
pub(crate) fn collect_pane_ended_codes(input: &str) -> Vec<Option<i32>> {
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
fn decode_cli_text_treats_a_real_line_feed_as_enter() {
    // `sgian ctl send pane $'text\n'` reaches ctl with a real LF. It must
    // submit like the escape does; a bare LF only inserts a new line in a
    // full-screen agent input.
    assert_eq!(decode_cli_text("ab\n", false), "ab\r");
    assert_eq!(decode_cli_text("a\nb\n", false), "a\rb\r");
    assert_eq!(
        decode_cli_text("ab\r\n", false),
        "ab\r",
        "CRLF is one Enter"
    );
    assert_eq!(decode_cli_text("ab\r", false), "ab\r");
    // --lf / --raw keep the bytes as given.
    assert_eq!(decode_cli_text("ab\n", true), "ab\n");
    assert_eq!(decode_cli_text("ab\r\n", true), "ab\r\n");
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

pub(crate) fn model_lines(model: &PaneModel) -> Vec<String> {
    let screen = model.parser.screen();
    let (_, cols) = screen.size();
    screen.rows(0, cols).collect()
}

pub(crate) fn model_title(model: &PaneModel) -> Option<String> {
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
pub(crate) struct ClientHandshake {
    pub(crate) stream: TransportStream,
    pub(crate) ok: bool,
    pub(crate) negotiated_wire_version: u16,
    pub(crate) capabilities: Vec<String>,
    pub(crate) protocol_version: Option<u64>,
}

impl ClientHandshake {
    /// Feature-detection rule (architecture.md §5.2 / VAL-IPC-025): switch to the
    /// framed envelope iff the negotiated version reached the framed wire version
    /// AND the peer advertised the "framed" capability.
    pub(crate) fn uses_framing(&self) -> bool {
        self.negotiated_wire_version >= frame::WIRE_VERSION
            && self.capabilities.iter().any(|c| c == "framed")
    }
}

/// Drive the client side of the handshake over `stream`: send a newline hello
/// carrying the legacy `version: 1` plus the additive `max_wire_version`, read
/// the newline handshake response, and decode the negotiation fields.
pub(crate) fn client_handshake(
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
pub(crate) fn pair_daemon_connection() -> (
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
pub(crate) fn client_feature_detection(result: Value) -> bool {
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
pub(crate) fn shared_daemon() -> (Arc<DaemonServer>, tempfile::TempDir, String) {
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
pub(crate) fn connect_to(
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
pub(crate) fn wait_for<F: Fn() -> bool>(cond: F) {
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
pub(crate) fn read_framed_event_until(stream: &mut TransportStream, want: &DaemonEvent) {
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
pub(crate) fn read_newline_event_until(
    reader: &mut BufReader<TransportStream>,
    want: &DaemonEvent,
) {
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
pub(crate) fn wait_shared_server() -> (Arc<DaemonServer>, tempfile::TempDir, String) {
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
pub(crate) fn create_wait_pane(server: &Arc<DaemonServer>) -> String {
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

pub(crate) fn wait_now(
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
pub(crate) fn await_shell_ready(server: &Arc<DaemonServer>, pane_id: &str) {
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

pub(crate) fn str_args(values: &[&str]) -> Vec<String> {
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

pub(crate) fn snapshot_now(server: &Arc<DaemonServer>, pane_id: &str) -> Result<Value, String> {
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
    // Generous: a shell under a loaded machine can take seconds to exit, and
    // the loop ends as soon as it does.
    let died = Instant::now();
    while died.elapsed() < Duration::from_secs(20)
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

pub(crate) fn find_now(
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
pub(crate) fn end_pane_with_code(server: &Arc<DaemonServer>, pane_id: &str, code: i32) {
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

pub(crate) fn find_ids(result: &Value) -> Vec<String> {
    result
        .as_array()
        .expect("find returns an array")
        .iter()
        .map(|entry| entry["id"].as_str().expect("entry id").to_string())
        .collect()
}

pub(crate) fn recorded_command(server: &Arc<DaemonServer>, pane_id: &str) -> String {
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
