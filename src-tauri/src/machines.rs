use super::*;

pub(crate) const MACHINES_DIR: &str = "machines";
pub(crate) const TUNNEL_LOCK_FILE: &str = "tunnel.lock";
pub(crate) const TUNNEL_PID_FILE: &str = "tunnel.pid";
pub(crate) const TUNNEL_LOG_FILE: &str = "tunnel.log";
#[cfg(windows)]
pub(crate) const TUNNEL_PORT_FILE: &str = "tunnel.port";
pub(crate) const GLOBAL_CONFIG_LOCK_FILE: &str = "config.lock";
pub(crate) const MAX_MACHINES: usize = 64;
const TUNNEL_LOCK_WAIT: Duration = Duration::from_secs(15);
const TUNNEL_READY_RETRIES: usize = 100;
const TUNNEL_READY_POLL: Duration = Duration::from_millis(50);

fn default_machine_transport() -> String {
    "ssh".to_string()
}

/// A saved remote workspace target stored in the user's global `config.json`
/// (`docs/design/agent-steering-and-machines.md` §3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MachineEntry {
    pub(crate) name: String,
    #[serde(default = "default_machine_transport")]
    pub(crate) transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port: Option<u16>,
    pub(crate) workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) token_file: Option<String>,
}

pub(crate) fn validate_machine_name(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("machine name must be non-empty".to_string());
    }
    if raw != raw.trim() {
        return Err(format!(
            "machine name '{raw}' must not have leading or trailing whitespace"
        ));
    }
    if raw.len() > 64 {
        return Err("machine name exceeds maximum length (64 characters)".to_string());
    }
    if raw.starts_with('-') || raw.starts_with('.') {
        return Err(format!(
            "machine name '{raw}' must not start with '-' or '.'"
        ));
    }
    if raw.eq_ignore_ascii_case("local") {
        return Err("machine name 'local' is reserved for the local workspace".to_string());
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "invalid machine name '{raw}': use only ASCII letters, digits, '.', '_', or '-'"
        ));
    }
    Ok(raw.to_string())
}

fn has_shell_metachar(value: &str) -> bool {
    value.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '$' | '`' | '"' | '\'' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
            )
    })
}

impl MachineEntry {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let name = validate_machine_name(&self.name)?;
        match self.transport.as_str() {
            "ssh" => {
                let host = self.host.as_deref().ok_or_else(|| {
                    format!("machine '{name}' with transport 'ssh' requires host")
                })?;
                if host.is_empty() || host != host.trim() {
                    return Err(format!(
                        "machine '{name}' host must be non-empty without surrounding whitespace"
                    ));
                }
                if host.len() > 512 {
                    return Err(format!("machine '{name}' host exceeds maximum length"));
                }
                if host.starts_with('-') {
                    return Err(format!("machine '{name}' host must not start with '-'"));
                }
                if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!(
                        "machine '{name}' host must not contain whitespace or control characters"
                    ));
                }
                if self.port == Some(0) {
                    return Err(format!("machine '{name}' port must be between 1 and 65535"));
                }
                if self.target.is_some() {
                    return Err(format!(
                        "machine '{name}' sets 'target', which applies only to transport 'ssm'"
                    ));
                }
            }
            "ssm" => {
                let target = self.target.as_deref().ok_or_else(|| {
                    format!("machine '{name}' with transport 'ssm' requires target")
                })?;
                if target.is_empty() || target != target.trim() {
                    return Err(format!(
                        "machine '{name}' target must be non-empty without surrounding whitespace"
                    ));
                }
                if target.len() > 512 {
                    return Err(format!("machine '{name}' target exceeds maximum length"));
                }
                if target.starts_with('-') {
                    return Err(format!("machine '{name}' target must not start with '-'"));
                }
                if target.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!(
                        "machine '{name}' target must not contain whitespace or control characters"
                    ));
                }
                if self.port.is_some() {
                    return Err(format!(
                        "machine '{name}' sets 'port', which applies only to transport 'ssh'"
                    ));
                }
            }
            other => {
                return Err(format!(
                    "invalid machine transport '{other}' for '{name}': must be 'ssh' or 'ssm'"
                ));
            }
        }

        let workspace = self.workspace.as_str();
        if workspace.is_empty() || workspace != workspace.trim() {
            return Err(format!(
                "machine '{name}' workspace must be non-empty without surrounding whitespace"
            ));
        }
        if workspace.len() > 4096 {
            return Err(format!(
                "machine '{name}' workspace exceeds maximum length (4096 bytes)"
            ));
        }
        if workspace.starts_with('-') {
            return Err(format!(
                "machine '{name}' workspace must not start with '-'"
            ));
        }
        if has_shell_metachar(workspace) {
            return Err(format!(
                "machine '{name}' workspace contains unsupported control or shell metacharacters"
            ));
        }

        if let Some(ref tf) = self.token_file {
            if tf.is_empty()
                || tf != tf.trim()
                || tf.len() > 4096
                || tf.chars().any(char::is_control)
            {
                return Err(format!("machine '{name}' has an invalid token_file path"));
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_machines(entries: &[MachineEntry]) -> Result<(), String> {
    if entries.len() > MAX_MACHINES {
        return Err(format!(
            "machines list exceeds maximum of {MAX_MACHINES} entries"
        ));
    }
    let mut seen = HashSet::with_capacity(entries.len());
    for entry in entries {
        entry.validate()?;
        if !seen.insert(entry.name.as_str()) {
            return Err(format!("duplicate machine name '{}'", entry.name));
        }
    }
    Ok(())
}

/// Root directory holding the user's global `config.json`, `machines/` token
/// files, and `runtime/machines/` SSH tunnel sockets. Honors
/// `SGIAN_MACHINES_HOME` so CLI and GUI integration tests remain hermetic and
/// never touch the developer's real `~/Library/Application Support/Sgian`.
pub(crate) fn machines_support_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("SGIAN_MACHINES_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return override_dir;
    }
    app_support_dir()
}

pub(crate) fn machine_default_token_path_in(app_dir: &Path, name: &str) -> PathBuf {
    app_dir.join(MACHINES_DIR).join(format!("{name}.token"))
}

pub(crate) fn machine_runtime_dir_in(app_dir: &Path, name: &str) -> PathBuf {
    app_dir.join(RUNTIME_DIR).join(MACHINES_DIR).join(name)
}

pub(crate) fn load_global_machines_in(app_dir: &Path) -> Result<Vec<MachineEntry>, String> {
    let config_path = app_dir.join(CONFIG_FILE);
    match read_config_file(&config_path)? {
        Some(config) => Ok(config.machines),
        None => Ok(Vec::new()),
    }
}

pub(crate) fn load_global_machines() -> Result<Vec<MachineEntry>, String> {
    load_global_machines_in(&machines_support_dir())
}

fn acquire_advisory_lock_bounded(lock_path: &Path, wait: Duration) -> Result<File, String> {
    if let Some(parent) = lock_path.parent() {
        ensure_private_dir(parent)?;
    }
    let file = open_lock_file(lock_path)?;
    let deadline = Instant::now() + wait;
    loop {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(fs4::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out waiting for lock {}",
                        lock_path.display()
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(fs4::TryLockError::Error(error)) => {
                return Err(format!(
                    "failed to acquire lock {}: {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

pub(crate) fn save_global_machines_in(
    app_dir: &Path,
    machines: Vec<MachineEntry>,
) -> Result<(), String> {
    validate_machines(&machines)?;
    ensure_private_dir(app_dir)?;
    let _lock = acquire_advisory_lock_bounded(
        &app_dir.join(GLOBAL_CONFIG_LOCK_FILE),
        Duration::from_secs(5),
    )?;
    let config_path = app_dir.join(CONFIG_FILE);
    let mut config = read_config_file(&config_path)?.unwrap_or_default();
    config.machines = machines;
    config.validate()?;
    let encoded = serde_json::to_vec_pretty(&config)
        .map_err(|error| format!("failed to serialize global config: {error}"))?;
    let temp_path = config_path.with_extension("json.tmp");
    write_file_atomic(&temp_path, &config_path, &encoded, true)
}

pub(crate) fn write_machine_token_in(
    app_dir: &Path,
    name: &str,
    raw_token: &str,
) -> Result<PathBuf, String> {
    let name = validate_machine_name(name)?;
    let token = raw_token.trim();
    if token.is_empty() {
        return Err("machine client token must be non-empty".to_string());
    }
    if token.len() > 4096 || token.chars().any(char::is_control) {
        return Err("machine client token contains invalid characters".to_string());
    }
    let dir = app_dir.join(MACHINES_DIR);
    ensure_private_dir(&dir)?;
    let final_path = machine_default_token_path_in(app_dir, &name);
    if let Ok(meta) = fs::symlink_metadata(&final_path) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "refusing to overwrite symlinked token file {}",
                final_path.display()
            ));
        }
    }
    let temp_path = final_path.with_extension("token.tmp");
    let mut payload = token.as_bytes().to_vec();
    payload.push(b'\n');
    write_file_atomic(&temp_path, &final_path, &payload, true)?;
    set_private_file_permissions(&final_path)?;
    Ok(final_path)
}

pub(crate) fn resolve_machine_client_token_in(
    app_dir: &Path,
    entry: &MachineEntry,
) -> Result<(Option<String>, PathBuf), String> {
    let default_path = machine_default_token_path_in(app_dir, &entry.name);
    if let Some(env_token) = client_token_from_env() {
        let env_path = std::env::var_os("SGIAN_CLIENT_TOKEN_FILE")
            .map(PathBuf::from)
            .unwrap_or(default_path);
        return Ok((Some(env_token), env_path));
    }
    if let Some(ref custom) = entry.token_file {
        let custom_path = PathBuf::from(custom);
        let token = read_token(&custom_path)?.ok_or_else(|| {
            format!(
                "configured token_file for machine '{}' is missing or empty: {}",
                entry.name,
                custom_path.display()
            )
        })?;
        return Ok((Some(token), custom_path));
    }
    if let Some(token) = read_token(&default_path)? {
        return Ok((Some(token), default_path));
    }
    Ok((None, default_path))
}

#[derive(Debug, Deserialize)]
struct RemoteEndpointDiscovery {
    transport: String,
    endpoint: String,
    protocol_version: u32,
    #[serde(default)]
    loopback_port: Option<u16>,
}

fn resolve_ssh_bin() -> String {
    std::env::var("SGIAN_SSH_BIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ssh".to_string())
}

fn extract_first_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end >= start).then_some(&raw[start..=end])
}

fn discover_remote_forward_target(entry: &MachineEntry, ssh_bin: &str) -> Result<String, String> {
    let host = entry
        .host
        .as_deref()
        .ok_or_else(|| format!("machine '{}' has no configured host", entry.name))?;
    let mut cmd = Command::new(ssh_bin);
    cmd.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    if let Some(port) = entry.port {
        cmd.arg("-p").arg(port.to_string());
    }
    let remote_command = format!(
        "sgian ctl --workspace \"{}\" --json ipc-endpoint",
        entry.workspace
    );
    cmd.arg("--").arg(host).arg(remote_command);
    let output = run_subprocess_bounded(&mut cmd, Duration::from_secs(25)).map_err(|error| {
        format!(
            "failed to run '{ssh_bin}' for machine '{}' ({host}): {error}",
            entry.name
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            stderr
        };
        return Err(format!(
            "remote endpoint discovery failed for machine '{}' ({host}, {}): {}",
            entry.name,
            output.status,
            if detail.is_empty() {
                "no diagnostic output"
            } else {
                detail.as_str()
            }
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_slice = extract_first_json_object(&stdout).ok_or_else(|| {
        format!("remote `sgian ctl --json ipc-endpoint` on '{host}' returned non-JSON output")
    })?;
    let discovery: RemoteEndpointDiscovery = serde_json::from_str(json_slice).map_err(|error| {
        format!(
            "invalid ipc-endpoint JSON from machine '{}' ({host}): {error}",
            entry.name
        )
    })?;
    if discovery.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch on machine '{}': remote={}, local={PROTOCOL_VERSION}",
            entry.name, discovery.protocol_version
        ));
    }
    match discovery.transport.as_str() {
        "unix_socket" => {
            let remote_sock = discovery.endpoint.trim();
            if !remote_sock.starts_with('/')
                || remote_sock
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(format!(
                    "remote machine '{}' reported an invalid unix socket path: {}",
                    entry.name, discovery.endpoint
                ));
            }
            Ok(remote_sock.to_string())
        }
        "named_pipe" => match discovery.loopback_port.filter(|port| *port > 0) {
            Some(port) => Ok(format!("127.0.0.1:{port}")),
            None => Err(format!(
                "remote workspace '{}' on '{}' uses Windows named pipes and has not enabled \
                 remote loopback attach; set \"remote_loopback_port\": 0 in config.json on the \
                 Windows host and restart its daemon",
                entry.workspace, host
            )),
        },
        other => Err(format!(
            "remote machine '{}' reported unsupported IPC transport '{other}'",
            entry.name
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MachineReconnectSpec {
    pub(crate) app_dir: PathBuf,
    pub(crate) machine_name: String,
    pub(crate) ssh_bin_override: Option<String>,
}

impl MachineReconnectSpec {
    pub(crate) fn reconnect(&self) -> Result<DaemonClient, String> {
        let (client, _) = ensure_machine_client_with_ssh_in(
            &self.app_dir,
            &self.machine_name,
            self.ssh_bin_override.as_deref(),
        )?;
        Ok(client)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SavedMachineStatus {
    pub name: String,
    pub transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
    pub token_configured: bool,
    pub tunnel_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTargetsSnapshot {
    pub local_workspace: String,
    pub active_machine: Option<String>,
    pub credential_from_env: bool,
    pub machines: Vec<SavedMachineStatus>,
}

pub(crate) fn list_saved_machine_statuses_in(
    app_dir: &Path,
) -> Result<Vec<SavedMachineStatus>, String> {
    let entries = load_global_machines_in(app_dir)?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let token_path = entry
                .token_file
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| machine_default_token_path_in(app_dir, &entry.name));
            let token_configured = read_token(&token_path).ok().flatten().is_some();
            let runtime_dir = machine_runtime_dir_in(app_dir, &entry.name);
            #[cfg(unix)]
            let tunnel_present = runtime_dir.join(SOCKET_FILE).exists();
            #[cfg(windows)]
            let tunnel_present = runtime_dir.join(TUNNEL_PORT_FILE).exists();
            SavedMachineStatus {
                name: entry.name,
                transport: entry.transport,
                host: entry.host,
                port: entry.port,
                workspace: entry.workspace,
                target: entry.target,
                token_file: entry.token_file,
                token_configured,
                tunnel_present,
            }
        })
        .collect())
}

pub(crate) fn set_or_clear_machine_token_in(
    app_dir: &Path,
    machine_name: &str,
    token: Option<&str>,
) -> Result<(), String> {
    let name = validate_machine_name(machine_name)?;
    let machines = load_global_machines_in(app_dir)?;
    let entry = machines
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| format!("unknown machine '{name}'"))?;
    if entry.token_file.is_some() {
        return Err(format!(
            "machine '{name}' uses an external token_file; update that file directly or re-add without --token-file"
        ));
    }
    let trimmed = token.map(str::trim).filter(|t| !t.is_empty());
    if let Some(secret) = trimmed {
        write_machine_token_in(app_dir, &name, secret).map(|_| ())
    } else {
        let path = machine_default_token_path_in(app_dir, &name);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                return Err(format!(
                    "refusing to remove directory at token path {}",
                    path.display()
                ));
            }
            if let Err(error) = fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(format!("failed to remove {}: {error}", path.display()));
                }
            }
        }
        Ok(())
    }
}

fn build_machine_client_and_endpoint(
    entry: &MachineEntry,
    runtime_dir: &Path,
    local_socket_path: PathBuf,
    tcp_endpoint: Option<std::net::SocketAddr>,
    client_token: Option<String>,
    token_path: PathBuf,
    reconnect: MachineReconnectSpec,
) -> (DaemonClient, NativeIpcEndpoint) {
    let (transport_name, endpoint_str) = match tcp_endpoint {
        Some(addr) => ("tcp_loopback".to_string(), addr.to_string()),
        None => (
            "unix_socket".to_string(),
            local_socket_path.display().to_string(),
        ),
    };
    let endpoint = NativeIpcEndpoint {
        transport: transport_name,
        endpoint: endpoint_str,
        token_path: token_path.display().to_string(),
        workspace: entry.workspace.clone(),
        workspace_key: format!("machine:{}", entry.name),
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec!["subscribe-ack".to_string()],
        loopback_port: None,
    };
    let client = DaemonClient {
        cwd: PathBuf::from(&entry.workspace),
        socket_path: local_socket_path,
        data_dir: runtime_dir.to_path_buf(),
        token: String::new(),
        auto_spawn: false,
        client_token,
        tcp_endpoint,
        machine_reconnect: Some(reconnect),
    };
    (client, endpoint)
}

enum TunnelProbe {
    Ready,
    NotReady(String),
    AuthFailed(String),
}

fn probe_machine_client(client: &DaemonClient, machine_name: &str) -> TunnelProbe {
    match client.raw_request_with_timeout(DaemonRequest::Ping, Some(Duration::from_secs(3))) {
        Ok(resp) if resp.ok => TunnelProbe::Ready,
        Ok(resp) => TunnelProbe::AuthFailed(format!(
            "machine '{machine_name}' rejected authentication: {}",
            resp.error.unwrap_or_else(|| "rejected".to_string())
        )),
        Err(error)
            if error.contains("authentication")
                || error.contains("client_token")
                || error.contains("credential") =>
        {
            TunnelProbe::AuthFailed(format!(
                "machine '{machine_name}' authentication failed: {error} \
                 (issue a credential on the remote host with `sgian ctl identity issue --holder NAME` \
                 and configure it via `sgian ctl machine add {machine_name} ... --token-stdin`)"
            ))
        }
        Err(error) => TunnelProbe::NotReady(error),
    }
}

/// Ensure a live, authenticated client-owned SSH tunnel exists for `machine_name`
/// and return both a ready `DaemonClient` and the local `NativeIpcEndpoint`.
pub(crate) fn ensure_machine_client_in(
    app_dir: &Path,
    machine_name: &str,
) -> Result<(DaemonClient, NativeIpcEndpoint), String> {
    ensure_machine_client_with_ssh_in(app_dir, machine_name, None)
}

pub(crate) fn ensure_machine_client_with_ssh_in(
    app_dir: &Path,
    machine_name: &str,
    ssh_bin_override: Option<&str>,
) -> Result<(DaemonClient, NativeIpcEndpoint), String> {
    let name = validate_machine_name(machine_name)?;
    let machines = load_global_machines_in(app_dir)?;
    let entry = machines
        .into_iter()
        .find(|m| m.name == name)
        .ok_or_else(|| {
            format!(
                "unknown machine '{name}' (run `sgian ctl machine list` to inspect saved machines)"
            )
        })?;

    if entry.transport == "ssm" {
        return Err(format!(
            "machine '{name}' uses transport 'ssm', which is not yet supported in this build \
             (tracked in issue #115)"
        ));
    }

    let (client_token, token_path) = resolve_machine_client_token_in(app_dir, &entry)?;
    let runtime_dir = machine_runtime_dir_in(app_dir, &name);
    ensure_private_dir(&runtime_dir)?;
    let _lock =
        acquire_advisory_lock_bounded(&runtime_dir.join(TUNNEL_LOCK_FILE), TUNNEL_LOCK_WAIT)?;

    let reconnect_spec = MachineReconnectSpec {
        app_dir: app_dir.to_path_buf(),
        machine_name: name.clone(),
        ssh_bin_override: ssh_bin_override.map(str::to_string),
    };

    let local_sock = runtime_dir.join(SOCKET_FILE);

    // 1. Reuse an already-live tunnel if it responds to an authenticated Ping.
    #[cfg(unix)]
    if local_sock.exists() {
        let (candidate, endpoint) = build_machine_client_and_endpoint(
            &entry,
            &runtime_dir,
            local_sock.clone(),
            None,
            client_token.clone(),
            token_path.clone(),
            reconnect_spec.clone(),
        );
        match probe_machine_client(&candidate, &name) {
            TunnelProbe::Ready => return Ok((candidate, endpoint)),
            TunnelProbe::AuthFailed(err) => return Err(err),
            TunnelProbe::NotReady(_) => {}
        }
    }

    #[cfg(windows)]
    if let Ok(port_text) = fs::read_to_string(runtime_dir.join(TUNNEL_PORT_FILE)) {
        if let Ok(port) = port_text.trim().parse::<u16>() {
            if port > 0 {
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                let (candidate, endpoint) = build_machine_client_and_endpoint(
                    &entry,
                    &runtime_dir,
                    local_sock.clone(),
                    Some(addr),
                    client_token.clone(),
                    token_path.clone(),
                    reconnect_spec.clone(),
                );
                match probe_machine_client(&candidate, &name) {
                    TunnelProbe::Ready => return Ok((candidate, endpoint)),
                    TunnelProbe::AuthFailed(err) => return Err(err),
                    TunnelProbe::NotReady(_) => {}
                }
            }
        }
    }

    // 2. Discover remote daemon endpoint via SSH (`sgian ctl --json ipc-endpoint`).
    let ssh_bin = ssh_bin_override
        .map(str::to_string)
        .unwrap_or_else(resolve_ssh_bin);
    let remote_target = discover_remote_forward_target(&entry, &ssh_bin)?;
    let host = entry
        .host
        .as_deref()
        .ok_or_else(|| format!("machine '{name}' has no host"))?;

    #[cfg(unix)]
    let (local_bind, tcp_endpoint): (String, Option<std::net::SocketAddr>) = {
        terminate_previous_tunnel(&runtime_dir, &local_sock);
        remove_stale_socket(&local_sock)?;
        (local_sock.display().to_string(), None)
    };

    #[cfg(windows)]
    let (local_bind, tcp_endpoint): (String, Option<std::net::SocketAddr>) = {
        let reservation = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("failed to allocate local tunnel port: {error}"))?;
        let local_port = reservation
            .local_addr()
            .map_err(|error| format!("failed to read local tunnel port: {error}"))?
            .port();
        drop(reservation);
        let port_path = runtime_dir.join(TUNNEL_PORT_FILE);
        let temp_path = port_path.with_extension("port.tmp");
        write_file_atomic(
            &temp_path,
            &port_path,
            format!("{local_port}\n").as_bytes(),
            false,
        )?;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], local_port));
        (addr.to_string(), Some(addr))
    };

    let log_path = runtime_dir.join(TUNNEL_LOG_FILE);
    let log_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .private_mode()
        .open(&log_path)
        .map_err(|error| format!("failed to open tunnel log {}: {error}", log_path.display()))?;
    let _ = set_private_file_permissions(&log_path);

    let mut ssh_cmd = Command::new(&ssh_bin);
    ssh_cmd.args([
        "-N",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "StreamLocalBindUnlink=yes",
    ]);
    if let Some(port) = entry.port {
        ssh_cmd.arg("-p").arg(port.to_string());
    }
    ssh_cmd
        .arg("-L")
        .arg(format!("{local_bind}:{remote_target}"))
        .arg("--")
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log_file));
    configure_daemon_process(&mut ssh_cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: async-signal-safe calls in child before exec: tighten umask so
        // the forwarded Unix domain socket is created 0600, and detach session so
        // terminal signals on a single ctl invocation do not kill the shared tunnel.
        unsafe {
            ssh_cmd.pre_exec(|| {
                libc::umask(0o077);
                let _ = libc::setsid();
                Ok(())
            });
        }
    }

    let mut child = ssh_cmd.spawn().map_err(|error| {
        format!("failed to spawn SSH tunnel ('{ssh_bin}') for machine '{name}': {error}")
    })?;
    let pid_path = runtime_dir.join(TUNNEL_PID_FILE);
    let _ = write_file_atomic(
        &pid_path.with_extension("pid.tmp"),
        &pid_path,
        format!("{}\n", child.id()).as_bytes(),
        false,
    );

    let (candidate, endpoint) = build_machine_client_and_endpoint(
        &entry,
        &runtime_dir,
        local_sock.clone(),
        tcp_endpoint,
        client_token,
        token_path,
        reconnect_spec,
    );

    let mut last_error = "tunnel socket not yet ready".to_string();
    for _ in 0..TUNNEL_READY_RETRIES {
        match probe_machine_client(&candidate, &name) {
            TunnelProbe::Ready => {
                #[cfg(unix)]
                if local_sock.exists() {
                    let _ = set_private_file_permissions(&local_sock);
                }
                return Ok((candidate, endpoint));
            }
            TunnelProbe::NotReady(err) => {
                last_error = err;
            }
            TunnelProbe::AuthFailed(auth_error) => {
                let _ = child.kill();
                let _ = child.wait();
                #[cfg(unix)]
                let _ = remove_stale_socket(&local_sock);
                return Err(auth_error);
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let excerpt = daemon_startup_log_excerpt(&log_path)
                    .map(|text| format!(": {text}"))
                    .unwrap_or_default();
                return Err(format!(
                    "SSH tunnel for machine '{name}' exited prematurely ({status}){excerpt}"
                ));
            }
            Ok(None) => {}
            Err(error) => last_error = error.to_string(),
        }
        thread::sleep(TUNNEL_READY_POLL);
    }

    let _ = child.kill();
    let _ = child.wait();
    let excerpt = daemon_startup_log_excerpt(&log_path)
        .map(|text| format!("; ssh stderr: {text}"))
        .unwrap_or_default();
    Err(format!(
        "SSH tunnel for machine '{name}' did not become ready ({last_error}{excerpt})"
    ))
}

pub(crate) fn ensure_machine_client(
    machine_name: &str,
) -> Result<(DaemonClient, NativeIpcEndpoint), String> {
    ensure_machine_client_in(&machines_support_dir(), machine_name)
}

#[cfg(unix)]
fn terminate_previous_tunnel(runtime_dir: &Path, expected_sock: &Path) {
    let pid_path = runtime_dir.join(TUNNEL_PID_FILE);
    let Ok(pid_text) = fs::read_to_string(&pid_path) else {
        return;
    };
    let Ok(pid) = pid_text.trim().parse::<u32>() else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // Guard against PID reuse: only signal a process whose argv still names our
    // exact per-machine tunnel socket path.
    let mut ps = Command::new("ps");
    ps.args(["-p", &pid.to_string(), "-o", "args="]);
    if let Ok(output) = run_subprocess_bounded(&mut ps, Duration::from_secs(2)) {
        let cmdline = String::from_utf8_lossy(&output.stdout);
        let needle = expected_sock.display().to_string();
        if !needle.is_empty() && cmdline.contains(&needle) {
            unsafe {
                let _ = libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    let _ = fs::remove_file(&pid_path);
}

fn cleanup_machine_state_in(app_dir: &Path, name: &str) {
    let token_path = machine_default_token_path_in(app_dir, name);
    let _ = fs::remove_file(&token_path);
    let runtime_dir = machine_runtime_dir_in(app_dir, name);
    #[cfg(unix)]
    {
        let sock = runtime_dir.join(SOCKET_FILE);
        terminate_previous_tunnel(&runtime_dir, &sock);
        let _ = remove_stale_socket(&sock);
    }
    let _ = fs::remove_dir_all(&runtime_dir);
}

// ----- `sgian ctl machine ...` CLI parsing & execution -----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MachineCommand {
    List,
    Add {
        entry: MachineEntry,
        token_stdin: bool,
    },
    Remove {
        name: String,
    },
}

pub(crate) fn parse_machine_args(args: &[String]) -> Result<MachineCommand, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Err("machine requires a subcommand: add, list, or remove".to_string());
    };
    match sub {
        "list" | "ls" => {
            ensure_no_extra_args("machine list", &args[1..])?;
            Ok(MachineCommand::List)
        }
        "remove" | "rm" => {
            if args.len() != 2 {
                return Err("machine remove requires exactly one machine NAME".to_string());
            }
            let name = validate_machine_name(&args[1])?;
            Ok(MachineCommand::Remove { name })
        }
        "add" => {
            let mut name: Option<String> = None;
            let mut transport = "ssh".to_string();
            let mut host: Option<String> = None;
            let mut port: Option<u16> = None;
            let mut workspace: Option<String> = None;
            let mut target: Option<String> = None;
            let mut token_file: Option<String> = None;
            let mut token_stdin = false;

            let mut idx = 1;
            while idx < args.len() {
                let arg = args[idx].as_str();
                let next_val = |idx: usize, flag: &str| -> Result<String, String> {
                    args.get(idx + 1)
                        .cloned()
                        .ok_or_else(|| format!("{flag} requires a value"))
                };
                match arg {
                    "--transport" => {
                        transport = next_val(idx, "--transport")?;
                        idx += 2;
                    }
                    "--host" => {
                        if host.is_some() {
                            return Err("--host given more than once".to_string());
                        }
                        host = Some(next_val(idx, "--host")?);
                        idx += 2;
                    }
                    "--port" | "-p" => {
                        if port.is_some() {
                            return Err("--port given more than once".to_string());
                        }
                        let raw = next_val(idx, "--port")?;
                        let parsed = raw
                            .parse::<u16>()
                            .ok()
                            .filter(|p| *p > 0)
                            .ok_or_else(|| format!("invalid --port '{raw}'"))?;
                        port = Some(parsed);
                        idx += 2;
                    }
                    "--workspace" | "-w" => {
                        if workspace.is_some() {
                            return Err("--workspace given more than once".to_string());
                        }
                        workspace = Some(next_val(idx, "--workspace")?);
                        idx += 2;
                    }
                    "--target" => {
                        if target.is_some() {
                            return Err("--target given more than once".to_string());
                        }
                        target = Some(next_val(idx, "--target")?);
                        idx += 2;
                    }
                    "--token-file" => {
                        if token_file.is_some() {
                            return Err("--token-file given more than once".to_string());
                        }
                        token_file = Some(next_val(idx, "--token-file")?);
                        idx += 2;
                    }
                    "--token-stdin" => {
                        token_stdin = true;
                        idx += 1;
                    }
                    other if other.starts_with('-') => {
                        return Err(format!("unexpected option for machine add: {other}"));
                    }
                    positional => {
                        if name.is_some() {
                            return Err(format!(
                                "unexpected positional argument for machine add: {positional}"
                            ));
                        }
                        name = Some(positional.to_string());
                        idx += 1;
                    }
                }
            }
            if token_stdin && token_file.is_some() {
                return Err("--token-stdin and --token-file cannot be used together".to_string());
            }
            let name = name.ok_or_else(|| "machine add requires a NAME".to_string())?;
            let workspace =
                workspace.ok_or_else(|| "machine add requires --workspace <PATH>".to_string())?;
            let entry = MachineEntry {
                name,
                transport,
                host,
                port,
                workspace,
                target,
                token_file,
            };
            entry.validate()?;
            Ok(MachineCommand::Add { entry, token_stdin })
        }
        other => Err(format!(
            "unknown machine subcommand '{other}': expected add, list, or remove"
        )),
    }
}

pub(crate) fn control_machine_in<R: Read>(
    app_dir: &Path,
    command: MachineCommand,
    stdin: R,
    json_output: bool,
) -> Result<Value, String> {
    match command {
        MachineCommand::List => {
            let entries = load_global_machines_in(app_dir)?;
            let rows: Vec<Value> = entries
                .iter()
                .map(|entry| {
                    let has_token = entry
                        .token_file
                        .as_deref()
                        .map(|p| Path::new(p).exists())
                        .unwrap_or_else(|| {
                            machine_default_token_path_in(app_dir, &entry.name).exists()
                        });
                    let runtime_dir = machine_runtime_dir_in(app_dir, &entry.name);
                    #[cfg(unix)]
                    let tunnel_present = runtime_dir.join(SOCKET_FILE).exists();
                    #[cfg(windows)]
                    let tunnel_present = runtime_dir.join(TUNNEL_PORT_FILE).exists();
                    json!({
                        "name": entry.name,
                        "transport": entry.transport,
                        "host": entry.host,
                        "port": entry.port,
                        "workspace": entry.workspace,
                        "target": entry.target,
                        "token_file": entry.token_file,
                        "token_configured": has_token,
                        "tunnel_present": tunnel_present,
                    })
                })
                .collect();
            let _ = json_output;
            Ok(Value::Array(rows))
        }
        MachineCommand::Add { entry, token_stdin } => {
            if token_stdin {
                let mut buf = String::new();
                stdin
                    .take(4096)
                    .read_to_string(&mut buf)
                    .map_err(|error| format!("failed to read token from stdin: {error}"))?;
                let first_line = buf.lines().next().unwrap_or("").trim();
                if first_line.is_empty() {
                    return Err("--token-stdin received an empty token on stdin".to_string());
                }
                write_machine_token_in(app_dir, &entry.name, first_line)?;
            }
            let mut machines = load_global_machines_in(app_dir)?;
            if let Some(existing) = machines.iter_mut().find(|m| m.name == entry.name) {
                *existing = entry.clone();
            } else {
                machines.push(entry.clone());
            }
            save_global_machines_in(app_dir, machines)?;
            Ok(json!({
                "ok": true,
                "machine": entry,
            }))
        }
        MachineCommand::Remove { name } => {
            let mut machines = load_global_machines_in(app_dir)?;
            let before = machines.len();
            machines.retain(|m| m.name != name);
            if machines.len() == before {
                return Err(format!("unknown machine '{name}'"));
            }
            save_global_machines_in(app_dir, machines)?;
            cleanup_machine_state_in(app_dir, &name);
            Ok(json!({
                "ok": true,
                "removed": name,
            }))
        }
    }
}

pub(crate) fn control_machine(args: &[String], json_output: bool) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_machine_args(args)?;
    let is_list = matches!(parsed, MachineCommand::List);
    let value = control_machine_in(
        &machines_support_dir(),
        parsed,
        std::io::stdin(),
        json_output,
    )?;
    if json_output {
        return write_json_stdout(&value);
    }
    let mut stdout = std::io::stdout();
    if is_list {
        for row in value.as_array().into_iter().flatten() {
            let endpoint = row["host"]
                .as_str()
                .or_else(|| row["target"].as_str())
                .unwrap_or("-");
            let endpoint = match row["port"].as_u64() {
                Some(port) => format!("{endpoint}:{port}"),
                None => endpoint.to_string(),
            };
            let auth = if row["token_configured"].as_bool().unwrap_or(false) {
                "token"
            } else {
                "no-token"
            };
            let tunnel = if row["tunnel_present"].as_bool().unwrap_or(false) {
                "tunnel"
            } else {
                "idle"
            };
            writeln!(
                stdout,
                "{}\t{}\t{}\t{}\t{}\t{}",
                row["name"].as_str().unwrap_or(""),
                row["transport"].as_str().unwrap_or("ssh"),
                endpoint,
                row["workspace"].as_str().unwrap_or(""),
                auth,
                tunnel,
            )
            .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        return Ok(());
    }
    if let Some(removed) = value["removed"].as_str() {
        writeln!(stdout, "removed\t{removed}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    } else {
        writeln!(
            stdout,
            "saved\t{}\t{}",
            value["machine"]["name"].as_str().unwrap_or(""),
            value["machine"]["workspace"].as_str().unwrap_or("")
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Split `<machine>/<pane>` when `<machine>` matches a saved machine name in
/// `known_machines`. Enforces conflict checks against `--machine` and
/// `--workspace` so ambiguous target routing fails closed.
pub(crate) fn split_qualified_pane_ref(
    pane_ref: &str,
    current_machine: Option<&str>,
    explicit_workspace: bool,
    known_machines: &HashSet<String>,
) -> Result<(Option<String>, String), String> {
    let Some((prefix, rest)) = pane_ref.split_once('/') else {
        return Ok((current_machine.map(str::to_string), pane_ref.to_string()));
    };
    if prefix.is_empty() || rest.is_empty() || !known_machines.contains(prefix) {
        return Ok((current_machine.map(str::to_string), pane_ref.to_string()));
    }
    if explicit_workspace {
        return Err(format!(
            "cannot combine --workspace with machine-qualified pane '{pane_ref}'"
        ));
    }
    if let Some(existing) = current_machine {
        if existing != prefix {
            return Err(format!(
                "--machine '{existing}' conflicts with machine-qualified pane '{pane_ref}'"
            ));
        }
    }
    Ok((Some(prefix.to_string()), rest.to_string()))
}

pub(crate) fn known_machine_names() -> HashSet<String> {
    load_global_machines()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

/// Inspect a subcommand's arguments for a `<machine>/<pane>` reference,
/// resolving the target machine and rewriting the pane slot to its bare pane
/// reference on the remote workspace.
pub(crate) fn rewrite_qualified_subcommand_args(
    command: &str,
    sub_args: &[String],
    current_machine: Option<&str>,
    explicit_workspace: bool,
    known_machines: &HashSet<String>,
) -> Result<(Option<String>, Vec<String>), String> {
    if known_machines.is_empty() {
        return Ok((current_machine.map(str::to_string), sub_args.to_vec()));
    }
    if sub_args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        return Ok((current_machine.map(str::to_string), sub_args.to_vec()));
    }

    let mut out = sub_args.to_vec();
    let candidate_idx: Option<usize> = match command {
        "status" => {
            if sub_args.iter().any(|a| a == "--verbose" || a == "-v") {
                None
            } else {
                sub_args
                    .iter()
                    .position(|a| !a.starts_with('-') && !a.is_empty())
            }
        }
        "restart" | "interrupt" | "attach" | "snapshot" => sub_args
            .iter()
            .position(|a| !a.starts_with('-') && !a.is_empty()),
        "agent" => {
            let mut idx = 0;
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                if a == "--watch" || a == "-w" {
                    idx += 1;
                } else if !a.starts_with('-') && a != "on" && a != "off" {
                    found = Some(idx);
                    break;
                } else {
                    idx += 1;
                }
            }
            found
        }
        "send" => {
            let mut idx = 0;
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                match a {
                    "--lf" | "--raw" => idx += 1,
                    "--as" | "--generation" => idx += 2,
                    "--" => break,
                    _ if a.starts_with('-') => idx += 1,
                    _ => {
                        found = Some(idx);
                        break;
                    }
                }
            }
            found
        }
        "lease" => {
            let mut idx = if matches!(
                sub_args.first().map(String::as_str),
                Some("status" | "take" | "release")
            ) {
                1
            } else {
                0
            };
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                match a {
                    "--as" | "--why" | "-m" | "--message" | "--generation" => idx += 2,
                    "--force" => idx += 1,
                    "--" => break,
                    _ if a.starts_with('-') => idx += 1,
                    _ => {
                        found = Some(idx);
                        break;
                    }
                }
            }
            found
        }
        "wait" => {
            let mut idx = 0;
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                match a {
                    "--text" | "--regex" | "--idle" | "--attention" | "--timeout" => idx += 2,
                    "--exit" => idx += 1,
                    "--" => break,
                    _ if a.starts_with('-') => idx += 1,
                    _ => {
                        found = Some(idx);
                        break;
                    }
                }
            }
            found
        }
        "search" => {
            let mut idx = 0;
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                match a {
                    "-i" | "--ignore-case" => idx += 1,
                    "-n" | "--limit" => idx += 2,
                    "--" => break,
                    _ if a.starts_with('-') => idx += 1,
                    _ => {
                        found = Some(idx);
                        break;
                    }
                }
            }
            found
        }
        "lines" => {
            let mut idx = 0;
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                match a {
                    "-n" | "--last" => idx += 2,
                    "--" => break,
                    _ if a.starts_with('-') => idx += 1,
                    _ => {
                        found = Some(idx);
                        break;
                    }
                }
            }
            found
        }
        "exec" | "run" => {
            let mut idx = 0;
            let mut found = None;
            while idx < sub_args.len() {
                let a = sub_args[idx].as_str();
                if a == "--" {
                    break;
                }
                if a == "--pane" {
                    if idx + 1 < sub_args.len() {
                        found = Some(idx + 1);
                    }
                    break;
                }
                idx += 1;
            }
            found
        }
        _ => None,
    };

    if let Some(idx) = candidate_idx {
        let (machine, bare_pane) = split_qualified_pane_ref(
            &out[idx],
            current_machine,
            explicit_workspace,
            known_machines,
        )?;
        out[idx] = bare_pane;
        return Ok((machine, out));
    }

    Ok((current_machine.map(str::to_string), out))
}
