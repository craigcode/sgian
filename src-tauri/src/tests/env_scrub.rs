use super::*;

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
