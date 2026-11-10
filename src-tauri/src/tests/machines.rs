use super::*;

#[test]
fn machine_entry_validation_enforces_naming_and_shell_safety() {
    // Reserved and malformed names are rejected.
    for bad_name in ["", "local", "LOCAL", "-flag", ".dot", "a/b", "has space"] {
        assert!(
            validate_machine_name(bad_name).is_err(),
            "expected '{bad_name}' to be rejected"
        );
    }
    assert_eq!(
        validate_machine_name("dev_box-01.us").unwrap(),
        "dev_box-01.us"
    );

    let valid_ssh = MachineEntry {
        name: "devbox".to_string(),
        transport: "ssh".to_string(),
        host: Some("ubuntu@10.0.1.5".to_string()),
        port: Some(2222),
        workspace: "/home/ubuntu/repo".to_string(),
        target: None,
        token_file: None,
    };
    valid_ssh.validate().expect("valid ssh entry");

    // Windows path with spaces (no shell metacharacters) is accepted so Windows
    // remote targets (#116) work cleanly when quoted by discovery.
    let valid_windows = MachineEntry {
        name: "winbox".to_string(),
        transport: "ssh".to_string(),
        host: Some("dev@10.0.1.9".to_string()),
        port: None,
        workspace: "C:\\Users\\Craig Martin\\Projects\\app".to_string(),
        target: None,
        token_file: None,
    };
    valid_windows.validate().expect("valid windows path entry");

    // Leading `-` in host or workspace (option injection) and shell metacharacters
    // in workspace (command injection over ssh) are rejected.
    for bad in [
        MachineEntry {
            host: Some("-oProxyCommand=evil".to_string()),
            ..valid_ssh.clone()
        },
        MachineEntry {
            workspace: "-rf".to_string(),
            ..valid_ssh.clone()
        },
        MachineEntry {
            workspace: "/srv/app$(id)".to_string(),
            ..valid_ssh.clone()
        },
        MachineEntry {
            workspace: "/srv/app;id".to_string(),
            ..valid_ssh.clone()
        },
        MachineEntry {
            workspace: "/srv/\"escape\"".to_string(),
            ..valid_ssh.clone()
        },
        MachineEntry {
            host: None,
            ..valid_ssh.clone()
        },
        MachineEntry {
            target: Some("i-012345".to_string()),
            ..valid_ssh.clone()
        },
    ] {
        assert!(
            bad.validate().is_err(),
            "expected {bad:?} to fail validation"
        );
    }

    // SSM schema readiness (#115): requires `target`, forbids `port`.
    let valid_ssm = MachineEntry {
        name: "ec2-prod".to_string(),
        transport: "ssm".to_string(),
        host: None,
        port: None,
        workspace: "/srv/service".to_string(),
        target: Some("i-0abc1234def567890".to_string()),
        token_file: None,
    };
    valid_ssm.validate().expect("valid ssm schema");
    assert!(MachineEntry {
        port: Some(22),
        ..valid_ssm.clone()
    }
    .validate()
    .is_err());

    // Duplicate names in `machines` list are rejected.
    assert!(validate_machines(&[valid_ssh.clone(), valid_ssh]).is_err());
}

#[test]
fn ctl_machine_add_list_remove_persists_global_config_and_private_token() {
    let app_dir = tempfile::tempdir().expect("temp app dir");

    let add_cmd = parse_machine_args(&[
        "add".to_string(),
        "devbox".to_string(),
        "--host".to_string(),
        "ubuntu@10.0.0.8".to_string(),
        "--port".to_string(),
        "2202".to_string(),
        "--workspace".to_string(),
        "/srv/repo".to_string(),
        "--token-stdin".to_string(),
    ])
    .expect("parse machine add");

    let fixture_cred = format!("sgc_{}", "a".repeat(64));
    let added = control_machine_in(
        app_dir.path(),
        add_cmd,
        std::io::Cursor::new(format!("{fixture_cred}\n")),
        true,
    )
    .expect("machine add should succeed");
    assert_eq!(added["ok"], json!(true));
    assert_eq!(added["machine"]["name"], json!("devbox"));

    // Global config.json must never contain the raw token string.
    let raw_config =
        fs::read_to_string(app_dir.path().join(CONFIG_FILE)).expect("config.json created");
    assert!(
        !raw_config.contains(&fixture_cred),
        "global config.json must never store plaintext client tokens"
    );

    let token_path = machine_default_token_path_in(app_dir.path(), "devbox");
    assert_eq!(
        read_token(&token_path).expect("read token"),
        Some(fixture_cred.clone())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "machine token file must be 0600, got {mode:o}");

        // Refuses to overwrite a symlinked token file.
        let outside = app_dir.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(&token_path).unwrap();
        std::os::unix::fs::symlink(&outside, &token_path).unwrap();
        assert!(
            write_machine_token_in(app_dir.path(), "devbox", "sgc_newtoken").is_err(),
            "must refuse overwriting symlinked token file"
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        fs::remove_file(&token_path).unwrap();
        write_machine_token_in(app_dir.path(), "devbox", &fixture_cred).unwrap();
    }

    // `machine list` reports token_configured = true without exposing the secret.
    let listed = control_machine_in(app_dir.path(), MachineCommand::List, std::io::empty(), true)
        .expect("machine list");
    let rows = listed.as_array().expect("array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], json!("devbox"));
    assert_eq!(rows[0]["port"], json!(2202));
    assert_eq!(rows[0]["token_configured"], json!(true));
    assert!(!listed.to_string().contains(&fixture_cred));

    // `machine remove` removes config entry and deletes default token file.
    let removed = control_machine_in(
        app_dir.path(),
        MachineCommand::Remove {
            name: "devbox".to_string(),
        },
        std::io::empty(),
        true,
    )
    .expect("machine remove");
    assert_eq!(removed["removed"], json!("devbox"));
    assert!(
        !token_path.exists(),
        "default token file should be cleaned up"
    );
    assert!(load_global_machines_in(app_dir.path()).unwrap().is_empty());
}

#[test]
fn parse_control_options_and_qualified_pane_refs_enforce_precedence() {
    // `--machine` and `--workspace` conflict on CLI.
    assert!(parse_control_options_with_env(
        vec![
            "--machine".into(),
            "devbox".into(),
            "--workspace".into(),
            "/tmp/ws".into(),
            "panes".into(),
        ],
        None,
        None,
    )
    .is_err());

    // `SGIAN_MACHINE` and `SGIAN_WORKSPACE` conflict when neither CLI flag is set,
    // while an explicit CLI flag overrides the opposite env var cleanly.
    assert!(parse_control_options_with_env(
        vec!["panes".into()],
        Some(PathBuf::from("/tmp/ws")),
        Some("devbox".into()),
    )
    .is_err());
    let cli_ws_wins = parse_control_options_with_env(
        vec!["--workspace".into(), "/tmp/explicit".into(), "panes".into()],
        None,
        Some("devbox".into()),
    )
    .expect("explicit --workspace overrides SGIAN_MACHINE");
    assert_eq!(cli_ws_wins.machine, None);
    assert!(cli_ws_wins.explicit_workspace);

    let cli_machine_wins = parse_control_options_with_env(
        vec!["-m".into(), "devbox".into(), "panes".into()],
        Some(PathBuf::from("/tmp/ws")),
        None,
    )
    .expect("explicit -m overrides SGIAN_WORKSPACE");
    assert_eq!(cli_machine_wins.machine.as_deref(), Some("devbox"));

    // `-m` AFTER `lease release` stays the mandatory release note flag, never `--machine`.
    let lease_opts = parse_control_options_with_env(
        vec![
            "lease".into(),
            "release".into(),
            "pane-1".into(),
            "-m".into(),
            "handed back".into(),
        ],
        None,
        None,
    )
    .expect("lease release -m must not be eaten as --machine");
    assert_eq!(lease_opts.machine, None);
    assert_eq!(
        lease_opts.args,
        vec!["lease", "release", "pane-1", "-m", "handed back"]
    );

    // `ctl machine add devbox --workspace /srv/repo --json` keeps `--workspace` for
    // `parse_machine_args` while still recognizing `--json`.
    let machine_add = parse_control_options_with_env(
        vec![
            "machine".into(),
            "add".into(),
            "devbox".into(),
            "--host".into(),
            "h".into(),
            "--workspace".into(),
            "/srv/repo".into(),
            "--json".into(),
        ],
        None,
        Some("other".into()),
    )
    .expect("machine add options");
    assert!(machine_add.json);
    assert_eq!(machine_add.machine, None);
    let parsed_add = parse_machine_args(&machine_add.args[1..]).expect("parse machine add");
    assert!(matches!(parsed_add, MachineCommand::Add { .. }));

    // `<machine>/<pane>` rewrites only the pane slot across subcommands.
    let known: HashSet<String> = ["devbox".to_string(), "winbox".to_string()]
        .into_iter()
        .collect();

    let (m, args) = rewrite_qualified_subcommand_args(
        "send",
        &[
            "--lf".into(),
            "devbox/pane-2".into(),
            "echo".into(),
            "devbox/keep".into(),
        ],
        None,
        false,
        &known,
    )
    .unwrap();
    assert_eq!(m.as_deref(), Some("devbox"));
    assert_eq!(args, vec!["--lf", "pane-2", "echo", "devbox/keep"]);

    let (m, args) = rewrite_qualified_subcommand_args(
        "wait",
        &[
            "winbox/agent-1".into(),
            "--attention".into(),
            "needs_input,idle".into(),
        ],
        None,
        false,
        &known,
    )
    .unwrap();
    assert_eq!(m.as_deref(), Some("winbox"));
    assert_eq!(args, vec!["agent-1", "--attention", "needs_input,idle"]);

    let (m, args) = rewrite_qualified_subcommand_args(
        "lines",
        &["devbox/pane-1".into(), "-n".into(), "40".into()],
        None,
        false,
        &known,
    )
    .unwrap();
    assert_eq!(m.as_deref(), Some("devbox"));
    assert_eq!(args, vec!["pane-1", "-n", "40"]);

    let (m, args) = rewrite_qualified_subcommand_args(
        "run",
        &[
            "--pane".into(),
            "devbox/build".into(),
            "--".into(),
            "cargo".into(),
            "test".into(),
        ],
        None,
        false,
        &known,
    )
    .unwrap();
    assert_eq!(m.as_deref(), Some("devbox"));
    assert_eq!(args, vec!["--pane", "build", "--", "cargo", "test"]);

    // Conflicts with `--workspace` or a different `--machine` fail closed.
    assert!(rewrite_qualified_subcommand_args(
        "snapshot",
        &["devbox/pane-1".into()],
        None,
        true,
        &known,
    )
    .is_err());
    assert!(rewrite_qualified_subcommand_args(
        "snapshot",
        &["devbox/pane-1".into()],
        Some("winbox"),
        false,
        &known,
    )
    .is_err());
}

#[test]
fn loopback_tcp_listener_requires_client_token_and_serves_framed_ipc() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        remote_loopback_port: Some(0),
        ..Default::default()
    });
    let local_client = daemon.client();

    // `native_ipc_endpoint` exposes the bound ephemeral `loopback_port`.
    let endpoint = native_ipc_endpoint(&local_client).expect("ipc endpoint");
    let port = endpoint
        .loopback_port
        .expect("daemon with remote_loopback_port=0 should publish daemon.loopback_port");
    assert!(port > 0);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    // Refuses dialing non-loopback addresses on the client transport helper.
    let non_loopback: std::net::SocketAddr = "192.0.2.1:9999".parse().unwrap();
    assert!(transport_connect_loopback(non_loopback).is_err());

    // 1. Connecting over LoopbackTcp with the valid root `daemon.token` and NO
    // `client_token` MUST be rejected (#116 security invariant).
    let root_only_err = DaemonConnection::connect_with_options(
        &daemon.socket_path,
        Some(addr),
        &daemon.token,
        None,
        Some(Duration::from_secs(5)),
    )
    .err()
    .expect("bare workspace token over loopback TCP must be rejected");
    assert!(
        root_only_err.contains("client_token"),
        "expected client_token rejection message, got: {root_only_err}"
    );

    // 2. Issue a scoped per-client credential on the local socket, then connect
    // over LoopbackTcp with ONLY `client_token` (empty workspace `token`).
    let issued: Value = local_client
        .request(DaemonRequest::IdentityIssue {
            holder: "remote-operator".to_string(),
            scopes: vec!["read".to_string(), "write".to_string(), "admin".to_string()],
        })
        .expect("issue client credential");
    let cred_id = issued["id"].as_str().unwrap().to_string();
    let client_token = issued["token"].as_str().unwrap().to_string();

    let tcp_client = DaemonClient {
        cwd: daemon.cwd.clone(),
        socket_path: daemon.socket_path.clone(),
        data_dir: daemon.data_dir.path().to_path_buf(),
        token: String::new(),
        auto_spawn: false,
        client_token: Some(client_token.clone()),
        tcp_endpoint: Some(addr),
        machine_reconnect: None,
    };

    let whoami: Value = tcp_client
        .request(DaemonRequest::Whoami)
        .expect("whoami over loopback TCP");
    assert_eq!(whoami["holder"], json!("remote-operator"));
    assert_eq!(effective_holder(&tcp_client), "remote-operator");

    // Subscribe over LoopbackTcp receives `SubscribeAck` and live `PaneCreated` events.
    let mut sub_conn = tcp_client.connect().expect("connect subscriber over TCP");
    sub_conn
        .write_request(&DaemonRequest::Subscribe)
        .expect("write subscribe");
    sub_conn
        .await_subscribe_ack()
        .expect("subscribe ack over loopback TCP");
    sub_conn.set_read_timeout(Some(Duration::from_secs(5)));

    let created: Pane = tcp_client
        .request(DaemonRequest::CreatePane {
            title: Some("remote-shell".to_string()),
            profile: None,
        })
        .expect("create pane over loopback TCP");
    assert_eq!(created.title, "remote-shell");

    let mut saw_pane_created = false;
    for _ in 0..20 {
        if let Ok(Some(event)) = sub_conn.read_event() {
            if matches!(event, DaemonEvent::PaneCreated { .. }) {
                saw_pane_created = true;
                break;
            }
        }
    }
    assert!(
        saw_pane_created,
        "subscriber over loopback TCP should receive PaneCreated"
    );

    // 3. Revoking the credential immediately blocks subsequent LoopbackTcp connects.
    let _: Value = local_client
        .request(DaemonRequest::IdentityRevoke { id: cred_id })
        .expect("revoke credential");
    assert!(
        tcp_client.request::<Value>(DaemonRequest::Whoami).is_err(),
        "revoked credential must be rejected over loopback TCP"
    );

    let port_file = daemon
        .socket_path
        .parent()
        .unwrap()
        .join(LOOPBACK_PORT_FILE);
    assert!(port_file.exists());
    daemon.shutdown();
    assert!(
        !port_file.exists(),
        "daemon.loopback_port should be unlinked on clean shutdown"
    );
}

#[cfg(unix)]
fn spawn_test_unix_to_tcp_bridge(
    req_path: PathBuf,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    use std::os::unix::net::UnixListener;

    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            if let Ok(spec) = fs::read_to_string(&req_path) {
                let spec = spec.trim().to_string();
                if let Some((sock_str, tcp_str)) = spec.split_once(':') {
                    let _ = fs::remove_file(&req_path);
                    let sock_path = PathBuf::from(sock_str);
                    let _ = fs::remove_file(&sock_path);
                    let listener = UnixListener::bind(&sock_path).expect("bind forwarded sock");
                    listener.set_nonblocking(true).unwrap();
                    while !stop.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((unix_stream, _)) => {
                                // Accepted BSD/macOS Unix sockets inherit O_NONBLOCK from
                                // the listener; restore blocking mode for `std::io::copy`.
                                let _ = unix_stream.set_nonblocking(false);
                                let target = tcp_str.to_string();
                                thread::spawn(move || {
                                    let Ok(tcp_stream) = std::net::TcpStream::connect(&target)
                                    else {
                                        return;
                                    };
                                    let _ = tcp_stream.set_nodelay(true);
                                    let Ok(mut u_read) = unix_stream.try_clone() else {
                                        return;
                                    };
                                    let mut u_write = unix_stream;
                                    let Ok(mut t_read) = tcp_stream.try_clone() else {
                                        return;
                                    };
                                    let mut t_write = tcp_stream;
                                    let t1 = thread::spawn(move || {
                                        let _ = std::io::copy(&mut u_read, &mut t_write);
                                        let _ = t_write.shutdown(std::net::Shutdown::Both);
                                    });
                                    let _ = std::io::copy(&mut t_read, &mut u_write);
                                    let _ = u_write.shutdown(std::net::Shutdown::Both);
                                    let _ = t1.join();
                                });
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(15));
                            }
                            Err(_) => break,
                        }
                    }
                    return;
                }
            }
            thread::sleep(Duration::from_millis(15));
        }
    })
}

#[cfg(unix)]
#[test]
fn ssh_tunnel_manager_attaches_to_remote_windows_loopback_daemon_and_reuses_tunnel() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        remote_loopback_port: Some(0),
        ..Default::default()
    });
    let local_admin = daemon.client();
    let loopback_port = native_ipc_endpoint(&local_admin)
        .expect("endpoint")
        .loopback_port
        .expect("loopback_port");

    let issued: Value = local_admin
        .request(DaemonRequest::IdentityIssue {
            holder: "laptop-op".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
        })
        .expect("issue credential");
    let client_token = issued["token"].as_str().unwrap().to_string();

    // Hermetic fake `ssh` script in a short `/tmp` directory (Unix socket paths have
    // `SUN_LEN` ~104 bytes on macOS).
    let root = tempfile::tempdir_in("/tmp").expect("short temp dir");
    let app_dir = root.path().join("app");
    let ssh_log = root.path().join("ssh.log");
    let endpoint_json_path = root.path().join("endpoint.json");
    let tunnel_req_path = root.path().join("tunnel.req");
    let keepalive_path = root.path().join("keepalive");
    fs::write(&keepalive_path, "1").unwrap();

    // Simulate a remote Windows host (#116) advertising `named_pipe` + `loopback_port`.
    fs::write(
        &endpoint_json_path,
        serde_json::to_vec(&json!({
            "transport": "named_pipe",
            "endpoint": "\\\\.\\pipe\\sgian2-S-1-5-21-test-0001",
            "token_path": "C:\\Users\\op\\AppData\\Roaming\\Sgian\\workspaces\\w\\daemon.token",
            "workspace": "C:\\Users\\op\\repo",
            "workspace_key": "repo-0001",
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": ["subscribe-ack"],
            "loopback_port": loopback_port,
        }))
        .unwrap(),
    )
    .unwrap();

    let fake_ssh = root.path().join("fake-ssh.sh");
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> \"{}\"\n\
         case \"$*\" in\n\
           *ipc-endpoint*)\n\
             cat \"{}\"\n\
             exit 0\n\
             ;;\n\
         esac\n\
         prev=\"\"\n\
         spec=\"\"\n\
         for arg in \"$@\"; do\n\
           if [ \"$prev\" = \"-L\" ]; then spec=\"$arg\"; break; fi\n\
           prev=\"$arg\"\n\
         done\n\
         printf '%s' \"$spec\" > \"{}.tmp\"\n\
         mv \"{}.tmp\" \"{}\"\n\
         while [ -f \"{}\" ]; do sleep 0.05; done\n",
        ssh_log.display(),
        endpoint_json_path.display(),
        tunnel_req_path.display(),
        tunnel_req_path.display(),
        tunnel_req_path.display(),
        keepalive_path.display(),
    );
    fs::write(&fake_ssh, script).unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700)).unwrap();

    // Background thread acting as OpenSSH's `-L <unix_sock>:127.0.0.1:<port>` forwarder
    // once `fake-ssh.sh` writes `tunnel.req`.
    let stop_bridge = Arc::new(AtomicBool::new(false));
    let bridge_thread =
        spawn_test_unix_to_tcp_bridge(tunnel_req_path.clone(), Arc::clone(&stop_bridge));

    // Save `winbox` WITHOUT a token first: `ensure_machine_client_with_ssh_in` must
    // fail closed with an actionable authentication error rather than hanging.
    let add_no_token = parse_machine_args(&[
        "add".into(),
        "winbox".into(),
        "--host".into(),
        "dev@win.example".into(),
        "--workspace".into(),
        "C:\\Users\\op\\repo".into(),
    ])
    .unwrap();
    control_machine_in(&app_dir, add_no_token, std::io::empty(), true).unwrap();

    let auth_err =
        ensure_machine_client_with_ssh_in(&app_dir, "winbox", Some(fake_ssh.to_str().unwrap()))
            .err()
            .expect("missing client_token on loopback target must fail authentication");
    assert!(
        auth_err.contains("authentication") || auth_err.contains("identity issue"),
        "expected actionable auth guidance, got: {auth_err}"
    );

    // Restart bridge watcher for the authenticated tunnel run.
    stop_bridge.store(true, Ordering::SeqCst);
    let _ = bridge_thread.join();
    let _ = fs::remove_file(&ssh_log);
    let _ = fs::remove_file(&tunnel_req_path);
    stop_bridge.store(false, Ordering::SeqCst);
    let bridge_thread2 =
        spawn_test_unix_to_tcp_bridge(tunnel_req_path.clone(), Arc::clone(&stop_bridge));

    // Configure the issued per-client credential and attach!
    write_machine_token_in(&app_dir, "winbox", &client_token).unwrap();
    let (remote_client, local_endpoint) =
        ensure_machine_client_with_ssh_in(&app_dir, "winbox", Some(fake_ssh.to_str().unwrap()))
            .expect("tunnel should come up and authenticate");
    assert_eq!(local_endpoint.transport, "unix_socket");
    assert_eq!(local_endpoint.workspace_key, "machine:winbox");

    let whoami: Value = remote_client
        .request(DaemonRequest::Whoami)
        .expect("whoami through ssh tunnel");
    assert_eq!(whoami["holder"], json!("laptop-op"));
    assert_eq!(effective_holder(&remote_client), "laptop-op");

    let panes: PaneList = remote_client
        .request(DaemonRequest::ListPanes)
        .expect("list panes through ssh tunnel");
    assert_eq!(panes.panes.len(), 1);

    let log_after_first = fs::read_to_string(&ssh_log).unwrap();
    let invocations_first = log_after_first.lines().count();
    assert_eq!(
        invocations_first, 2,
        "expected 1 discovery + 1 tunnel spawn, got:\n{log_after_first}"
    );

    // Second attach reuses the live tunnel socket without spawning `ssh` again!
    let (reused_client, _) =
        ensure_machine_client_with_ssh_in(&app_dir, "winbox", Some(fake_ssh.to_str().unwrap()))
            .expect("second attach should reuse live tunnel");
    let ok: CommandOk = reused_client.request(DaemonRequest::Ping).unwrap();
    assert!(ok.ok);
    let log_after_second = fs::read_to_string(&ssh_log).unwrap();
    assert_eq!(
        log_after_second.lines().count(),
        invocations_first,
        "live tunnel should be reused with zero extra ssh invocations"
    );

    // Teardown.
    let _ = fs::remove_file(&keepalive_path);
    stop_bridge.store(true, Ordering::SeqCst);
    let _ = control_machine_in(
        &app_dir,
        MachineCommand::Remove {
            name: "winbox".to_string(),
        },
        std::io::empty(),
        true,
    );
    let _ = bridge_thread2.join();
    daemon.shutdown();
}

#[test]
fn gui_startup_target_resolution_covers_flags_env_smoke_and_conflicts() {
    let default_ws = PathBuf::from("/work/default");

    let (local, target, err) =
        resolve_gui_startup_target_from(&["sgian".into()], None, default_ws.clone(), false);
    assert_eq!(local, default_ws);
    assert_eq!(target, WorkspaceTarget::Local(default_ws.clone()));
    assert!(err.is_none());

    let (local, target, err) = resolve_gui_startup_target_from(
        &["sgian".into(), "--machine".into(), "gpu-box".into()],
        None,
        default_ws.clone(),
        false,
    );
    assert_eq!(local, default_ws);
    assert_eq!(target, WorkspaceTarget::Machine("gpu-box".to_string()));
    assert!(err.is_none());

    let (local, target, err) = resolve_gui_startup_target_from(
        &["sgian".into(), "--workspace=/work/other".into()],
        Some("gpu-box"),
        default_ws.clone(),
        false,
    );
    assert_eq!(local, PathBuf::from("/work/other"));
    assert_eq!(target, WorkspaceTarget::Local(PathBuf::from("/work/other")));
    assert!(err.is_none());

    // SGIAN_MACHINE applies when no CLI target is given, EXCEPT under UI smoke!
    let (_, target, err) = resolve_gui_startup_target_from(
        &["sgian".into()],
        Some("gpu-box"),
        default_ws.clone(),
        false,
    );
    assert_eq!(target, WorkspaceTarget::Machine("gpu-box".to_string()));
    assert!(err.is_none());

    let (_, smoke_target, smoke_err) = resolve_gui_startup_target_from(
        &["sgian".into()],
        Some("gpu-box"),
        default_ws.clone(),
        true,
    );
    assert_eq!(smoke_target, WorkspaceTarget::Local(default_ws.clone()));
    assert!(smoke_err.is_none());

    let (_, _, conflict_err) = resolve_gui_startup_target_from(
        &[
            "sgian".into(),
            "--workspace".into(),
            "/work/other".into(),
            "-m".into(),
            "gpu-box".into(),
        ],
        None,
        default_ws,
        false,
    );
    assert!(conflict_err
        .as_deref()
        .unwrap_or("")
        .contains("cannot be used together"));
}

#[test]
fn machine_token_gui_helper_writes_clears_and_refuses_external_token_file() {
    let dir = tempfile::tempdir().unwrap();
    let app_dir = dir.path();

    let add_managed = parse_machine_args(&[
        "add".into(),
        "gpu".into(),
        "--host".into(),
        "op@gpu.example".into(),
        "--workspace".into(),
        "/srv/gpu".into(),
    ])
    .unwrap();
    control_machine_in(app_dir, add_managed, std::io::empty(), true).unwrap();

    let add_ext = parse_machine_args(&[
        "add".into(),
        "ext".into(),
        "--host".into(),
        "op@ext.example".into(),
        "--workspace".into(),
        "/srv/ext".into(),
        "--token-file".into(),
        "/etc/sgian/ext.tok".into(),
    ])
    .unwrap();
    control_machine_in(app_dir, add_ext, std::io::empty(), true).unwrap();

    let statuses = list_saved_machine_statuses_in(app_dir).unwrap();
    assert_eq!(statuses.len(), 2);
    assert!(
        !statuses
            .iter()
            .find(|s| s.name == "gpu")
            .unwrap()
            .token_configured
    );

    let issued_val = format!("sgc_{}", "c".repeat(64));
    set_or_clear_machine_token_in(app_dir, "gpu", Some(&issued_val)).unwrap();
    let statuses = list_saved_machine_statuses_in(app_dir).unwrap();
    assert!(
        statuses
            .iter()
            .find(|s| s.name == "gpu")
            .unwrap()
            .token_configured
    );

    set_or_clear_machine_token_in(app_dir, "gpu", Some("   ")).unwrap();
    let statuses = list_saved_machine_statuses_in(app_dir).unwrap();
    assert!(
        !statuses
            .iter()
            .find(|s| s.name == "gpu")
            .unwrap()
            .token_configured
    );

    let ext_err = set_or_clear_machine_token_in(app_dir, "ext", Some(&issued_val)).unwrap_err();
    assert!(ext_err.contains("external token_file"));

    let unknown_err =
        set_or_clear_machine_token_in(app_dir, "missing", Some(&issued_val)).unwrap_err();
    assert!(unknown_err.contains("unknown machine"));
}

#[test]
fn subscription_handle_cancel_wakes_parked_subscriber_and_exits_worker() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        ..Default::default()
    });
    let client = daemon.client();

    let received = Arc::new(AtomicUsize::new(0));
    let received_sink = Arc::clone(&received);
    let worker_alive = Arc::new(());
    let worker_alive_closure = Arc::clone(&worker_alive);
    let handle = client.start_subscription_with_sink(move |_event| {
        let _keep = &worker_alive_closure;
        received_sink.fetch_add(1, Ordering::SeqCst);
    });

    // Trigger panes until the subscriber is live and has observed at least 1 event.
    let event_deadline = Instant::now() + Duration::from_secs(3);
    let mut seq = 0usize;
    while received.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < event_deadline,
            "subscriber did not receive PaneCreated event"
        );
        seq += 1;
        let _: Value = client
            .request(DaemonRequest::CreatePane {
                title: Some(format!("sub-test-{seq}")),
                profile: None,
            })
            .unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(Arc::strong_count(&worker_alive), 2);

    // Cancelling shuts down the parked socket and exits the background worker thread,
    // dropping the closure (`strong_count == 1`).
    handle.cancel();
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    while Arc::strong_count(&worker_alive) > 1 {
        assert!(
            Instant::now() < exit_deadline,
            "subscription worker thread did not exit promptly after cancel()"
        );
        thread::sleep(Duration::from_millis(15));
    }

    let after_cancel = received.load(Ordering::SeqCst);
    let _: Value = client
        .request(DaemonRequest::CreatePane {
            title: Some("post-cancel".to_string()),
            profile: None,
        })
        .unwrap();
    thread::sleep(Duration::from_millis(40));
    assert_eq!(received.load(Ordering::SeqCst), after_cancel);
    daemon.shutdown();
}

#[test]
fn serve_http_hides_saved_machines_and_refuses_desktop_switch_commands() {
    assert_eq!(
        serve_http::frontend_command("list_saved_machines", &json!({}), "op@host").unwrap(),
        serve_http::FrontendCall::SavedMachinesEmpty
    );
    for cmd in [
        "connect_machine",
        "connect_local_workspace",
        "set_machine_client_token",
    ] {
        assert!(matches!(
            serve_http::frontend_command(cmd, &json!({}), "op@host").unwrap(),
            serve_http::FrontendCall::Unsupported(_)
        ));
    }
}

#[cfg(unix)]
#[test]
fn daemon_client_machine_reconnect_recovers_after_tunnel_drop() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        remote_loopback_port: Some(0),
        ..Default::default()
    });
    let local_admin = daemon.client();
    let loopback_port = native_ipc_endpoint(&local_admin)
        .expect("endpoint")
        .loopback_port
        .expect("loopback_port");

    let issued: Value = local_admin
        .request(DaemonRequest::IdentityIssue {
            holder: "reconnect-op".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
        })
        .expect("issue credential");
    let credential_val = issued["token"].as_str().unwrap().to_string();

    let root = tempfile::tempdir_in("/tmp").unwrap();
    let app_dir = root.path().join("app");
    fs::create_dir_all(&app_dir).unwrap();

    let ssh_log = root.path().join("ssh.log");
    let tunnel_req_path = root.path().join("tunnel.req");
    let keepalive_path = root.path().join("keepalive");
    fs::write(&keepalive_path, "1").unwrap();

    let endpoint_json_path = root.path().join("endpoint.json");
    fs::write(
        &endpoint_json_path,
        serde_json::to_vec(&json!({
            "transport": "named_pipe",
            "endpoint": "\\\\.\\pipe\\sgian2-reconnect-0001",
            "token_path": "C:\\Users\\op\\AppData\\Roaming\\Sgian\\workspaces\\w\\daemon.token",
            "workspace": "C:\\Users\\op\\repo",
            "workspace_key": "ws-reconnect",
            "protocol_version": PROTOCOL_VERSION,
            "capabilities": ["subscribe-ack"],
            "loopback_port": loopback_port,
        }))
        .unwrap(),
    )
    .unwrap();

    let fake_ssh = root.path().join("fake-ssh.sh");
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> \"{}\"\n\
         case \"$*\" in\n\
           *ipc-endpoint*)\n\
             cat \"{}\"\n\
             exit 0\n\
             ;;\n\
         esac\n\
         prev=\"\"\n\
         spec=\"\"\n\
         for arg in \"$@\"; do\n\
           if [ \"$prev\" = \"-L\" ]; then spec=\"$arg\"; break; fi\n\
           prev=\"$arg\"\n\
         done\n\
         printf '%s' \"$spec\" > \"{}.tmp\"\n\
         mv \"{}.tmp\" \"{}\"\n\
         while [ -f \"{}\" ]; do sleep 0.05; done\n",
        ssh_log.display(),
        endpoint_json_path.display(),
        tunnel_req_path.display(),
        tunnel_req_path.display(),
        tunnel_req_path.display(),
        keepalive_path.display(),
    );
    fs::write(&fake_ssh, script).unwrap();
    fs::set_permissions(&fake_ssh, fs::Permissions::from_mode(0o700)).unwrap();

    let add_cmd = parse_machine_args(&[
        "add".into(),
        "reconn".into(),
        "--host".into(),
        "dev@reconn.example".into(),
        "--workspace".into(),
        "/srv/reconn".into(),
    ])
    .unwrap();
    control_machine_in(&app_dir, add_cmd, std::io::empty(), true).unwrap();
    write_machine_token_in(&app_dir, "reconn", &credential_val).unwrap();

    let stop_bridge1 = Arc::new(AtomicBool::new(false));
    let bridge1 = spawn_test_unix_to_tcp_bridge(tunnel_req_path.clone(), Arc::clone(&stop_bridge1));

    let (remote_client, local_ep) =
        ensure_machine_client_with_ssh_in(&app_dir, "reconn", Some(fake_ssh.to_str().unwrap()))
            .expect("initial tunnel should connect");
    let ok: CommandOk = remote_client.request(DaemonRequest::Ping).unwrap();
    assert!(ok.ok);

    // Simulate a dropped SSH tunnel: stop the first bridge, remove the forwarded
    // unix socket, terminate the first fake-ssh process, and arm a fresh bridge watcher
    // for the re-spawned tunnel.
    stop_bridge1.store(true, Ordering::SeqCst);
    let _ = bridge1.join();
    let _ = fs::remove_file(&keepalive_path);
    thread::sleep(Duration::from_millis(100));
    let _ = fs::remove_file(&local_ep.endpoint);
    let _ = fs::remove_file(&tunnel_req_path);
    fs::write(&keepalive_path, "1").unwrap();

    let stop_bridge2 = Arc::new(AtomicBool::new(false));
    let bridge2 = spawn_test_unix_to_tcp_bridge(tunnel_req_path.clone(), Arc::clone(&stop_bridge2));

    // `remote_client.request(Ping)` must automatically invoke `machine_reconnect`
    // and succeed over the newly spawned tunnel!
    let ok2: CommandOk = remote_client
        .request(DaemonRequest::Ping)
        .expect("DaemonClient with machine_reconnect should auto-recover dropped SSH tunnel");
    assert!(ok2.ok);

    let log_lines = fs::read_to_string(&ssh_log).unwrap();
    assert!(
        log_lines.lines().count() >= 4,
        "expected second discovery + tunnel spawn on reconnect, got:\n{log_lines}"
    );

    let _ = fs::remove_file(&keepalive_path);
    stop_bridge2.store(true, Ordering::SeqCst);
    let _ = bridge2.join();
    daemon.shutdown();
}
