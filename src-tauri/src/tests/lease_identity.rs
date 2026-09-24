use super::*;

// ----- Keyboard lease predicates, ledger chain, ctl parsing, IPC round trip -----
// docs/design/keyboard-lease-and-ledger.md

pub(crate) fn held_by(holder: &str) -> HeldLease {
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

pub(crate) fn args(values: &[&str]) -> Vec<String> {
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
        unattended: is_unattended_mode(Some("auto".to_string()).as_deref()),
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
pub(crate) fn connect_with_client_token(
    daemon: &TestDaemon,
    token: &str,
) -> Result<DaemonConnection, String> {
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
    // Wait for the prompt before typing: a write that races the shell's
    // startup handshake can be partly swallowed under full-suite load, and
    // then the marker never appears (the same race the run harness closes).
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        let snapshot: Value = client
            .request(DaemonRequest::Snapshot {
                pane_id: pane_id.clone(),
            })
            .unwrap_or(Value::Null);
        if snapshot["revision"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    let _ = client.request::<Value>(DaemonRequest::Wait {
        pane_id: pane_id.clone(),
        condition: WaitCondition::Idle(120),
        timeout_ms: Some(5000),
    });
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
    // SIGTERM, a grace period, then SIGKILL: allow well past the grace.
    for _ in 0..400 {
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
    // Terminal capability traffic is not hidden output: the XTVERSION reply
    // a terminal sends (and the tty echoes before the app goes raw), the
    // DECRQSS and XTGETTCAP queries and replies, and the kitty graphics
    // support query. An image transmission or any other payload still counts.
    for benign in [
        "\u{1b}P>|SwiftTerm 1.2.3\u{1b}\\",
        "\u{1b}P$q q\u{1b}\\",
        "\u{1b}P1$r0 q\u{1b}\\",
        "\u{1b}P+q544e\u{1b}\\",
        "\u{1b}P1+r544e=1\u{1b}\\",
        "\u{1b}_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\u{1b}\\",
    ] {
        assert_eq!(scan_output_tricks(benign).total(), 0, "{benign:?}");
    }
    let (tricks, sample) = scan_output_tricks_detailed("\u{1b}_Ga=T,f=100;iVBOR\u{1b}\\");
    assert_eq!(tricks.string_controls, 1);
    assert_eq!(sample.as_deref(), Some("APC \"Ga=T,f=100;iVBOR\""));
    let (tricks, sample) = scan_output_tricks_detailed("\u{1b}Pq payload\u{1b}\\");
    assert_eq!(tricks.string_controls, 1);
    assert_eq!(sample.as_deref(), Some("DCS \"q payload\""));
    let long = format!("\u{1b}P{}\u{1b}\\", "x".repeat(200));
    let (_, sample) = scan_output_tricks_detailed(&long);
    let sample = sample.expect("sample");
    assert!(
        sample.ends_with('…') && sample.chars().count() < 70,
        "{sample}"
    );
    assert_eq!(
        scan_output_tricks_detailed("\u{1b}[8mhidden\u{1b}[28m").1,
        None
    );
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
    assert_eq!(
        suspicious[0]["payload"]["sample"],
        Value::Null,
        "self-describing hits carry no sample"
    );
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

#[test]
fn serve_maps_frontend_commands_and_guards_assets() {
    let call = frontend_command(
        "write_to_pane",
        &json!({ "paneId": "p1", "data": "ls\n" }),
        "phone",
    )
    .expect("map");
    assert_eq!(
        call,
        FrontendCall::Request(DaemonRequest::SendInputAs {
            pane_id: "p1".into(),
            input: "ls\n".into(),
            holder: "phone".into(),
            generation: None
        })
    );
    let call = frontend_command(
        "resize_pane_terminal",
        &json!({ "paneId": "p1", "cols": 120, "rows": 0 }),
        "phone",
    )
    .expect("resize");
    assert!(matches!(
        call,
        FrontendCall::Request(DaemonRequest::ResizePaneTerminal {
            cols: 120,
            rows: 1,
            ..
        })
    ));
    assert!(matches!(
        frontend_command("take_lease", &json!({ "paneId": "p1", "force": true }), "phone").expect("take"),
        FrontendCall::Request(DaemonRequest::TakeLease { ref holder, force: true, .. }) if holder == "phone"
    ));
    assert!(matches!(
        frontend_command("create_agent_pane", &json!({ "backend": "droid" }), "x").expect("agent"),
        FrontendCall::Request(DaemonRequest::CreateAgentPaneWithSpec {
            backend: Some(AgentBackendKind::Droid),
            ..
        })
    ));
    assert!(frontend_command("create_agent_pane", &json!({ "backend": "nope" }), "x").is_err());
    assert_eq!(
        frontend_command("client_holder", &Value::Null, "x").expect("holder"),
        FrontendCall::Holder
    );
    assert_eq!(
        frontend_command("ui_smoke_enabled", &Value::Null, "x").expect("smoke"),
        FrontendCall::SmokeDisabled
    );
    assert!(matches!(
        frontend_command("install_update", &Value::Null, "x").expect("update"),
        FrontendCall::Unsupported(_)
    ));
    assert!(
        frontend_command("close_pane", &json!({}), "x").is_err(),
        "missing paneId"
    );
    assert!(
        frontend_command("shutdown", &Value::Null, "x").is_err(),
        "not a frontend command"
    );

    assert_eq!(
        sse_frame("pty-output", &json!({ "a": 1 })),
        "event: pty-output\ndata: {\"a\":1}\n\n"
    );
    let (name, payload) = frontend_event(DaemonEvent::PaneClosed {
        pane_id: "p".into(),
    })
    .expect("event");
    assert_eq!((name, payload), ("pane-closed", json!({ "pane_id": "p" })));
    assert!(frontend_event(DaemonEvent::SubscribeAck).is_none());

    assert!(embedded_asset("/../Cargo.toml").is_none());
    assert!(
        host_is_loopback(Some("localhost:8321"))
            && host_is_loopback(Some("127.0.0.1"))
            && host_is_loopback(Some("[::1]:1"))
    );
    assert!(
        !host_is_loopback(Some("evil.example"))
            && !host_is_loopback(Some("localhost.evil:1"))
            && !host_is_loopback(None)
    );
    let head = parse_request_head(
        "POST /api/invoke?x=1 HTTP/1.1\r\nHost: localhost:8321\r\ncontent-length: 12\r\n",
    )
    .expect("parse");
    assert_eq!(
        head,
        HttpRequest {
            method: "POST".into(),
            path: "/api/invoke".into(),
            host: Some("localhost:8321".into()),
            content_length: 12,
            query_key: None,
            presented_key: None,
        }
    );
    let keyed = parse_request_head(
        "GET /api/events?key=abc HTTP/1.1\r\nHost: localhost\r\nCookie: a=b; sgian_serve=k1\r\n",
    )
    .expect("parse");
    assert_eq!(keyed.query_key.as_deref(), Some("abc"));
    assert_eq!(keyed.presented_key.as_deref(), Some("k1"));
    let header = parse_request_head(
        "GET / HTTP/1.1\r\nHost: localhost\r\nX-Sgian-Key: k2\r\nCookie: sgian_serve=k1\r\n",
    )
    .expect("parse");
    assert_eq!(
        header.presented_key.as_deref(),
        Some("k2"),
        "the header wins"
    );
    assert!(parse_request_head("GARBAGE").is_none());
    assert!(parse_request_head("GET / HTTP/1.1\r\nContent-Length: x\r\n").is_none());
    assert!(embedded_asset("/assets//x.js").is_none());
    let opts = parse_serve_args(
        PathBuf::from("/w"),
        &args(&["--port", "9000", "--allow-write"]),
    )
    .expect("args");
    assert_eq!((opts.port, opts.allow_write), (9000, true));
    assert_eq!(
        parse_serve_args(PathBuf::from("/w"), &[])
            .expect("bare")
            .port,
        SERVE_DEFAULT_PORT
    );
    assert!(parse_serve_args(PathBuf::from("/w"), &args(&["--port", "x"])).is_err());
    assert!(parse_serve_args(PathBuf::from("/w"), &args(&["--bogus"])).is_err());
}

/// One HTTP exchange over a raw socket: status line, headers, body. For a
/// streaming response, stop once the body contains `until`. `key` is sent
/// as the `X-Sgian-Key` header (empty = none).
#[cfg(unix)]
fn http(addr: std::net::SocketAddr, request: &str, until: &str) -> (u16, String, String) {
    http_keyed(addr, request, until, "")
}

#[cfg(unix)]
fn http_keyed(
    addr: std::net::SocketAddr,
    request: &str,
    until: &str,
    key: &str,
) -> (u16, String, String) {
    let request = if key.is_empty() {
        request.to_string()
    } else {
        request.replacen("\r\n", &format!("\r\nX-Sgian-Key: {key}\r\n"), 1)
    };
    let request = request.as_str();
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    stream.write_all(request.as_bytes()).expect("write");
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if !until.is_empty() && String::from_utf8_lossy(&raw).contains(until) {
                    break;
                }
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..pos]).to_string();
                    if let Some(len) = head
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                    {
                        if raw.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

#[cfg(unix)]
#[test]
fn serve_answers_invokes_streams_events_and_stays_read_only_by_default() {
    let daemon = TestDaemon::spawn(Config::default());
    let client = daemon.client();
    let initial: WorkspaceSnapshot = client
        .request(DaemonRequest::BootstrapWorkspace)
        .expect("bootstrap");
    let pane_id = initial.panes[0].id.clone();

    let viewer = start_serve_with(daemon.client(), 0, false).expect("serve");
    let key = viewer.key.clone();
    let port = viewer.addr.port();
    let post = |body: &str| {
        format!(
            "POST /api/invoke HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    // No key: nothing reaches the daemon, page or API.
    let (status, _, body) = http(
        viewer.addr,
        &post(r#"{"command":"bootstrap_workspace","args":{}}"#),
        "",
    );
    assert_eq!(status, 401, "{body}");
    let (status, _, body) = http(
        viewer.addr,
        &format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"),
        "",
    );
    assert_eq!(status, 401);
    assert!(body.contains("sgian ctl serve"), "{body}");
    let (status, _, _) = http_keyed(
        viewer.addr,
        &post(r#"{"command":"bootstrap_workspace","args":{}}"#),
        "",
        "not-the-key",
    );
    assert_eq!(status, 401);
    // The printed URL sets the cookie and redirects; the cookie then works.
    assert!(viewer.url().ends_with(&format!("/?key={key}")));
    let (status, head, _) = http(
        viewer.addr,
        &format!("GET /?key={key} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"),
        "",
    );
    assert_eq!(status, 303, "{head}");
    assert!(
        head.contains(&format!(
            "Set-Cookie: {SERVE_COOKIE}={key}; HttpOnly; SameSite=Strict; Path=/"
        )),
        "{head}"
    );
    assert!(head.contains("Location: /"), "{head}");
    let cookie_body = r#"{"command":"client_holder","args":{}}"#;
    let (status, _, body) = http(
        viewer.addr,
        &format!(
            "POST /api/invoke HTTP/1.1\r\nHost: localhost:{port}\r\nCookie: other=1; {SERVE_COOKIE}={key}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{cookie_body}",
            cookie_body.len()
        ),
        "",
    );
    assert_eq!(status, 200, "{body}");
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["ok"], json!(true), "{body}");

    let (status, _, body) = http_keyed(
        viewer.addr,
        &post(r#"{"command":"bootstrap_workspace","args":{}}"#),
        "",
        &key,
    );
    assert_eq!(status, 200);
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["ok"], json!(true), "{body}");
    assert_eq!(answer["result"]["panes"][0]["id"], json!(pane_id));
    let (_, _, body) = http_keyed(
        viewer.addr,
        &post(r#"{"command":"client_holder","args":{}}"#),
        "",
        &key,
    );
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(answer["result"].as_str().is_some_and(|h| !h.is_empty()));
    // Read-only by default: a write is refused before it reaches the daemon.
    let (_, _, body) = http_keyed(
        viewer.addr,
        &post(&format!(
            r#"{{"command":"write_to_pane","args":{{"paneId":"{pane_id}","data":"x"}}}}"#
        )),
        "",
        &key,
    );
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["ok"], json!(false));
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or("")
            .contains("read-only view"),
        "{body}"
    );
    // View-local writes succeed silently for a viewer instead of refusing:
    // the page calls them for every pane on boot.
    let (_, _, body) = http_keyed(
        viewer.addr,
        &post(&format!(
            r#"{{"command":"ensure_pane_terminal","args":{{"paneId":"{pane_id}"}}}}"#
        )),
        "",
        &key,
    );
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["ok"], json!(true), "{body}");
    assert!(is_view_local_write("resize_pane_terminal") && !is_view_local_write("write_to_pane"));
    // Not a loopback host: refused before the key is even looked at.
    let (status, _, _) = http_keyed(
        viewer.addr,
        &format!(
            "GET /api/events HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n"
        ),
        "",
        &key,
    );
    assert_eq!(status, 403);
    // Events stream: the connected comment arrives.
    let (status, head, body) = http_keyed(
        viewer.addr,
        &format!("GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        ": connected",
        &key,
    );
    assert_eq!(status, 200);
    assert!(head.contains("text/event-stream"), "{head}");
    assert!(body.contains(": connected"), "{body}");
    viewer.stop();

    // With writes allowed the same call lands (attributed to the holder),
    // but admin never does.
    let writer = start_serve_with(daemon.client(), 0, true).expect("serve rw");
    let wkey = writer.key.clone();
    client
        .request::<CommandOk>(DaemonRequest::EnsurePaneTerminal {
            pane_id: pane_id.clone(),
        })
        .expect("ensure terminal");
    let post_rw = |body: String| {
        format!(
            "POST /api/invoke HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    let (_, _, body) = http_keyed(
        writer.addr,
        &post_rw(format!(
            r#"{{"command":"write_to_pane","args":{{"paneId":"{pane_id}","data":""}}}}"#
        )),
        "",
        &wkey,
    );
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(answer["ok"], json!(true), "{body}");
    let (_, _, body) = http_keyed(
        writer.addr,
        &post_rw(r#"{"command":"write_config","args":{"config":{}}}"#.to_string()),
        "",
        &wkey,
    );
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available from a served view"),
        "{body}"
    );
    // Unsupported desktop command: a clean error, not a crash.
    let (_, _, body) = http_keyed(
        writer.addr,
        &post_rw(r#"{"command":"install_update","args":{}}"#.to_string()),
        "",
        &wkey,
    );
    let answer: Value = serde_json::from_str(&body).expect("json");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or("")
            .contains("not available"),
        "{body}"
    );
    writer.stop();
    daemon.shutdown();
}

#[test]
fn review_p2_pure_guards() {
    // S6: with workspace.json unparseable, the cwd marker still guards.
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join(WORKSPACE_FILE), b"{not json").expect("corrupt file");
    assert!(
        check_persisted_cwd(Path::new("/tmp/a"), dir.path()).is_ok(),
        "no marker: nothing to check"
    );
    write_workspace_cwd_marker(dir.path(), Path::new("/tmp/a"));
    assert!(check_persisted_cwd(Path::new("/tmp/a"), dir.path()).is_ok());
    let refused = check_persisted_cwd(Path::new("/tmp/b"), dir.path()).expect_err("collision");
    assert!(refused.contains("collision"), "{refused}");

    // S11: the input queue is capped by bytes, not only entries.
    let (sender, _receiver) = sync_channel::<Vec<u8>>(PANE_INPUT_QUEUE_LIMIT);
    let queue = InputQueue {
        sender,
        queued_bytes: Arc::new(AtomicUsize::new(0)),
    };
    let chunk = "x".repeat(PANE_INPUT_QUEUE_BYTES / 2 + 1);
    assert!(queue_pane_input(&queue, "p", &chunk).is_ok());
    let refused = queue_pane_input(&queue, "p", &chunk).expect_err("over the byte cap");
    assert!(refused.contains("backlogged"), "{refused}");
    assert!(
        queue_pane_input(&queue, "p", "small").is_ok(),
        "small chunks still fit"
    );
    assert_eq!(
        queue.queued_bytes.load(Ordering::SeqCst),
        chunk.len() + "small".len(),
        "a refused chunk is not counted"
    );

    // S8: a reload updates the scrub list of every stored per-pane shell.
    let mut store = crate::tests::terminal::sh_terminal_store("/tmp");
    store.pane_shells.insert(
        "pane-p".to_string(),
        ShellConfig {
            shell: "/bin/zsh".to_string(),
            args: vec!["-l".to_string()],
            env: HashMap::new(),
            scrub_env: vec!["OLD".to_string()],
        },
    );
    store.apply_reloaded_config(
        ShellConfig {
            shell: "/bin/sh".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            scrub_env: vec!["SECRET".to_string()],
        },
        AgentSpawnConfig::default(),
    );
    let stored = store.pane_shells.get("pane-p").expect("stored shell");
    assert_eq!(stored.scrub_env, vec!["SECRET".to_string()]);
    assert_eq!(stored.shell, "/bin/zsh", "the profile's own shell stays");
}

#[test]
fn output_guard_announcement_names_the_first_opaque_string() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = OutputRouter::new(dir.path().join("scrollback"));
    let ledger_dir = dir.path().join(LEDGER_DIR);
    fs::create_dir_all(&ledger_dir).expect("ledger dir");
    router.set_ledger(Arc::new(Mutex::new(LedgerSink::new(ledger_dir.clone()))));
    fs::create_dir_all(dir.path().join("scrollback")).expect("scrollback dir");
    router.ensure_model("pane-7", 80, 24);
    router.ensure_model("pane-8", 80, 24);

    // What a Claude Code start looks like through SwiftTerm: the query, then
    // the terminal's reply echoed by the tty. Not a warning.
    router.emit(
        "pane-7",
        "\u{1b}[>0q\u{1b}[?u\u{1b}[c\u{1b}P>|SwiftTerm 1.2.3\u{1b}\\\r\n".to_string(),
    );
    assert_eq!(router.output_tricks("pane-7"), OutputTricks::default());
    assert!(router.output_warnings().is_empty());

    router.emit("pane-8", "\u{1b}_Ga=T,f=100;iVBOR\u{1b}\\".to_string());
    let records = read_ledger_tail(&ledger_path(&ledger_dir, "pane-8"), 0);
    let suspicious: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] == json!("output.suspicious"))
        .collect();
    assert_eq!(suspicious.len(), 1);
    assert_eq!(
        suspicious[0]["payload"]["sample"],
        json!("APC \"Ga=T,f=100;iVBOR\"")
    );
    assert_eq!(
        suspicious[0]["payload"]["total"]["string_controls"],
        json!(1)
    );
}

#[test]
fn agent_state_event_carries_unattended_derived_from_mode() {
    let auto = DaemonEvent::agent_state(
        "pane-1".to_string(),
        Some("claude".to_string()),
        Some(AgentAttention::Idle),
        Some("auto".to_string()),
    );
    let json = serde_json::to_value(&auto).expect("serialize");
    assert_eq!(json["event"], json!("agent_state"));
    assert_eq!(json["mode"], json!("auto"));
    assert_eq!(json["unattended"], json!(true));
    for mode in ["bypass", "bypassPermissions", "dontAsk"] {
        let event = DaemonEvent::agent_state("p".to_string(), None, None, Some(mode.to_string()));
        assert_eq!(
            serde_json::to_value(&event).unwrap()["unattended"],
            json!(true)
        );
    }
    let plan = DaemonEvent::agent_state("p".to_string(), None, None, Some("plan".to_string()));
    assert_eq!(
        serde_json::to_value(&plan).unwrap()["unattended"],
        json!(false)
    );
    let none = DaemonEvent::agent_state("p".to_string(), None, None, None);
    let json = serde_json::to_value(&none).expect("serialize");
    assert_eq!(json["unattended"], json!(false));
    assert!(json.get("mode").is_none(), "mode stays additive");
    // An old daemon's event without the field still decodes.
    let legacy: DaemonEvent = serde_json::from_str(
        r#"{"event":"agent_state","pane_id":"p","agent":null,"attention":null}"#,
    )
    .expect("decode");
    assert_eq!(
        legacy,
        DaemonEvent::agent_state("p".to_string(), None, None, None)
    );
}
