use super::*;

pub(crate) fn run_daemon_from_args(args: &[String]) -> Result<(), String> {
    // Detach from the launching session. Without this the daemon stays in the
    // spawning terminal's session/process group, so Ctrl+C or closing that terminal
    // would SIGINT/SIGHUP the daemon and kill every shell — the opposite of the
    // tmux-style persistence the daemon exists to provide. The usual auto-spawned
    // child is not a group leader, so setsid() just works; a daemon launched
    // directly as a foreground job IS one (setsid fails with EPERM), so fork once
    // and let the non-leader child detach instead. This runs before any threads
    // exist, so the fork is safe.
    //
    // On Windows there is no setsid/fork equivalent. Note that `ensure_daemon`
    // currently sets NO creation flags, so an auto-spawned daemon would inherit
    // the client's console until the spawn site passes `CREATE_NO_WINDOW` /
    // `DETACHED_PROCESS`. SIGHUP has no Windows analog (closing a terminal does
    // not send a signal to child processes in the same way); the daemon's
    // lifecycle is controlled via the explicit Shutdown RPC and idle timeout.
    detach_from_session();

    let workspace = arg_value(args, WORKSPACE_ARG)
        .map(PathBuf::from)
        .unwrap_or_else(resolve_workspace_dir);
    ensure_app_private_roots()?;
    let socket_path = arg_value(args, SOCKET_ARG)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_runtime_dir(&workspace_key(&workspace)).join(SOCKET_FILE));
    let data_dir = arg_value(args, DATA_DIR_ARG)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_data_dir_for(&workspace, &workspace_key(&workspace)));

    run_daemon(workspace, socket_path, data_dir)
}

/// Keep the background daemon out of a transient console on Windows. This is
/// deliberately isolated behind cfg so Unix process/session behavior remains
/// owned by `detach_from_session` in the child.
pub(crate) fn configure_daemon_process(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW. A GUI-launched background daemon must not inherit or
        // flash a console, and it must behave the same from an installed NSIS
        // executable as it does from a developer shell.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Truncate stale early-startup diagnostics before each spawn attempt. Failure
/// to create the diagnostic file must not itself prevent the daemon from
/// starting; the returned path is still useful in the surfaced error.
pub(crate) fn reset_daemon_startup_log(data_dir: &Path) -> PathBuf {
    let path = data_dir.join(DAEMON_STARTUP_LOG_FILE);
    if OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .private_mode()
        .open(&path)
        .is_ok()
    {
        let _ = set_private_file_permissions(&path);
    }
    path
}

/// Persist an error that occurs before `DaemonServer` initializes structured
/// logging. The daemon is normally spawned with stderr redirected to NUL, so
/// without this file the GUI only sees the client's final missing-pipe error.
pub(crate) fn record_daemon_startup_error(args: &[String], error: &str) {
    let Some(data_dir) = arg_value(args, DATA_DIR_ARG).map(PathBuf::from) else {
        return;
    };
    let _ = ensure_private_dir(&data_dir);
    let path = data_dir.join(DAEMON_STARTUP_LOG_FILE);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .private_mode()
        .open(&path)
    {
        let _ = writeln!(file, "{error}");
        let _ = file.flush();
        let _ = set_private_file_permissions(&path);
    }
}

pub(crate) fn daemon_startup_log_excerpt(path: &Path) -> Option<String> {
    const MAX_EXCERPT_BYTES: usize = 8 * 1024;
    let data = fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let start = data.len().saturating_sub(MAX_EXCERPT_BYTES);
    let excerpt = String::from_utf8_lossy(&data[start..]).trim().to_string();
    (!excerpt.is_empty()).then_some(excerpt)
}

pub(crate) fn format_daemon_start_failure(
    last_error: &str,
    child_status: Option<String>,
    startup_log_path: &Path,
) -> String {
    let child = child_status
        .map(|status| format!("spawned daemon exited ({status})"))
        .unwrap_or_else(|| "spawned daemon is still running but exposed no endpoint".to_string());
    let startup = daemon_startup_log_excerpt(startup_log_path)
        .map(|excerpt| format!("; startup error: {excerpt}"))
        .unwrap_or_default();
    format!(
        "daemon did not become ready: {child} \
         (last ping error: {last_error}; diagnostics: {}{startup})",
        startup_log_path.display()
    )
}

pub(crate) static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Detach the daemon process from the launching terminal session.
///
/// On Unix, calls `setsid()` (with a `fork()` fallback if the process is a
/// session leader). On Windows, this is a no-op: there is no `setsid`/`fork`
/// equivalent, and the spawning client currently sets NO creation flags, so an
/// auto-spawned daemon would inherit the client's console (until the spawn
/// site passes `CREATE_NO_WINDOW` / `DETACHED_PROCESS`). SIGHUP has no Windows
/// analog.
pub(crate) fn detach_from_session() {
    #[cfg(unix)]
    unsafe {
        if libc::setsid() == -1 {
            match libc::fork() {
                -1 => {} // out of processes: stay attached rather than abort
                0 => {
                    libc::setsid();
                }
                _ => libc::_exit(0),
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: no setsid/fork equivalent, and no creation flags are set at
        // the spawn site today — an auto-spawned daemon would inherit the
        // client's console. Closing the terminal does not send SIGHUP.
    }
}

/// Install POSIX signal handlers for graceful shutdown (SIGTERM, SIGINT, SIGHUP).
///
/// On Unix, installs `libc::signal` handlers that set the `SHUTDOWN_REQUESTED`
/// atomic so the accept loop exits cleanly. On Windows, this is a no-op:
/// Windows has no POSIX signal analog. SIGHUP has no Windows equivalent.
/// Ctrl+C / Ctrl+Break could be handled via `SetConsoleCtrlHandler`, but the
/// daemon's primary lifecycle control is the explicit Shutdown RPC and idle
/// timeout, so a no-op is safe.
pub(crate) fn install_shutdown_signal_handlers() {
    #[cfg(unix)]
    {
        let handler = handle_shutdown_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        unsafe {
            libc::signal(libc::SIGTERM, handler);
            libc::signal(libc::SIGINT, handler);
            libc::signal(libc::SIGHUP, handler);
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: no POSIX signals. SIGHUP has no analog. The daemon relies
        // on the Shutdown RPC and idle_timeout for lifecycle control.
    }
}

#[cfg(unix)]
extern "C" fn handle_shutdown_signal(_signal: libc::c_int) {
    // Async-signal-safe: only an atomic store.
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Set up a structured log writer in `log_dir`. The log file is opened in
/// append mode (history survives restart) and is rotated to `daemon.log.old` —
/// both at startup (if the pre-existing file exceeds `LOG_MAX_BYTES`) AND during
/// operation (the `BoundedFileWriter` checks the cap on every write and rotates
/// when it is exceeded). This keeps total log storage bounded at ~2×
/// `LOG_MAX_BYTES` across a long-lived daemon session.
///
/// Logging is best-effort: if the log file cannot be opened (e.g. an
/// unwritable/inaccessible log directory), `setup_log_writer` does NOT panic.
/// It falls back to stderr logging and returns a usable `NonBlocking` writer so
/// the daemon starts and serves normally.
pub(crate) fn setup_log_writer(log_dir: &Path) -> (NonBlocking, WorkerGuard) {
    match BoundedFileWriter::new(log_dir) {
        Ok(writer) => tracing_appender::non_blocking(writer),
        Err(error) => {
            eprintln!(
                "warning: failed to initialize daemon log file in {}: {error}; \
                 falling back to stderr logging",
                log_dir.display()
            );
            tracing_appender::non_blocking(std::io::stderr())
        }
    }
}

/// Open (or create) a log file in append mode with owner-only (0600) permissions
/// on Unix (no-op on Windows via the centralized `private_mode` helper).
pub(crate) fn open_log_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .private_mode()
        .open(path)
}

/// A bounded file writer that rotates the log file when it exceeds
/// `LOG_MAX_BYTES` during operation (not just at startup). The active file is
/// always `daemon.log`; when rotation occurs, the current file is renamed to
/// `daemon.log.old` (replacing any previous `.old`), and a new `daemon.log` is
/// opened with 0600 permissions. This keeps total log storage bounded at
/// ~2× `LOG_MAX_BYTES` while preserving the `daemon.log` filename so `ctl logs`
/// continues to work against the active file.
///
/// Each `write` call flushes immediately so log entries are visible on disk
/// without waiting for the `WorkerGuard` to drop (so `ctl logs` can tail a live
/// daemon's log). If rotation fails (e.g. the directory becomes unwritable
/// mid-session), the writer continues writing to the current file — logging is
/// best-effort and never panics.
pub(crate) struct BoundedFileWriter {
    pub(crate) dir: PathBuf,
    pub(crate) file: Option<File>,
    pub(crate) written: u64,
}

impl BoundedFileWriter {
    /// Create a new `BoundedFileWriter` for `dir`. Rotates the pre-existing
    /// `daemon.log` to `daemon.log.old` if it exceeds `LOG_MAX_BYTES` at startup,
    /// then opens (or creates) `daemon.log` in append mode with 0600 perms.
    pub(crate) fn new(dir: &Path) -> std::io::Result<Self> {
        let log_path = dir.join(LOG_FILE);

        // Rotate the previous log if it exceeded the size threshold (startup
        // rotation, preserving the existing bounded-growth behavior).
        if let Ok(metadata) = fs::metadata(&log_path) {
            if metadata.len() > LOG_MAX_BYTES {
                let _ = fs::rename(&log_path, dir.join(format!("{LOG_FILE}.old")));
            }
        }

        let file = open_log_file(&log_path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);

        Ok(Self {
            dir: dir.to_path_buf(),
            file: Some(file),
            written,
        })
    }

    /// Rotate: close the current file, rename it to `daemon.log.old` (replacing
    /// any previous `.old`), and open a fresh `daemon.log`. Best-effort — if
    /// rotation fails, the writer keeps the current file open and continues
    /// writing to it (logging never panics).
    pub(crate) fn rotate(&mut self) {
        // Close the current file first so the rename succeeds on all platforms.
        self.file = None;
        self.written = 0;

        let log_path = self.dir.join(LOG_FILE);
        let old_path = self.dir.join(format!("{LOG_FILE}.old"));

        // Remove any previous .old file, then rename the current log to .old.
        let _ = fs::remove_file(&old_path);
        let _ = fs::rename(&log_path, &old_path);

        // Open a fresh log file. If this fails, leave file as None — writes
        // will be silently dropped (best-effort logging).
        match open_log_file(&log_path) {
            Ok(file) => {
                self.file = Some(file);
                self.written = 0;
            }
            Err(_) => {
                // Best-effort: logging is non-essential. The daemon continues.
            }
        }
    }
}

impl Write for BoundedFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Check if this write would exceed the cap. If so, rotate first.
        if self.written + (buf.len() as u64) > LOG_MAX_BYTES {
            self.rotate();
        }

        // A single write larger than the cap must not blow PAST it (L15): write
        // at most `cap` bytes and report the honest short count — a write_all
        // caller retries the remainder, which lands after the next rotation.
        let buf = if buf.len() as u64 > LOG_MAX_BYTES {
            &buf[..LOG_MAX_BYTES as usize]
        } else {
            buf
        };

        if let Some(ref mut file) = self.file {
            let n = file.write(buf)?;
            self.written += n as u64;
            // Flush immediately so entries are visible to `ctl logs` on a live
            // daemon (no userspace buffering — goes straight to the OS).
            let _ = file.flush();
            Ok(n)
        } else {
            // No file open (rotation failed and reopen also failed). The data is
            // dropped — best-effort logging never panics — but report the honest
            // count (0) instead of over-reporting Ok(len) (L15); a write_all
            // caller surfaces this as WriteZero, the same degraded outcome as
            // any other I/O error here.
            Ok(0)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(ref mut file) = self.file {
            file.flush()
        } else {
            Ok(())
        }
    }
}

/// How the accept loop treats a non-WouldBlock accept error (M8). Transient
/// per-connection failures (ECONNABORTED — a peer that connected and reset
/// before accept, which macOS/BSD surface readily — and EINTR) must not kill
/// the daemon and every shell it owns; fd/memory pressure (EMFILE/ENFILE/
/// ENOBUFS/ENOMEM) gets a brief backoff so a hot error loop can't spin at
/// 100% CPU while the pressure persists; anything else stays fatal (the
/// listener itself is broken).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AcceptErrorClass {
    Transient,
    // Only constructed from unix raw OS errors (EMFILE/ENFILE/ENOBUFS/ENOMEM).
    #[cfg_attr(not(unix), allow(dead_code))]
    ResourcePressure,
    Fatal,
}

pub(crate) fn classify_accept_error(error: &std::io::Error) -> AcceptErrorClass {
    match error.kind() {
        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted => {
            AcceptErrorClass::Transient
        }
        _ => {
            #[cfg(unix)]
            if matches!(
                error.raw_os_error(),
                Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM)
            ) {
                return AcceptErrorClass::ResourcePressure;
            }
            AcceptErrorClass::Fatal
        }
    }
}

pub(crate) fn run_daemon(
    cwd: PathBuf,
    socket_path: PathBuf,
    data_dir: PathBuf,
) -> Result<(), String> {
    let (config, config_warnings) = load_config(&data_dir);
    // There is no previous safe policy at startup. Ignoring a broken layer
    // could discard scrub_env or inherit a more permissive agent mode.
    if !config_warnings.is_empty() {
        return Err(format!(
            "refusing to start with invalid configuration: {}",
            config_warnings.join("; ")
        ));
    }
    run_daemon_with_config_and_warnings(cwd, socket_path, data_dir, config, config_warnings)
}

/// Set up a `notify` file watcher on the per-workspace and global config files.
/// On any modify/create event affecting `config.json`, a unit `()` is sent to
/// the channel; the daemon's accept loop polls this channel and calls
/// `reload_config` on change. The returned watcher must be held alive for the
/// duration of the daemon (it is a local in `run_daemon_with_config`). If
/// watcher initialization fails, logging is best-effort and the daemon
/// continues with frozen config (degraded but safe).
///
/// We watch the *parent directory* of each config file (in NonRecursive mode)
/// rather than the file itself, because the config file may not exist yet when
/// the daemon starts. Events are filtered to those whose path ends with
/// `config.json` so unrelated files in the same directory (workspace.json,
/// scrollback, etc.) do not trigger a reload.
pub(crate) fn setup_config_watcher(
    data_dir: &Path,
    notify_tx: std::sync::mpsc::Sender<()>,
) -> Option<notify::RecommendedWatcher> {
    let config_file_name = CONFIG_FILE.to_string();
    let mut watcher = match notify::RecommendedWatcher::new(
        move |res: Result<notify::Event, notify::Error>| {
            if let Ok(event) = res {
                if matches!(
                    event.kind,
                    notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                ) {
                    // Only react to changes on config.json (not workspace.json,
                    // scrollback, etc. in the same directory).
                    if event.paths.iter().any(|p| {
                        p.file_name()
                            .is_some_and(|n| n == config_file_name.as_str())
                    }) {
                        let _ = notify_tx.send(());
                    }
                }
            }
        },
        notify::Config::default(),
    ) {
        Ok(w) => w,
        Err(error) => {
            tracing::warn!(
                event = "config_watch_init_failed",
                error = %error,
                "failed to initialize config file watcher; config will stay frozen"
            );
            return None;
        }
    };

    // Watch the per-workspace data dir (parent of config.json).
    if let Err(error) = watcher.watch(data_dir, notify::RecursiveMode::NonRecursive) {
        tracing::warn!(
            event = "config_watch_failed",
            path = %data_dir.display(),
            error = %error,
            "failed to watch per-workspace config directory"
        );
    }

    // Watch the global config's parent directory (best-effort — app_support_dir
    // may not exist in isolated test environments).
    let global_dir = app_support_dir();
    let _ = watcher.watch(&global_dir, notify::RecursiveMode::NonRecursive);

    Some(watcher)
}

/// Run the daemon with an explicit, injected `Config`. Tests use this so they never
/// read the developer's real global `~/Library/Application Support/Sgian/config.json`
/// (the hermetic-config invariant). `run_daemon` is the production wrapper that loads
/// config from disk (with warnings); this test-only shim injects none.
#[cfg(test)]
pub(crate) fn run_daemon_with_config(
    cwd: PathBuf,
    socket_path: PathBuf,
    data_dir: PathBuf,
    config: Config,
) -> Result<(), String> {
    run_daemon_with_config_and_warnings(cwd, socket_path, data_dir, config, Vec::new())
}

/// The daemon core, plus any config-load warnings from `run_daemon` to surface
/// once the tracing dispatcher is live (a malformed config.json must be loud —
/// M2 — but at load time there is nowhere to log yet).
pub(crate) fn run_daemon_with_config_and_warnings(
    cwd: PathBuf,
    socket_path: PathBuf,
    data_dir: PathBuf,
    config: Config,
    config_warnings: Vec<String>,
) -> Result<(), String> {
    // Ensure the runtime dir (socket parent) exists and is private before creating
    // the daemon lock file in it. This must precede lock acquisition.
    if let Some(parent) = socket_path.parent() {
        ensure_private_dir(parent)?;
    }

    // Windows single-daemon guard: a per-`workspace_key` named mutex, the analog
    // of the Unix flock below (Win32 named pipes have no flock equivalent). Held
    // for the daemon's lifetime; on contention we defer to the existing owner,
    // mirroring the flock `Ok(None)` defer. cfg(windows)-only so the Unix path is
    // byte-for-byte unchanged.
    #[cfg(windows)]
    let _windows_daemon_mutex = match acquire_windows_daemon_mutex(&workspace_key(&cwd))? {
        Some(guard) => guard,
        None => return Ok(()),
    };

    // Acquire an advisory exclusive lock (flock via fs4::FileExt::try_lock) on
    // `runtime/<key>/daemon.lock`, held across the connect-check + bind window AND
    // the daemon lifetime. Under concurrent cold start exactly one daemon acquires
    // the lock and owns the socket; the other defers (returns Ok). A stale lock
    // from a crashed daemon is auto-released by the kernel (flock is tied to the
    // open file description, released on fd close / process death), so the next
    // start reacquires without a stale-recovery path. Clean shutdown drops the
    // File, releasing the lock. The `_lock_file` local lives for the whole run.
    //
    // Bounded lock wait: a spawn issued immediately after a clean `ctl shutdown`
    // races with the dying daemon's teardown. The dying daemon still holds the
    // flock for a brief window while it kills PTYs, flushes state, and drops the
    // lock File. A freshly-spawned daemon that finds the lock held must NOT defer
    // instantly (that left the client waiting 2s for a daemon that never bound,
    // producing "daemon did not become ready"). Instead it waits, retrying the
    // acquire for a bounded window: if the lock frees up (dying daemon released
    // it), this daemon takes over and binds; if a live daemon is already serving
    // on the socket (concurrent cold start), it defers promptly via the connect
    // check. The window stays well under the client's `DAEMON_CONNECT_RETRIES`
    // budget so a re-spawn after shutdown binds before the client gives up.
    let _lock_file: Option<File> = {
        let deadline = Instant::now() + DAEMON_LOCK_WAIT;
        loop {
            match acquire_daemon_lock(&socket_path)? {
                Some(file) => break Some(file),
                None => {
                    // A live daemon serving on the socket owns the lock; defer to
                    // it rather than waiting out the window (concurrent cold start
                    // / duplicate spawn). The connect check is cheap and lets the
                    // loser exit promptly instead of blocking for the full wait.
                    if transport_connect(&socket_path).is_ok() {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        // The lock is still held and no daemon is serving. Give up
                        // gracefully rather than hanging the client indefinitely;
                        // the client's bounded retry will surface a clear error.
                        return Ok(());
                    }
                    thread::sleep(DAEMON_LOCK_POLL);
                }
            }
        }
    };

    // If a daemon is already accepting on this socket, defer to it rather than
    // removing its socket and binding our own (which would orphan it). With the
    // lock held this is a defensive no-op (no other daemon can be listening while
    // we hold the lock), but it covers the legacy/edge case cheaply.
    if transport_connect(&socket_path).is_ok() {
        return Ok(());
    }
    remove_stale_socket(&socket_path)?;

    let listener =
        transport_bind(&socket_path).map_err(|error| format!("failed to bind daemon: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("failed to configure daemon listener: {error}"))?;
    set_private_file_permissions(&socket_path)?;
    let server = Arc::new(DaemonServer::with_config(
        cwd.clone(),
        data_dir.clone(),
        config,
    )?);
    // Watch the notes roots of the projects restored from disk.
    server.refresh_notes_watch();

    // Activate structured logging for the daemon's main thread. The guard lives
    // for the entire run so all tracing calls in the accept loop are captured.
    let _log_guard = tracing::dispatcher::set_default(&server.log_dispatch);

    // (M3b) The official agent probe runs on its own thread so a slow
    // `claude agents --json` never touches the accept loop; it re-reads the
    // interval each round so a config reload takes effect without a restart.
    // It holds only a Weak reference and sleeps in short slices: a strong Arc
    // parked in a one-second sleep would keep the server (and its log guard,
    // whose drop flushes `daemon_shutdown`) alive after the accept loop ended.
    #[cfg(unix)]
    {
        let probe_server = Arc::downgrade(&server);
        thread::spawn(move || {
            let mut next_round = Instant::now();
            loop {
                let Some(server) = probe_server.upgrade() else {
                    return;
                };
                if server.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                if Instant::now() >= next_round {
                    let _guard = tracing::dispatcher::set_default(&server.log_dispatch);
                    match server.effective_config().agent_probe_interval() {
                        Some(interval) => {
                            server.run_agent_probe(interval);
                            next_round = Instant::now() + interval;
                        }
                        None => next_round = Instant::now() + Duration::from_secs(1),
                    }
                }
                drop(server);
                thread::sleep(Duration::from_millis(50));
            }
        });
    }

    install_shutdown_signal_handlers();
    tracing::info!(
        workspace_key = %server.workspace_key,
        event = "daemon_start",
        pid = std::process::id(),
        cwd = %cwd.display(),
        "daemon started"
    );

    // Surface config-load warnings from startup (malformed global/workspace
    // config.json). The daemon runs with that layer treated as absent, but the
    // misconfiguration must be visible — silently-defaulted config previously
    // discarded scrub_env/shell/idle settings with no trace (M2).
    for warning in &config_warnings {
        tracing::warn!(
            workspace_key = %server.workspace_key,
            event = "config_malformed",
            warning = %warning,
            "config.json is malformed; that layer is ignored"
        );
    }

    // Surface a clear warning if the persisted workspace.json was corrupt. The
    // daemon fell back to a fresh workspace (in `load_workspace`), but the silent
    // .ok() parse-failure path must surface a clear log, not fail silently.
    if server.workspace_was_corrupt {
        tracing::warn!(
            workspace_key = %server.workspace_key,
            event = "workspace_corrupt",
            "persisted workspace.json was corrupt; reseeding to a fresh workspace"
        );
    }

    // Surface a clear warning for an unrecognized restore_policy value. The
    // effective policy falls back to auto_respawn, but the misconfiguration
    // must be visible in the log (no silent fallback).
    if let Some(policy) = server.effective_config().restore_policy.as_deref() {
        if !matches!(policy, "auto_respawn" | "restore_on_demand") {
            tracing::warn!(
                workspace_key = %server.workspace_key,
                event = "restore_policy_invalid",
                policy = policy,
                "unrecognized restore_policy; falling back to auto_respawn"
            );
        }
    }

    // Apply the restore policy on daemon start. Under `auto_respawn` (the
    // default), ended panes are revived immediately. Under `restore_on_demand`,
    // they stay ended until explicitly revived. A fresh workspace always seeds
    // one live pane. This ensures the `ctl` surface (which never calls
    // BootstrapWorkspace) sees the correct pane states right after daemon start.
    // The BootstrapWorkspace handler's own spawn-on-bootstrap flag is taken here
    // so the frontend path is a no-op (panes already spawned).
    let should_spawn = {
        let mut guard = server
            .spawn_on_bootstrap
            .lock()
            .map_err(|_| "daemon bootstrap lock poisoned".to_string())?;
        let value = *guard;
        *guard = false;
        value
    };
    if should_spawn {
        let snapshot = server.snapshot()?;
        let pane_ids: Vec<String> = snapshot.panes.iter().map(|pane| pane.id.clone()).collect();
        // (M7) Spawns run their fork/exec off the TerminalStore lock, so a slow
        // spawn here can't stall input/resize/liveness for other panes.
        if let Err(error) = server.ensure_terminals(&pane_ids) {
            tracing::warn!(
                workspace_key = %server.workspace_key,
                event = "bootstrap_spawn_failed",
                error = %error,
                "failed to spawn some panes on bootstrap"
            );
        }
    }

    let mut idle_since: Option<Instant> = None;
    let mut last_lazy_flush = Instant::now();
    let mut last_closed_sweep = Instant::now();

    // Live count of connection threads (request phase, persistent v2 sessions,
    // and blocked waits — a Subscribe hand-off exits its thread and is counted
    // by subscriber_count instead). Guards the concurrency cap (L18) and keeps
    // idle shutdown from firing under a blocked `ctl wait` or an idle persistent
    // connection (M7).
    let active_connections = Arc::new(AtomicUsize::new(0));
    struct ConnectionGuard(Arc<AtomicUsize>);
    impl Drop for ConnectionGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    // Set up config file watch (notify crate). On config.json change, reload
    // config into the daemon's mutable shared state and broadcast a
    // ConfigChanged event. Config is no longer frozen at construction.
    // VAL-CFG-011 / VAL-CROSS-007.
    //
    // The watcher is polled in the accept loop (not a separate thread) so it
    // is dropped when the daemon exits — no Arc leak or orphaned watcher thread.
    let (config_notify_tx, config_notify_rx) = channel::<()>();
    let _config_watcher = setup_config_watcher(&data_dir, config_notify_tx);

    while !server.should_shutdown() && !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        // Flush lazily-persisted state (resize/focus churn) at most once per interval
        // instead of fsyncing workspace.json on every such request.
        if last_lazy_flush.elapsed() >= LAZY_PERSIST_INTERVAL && server.take_dirty() {
            if let Err(error) = server.persist() {
                // Re-mark dirty so the NEXT tick retries the flush: take_dirty
                // already cleared the flag, so a transient persist failure
                // would otherwise silently drop the pending state.
                server.mark_dirty();
                tracing::warn!(
                    workspace_key = %server.workspace_key,
                    event = "persist_failed",
                    error = %error,
                    "lazy persist failed; will retry"
                );
            }
            last_lazy_flush = Instant::now();
        }

        match listener.accept() {
            Ok((stream, _address)) => {
                idle_since = None;
                // (M6) Same-user boundary made explicit: a peer running as
                // another uid is dropped before the hello (Unix only; Windows
                // pipes are owner-restricted at creation).
                #[cfg(unix)]
                {
                    // SAFETY: getuid has no preconditions and cannot fail.
                    let own = unsafe { libc::getuid() };
                    match peer_uid(&stream) {
                        Some(uid) if uid != own => {
                            tracing::warn!(
                                workspace_key = %server.workspace_key,
                                event = "peer_uid_rejected",
                                peer_uid = uid,
                                "dropping connection from another user"
                            );
                            drop(stream);
                            continue;
                        }
                        // Where the platform can answer, an unanswered
                        // question is a refusal, not a pass (S7).
                        None if PEER_UID_SUPPORTED => {
                            tracing::warn!(
                                workspace_key = %server.workspace_key,
                                event = "peer_uid_unreadable",
                                "dropping connection whose peer uid could not be read"
                            );
                            drop(stream);
                            continue;
                        }
                        _ => {}
                    }
                }
                // Concurrency cap (L18): refuse connections beyond the bound
                // instead of pinning an unbounded number of threads.
                let active_count = active_connections.load(Ordering::SeqCst);
                let subscriber_count = server.router.subscriber_count();
                if active_count >= MAX_CONCURRENT_CONNECTIONS
                    || live_transport_limit_reached(active_count, subscriber_count)
                {
                    tracing::warn!(
                        workspace_key = %server.workspace_key,
                        event = "connection_limit",
                        active_connections = active_count,
                        subscribers = subscriber_count,
                        active_limit = MAX_CONCURRENT_CONNECTIONS,
                        live_transport_limit = MAX_LIVE_TRANSPORTS,
                        "connection limit reached; dropping new connection"
                    );
                    drop(stream);
                    continue;
                }
                let _ = stream.set_nonblocking(false);
                let server = Arc::clone(&server);
                active_connections.fetch_add(1, Ordering::SeqCst);
                let guard = ConnectionGuard(Arc::clone(&active_connections));
                // (M8) std::thread::spawn PANICS on OS thread-creation failure,
                // which would unwind and kill the accept loop; Builder::spawn
                // returns the error instead. On failure the closure drops here,
                // closing the stream and freeing the connection slot.
                let client_workspace_key = server.workspace_key.clone();
                if let Err(error) = std::thread::Builder::new().spawn(move || {
                    let _guard = guard;
                    if let Err(error) = handle_daemon_client(Arc::clone(&server), stream) {
                        tracing::warn!(
                            workspace_key = %server.workspace_key,
                            event = "client_error",
                            error = %error,
                            "client connection ended with an error"
                        );
                    }
                }) {
                    tracing::error!(
                        workspace_key = %client_workspace_key,
                        event = "connection_thread_spawn_failed",
                        error = %error,
                        "failed to spawn connection thread; dropping connection"
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // Poll for config file-watch events. On change, reload config
                // into mutable shared state and broadcast ConfigChanged.
                // Debounce: drain additional events (write_file_atomic's
                // temp→rename may fire multiple events in quick succession).
                if config_notify_rx.try_recv().is_ok() {
                    while config_notify_rx.try_recv().is_ok() {}
                    server.reload_config();
                }
                // Shared context notes edited outside the daemon.
                server.drain_notes_changes();

                // Prune old closed-pane suppression entries (L13).
                if last_closed_sweep.elapsed() >= CLOSED_SWEEP_INTERVAL {
                    server.router.sweep_closed(CLOSED_PANE_RETENTION);
                    // (review-low) Prune scrollback orphans on the same cadence,
                    // not just at startup: a reader racing ClosePane (preempted
                    // between the is_closed check and the append) can re-create
                    // a closed pane's deleted file and a cached append handle,
                    // which would otherwise linger until daemon restart.
                    if let Ok(registry) = server.lock_registry() {
                        let live_pane_ids: HashSet<String> =
                            registry.panes.iter().map(|pane| pane.id.clone()).collect();
                        drop(registry);
                        prune_orphan_scrollback(&server.scrollback_dir, &live_pane_ids, false);
                        // (T2) M3: keep the agent-log sweep consistent with it.
                        prune_orphan_agent_logs(&server.agents_dir, &live_pane_ids);
                        server.router.prune_orphan_append_handles(&live_pane_ids);
                    }
                    last_closed_sweep = Instant::now();
                }

                // Optional idle shutdown: reap the daemon (and its shells) after no client
                // has been connected for idle_limit seconds. Disabled when idle_limit == 0.
                // Read from the live config each tick so a file-watch reload takes
                // effect immediately (M4) instead of being frozen at startup.
                // "No client" means no subscriber AND no active connection thread:
                // a blocked `ctl wait` or an idle persistent v2 connection is a
                // live client and must hold the daemon open (M7).
                let idle_limit = server.effective_config().idle_shutdown_secs_effective();
                if idle_limit > 0 {
                    if server.router.subscriber_count() == 0
                        && active_connections.load(Ordering::SeqCst) == 0
                    {
                        let since = *idle_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(idle_limit) {
                            tracing::info!(
                                workspace_key = %server.workspace_key,
                                event = "idle_shutdown",
                                "daemon idle-timeout shutdown"
                            );
                            break;
                        }
                    } else {
                        idle_since = None;
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => match classify_accept_error(&error) {
                // (M8) Per-accept errors must not kill the daemon and every
                // shell it owns: a peer that aborted before accept or an
                // interrupted syscall is logged and skipped.
                AcceptErrorClass::Transient => {
                    tracing::warn!(
                        workspace_key = %server.workspace_key,
                        event = "accept_error",
                        error = %error,
                        "transient accept error; continuing"
                    );
                }
                // fd/memory pressure: back off briefly so a hot error loop
                // can't spin at 100% CPU while the pressure persists.
                AcceptErrorClass::ResourcePressure => {
                    tracing::warn!(
                        workspace_key = %server.workspace_key,
                        event = "accept_error",
                        error = %error,
                        "accept under resource pressure; backing off"
                    );
                    thread::sleep(Duration::from_millis(100));
                }
                AcceptErrorClass::Fatal => {
                    tracing::error!(
                        workspace_key = %server.workspace_key,
                        event = "listener_error",
                        error = %error,
                        "listener error"
                    );
                    // This early exit skips the post-loop teardown: flush persisted
                    // state and kill the shells directly (see the post-loop comments).
                    let _ = server.take_dirty();
                    let _ = server.persist();
                    if let Ok(mut terminals) = server.lock_terminals() {
                        terminals.kill_all_sessions();
                    }
                    return Err(format!("daemon listener failed: {error}"));
                }
            },
        }
    }

    // Unlink the socket first: a replacement daemon spawned while this one tears down
    // binds a fresh socket at the same path, and a late unlink here would delete the
    // replacement's socket instead of ours.
    let _ = fs::remove_file(&socket_path);
    drop(listener);
    // Final persist is UNCONDITIONAL (L16): gating on take_dirty raced an
    // in-flight handler's mark_dirty, silently dropping its state on shutdown.
    // persist() reads current state, so an unconditional write is always right;
    // clear the flag too so nothing appears pending.
    let _ = server.take_dirty();
    if let Err(error) = server.persist() {
        tracing::warn!(
            workspace_key = %server.workspace_key,
            event = "persist_failed",
            error = %error,
            "final persist failed at shutdown"
        );
    }

    // Kill the shells DIRECTLY rather than relying on TerminalSession::drop:
    // lingering connection threads hold Arc<DaemonServer> clones, which can defer
    // the store's drop indefinitely and leak SIGHUP-ignoring children (L17).
    if let Ok(mut terminals) = server.lock_terminals() {
        terminals.kill_all_sessions();
    }

    tracing::info!(
        workspace_key = %server.workspace_key,
        event = "daemon_shutdown",
        "daemon stopped"
    );
    Ok(())
}

/// Negotiate a connection's wire version: the minimum of the client's advertised
/// maximum (absent ⇒ 1, the legacy newline path) and the daemon's maximum
/// (`DAEMON_MAX_WIRE_VERSION`). Pure `min` so the result never exceeds either
/// side's maximum (architecture.md §5.2; VAL-IPC-014/016/020).
pub(crate) fn negotiate_wire_version(client_max_wire_version: Option<u16>) -> u16 {
    client_max_wire_version
        .unwrap_or(1)
        .min(DAEMON_MAX_WIRE_VERSION)
}

/// Capabilities the daemon advertises in the handshake response so clients/SDKs can
/// feature-detect (architecture.md §5.2): `framed` = the v2 length-prefixed envelope
/// is supported; `persistent` = a v2 connection may carry multiple sequential
/// requests. Returned alongside `negotiated_wire_version` in the response `result`.
pub(crate) fn daemon_capabilities() -> Vec<String> {
    vec![
        "framed".to_string(),
        "persistent".to_string(),
        "subscribe-ack".to_string(),
        // Keyboard lease requests, LeaseState events, snapshot `leases`.
        "lease".to_string(),
    ]
}

/// The capabilities a client advertises in its hello. `subscribe-ack` asks the
/// daemon to acknowledge Subscribe registration (M8); old daemons ignore it.
pub(crate) fn client_capabilities() -> Vec<String> {
    vec!["subscribe-ack".to_string()]
}

/// Poll a wait's client connection for liveness WITHOUT consuming any data (M6):
/// a `wait` without `--timeout` parks its handler thread inside the condition
/// loop, never reading the socket, so a disconnected client would otherwise pin
/// the connection slot forever. On unix a `recv` with MSG_PEEK|MSG_DONTWAIT
/// distinguishes the cases: `0` = orderly peer shutdown (gone); `1` = pending
/// data (alive — a synchronous client never pipelines mid-wait, and the byte is
/// left for the next frame read); WouldBlock/Interrupted = alive but quiet;
/// any other error (ECONNRESET, ENOTCONN, …) = gone. Windows uses the analogous
/// non-consuming `PeekNamedPipe` probe below.
#[cfg(unix)]
pub(crate) fn wait_peer_disconnected(stream: &TransportStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut byte: libc::c_uchar = 0;
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            &mut byte as *mut libc::c_uchar as *mut libc::c_void,
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    if result == 0 {
        return true;
    }
    if result > 0 {
        return false;
    }
    // Alive when merely quiet (WouldBlock/Interrupted); gone on any other error
    // (ECONNRESET, ENOTCONN, …).
    !matches!(
        std::io::Error::last_os_error().kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

/// Windows named-pipe analog of the Unix MSG_PEEK liveness probe.
#[cfg(windows)]
pub(crate) fn wait_peer_disconnected(stream: &TransportStream) -> bool {
    stream.is_peer_disconnected()
}

/// Fallback for any future transport without a non-consuming liveness probe.
#[cfg(not(any(unix, windows)))]
pub(crate) fn wait_peer_disconnected(_stream: &TransportStream) -> bool {
    false
}

pub(crate) fn handle_daemon_client(
    server: Arc<DaemonServer>,
    stream: TransportStream,
) -> Result<(), String> {
    handle_daemon_client_with_handshake_budget(server, stream, HANDSHAKE_READ_TIMEOUT)
}

/// `handshake_budget` is the TOTAL deadline for the hello/auth phase (L18) — a
/// parameter rather than the const directly so tests can drive it with
/// millisecond budgets.
pub(crate) fn handle_daemon_client_with_handshake_budget(
    server: Arc<DaemonServer>,
    mut stream: TransportStream,
    handshake_budget: Duration,
) -> Result<(), String> {
    // Activate structured logging for this per-connection thread.
    let _log_guard = tracing::dispatcher::set_default(&server.log_dispatch);

    // (L18) The hello/auth phase gets ONE total deadline, not a per-read timeout:
    // HandshakeDeadline re-arms the REMAINING budget before every underlying read,
    // so a peer that connects and never speaks — or a slowloris dribbling bytes
    // just under each per-read timeout — is cut off at the deadline instead of
    // pinning this per-connection thread indefinitely.
    let handshake_started = Instant::now();
    let hello_line = {
        let mut deadline = HandshakeDeadline::new(&mut stream, handshake_started, handshake_budget);
        let mut reader = BufReader::new(&mut deadline);
        read_ipc_line(&mut reader)?
    };
    let hello: IpcHello = serde_json::from_str(&hello_line)
        .map_err(|error| format!("invalid daemon hello: {error}"))?;

    let (hello_response, negotiated_wire_version, identity) = match server.authenticate(&hello) {
        Ok((negotiated, identity)) => (
            IpcResponse {
                ok: true,
                // Extend (do NOT replace) the pre-existing protocol_version field
                // with the negotiated wire version + capabilities (VAL-IPC-026).
                result: json!({
                    "protocol_version": PROTOCOL_VERSION,
                    "negotiated_wire_version": negotiated,
                    "capabilities": daemon_capabilities(),
                    "identity": identity.describe(server.identity_policy()),
                }),
                error: None,
            },
            negotiated,
            identity,
        ),
        Err(error) => {
            // Never log the token value (VAL-SEC-009): the error message is a
            // generic reason, not the presented or expected token.
            tracing::warn!(
                workspace_key = %server.workspace_key,
                event = "auth_rejected",
                "client auth rejected"
            );
            (
                IpcResponse {
                    ok: false,
                    result: Value::Null,
                    error: Some(error),
                },
                1,
                ClientIdentity::root(IdentityPolicy::Required),
            )
        }
    };
    // The handshake response is ALWAYS newline-JSON (even when v2 is negotiated) so a
    // v1 client can read it (VAL-IPC-024); the switch to framing happens only for
    // messages AFTER a successful v2 negotiation.
    write_json_line(&mut stream, &hello_response)?;
    if !hello_response.ok {
        // The token gate precedes any request handling (VAL-IPC-023): a failed
        // handshake closes the connection without dispatching a request.
        return Ok(());
    }

    // Bound every RESPONSE write from here on (both v1 and v2 paths inherit it):
    // a peer that authenticates and then stops reading would otherwise let a
    // large response pin this thread on a full socket buffer forever. Mirrors
    // SUBSCRIBER_WRITE_TIMEOUT's intent for the request/response path; subscriber
    // streams re-arm their own tighter timeout at registration. Best-effort on
    // non-unix transports.
    let _ = stream.set_write_timeout(Some(RESPONSE_WRITE_TIMEOUT));

    tracing::info!(
        workspace_key = %server.workspace_key,
        event = "client_connect",
        "client connected"
    );

    // Whether this client asked for a Subscribe registration ack (M8). Gated on
    // the client's advertisement so old clients keep the ack-less stream shape.
    let client_wants_subscribe_ack = hello
        .capabilities
        .as_ref()
        .is_some_and(|caps| caps.iter().any(|c| c == "subscribe-ack"));

    if negotiated_wire_version >= frame::WIRE_VERSION {
        return serve_framed_connection(
            server,
            stream,
            negotiated_wire_version,
            client_wants_subscribe_ack,
            identity,
        );
    }

    // Negotiated wire v1: the legacy newline single-request-per-connection path.
    // The request read shares the hello/auth phase's TOTAL budget (L18): it is
    // still part of connection set-up for the single-shot v1 path, so it must not
    // extend this thread's lifetime beyond the same deadline.
    let request_line = {
        let mut deadline = HandshakeDeadline::new(&mut stream, handshake_started, handshake_budget);
        let mut reader = BufReader::new(&mut deadline);
        read_ipc_line(&mut reader)?
    };
    let request: DaemonRequest = serde_json::from_str(&request_line)
        .map_err(|error| format!("invalid daemon request: {error}"))?;

    if matches!(request, DaemonRequest::Subscribe) {
        // Subscribe converts a v1 connection into a newline-JSON event stream; no
        // further requests are served on it. It passes the same revocation and
        // scope gate as any request first.
        if let Err(error) = server.authorize_subscribe(&identity) {
            let response = IpcResponse {
                ok: false,
                result: Value::Null,
                error: Some(error),
            };
            let _ = write_json_line(&mut stream, &response);
            return Ok(());
        }
        begin_subscription(
            &server,
            stream,
            negotiated_wire_version,
            client_wants_subscribe_ack,
            identity.credential.as_deref(),
        );
        return Ok(());
    }

    let response = match server.handle_as(request, Some(&stream), &identity) {
        Ok(result) => IpcResponse {
            ok: true,
            result,
            error: None,
        },
        Err(error) => IpcResponse {
            ok: false,
            result: Value::Null,
            error: Some(error),
        },
    };
    write_json_line(&mut stream, &response)
}

/// Register `stream` as an event-stream subscriber on its negotiated `wire_version`
/// (events are framed for v2, newline-JSON for v1) and push catch-up state.
///
/// The subscriber is added FIRST so any `PaneEnded` broadcast that races with the
/// catch-up snapshot is still delivered (no missed event). The current per-pane
/// runtime state is then replayed as catch-up `PaneEnded` events for panes that
/// already ended before this subscription (VAL-LIFE-001 / VAL-LIFE-011 /
/// VAL-CROSS-002). A pane that ends between `add_subscriber` and the snapshot yields
/// both a broadcast and a catch-up `PaneEnded` — the duplicate is idempotent (the
/// GUI sets the same ended state again).
/// Returns the subscriber id, or `None` if the credential was revoked or the
/// subscriber cap refused it. Credential registration precedes output and ack.
pub(crate) fn begin_subscription(
    server: &Arc<DaemonServer>,
    stream: TransportStream,
    wire_version: u16,
    send_ack: bool,
    credential: Option<&str>,
) -> Option<u64> {
    let sub_id = match server.register_subscription(stream, wire_version, credential) {
        Ok(id) => id,
        Err((reason, mut stream)) => {
            // Revoked credential or subscriber cap: report a clean error on the
            // connection's own wire protocol, then let the stream drop (close)
            // — no subscriber entry, channel, or threads were created, so the
            // connection is not leaked.
            let response = IpcResponse {
                ok: false,
                result: Value::Null,
                error: Some(reason),
            };
            if wire_version >= frame::WIRE_VERSION {
                let _ = frame::write(&mut stream, &response);
            } else {
                let _ = write_json_line(&mut stream, &response);
            }
            return None;
        }
    };
    // The ack rides the subscriber's own ordered queue as the FIRST payload,
    // enqueued only after add_subscriber returned: once the client reads it,
    // registration is a fact and no later broadcast can be missed (M8).
    if send_ack {
        server
            .router
            .send_to_subscriber(sub_id, &DaemonEvent::SubscribeAck);
    }
    if let Ok(snapshot) = server.snapshot() {
        for (pane_id, state) in &snapshot.pane_states {
            if *state == PaneRuntimeState::Ended {
                server.router.send_to_subscriber(
                    sub_id,
                    &DaemonEvent::PaneEnded {
                        pane_id: pane_id.clone(),
                        exit_code: server.pane_exit_code(pane_id),
                    },
                );
            }
        }
    }
    Some(sub_id)
}

/// (L18) Remaining budget for the v1 hello/auth phase: `total` minus the time
/// since `started`, or an error once the phase has overrun. Factored out of
/// `HandshakeDeadline` so tests can drive it with millisecond-scale budgets.
pub(crate) fn handshake_budget_remaining(
    started: Instant,
    total: Duration,
) -> Result<Duration, String> {
    let elapsed = started.elapsed();
    if elapsed >= total {
        return Err("daemon handshake timed out".to_string());
    }
    Ok(total - elapsed)
}

/// A `Read` wrapper enforcing ONE total deadline across the whole v1 hello/auth
/// phase (L18), the absolute-deadline counterpart of `StallReadTimeout`: the
/// stream's read timeout is re-armed to the REMAINING budget before every
/// underlying read, so a peer dribbling bytes just under each per-read timeout
/// (slowloris) still hits the absolute deadline instead of pinning the
/// per-connection thread indefinitely.
pub(crate) struct HandshakeDeadline<'a> {
    pub(crate) stream: &'a mut TransportStream,
    pub(crate) started: Instant,
    pub(crate) total: Duration,
}

impl<'a> HandshakeDeadline<'a> {
    pub(crate) fn new(stream: &'a mut TransportStream, started: Instant, total: Duration) -> Self {
        Self {
            stream,
            started,
            total,
        }
    }
}

impl Read for HandshakeDeadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = handshake_budget_remaining(self.started, self.total)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?;
        let _ = self.stream.set_read_timeout(Some(remaining));
        self.stream.read(buf)
    }
}

/// A `Read` wrapper that arms an ABSOLUTE read-stall deadline on the underlying
/// stream once the FIRST byte of a frame has been read (M6). The daemon's
/// persistent v2 loop clears the handshake read timeout so an idle connection
/// may sit between requests indefinitely, but a peer that has STARTED a frame
/// must finish it within `stall_timeout` — the deadline is absolute across the
/// frame (re-armed as the remaining budget before every read), so a peer
/// dribbling one byte per interval still dies at the deadline instead of
/// pinning its thread forever. The caller clears the deadline again once the
/// frame completes (or errors), so idle time BETWEEN frames is never bounded.
pub(crate) struct StallReadTimeout<'a> {
    pub(crate) stream: &'a mut TransportStream,
    pub(crate) stall_timeout: Duration,
    pub(crate) deadline: Option<Instant>,
}

impl<'a> StallReadTimeout<'a> {
    pub(crate) fn new(stream: &'a mut TransportStream, stall_timeout: Duration) -> Self {
        Self {
            stream,
            stall_timeout,
            deadline: None,
        }
    }
}

impl Read for StallReadTimeout<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.deadline {
            // First byte of a frame: wait unbounded (idle-between-frames is fine),
            // then arm the absolute deadline for the rest of the frame.
            None => {
                let n = self.stream.read(buf)?;
                if n > 0 {
                    self.deadline = Some(Instant::now() + self.stall_timeout);
                }
                Ok(n)
            }
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "frame read exceeded the stall deadline",
                    ));
                }
                let _ = self.stream.set_read_timeout(Some(remaining));
                self.stream.read(buf)
            }
        }
    }
}

/// Serve a connection that negotiated wire version ≥ 2 (architecture.md §5.3): a
/// synchronous, in-order request/response loop over framed envelopes on the SAME
/// connection. Each iteration reads one framed `DaemonRequest`, dispatches it
/// through the same `DaemonServer::handle` path as the v1 single-shot path, and
/// writes a framed `IpcResponse` BEFORE the next request is read (one outstanding
/// request at a time, no correlation id needed). Identical dispatch semantics to v1
/// (VAL-IPC-031); only the framing and the multi-request lifetime differ.
///
/// Termination:
/// - A clean EOF at a frame boundary (the peer closed) ends the loop with `Ok(())`
///   (VAL-IPC-029).
/// - A malformed / oversized / unframed frame is a clean bounded protocol error:
///   it is logged and the connection is closed by returning `Err` (VAL-IPC-034).
///   Each connection runs on its own thread, so this never disturbs other
///   connections (VAL-IPC-052).
/// - `Subscribe` converts THIS connection into a framed event stream and serves no
///   further requests on it (VAL-IPC-030 / VAL-IPC-033); events are framed because
///   the connection negotiated v2 (VAL-IPC-050).
pub(crate) fn serve_framed_connection(
    server: Arc<DaemonServer>,
    mut stream: TransportStream,
    wire_version: u16,
    client_wants_subscribe_ack: bool,
    identity: ClientIdentity,
) -> Result<(), String> {
    // A persistent connection may sit idle between requests, so the handshake read
    // timeout must not kill it; the loop blocks until the next request or EOF/close.
    // A STALLED PARTIAL frame is different (M6): StallReadTimeout re-arms a read
    // deadline once the first header byte of a frame has arrived, so a peer that
    // starts a frame must finish it promptly or the connection is dropped.
    let _ = stream.set_read_timeout(None);

    loop {
        let request: DaemonRequest = {
            let mut stall_reader = StallReadTimeout::new(&mut stream, HANDSHAKE_READ_TIMEOUT);
            let read = frame::read(&mut stall_reader);
            // Between frames the connection idles with NO read deadline.
            let _ = stream.set_read_timeout(None);
            match read {
                Ok(Some(request)) => request,
                Ok(None) => return Ok(()),
                Err(error) => {
                    tracing::warn!(
                        workspace_key = %server.workspace_key,
                        event = "protocol_error",
                        error = %error,
                        "closing v2 connection on framed protocol error"
                    );
                    return Err(error);
                }
            }
        };

        if matches!(request, DaemonRequest::Subscribe) {
            if let Err(error) = server.authorize_subscribe(&identity) {
                let response = IpcResponse {
                    ok: false,
                    result: Value::Null,
                    error: Some(error),
                };
                let _ = frame::write(&mut stream, &response);
                return Ok(());
            }
            begin_subscription(
                &server,
                stream,
                wire_version,
                client_wants_subscribe_ack,
                identity.credential.as_deref(),
            );
            return Ok(());
        }

        let response = match server.handle_as(request, Some(&stream), &identity) {
            Ok(result) => IpcResponse {
                ok: true,
                result,
                error: None,
            },
            Err(error) => IpcResponse {
                ok: false,
                result: Value::Null,
                error: Some(error),
            },
        };
        // Write the response before reading the next request: synchronous, in-order
        // (VAL-IPC-028). A write failure (peer gone) ends the loop.
        frame::write(&mut stream, &response)?;
    }
}

/// Read a single newline-delimited frame, refusing frames larger than MAX_FRAME_BYTES
/// so an untrusted local peer cannot OOM the daemon with an unbounded line.
pub(crate) fn read_ipc_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut bytes = Vec::new();
    let read = (&mut *reader)
        .take(MAX_FRAME_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| format!("failed to read ipc frame: {error}"))?;
    if read as u64 > MAX_FRAME_BYTES {
        return Err("ipc frame exceeds maximum size".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) fn write_json_line<T: Serialize>(
    stream: &mut TransportStream,
    value: &T,
) -> Result<(), String> {
    // ONE write of line+'\n': a peer that reads the first bytes, decides the
    // message is a protocol error and closes (the v2 bad-magic path) must not
    // turn the trailing newline into a spurious EPIPE for a message that was
    // fully delivered. Also one syscall instead of two per message.
    let mut bytes =
        serde_json::to_vec(value).map_err(|error| format!("failed to encode ipc: {error}"))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|error| format!("failed to write ipc: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("failed to flush ipc: {error}"))
}

/// Test-only legacy v1 handshake helper: it advertises no `max_wire_version`, so
/// the daemon negotiates wire v1 and the one-request-per-connection behavior is
/// preserved. EVERY production ctl/daemon path now negotiates by default through
/// `DaemonConnection` (including the `daemon_is_alive` liveness probe behind
/// `ctl daemons` / `ctl shutdown --all`), so this helper is `#[cfg(test)]`: it keeps
/// the v1 backward-compat coverage alive (the socket-based `TestDaemon` suite drives
/// Subscribe/Ping/Shutdown over newline v1 through it — the standing
/// old-client↔new-daemon regression proof, Invariant 8) without leaving an unused
/// production code path that would trip clippy's dead-code lint under `-D warnings`.
#[cfg(test)]
pub(crate) fn authenticate_stream_at(
    socket_path: &Path,
    token: &str,
) -> Result<TransportStream, String> {
    let mut stream = transport_connect(socket_path)
        .map_err(|error| format!("failed to connect to daemon: {error}"))?;
    let hello = IpcHello {
        frame_type: "hello".to_string(),
        version: PROTOCOL_VERSION,
        token: token.to_string(),
        max_wire_version: None,
        capabilities: None,
        client_token: None,
    };
    write_json_line(&mut stream, &hello)?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("failed to read daemon hello: {error}"))?;
    let response: IpcResponse =
        serde_json::from_str(&line).map_err(|error| format!("invalid daemon hello: {error}"))?;
    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "daemon authentication failed".to_string()));
    }

    Ok(reader.into_inner())
}
