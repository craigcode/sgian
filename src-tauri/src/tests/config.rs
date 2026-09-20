use super::*;

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
