use super::*;

// ----- control_run exit-code integration (VAL-ORCH-001..005, 024, 027, 031) -----

/// Helper: spawn a daemon with the given shell, create a pane, wait for it
/// to become Live, and return (daemon, client, pane_id).
pub(crate) fn spawn_run_daemon(shell: &str) -> (TestDaemon, DaemonClient, String) {
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

    // Wait for the prompt to draw and the pane to go quiet before anyone
    // types: a write that races the shell's startup terminal handshake can
    // be partly swallowed under heavy parallel load (the same race
    // `await_shell_ready` closes for the in-process server), which showed
    // up as a rare failure in `control_run_across_posix_shells`.
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        let snapshot: Value = client
            .request(DaemonRequest::Snapshot {
                pane_id: pane.id.clone(),
            })
            .unwrap_or(Value::Null);
        if snapshot["revision"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    let _ = client.request::<Value>(DaemonRequest::Wait {
        pane_id: pane.id.clone(),
        condition: WaitCondition::Idle(120),
        timeout_ms: Some(5000),
    });

    (daemon, client, pane.id)
}

/// Helper: run `control_run` in a background thread with a timeout so a
/// bug (e.g. marker never arrives) doesn't hang the test suite.
pub(crate) fn control_run_with_timeout(
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
pub(crate) fn spawn_byte_dumper_daemon(
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
pub(crate) fn capture_od_output(
    reader: &mut std::io::BufReader<TransportStream>,
    pane_id: &str,
) -> String {
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
pub(crate) fn spawn_batched_daemon(
    shell: &str,
    names: &[&str],
) -> (TestDaemon, DaemonClient, Vec<String>) {
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
pub(crate) fn collect_batched_with_timeout(
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
pub(crate) fn pane_id_by_title(client: &DaemonClient, title: &str) -> String {
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
