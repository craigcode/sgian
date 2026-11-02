//! Execution grants (docs/design/execution-grants.md): Kranz mints an
//! HMAC-SHA256-signed capability per ACP `terminal/*` request; Sgian verifies
//! the signature, expiry, workspace/cwd containment, single-use grant id, and
//! `env_allow` filter, runs `argv` verbatim in a PTY pane whose keyboard lease
//! is held by `kranz:<run>` (read-only to humans unless force-taken with a
//! ledgered `--why`), and records `grant.exec` / `grant.refused` receipts.

use std::ffi::{OsStr, OsString};

use super::*;

/// Sibling of `.kranz/serve.read.token` (0600) written and rotated by Kranz.
pub(crate) const KRANZ_GRANT_KEY_DIR: &str = ".kranz";
pub(crate) const KRANZ_GRANT_KEY_FILE: &str = "serve.grant.key";
pub(crate) const KRANZ_GRANT_KEY_MIN_BYTES: usize = 16;
pub(crate) const KRANZ_GRANT_KEY_MAX_BYTES: usize = 4096;

/// Bounded raw PTY output returned by `grant_output` (control sequences intact).
pub(crate) const GRANT_OUTPUT_MAX_BYTES: usize = 512 * 1024;
/// Upper bound on live replay entries after pruning expired grants.
pub(crate) const MAX_GRANT_REPLAY_ENTRIES: usize = 16_384;
/// Upper bound on unreleased `GrantTerminal` handles per daemon.
pub(crate) const MAX_ACTIVE_GRANT_TERMINALS: usize = 256;
/// Ledger key (`ledger/grants.jsonl`) used for `grant.refused` when the request
/// did not target an existing pane.
pub(crate) const GRANT_LEDGER_FALLBACK_KEY: &str = "grants";
/// Slice duration for `grant_wait` condition-variable waits so client
/// disconnect and daemon shutdown are observed promptly without holding locks.
pub(crate) const GRANT_WAIT_SLICE: Duration = Duration::from_millis(50);
/// Grace window between `SIGTERM` and `SIGKILL` on `grant_kill` / `grant_release`.
#[cfg(unix)]
pub(crate) const GRANT_KILL_GRACE: Duration = Duration::from_millis(500);

/// Pure-Rust RFC 2104 HMAC-SHA256 over `sha2::Sha256`, encoded as lowercase hex.
pub(crate) fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    const BLOCK_LEN: usize = 64;
    let mut key_block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK_LEN];
    let mut opad = [0x5cu8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    hex_encode(&outer.finalize())
}

/// Normalize a caller-supplied hex signature (optional `sha256=` prefix,
/// surrounding whitespace, ASCII case). Returns `None` if not 64 hex digits.
pub(crate) fn normalize_hmac_signature(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let hex_part = trimmed
        .strip_prefix("sha256=")
        .or_else(|| trimmed.strip_prefix("SHA256="))
        .unwrap_or(trimmed);
    if hex_part.len() != 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex_part.to_ascii_lowercase())
}

/// Read and validate `<repo_canon>/.kranz/serve.grant.key`: refuses symlinks on
/// `.kranz` and the key file, refuses group/world permissions on Unix (must be
/// owner-only like `0600`), and enforces `[16, 4096]` trimmed key bytes.
pub(crate) fn load_kranz_grant_key(repo_canon: &Path) -> Result<Vec<u8>, String> {
    let kranz_dir = repo_canon.join(KRANZ_GRANT_KEY_DIR);
    let dir_meta = fs::symlink_metadata(&kranz_dir)
        .map_err(|error| format!("grant key unavailable ({}): {error}", kranz_dir.display()))?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return Err(format!(
            "grant key directory {} must be a real directory",
            kranz_dir.display()
        ));
    }

    let key_path = kranz_dir.join(KRANZ_GRANT_KEY_FILE);
    let meta = fs::symlink_metadata(&key_path)
        .map_err(|error| format!("grant key unavailable ({}): {error}", key_path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!(
            "grant key {} must be a regular file (not a symlink)",
            key_path.display()
        ));
    }
    #[cfg(unix)]
    {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "grant key {} has insecure permissions {:04o}; expected owner-only (0600)",
                key_path.display(),
                mode
            ));
        }
    }
    if meta.len() > KRANZ_GRANT_KEY_MAX_BYTES as u64 {
        return Err(format!(
            "grant key {} exceeds {} bytes",
            key_path.display(),
            KRANZ_GRANT_KEY_MAX_BYTES
        ));
    }

    let mut raw = Vec::new();
    File::open(&key_path)
        .and_then(|file| {
            file.take(KRANZ_GRANT_KEY_MAX_BYTES as u64 + 1)
                .read_to_end(&mut raw)
        })
        .map_err(|error| format!("failed to read grant key {}: {error}", key_path.display()))?;
    if raw.len() > KRANZ_GRANT_KEY_MAX_BYTES {
        return Err(format!(
            "grant key {} exceeds {} bytes",
            key_path.display(),
            KRANZ_GRANT_KEY_MAX_BYTES
        ));
    }
    let trimmed = trim_ascii_whitespace(&raw);
    if trimmed.len() < KRANZ_GRANT_KEY_MIN_BYTES {
        return Err(format!(
            "grant key {} is shorter than {} bytes",
            key_path.display(),
            KRANZ_GRANT_KEY_MIN_BYTES
        ));
    }
    Ok(trimmed.to_vec())
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|idx| idx + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

/// Canonical capability minted by Kranz (`docs/design/execution-grants.md` §2).
/// `deny_unknown_fields` fails closed if a newer Kranz attaches an unfamiliar
/// restriction field that this daemon version does not know how to enforce.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionGrant {
    pub(crate) grant: String,
    pub(crate) mission: String,
    pub(crate) run: String,
    pub(crate) repo: String,
    pub(crate) cwd: String,
    pub(crate) argv: Vec<String>,
    #[serde(default)]
    pub(crate) env_allow: Vec<String>,
    pub(crate) expires_ms: u64,
    pub(crate) issued_ms: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GrantExitState {
    pub(crate) exited: bool,
    pub(crate) exit_code: Option<i32>,
}

pub(crate) struct GrantTerminal {
    pub(crate) terminal_id: String,
    pub(crate) pane_id: String,
    pub(crate) grant_id: String,
    pub(crate) generation: u64,
    pub(crate) scrollback_start_offset: u64,
    pub(crate) exit_state: Mutex<GrantExitState>,
    pub(crate) exit_cvar: Condvar,
}

impl GrantTerminal {
    fn mark_exited(&self, exit_code: Option<i32>) {
        if let Ok(mut state) = self.exit_state.lock() {
            if !state.exited {
                state.exited = true;
                state.exit_code = exit_code;
            }
        }
        self.exit_cvar.notify_all();
    }

    fn snapshot_exit(&self) -> Option<Value> {
        self.exit_state.lock().ok().and_then(|state| {
            state
                .exited
                .then(|| json!({ "exit_code": state.exit_code }))
        })
    }
}

struct GrantStoreInner {
    served_repos: HashSet<PathBuf>,
    cached_keys: HashMap<PathBuf, Vec<u8>>,
    replay: HashMap<String, u64>,
    terminals: HashMap<String, Arc<GrantTerminal>>,
}

pub(crate) struct GrantStore {
    inner: Mutex<GrantStoreInner>,
    next_terminal_seq: AtomicU64,
}

impl GrantStore {
    pub(crate) fn new(workspace_cwd: &Path) -> Self {
        let mut served_repos = HashSet::new();
        let canon = workspace_cwd
            .canonicalize()
            .unwrap_or_else(|_| workspace_cwd.to_path_buf());
        let mut cached_keys = HashMap::new();
        if let Ok(key) = load_kranz_grant_key(&canon) {
            cached_keys.insert(canon.clone(), key);
        }
        served_repos.insert(canon);
        Self {
            inner: Mutex::new(GrantStoreInner {
                served_repos,
                cached_keys,
                replay: HashMap::new(),
                terminals: HashMap::new(),
            }),
            next_terminal_seq: AtomicU64::new(1),
        }
    }

    /// Register a bound Kranz repository (`ctl kranz bind` or auto-bind) as a
    /// repo this daemon serves for execution grants, reading and caching its
    /// `.kranz/serve.grant.key` if present. Returns `true` when a valid grant
    /// key was loaded at bind time.
    pub(crate) fn register_repo_path(&self, workspace_cwd: &Path, raw_repo: &str) -> bool {
        let candidate = if Path::new(raw_repo).is_absolute() {
            PathBuf::from(raw_repo)
        } else {
            workspace_cwd.join(raw_repo)
        };
        let Ok(canon) = candidate.canonicalize() else {
            return false;
        };
        if !canon.is_dir() {
            return false;
        }
        let loaded_key = load_kranz_grant_key(&canon).ok();
        let has_key = loaded_key.is_some();
        if let Ok(mut inner) = self.inner.lock() {
            inner.served_repos.insert(canon.clone());
            if let Some(key) = loaded_key {
                inner.cached_keys.insert(canon, key);
            }
        }
        has_key
    }

    /// Return the active HMAC key for `repo_canon` if `repo_canon` is served by
    /// this daemon. Re-reads `.kranz/serve.grant.key` from disk so key rotation
    /// (or revocation by deleting the file) takes effect immediately.
    pub(crate) fn key_for_served_repo(&self, repo_canon: &Path) -> Result<Vec<u8>, String> {
        let served = self
            .inner
            .lock()
            .map_err(|_| "grant store lock poisoned".to_string())?
            .served_repos
            .contains(repo_canon);
        if !served {
            return Err(format!(
                "grant repo {} is not served by this daemon",
                repo_canon.display()
            ));
        }
        let key = load_kranz_grant_key(repo_canon)?;
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .cached_keys
                .insert(repo_canon.to_path_buf(), key.clone());
        }
        Ok(key)
    }

    /// Prune expired replay entries, verify `grant_id` has not been used and
    /// active terminal handles are within cap, and atomically reserve `grant_id`.
    pub(crate) fn reserve_grant_id(
        &self,
        grant_id: &str,
        expires_ms: u64,
        now_ms: u64,
    ) -> Result<String, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "grant store lock poisoned".to_string())?;
        inner.replay.retain(|_, exp| *exp > now_ms);
        if inner.replay.contains_key(grant_id) {
            return Err(format!("grant '{grant_id}' has already been used"));
        }
        if inner.replay.len() >= MAX_GRANT_REPLAY_ENTRIES {
            return Err(format!(
                "grant replay table limit reached ({MAX_GRANT_REPLAY_ENTRIES})"
            ));
        }
        if inner.terminals.len() >= MAX_ACTIVE_GRANT_TERMINALS {
            return Err(format!(
                "active grant terminal limit reached ({MAX_ACTIVE_GRANT_TERMINALS}); release finished handles"
            ));
        }
        inner.replay.insert(grant_id.to_string(), expires_ms);
        let seq = self.next_terminal_seq.fetch_add(1, Ordering::SeqCst);
        let mut rand_bytes = [0u8; 6];
        let suffix = if fill_secure_random(&mut rand_bytes).is_ok() {
            hex_encode(&rand_bytes)
        } else {
            format!("{seq:012x}")
        };
        Ok(format!("gt-{seq}-{suffix}"))
    }

    pub(crate) fn insert_terminal(&self, terminal: Arc<GrantTerminal>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner
                .terminals
                .insert(terminal.terminal_id.clone(), terminal);
        }
    }

    pub(crate) fn get_terminal(&self, terminal_id: &str) -> Result<Arc<GrantTerminal>, String> {
        self.inner
            .lock()
            .map_err(|_| "grant store lock poisoned".to_string())?
            .terminals
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| format!("unknown grant terminal: {terminal_id}"))
    }

    pub(crate) fn remove_terminal(&self, terminal_id: &str) -> Result<Arc<GrantTerminal>, String> {
        self.inner
            .lock()
            .map_err(|_| "grant store lock poisoned".to_string())?
            .terminals
            .remove(terminal_id)
            .ok_or_else(|| format!("unknown grant terminal: {terminal_id}"))
    }

    /// Called by the PTY reader thread when `(pane_id, generation)` finishes.
    pub(crate) fn notify_pane_exit(&self, pane_id: &str, generation: u64, exit_code: Option<i32>) {
        let matching: Vec<Arc<GrantTerminal>> = self
            .inner
            .lock()
            .map(|inner| {
                inner
                    .terminals
                    .values()
                    .filter(|term| term.pane_id == pane_id && term.generation == generation)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for term in matching {
            term.mark_exited(exit_code);
        }
    }

    /// Called when `close_pane` drops or supersedes a pane's PTY session.
    pub(crate) fn notify_pane_superseded(&self, pane_id: &str) {
        let matching: Vec<Arc<GrantTerminal>> = self
            .inner
            .lock()
            .map(|inner| {
                inner
                    .terminals
                    .values()
                    .filter(|term| term.pane_id == pane_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        for term in matching {
            term.mark_exited(None);
        }
    }
}

fn env_key_matches(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Build the child environment for a grant from Sgian's configured spawn
/// environment (`std::env::vars()` minus session markers minus `scrub_env`,
/// plus `TERM`/`COLORTERM` and `config.env`), filtered strictly to keys named
/// in `env_allow`. Returns `(allowed_env, daemon_path_for_lookup)` so Sgian can
/// resolve bare `argv[0]` binaries even when `PATH` is not in `env_allow`.
pub(crate) fn build_grant_spawn_env(
    config: &Config,
    env_allow: &[String],
) -> (Vec<(String, String)>, Option<OsString>) {
    let mut base: Vec<(String, String)> = std::env::vars().collect();
    base.retain(|(k, _)| {
        !INHERITED_SESSION_MARKERS
            .iter()
            .any(|marker| env_key_matches(k, marker))
            && !config
                .scrub_env
                .iter()
                .any(|scrub| env_key_matches(k, scrub))
    });
    let upsert = |entries: &mut Vec<(String, String)>, key: &str, value: &str| {
        if let Some(existing) = entries.iter_mut().find(|(k, _)| env_key_matches(k, key)) {
            existing.0 = key.to_string();
            existing.1 = value.to_string();
        } else {
            entries.push((key.to_string(), value.to_string()));
        }
    };
    upsert(&mut base, "TERM", "xterm-256color");
    upsert(&mut base, "COLORTERM", "truecolor");
    for (k, v) in &config.env {
        upsert(&mut base, k, v);
    }

    let daemon_path = base
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(_, v)| OsString::from(v));

    let mut allowed: Vec<(String, String)> = Vec::new();
    for allow_key in env_allow {
        if allow_key.is_empty() || allow_key.contains('=') || allow_key.contains('\0') {
            continue;
        }
        if allowed.iter().any(|(k, _)| env_key_matches(k, allow_key)) {
            continue;
        }
        if let Some((k, v)) = base.iter().find(|(k, _)| env_key_matches(k, allow_key)) {
            allowed.push((k.clone(), v.clone()));
        }
    }
    (allowed, daemon_path)
}

/// Resolve `argv[0]` to an explicit executable path without ever searching
/// `cwd` for bare command names (avoiding `portable-pty`'s `cwd`-first hazard).
pub(crate) fn resolve_grant_program(
    arg0: &str,
    cwd_canon: &Path,
    daemon_path: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if arg0.is_empty() || arg0.contains('\0') {
        return Err("grant argv[0] must be a non-empty program name".to_string());
    }
    let has_sep =
        arg0.contains('/') || (cfg!(windows) && (arg0.contains('\\') || arg0.contains(':')));
    if has_sep {
        let candidate = if Path::new(arg0).is_absolute() {
            PathBuf::from(arg0)
        } else {
            cwd_canon.join(arg0)
        };
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
        return Err(format!(
            "grant program '{}' does not exist or is not executable",
            candidate.display()
        ));
    }

    let Some(path_var) = daemon_path else {
        return Err(format!("grant program '{arg0}' not found (PATH is unset)"));
    };
    #[cfg(windows)]
    let pathext: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.trim_start_matches('.').to_string())
        .collect();

    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() || dir == Path::new(".") {
            // Never implicitly resolve a bare command from relative `.` in PATH.
            continue;
        }
        let candidate = dir.join(arg0);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
        #[cfg(windows)]
        for ext in &pathext {
            let with_ext = dir.join(format!("{arg0}.{ext}"));
            if is_executable_file(&with_ext) {
                return Ok(with_ext);
            }
        }
    }
    Err(format!("grant program '{arg0}' not found in PATH"))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Spawn `argv` directly in a fresh PTY (never through a shell), with a cleared
/// environment populated strictly from `allowed_env`.
pub(crate) fn execute_grant_spawn(
    size: PtySize,
    resolved_program: &Path,
    argv: &[String],
    cwd_canon: &Path,
    allowed_env: &[(String, String)],
) -> Result<PreparedSpawn, String> {
    let pty_system = native_pty_system();
    let pair = open_pty_with_retry(pty_system.as_ref(), size)?;

    let mut command = CommandBuilder::new(resolved_program);
    for arg in argv.iter().skip(1) {
        command.arg(arg);
    }
    command.cwd(cwd_canon);
    command.env_clear();
    for (key, value) in allowed_env {
        command.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("failed to spawn grant command: {error}"))?;
    let killer = child.clone_killer();
    let writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            kill_and_reap_child(child);
            return Err(format!("failed to open pty writer: {error}"));
        }
    };
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            kill_and_reap_child(child);
            return Err(format!("failed to open pty reader: {error}"));
        }
    };

    Ok(PreparedSpawn {
        master: pair.master,
        child,
        killer,
        writer,
        reader,
        command_str: argv.join(" "),
        cwd_str: cwd_canon.to_string_lossy().to_string(),
    })
}

/// Read bounded raw PTY bytes (control sequences intact) written to `pane_id`
/// at or after `start_offset`.
pub(crate) fn read_grant_scrollback(
    scrollback_dir: &Path,
    pane_id: &str,
    start_offset: u64,
    max_bytes: usize,
) -> (String, bool) {
    let path = scrollback_path(scrollback_dir, pane_id);
    let Ok(mut file) = File::open(&path) else {
        return (String::new(), false);
    };
    let Ok(meta) = file.metadata() else {
        return (String::new(), false);
    };
    let len = meta.len();
    let (seek_to, truncated) = if len < start_offset {
        // The 16 MiB scrollback cap rotated the file after the grant started.
        let cap_offset = len.saturating_sub(max_bytes as u64);
        (cap_offset, true)
    } else {
        let available = len - start_offset;
        if available > max_bytes as u64 {
            (len - max_bytes as u64, true)
        } else {
            (start_offset, false)
        }
    };
    if file.seek(SeekFrom::Start(seek_to)).is_err() {
        return (String::new(), truncated);
    }
    let mut data = Vec::new();
    if (&mut file)
        .take(max_bytes as u64)
        .read_to_end(&mut data)
        .is_err()
    {
        return (String::new(), truncated);
    }
    let start = utf8_boundary_at_or_after(&data, 0);
    (
        String::from_utf8_lossy(&data[start..]).to_string(),
        truncated,
    )
}

fn preview_grant_id(grant_raw: &Value) -> Option<String> {
    grant_raw
        .get("grant")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| {
            id.chars()
                .filter(|c| !c.is_control())
                .take(128)
                .collect::<String>()
        })
}

struct VerifiedGrant {
    parsed: ExecutionGrant,
    cwd_canon: PathBuf,
    holder: String,
}

impl DaemonServer {
    fn ledger_grant_refused(
        &self,
        requested_pane: Option<&str>,
        grant_id: Option<&str>,
        reason: &str,
    ) {
        let target_key = requested_pane
            .filter(|pane_id| {
                self.lock_registry()
                    .map(|reg| reg.contains_pane(pane_id))
                    .unwrap_or(false)
            })
            .unwrap_or(GRANT_LEDGER_FALLBACK_KEY);
        let clip_reason: String = reason.chars().take(512).collect();
        let _ = self.ledger_record(
            target_key,
            "grant.refused",
            json!({
                "grant": grant_id,
                "reason": clip_reason,
            }),
        );
    }

    fn verify_grant_capability(
        &self,
        grant_raw: &Value,
        signature: &str,
    ) -> Result<VerifiedGrant, String> {
        if !grant_raw.is_object() {
            return Err("grant must be a JSON object".to_string());
        }
        let provided_sig = normalize_hmac_signature(signature)
            .ok_or_else(|| "grant signature must be a 64-character hex HMAC-SHA256".to_string())?;
        let canonical = canonical_json(grant_raw);
        let parsed: ExecutionGrant = serde_json::from_value(grant_raw.clone())
            .map_err(|error| format!("invalid grant payload: {error}"))?;

        let grant_id = validate_bounded_text(&parsed.grant, "grant", 128)?;
        if grant_id != parsed.grant || !grant_id.chars().all(|c| c.is_ascii_graphic()) {
            return Err("grant id must be printable ASCII with no whitespace".to_string());
        }
        let _ = validate_bounded_text(&parsed.mission, "mission", 128)?;
        let run_clean = validate_bounded_text(&parsed.run, "run", 48)?;
        if run_clean != parsed.run || !run_clean.chars().all(|c| c.is_ascii_graphic()) {
            return Err("grant run must be printable ASCII with no whitespace".to_string());
        }
        let holder = validate_holder(&format!("kranz:{run_clean}"))?;

        if parsed.argv.is_empty() || parsed.argv[0].is_empty() {
            return Err("grant argv must contain at least one non-empty element".to_string());
        }
        if parsed.argv.len() > 1024 || parsed.argv.iter().any(|a| a.contains('\0')) {
            return Err("grant argv exceeds bounds or contains NUL".to_string());
        }
        if parsed.env_allow.len() > 256
            || parsed
                .env_allow
                .iter()
                .any(|k| k.is_empty() || k.contains('=') || k.contains('\0'))
        {
            return Err("grant env_allow contains an invalid variable name".to_string());
        }

        let repo_path = Path::new(&parsed.repo);
        if !repo_path.is_absolute() {
            return Err("grant repo must be an absolute path".to_string());
        }
        let cwd_path = Path::new(&parsed.cwd);
        if !cwd_path.is_absolute() {
            return Err("grant cwd must be an absolute path".to_string());
        }
        let repo_canon = repo_path
            .canonicalize()
            .map_err(|error| format!("grant repo {}: {error}", repo_path.display()))?;
        if !repo_canon.is_dir() {
            return Err(format!(
                "grant repo {} is not a directory",
                repo_canon.display()
            ));
        }

        // Verify repo membership + HMAC signature BEFORE probing arbitrary cwd
        // paths or mutating the replay set.
        let key = self.grants.key_for_served_repo(&repo_canon)?;
        let expected_sig = hmac_sha256_hex(&key, canonical.as_bytes());
        if !constant_time_eq(&provided_sig, &expected_sig) {
            return Err("grant signature verification failed".to_string());
        }

        let now = now_millis();
        if parsed.issued_ms > parsed.expires_ms {
            return Err("grant issued_ms exceeds expires_ms".to_string());
        }
        if parsed.expires_ms <= now {
            return Err("grant has expired".to_string());
        }

        let cwd_canon = cwd_path
            .canonicalize()
            .map_err(|error| format!("grant cwd {}: {error}", cwd_path.display()))?;
        if !cwd_canon.is_dir() {
            return Err(format!(
                "grant cwd {} is not a directory",
                cwd_canon.display()
            ));
        }
        if !cwd_canon.starts_with(&repo_canon) {
            return Err(format!(
                "grant cwd {} escapes repo {}",
                cwd_canon.display(),
                repo_canon.display()
            ));
        }

        Ok(VerifiedGrant {
            parsed,
            cwd_canon,
            holder,
        })
    }

    pub(crate) fn handle_grant_exec(
        &self,
        grant_raw: Value,
        signature: &str,
        requested_pane: Option<&str>,
    ) -> Result<Value, String> {
        let grant_preview = preview_grant_id(&grant_raw);
        let verified = match self.verify_grant_capability(&grant_raw, signature) {
            Ok(v) => v,
            Err(error) => {
                self.ledger_grant_refused(requested_pane, grant_preview.as_deref(), &error);
                return Err(error);
            }
        };
        let grant_id = verified.parsed.grant.clone();
        match self.execute_verified_grant(verified, requested_pane) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.ledger_grant_refused(requested_pane, Some(&grant_id), &error);
                Err(error)
            }
        }
    }

    fn execute_verified_grant(
        &self,
        verified: VerifiedGrant,
        requested_pane: Option<&str>,
    ) -> Result<Value, String> {
        let VerifiedGrant {
            parsed,
            cwd_canon,
            holder,
        } = verified;

        // Pre-resolve the binary + filtered environment before burning the
        // single-use grant id or creating a pane.
        let config = self.effective_config();
        let (allowed_env, daemon_path) = build_grant_spawn_env(&config, &parsed.env_allow);
        let resolved_program =
            resolve_grant_program(&parsed.argv[0], &cwd_canon, daemon_path.as_deref())?;

        // Validate target pane preconditions before reserving the grant id.
        if let Some(target_id) = requested_pane {
            let kind = self
                .lock_registry()?
                .pane_kind(target_id)
                .ok_or_else(|| format!("pane not found: {target_id}"))?;
            if kind != PaneKind::Shell {
                return Err(format!("grant target pane {target_id} is not a shell pane"));
            }
            if self.lock_terminals()?.is_live_or_spawning(target_id) {
                return Err(format!(
                    "grant target pane {target_id} already has a live process"
                ));
            }
            let leases = self.lock_leases()?;
            if let Some(held) = leases.get(target_id) {
                if held.holder != holder {
                    return Err(format!(
                        "grant target pane {target_id} keyboard is held by {}",
                        held.holder
                    ));
                }
            }
        } else if self.lock_registry()?.panes.len() >= MAX_PANES {
            return Err(format!(
                "pane limit reached ({MAX_PANES}); close a pane before creating another"
            ));
        }

        // Single-use replay reservation (atomic after expiry prune).
        let now = now_millis();
        let terminal_id = self
            .grants
            .reserve_grant_id(&parsed.grant, parsed.expires_ms, now)?;

        // Prepare target pane + read-only-to-humans lease (`kranz:<run>`).
        let (created_pane, previous_active, pane_id, scrollback_start_offset, newly_leased) =
            if let Some(target_id) = requested_pane {
                let kind = self
                    .lock_registry()?
                    .pane_kind(target_id)
                    .ok_or_else(|| format!("pane not found: {target_id}"))?;
                if kind != PaneKind::Shell {
                    return Err(format!("grant target pane {target_id} is not a shell pane"));
                }
                let newly_leased = {
                    let mut leases = self.lock_leases()?;
                    match leases.get(target_id) {
                        Some(held) if held.holder == holder => None,
                        Some(held) => {
                            return Err(format!(
                                "grant target pane {target_id} keyboard is held by {}",
                                held.holder
                            ));
                        }
                        None => {
                            let gen = self.next_lease_generation.fetch_add(1, Ordering::SeqCst);
                            leases.insert(target_id.to_string(), HeldLease::new(&holder, now, gen));
                            Some(gen)
                        }
                    }
                };
                let start_offset = fs::metadata(scrollback_path(&self.scrollback_dir, target_id))
                    .map(|m| m.len())
                    .unwrap_or(0);
                (
                    None,
                    None,
                    target_id.to_string(),
                    start_offset,
                    newly_leased,
                )
            } else {
                let (pane, prev_active) = {
                    let mut registry = self.lock_registry()?;
                    if registry.panes.len() >= MAX_PANES {
                        return Err(format!(
                            "pane limit reached ({MAX_PANES}); close a pane before creating another"
                        ));
                    }
                    let prev_active = registry.active_pane_id.clone();
                    let pane = registry.create_pane(Some(format!("grant:{}", parsed.grant)));
                    (pane, prev_active)
                };
                let gen = self.next_lease_generation.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut leases) = self.lock_leases() {
                    leases.insert(pane.id.clone(), HeldLease::new(&holder, now, gen));
                }
                let pane_id = pane.id.clone();
                (Some(pane), prev_active, pane_id, 0, Some(gen))
            };

        let rollback_pane_and_lease = |server: &DaemonServer| {
            if newly_leased.is_some() {
                if let Ok(mut leases) = server.lock_leases() {
                    leases.remove(&pane_id);
                }
            }
            if created_pane.is_some() {
                server.rollback_created_pane(&pane_id, previous_active.clone());
            }
        };

        let size = {
            let mut terminals = match self.lock_terminals() {
                Ok(t) => t,
                Err(error) => {
                    rollback_pane_and_lease(self);
                    return Err(error);
                }
            };
            if terminals.is_live_or_spawning(&pane_id) {
                drop(terminals);
                rollback_pane_and_lease(self);
                return Err(format!(
                    "grant target pane {pane_id} already has a live process"
                ));
            }
            terminals.spawning.insert(pane_id.clone());
            terminals
                .sizes
                .get(&pane_id)
                .copied()
                .unwrap_or_else(default_pty_size)
        };

        let _in_flight = SpawnInFlight {
            server: self,
            pane_id: pane_id.clone(),
        };

        let prepared = match execute_grant_spawn(
            size,
            &resolved_program,
            &parsed.argv,
            &cwd_canon,
            &allowed_env,
        ) {
            Ok(p) => p,
            Err(error) => {
                rollback_pane_and_lease(self);
                return Err(error);
            }
        };

        let grant_terminal = {
            let mut terminals = match self.lock_terminals() {
                Ok(t) => t,
                Err(error) => {
                    kill_and_reap_child(prepared.child);
                    rollback_pane_and_lease(self);
                    return Err(error);
                }
            };
            if self.router.is_closed(&pane_id) || self.should_shutdown() {
                drop(terminals);
                kill_and_reap_child(prepared.child);
                rollback_pane_and_lease(self);
                return Err("grant spawn aborted: pane closed or daemon shutting down".to_string());
            }
            let pty_generation = terminals.commit_spawn(&pane_id, size, prepared);
            let term = Arc::new(GrantTerminal {
                terminal_id: terminal_id.clone(),
                pane_id: pane_id.clone(),
                grant_id: parsed.grant.clone(),
                generation: pty_generation,
                scrollback_start_offset,
                exit_state: Mutex::new(GrantExitState::default()),
                exit_cvar: Condvar::new(),
            });
            self.grants.insert_terminal(Arc::clone(&term));
            // Close the fast-exit race if the child exited before insert_terminal.
            let (already_ended, exit_code) =
                terminals.generation_exit_snapshot(&pane_id, pty_generation);
            if already_ended {
                term.mark_exited(exit_code);
            }
            term
        };
        let _ = grant_terminal;

        if let Some(lease_gen) = newly_leased {
            let _ = self.ledger_record(
                &pane_id,
                "lease.taken",
                json!({
                    "holder": holder,
                    "forced": false,
                    "generation": lease_gen,
                    "grant": parsed.grant,
                }),
            );
        }
        let _ = self.ledger_record(
            &pane_id,
            "grant.exec",
            json!({
                "grant": parsed.grant,
                "mission": parsed.mission,
                "run": parsed.run,
                "terminal_id": terminal_id,
                "argv": parsed.argv,
                "cwd": cwd_canon.to_string_lossy(),
            }),
        );
        let _ = self.persist();

        if let Some(pane) = created_pane {
            if self
                .lock_registry()
                .map(|r| r.contains_pane(&pane.id))
                .unwrap_or(false)
            {
                self.router.broadcast(&DaemonEvent::PaneCreated { pane });
            }
        }
        if newly_leased.is_some() {
            self.router.broadcast(&DaemonEvent::LeaseState {
                pane_id: pane_id.clone(),
                transition: LeaseTransition::Taken,
                holder: Some(holder),
                since_ms: Some(now),
                note: None,
            });
        }

        Ok(json!({
            "terminal_id": terminal_id,
            "pane_id": pane_id,
        }))
    }

    pub(crate) fn handle_grant_output(&self, terminal_id: &str) -> Result<Value, String> {
        let term = self.grants.get_terminal(terminal_id)?;
        // Snapshot exit BEFORE reading scrollback so when `exit` is non-null,
        // every byte flushed before `emit_pane_ended` is already on disk.
        let exit = term.snapshot_exit();
        let (output, truncated) = read_grant_scrollback(
            &self.scrollback_dir,
            &term.pane_id,
            term.scrollback_start_offset,
            GRANT_OUTPUT_MAX_BYTES,
        );
        Ok(json!({
            "terminal_id": term.terminal_id,
            "pane_id": term.pane_id,
            "grant": term.grant_id,
            "output": output,
            "truncated": truncated,
            "exit": exit,
        }))
    }

    pub(crate) fn handle_grant_wait(
        &self,
        terminal_id: &str,
        timeout_ms: Option<u64>,
        peer: Option<&TransportStream>,
    ) -> Result<Value, String> {
        let term = self.grants.get_terminal(terminal_id)?;
        let started = Instant::now();
        let deadline = timeout_ms.map(|ms| started + Duration::from_millis(ms));

        let mut state = term
            .exit_state
            .lock()
            .map_err(|_| "grant terminal lock poisoned".to_string())?;
        while !state.exited {
            if self.should_shutdown() {
                return Err("grant wait aborted: daemon shutting down".to_string());
            }
            if let Some(stream) = peer {
                if wait_peer_disconnected(stream) {
                    return Err(format!(
                        "grant wait aborted: client disconnected: {terminal_id}"
                    ));
                }
            }
            let slice = match deadline {
                Some(limit) => {
                    let now = Instant::now();
                    if now >= limit {
                        return Ok(json!({
                            "terminal_id": term.terminal_id,
                            "pane_id": term.pane_id,
                            "exited": false,
                            "timed_out": true,
                            "exit": Value::Null,
                            "elapsed_ms": started.elapsed().as_millis() as u64,
                        }));
                    }
                    (limit - now).min(GRANT_WAIT_SLICE)
                }
                None => GRANT_WAIT_SLICE,
            };
            let (guard, _) = term
                .exit_cvar
                .wait_timeout(state, slice)
                .map_err(|_| "grant terminal lock poisoned".to_string())?;
            state = guard;
        }
        let exit_code = state.exit_code;
        drop(state);

        Ok(json!({
            "terminal_id": term.terminal_id,
            "pane_id": term.pane_id,
            "exited": true,
            "timed_out": false,
            "exit": { "exit_code": exit_code },
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }))
    }

    pub(crate) fn handle_grant_kill(&self, terminal_id: &str) -> Result<Value, String> {
        let term = self.grants.get_terminal(terminal_id)?;
        let signalled = self
            .lock_terminals()?
            .kill_session_generation(&term.pane_id, term.generation);
        Ok(json!({
            "terminal_id": term.terminal_id,
            "pane_id": term.pane_id,
            "killed": signalled,
            "already_exited": !signalled,
        }))
    }

    pub(crate) fn handle_grant_release(&self, terminal_id: &str) -> Result<Value, String> {
        let term = self.grants.remove_terminal(terminal_id)?;
        // Per ACP `terminal/release`, releasing a handle whose child is still
        // running terminates the child while keeping the Sgian pane + ledger.
        let _ = self
            .lock_terminals()
            .map(|mut terminals| terminals.kill_session_generation(&term.pane_id, term.generation));
        Ok(json!({
            "terminal_id": term.terminal_id,
            "pane_id": term.pane_id,
            "released": true,
        }))
    }
}
