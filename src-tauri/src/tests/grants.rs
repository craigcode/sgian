//! Execution grants (docs/design/execution-grants.md): RFC 4231 HMAC-SHA256
//! vectors, CLI parser coverage, end-to-end PTY grant execution, strict
//! `env_allow` filtering (no `SHELL` or `cwd` binary hijack), read-only-to-
//! humans `kranz:<run>` lease default, refusal receipts (`grant.refused`),
//! `kranz bind` key loading/rotation, and non-blocking `grant_wait`/`grant_kill`.

use super::*;

#[test]
fn hmac_sha256_matches_rfc_4231_vectors_and_grant_args_parse() {
    // RFC 4231 Test Case 1: 20 bytes of 0x0b, data = "Hi There"
    let key1 = [0x0bu8; 20];
    assert_eq!(
        hmac_sha256_hex(&key1, b"Hi There"),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );

    // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
    assert_eq!(
        hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );

    // RFC 4231 Test Case 6: 131 bytes of 0xaa (> 64-byte block size)
    let key6 = [0xaau8; 131];
    assert_eq!(
        hmac_sha256_hex(
            &key6,
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        ),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );

    let sample_sig = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";
    assert_eq!(
        normalize_hmac_signature(&format!("  SHA256={}  ", sample_sig.to_ascii_uppercase())),
        Some(sample_sig.to_string())
    );
    assert_eq!(normalize_hmac_signature("short"), None);
    assert_eq!(normalize_hmac_signature(&"z".repeat(64)), None);

    let args = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };
    assert_eq!(
        parse_grant_args(&args("exec --grant {} --signature deadbeef")).expect("exec inline"),
        GrantArgs::Exec {
            grant_json: Some("{}".into()),
            grant_file: None,
            signature: "deadbeef".into(),
            pane_ref: None,
        }
    );
    assert_eq!(
        parse_grant_args(&args("exec --file g.json --sig deadbeef --pane worker"))
            .expect("exec file"),
        GrantArgs::Exec {
            grant_json: None,
            grant_file: Some("g.json".into()),
            signature: "deadbeef".into(),
            pane_ref: Some("worker".into()),
        }
    );
    assert_eq!(
        parse_grant_args(&args("output gt-1")).expect("output"),
        GrantArgs::Output {
            terminal_id: "gt-1".into()
        }
    );
    assert_eq!(
        parse_grant_args(&args("wait gt-1 --timeout 2500")).expect("wait"),
        GrantArgs::Wait {
            terminal_id: "gt-1".into(),
            timeout_ms: Some(2500),
        }
    );
    assert_eq!(
        parse_grant_args(&args("kill gt-1")).expect("kill"),
        GrantArgs::Kill {
            terminal_id: "gt-1".into()
        }
    );
    assert_eq!(
        parse_grant_args(&args("release gt-1")).expect("release"),
        GrantArgs::Release {
            terminal_id: "gt-1".into()
        }
    );

    assert!(parse_grant_args(&[]).is_err());
    assert!(parse_grant_args(&args("exec --signature abc")).is_err());
    assert!(parse_grant_args(&args("exec --grant {} --file g.json --sig abc")).is_err());
    assert!(parse_grant_args(&args("output")).is_err());
    assert!(parse_grant_args(&args("wait --timeout nope gt-1")).is_err());
    assert!(parse_grant_args(&args("unknown gt-1")).is_err());

    for req in [
        DaemonRequest::GrantExec {
            grant: json!({}),
            signature: String::new(),
            pane: None,
        },
        DaemonRequest::GrantOutput {
            terminal_id: "gt-1".into(),
        },
        DaemonRequest::GrantWait {
            terminal_id: "gt-1".into(),
            timeout_ms: None,
        },
        DaemonRequest::GrantKill {
            terminal_id: "gt-1".into(),
        },
        DaemonRequest::GrantRelease {
            terminal_id: "gt-1".into(),
        },
    ] {
        assert_eq!(request_scope(&req), ClientScope::Write);
    }
}

#[cfg(unix)]
fn write_grant_key(repo: &Path, key: &str) -> PathBuf {
    let dir = repo.join(".kranz");
    fs::create_dir_all(&dir).expect("create .kranz");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700 .kranz");
    let path = dir.join("serve.grant.key");
    fs::write(&path, format!("{key}\n")).expect("write grant key");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 0600 key");
    path
}

#[cfg(unix)]
fn sign_grant(key: &str, grant: &Value) -> String {
    hmac_sha256_hex(key.as_bytes(), canonical_json(grant).as_bytes())
}

#[cfg(unix)]
#[test]
fn grant_exec_verifies_spawns_verbatim_filters_env_holds_lease_and_ledgers() {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let sub = workspace.path().join("sub");
    fs::create_dir_all(&sub).expect("create sub");
    let key = "kranz-grant-secret-key-0123456789abcdef";
    write_grant_key(workspace.path(), key);

    // Plant a decoy executable `./env` inside `sub` to verify bare `argv[0]`
    // ("env") is resolved strictly from Sgian's PATH, never from `cwd`.
    let decoy = sub.join("env");
    fs::write(&decoy, "#!/bin/sh\necho HIJACKED_FROM_CWD\n").expect("write decoy");
    fs::set_permissions(&decoy, fs::Permissions::from_mode(0o755)).expect("chmod decoy");

    let mut cfg_env = HashMap::new();
    cfg_env.insert("SGIAN_GRANT_ALLOWED".into(), "allowed-value-42".into());
    cfg_env.insert("SGIAN_GRANT_SECRET".into(), "top-secret-never-leak".into());
    let config = Config {
        env: cfg_env,
        ..Config::default()
    };

    let daemon = TestDaemon::spawn_with_cwd(config, workspace.path().to_path_buf());
    let client = daemon.client();
    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");

    let repo_str = workspace
        .path()
        .canonicalize()
        .expect("canon workspace")
        .display()
        .to_string();
    let cwd_str = sub.canonicalize().expect("canon sub").display().to_string();
    let now = now_millis();

    // 1. Bare `env` with ONLY `SGIAN_GRANT_ALLOWED` in `env_allow` (neither
    //    `PATH` nor `SHELL` nor `SGIAN_GRANT_SECRET` is allowed).
    let grant1 = json!({
        "grant": "g-01-env-filter",
        "mission": "m-3cda6a",
        "run": "r-07",
        "repo": repo_str,
        "cwd": cwd_str,
        "argv": ["env"],
        "env_allow": ["SGIAN_GRANT_ALLOWED"],
        "issued_ms": now,
        "expires_ms": now + 60_000,
    });
    let sig1 = sign_grant(key, &grant1);
    let created1: Value = client
        .request(DaemonRequest::GrantExec {
            grant: grant1,
            signature: sig1,
            pane: None,
        })
        .expect("grant_exec env");
    let term1 = created1["terminal_id"].as_str().expect("term1").to_string();
    let pane1 = created1["pane_id"].as_str().expect("pane1").to_string();

    let waited1: Value = client
        .request(DaemonRequest::GrantWait {
            terminal_id: term1.clone(),
            timeout_ms: Some(5_000),
        })
        .expect("grant_wait 1");
    assert_eq!(waited1["exited"], json!(true), "{waited1}");
    assert_eq!(waited1["exit"]["exit_code"], json!(0), "{waited1}");

    let out1: Value = client
        .request(DaemonRequest::GrantOutput {
            terminal_id: term1.clone(),
        })
        .expect("grant_output 1");
    assert_eq!(out1["exit"]["exit_code"], json!(0));
    let env_text = out1["output"].as_str().expect("output text");
    assert!(
        env_text.contains("SGIAN_GRANT_ALLOWED=allowed-value-42"),
        "allowed env var missing: {env_text:?}"
    );
    assert!(
        !env_text.contains("HIJACKED_FROM_CWD"),
        "bare argv[0] must never resolve from cwd: {env_text:?}"
    );
    assert!(
        !env_text.contains("SGIAN_GRANT_SECRET"),
        "unallowed secret leaked: {env_text:?}"
    );
    assert!(
        !env_text
            .lines()
            .any(|line| line.trim_end_matches('\r').starts_with("SHELL=")),
        "portable-pty must not inject SHELL outside env_allow: {env_text:?}"
    );
    assert!(
        !env_text
            .lines()
            .any(|line| line.trim_end_matches('\r').starts_with("PATH=")),
        "PATH must not enter child when omitted from env_allow: {env_text:?}"
    );

    // 2. Verbatim `argv` execution (no shell expansion) + ANSI control bytes
    //    preserved intact in `grant_output`, running in `pane1` (same run
    //    `kranz:r-07` already holds its lease and its prior process exited).
    let metachar_arg = "$SGIAN_GRANT_ALLOWED; touch should_not_exist; \x1b[32mok\x1b[0m";
    let grant2 = json!({
        "grant": "g-02-verbatim-argv",
        "mission": "m-3cda6a",
        "run": "r-07",
        "repo": repo_str,
        "cwd": cwd_str,
        "argv": ["/bin/echo", metachar_arg],
        "env_allow": ["SGIAN_GRANT_ALLOWED"],
        "issued_ms": now,
        "expires_ms": now + 60_000,
    });
    let sig2 = sign_grant(key, &grant2);
    let created2: Value = client
        .request(DaemonRequest::GrantExec {
            grant: grant2,
            signature: format!("sha256={sig2}"),
            pane: Some(pane1.clone()),
        })
        .expect("grant_exec verbatim in existing pane");
    assert_eq!(created2["pane_id"], json!(pane1));
    let term2 = created2["terminal_id"].as_str().expect("term2").to_string();
    let waited2: Value = client
        .request(DaemonRequest::GrantWait {
            terminal_id: term2.clone(),
            timeout_ms: Some(5_000),
        })
        .expect("grant_wait 2");
    assert_eq!(waited2["exit"]["exit_code"], json!(0));
    let out2: Value = client
        .request(DaemonRequest::GrantOutput {
            terminal_id: term2.clone(),
        })
        .expect("grant_output 2");
    let echo_text = out2["output"].as_str().expect("echo output");
    assert!(
        echo_text.contains("$SGIAN_GRANT_ALLOWED; touch should_not_exist;"),
        "shell metacharacters must be printed literally: {echo_text:?}"
    );
    assert!(
        echo_text.contains("\x1b[32mok\x1b[0m"),
        "control sequences must stay intact in grant_output: {echo_text:?}"
    );
    assert!(
        !echo_text.contains("SGIAN_GRANT_ALLOWED=allowed-value-42"),
        "scrollback_start_offset must isolate grant2 output from grant1: {echo_text:?}"
    );
    assert!(!sub.join("should_not_exist").exists());

    // 3. Read-only-to-humans default: keyboard lease is held by `kranz:r-07`.
    let lease: LeaseInfo = client
        .request(DaemonRequest::LeaseStatus {
            pane_id: pane1.clone(),
        })
        .expect("lease status");
    assert_eq!(lease.holder.as_deref(), Some("kranz:r-07"));

    let refused_human: Result<Value, String> = client.request(DaemonRequest::SendInputAs {
        pane_id: pane1.clone(),
        input: "ls\r".into(),
        holder: "alice".into(),
        generation: None,
    });
    assert!(
        refused_human.unwrap_err().contains("held by kranz:r-07"),
        "human typing must be refused while kranz:<run> holds the pane"
    );

    // Explicit takeover requires `--force` + `--why`, which is ledgered.
    let taken: LeaseInfo = client
        .request(DaemonRequest::TakeLease {
            pane_id: pane1.clone(),
            holder: "alice".into(),
            force: true,
            why: Some("inspecting failed grant output".into()),
        })
        .expect("force take with why");
    assert_eq!(taken.holder.as_deref(), Some("alice"));

    // 4. Release drops the terminal handle while keeping the pane + ledger.
    for tid in [&term1, &term2] {
        let rel: Value = client
            .request(DaemonRequest::GrantRelease {
                terminal_id: tid.clone(),
            })
            .expect("grant_release");
        assert_eq!(rel["released"], json!(true));
        let gone: Result<Value, String> = client.request(DaemonRequest::GrantOutput {
            terminal_id: tid.clone(),
        });
        assert!(gone.unwrap_err().contains("unknown grant terminal"));
    }

    let panes: PaneList = client
        .request(DaemonRequest::ListPanes)
        .expect("list panes");
    assert!(
        panes.panes.iter().any(|p| p.pane.id == pane1),
        "pane must remain after grant_release"
    );

    // Verify durable hash-chained receipts (`grant.exec`, `pane.ended`, `lease.revoked`).
    let pane_ledger_path = daemon
        .data_dir
        .path()
        .join(LEDGER_DIR)
        .join(format!("{pane1}.jsonl"));
    let summary = ledger_verify(&pane_ledger_path).expect("pane ledger chain valid");
    assert!(summary.records >= 5);
    let records = read_ledger_tail(&pane_ledger_path, 0);
    let exec_records: Vec<&Value> = records
        .iter()
        .filter(|r| r["type"] == json!("grant.exec"))
        .collect();
    assert_eq!(exec_records.len(), 2);
    assert_eq!(
        exec_records[0]["payload"]["grant"],
        json!("g-01-env-filter")
    );
    assert_eq!(exec_records[0]["payload"]["mission"], json!("m-3cda6a"));
    assert_eq!(exec_records[0]["payload"]["run"], json!("r-07"));
    assert!(records.iter().any(|r| r["type"] == json!("pane.ended")));
    assert!(records.iter().any(|r| r["type"] == json!("lease.revoked")));

    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn grant_refusals_cover_hmac_expiry_replay_escape_permissions_and_unknown_fields() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let key = "primary-kranz-grant-key-0123456789abcdef";
    write_grant_key(workspace.path(), key);

    let symlink_escape = workspace.path().join("escape-link");
    std::os::unix::fs::symlink(outside.path(), &symlink_escape).expect("create escape symlink");

    let daemon = TestDaemon::spawn_with_cwd(Config::default(), workspace.path().to_path_buf());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let active_pane = initial.panes[0].id.clone();

    let repo_str = workspace
        .path()
        .canonicalize()
        .expect("canon")
        .display()
        .to_string();
    let now = now_millis();

    let make_grant = |id: &str, cwd: &str, expires_ms: u64| {
        json!({
            "grant": id,
            "mission": "m-1",
            "run": "r-1",
            "repo": repo_str,
            "cwd": cwd,
            "argv": ["/usr/bin/true"],
            "env_allow": [],
            "issued_ms": now.saturating_sub(1_000),
            "expires_ms": expires_ms,
        })
    };

    // 1. Bad HMAC signature -> refused, does NOT burn the grant id!
    let valid_grant = make_grant("g-replay-test", &repo_str, now + 60_000);
    let bad_sig_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: valid_grant.clone(),
            signature: "0".repeat(64),
            pane: None,
        })
        .unwrap_err();
    assert!(
        bad_sig_err.contains("signature verification failed"),
        "{bad_sig_err}"
    );

    // Now the real signature for `g-replay-test` succeeds once...
    let real_sig = sign_grant(key, &valid_grant);
    let ok_once: Value = client
        .request(DaemonRequest::GrantExec {
            grant: valid_grant.clone(),
            signature: real_sig.clone(),
            pane: None,
        })
        .expect("valid grant accepted after bad-sig probe");
    let tid = ok_once["terminal_id"].as_str().expect("tid").to_string();
    let _: Value = client
        .request(DaemonRequest::GrantWait {
            terminal_id: tid.clone(),
            timeout_ms: Some(3_000),
        })
        .expect("wait");
    let _: Value = client
        .request(DaemonRequest::GrantRelease { terminal_id: tid })
        .expect("release");

    // 2. Replaying the exact same signed grant is refused.
    let replay_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: valid_grant,
            signature: real_sig,
            pane: None,
        })
        .unwrap_err();
    assert!(replay_err.contains("already been used"), "{replay_err}");

    // 3. Expired grant is refused.
    let expired = make_grant("g-expired", &repo_str, now.saturating_sub(1));
    let exp_sig = sign_grant(key, &expired);
    let exp_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: expired,
            signature: exp_sig,
            pane: None,
        })
        .unwrap_err();
    assert!(exp_err.contains("expired"), "{exp_err}");

    // 4. Symlink `cwd` escaping `repo` is refused after canonicalization.
    let escaped = make_grant(
        "g-escape",
        &symlink_escape.display().to_string(),
        now + 60_000,
    );
    let esc_sig = sign_grant(key, &escaped);
    let esc_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: escaped,
            signature: esc_sig,
            pane: None,
        })
        .unwrap_err();
    assert!(esc_err.contains("escapes repo"), "{esc_err}");

    // 5. Unknown restriction field in grant payload fails closed.
    let unknown_field = json!({
        "grant": "g-unknown-constraint",
        "mission": "m-1",
        "run": "r-1",
        "repo": repo_str,
        "cwd": repo_str,
        "argv": ["/usr/bin/true"],
        "env_allow": [],
        "network": false,
        "issued_ms": now,
        "expires_ms": now + 60_000,
    });
    let unk_sig = sign_grant(key, &unknown_field);
    let unk_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: unknown_field,
            signature: unk_sig,
            pane: None,
        })
        .unwrap_err();
    assert!(unk_err.contains("unknown field"), "{unk_err}");

    // 6. Unserved repo is refused until `KranzBind` registers it; key rotation
    //    and `0600` permission enforcement apply on that bound repo.
    let other_repo = tempfile::tempdir().expect("other repo");
    let other_key = "secondary-bound-repo-key-0123456789abcdef";
    let other_key_path = write_grant_key(other_repo.path(), other_key);
    let other_canon = other_repo
        .path()
        .canonicalize()
        .expect("canon other")
        .display()
        .to_string();
    let other_grant = json!({
        "grant": "g-bound-1",
        "mission": "m-2",
        "run": "r-2",
        "repo": other_canon,
        "cwd": other_canon,
        "argv": ["/usr/bin/true"],
        "env_allow": [],
        "issued_ms": now,
        "expires_ms": now + 60_000,
    });
    let other_sig = sign_grant(other_key, &other_grant);
    let unserved_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: other_grant.clone(),
            signature: other_sig.clone(),
            pane: None,
        })
        .unwrap_err();
    assert!(
        unserved_err.contains("not served by this daemon"),
        "{unserved_err}"
    );

    let bound: Value = client
        .request(DaemonRequest::KranzBind {
            pane_id: active_pane,
            repo: Some(other_canon.clone()),
        })
        .expect("kranz bind other repo");
    assert_eq!(bound["grant_key"], json!(true));

    let ok_bound: Value = client
        .request(DaemonRequest::GrantExec {
            grant: other_grant,
            signature: other_sig,
            pane: None,
        })
        .expect("bound repo grant accepted");
    let bound_tid = ok_bound["terminal_id"]
        .as_str()
        .expect("bound tid")
        .to_string();
    let _: Value = client
        .request(DaemonRequest::GrantWait {
            terminal_id: bound_tid.clone(),
            timeout_ms: Some(3_000),
        })
        .expect("wait bound");
    let _: Value = client
        .request(DaemonRequest::GrantRelease {
            terminal_id: bound_tid,
        })
        .expect("release bound");

    // Rotate key & widen permissions to 0644 -> refused!
    fs::set_permissions(&other_key_path, fs::Permissions::from_mode(0o644))
        .expect("widen key perms");
    let insecure_grant = json!({
        "grant": "g-bound-insecure",
        "mission": "m-2",
        "run": "r-2",
        "repo": other_canon,
        "cwd": other_canon,
        "argv": ["/usr/bin/true"],
        "env_allow": [],
        "issued_ms": now,
        "expires_ms": now + 60_000,
    });
    let insecure_sig = sign_grant(other_key, &insecure_grant);
    let perm_err = client
        .request::<Value>(DaemonRequest::GrantExec {
            grant: insecure_grant,
            signature: insecure_sig,
            pane: None,
        })
        .unwrap_err();
    assert!(perm_err.contains("insecure permissions"), "{perm_err}");

    // Every refusal above was durably chained into `ledger/grants.jsonl`.
    let grants_ledger = daemon.data_dir.path().join(LEDGER_DIR).join("grants.jsonl");
    let chain = ledger_verify(&grants_ledger).expect("grants.jsonl hash chain valid");
    assert_eq!(chain.records, 7, "all 7 refusals recorded");
    let records = read_ledger_tail(&grants_ledger, 0);
    assert!(records.iter().all(|r| r["type"] == json!("grant.refused")));

    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn grant_kill_terminates_running_pty_child_and_wakes_concurrent_grant_wait() {
    let workspace = tempfile::tempdir().expect("workspace");
    let key = "kill-test-kranz-grant-key-0123456789abcdef";
    write_grant_key(workspace.path(), key);

    let daemon = TestDaemon::spawn_with_cwd(Config::default(), workspace.path().to_path_buf());
    let client = daemon.client();
    let _: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");

    let repo_str = workspace
        .path()
        .canonicalize()
        .expect("canon")
        .display()
        .to_string();
    let now = now_millis();
    let grant = json!({
        "grant": "g-sleep-kill",
        "mission": "m-kill",
        "run": "r-kill",
        "repo": repo_str,
        "cwd": repo_str,
        "argv": ["/bin/sleep", "30"],
        "env_allow": [],
        "issued_ms": now,
        "expires_ms": now + 60_000,
    });
    let sig = sign_grant(key, &grant);
    let created: Value = client
        .request(DaemonRequest::GrantExec {
            grant,
            signature: sig,
            pane: None,
        })
        .expect("spawn sleep grant");
    let tid = created["terminal_id"].as_str().expect("tid").to_string();
    let pane_id = created["pane_id"].as_str().expect("pane_id").to_string();

    // Short bounded wait times out while child is sleeping.
    let timed: Value = client
        .request(DaemonRequest::GrantWait {
            terminal_id: tid.clone(),
            timeout_ms: Some(60),
        })
        .expect("short grant_wait");
    assert_eq!(timed["exited"], json!(false));
    assert_eq!(timed["timed_out"], json!(true));

    // Park a concurrent untimed `grant_wait`, verify daemon stays responsive
    // (zero daemon locks held during wait), then kill the grant child.
    let waiter_client = daemon.client();
    let waiter_tid = tid.clone();
    let waiter = thread::spawn(move || {
        let mut conn = waiter_client.connect().expect("waiter connect");
        conn.set_read_timeout(Some(Duration::from_secs(10)));
        conn.request(&DaemonRequest::GrantWait {
            terminal_id: waiter_tid,
            timeout_ms: None,
        })
        .expect("untimed grant_wait")
    });

    thread::sleep(Duration::from_millis(50));
    let pong: CommandOk = client
        .request(DaemonRequest::Ping)
        .expect("ping while waiting");
    assert!(pong.ok);

    let killed: Value = client
        .request(DaemonRequest::GrantKill {
            terminal_id: tid.clone(),
        })
        .expect("grant_kill");
    assert_eq!(killed["killed"], json!(true));
    assert_eq!(killed["descendants_reaped"], json!(true), "{killed}");
    assert_eq!(killed["output_drained"], json!(true), "{killed}");
    assert_eq!(killed["released"], json!(false), "{killed}");
    // Strict XOR invariant (`exit_code.is_some() != signal.is_some()`): a
    // signal-killed child reports `exit_code: null` and a non-empty `signal`.
    assert_eq!(killed["exit_status"]["exit_code"], Value::Null, "{killed}");
    assert!(
        killed["exit_status"]["signal"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "expected signal name on killed grant: {killed}"
    );

    let wait_resp = waiter.join().expect("waiter joined");
    assert!(wait_resp.ok, "{wait_resp:?}");
    assert_eq!(wait_resp.result["exited"], json!(true));
    assert_eq!(wait_resp.result["timed_out"], json!(false));
    assert_eq!(wait_resp.result["exit_status"]["exit_code"], Value::Null);
    assert!(wait_resp.result["exit_status"]["signal"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));

    // Second `grant_kill` after exit is idempotent (`already_exited: true`).
    let killed_again: Value = client
        .request(DaemonRequest::GrantKill {
            terminal_id: tid.clone(),
        })
        .expect("idempotent grant_kill");
    assert_eq!(killed_again["already_exited"], json!(true));
    assert_eq!(killed_again["descendants_reaped"], json!(true));
    assert_eq!(killed_again["output_drained"], json!(true));

    // Pane stays in registry after kill + release; CleanupReceipt carries
    // `released: true`, `descendants_reaped: true`, `output_drained: true`.
    let released: Value = client
        .request(DaemonRequest::GrantRelease { terminal_id: tid })
        .expect("release");
    assert_eq!(released["released"], json!(true));
    assert_eq!(released["descendants_reaped"], json!(true));
    assert_eq!(released["output_drained"], json!(true));
    assert_eq!(released["exit_status"]["exit_code"], Value::Null);
    assert!(released["exit_status"]["signal"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    let status: PaneStatus = client
        .request(DaemonRequest::PaneStatus { pane_id })
        .expect("pane status after kill+release");
    assert_eq!(status.state, PaneRuntimeState::Ended);

    daemon.shutdown();
}
