use super::*;

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
pub(crate) fn create_and_end_pane(client: &DaemonClient) -> String {
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

/// check_persisted_cwd refuses a corrupt workspace.json with no cwd marker.
#[test]
fn check_persisted_cwd_corrupt_file_without_marker_refused() {
    let dir = tempfile::tempdir().expect("temp dir");
    fs::write(dir.path().join(WORKSPACE_FILE), "not valid json").expect("write corrupt file");

    let refused = check_persisted_cwd(&PathBuf::from("/tmp/any-cwd"), dir.path())
        .expect_err("corrupt persist without a marker");
    assert!(refused.contains("unparseable"), "{refused}");
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

#[test]
fn corrupt_workspace_refusal_preserves_the_original_marker_and_state() {
    for corrupt in [b"{not json".as_slice(), b"\xff\xfe".as_slice()] {
        let dir = tempfile::tempdir().expect("temp dir");
        let owner = dir.path().join("owner");
        let other = dir.path().join("other");
        let data = dir.path().join("data");
        fs::create_dir_all(&data).expect("data dir");
        fs::write(data.join(WORKSPACE_FILE), corrupt).expect("corrupt workspace");
        assert!(
            check_persisted_cwd(&other, &data).is_err(),
            "missing marker must refuse"
        );
        assert!(DaemonServer::with_config(other.clone(), data.clone(), Config::default()).is_err());
        assert!(
            !data.join(WORKSPACE_CWD_FILE).exists(),
            "refusal must not write a marker"
        );
        write_workspace_cwd_marker(&data, &owner);
        let marker = fs::read(data.join(WORKSPACE_CWD_FILE)).expect("original marker");
        assert!(
            check_persisted_cwd(&owner, &data).is_ok(),
            "matching marker can recover"
        );
        assert!(
            check_persisted_cwd(&other, &data).is_err(),
            "foreign marker must refuse"
        );
        assert!(DaemonServer::with_config(other.clone(), data.clone(), Config::default()).is_err());
        assert_eq!(
            fs::read(data.join(WORKSPACE_CWD_FILE)).expect("marker"),
            marker
        );
        assert_eq!(
            fs::read(data.join(WORKSPACE_FILE)).expect("workspace"),
            corrupt
        );
    }
}
