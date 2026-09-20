use super::*;

// ----- (T1) agent detection + attention classification -----

/// (T1) Claude Code at rest: welcome banner + ❯ input box with chrome (2
/// signature groups, no working/needs-input patterns).
pub(crate) const CLAUDE_IDLE_SCREEN: &str =
    "  Claude Code v2.0  \r\n╭──────────╮\r\n│ ❯        │\r\n╰──────────╯\r\n";
/// (T1) Claude Code mid-turn: spinner + verb + the working footer.
pub(crate) const CLAUDE_WORKING_SCREEN: &str =
    "  Claude Code v2.0  \r\n✻ Thinking… esc to interrupt ⠋\r\n╭──────────╮\r\n│ ❯        │\r\n╰──────────╯\r\n";

/// (T1) Reset a pane's classification throttle so the next feed_model
/// reclassifies immediately (classification is throttled to 500 ms/pane).
pub(crate) fn agent_unthrottle(router: &OutputRouter, pane_id: &str) {
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
    pub(crate) fn next_event(reader: &mut BufReader<TransportStream>) -> DaemonEvent {
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
pub(crate) fn router_event_reader(router: &OutputRouter) -> BufReader<UnixStream> {
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
pub(crate) fn read_router_agent_state(
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
pub(crate) fn assert_no_agent_event(
    reader: &mut BufReader<UnixStream>,
    window: Duration,
    what: &str,
) {
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

pub(crate) fn agent_args(items: &[&str]) -> Vec<String> {
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
pub(crate) const FAKE_CLAUDE_SH: &str = r#"#!/bin/sh
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
pub(crate) struct FakeClaude {
    pub(crate) _dir: tempfile::TempDir,
    pub(crate) bin: PathBuf,
    pub(crate) log: PathBuf,
}

#[cfg(unix)]
pub(crate) fn install_fake_claude() -> FakeClaude {
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
pub(crate) const FAKE_DROID_SH: &str = r#"#!/bin/sh
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
pub(crate) struct FakeDroid {
    pub(crate) _dir: tempfile::TempDir,
    pub(crate) bin: PathBuf,
    pub(crate) log: PathBuf,
}

#[cfg(unix)]
pub(crate) fn install_fake_droid() -> FakeDroid {
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
pub(crate) fn droid_test_config(fake: &FakeDroid) -> Config {
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
pub(crate) fn agent_test_config(fake: &FakeClaude) -> Config {
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
pub(crate) fn driver_log_contents(fake: &FakeClaude) -> String {
    fs::read_to_string(&fake.log).unwrap_or_default()
}

/// (T2) Spawn a TestDaemon with the fake driver and an EXISTING cwd (the
/// shared /tmp/sgian-itest cwd does not exist, and std::process::Command
/// — unlike portable-pty — fails the spawn with ENOENT for a missing cwd).
/// The cwd TempDir is returned to keep it alive for the test.
#[cfg(unix)]
pub(crate) fn spawn_agent_test_daemon(fake: &FakeClaude) -> (TestDaemon, tempfile::TempDir) {
    let cwd = tempfile::tempdir().expect("agent test cwd");
    let daemon = TestDaemon::spawn_with_cwd(agent_test_config(fake), cwd.path().to_path_buf());
    (daemon, cwd)
}

#[cfg(unix)]
pub(crate) fn spawn_droid_test_daemon(fake: &FakeDroid) -> (TestDaemon, tempfile::TempDir) {
    let cwd = tempfile::tempdir().expect("Droid test cwd");
    let daemon = TestDaemon::spawn_with_cwd(droid_test_config(fake), cwd.path().to_path_buf());
    (daemon, cwd)
}

#[cfg(unix)]
pub(crate) fn subscribe_events(client: &DaemonClient) -> DaemonConnection {
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
pub(crate) fn read_event_until<T>(
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
pub(crate) fn read_agent_event(reader: &mut DaemonConnection, pane_id: &str, kind: &str) -> Value {
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
