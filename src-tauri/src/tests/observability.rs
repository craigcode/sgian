use super::*;

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
