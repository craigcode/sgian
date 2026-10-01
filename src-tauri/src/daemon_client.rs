use super::*;

/// A client-side connection to a workspace daemon that speaks the NEGOTIATED wire
/// protocol (architecture.md §5.2/§5.3, Invariant 8). It performs the newline-JSON
/// capability handshake, then uses the framed v2 envelope for every subsequent
/// message when the daemon negotiated wire v2 AND advertised the `framed`
/// capability, or stays on the legacy newline path otherwise. This is the single
/// client abstraction `ctl` and the GUI `DaemonClient` use, so a new client
/// transparently talks framed v2 to a new daemon and gracefully falls back to
/// newline v1 against an old daemon.
///
/// One `BufReader` spans the handshake AND every later read so a v1 event stream
/// that delivers several newline events in a single underlying socket read is
/// buffered correctly.
pub(crate) struct DaemonConnection {
    pub(crate) reader: BufReader<TransportStream>,
    /// Negotiated wire version: `min(client_max, daemon_max)`; 1 against a v1 daemon
    /// or when the response omits `negotiated_wire_version` (graceful fallback).
    pub(crate) wire_version: u16,
    /// Whether the daemon advertised the `framed` capability in its handshake
    /// response. Both this and a v2 `wire_version` are required to switch to framing.
    pub(crate) advertises_framed: bool,
    /// Whether the daemon advertised `subscribe-ack`: a Subscribe is then
    /// acknowledged with a first SubscribeAck event once registration is done (M8).
    pub(crate) advertises_subscribe_ack: bool,
}

/// Default client-side read deadline for one request/response round-trip. Without
/// it, a wedged daemon (see H2's history) pins every caller — each GUI invoke
/// thread, every ctl command — in a blocking read forever. Streams with
/// legitimately unbounded gaps (event subscriptions, waits) opt out explicitly
/// via `set_read_timeout`.
pub(crate) const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(20);

impl DaemonConnection {
    /// Connect to the daemon at `socket_path` and complete the capability handshake.
    pub(crate) fn connect(socket_path: &Path, token: &str) -> Result<Self, String> {
        Self::connect_with_timeout(socket_path, token, Some(CLIENT_READ_TIMEOUT))
    }

    /// `connect` with an explicit read deadline (tests use a short one to prove a
    /// silent daemon can't hang the client).
    pub(crate) fn connect_with_timeout(
        socket_path: &Path,
        token: &str,
        read_timeout: Option<Duration>,
    ) -> Result<Self, String> {
        let stream = transport_connect(socket_path)
            .map_err(|error| format!("failed to connect to daemon: {error}"))?;
        // Best-effort: a transport that cannot set timeouts still works, it just
        // keeps the old blocking behavior.
        let _ = stream.set_read_timeout(read_timeout);
        Self::handshake(stream, token, client_token_from_env().as_deref())
    }

    /// Adjust this connection's read deadline. Event subscriptions clear it
    /// (events are legitimately sparse); `ctl wait` scales it to the wait's own
    /// timeout.
    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) {
        let _ = self.reader.get_ref().set_read_timeout(timeout);
    }

    /// Drive the client side of the capability handshake over an already-connected
    /// `stream`: send the newline-JSON hello carrying the legacy `version: 1` (so a
    /// v1 daemon still accepts it) plus the additive `max_wire_version` = the framed
    /// wire version, read the newline-JSON handshake response, and record the
    /// negotiated wire version + whether framing was advertised. Hello and response
    /// stay newline-JSON so a v1 peer can read them (VAL-IPC-012/024).
    pub(crate) fn handshake(
        stream: TransportStream,
        token: &str,
        client_token: Option<&str>,
    ) -> Result<Self, String> {
        let mut reader = BufReader::new(stream);
        let hello = IpcHello {
            frame_type: "hello".to_string(),
            version: PROTOCOL_VERSION,
            token: token.to_string(),
            max_wire_version: Some(frame::WIRE_VERSION),
            capabilities: Some(client_capabilities()),
            client_token: client_token.map(str::to_string),
        };
        write_json_line(reader.get_mut(), &hello)?;

        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read daemon hello: {error}"))?;
        let response: IpcResponse = serde_json::from_str(line.trim_end())
            .map_err(|error| format!("invalid daemon hello: {error}"))?;
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| "daemon authentication failed".to_string()));
        }
        let wire_version = response
            .result
            .get("negotiated_wire_version")
            .and_then(Value::as_u64)
            .map(|v| v as u16)
            .unwrap_or(1);
        let daemon_caps = response
            .result
            .get("capabilities")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let has_cap = |name: &str| daemon_caps.iter().any(|cap| cap.as_str() == Some(name));
        Ok(Self {
            reader,
            wire_version,
            advertises_framed: has_cap("framed"),
            advertises_subscribe_ack: has_cap("subscribe-ack"),
        })
    }

    /// Consume the Subscribe registration ack when the daemon supports it. Call
    /// immediately after writing a Subscribe request (while the request read
    /// deadline is still armed): once the ack arrives, this connection is
    /// registered as a subscriber, so input sent on ANOTHER connection
    /// afterwards cannot have its output broadcast before we joined (M8).
    /// Against an older daemon this is a no-op and the historical (tiny)
    /// subscribe-then-send race window remains.
    pub(crate) fn await_subscribe_ack(&mut self) -> Result<(), String> {
        if !self.advertises_subscribe_ack {
            return Ok(());
        }
        match self.read_event()? {
            Some(DaemonEvent::SubscribeAck) => Ok(()),
            Some(other) => Err(format!(
                "expected the subscribe ack as the first event, got {other:?}"
            )),
            None => Err("daemon closed before acknowledging the subscription".to_string()),
        }
    }

    /// The single source of truth for framed-vs-newline on this connection, mirroring
    /// the daemon's own post-handshake branch in `handle_daemon_client`: use framing
    /// iff the negotiated wire version reached the framed version AND the daemon
    /// advertised `framed` (VAL-IPC-025).
    pub(crate) fn uses_framing(&self) -> bool {
        self.wire_version >= frame::WIRE_VERSION && self.advertises_framed
    }

    /// Write one request over the negotiated protocol (framed v2 or newline v1).
    pub(crate) fn write_request(&mut self, request: &DaemonRequest) -> Result<(), String> {
        if self.uses_framing() {
            frame::write(self.reader.get_mut(), request)
        } else {
            write_json_line(self.reader.get_mut(), request)
        }
    }

    /// Read one response over the negotiated protocol; a clean EOF before a response
    /// arrives is a clear error rather than a hang.
    pub(crate) fn read_response(&mut self) -> Result<IpcResponse, String> {
        if self.uses_framing() {
            match frame::read::<_, IpcResponse>(&mut self.reader)? {
                Some(response) => Ok(response),
                None => Err("daemon closed before responding".to_string()),
            }
        } else {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .map_err(|error| format!("failed to read daemon response: {error}"))?;
            if line.is_empty() {
                return Err("daemon closed before responding".to_string());
            }
            serde_json::from_str(line.trim_end())
                .map_err(|error| format!("invalid daemon response: {error}"))
        }
    }

    /// Send a request and read its response (one synchronous round-trip).
    pub(crate) fn request(&mut self, request: &DaemonRequest) -> Result<IpcResponse, String> {
        self.write_request(request)?;
        self.read_response()
    }

    /// Read the next event from a subscribed connection. `Ok(None)` is a clean EOF
    /// (the daemon closed the stream), letting the subscribe loops terminate
    /// cleanly. On the newline path an un-decodable line is skipped (the historical
    /// behavior — the daemon only ever writes well-formed events on a stream); on the
    /// framed path a decode error is terminal (a frame stream cannot resync mid-frame).
    pub(crate) fn read_event(&mut self) -> Result<Option<DaemonEvent>, String> {
        if self.uses_framing() {
            frame::read::<_, DaemonEvent>(&mut self.reader)
        } else {
            loop {
                let mut line = String::new();
                match self.reader.read_line(&mut line) {
                    Ok(0) => return Ok(None),
                    Ok(_) => {
                        if let Ok(event) = serde_json::from_str::<DaemonEvent>(line.trim_end()) {
                            return Ok(Some(event));
                        }
                    }
                    Err(error) => return Err(format!("failed to read daemon events: {error}")),
                }
            }
        }
    }
}

pub(crate) fn no_daemon_error(cwd: &Path) -> String {
    format!(
        "no daemon running for workspace {} (open the app or run a mutating ctl command to start one)",
        cwd.display()
    )
}

/// Check that the persisted `cwd` in `data_dir/workspace.json` (if any) matches the
/// connecting `cwd`. A mismatch indicates either a workspace_key hash collision
/// (two different cwds hashing to the same key) or data-dir tampering — in either
/// case the client must refuse rather than silently serve another workspace's
/// panes, scrollback, and token. A fresh workspace (no workspace.json) passes.
/// An unparseable file without a `workspace.cwd` marker is a refusal: the
/// previous fail-open let a colliding cwd inherit the other workspace's
/// token and scrollback (S6 follow-up).
pub(crate) fn check_persisted_cwd(cwd: &Path, data_dir: &Path) -> Result<(), String> {
    let persist_path = data_dir.join(WORKSPACE_FILE);
    let data = match fs::read(&persist_path) {
        Ok(data) => data,
        Err(_) => return Ok(()), // no persisted file — fresh workspace
    };
    let persisted_cwd = match serde_json::from_slice::<PersistedWorkspace>(&data) {
        Ok(p) => p.cwd,
        Err(_) => match read_workspace_cwd_marker(data_dir) {
            Some(marker) => marker,
            None => {
                return Err(
                    "workspace.json is unparseable and there is no workspace.cwd marker; \
                     refusing to serve this data dir. \
                     If this is intentional, remove the workspace data for this key."
                        .to_string(),
                );
            }
        },
    };
    refuse_if_cwd_mismatch(&persisted_cwd, cwd)
}

/// Daemon-side twin of `check_persisted_cwd`. Uses the parsed persist file
/// when it is valid, otherwise the on-disk marker. Must run *before* the
/// marker is rewritten for the connecting cwd, or a corrupt file plus a
/// colliding start would stamp the new cwd and skip the guard.
pub(crate) fn refuse_workspace_cwd_mismatch(
    cwd: &Path,
    data_dir: &Path,
    loaded: &LoadedWorkspace,
) -> Result<(), String> {
    let persisted = loaded
        .persisted_cwd
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| read_workspace_cwd_marker(data_dir));
    if let Some(persisted_cwd) = persisted {
        return refuse_if_cwd_mismatch(&persisted_cwd, cwd);
    }
    if loaded.was_corrupt {
        return Err(
            "workspace.json is unparseable and there is no workspace.cwd marker; \
             refusing to serve this data dir. \
             If this is intentional, remove the workspace data for this key."
                .to_string(),
        );
    }
    Ok(())
}

fn refuse_if_cwd_mismatch(persisted_cwd: &str, cwd: &Path) -> Result<(), String> {
    let connecting = canonical_workspace_path(cwd);
    if !persisted_cwd.is_empty() && !workspace_cwds_match(Path::new(persisted_cwd), cwd) {
        return Err(format!(
            "workspace_key collision detected: the persisted workspace cwd '{persisted_cwd}' does not match \
             the connecting cwd '{}'; refusing to serve mismatched workspace data. \
             If this is intentional, remove the workspace data for this key.",
            connecting.display()
        ));
    }
    Ok(())
}

pub(crate) fn read_workspace_cwd_marker(data_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(data_dir.join(WORKSPACE_CWD_FILE)).ok()?;
    let trimmed = text.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Best-effort: the marker is a hint for `check_persisted_cwd`, never the
/// source of truth while `workspace.json` parses. Write it only after the
/// collision guard has accepted this cwd.
pub(crate) fn write_workspace_cwd_marker(data_dir: &Path, cwd: &Path) {
    let path = data_dir.join(WORKSPACE_CWD_FILE);
    if let Ok(mut file) = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .private_mode()
        .open(&path)
    {
        let _ = file.write_all(cwd.display().to_string().as_bytes());
    }
}

/// Resolve the identity used by both workspace-key derivation and BOTH cwd
/// collision guards. On Windows, canonicalization also folds ordinary
/// case-insensitive path spellings to the filesystem's stored spelling, so
/// `C:\\Craig\\tools\\sgian` and `C:\\craig\\tools\\Sgian` converge. If the
/// path no longer exists, preserve the raw spelling rather than weakening the
/// collision guard with a guessed normalization.
pub(crate) fn canonical_workspace_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn workspace_cwds_match(left: &Path, right: &Path) -> bool {
    canonical_workspace_path(left) == canonical_workspace_path(right)
}

/// Create `path` (recursively) and set owner-only (0700) permissions on Unix.
/// On non-Unix the directory is created without explicit mode (OS-default ACLs).
pub(crate) fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("failed to create private directory {path:?}: {error}"))?;
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
            .map_err(|error| format!("failed to secure private directory {path:?}: {error}"))?;
    }
    Ok(())
}

/// Set owner-only (0600) permissions on an existing file on Unix.
/// On non-Unix this is a no-op (file ACLs are managed by the OS).
///
/// Symlinks are REFUSED (same-UID defense-in-depth): `fs::set_permissions`
/// follows them, so a planted symlink could chmod an arbitrary same-UID file.
/// The check uses `symlink_metadata`; a small TOCTOU window remains, but every
/// caller's file lives under a 0700 private dir only the owner can write to.
pub(crate) fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("failed to inspect private file {path:?}: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing to secure symlinked private file: {}",
                path.display()
            ));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(|error| format!("failed to secure private file {path:?}: {error}"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub(crate) fn remove_stale_socket(path: &Path) -> Result<(), String> {
    // On Unix, check that the stale path is actually a socket file before
    // removing it (refuse to delete non-socket files). On Windows, named
    // pipes are kernel objects with no filesystem residue, so stale-handle
    // cleanup is a no-op.
    #[cfg(unix)]
    {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return Ok(());
        };

        if metadata.file_type().is_socket() {
            fs::remove_file(path).map_err(|error| {
                format!("failed to remove stale daemon socket {path:?}: {error}")
            })?;
            return Ok(());
        }

        Err(format!(
            "refusing to remove non-socket daemon path: {}",
            path.display()
        ))
    }
    #[cfg(not(unix))]
    {
        // Windows: named pipes are kernel objects, not filesystem files.
        // There is no stale socket file to clean up.
        let _ = path;
        Ok(())
    }
}

/// Open (creating if necessary) the daemon lock file with owner-only `0600`
/// permissions. The file persists across restarts (it is the lock anchor); only
/// its advisory lock is acquired/released.
pub(crate) fn open_lock_file(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .private_mode()
        .open(path)
        .map_err(|error| format!("failed to open daemon lock {path:?}: {error}"))?;
    // Belt-and-suspenders: force 0600 even if the file pre-existed with
    // broader perms (e.g. created by a umask interaction). No-op on Windows.
    set_private_file_permissions(path)?;
    Ok(file)
}

/// Acquire an advisory exclusive lock on `runtime/<key>/daemon.lock` (next to the
/// socket). Returns `Ok(Some(file))` when the lock is acquired (held until the
/// `File` is dropped), `Ok(None)` when another daemon holds the lock (caller should
/// defer), or an `Err` on a real I/O failure. Uses `fs4::FileExt::try_lock`
/// (non-blocking flock) so this never blocks/hangs.
pub(crate) fn acquire_daemon_lock(socket_path: &Path) -> Result<Option<File>, String> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| "daemon socket path has no parent".to_string())?;
    let lock_path = parent.join(DAEMON_LOCK_FILE);
    let file = open_lock_file(&lock_path)?;
    // Call the fs4 trait method explicitly (fully-qualified) to disambiguate from
    // std::fs::File::try_lock (stabilized in Rust 1.89, returning a different
    // TryLockError type). fs4::FileExt::try_lock returns fs4::TryLockError.
    match fs4::FileExt::try_lock(&file) {
        Ok(()) => Ok(Some(file)),
        Err(fs4::TryLockError::WouldBlock) => Ok(None),
        Err(fs4::TryLockError::Error(error)) => Err(format!(
            "failed to acquire daemon lock {lock_path:?}: {error}"
        )),
    }
}

/// Probe whether ANY process holds the daemon flock for the workspace whose
/// socket is `socket_path` — WITHOUT creating the lock file or its parent dir
/// (unlike `acquire_daemon_lock`, which creates it; a pure probe must not
/// mutate). Returns true only when the lock file exists and another process
/// holds an exclusive flock on it (H4): that distinguishes a live-but-wedged
/// daemon (lock held, socket unresponsive) from a dead one (lock free, stale
/// socket). A successful probe acquire drops its guard immediately on return,
/// so a subsequently spawned daemon can acquire the lock itself.
pub(crate) fn daemon_lock_is_held(socket_path: &Path) -> bool {
    let Some(parent) = socket_path.parent() else {
        return false;
    };
    let lock_path = parent.join(DAEMON_LOCK_FILE);
    // Open WITHOUT create: a missing lock file means no daemon ever ran here.
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) else {
        return false;
    };
    // Fully-qualified fs4 call, same as acquire_daemon_lock (std's own try_lock
    // has a different TryLockError type).
    matches!(
        fs4::FileExt::try_lock(&file),
        Err(fs4::TryLockError::WouldBlock)
    )
}

/// Cross-platform probe for a daemon instance that is alive enough to retain
/// its single-owner guard but no longer exposes a usable transport endpoint.
/// Unix is represented by the lock file alone. Windows checks both LockFileEx
/// and the named mutex because an older/partially-started installed build can
/// retain the mutex even when its file-lock setup did not complete.
pub(crate) fn daemon_instance_is_held(cwd: &Path, socket_path: &Path) -> Result<bool, String> {
    if daemon_lock_is_held(socket_path) {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        return windows_daemon_mutex_is_held(&workspace_key(cwd));
    }
    #[cfg(not(windows))]
    {
        let _ = cwd;
        Ok(false)
    }
}

/// RAII guard for the Windows single-daemon named mutex. Releasing the mutex and
/// closing the handle on drop is the analog of the Unix flock `File` dropping.
#[cfg(windows)]
pub(crate) struct WindowsDaemonMutex {
    pub(crate) handle: windows_sys::Win32::Foundation::HANDLE,
}

// Deliberately not Send: Win32 mutex ownership belongs to the acquiring THREAD,
// and ReleaseMutex must run on that same thread. The daemon guard stays on the
// daemon main thread for its full lifetime.

#[cfg(windows)]
impl Drop for WindowsDaemonMutex {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.handle);
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

/// Acquire a per-`workspace_key` named mutex — the Windows analog of the Unix
/// `flock` single-daemon guard (`acquire_daemon_lock`). Returns `Ok(Some(guard))`
/// when this process acquired ownership (held until the guard drops), `Ok(None)`
/// when another live daemon owns it (caller should defer), or `Err` on a real
/// failure. Ownership—not mere object existence—is decisive: an existing but
/// unowned/abandoned mutex is recoverable and must be acquired. The `Local\`
/// namespace scopes the mutex to the user's session, matching per-user pipes.
#[cfg(windows)]
pub(crate) fn acquire_windows_daemon_mutex(
    workspace_key: &str,
) -> Result<Option<WindowsDaemonMutex>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};

    let name = format!("Local\\{WINDOWS_IPC_NAMESPACE}-daemon-{workspace_key}");
    let wide_name: Vec<u16> = std::ffi::OsStr::new(&name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        // Create/open without taking initial ownership, then perform a zero-time
        // wait. CreateMutex's ERROR_ALREADY_EXISTS only says some process still
        // has a HANDLE; it does not say the mutex is currently owned. Treating
        // existence as contention strands startup behind an unowned mutex.
        let handle = CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr());
        if handle.is_null() {
            return Err(format!(
                "failed to create daemon mutex: {}",
                std::io::Error::last_os_error()
            ));
        }
        match WaitForSingleObject(handle, 0) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Some(WindowsDaemonMutex { handle })),
            WAIT_TIMEOUT => {
                // Another thread/process currently owns the workspace mutex.
                CloseHandle(handle);
                Ok(None)
            }
            WAIT_FAILED => {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                Err(format!("failed to wait for daemon mutex: {error}"))
            }
            result => {
                CloseHandle(handle);
                Err(format!("unexpected daemon mutex wait result: {result}"))
            }
        }
    }
}

#[cfg(windows)]
pub(crate) fn windows_daemon_mutex_is_held(workspace_key: &str) -> Result<bool, String> {
    match acquire_windows_daemon_mutex(workspace_key)? {
        Some(guard) => {
            drop(guard);
            Ok(false)
        }
        None => Ok(true),
    }
}

pub(crate) fn load_or_create_token(data_dir: &Path) -> Result<String, String> {
    ensure_private_dir(data_dir)?;
    let token_path = data_dir.join(TOKEN_FILE);
    if let Some(token) = read_token(&token_path)? {
        set_private_file_permissions(&token_path)?;
        return Ok(token);
    }

    let token = create_token()?;
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .private_mode()
        .open(&token_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another process won the create race; re-read its token. Apply the
            // same owner-only re-chmod the normal path applies — the winner's
            // chmod may not have landed yet, and a pre-existing lax-mode file is
            // repaired either way.
            set_private_file_permissions(&token_path)?;
            return read_token(&token_path)?
                .ok_or_else(|| format!("daemon token file is empty: {}", token_path.display()));
        }
        Err(error) => return Err(format!("failed to create daemon token: {error}")),
    };

    file.write_all(token.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| format!("failed to write daemon token: {error}"))?;
    set_private_file_permissions(&token_path)?;
    Ok(token)
}

pub(crate) fn read_token(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(data) => Ok(data
            .lines()
            .next()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(ToString::to_string)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("failed to read daemon token: {error}")),
    }
}

/// (M6) Load `clients.json`; missing means none, unreadable means none with a
/// warning (a corrupt file must not lock the operator out of the root token).
pub(crate) fn load_clients_file(path: &Path) -> ClientsFile {
    match fs::read_to_string(path) {
        Ok(data) => match serde_json::from_str::<ClientsFile>(&data) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(
                    event = "clients_file_unreadable",
                    path = %path.display(),
                    error = %error,
                    "clients.json is unreadable; no client credentials are active"
                );
                ClientsFile::default()
            }
        },
        Err(_) => ClientsFile::default(),
    }
}

/// Write `clients.json` owner-only through a temp file and rename.
pub(crate) fn save_clients_file(path: &Path, file: &ClientsFile) -> Result<(), String> {
    let encoded = serde_json::to_vec_pretty(file)
        .map_err(|error| format!("failed to encode clients file: {error}"))?;
    let temp = path.with_extension("json.tmp");
    {
        let mut out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .private_mode()
            .open(&temp)
            .map_err(|error| format!("failed to write {}: {error}", temp.display()))?;
        out.write_all(&encoded)
            .and_then(|_| out.sync_all())
            .map_err(|error| format!("failed to write {}: {error}", temp.display()))?;
    }
    set_private_file_permissions(&temp)?;
    fs::rename(&temp, path)
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))
}

/// Whether `peer_uid` can answer on this platform; where it can, a failed
/// read refuses the connection (S7 of the 2026-09-20 review).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) const PEER_UID_SUPPORTED: bool = true;
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) const PEER_UID_SUPPORTED: bool = false;

/// (M6) The uid of the process at the other end of a Unix socket.
#[cfg(target_os = "macos")]
pub(crate) fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the fd is open for the stream's lifetime and both out-pointers
    // are valid for the call.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: ucred is plain data; a zeroed value is a valid out-buffer.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is open, `cred` is a struct we own and `len` says how
    // large it is.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn peer_uid(_stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    None
}

/// Fill `bytes` with cryptographically secure randomness from the OS.
#[cfg(unix)]
pub(crate) fn fill_secure_random(bytes: &mut [u8]) -> Result<(), String> {
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(bytes))
        .map_err(|error| format!("failed to read /dev/urandom: {error}"))
}

/// Windows: BCrypt's system-preferred RNG (CNG). `/dev/urandom` does not exist
/// here, so without this branch no daemon token could ever be created on
/// Windows (H5).
#[cfg(windows)]
pub(crate) fn fill_secure_random(bytes: &mut [u8]) -> Result<(), String> {
    use windows_sys::Win32::Security::Cryptography::{
        BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    };
    // SAFETY: the buffer pointer/length come from a live &mut slice; a null
    // algorithm handle + BCRYPT_USE_SYSTEM_PREFERRED_RNG selects the system RNG.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("BCryptGenRandom failed with NTSTATUS {status:#x}"))
    }
}

pub(crate) fn create_token() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    fill_secure_random(&mut bytes)
        .map_err(|error| format!("failed to create daemon token: {error}"))?;
    Ok(hex_encode(&bytes))
}

/// Constant-time comparison so the token check doesn't leak HOW MUCH of a
/// presented token matched via timing. The length early-return is not a
/// meaningful leak here: real tokens are fixed-length (64 hex chars from
/// `create_token`), so a length mismatch only reveals what the file format
/// already says. (Defense-in-depth: socket access already implies the caller
/// could read the token file, but this is one line of paranoia.)
pub(crate) fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |acc, (l, r)| acc | (l ^ r))
        == 0
}

pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// The frontend-facing name and payload for a daemon event, shared by the
/// Tauri host (`emit_daemon_event`) and `sgian serve` (server-sent events)
/// so both clients see the same contract. `None` for events the frontend
/// never needs (`subscribe_ack`).
pub(crate) fn frontend_event(event: DaemonEvent) -> Option<(&'static str, Value)> {
    Some(match event {
        DaemonEvent::PtyOutput { pane_id, data } => {
            ("pty-output", json!({ "pane_id": pane_id, "data": data }))
        }
        DaemonEvent::PaneEnded { pane_id, exit_code } => (
            "pane-ended",
            json!({ "pane_id": pane_id, "exit_code": exit_code }),
        ),
        DaemonEvent::PaneCreated { pane } => ("pane-created", json!(pane)),
        DaemonEvent::PaneClosed { pane_id } => ("pane-closed", json!({ "pane_id": pane_id })),
        DaemonEvent::PaneRenamed { pane } => ("pane-renamed", json!(pane)),
        DaemonEvent::ConfigChanged { config } => ("config-changed", json!(config)),
        // (T1) Agent state transitions ride through to the frontend verbatim.
        DaemonEvent::AgentState {
            pane_id,
            agent,
            attention,
            mode,
            unattended,
        } => (
            "agent-state",
            json!({
                "pane_id": pane_id,
                "agent": agent,
                "attention": attention,
                "mode": mode,
                "unattended": unattended,
            }),
        ),
        DaemonEvent::OutputWarning {
            pane_id,
            added,
            total,
            sample,
        } => (
            "output-warning",
            json!({ "pane_id": pane_id, "added": added, "total": total, "sample": sample }),
        ),
        DaemonEvent::AgentUsage { pane_id, usage } => {
            ("agent-usage", json!({ "pane_id": pane_id, "usage": usage }))
        }
        // The whole project table after a change; the overview groups by it.
        DaemonEvent::ProjectsChanged { projects } => {
            ("projects-changed", json!({ "projects": projects }))
        }
        // Keyboard lease transitions ride to the frontend as `lease-state`
        // (docs/design/keyboard-lease-and-ledger.md). Unknown to older
        // frontends, which ignore the name.
        DaemonEvent::LeaseState {
            pane_id,
            transition,
            holder,
            since_ms,
            note,
        } => (
            "lease-state",
            json!({
                "pane_id": pane_id,
                "transition": transition,
                "holder": holder,
                "since_ms": since_ms,
                "note": note,
            }),
        ),
        // (T2) Normalized agent conversation events. The frontend payload keeps
        // the contract's {pane_id, event} shape (the daemon-wire field is
        // `payload` only because of the enum's internal tag).
        DaemonEvent::AgentEvent { pane_id, event } => {
            ("agent-event", json!({ "pane_id": pane_id, "event": event }))
        }
        // Consumed by await_subscribe_ack before the event loop starts.
        DaemonEvent::SubscribeAck => return None,
    })
}

pub(crate) fn emit_daemon_event(app: &AppHandle, event: DaemonEvent) {
    if let Some((name, payload)) = frontend_event(event) {
        let _ = app.emit(name, payload);
    }
}

/// The decoded result of `load_workspace`: the registry, pty sizes, layout,
/// whether a persisted file was found, persisted per-pane runtime states, and
/// whether the persisted file was corrupt (unparseable).
pub(crate) struct LoadedWorkspace {
    pub(crate) registry: PaneRegistry,
    pub(crate) sizes: HashMap<String, PtySize>,
    pub(crate) layout: Option<Value>,
    pub(crate) restored_from_disk: bool,
    pub(crate) pane_states: HashMap<String, PaneRuntimeState>,
    /// (T1) Manual agent marks restored from workspace.json (empty for a fresh
    /// or corrupt workspace).
    pub(crate) agents: HashMap<String, String>,
    /// (T2) Agent-pane CLI session ids restored from workspace.json
    /// (`agents_v2`; empty for a fresh, corrupt, or pre-T2 workspace).
    pub(crate) agents_v2: HashMap<String, String>,
    pub(crate) agent_specs: HashMap<String, AgentPaneSpec>,
    /// Frozen shell profile overrides restored from workspace.json (empty for
    /// a fresh, corrupt, or pre-§4 workspace).
    pub(crate) pane_shells: HashMap<String, ShellConfig>,
    /// Held keyboard leases restored from workspace.json (empty for a fresh,
    /// corrupt, or pre-lease workspace).
    pub(crate) leases: HashMap<String, HeldLease>,
    pub(crate) projects: HashMap<String, Project>,
    pub(crate) was_corrupt: bool,
    /// The cwd recorded in the persisted workspace.json, if the file was parsed
    /// successfully. Used by the daemon-side collision check (defense-in-depth:
    /// the client also checks before connecting).
    pub(crate) persisted_cwd: Option<String>,
}

pub(crate) fn load_workspace(persist_path: &Path, cwd: String) -> LoadedWorkspace {
    let data = match fs::read(persist_path) {
        Ok(data) => data,
        Err(_) => {
            // No persisted file — fresh workspace.
            return LoadedWorkspace {
                registry: PaneRegistry::new(cwd),
                sizes: HashMap::new(),
                layout: None,
                restored_from_disk: false,
                pane_states: HashMap::new(),
                agents: HashMap::new(),
                agents_v2: HashMap::new(),
                agent_specs: HashMap::new(),
                pane_shells: HashMap::new(),
                leases: HashMap::new(),
                projects: HashMap::new(),
                was_corrupt: false,
                persisted_cwd: None,
            };
        }
    };

    match serde_json::from_slice::<PersistedWorkspace>(&data) {
        Ok(persisted) => {
            let layout = persisted.layout.clone();
            let sizes = persisted
                .sizes
                .iter()
                .map(|(pane_id, size)| (pane_id.clone(), pty_size(size.cols, size.rows)))
                .collect();
            let pane_states = persisted.pane_states.clone();
            let agents = persisted.agents.clone();
            let agents_v2 = persisted.agents_v2.clone();
            let agent_specs = persisted.agent_specs.clone();
            let pane_shells = persisted.pane_shells.clone();
            let leases = persisted.leases.clone();
            let projects = persisted.projects.clone();
            let persisted_cwd = Some(persisted.cwd.clone());
            let registry = PaneRegistry::from_persisted(persisted, cwd);
            LoadedWorkspace {
                registry,
                sizes,
                layout,
                restored_from_disk: true,
                pane_states,
                agents,
                agents_v2,
                agent_specs,
                pane_shells,
                leases,
                projects,
                was_corrupt: false,
                persisted_cwd,
            }
        }
        Err(_) => {
            // File exists but is unparseable (corrupt/truncated). Fall back safely
            // to a fresh workspace. The warning is logged by the caller after the
            // tracing dispatcher is active (see `run_daemon_with_config`).
            LoadedWorkspace {
                registry: PaneRegistry::new(cwd),
                sizes: HashMap::new(),
                layout: None,
                restored_from_disk: false,
                pane_states: HashMap::new(),
                agents: HashMap::new(),
                agents_v2: HashMap::new(),
                agent_specs: HashMap::new(),
                pane_shells: HashMap::new(),
                leases: HashMap::new(),
                projects: HashMap::new(),
                was_corrupt: true,
                persisted_cwd: None,
            }
        }
    }
}

pub(crate) fn read_scrollback(scrollback_dir: &Path, pane_id: &str) -> Option<String> {
    read_scrollback_tail(scrollback_dir, pane_id, SCROLLBACK_REPLAY_LIMIT_BYTES)
}

pub(crate) const SCROLLBACK_SEARCH_NEEDLE_MAX_BYTES: usize = 512;
pub(crate) const SCROLLBACK_SEARCH_DEFAULT_LIMIT: usize = 100;
pub(crate) const SCROLLBACK_SEARCH_MAX_LIMIT: usize = 1000;
pub(crate) const SCROLLBACK_LINES_MAX_PER_REQUEST: usize = 2000;

/// Remove terminal control sequences so search and citation see what a
/// person saw: CSI (`ESC [ … final`), OSC/DCS/APC/PM/SOS strings (to BEL or
/// `ESC \`), two-byte `ESC x` escapes, carriage returns and other C0 bytes
/// (tabs and newlines kept). Malformed sequences are dropped to end of text.
pub(crate) fn strip_terminal_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    // CSI: parameter/intermediate bytes 0x20..=0x3F, final 0x40..=0x7E.
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') | Some('X') => {
                    // String sequences end at BEL or ST (ESC \).
                    let mut previous_esc = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_esc && next == '\\') {
                            break;
                        }
                        previous_esc = next == '\u{1b}';
                    }
                }
                Some(intermediate) if ('\u{20}'..='\u{2f}').contains(&intermediate) => {
                    // nF escapes such as charset designation `ESC ( B`: intermediates
                    // 0x20..=0x2F, then one final 0x30..=0x7E.
                    for next in chars.by_ref() {
                        if ('\u{30}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            },
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// A pane's whole scrollback as plain-text lines (see `strip_terminal_controls`).
pub(crate) fn scrollback_text_lines(scrollback_dir: &Path, pane_id: &str) -> Vec<String> {
    let raw = read_scrollback_tail(scrollback_dir, pane_id, SCROLLBACK_MAX_BYTES as usize)
        .unwrap_or_default();
    let plain = strip_terminal_controls(&raw);
    let mut lines: Vec<String> = plain.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(|last| last.is_empty()) {
        lines.pop();
    }
    lines
}

/// Case-sensitive (or folded) substring search; returns `(line, text)` with
/// 1-based line numbers, at most `limit` hits.
pub(crate) fn search_lines(
    lines: &[String],
    needle: &str,
    ignore_case: bool,
    limit: usize,
) -> Vec<(usize, String)> {
    let folded_needle = ignore_case.then(|| needle.to_lowercase());
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| match &folded_needle {
            Some(folded) => line.to_lowercase().contains(folded.as_str()),
            None => line.contains(needle),
        })
        .map(|(index, line)| (index + 1, line.clone()))
        .take(limit)
        .collect()
}

/// Read at most the last `limit` bytes of a pane's scrollback, seeking to the tail
/// instead of reading the whole (up to SCROLLBACK_MAX_BYTES) file into memory, and
/// starting on a UTF-8 boundary.
pub(crate) fn read_scrollback_tail(
    scrollback_dir: &Path,
    pane_id: &str,
    limit: usize,
) -> Option<String> {
    let mut file = File::open(scrollback_path(scrollback_dir, pane_id)).ok()?;
    let len = file.metadata().ok()?.len();
    if len > limit as u64 {
        file.seek(SeekFrom::Start(len - limit as u64)).ok()?;
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data).ok()?;

    let start = utf8_boundary_at_or_after(&data, 0);
    Some(String::from_utf8_lossy(&data[start..]).to_string())
}

/// The length of `data`'s JSON string serialization (escapes included) — the
/// size it actually contributes to a serialized response. Used by the bootstrap
/// scrollback budget (H2), since control bytes escape to `\u00XX` (up to 6x).
pub(crate) fn serialized_json_len(data: &str) -> usize {
    serde_json::to_vec(data)
        .map(|encoded| encoded.len())
        .unwrap_or(usize::MAX)
}

/// Truncate `data` to at most `max_bytes`, preferring to cut just after the
/// last newline in range so a truncated scrollback replay doesn't end mid-ANSI
/// escape (same garble concern as the scrollback cap, L9), and always ending on
/// a UTF-8 char boundary. Used by the bootstrap aggregate budget (H2).
pub(crate) fn truncate_scrollback_replay(data: &mut String, max_bytes: usize) {
    if data.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !data.is_char_boundary(end) {
        end -= 1;
    }
    let cut = data[..end]
        .rfind('\n')
        .map(|newline| newline + 1)
        .unwrap_or(end);
    data.truncate(cut);
}

pub(crate) fn open_scrollback_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .private_mode()
        .open(path)
}

/// Advance `index` forward to the next UTF-8 character boundary so a byte-offset
/// slice never starts in the middle of a multibyte sequence (which would render as
/// a replacement glyph). Drops at most 3 bytes.
pub(crate) fn utf8_boundary_at_or_after(data: &[u8], mut index: usize) -> usize {
    while index < data.len() && (data[index] & 0xC0) == 0x80 {
        index += 1;
    }
    index
}

/// Write `data` to `temp_path` and atomically rename it onto `final_path`. The file
/// is created 0600. With `durable`, the temp file and the parent directory are
/// fsynced so the replacement survives a crash/power loss; scrollback caps skip
/// that (best-effort replay data on a hot path).
pub(crate) fn write_file_atomic(
    temp_path: &Path,
    final_path: &Path,
    data: &[u8],
    durable: bool,
) -> Result<(), String> {
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .private_mode()
            .open(temp_path)
            .map_err(|error| format!("failed to open temp file {temp_path:?}: {error}"))?;
        file.write_all(data)
            .map_err(|error| format!("failed to write temp file {temp_path:?}: {error}"))?;
        if durable {
            file.sync_all()
                .map_err(|error| format!("failed to sync temp file {temp_path:?}: {error}"))?;
        }
    }
    fs::rename(temp_path, final_path)
        .map_err(|error| format!("failed to replace {final_path:?}: {error}"))?;
    if durable {
        if let Some(parent) = final_path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(())
}

pub(crate) fn cap_scrollback_file(scrollback_dir: &Path, pane_id: &str) -> Result<(), String> {
    cap_scrollback_file_to(scrollback_dir, pane_id, SCROLLBACK_MAX_BYTES)
}

/// Cap a scrollback file that grew past `max_bytes`. The file is trimmed down to
/// *half* the cap, not the cap itself: trimming to the cap would make every later
/// append re-read and rewrite the whole file (a ~4000x write amplification on busy
/// panes), while the hysteresis gap buys max_bytes/2 of cheap appends per rewrite.
///
/// Concurrency (M11): the daemon's only caller is `append_scrollback`, which
/// holds the pane's append-state lock across the append AND this cap, so no
/// chunk can land in the read→rename window (which would have been lost by the
/// rename), and two caps for the same file can never share the `.ansi.tmp`
/// temp path. Crash-littered temp files are pruned at startup (`.ansi.tmp`).
pub(crate) fn cap_scrollback_file_to(
    scrollback_dir: &Path,
    pane_id: &str,
    max_bytes: u64,
) -> Result<(), String> {
    let path = scrollback_path(scrollback_dir, pane_id);
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(());
    };
    if metadata.len() <= max_bytes {
        return Ok(());
    }

    let mut file =
        File::open(&path).map_err(|error| format!("failed to open scrollback for cap: {error}"))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|error| format!("failed to read scrollback for cap: {error}"))?;
    if data.len() as u64 <= max_bytes {
        return Ok(());
    }

    let target = (max_bytes / 2).max(1) as usize;
    let keep_from = data.len().saturating_sub(target);
    let keep_from = utf8_boundary_at_or_after(&data, keep_from);
    // Advance to just past the next newline so the kept tail starts at a line
    // boundary: a byte-offset start can land mid-ANSI-escape (CSI/OSC), and a
    // replayed half-sequence garbles the terminal (L9). Worst case one extra
    // line is dropped; a tail with no newline at all keeps the byte boundary.
    let keep_from = data[keep_from..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|newline| keep_from + newline + 1)
        .filter(|&start| start < data.len())
        .unwrap_or(keep_from);
    let temp_path = path.with_extension("ansi.tmp");
    write_file_atomic(&temp_path, &path, &data[keep_from..], false)
}

pub(crate) fn scrollback_path(scrollback_dir: &Path, pane_id: &str) -> PathBuf {
    scrollback_dir.join(format!("{pane_id}.ansi"))
}

/// Remove scrollback files for panes no longer in the registry (orphans from a
/// reader racing ClosePane). With `include_temps` (startup only — no cap can be
/// in flight before the daemon serves), also remove leftover `.ansi.tmp` cap
/// litter from a crash mid-cap; the runtime sweep passes false so an in-flight
/// cap's temp file is never deleted under it.
pub(crate) fn prune_orphan_scrollback(
    scrollback_dir: &Path,
    live_pane_ids: &HashSet<String>,
    include_temps: bool,
) {
    let Ok(entries) = fs::read_dir(scrollback_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if include_temps {
            let is_cap_temp = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".ansi.tmp"));
            if is_cap_temp {
                let _ = fs::remove_file(&path);
                continue;
            }
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("ansi") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if is_valid_pane_id(stem) && !live_pane_ids.contains(stem) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// The single workspace-identity derivation: EVERY workspace key consumer goes
/// through here (data dir, runtime/socket dir, lock, Windows mutex). The path is
/// canonicalized first (M9) so `/a/b`, `/a/b/`, `/a/./b`, and symlinks to the
/// same directory all derive ONE key instead of parallel workspaces. When
/// canonicalization fails (e.g. the path does not exist yet) the raw path string
/// is hashed — a deterministic fallback, so a given raw path always maps to the
/// same key.
pub(crate) fn workspace_key(path: &Path) -> String {
    let canonical = canonical_workspace_path(path);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical.display().to_string().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// The pre-FNV workspace key (DefaultHasher/SipHash over the RAW path string).
/// Kept byte-for-byte compatible with what old builds computed — including the
/// missing canonicalization — so their data dirs can still be found (M17).
pub(crate) fn legacy_workspace_key(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.display().to_string().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Resolve the app's private data root via the `dirs` crate's `data_dir()`.
///
/// On macOS this is `~/Library/Application Support`; on Linux it follows XDG
/// (`~/.local/share`), and on Windows `%APPDATA%`. New installations append
/// `Sgian`. Existing installations continue using `Sgian2` when that directory
/// exists and the new one does not, preserving configuration, workspaces,
/// runtime locks, and connections to an older daemon during a rolling upgrade.
/// If both exist, the new root wins and they are never merged implicitly.
pub(crate) fn app_support_dir() -> PathBuf {
    app_support_dir_in(&dirs::data_dir().unwrap_or_else(std::env::temp_dir))
}

pub(crate) fn app_support_dir_in(data_root: &Path) -> PathBuf {
    let current = data_root.join(APP_SUPPORT_DIR);
    if current.exists() {
        return current;
    }
    let legacy = data_root.join(LEGACY_APP_SUPPORT_DIR);
    if legacy.exists() {
        return legacy;
    }
    current
}

pub(crate) fn ensure_app_private_roots() -> Result<(), String> {
    let app_dir = app_support_dir();
    ensure_private_dir(&app_dir)?;
    ensure_private_dir(&app_dir.join("workspaces"))?;
    ensure_private_dir(&app_dir.join(RUNTIME_DIR))
}

pub(crate) fn workspace_data_dir(workspace_key: &str) -> PathBuf {
    app_support_dir().join("workspaces").join(workspace_key)
}

pub(crate) fn workspace_data_dir_for(path: &Path, workspace_key: &str) -> PathBuf {
    workspace_data_dir_in(&app_support_dir().join("workspaces"), path, workspace_key)
}

/// Resolve a workspace's data dir under `workspaces_root`, migrating the legacy
/// `DefaultHasher`-keyed dir on first use (M17): DefaultHasher (SipHash) output
/// is not guaranteed stable across toolchain bumps, so a rustc upgrade could
/// strand a legacy-keyed data dir. When the FNV-keyed dir does not exist yet and
/// the legacy one does, rename legacy → FNV and use it. If the rename fails,
/// keep using the legacy dir (with a warning) rather than splitting state across
/// two dirs. When BOTH exist the FNV dir already won — never merge. Factored
/// from `workspace_data_dir_for` so tests can drive it against a temp root.
pub(crate) fn workspace_data_dir_in(
    workspaces_root: &Path,
    path: &Path,
    workspace_key: &str,
) -> PathBuf {
    let current = workspaces_root.join(workspace_key);
    if current.exists() {
        return current;
    }

    let legacy = workspaces_root.join(legacy_workspace_key(path));
    if legacy.exists() {
        // Logged via tracing (not eprintln): stderr is not available in every
        // process that touches this path (the daemon's is /dev/null; GUI processes
        // have no console). Wherever a tracing dispatcher is live (daemon, tests)
        // the event is captured; elsewhere it is a cheap no-op.
        return match fs::rename(&legacy, &current) {
            Ok(()) => {
                tracing::info!(
                    event = "workspace_data_dir_migrated",
                    from = %legacy.display(),
                    to = %current.display(),
                    "migrated legacy workspace data dir"
                );
                current
            }
            Err(error) => {
                tracing::warn!(
                    event = "workspace_data_dir_migration_failed",
                    error = %error,
                    from = %legacy.display(),
                    to = %current.display(),
                    "could not migrate legacy workspace data dir; keeping the legacy dir"
                );
                legacy
            }
        };
    }

    current
}

pub(crate) fn workspace_runtime_dir(workspace_key: &str) -> PathBuf {
    app_support_dir().join(RUNTIME_DIR).join(workspace_key)
}

pub(crate) fn arg_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().cloned();
        }
    }
    None
}

pub(crate) fn is_valid_pane_id(pane_id: &str) -> bool {
    pane_id
        .strip_prefix("pane-")
        .map(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or(false)
}

pub(crate) fn next_pane_id_after(panes: &[Pane]) -> u64 {
    panes
        .iter()
        .filter_map(|pane| pane.id.strip_prefix("pane-"))
        .filter_map(|suffix| suffix.parse::<u64>().ok())
        .max()
        // Saturating: a tampered `pane-18446744073709551615` must not overflow
        // (debug panic / release wrap to id reuse) — the daemon just stops
        // minting fresh numbers at the ceiling instead (L14).
        .map(|value| value.saturating_add(1))
        .unwrap_or(1)
}

pub(crate) fn clean_title(title: Option<String>) -> Option<String> {
    title
        .map(|value| {
            value
                .trim()
                .chars()
                // Strip control characters (L11): a title containing ESC/CSI
                // bytes would otherwise be stored verbatim, injected into the
                // structured log (`title = %pane.title`), and escape-injected
                // into any terminal printing a pane list.
                .filter(|c| !c.is_control())
                .take(MAX_TITLE_CHARS)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(crate) fn default_shell() -> String {
    // Windows: $SHELL and POSIX shell paths do not apply; honor %COMSPEC%
    // (normally set by the OS) and fall back to cmd.exe. portable-pty uses
    // ConPTY on Windows, which cmd/powershell run under fine.
    #[cfg(windows)]
    {
        return std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
    }

    // macOS ships zsh at /bin/zsh, while a minimal Linux installation is only
    // required to provide /bin/sh. Respect an explicit $SHELL everywhere and
    // keep the platform fallback executable on a stock installation.
    #[cfg(not(windows))]
    default_unix_shell(std::env::var("SHELL").ok(), cfg!(target_os = "macos"))
}

#[cfg(any(not(windows), test))]
pub(crate) fn default_unix_shell(shell: Option<String>, is_macos: bool) -> String {
    shell.unwrap_or_else(|| {
        if is_macos {
            "/bin/zsh".to_string()
        } else {
            "/bin/sh".to_string()
        }
    })
}

pub(crate) fn resolve_workspace_dir() -> PathBuf {
    let explicit = std::env::var_os("SGIAN_WORKSPACE").map(PathBuf::from);
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return path;
    }

    resolve_workspace_dir_from_cwd(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The cwd-based half of `resolve_workspace_dir`, factored out so tests don't
/// have to chdir the whole (parallel) test process.
pub(crate) fn resolve_workspace_dir_from_cwd(current: PathBuf) -> PathBuf {
    // (L21) Dev-loop convenience ONLY: `cargo tauri dev` runs the app with cwd =
    // `<repo>/src-tauri`, so a bare launch from a dev shell retargets the
    // workspace at the repo root. Gated to debug builds — in production a
    // directory legitimately NAMED "src-tauri" is a perfectly good workspace and
    // must not be silently retargeted.
    if cfg!(debug_assertions)
        && current.file_name().and_then(|name| name.to_str()) == Some("src-tauri")
    {
        return current.parent().map(PathBuf::from).unwrap_or(current);
    }

    current
}

#[derive(Debug)]
pub(crate) struct ControlOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) json: bool,
    pub(crate) args: Vec<String>,
}
