use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
// UnixStream/UnixListener are no longer imported at the top level — the
// transport type aliases (TransportStream/TransportListener) use full paths,
// and the test module imports UnixStream directly.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::sync::{Condvar, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::Watcher;
use portable_pty::{
    native_pty_system, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize,
};
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;
use tracing::dispatcher;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

const DAEMON_ARG: &str = "--daemon";
const CTL_ARG: &str = "ctl";
const WORKSPACE_ARG: &str = "--workspace";
const SOCKET_ARG: &str = "--socket";
const DATA_DIR_ARG: &str = "--data-dir";
const APP_SUPPORT_DIR: &str = "Sgian";
/// Existing installations keep using their original data root when the new
/// root does not yet exist. Besides preserving workspaces/config, this keeps
/// the socket path stable while an older daemon is still running.
const LEGACY_APP_SUPPORT_DIR: &str = "Sgian2";
/// The Windows pipe/mutex namespace is a protocol identifier, not UI branding.
/// Keep it stable so a renamed client can connect to an older running daemon.
#[cfg(any(windows, test))]
const WINDOWS_IPC_NAMESPACE: &str = "sgian2";
const WORKSPACE_FILE: &str = "workspace.json";
const CONFIG_FILE: &str = "config.json";
const SCROLLBACK_DIR: &str = "scrollback";
const RUNTIME_DIR: &str = "runtime";
const SOCKET_FILE: &str = "daemon.sock";
const TOKEN_FILE: &str = "daemon.token";
/// Per-client credentials (docs/design/client-identity.md): beside the
/// workspace token, owner-only, token hashes only.
const CLIENTS_FILE: &str = "clients.json";
const LOG_FILE: &str = "daemon.log";
/// Early daemon-launch diagnostics written before structured logging exists.
/// The GUI reads this file when a freshly-spawned daemon exits or never binds,
/// so Windows startup failures are not discarded with the daemon's stderr.
const DAEMON_STARTUP_LOG_FILE: &str = "daemon-startup.log";
/// Advisory exclusive lock file (flock via fs4::FileExt::try_lock) that serializes
/// the connect-check + bind window of concurrent cold starts so exactly one daemon
/// owns the socket. Lives next to the socket in `runtime/<key>/`.
const DAEMON_LOCK_FILE: &str = "daemon.lock";
/// When the daemon log file exceeds this size on startup, it is rotated to
/// `daemon.log.old` so total log storage stays bounded (max ~2× this size).
const LOG_MAX_BYTES: u64 = 1024 * 1024;
const PROTOCOL_VERSION: u32 = 1;
/// Highest wire version this daemon speaks. Wire v1 = the legacy newline-JSON path
/// (`read_ipc_line`/`write_json_line`); wire v2 = the length-prefixed framed
/// envelope (`mod frame`). The capability handshake negotiates
/// `min(client_max_wire_version, DAEMON_MAX_WIRE_VERSION)` (architecture.md §5.2),
/// so this is kept in lockstep with the framed codec's wire version.
const DAEMON_MAX_WIRE_VERSION: u16 = frame::WIRE_VERSION;
/// Owner-only directory mode (0700) — applied on Unix only via `PermissionsExt`.
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
/// Owner-only file mode (0600) — applied on Unix only via `OpenOptionsExt`/`PermissionsExt`.
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

/// Centralized owner-only file-creation mode for `OpenOptions`.
///
/// On Unix this applies `OpenOptionsExt::mode(PRIVATE_FILE_MODE)` (0600) so
/// newly-created files are owner-only. On non-Unix (Windows) `OpenOptionsExt`
/// is unavailable and file ACLs are managed by the OS, so this is a no-op.
/// This trait centralizes all 0600 perm application behind a single cfg-gated
/// boundary so the calling code is cross-platform.
trait PrivateOpenOptions {
    /// Set the create-mode to owner-only (0600) on Unix; no-op on Windows.
    fn private_mode(&mut self) -> &mut Self;
}

#[cfg(unix)]
impl PrivateOpenOptions for OpenOptions {
    fn private_mode(&mut self) -> &mut Self {
        self.mode(PRIVATE_FILE_MODE)
    }
}

#[cfg(not(unix))]
impl PrivateOpenOptions for OpenOptions {
    fn private_mode(&mut self) -> &mut Self {
        self
    }
}

// ---------------------------------------------------------------------------
// Transport abstraction: Unix domain sockets (cfg(unix)) vs Windows named
// pipes (cfg(windows)).
//
// The proven Unix implementation is kept intact behind cfg(unix). A
// cfg(windows) named-pipe implementation using windows-sys
// (CreateNamedPipeW / ConnectNamedPipe / CreateFileW) is provided so the
// crate cross-compiles for x86_64-pc-windows-gnu. The transport trait
// abstracts connect / listen / accept (including non-blocking WouldBlock
// idle semantics), addressing, permissioning, and stale-handle cleanup.
//
// Windows RUNTIME behavior is unvalidated by agreement — the bound is
// `cargo check --target x86_64-pc-windows-gnu` compiles with 0 errors.
// ---------------------------------------------------------------------------

/// The connected-stream type used by the IPC transport layer.
///
/// On Unix this is `std::os::unix::net::UnixStream`; on Windows it is a
/// named-pipe handle wrapper (`WindowsNamedPipeStream`). Both implement
/// `Read` + `Write` and provide `try_clone`, `shutdown`, `set_write_timeout`,
/// `set_read_timeout`, and `set_nonblocking` as inherent methods so the
/// calling code is identical across platforms via this alias.
#[cfg(unix)]
type TransportStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type TransportStream = WindowsNamedPipeStream;

/// The listener type used by the daemon's accept loop.
///
/// On Unix this is `std::os::unix::net::UnixListener`; on Windows it is a
/// named-pipe server (`WindowsNamedPipeListener`). Both provide `accept()`
/// (returning `(TransportStream, ())`) and `set_nonblocking(bool)`.
#[cfg(unix)]
type TransportListener = std::os::unix::net::UnixListener;
#[cfg(windows)]
type TransportListener = WindowsNamedPipeListener;

/// Connect to the transport endpoint at `path`.
///
/// On Unix, `path` is a filesystem socket path (`UnixStream::connect`).
/// On Windows, a named-pipe name is derived from `path` and opened via
/// `CreateFileW`.
fn transport_connect(path: &Path) -> std::io::Result<TransportStream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path)
    }
    #[cfg(windows)]
    {
        WindowsNamedPipeStream::connect(path)
    }
}

/// Return the concrete endpoint a native client should connect to. Unix
/// clients receive the domain-socket path; Windows clients receive the
/// owner-scoped named-pipe path derived by the same code the daemon uses.
fn transport_endpoint(path: &Path) -> std::io::Result<String> {
    #[cfg(unix)]
    {
        Ok(path.display().to_string())
    }
    #[cfg(windows)]
    {
        windows_transport::pipe_name_from_path(path)
    }
}

/// Bind a transport listener at `path`.
///
/// On Unix, `path` is a filesystem socket path (`UnixListener::bind`).
/// On Windows, a named-pipe server is created via `CreateNamedPipeW`.
fn transport_bind(path: &Path) -> std::io::Result<TransportListener> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixListener::bind(path)
    }
    #[cfg(windows)]
    {
        WindowsNamedPipeListener::bind(path)
    }
}

/// Compose a Windows named-pipe name scoped to BOTH the workspace socket path
/// (hashed) AND a per-user component (the caller's user-SID string on Windows).
/// Two different users therefore derive distinct pipe names for the same
/// workspace, so they cannot collide on (or hijack) each other's pipe. Factored
/// out and platform-independent so the SID-scoping contract is unit-testable on
/// any host; the live SID lookup that feeds `sid_component` is Windows-only.
#[cfg(any(windows, test))]
fn pipe_name_with_sid(sid_component: &str, path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    // Keep only pipe-name-safe characters; SID strings ("S-1-5-21-...") already
    // satisfy this, but guard against anything unexpected from the OS.
    let safe_sid: String = sid_component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "\\\\.\\pipe\\{WINDOWS_IPC_NAMESPACE}-{}-{:016x}",
        safe_sid,
        hasher.finish()
    )
}

/// Compose the per-user pipe name from a resolved SID, failing CLOSED when the
/// per-user SID could not be resolved. A Windows pipe name MUST always embed a
/// real per-user SID — there is NO placeholder/"nosid" fallback, because a fixed
/// component would let two users collide on (or hijack) the same workspace pipe.
/// Platform-independent so the fail-closed contract is unit-testable on any host;
/// the live SID lookup that feeds it is Windows-only.
#[cfg(any(windows, test))]
fn pipe_name_from_sid(sid: Option<String>, path: &Path) -> std::io::Result<String> {
    match sid {
        Some(sid) => Ok(pipe_name_with_sid(&sid, path)),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to build a Windows pipe name without a per-user SID",
        )),
    }
}

/// Fail-closed guard for the owner-restricted pipe security descriptor. The pipe
/// MUST NOT be created with default/inherited ACLs, so a NULL descriptor (the SID
/// lookup or the SDDL build failed) is refused — on BOTH the initial bind and each
/// post-accept recreate — rather than silently downgraded. Platform-independent so
/// the fail-closed contract is unit-testable on any host.
#[cfg(any(windows, test))]
fn require_owner_descriptor(descriptor_is_null: bool) -> std::io::Result<()> {
    if descriptor_is_null {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to create a named pipe without an owner-restricted security descriptor",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows named-pipe transport implementation (cfg(windows) only).
//
// Uses windows-sys FFI: CreateNamedPipeW (server), CreateFileW (client),
// ConnectNamedPipe (accept), ReadFile/WriteFile (I/O), DuplicateHandle
// (try_clone). Non-blocking accept uses overlapped I/O so WouldBlock is
// returned when no client is pending. Stream I/O is overlapped too (the pipe
// handles are opened with FILE_FLAG_OVERLAPPED), with per-direction
// OVERLAPPED/event state shared across clones and deadline-bounded waits so
// set_read_timeout/set_write_timeout are honored (H5).
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_transport;

#[cfg(windows)]
use windows_transport::{WindowsNamedPipeListener, WindowsNamedPipeStream};

const SCROLLBACK_REPLAY_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const SCROLLBACK_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Aggregate bound on the scrollback attached to a BootstrapWorkspace response
/// (H2), measured as the scrollback's contribution to the SERIALIZED response.
/// Per-pane reads are already capped at `SCROLLBACK_REPLAY_LIMIT_BYTES`, but a
/// framed (v2) response is rejected outright over `MAX_FRAME_BYTES` (8 MiB) —
/// without an aggregate bound, 4+ panes of grown scrollback make every v2
/// bootstrap undeliverable and the GUI can never attach. 4 MiB leaves ample
/// headroom under the frame cap for the rest of the snapshot (layout ≤
/// `MAX_LAYOUT_BYTES`, pane metadata, keys).
const BOOTSTRAP_SCROLLBACK_BUDGET_BYTES: usize = 4 * 1024 * 1024;
const DAEMON_CONNECT_RETRIES: usize = 80;
const DAEMON_CONNECT_DELAY: Duration = Duration::from_millis(50);
/// Fixed delay between GUI event-subscription reconnect attempts — applied on
/// connect failure and (L19) after a session that died younger than
/// `SUBSCRIPTION_HEALTHY_MIN`.
const SUBSCRIPTION_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
/// A subscription session shorter than this counts as FAILED for reconnect
/// backoff (L19): a daemon that accepts+acks then immediately EOFs must not be
/// reconnect-looped with no delay.
const SUBSCRIPTION_HEALTHY_MIN: Duration = Duration::from_secs(5);
/// How long a freshly-spawned daemon waits for the flock to be released by a
/// dying daemon (in teardown after `ctl shutdown`) before giving up. Must stay
/// comfortably under `DAEMON_CONNECT_RETRIES * DAEMON_CONNECT_DELAY` so a
/// re-spawn after shutdown binds before the client's readiness retry expires.
const DAEMON_LOCK_WAIT: Duration = Duration::from_millis(2500);
/// Poll interval for the bounded lock-wait loop (re-try the acquire + connect
/// check at this cadence).
const DAEMON_LOCK_POLL: Duration = Duration::from_millis(25);
const MAX_FRAME_BYTES: u64 = 8 * 1024 * 1024;
const MAX_LAYOUT_BYTES: usize = 256 * 1024;
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on any single post-auth RESPONSE write (both wire paths): a peer that
/// stops reading after a valid handshake must not pin the per-connection thread
/// on a full socket buffer forever. Mirrors `SUBSCRIBER_WRITE_TIMEOUT`'s intent
/// for the request/response path; subscriber streams re-arm their own tighter
/// timeout at registration. Best-effort on non-unix transports.
const RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const SUBSCRIBER_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SUBSCRIBER_QUEUE_LIMIT: usize = 1024;
/// How long the reliable catch-up send retries before giving up and dropping a
/// stalled subscriber. This must be long enough for a slow-but-reading subscriber
/// to drain its queue (e.g. a GUI that is still starting up), but short enough
/// that a truly stalled subscriber doesn't block the daemon's accept loop
/// indefinitely. A stalled subscriber may be dropped/disconnected, but must never
/// receive a PARTIAL catch-up that omits panes silently — the retry loop either
/// delivers every event or removes the subscriber entirely.
const CATCHUP_SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval for the reliable catch-up send retry loop.
const CATCHUP_SEND_RETRY_INTERVAL: Duration = Duration::from_millis(1);
const MAX_TITLE_CHARS: usize = 256;
/// How often lazily-persisted state (resize/focus churn) is flushed to disk, so a
/// divider drag doesn't fsync workspace.json once per animation frame.
const LAZY_PERSIST_INTERVAL: Duration = Duration::from_secs(1);
/// Cap on concurrently-served connections (L18): the socket is owner-only, but a
/// same-UID flood of opened-and-stalled connections would otherwise pin one
/// thread + handshake buffer each with no bound.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;
/// Aggregate live transport budget: request/response connections plus handed-off
/// event subscriptions. Windows named pipes hard-cap one pipe name at 255 live
/// instances; the listener itself consumes one, and temporary connect/accept
/// overlap needs headroom. Keeping the cross-platform daemon at 192 prevents a
/// collection of abandoned waits plus subscribers from exhausting the Windows
/// pipe namespace before the application-level caps can reject cleanly.
const MAX_LIVE_TRANSPORTS: usize = 192;
/// Cap on total panes per workspace (M4): every pane costs a shell, a PTY, two
/// threads, and a vt100 screen model, so an unbounded CreatePane loop would
/// exhaust PIDs/fds. Only NEW pane creation is refused at the cap; a persisted
/// workspace that exceeds it still loads (its panes just can't grow further).
const MAX_PANES: usize = 64;
/// Cap on event subscribers (M5): each subscriber costs two threads, and its
/// connection slot is released at hand-off — an unbounded connect→Subscribe→hold
/// loop would otherwise accumulate threads/fds until exhaustion (and a nonzero
/// subscriber count permanently suppresses idle shutdown).
const MAX_SUBSCRIBERS: usize = 64;

fn live_transport_limit_reached(active_connections: usize, subscribers: usize) -> bool {
    active_connections.saturating_add(subscribers) >= MAX_LIVE_TRANSPORTS
}
/// Closed-pane suppression entries older than this are pruned (L13): the window
/// in which a closed pane's reader can still drain is seconds (its child is
/// killed at close), and pane ids are never reused.
const CLOSED_PANE_RETENTION: Duration = Duration::from_secs(300);
const CLOSED_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

// created_at_ms is u64 (not u128): serde's internally-tagged enums (DaemonEvent)
// cannot buffer u128 during deserialization, and u64 millis outlast the sun anyway.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pane {
    pub id: String,
    pub title: String,
    pub kind: PaneKind,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Shell,
    /// (T2) A chat-native agent pane: the daemon owns a headless `claude` CLI
    /// process (piped-stdin/stdout stream-json) behind it instead of a PTY
    /// shell. Serialized as `"agent"`; workspace.json files written by older
    /// builds only ever contain `"shell"` and keep loading, while an OLD build
    /// reading a NEW file with `"agent"` fails to parse the pane — accepted:
    /// downgrade of a workspace that adopted agent panes is not supported.
    Agent,
}

/// The CLI implementation that owns an agent pane. Provider identity is kept
/// separate from the model because one CLI (notably Droid) can route many
/// model families, including user-configured endpoints.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    #[default]
    Claude,
    Droid,
}

impl AgentBackendKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Droid => "droid",
        }
    }
}

/// Immutable identity for an agent pane. Missing specs in pre-provider
/// workspaces are read as Claude with that CLI's configured default model.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPaneSpec {
    #[serde(default)]
    pub backend: AgentBackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl AgentPaneSpec {
    fn normalized(
        backend: Option<AgentBackendKind>,
        model: Option<String>,
    ) -> Result<Self, String> {
        let model = model
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if model
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TITLE_CHARS)
        {
            return Err(format!(
                "agent model exceeds maximum size ({MAX_TITLE_CHARS} bytes)"
            ));
        }
        Ok(Self {
            backend: backend.unwrap_or_default(),
            model,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaneRuntimeState {
    Live,
    Ended,
}

/// (T1) A pane's classified agent attention state. Serialized snake_case
/// (`"working"`/`"needs_input"`/`"idle"`); the daemon classifies it from the
/// pane's rendered screen and broadcasts it on transitions only.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAttention {
    Working,
    NeedsInput,
    Idle,
}

/// (T1) Per-pane agent state surfaced in the bootstrap payload's
/// `agent_states` map (and mirrored by snapshot/find). `agent` is the agent
/// CLI name when known (`"claude"` today; future names are additive); a pane
/// with no known agent is simply absent from the map. `attention` is omitted
/// until the pane has been classified.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPaneInfo {
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<AgentAttention>,
    /// The agent's permission mode as observed (`auto`, `bypass`,
    /// `accept-edits`, `plan` from a Claude Code screen; an agent pane's
    /// configured mode verbatim). Absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// True when `mode` runs tools without asking a person (`auto`, `bypass`,
    /// `bypassPermissions`, `dontAsk`): the pane must be visibly marked.
    #[serde(default)]
    pub unattended: bool,
}

/// Modes in which an agent runs tools without a person approving them.
fn is_unattended_mode(mode: Option<&str>) -> bool {
    matches!(
        mode,
        Some("auto") | Some("bypass") | Some("bypassPermissions") | Some("dontAsk")
    )
}

/// Claude Code prints its permission mode in the input-box footer
/// (`⏵⏵ auto mode on`, `⏵⏵ bypass permissions on`, `⏵⏵ accept edits on`,
/// `⏸ plan mode on`). Reduce it to a short stable token.
fn classify_agent_mode(text: &str) -> Option<&'static str> {
    if text.contains("bypass permissions on") {
        Some("bypass")
    } else if text.contains("auto mode on") {
        Some("auto")
    } else if text.contains("accept edits on") || text.contains("auto-accept edits on") {
        Some("accept-edits")
    } else if text.contains("plan mode on") {
        Some("plan")
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub panes: Vec<Pane>,
    pub active_pane_id: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub layout: Option<Value>,
    #[serde(default)]
    pub scrollback: HashMap<String, String>,
    #[serde(default)]
    pub sizes: HashMap<String, PaneSize>,
    #[serde(default)]
    pub pane_states: HashMap<String, PaneRuntimeState>,
    /// (T1) Agent info for panes with a known agent, parallel to `pane_states`.
    /// Additive (serde default): old daemons omit it, old clients ignore it.
    #[serde(default)]
    pub agent_states: HashMap<String, AgentPaneInfo>,
    /// (T2) Bounded conversation replay for agent-kind panes: the tail of each
    /// pane's NORMALIZED agent event stream (the same `DaemonEvent::AgentEvent`
    /// payloads live subscribers receive), so a client can render recent
    /// history without the raw JSONL log. Additive like `agent_states`.
    #[serde(default)]
    pub agent_events: HashMap<String, Vec<Value>>,
    /// Provider/model identity for agent panes. This is a separate map rather
    /// than fields on Pane so older clients continue to deserialize Pane
    /// objects byte-for-byte.
    #[serde(default)]
    pub agent_specs: HashMap<String, AgentPaneSpec>,
    /// Keyboard leases for HELD panes only (pane_id → holder and counters).
    /// Additive: old daemons omit it, old clients ignore it.
    #[serde(default)]
    pub leases: HashMap<String, LeaseInfo>,
    /// Named pane groups (docs/design/keyboard-lease-and-ledger.md §7).
    #[serde(default)]
    pub projects: HashMap<String, Project>,
    /// Output-guard counters for panes with at least one hit. Additive.
    #[serde(default)]
    pub output_warnings: HashMap<String, OutputTricks>,
    /// Per-pane usage from Claude Code's status line (`ctl statusline`):
    /// model, context fill, rate-limit windows. Additive.
    #[serde(default)]
    pub agent_usage: HashMap<String, AgentUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandOk {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyOutput {
    pub pane_id: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneEnded {
    pub pane_id: String,
    /// Exit code captured by the reaper (`child.wait()`): the real process exit
    /// status, or `None` when the pane was terminated by a signal (or the status
    /// could not be read). Additive + omitted when absent, so a None payload keeps
    /// the byte-identical pre-MB shape and a legacy payload without it still loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneClosed {
    pub pane_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneStatus {
    pub pane: Pane,
    pub state: PaneRuntimeState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneList {
    pub panes: Vec<PaneStatus>,
    pub active_pane_id: Option<String>,
    pub cwd: String,
}

/// Richer runtime detail returned by `ctl status --verbose`. The config summary
/// excludes `env` values (observability surfaces must never expose secrets).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerboseStatus {
    pub subscribers: usize,
    pub panes: Vec<PaneStatus>,
    pub active_pane_id: Option<String>,
    pub cwd: String,
    pub uptime_secs: u64,
    pub config: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedPtySize {
    cols: u16,
    rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PersistedWorkspace {
    panes: Vec<Pane>,
    active_pane_id: Option<String>,
    cwd: String,
    next_id: u64,
    #[serde(default)]
    layout: Option<Value>,
    #[serde(default)]
    sizes: HashMap<String, PersistedPtySize>,
    /// Per-pane runtime state (Live/Ended) persisted so an ended pane's state
    /// survives a daemon restart and is reflected post-bootstrap.
    #[serde(default)]
    pane_states: HashMap<String, PaneRuntimeState>,
    /// (T1) Manual agent marks (pane_id → agent name), set via SetPaneAgent.
    /// Only MANUAL marks are persisted; auto-detected agent state re-derives
    /// from the pane's screen after a restart.
    #[serde(default)]
    agents: HashMap<String, String>,
    /// (T2) pane_id → `claude` CLI session id for agent-kind panes, recorded
    /// from the CLI's `system/init` event so a respawn after a daemon restart
    /// can pass `--resume <session_id>` and continue the conversation. Additive
    /// (serde default): pre-T2 workspace files omit it.
    #[serde(default)]
    agents_v2: HashMap<String, String>,
    /// Per-pane provider/model selection. Pre-provider workspaces omit this
    /// and default agent-kind panes to Claude.
    #[serde(default)]
    agent_specs: HashMap<String, AgentPaneSpec>,
    /// Per-pane frozen shell overrides from named profiles at create time
    /// (ENHANCEMENTS §4). Additive: pre-feature workspace files omit it.
    #[serde(default)]
    pane_shells: HashMap<String, ShellConfig>,
    /// Held keyboard leases (pane_id → holder and counters), so a daemon
    /// restart does not silently forget who was in control. Additive.
    #[serde(default)]
    leases: HashMap<String, HeldLease>,
    /// Named pane groups; additive.
    #[serde(default)]
    projects: HashMap<String, Project>,
}

/// User configuration, loaded from a global config.json and an optional per-workspace
/// override. Every field is optional so a partial or missing file is valid.
///
/// Unknown keys are REJECTED (07-19 review, persistence lows): a typo'd key
/// previously deserialized fine and was silently dropped — the operator's
/// intended setting never applied. The GUI round-trip stays valid because
/// `get_config` emits exactly this struct's fields (`full_config`) and the
/// settings form only writes those keys back; the scrubbed `summary()` payload
/// (which adds `resolved_shell`) is observability-only and never written back.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    shell: Option<String>,
    /// `None` = not set at this layer (a lower-priority layer's value applies);
    /// `Some([])` = explicitly no arguments, overriding the lower layer (M3:
    /// with a plain Vec, `[]` was indistinguishable from "unset" and could
    /// never cancel a global value).
    #[serde(default)]
    shell_args: Option<Vec<String>>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Optional list of environment variable names to scrub (remove) from the
    /// inherited process environment before spawning a pane's PTY. Default is
    /// empty, preserving the current behavior where panes inherit the daemon's
    /// full environment. A variable explicitly set in `env` takes precedence
    /// over the scrub list for the same name (the operator-set value wins).
    /// VAL-SEC-003/004/007.
    #[serde(default)]
    scrub_env: Vec<String>,
    #[serde(default)]
    font_family: Option<String>,
    #[serde(default)]
    font_size: Option<u32>,
    #[serde(default)]
    theme: Option<Value>,
    /// Shut the daemon down after this many seconds with no connected client.
    /// `Some(0)` explicitly disables it (the default tmux-style behavior where
    /// the daemon outlives the GUI); `None` = not set at this layer, so a
    /// workspace `0` can cancel a non-zero global value (M3).
    #[serde(default)]
    idle_shutdown_secs: Option<u64>,
    /// Controls how ended panes are treated on daemon bootstrap. Defaults to
    /// `auto_respawn` (revive ended panes). `restore_on_demand` lists them but
    /// does not auto-restart. An unrecognized value falls back to the default.
    #[serde(default)]
    restore_policy: Option<String>,
    /// Keyboard lease policy (docs/design/keyboard-lease-and-ledger.md).
    /// `open` (default): an unheld pane accepts input from anyone and a held
    /// pane only from its holder. `required`: every write needs the lease.
    /// An unrecognized value is rejected by `validate`.
    #[serde(default)]
    lease_policy: Option<String>,
    /// (M3b) How often the daemon polls `claude agents --json` to read Claude
    /// Code's own session state for shell panes (milliseconds). Unset =
    /// 2000; `0` disables the probe. While a probe result is fresh it
    /// outranks the screen heuristic. Unix only.
    #[serde(default)]
    agent_probe_interval_ms: Option<u64>,
    /// (M4) The `kranz` CLI used to read a bound pane's mission state and to
    /// mirror hand-back notes into its inbox. Unset resolves `SGIAN_KRANZ_BIN`
    /// and then `kranz` on PATH.
    #[serde(default)]
    kranz_bin: Option<String>,
    /// (M6) Client identity policy (docs/design/client-identity.md). `open`
    /// (default): the workspace token is a full credential, as before.
    /// `required`: the workspace token can read and administer identities but
    /// every write needs a per-client credential, so each keystroke and lease
    /// is attributed to one. An unrecognized value is rejected by `validate`.
    #[serde(default)]
    identity: Option<String>,
    /// (T2) Permission mode for agent-pane `claude` processes, passed to
    /// `--permission-mode`. Defaults to `manual`: every tool use that needs
    /// approval arrives as a `permission_request` agent event and blocks until
    /// an `AgentApproval` answers it. `bypassPermissions`/`acceptEdits`/`auto`/
    /// `dontAsk`/`plan` are passed through for operators who want unattended
    /// runs (no approval prompts are then expected).
    #[serde(default)]
    agent_permission_mode: Option<String>,
    /// (T2) Override for the `claude` CLI binary agent panes spawn. Unset
    /// resolves `claude` via PATH (with the `SGIAN_CLAUDE_BIN` environment
    /// variable as an operator override — see `resolve_agent_bin`). Tests point
    /// this at a fake driver script.
    #[serde(default)]
    agent_claude_bin: Option<String>,
    /// Override for the Factory Droid CLI. Unset resolves `droid` through
    /// PATH, with SGIAN_DROID_BIN as an operator-level escape hatch.
    #[serde(default)]
    agent_droid_bin: Option<String>,
    /// Named shell/agent profiles selectable when creating a pane
    /// (ENHANCEMENTS §4). Empty by default; workspace overlays replace the
    /// global list when non-empty.
    #[serde(default)]
    profiles: Vec<PaneProfile>,
}

/// A named creation preset for shell or agent panes (ENHANCEMENTS §4).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct PaneProfile {
    name: String,
    /// `"shell"`, `"agent"`, or omitted (inferred from agent_* fields).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_model: Option<String>,
}

impl PaneProfile {
    fn is_agent_profile(&self) -> bool {
        // Explicit shell kind wins even if agent_* fields were present (rejected
        // by validate); inferred agent when kind is omitted or "agent".
        if matches!(self.kind.as_deref(), Some("shell")) {
            return false;
        }
        matches!(self.kind.as_deref(), Some("agent"))
            || self.agent_backend.is_some()
            || self.agent_model.is_some()
    }
}

impl Config {
    /// Overlay `other` (the higher-priority source, e.g. per-workspace) onto `self`.
    fn overlay(self, other: Config) -> Config {
        let mut env = self.env;
        env.extend(other.env);
        // Union the scrub lists (additive security): a variable scrubbed by
        // either the global or the per-workspace config is scrubbed in the
        // effective config. Dedup preserves a stable order.
        let mut scrub_env = self.scrub_env.clone();
        for name in &other.scrub_env {
            if !scrub_env.contains(name) {
                scrub_env.push(name.clone());
            }
        }
        Config {
            shell: other.shell.or(self.shell),
            // Option semantics: a workspace layer that SETS the field wins even
            // when it sets the "empty" value ([] / 0); an absent field inherits.
            shell_args: other.shell_args.or(self.shell_args),
            env,
            scrub_env,
            font_family: other.font_family.or(self.font_family),
            font_size: other.font_size.or(self.font_size),
            theme: other.theme.or(self.theme),
            idle_shutdown_secs: other.idle_shutdown_secs.or(self.idle_shutdown_secs),
            restore_policy: other.restore_policy.or(self.restore_policy),
            lease_policy: other.lease_policy.or(self.lease_policy),
            agent_probe_interval_ms: other
                .agent_probe_interval_ms
                .or(self.agent_probe_interval_ms),
            kranz_bin: other.kranz_bin.or(self.kranz_bin),
            identity: other.identity.or(self.identity),
            agent_permission_mode: other.agent_permission_mode.or(self.agent_permission_mode),
            agent_claude_bin: other.agent_claude_bin.or(self.agent_claude_bin),
            agent_droid_bin: other.agent_droid_bin.or(self.agent_droid_bin),
            // Absent-vs-empty cannot be distinguished on Vec; treat empty
            // overlay as inherit so a workspace file that omits `profiles`
            // keeps the global list.
            profiles: if other.profiles.is_empty() {
                self.profiles
            } else {
                other.profiles
            },
        }
    }

    /// Effective idle-shutdown value: unset means disabled (0).
    fn idle_shutdown_secs_effective(&self) -> u64 {
        self.idle_shutdown_secs.unwrap_or(0)
    }

    /// Effective shell args: unset means none.
    fn shell_args_effective(&self) -> Vec<String> {
        self.shell_args.clone().unwrap_or_default()
    }

    fn shell_config(&self) -> ShellConfig {
        ShellConfig {
            shell: self
                .shell
                .clone()
                .filter(|shell| !shell.is_empty())
                .unwrap_or_else(default_shell),
            args: self.shell_args_effective(),
            env: self.env.clone(),
            scrub_env: self.scrub_env.clone(),
        }
    }

    /// The effective restore policy, defaulting to `auto_respawn` when unset or
    /// unrecognized. This is a pure function — the warning for an unrecognized
    /// value is logged once in `run_daemon_with_config` after the tracing
    /// dispatcher is active.
    /// Effective keyboard lease policy; unset (or, defensively, unparseable)
    /// falls back to `open` so a stale config can never lock every pane.
    fn lease_policy_effective(&self) -> LeasePolicy {
        self.lease_policy
            .as_deref()
            .and_then(LeasePolicy::parse)
            .unwrap_or(LeasePolicy::Open)
    }

    /// (M6) Effective identity policy; unset or unparseable falls back to
    /// `open` so a stale config can never lock the operator out.
    fn identity_effective(&self) -> IdentityPolicy {
        self.identity
            .as_deref()
            .and_then(IdentityPolicy::parse)
            .unwrap_or(IdentityPolicy::Open)
    }

    /// (M4) The `kranz` binary: config, then `SGIAN_KRANZ_BIN`, then PATH.
    fn kranz_bin_effective(&self) -> String {
        self.kranz_bin
            .clone()
            .filter(|bin| !bin.is_empty())
            .or_else(|| {
                std::env::var("SGIAN_KRANZ_BIN")
                    .ok()
                    .filter(|bin| !bin.is_empty())
            })
            .unwrap_or_else(|| "kranz".to_string())
    }

    /// (M3b) The official agent probe cadence; `None` when disabled.
    fn agent_probe_interval(&self) -> Option<Duration> {
        match self.agent_probe_interval_ms.unwrap_or(2000) {
            0 => None,
            millis => Some(Duration::from_millis(millis.max(250))),
        }
    }

    fn restore_policy_effective(&self) -> String {
        match self.restore_policy.as_deref() {
            Some("auto_respawn") | None => "auto_respawn".to_string(),
            Some("restore_on_demand") => "restore_on_demand".to_string(),
            Some(_) => "auto_respawn".to_string(),
        }
    }

    /// (T2) The effective agent permission mode, defaulting to `manual` (every
    /// approval-needing tool use round-trips through the daemon as a
    /// `permission_request` event + `AgentApproval` reply).
    fn agent_permission_mode_effective(&self) -> String {
        match self.agent_permission_mode.as_deref() {
            Some(mode) if AGENT_PERMISSION_MODES.contains(&mode) => mode.to_string(),
            _ => "manual".to_string(),
        }
    }

    /// (T2) The agent-spawn half of the effective config, mirrored into the
    /// TerminalStore alongside `shell_config` (and likewise refreshed by the
    /// config file-watch for newly-spawned sessions).
    fn agent_config(&self) -> AgentSpawnConfig {
        AgentSpawnConfig {
            claude_bin: self.agent_claude_bin.clone(),
            droid_bin: self.agent_droid_bin.clone(),
            permission_mode: self.agent_permission_mode_effective(),
        }
    }

    /// Validate a config that is about to be persisted via `write-config`.
    /// Returns `Err` with a clear message if any field has an invalid value
    /// (e.g. an unrecognized `restore_policy`). Type-level validation (wrong
    /// JSON types) is handled by serde deserialization before this is called.
    /// VAL-CFG-009: invalid input is rejected without corrupting existing config.
    fn validate(&self) -> Result<(), String> {
        if let Some(ref policy) = self.restore_policy {
            if !matches!(policy.as_str(), "auto_respawn" | "restore_on_demand") {
                return Err(format!(
                    "invalid restore_policy '{policy}': must be 'auto_respawn' or 'restore_on_demand'"
                ));
            }
        }
        if let Some(ref policy) = self.lease_policy {
            if LeasePolicy::parse(policy).is_none() {
                return Err(format!(
                    "invalid lease_policy '{policy}': must be 'open' or 'required'"
                ));
            }
        }
        if let Some(ref policy) = self.identity {
            if IdentityPolicy::parse(policy).is_none() {
                return Err(format!(
                    "invalid identity '{policy}': must be 'open' or 'required'"
                ));
            }
        }
        if let Some(ref mode) = self.agent_permission_mode {
            if !AGENT_PERMISSION_MODES.contains(&mode.as_str()) {
                return Err(format!(
                    "invalid agent_permission_mode '{mode}': must be one of {}",
                    AGENT_PERMISSION_MODES.join(", ")
                ));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for profile in &self.profiles {
            let name = profile.name.as_str();
            if name.trim().is_empty() {
                return Err("profile name must be non-empty".to_string());
            }
            if name != name.trim() {
                return Err(format!(
                    "profile name '{name}' must not have leading or trailing whitespace"
                ));
            }
            if !seen.insert(name.to_string()) {
                return Err(format!("duplicate profile name '{name}'"));
            }
            if let Some(kind) = profile.kind.as_deref() {
                if kind != "shell" && kind != "agent" {
                    return Err(format!(
                        "invalid profile kind '{kind}' for '{name}': must be 'shell' or 'agent'"
                    ));
                }
            }
            if let Some(backend) = profile.agent_backend.as_deref() {
                if backend != "claude" && backend != "droid" {
                    return Err(format!(
                        "invalid profile agent_backend '{backend}' for '{name}': expected claude or droid"
                    ));
                }
            }
            let kind = profile.kind.as_deref();
            let has_agent_fields = profile.agent_backend.is_some() || profile.agent_model.is_some();
            let has_shell_fields =
                profile.shell.is_some() || profile.shell_args.is_some() || !profile.env.is_empty();
            if kind == Some("shell") && has_agent_fields {
                return Err(format!(
                    "profile '{name}' kind is shell but sets agent_backend/agent_model"
                ));
            }
            if kind == Some("agent") && has_shell_fields {
                return Err(format!(
                    "profile '{name}' kind is agent but sets shell/shell_args/env"
                ));
            }
        }
        Ok(())
    }

    /// All editable fields as a JSON value, including `env` (used by `get_config`
    /// for the settings modal, which is GUI-only and not accessible via `ctl`).
    /// This is the full effective config (global overlaid by per-workspace).
    /// `scrub_env` is included so a get→edit→write round-trip carries it (M1);
    /// Option fields are emitted in their effective form so the shape the GUI
    /// and `ctl config` consume is unchanged.
    fn full_config(&self) -> Value {
        json!({
            "shell": self.shell,
            "shell_args": self.shell_args_effective(),
            "env": self.env,
            "scrub_env": self.scrub_env,
            "font_family": self.font_family,
            "font_size": self.font_size,
            "theme": self.theme,
            "idle_shutdown_secs": self.idle_shutdown_secs_effective(),
            "restore_policy": self.restore_policy_effective(),
            "agent_permission_mode": self.agent_permission_mode_effective(),
            "agent_claude_bin": self.agent_claude_bin,
            "agent_droid_bin": self.agent_droid_bin,
            "profiles": self.profiles,
        })
    }

    /// Effective (merged) config summary for `status --verbose`. Deliberately
    /// excludes the `env` map (which may hold secrets) — observability surfaces
    /// must never expose configured env values (VAL-SEC-010). Includes `env`
    /// key names (with null values) so the field is present without leaking
    /// secret values (VAL-CFG-001).
    fn summary(&self) -> Value {
        // Include env key names with null values so the field is present
        // (VAL-CFG-001) without leaking secret values (VAL-SEC-010).
        let env_keys: std::collections::BTreeMap<&str, Value> = self
            .env
            .keys()
            .map(|key| (key.as_str(), Value::Null))
            .collect();
        // Profiles: names + kinds only (env maps on profiles are redacted).
        let profiles: Vec<Value> = self
            .profiles
            .iter()
            .map(|profile| {
                json!({
                    "name": profile.name,
                    "kind": profile.kind,
                    "shell": profile.shell,
                    "agent_backend": profile.agent_backend,
                    "agent_model": profile.agent_model,
                    "env_keys": profile.env.keys().cloned().collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "shell": self.shell,
            // (M10) The EFFECTIVE shell program the daemon would spawn (configured
            // `shell` or the daemon-side default): ctl resolves the shell family
            // from this so it cannot diverge by re-resolving $SHELL in the ctl
            // process's own environment. Additive — the raw `shell` field above
            // keeps its exact shape (the scrubbed-summary shape is load-bearing).
            "resolved_shell": self.shell_config().shell,
            "shell_args": self.shell_args_effective(),
            "env": env_keys,
            // Variable NAMES only — the scrub list never contains values.
            "scrub_env": self.scrub_env,
            "font_family": self.font_family,
            "font_size": self.font_size,
            "theme": self.theme,
            "idle_shutdown_secs": self.idle_shutdown_secs_effective(),
            "restore_policy": self.restore_policy_effective(),
            "agent_permission_mode": self.agent_permission_mode_effective(),
            // The binary PATH/name only — never resolved env or secrets.
            "agent_claude_bin": self.agent_claude_bin,
            "agent_droid_bin": self.agent_droid_bin,
            "profiles": profiles,
        })
    }

    fn profile(&self, name: &str) -> Option<&PaneProfile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    /// Merge a shell profile onto the effective shell config (profile wins on
    /// shell/args; profile env overlays the base env map).
    fn shell_config_for_profile(&self, profile: &PaneProfile) -> ShellConfig {
        let mut shell = self.shell_config();
        if let Some(program) = profile.shell.as_ref().filter(|value| !value.is_empty()) {
            shell.shell = program.clone();
        }
        if let Some(args) = &profile.shell_args {
            shell.args = args.clone();
        }
        for (key, value) in &profile.env {
            shell.env.insert(key.clone(), value.clone());
        }
        shell
    }
}

/// The shell command, args, and extra environment a pane's PTY is spawned with.
/// Persisted in `workspace.json` as a frozen create-time profile snapshot so
/// restarts (daemon or in-process) keep the same shell/args/env.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ShellConfig {
    shell: String,
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// Inherited env var names to scrub before applying `env`. Explicit `env`
    /// entries take precedence over this list (applied after scrubbing).
    #[serde(default)]
    scrub_env: Vec<String>,
}

/// Read one config layer. `Ok(None)` means the file is absent (fine — the layer
/// is simply unset); `Err` means the file exists but is unreadable or malformed.
/// Malformed configs must be SURFACED, not silently defaulted (M2): a stray
/// comma would otherwise quietly discard the operator's scrub_env list, custom
/// shell, and idle settings.
fn read_config_file(path: &Path) -> Result<Option<Config>, String> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read config {}: {error}", path.display())),
    };
    let config: Config = serde_json::from_str(&data)
        .map_err(|error| format!("malformed config {}: {error}", path.display()))?;
    config
        .validate()
        .map_err(|error| format!("invalid config {}: {error}", path.display()))?;
    Ok(Some(config))
}

/// Load the merged (global overlaid by workspace) config. A malformed layer is
/// treated as absent for the merge but reported in the returned warnings so the
/// caller can log it loudly (startup) or refuse the reload entirely (file watch).
fn load_config(data_dir: &Path) -> (Config, Vec<String>) {
    let mut warnings = Vec::new();
    let mut layer = |path: PathBuf| match read_config_file(&path) {
        Ok(Some(config)) => config,
        Ok(None) => Config::default(),
        Err(warning) => {
            warnings.push(warning);
            Config::default()
        }
    };
    let global = layer(app_support_dir().join(CONFIG_FILE));
    let workspace = layer(data_dir.join(CONFIG_FILE));
    (global.overlay(workspace), warnings)
}

/// Compute the environment a spawned pane should see, given the daemon's
/// inherited environment, a scrub list (variable names to strip from the
/// inherited env), and an explicit `env` map (operator-set values). Scrubbing
/// removes only *inherited* values; an explicit `env` entry for the same name
/// takes precedence (applied after scrubbing). With an empty scrub list the
/// inherited environment is returned unchanged (default inheritance
/// preserved). This is a pure test oracle for the scrubbing logic;
/// `spawn_pane` applies the same logic via `CommandBuilder::env_remove` /
/// `env` (verified end-to-end by `env_scrubbing_integration_spawned_pane`).
/// VAL-SEC-003/004/007.
/// Claude Code marks its own subprocesses so a nested `claude` knows it is a
/// child session (transcripts off, not listed by `claude agents`). A daemon
/// started from inside such a session would otherwise pass those marks to
/// every pane it ever spawns, since it outlives the session. Pane shells are
/// the operator's, not Claude's tools, so the marks are always dropped; an
/// explicit `env` entry still wins.
const INHERITED_SESSION_MARKERS: [&str; 3] = [
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_ENTRYPOINT",
];

#[cfg(test)]
fn compute_spawn_env(
    inherited: &HashMap<String, String>,
    scrub: &[String],
    explicit: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env = inherited.clone();
    for key in INHERITED_SESSION_MARKERS {
        env.remove(key);
    }
    for key in scrub {
        env.remove(key);
    }
    for (key, value) in explicit {
        env.insert(key.clone(), value.clone());
    }
    env
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case")]
enum DaemonRequest {
    Ping,
    BootstrapWorkspace,
    ListPanes,
    PaneStatus {
        pane_id: String,
    },
    CreatePane {
        title: Option<String>,
        /// Optional named shell profile (ENHANCEMENTS §4). Ignored/rejected for
        /// agent profiles — those go through CreateAgentPane.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    ClosePane {
        pane_id: String,
    },
    RenamePane {
        pane_id: String,
        title: String,
    },
    EnsurePaneTerminal {
        pane_id: String,
    },
    RestartPaneTerminal {
        pane_id: String,
    },
    WriteToPane {
        pane_id: String,
        data: String,
    },
    SendInput {
        pane_id: String,
        input: String,
    },
    /// `SendInput` attributed to a keyboard-lease holder
    /// (docs/design/keyboard-lease-and-ledger.md). The legacy `WriteToPane` /
    /// `SendInput` carry no holder and are refused while a pane is held.
    SendInputAs {
        pane_id: String,
        input: String,
        holder: String,
        /// Optional: the lease generation the writer believes it holds; a
        /// mismatch is refused as stale (docs/design/keyboard-lease-and-ledger.md §3).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u64>,
    },
    /// Claim a pane's keyboard for `holder`. Idempotent for the current
    /// holder; against another holder it needs `force` plus a `why`, and both
    /// the revocation and the new claim are ledgered.
    TakeLease {
        pane_id: String,
        holder: String,
        #[serde(default)]
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        why: Option<String>,
    },
    /// Hand a pane's keyboard back. The note is mandatory: it is the record.
    ReleaseLease {
        pane_id: String,
        holder: String,
        note: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        generation: Option<u64>,
    },
    LeaseStatus {
        pane_id: String,
    },
    /// (M4) Bind a pane to a Kranz mission by hand (`repo` defaults to the
    /// pane's cwd); auto bindings come from a `kranz run` under the pane.
    KranzBind {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    KranzUnbind {
        pane_id: String,
    },
    KranzBindings,
    /// Projects (docs/design/keyboard-lease-and-ledger.md §7): a named group
    /// of panes serving one goal, with an attention roll-up and a merged
    /// ledger. Panes belong to at most one project.
    ProjectCreate {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    ProjectDelete {
        name: String,
    },
    ProjectAssign {
        name: String,
        pane_id: String,
    },
    ProjectUnassign {
        pane_id: String,
    },
    ProjectList,
    ProjectShow {
        name: String,
    },
    ProjectLedger {
        name: String,
        #[serde(default)]
        limit: usize,
    },
    /// Everything about a project in one document: the roll-up, each member
    /// pane's state, its full ledger with the chain verified, and the tail of
    /// its scrollback with controls stripped. `lines` = scrollback lines per
    /// pane (0 = default). The receipt half of docs/design/execution-grants.md.
    ProjectDossier {
        name: String,
        #[serde(default)]
        lines: usize,
    },
    /// (M6) Issue a per-client credential: the token is returned once.
    IdentityIssue {
        holder: String,
        #[serde(default)]
        scopes: Vec<String>,
    },
    IdentityList,
    IdentityRevoke {
        id: String,
    },
    /// Who this connection is: credential, holder, scopes, policy.
    Whoami,
    /// Claude Code's status-line payload from a session inside some pane
    /// (`ctl statusline`): `pid` is the status-line process, `payload` the
    /// JSON Claude Code wrote to it. Never an error when nothing matches.
    AgentStatus {
        pid: u32,
        payload: Value,
    },
    /// A Claude Code hook fired inside some pane (`ctl hook`): `pid` is the
    /// hook process (the daemon walks its ancestry to the pane), `event` the
    /// `hook_event_name`, `notification_type` the Notification kind. Maps to
    /// an official attention reading with evidence `hook`; never an error
    /// when nothing matches (a hook must not fail the session).
    AgentSignal {
        pid: u32,
        event: String,
        #[serde(default)]
        notification_type: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },
    ResizePaneTerminal {
        pane_id: String,
        cols: u16,
        rows: u16,
    },
    SetActivePane {
        pane_id: String,
    },
    GetScrollback {
        pane_id: String,
    },
    /// Substring search over a pane's whole scrollback file with terminal
    /// control sequences stripped. Line numbers are 1-based and relative to
    /// the current file: the 16 MiB cap drops the oldest half and renumbers,
    /// which `total_lines` lets a script notice.
    SearchScrollback {
        pane_id: String,
        needle: String,
        #[serde(default)]
        ignore_case: bool,
        #[serde(default)]
        limit: usize,
    },
    /// Lines `from..=to` (1-based, inclusive) of a pane's scrollback, control
    /// sequences stripped — the range a ledger record or a search hit cites.
    ScrollbackLines {
        pane_id: String,
        from: usize,
        to: usize,
    },
    UpdateWorkspaceLayout {
        layout: Value,
    },
    GetConfig,
    Broadcast {
        input: String,
    },
    SetSyncInput {
        enabled: bool,
    },
    /// (T1) Manually mark a pane as running an agent CLI (`Some("claude")`) or
    /// clear the mark (`None`, returning the pane to auto-detection). Manual
    /// marks override screen-signature detection and are persisted in
    /// workspace.json; the daemon responds with the pane's current agent state.
    SetPaneAgent {
        pane_id: String,
        agent: Option<String>,
    },
    /// Read-only verbose status: subscriber count, per-pane runtime states,
    /// monotonic uptime, and effective config summary. Never spawns a daemon.
    StatusVerbose,
    /// Block until a pane satisfies `condition` (text/regex on the visible screen,
    /// idle, or exit) or `timeout_ms` elapses. Runs on the connection's own thread and
    /// holds no registry/router/parser lock while blocking, so it never stalls other
    /// daemon operations (architecture §6.3; VAL-PRIM-001..016/049/050).
    Wait {
        pane_id: String,
        condition: WaitCondition,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    /// Read a pane's rendered visible-screen state plus its spawn/exit metadata into
    /// the documented snapshot struct (architecture §6.3). CONSUMES the existing data
    /// layer without recomputing it: the vt100 model supplies lines/cursor/revision/
    /// size/title via `OutputRouter::model_handle`, while command/cwd/exit_code come
    /// from `TerminalStore::pane_meta` and `alive` from `is_live`. Read-only; holds
    /// only tiny, brief locks (VAL-PRIM-018..027/051/052).
    Snapshot {
        pane_id: String,
    },
    /// Query the workspace's panes by metadata, returning an array of per-pane
    /// metadata for every pane that matches ALL supplied filters (architecture §6.3).
    /// `command`/`title`/`cwd` are case-sensitive substring matches; `state` filters by
    /// runtime liveness (`Live`/`Ended`). Absent filters impose no constraint, so a
    /// request with every field `None` returns all panes (live and ended). Metadata is
    /// CONSUMED from the existing data layer — command/cwd/exit_code from
    /// `TerminalStore::pane_meta`, liveness from `is_live`/`runtime_states`, and
    /// title/revision/size from the vt100 model via `OutputRouter::model_handle`
    /// (VAL-PRIM-028..039/052/054). Agent/group filters arrive in MC; the fields are
    /// reported (nullable) here. Read-only; holds only tiny, brief locks.
    Find {
        // Renamed on the wire because the enum's internal tag key is itself `command`;
        // a variant field literally named `command` would collide with that tag.
        #[serde(
            rename = "command_filter",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<PaneRuntimeState>,
    },
    /// Persist a new configuration to the per-workspace config.json atomically.
    /// The file-watch picks up the change, reloads config into the daemon's
    /// mutable shared state, and broadcasts a `ConfigChanged` event. Invalid
    /// input is rejected without corrupting the existing config.
    WriteConfig {
        config: Value,
    },
    /// (T2) Create an agent-kind pane: a chat-native session backed by a
    /// headless `claude` CLI process (stream-json over piped stdio) owned by
    /// the daemon, instead of a PTY shell. Counts toward MAX_PANES like any
    /// pane. Responds with the created Pane json (`kind: "agent"`).
    CreateAgentPane {
        title: Option<String>,
    },
    /// Provider-aware form of CreateAgentPane. Kept additive so older GUI/ctl
    /// clients can continue sending the original request, which defaults to
    /// Claude.
    CreateAgentPaneWithSpec {
        title: Option<String>,
        #[serde(default)]
        backend: Option<AgentBackendKind>,
        #[serde(default)]
        model: Option<String>,
    },
    /// (T2) Post one user message to an agent pane's conversation: written as a
    /// single stream-json line on the CLI's stdin. One in-flight turn per pane
    /// — a send while the agent is still running a turn is rejected (the CLI
    /// protocol has no interleave; queueing client-side would reorder replies
    /// against permission prompts). Spawns the session first if the pane has
    /// no live one (ended/never-spawned), resuming the recorded CLI session.
    SendAgentMessage {
        pane_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    /// (T2) Answer a pending `permission_request` agent event. `request_id`
    /// matches the event's; `allow: false` denies with `message` as feedback
    /// to the agent (the CLI surfaces it to the model). Unknown/stale request
    /// ids are an error.
    AgentApproval {
        pane_id: String,
        request_id: String,
        allow: bool,
        message: Option<String>,
    },
    /// (T2) Interrupt the agent's current turn (the CLI's `interrupt` control
    /// request). The turn ends with an error-subtype `turn_complete`; the
    /// process stays alive for later messages. Agent panes only.
    InterruptAgent {
        pane_id: String,
    },
    /// (ENHANCEMENTS §3) Run argv directly in the daemon (non-PTY) with exact
    /// exit-code semantics for automation. Does not touch interactive panes.
    RunProcess {
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
    },
    Subscribe,
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
enum DaemonEvent {
    PtyOutput {
        pane_id: String,
        data: String,
    },
    PaneEnded {
        pane_id: String,
        /// Exit code captured by the reaper (`child.wait()`): the real process exit
        /// status, or `None` for a signal death / unreadable status. Additive +
        /// omitted when absent, so a None event keeps the byte-identical pre-MB shape
        /// (`pane_id` only) and a legacy payload still deserializes (Invariant 8).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// Sent as the FIRST event on a subscription — after the subscriber is
    /// registered, before any catch-up or live events — when BOTH sides
    /// advertised the `subscribe-ack` capability in the handshake. Reading it
    /// guarantees the registration happened, closing the subscribe-then-send
    /// race (M8): input sent on another connection after the ack cannot have
    /// its output broadcast before this subscriber joined. Capability-gated in
    /// both directions, so old↔new pairings keep the historical ack-less flow.
    SubscribeAck,
    // Registry changes are broadcast so every client (GUI, `ctl attach`) stays in
    // sync with panes created/closed/renamed by any other client. Old clients skip
    // event tags they don't know, so these are protocol-compatible additions.
    PaneCreated {
        pane: Pane,
    },
    PaneClosed {
        pane_id: String,
    },
    PaneRenamed {
        pane: Pane,
    },
    /// Broadcast when the daemon's file-watch detects a config.json change
    /// (external edit or write-config). Carries the new effective config
    /// summary (all editable fields except `env` values, which may hold
    /// secrets). This makes config no longer frozen at construction.
    ConfigChanged {
        config: Value,
    },
    /// (T1) A pane's agent state TRANSITIONED: the agent was detected/cleared/
    /// re-marked, or its attention classification changed. Broadcast on
    /// transitions only (never per output chunk). `agent`/`attention` are null
    /// when the pane has no known agent (e.g. a detected agent exited and its
    /// signature left the screen). Old clients skip the unknown event tag,
    /// same as the registry-change events above.
    AgentState {
        pane_id: String,
        agent: Option<String>,
        attention: Option<AgentAttention>,
        /// Observed permission mode (see `AgentPaneInfo::mode`); additive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    /// Output-guard hit: `added` since the last announcement, `total` so far
    /// (docs/design/keyboard-lease-and-ledger.md §7). Rate-limited per pane.
    OutputWarning {
        pane_id: String,
        added: OutputTricks,
        total: OutputTricks,
    },
    /// A pane's usage reading changed (see `AgentUsage`). Frequent (every
    /// turn of a session) and not the product: never ledgered.
    AgentUsage {
        pane_id: String,
        usage: AgentUsage,
    },
    /// The project table after a change (create, delete, assign, unassign,
    /// or a member pane closing): the whole map, small and idempotent, so a
    /// client renders a board without diffing. Old clients skip the tag.
    ProjectsChanged {
        projects: HashMap<String, Project>,
    },
    /// Keyboard lease transition (docs/design/keyboard-lease-and-ledger.md).
    /// `holder`/`since_ms` describe the lease AFTER the transition (null once
    /// released or revoked); `note` rides a release. Old clients skip the
    /// unknown event tag.
    LeaseState {
        pane_id: String,
        transition: LeaseTransition,
        holder: Option<String>,
        since_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// (T2) One NORMALIZED conversation event from an agent pane's `claude`
    /// stream-json output. `event` is an object tagged by its `kind` field:
    /// `session` (init/resume: session_id, model), `message_start` /
    /// `text_delta` / `message_complete` (assistant streaming, driven by
    /// --include-partial-messages), `tool_use` / `tool_result`,
    /// `permission_request` (blocks the CLI until an AgentApproval answers),
    /// `permission_resolved` (the request was answered — allow/deny, with
    /// reason "user"/"timeout"/"closed"/"process_exit"), `turn_complete`
    /// (subtype/usage/cost_usd), `error`, and `process_exit`. Every event
    /// carries the pane's monotonic `seq` (per-pane u64 from 1, continuing
    /// across respawns/restarts; the JSONL log and the bootstrap replay
    /// carry the same stamped objects) so clients can dedupe replay-vs-live.
    /// Old clients skip the unknown event tag like the other additions above.
    ///
    /// The field is renamed on the DAEMON wire because the enum's internal
    /// tag key is itself `event` (same collision as `Find.command`); the
    /// Tauri-level `"agent-event"` payload keeps the contract's
    /// `{pane_id, event}` shape verbatim (see emit_daemon_event).
    AgentEvent {
        pane_id: String,
        #[serde(rename = "payload")]
        event: Value,
    },
}

/// The single blocking condition a `ctl wait` request waits on. Exactly one is
/// supplied per request (the CLI enforces this; see `parse_wait_args`). `Text`/`Regex`
/// match against the pane's *visible* vt100 screen (not scrollback); `Idle` resolves
/// after the pane's revision is unchanged for the given milliseconds; `Exit` resolves
/// when the pane's process has ended. Adjacently tagged so the unit `Exit` variant and
/// the data-carrying variants both round-trip over the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
enum WaitCondition {
    Text(String),
    Regex(String),
    Idle(u64),
    Exit,
}

/// Poll cadence for a blocking `wait`. The handler re-checks its condition this often,
/// holding the per-pane/registry locks only for the brief read each tick and never
/// across the sleep, so a blocking wait never stalls other daemon operations
/// (Invariant 9 / VAL-PRIM-014).
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The terminal cursor position reported by `snapshot`, 0-based `(row, col)`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
struct SnapshotCursor {
    row: u16,
    col: u16,
}

/// The documented `ctl snapshot` struct (architecture §6.3): a pane's rendered
/// visible screen plus its spawn/exit/runtime metadata, all CONSUMED from the
/// existing data layer (vt100 model + `pane_meta` + `is_live`), never recomputed.
///
/// `exit_code` is `Option<Option<i32>>` so its three documented states are distinct:
/// `None` ⇒ the key is omitted (a live pane has no exit code yet); `Some(None)` ⇒
/// serialized as JSON `null` (the pane ended but was killed by a signal, whose numeric
/// code portable-pty erases — reported null, never a misleading `1`); `Some(Some(code))`
/// ⇒ the real code of a normally-exited pane. `group`/`origin` are the MC §7.1
/// fields; until `Pane` carries them they report the documented defaults (no group,
/// origin `User`). (T1) `agent` reports the pane's detected/manually-marked agent
/// name (null when none); `attention` carries the classified attention state and is
/// omitted while the pane has no agent.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct PaneSnapshot {
    pane_id: String,
    cols: u16,
    rows: u16,
    lines: Vec<String>,
    title: String,
    revision: u64,
    cursor: SnapshotCursor,
    alive: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<Option<i32>>,
    command: Option<String>,
    cwd: Option<String>,
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attention: Option<AgentAttention>,
    group: Option<String>,
    origin: String,
}

/// One entry of the documented `ctl find` result (architecture §6.3): a pane's
/// queryable metadata, CONSUMED from the existing data layer (never recomputed).
/// `state` serializes as `"live"`/`"ended"`. `exit_code` is `Option<Option<i32>>`
/// with the same three-state meaning as `PaneSnapshot::exit_code`: omitted while the
/// pane is live, `null` for a signal death, and the real code for a normal exit — so an
/// ended entry always carries its captured code (VAL-PRIM-038). `group` is the
/// MC §7.1 field, reported as `null` until `Pane` carries it (agent/group *filters*
/// arrive in MC; the find output already lists the nullable fields per §6.3).
/// (T1) `agent` reports the pane's detected/manually-marked agent name (null when
/// none); `attention` carries the classified attention state, omitted while the
/// pane has no agent.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct FindEntry {
    id: String,
    title: String,
    command: Option<String>,
    cwd: Option<String>,
    state: PaneRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<Option<i32>>,
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attention: Option<AgentAttention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(default)]
    unattended: bool,
    /// Output-guard counters; absent when nothing was flagged.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_warnings: Option<OutputTricks>,
    /// Status-line usage; absent until a session under the pane reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<AgentUsage>,
    group: Option<String>,
    cols: u16,
    rows: u16,
    revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct IpcHello {
    #[serde(rename = "type")]
    frame_type: String,
    /// Legacy protocol version. A new client still sends `1` here so an unmodified
    /// v1 daemon accepts the hello on its version check; wire-version negotiation is
    /// driven by the additive `max_wire_version` instead.
    version: u32,
    token: String,
    /// Highest wire version the client supports (additive; absent ⇒ treated as 1).
    /// `IpcHello` must NOT use `deny_unknown_fields` so a v1 daemon (which lacks this
    /// field) still parses a new client's hello, and a new daemon still parses a
    /// legacy hello that omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_wire_version: Option<u16>,
    /// Optional client capability advertisement (additive; not required by the
    /// daemon today — the daemon advertises its own capabilities in the response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<Vec<String>>,
    /// (M6) A per-client credential issued by `ctl identity issue`
    /// (docs/design/client-identity.md). Additive: an old daemon ignores it
    /// and authenticates on `token` alone; a client that presents one may
    /// leave `token` empty. A presented credential must be valid: a revoked
    /// or unknown one is refused even beside a valid workspace token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_token: Option<String>,
}

// ---------------------------------------------------------------------------
// (M6) Per-client identity: docs/design/client-identity.md.
// ---------------------------------------------------------------------------

/// What a credential may do. `read`: snapshot, subscribe, search, dossier,
/// hook and status-line reports. `write`: input, leases, pane lifecycle,
/// projects. `admin`: identities, config, shutdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ClientScope {
    Read,
    Write,
    Admin,
}

impl ClientScope {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityPolicy {
    Open,
    Required,
}

impl IdentityPolicy {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "open" => Some(Self::Open),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Required => "required",
        }
    }
}

/// One issued credential. The token itself is shown once at issue time and
/// only its hash is kept.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ClientRecord {
    id: String,
    holder: String,
    scopes: Vec<ClientScope>,
    token_hash: String,
    created_at_ms: u64,
    #[serde(default)]
    last_seen_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at_ms: Option<u64>,
}

impl ClientRecord {
    /// The listing shape: everything but the hash.
    fn public(&self) -> Value {
        json!({
            "id": self.id,
            "holder": self.holder,
            "scopes": self.scopes,
            "created_at_ms": self.created_at_ms,
            "last_seen_ms": self.last_seen_ms,
            "revoked_at_ms": self.revoked_at_ms,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ClientsFile {
    #[serde(default)]
    clients: Vec<ClientRecord>,
}

const CLIENT_TOKEN_PREFIX: &str = "sgc_";
const CLIENT_TOKEN_HASH_PREFIX: &str = "sgian.client.v1\n";
const MAX_CLIENT_RECORDS: usize = 256;

fn client_token_hash(token: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(CLIENT_TOKEN_HASH_PREFIX.as_bytes());
    hasher.update(token.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Who a connection is (docs/design/client-identity.md). `credential` and
/// `holder` are `None` for the workspace token (the root credential): its
/// holder stays self-declared, as before M6.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientIdentity {
    credential: Option<String>,
    holder: Option<String>,
    scopes: Vec<ClientScope>,
}

impl ClientIdentity {
    /// The workspace token: everything under `open`; read and admin (no
    /// writes) under `required`, so every keystroke needs a credential.
    fn root(policy: IdentityPolicy) -> Self {
        let scopes = match policy {
            IdentityPolicy::Open => vec![ClientScope::Read, ClientScope::Write, ClientScope::Admin],
            IdentityPolicy::Required => vec![ClientScope::Read, ClientScope::Admin],
        };
        Self {
            credential: None,
            holder: None,
            scopes,
        }
    }

    fn from_record(record: &ClientRecord) -> Self {
        let mut scopes = record.scopes.clone();
        if !scopes.contains(&ClientScope::Read) {
            scopes.push(ClientScope::Read);
        }
        Self {
            credential: Some(record.id.clone()),
            holder: Some(record.holder.clone()),
            scopes,
        }
    }

    fn has(&self, scope: ClientScope) -> bool {
        self.scopes.contains(&scope)
    }

    fn describe(&self, policy: IdentityPolicy) -> Value {
        json!({
            "credential": self.credential,
            "holder": self.holder,
            "scopes": self.scopes,
            "root": self.credential.is_none(),
            "identity_policy": policy.name(),
        })
    }
}

/// The scope a request needs. Reads include the hook and status-line
/// reports (observations, not keystrokes). Anything not listed is a write.
fn request_scope(request: &DaemonRequest) -> ClientScope {
    match request {
        DaemonRequest::Ping
        | DaemonRequest::BootstrapWorkspace
        | DaemonRequest::ListPanes
        | DaemonRequest::PaneStatus { .. }
        | DaemonRequest::LeaseStatus { .. }
        | DaemonRequest::KranzBindings
        | DaemonRequest::ProjectList
        | DaemonRequest::ProjectShow { .. }
        | DaemonRequest::ProjectLedger { .. }
        | DaemonRequest::ProjectDossier { .. }
        | DaemonRequest::AgentStatus { .. }
        | DaemonRequest::AgentSignal { .. }
        | DaemonRequest::GetScrollback { .. }
        | DaemonRequest::SearchScrollback { .. }
        | DaemonRequest::ScrollbackLines { .. }
        | DaemonRequest::GetConfig
        | DaemonRequest::StatusVerbose
        | DaemonRequest::Wait { .. }
        | DaemonRequest::Snapshot { .. }
        | DaemonRequest::Find { .. }
        | DaemonRequest::Subscribe
        | DaemonRequest::Whoami => ClientScope::Read,
        DaemonRequest::WriteConfig { .. }
        | DaemonRequest::Shutdown
        | DaemonRequest::IdentityIssue { .. }
        | DaemonRequest::IdentityList
        | DaemonRequest::IdentityRevoke { .. } => ClientScope::Admin,
        _ => ClientScope::Write,
    }
}

fn request_name(request: &DaemonRequest) -> String {
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value["command"].as_str().map(str::to_string))
        .unwrap_or_else(|| "request".to_string())
}

/// Bind a credentialed connection's writes to its holder: unattributed input
/// becomes attributed input, a declared holder must match, and broadcast (no
/// holder) is refused. The root credential passes through unchanged.
fn bind_holder(request: DaemonRequest, identity: &ClientIdentity) -> Result<DaemonRequest, String> {
    let Some(own) = identity.holder.as_deref() else {
        return Ok(request);
    };
    let mismatch = |declared: &str| {
        format!("holder '{declared}' does not match this credential's holder '{own}'")
    };
    Ok(match request {
        DaemonRequest::SendInput { pane_id, input }
        | DaemonRequest::WriteToPane {
            pane_id,
            data: input,
        } => DaemonRequest::SendInputAs {
            pane_id,
            input,
            holder: own.to_string(),
            generation: None,
        },
        DaemonRequest::SendInputAs { ref holder, .. }
        | DaemonRequest::TakeLease { ref holder, .. }
        | DaemonRequest::ReleaseLease { ref holder, .. }
            if holder != own =>
        {
            return Err(mismatch(holder));
        }
        DaemonRequest::Broadcast { .. } => {
            return Err(
                "broadcast has no holder; a credentialed client sends per pane".to_string(),
            );
        }
        other => other,
    })
}

/// The per-client token this process presents, if any: `SGIAN_CLIENT_TOKEN`,
/// else the first line of the file named by `SGIAN_CLIENT_TOKEN_FILE`.
fn client_token_from_env() -> Option<String> {
    if let Ok(token) = std::env::var("SGIAN_CLIENT_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    let path = std::env::var_os("SGIAN_CLIENT_TOKEN_FILE")?;
    read_token(Path::new(&path)).ok().flatten()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IpcResponse {
    ok: bool,
    #[serde(default)]
    result: Value,
    error: Option<String>,
}

#[derive(Debug)]
pub struct PaneRegistry {
    panes: Vec<Pane>,
    active_pane_id: Option<String>,
    next_id: u64,
    cwd: String,
}

impl PaneRegistry {
    pub fn new(cwd: String) -> Self {
        let mut registry = Self {
            panes: Vec::new(),
            active_pane_id: None,
            next_id: 1,
            cwd,
        };
        registry.create_pane(Some("term-1".to_string()));
        registry
    }

    fn from_persisted(persisted: PersistedWorkspace, cwd: String) -> Self {
        let mut panes = persisted.panes;
        // Pane ids are used to build scrollback file paths; never trust on-disk ids
        // that don't match the canonical `pane-<n>` shape.
        panes.retain(|pane| is_valid_pane_id(&pane.id));
        // A tampered/duplicated persisted file must not yield two panes with the
        // same id (registry lookups assume uniqueness); keep the first (L14).
        let mut seen_ids = HashSet::new();
        panes.retain(|pane| seen_ids.insert(pane.id.clone()));
        if panes.is_empty() {
            panes.push(Pane {
                id: "pane-1".to_string(),
                title: "term-1".to_string(),
                kind: PaneKind::Shell,
                created_at_ms: now_millis(),
            });
        }

        let active_pane_id = persisted
            .active_pane_id
            .filter(|id| panes.iter().any(|pane| pane.id == *id))
            .or_else(|| panes.first().map(|pane| pane.id.clone()));
        let next_id = persisted.next_id.max(next_pane_id_after(&panes)).max(1);

        Self {
            panes,
            active_pane_id,
            next_id,
            cwd,
        }
    }

    pub fn create_pane(&mut self, title: Option<String>) -> Pane {
        self.create_pane_with_kind(title, PaneKind::Shell)
    }

    /// (T2) Create a pane of an explicit kind. Agent panes default to an
    /// `agent-N` fallback title so an unnamed agent pane reads differently
    /// from an unnamed shell in the pane list.
    pub fn create_pane_with_kind(&mut self, title: Option<String>, kind: PaneKind) -> Pane {
        let id = format!("pane-{}", self.next_id);
        // Saturating (L14): a tampered persisted next_id at u64::MAX must not
        // panic (debug) or wrap to id reuse (release). Unreachable organically.
        self.next_id = self.next_id.saturating_add(1);

        let fallback_title = match kind {
            PaneKind::Shell => format!("term-{}", self.next_id - 1),
            PaneKind::Agent => format!("agent-{}", self.next_id - 1),
        };
        let pane = Pane {
            id,
            title: clean_title(title).unwrap_or(fallback_title),
            kind,
            created_at_ms: now_millis(),
        };

        self.active_pane_id = Some(pane.id.clone());
        self.panes.push(pane.clone());
        pane
    }

    pub fn close_pane(&mut self, pane_id: &str) -> Result<WorkspaceSnapshot, String> {
        if self.panes.len() <= 1 {
            return Err("at least one pane must remain open".to_string());
        }

        let before = self.panes.len();
        self.panes.retain(|pane| pane.id != pane_id);
        if before == self.panes.len() {
            return Err(format!("pane not found: {pane_id}"));
        }

        if self.active_pane_id.as_deref() == Some(pane_id) {
            self.active_pane_id = self.panes.first().map(|pane| pane.id.clone());
        }

        Ok(self.snapshot())
    }

    pub fn set_active(&mut self, pane_id: &str) -> Result<(), String> {
        if !self.contains_pane(pane_id) {
            return Err(format!("pane not found: {pane_id}"));
        }
        self.active_pane_id = Some(pane_id.to_string());
        Ok(())
    }

    /// Remove a pane WITHOUT `close_pane`'s minimum-count guard: used only to
    /// roll back a just-created pane whose spawn/persist failed before it was
    /// announced (the registry is guaranteed to have held another pane already).
    fn remove_pane(&mut self, pane_id: &str) {
        self.panes.retain(|pane| pane.id != pane_id);
        if self.active_pane_id.as_deref() == Some(pane_id) {
            self.active_pane_id = self.panes.first().map(|pane| pane.id.clone());
        }
    }

    pub fn rename_pane(&mut self, pane_id: &str, title: String) -> Result<Pane, String> {
        let clean =
            clean_title(Some(title)).ok_or_else(|| "pane title cannot be blank".to_string())?;
        let pane = self
            .panes
            .iter_mut()
            .find(|pane| pane.id == pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;
        pane.title = clean;
        Ok(pane.clone())
    }

    pub fn snapshot(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            panes: self.panes.clone(),
            active_pane_id: self.active_pane_id.clone(),
            cwd: self.cwd.clone(),
            layout: None,
            scrollback: HashMap::new(),
            sizes: HashMap::new(),
            pane_states: HashMap::new(),
            agent_states: HashMap::new(),
            agent_events: HashMap::new(),
            agent_specs: HashMap::new(),
            leases: HashMap::new(),
            projects: HashMap::new(),
            output_warnings: HashMap::new(),
            agent_usage: HashMap::new(),
        }
    }

    pub fn contains_pane(&self, pane_id: &str) -> bool {
        self.panes.iter().any(|pane| pane.id == pane_id)
    }

    /// (T2) A pane's kind, used to route session management (PTY shell vs
    /// headless agent process) and ctl commands. `None` for an unknown pane.
    pub fn pane_kind(&self, pane_id: &str) -> Option<PaneKind> {
        self.panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .map(|pane| pane.kind)
    }
}

/// Every descendant of `root` in one `ps` snapshot (root excluded).
#[cfg(unix)]
fn process_descendants(root: u32, table: &ProcessTable) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, ppid) in &table.parent {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut found = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if let Some(kids) = children.get(&pid) {
            for kid in kids {
                if *kid != root && !found.contains(kid) {
                    found.push(*kid);
                    queue.push(*kid);
                }
            }
        }
    }
    found
}

/// Terminate everything under a pane's child, not only the shell: an
/// interactive shell puts each job in its own process group, so killing the
/// shell alone orphans an agent started from it. Descendants get SIGTERM now
/// and SIGKILL after a grace period if still alive. Best-effort; pid reuse
/// inside the grace window is the accepted hazard.
/// Child → parent for every process, read from the kernel without forking:
/// libproc on macOS, /proc on Linux. Forking here would be wrong twice over:
/// a pane close would pay a `ps` per session, and a forked child briefly
/// holds duplicates of every fd, which keeps an advisory `flock` alive past
/// its owner's drop (the daemon lock probe races that window).
#[cfg(target_os = "macos")]
fn process_parent_snapshot() -> Option<HashMap<u32, u32>> {
    // SAFETY: proc_listallpids sizes its answer to the buffer we pass, and
    // proc_pidinfo writes at most `size_of::<proc_bsdinfo>()` bytes into a
    // zeroed struct we own; every pointer is valid for the call's duration.
    unsafe {
        let needed = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut pids = vec![0 as libc::pid_t; needed as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let count = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        if count <= 0 {
            return None;
        }
        pids.truncate(count as usize);
        let mut parents = HashMap::with_capacity(pids.len());
        for pid in pids {
            if pid <= 0 {
                continue;
            }
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let got = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            );
            if got == size {
                parents.insert(pid as u32, info.pbi_ppid);
            }
        }
        Some(parents)
    }
}

#[cfg(target_os = "linux")]
fn process_parent_snapshot() -> Option<HashMap<u32, u32>> {
    let mut parents = HashMap::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid …` — comm may contain spaces or parens, so
        // split after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next(); // state
        if let Some(ppid) = fields.next().and_then(|field| field.parse().ok()) {
            parents.insert(pid, ppid);
        }
    }
    Some(parents)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn process_parent_snapshot() -> Option<HashMap<u32, u32>> {
    None
}

/// The process tree for hook placement: the fork-free kernel snapshot on
/// macOS and Linux; nothing elsewhere (a hook there reports `mapped: false`).
#[cfg(unix)]
fn process_parent_snapshot_for_hooks() -> Option<HashMap<u32, u32>> {
    process_parent_snapshot()
}

#[cfg(not(unix))]
fn process_parent_snapshot_for_hooks() -> Option<HashMap<u32, u32>> {
    None
}

#[cfg(unix)]
fn terminate_process_tree(root: u32) {
    let parent = match process_parent_snapshot() {
        Some(parent) => parent,
        None => {
            // Last resort on other Unixes: a `ps` snapshot (forks once).
            let Ok(output) = Command::new("ps")
                .args(["-axo", "pid=,ppid="])
                .stdin(Stdio::null())
                .output()
            else {
                return;
            };
            parse_process_table(&String::from_utf8_lossy(&output.stdout)).parent
        }
    };
    let table = ProcessTable {
        parent,
        args: HashMap::new(),
    };
    let targets = process_descendants(root, &table);
    if targets.is_empty() {
        return;
    }
    for pid in &targets {
        // SAFETY: kill(2) with a pid we just read from the process table; a
        // stale pid is an ESRCH we ignore.
        unsafe {
            libc::kill(*pid as libc::pid_t, libc::SIGTERM);
        }
    }
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        for pid in targets {
            // SAFETY: as above; signal 0 only probes existence.
            unsafe {
                if libc::kill(pid as libc::pid_t, 0) == 0 {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    });
}

/// A kill-on-close Job Object holding the pane's child so closing the pane
/// (or the daemon exiting) terminates the whole tree, ConPTY included.
#[cfg(windows)]
struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl KillOnCloseJob {
    fn attach(pid: u32) -> Option<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        // SAFETY: plain Win32 calls with valid arguments; every handle we
        // open is closed on every path below.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() || job == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                CloseHandle(job);
                return None;
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                CloseHandle(job);
                return None;
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Self(job))
        }
    }
}

// SAFETY: a job object handle is a kernel object reference with no thread
// affinity; it is only ever used to close the job, from whichever thread
// drops the owning session.
#[cfg(windows)]
unsafe impl Send for KillOnCloseJob {}
#[cfg(windows)]
unsafe impl Sync for KillOnCloseJob {}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: the handle was created by CreateJobObjectW and is closed once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

struct TerminalSession {
    _master: Box<dyn MasterPty + Send>,
    /// The child's pid, for terminating its descendants on close (Unix).
    pid: Option<u32>,
    /// Kill-on-close job holding the child tree (Windows). Dropped last.
    #[cfg(windows)]
    _job: Option<KillOnCloseJob>,
    /// A killer split off the child via `clone_killer()`. The `child` itself is
    /// owned by the reader thread (which reaps it via `child.wait()`), so the
    /// session keeps only this handle to terminate the process on close/restart.
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// Bounded queue into the pane's dedicated writer thread, which owns the PTY
    /// writer and performs every (potentially blocking) write. Requests only
    /// try_send here — never a blocking PTY write under the TerminalStore mutex —
    /// so a pane whose foreground process stopped reading stdin (Ctrl-S, stopped
    /// job) can no longer wedge the whole daemon (H2).
    input: SyncSender<Vec<u8>>,
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            terminate_process_tree(pid);
        }
        let _ = self.killer.kill();
    }
}

/// Cap on queued-but-unwritten input chunks per pane. The queue absorbs normal
/// typing/paste bursts; once a non-draining pane fills it, writes fail fast with
/// a "backlogged" error instead of blocking a request thread forever.
const PANE_INPUT_QUEUE_LIMIT: usize = 256;

/// Spawn the dedicated writer thread that drains a pane's input queue into its
/// PTY. The thread exits when the session drops (sender dropped → recv errs) or
/// when a write fails (PTY gone); after that, queued sends error `Disconnected`.
fn spawn_input_writer(mut writer: Box<dyn Write + Send>) -> SyncSender<Vec<u8>> {
    let (sender, receiver) = sync_channel::<Vec<u8>>(PANE_INPUT_QUEUE_LIMIT);
    thread::spawn(move || {
        while let Ok(chunk) = receiver.recv() {
            if writer.write_all(&chunk).is_err() || writer.flush().is_err() {
                break;
            }
        }
    });
    sender
}

/// Queue input for a pane's writer thread, failing fast when the pane has
/// stopped draining (queue full) or its writer thread has exited.
fn queue_pane_input(input: &SyncSender<Vec<u8>>, pane_id: &str, data: &str) -> Result<(), String> {
    match input.try_send(data.as_bytes().to_vec()) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(format!(
            "terminal input backlogged (pane is not reading stdin): {pane_id}"
        )),
        Err(TrySendError::Disconnected(_)) => Err(format!("terminal session ended: {pane_id}")),
    }
}

/// Tracks the currently-active reader generation for a pane so that a stale reader
/// thread from a previous (closed/restarted) session cannot mark the live session
/// as ended or emit a spurious PaneEnded event.
///
/// It also carries the pane's spawn metadata (the launched command + working dir,
/// set at spawn) and the reaped `exit_code` (set once by the current generation's
/// reader when the process is reaped). Co-locating these in the generation-tagged
/// entry makes them generation-safe for free: an in-place restart replaces the
/// entry, so the new generation starts with `exit_code: None` (the prior code is
/// cleared) and the metadata stays queryable for an ended pane until it is closed.
struct PaneLiveness {
    generation: u64,
    ended: bool,
    command: Option<String>,
    cwd: Option<String>,
    exit_code: Option<i32>,
    /// The shell pane's child pid, so the official agent probe can map a
    /// `claude agents --json` session to its pane through the process tree
    /// (M3b). `None` for agent panes and restored-not-yet-spawned panes.
    pid: Option<u32>,
}

/// Per-pane spawn/exit metadata surfaced to snapshot/find (and the reaper's
/// `PaneEnded`). Read out of the generation-tagged liveness entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PaneMeta {
    command: Option<String>,
    cwd: Option<String>,
    exit_code: Option<i32>,
}

/// Derive the reported exit code from a reaped child's `ExitStatus`.
///
/// portable-pty 0.8's `ExitStatus` forces the numeric code to 1 for a signal death
/// and exposes the signal only through its `Display` ("Terminated by <name>") — it
/// has no public signal accessor — so a signal-terminated child is reported with a
/// `None` (documented) exit code rather than a misleading `1` that would collide
/// with a genuine `exit 1`. A normal exit yields its real code (0, 7, 42, …).
fn reaped_exit_code(status: &ExitStatus) -> Option<i32> {
    if status.success() {
        return Some(0);
    }
    if status.to_string().starts_with("Terminated by") {
        None
    } else {
        Some(status.exit_code() as i32)
    }
}

/// Sink for OSC window-title sequences. vt100 0.16 reports the title via the
/// `Callbacks` trait (there is no `Screen::title()` getter), so the model owns this
/// sink and reads the latest title from it. OSC 0 and OSC 2 both invoke
/// `set_window_title`; OSC 1 (icon name) is intentionally ignored so the captured
/// title reflects the program's window title only.
#[derive(Default)]
struct TitleSink {
    title: Option<String>,
}

impl vt100::Callbacks for TitleSink {
    fn set_window_title(&mut self, _screen: &mut vt100::Screen, title: &[u8]) {
        // Cap the STORED title (M6): an OSC title is attacker-length (pane-resident
        // code), and the stored string is cloned on every snapshot/find. The cap
        // matches rename titles (MAX_TITLE_CHARS), and control characters are
        // stripped for the same escape-injection reason as clean_title (L11).
        // Note the residual: vte's `std` build accumulates a never-terminated OSC
        // sequence in its own parser buffer before this callback ever fires —
        // that in-parser growth is upstream (the 1024-byte osc_raw guard is
        // compiled out under `std`) and only bounded by what the local pane emits.
        let title = String::from_utf8_lossy(title);
        self.title = Some(
            title
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_TITLE_CHARS)
                .collect(),
        );
    }
}

/// Per-pane in-daemon screen model. A `vt100::Parser` is fed the SAME raw PTY bytes
/// the reader thread drains for `emit`, so the daemon can answer snapshot/wait/find
/// against a rendered grid instead of raw ANSI (architecture §6.1, Invariant 9).
/// `revision` advances on every processed chunk and is monotonic for the life of a
/// pane id: it never resets on resize and is preserved across an in-place restart.
struct PaneModel {
    parser: vt100::Parser<TitleSink>,
    revision: u64,
}

impl PaneModel {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, 0, TitleSink::default()),
            revision: 0,
        }
    }

    /// Feed raw PTY bytes into the parser and bump the revision once. vt100 is robust
    /// to binary, invalid UTF-8, NUL, and partial escape sequences, so this never
    /// panics on pathological output.
    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.revision = self.revision.saturating_add(1);
    }

    /// Resize the grid to `cols`x`rows`. The revision is deliberately NOT bumped
    /// (resize is not output) and never reset, so it stays monotonic across resize.
    fn set_size(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Reset to a fresh screen (used when a pane is restarted in place). The
    /// monotonic revision counter is BUMPED, not reset: the wipe itself is a state
    /// change, and without the bump revision N would map to two different screens
    /// across a restart — a revision-deduping client would never repaint (L10).
    fn reset(&mut self, cols: u16, rows: u16) {
        self.parser = vt100::Parser::new_with_callbacks(rows, cols, 0, TitleSink::default());
        self.revision = self.revision.saturating_add(1);
    }
}

// ----- (T1) agent detection + attention classification -----

/// (T1) Minimum interval between full agent classifications of one pane. The
/// reader thread feeds the screen model per ≤8 KiB output chunk; reclassifying
/// on every chunk would render the grid (a multi-KiB allocation) far more often
/// than any client can usefully consume. Transitions coalesce inside the
/// window — the NEXT classification reports the latest state. The throttle has
/// a TRAILING EDGE (H1): a chunk skipped by the window schedules one deferred
/// classification at window expiry, so the final frame of a burst (typically
/// the permission prompt, after which the agent blocks on stdin and no further
/// chunk ever arrives) is still classified.
const AGENT_CLASSIFY_INTERVAL: Duration = Duration::from_millis(500);

/// (T1) L8: manual agent names are capped and shell-safe (they round-trip
/// through workspace.json and CLI output).
const AGENT_NAME_MAX_LEN: usize = 32;

/// (T1) Strong Claude Code signature markers. Each is one independent marker
/// GROUP; a fourth group is a "❯" prompt combined with box-drawing chrome
/// (checked separately, since it needs both halves). Case-sensitive.
const AGENT_MARKERS: [&str; 3] = ["esc to interrupt", "⏵⏵", "Claude Code"];

/// (T1) Box-drawing chrome the Claude Code TUI draws around its input box.
const AGENT_CHROME_CHARS: [char; 10] = ['─', '│', '╭', '╮', '╰', '╯', '┌', '┐', '└', '┘'];

/// (T1) Permission/confirmation prompts: the agent is blocked waiting on the
/// user. `"1. Yes"` requires `"2. No"` alongside (a bare numbered "Yes" line is
/// common prose; a numbered yes/no pair is distinctive).
const AGENT_NEEDS_INPUT: [&str; 3] = [
    "Do you want to proceed?",
    "Waiting for your response",
    "Press enter to continue",
];

/// (T1) The agent is actively working. `"esc to interrupt"` doubles as a
/// signature marker — Claude Code shows it in the footer while a turn runs.
const AGENT_WORKING: [&str; 3] = ["esc to interrupt", "Thinking", "Working"];

/// (T1) Braille spinner frames the Claude Code TUI animates while working.
const AGENT_SPINNER_CHARS: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// (T1) `needle` occurs in `haystack` (byte-level; no UTF-8 validation and —
/// unlike a `from_utf8_lossy` scan — no allocation on the hot path).
fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// (T1) Cheap raw-output pre-check: could this chunk carry a strong signature
/// marker? A pane with no manual mark and no detected agent is fully classified
/// only when this passes, so plain shell output never pays for the screen-text
/// render. The loose ❯-prompt group is deliberately NOT a candidate: a bare ❯
/// (common zsh prompt) must not trigger renders, and it can never reach the
/// 2-group detection threshold without one of the strong markers anyway.
fn agent_signature_candidate(bytes: &[u8]) -> bool {
    AGENT_MARKERS
        .iter()
        .any(|marker| bytes_contain(bytes, marker.as_bytes()))
}

/// (T1) Count the independent Claude Code signature groups on screen:
/// "esc to interrupt" (working footer), "⏵⏵" (auto-accept/bypass indicator),
/// "Claude Code" (welcome/banner), and a "❯" prompt drawn with box-drawing
/// chrome (the input box). Distinct groups, not occurrences.
fn agent_signature_groups(text: &str) -> usize {
    let mut groups = AGENT_MARKERS
        .iter()
        .filter(|marker| text.contains(**marker))
        .count();
    if text.contains('❯')
        && AGENT_CHROME_CHARS
            .iter()
            .any(|chrome| text.contains(*chrome))
    {
        groups += 1;
    }
    groups
}

/// (T1) Auto-detect the agent from the rendered screen. Fresh detection
/// requires 2+ independent marker groups (conservative: no single marker is
/// proof); a pane already detected keeps its mark while 1+ group remains —
/// hysteresis so transient repaint frames (a full-screen redraw passes through
/// a nearly-empty grid) don't flap the state off and back on.
fn detect_agent(text: &str, already_detected: bool) -> Option<String> {
    let threshold = if already_detected { 1 } else { 2 };
    (agent_signature_groups(text) >= threshold).then(|| "claude".to_string())
}

/// (T1) Classify an agent pane's attention state from its rendered screen.
/// Priority: NeedsInput > Working > Idle — a permission prompt must win over a
/// stale "esc to interrupt" still visible above it. Case-sensitive substrings,
/// deliberately conservative to avoid false positives.
fn classify_agent_attention(text: &str) -> AgentAttention {
    if AGENT_NEEDS_INPUT
        .iter()
        .any(|pattern| text.contains(pattern))
        || (text.contains("1. Yes") && text.contains("2. No"))
    {
        return AgentAttention::NeedsInput;
    }
    if AGENT_WORKING.iter().any(|pattern| text.contains(pattern))
        || AGENT_SPINNER_CHARS.iter().any(|c| text.contains(*c))
    {
        return AgentAttention::Working;
    }
    AgentAttention::Idle
}

/// (T1) The pane's visible screen as newline-joined rows — the same
/// rows-concatenation the wait/snapshot path renders from the vt100 model.
fn model_screen_text(model: &PaneModel) -> String {
    let screen = model.parser.screen();
    let cols = screen.size().1;
    screen.rows(0, cols).collect::<Vec<String>>().join("\n")
}

/// (T1) Per-pane agent state tracked by the daemon: the current agent (manual
/// mark or detected), whether the mark is manual (manual overrides detection
/// and persists), the last classified attention state, and classification
/// throttle bookkeeping.
#[derive(Debug, Clone, Default)]
struct AgentPaneState {
    agent: Option<String>,
    manual: bool,
    attention: Option<AgentAttention>,
    last_classified_revision: u64,
    last_classified_at: Option<Instant>,
    /// (T1) H1 trailing edge: one deferred classification is already
    /// scheduled to run at the throttle window's expiry.
    trailing_scheduled: bool,
    /// (T1) M4: consecutive signature-free classifications of a DETECTED
    /// pane; the mark clears on the second (torn-redraw flap guard).
    zero_signature_streak: u8,
    /// (T1) The pane's process has ended (or was killed for a restart): no
    /// further automatic classification until the next spawn — a dead agent
    /// must not be re-classified back to a working/needs-input badge from
    /// its preserved final screen (M2).
    ended: bool,
    /// (M3b) Until when an official `claude agents --json` reading outranks
    /// the screen heuristic for this pane. `None` = never had one.
    official_until: Option<Instant>,
    /// The permission mode read off the screen (see `classify_agent_mode`).
    mode: Option<String>,
    /// The attention state the agent had when its process ended (taken by
    /// `clear_agent_attention`), so `pane.ended` can say whether the agent
    /// was still waiting on a person. Reset on the next spawn.
    last_attention: Option<AgentAttention>,
}

impl AgentPaneState {
    /// The wire-facing view of this entry.
    fn info(&self) -> AgentPaneInfo {
        AgentPaneInfo {
            agent: self.agent.clone(),
            attention: self.attention,
            mode: self.mode.clone(),
            unattended: is_unattended_mode(self.mode.as_deref()),
        }
    }
}

/// (T1) Per-pane agent tracking, held by the `OutputRouter` alongside the
/// screen models. Entries are created on first classification and removed when
/// the pane closes; bounded by the pane count.
#[derive(Debug, Default)]
struct AgentTracker {
    panes: HashMap<String, AgentPaneState>,
}

/// A subscriber is fed through a bounded channel drained by its own writer thread, so
/// broadcast never performs socket I/O while holding the subscriber lock (one slow/hung
/// consumer can't stall other panes), while per-subscriber ordering is preserved.
struct Subscriber {
    id: u64,
    sender: SyncSender<Arc<Vec<u8>>>,
    /// Negotiated wire version of this subscriber's connection: events are framed
    /// (v2 envelope) for `>= frame::WIRE_VERSION`, newline-JSON otherwise. This is
    /// what makes the Subscribe event stream framed iff the connection negotiated v2.
    wire_version: u16,
}

/// Encode an already-serialized event `payload_json` for a subscriber on
/// `wire_version`: a framed v2 envelope for `>= frame::WIRE_VERSION`, else
/// newline-JSON (the legacy v1 stream). Returns `None` only when the payload is too
/// large to frame (the event is then skipped for that subscriber rather than
/// corrupting its stream); the newline path is always `Some`.
fn encode_event_for_wire(wire_version: u16, payload_json: &[u8]) -> Option<Vec<u8>> {
    if wire_version >= frame::WIRE_VERSION {
        frame::encode_payload(payload_json).ok()
    } else {
        let mut bytes = Vec::with_capacity(payload_json.len() + 1);
        bytes.extend_from_slice(payload_json);
        bytes.push(b'\n');
        Some(bytes)
    }
}

#[derive(Clone)]
struct OutputRouter {
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    scrollback_dir: PathBuf,
    /// Panes closed via ClosePane, with the closure time: a still-draining reader
    /// must not recreate their scrollback or deliver further output. Entries are
    /// pruned by `sweep_closed` once the reader window is long past, so the set
    /// stays bounded for the daemon's lifetime (L13).
    closed: Arc<Mutex<HashMap<String, Instant>>>,
    next_subscriber_id: Arc<AtomicU64>,
    /// Tracing dispatch for structured logging from reader/watcher threads. Set
    /// once by `DaemonServer::with_config`; `None` in unit tests (tracing macros
    /// are no-ops without a subscriber).
    log_dispatch: Arc<std::sync::OnceLock<tracing::dispatcher::Dispatch>>,
    /// Workspace key included in structured log fields for events emitted from
    /// reader/watcher threads (pane-end, client-disconnect).
    log_workspace_key: Arc<std::sync::OnceLock<String>>,
    /// Per-pane vt100 screen models, each behind its own `Mutex` so feeding one
    /// pane never blocks another and a reader holds a parser lock only for the
    /// duration of a single `process` call, never across a PTY read (Invariant 9).
    models: Arc<Mutex<HashMap<String, Arc<Mutex<PaneModel>>>>>,
    /// Per-pane scrollback append state (M11): a cached open File handle + the
    /// tracked byte count, so the reader hot path no longer pays
    /// open+write+close+stat per ≤8 KiB chunk. The map lock is held only to
    /// clone the per-pane Arc; all I/O happens under the per-pane state lock,
    /// which also serializes appends with a cap rewrite of the same file.
    /// Entries are invalidated on pane close and re-primed (one stat) after a
    /// cap replaces the file.
    append_handles: Arc<Mutex<HashMap<String, Arc<Mutex<ScrollbackAppendState>>>>>,
    /// (T1) Per-pane agent detection/attention state. Leaf lock: it is only
    /// ever taken briefly, and no other lock is acquired while holding it —
    /// except that the per-pane MODEL lock may already be held by the caller
    /// (lock order: model → agents, never reversed).
    agents: Arc<Mutex<AgentTracker>>,
    /// The workspace ledger (docs/design/keyboard-lease-and-ledger.md), set
    /// once by `DaemonServer::with_config`; `None` in unit tests that build a
    /// bare router. Attention transitions and pane ends are noted here.
    ledger: Arc<std::sync::OnceLock<Arc<Mutex<LedgerSink>>>>,
    /// Per-pane output-guard counters (see `scan_output_tricks`). Leaf lock.
    output_guard: Arc<Mutex<HashMap<String, OutputGuardState>>>,
    /// The daemon's lease table, shared so a `pane.ended` record can name the
    /// keyboard holder at exit. Read only here; set once by
    /// `DaemonServer::with_config`; `None` in unit tests. Leaf lock.
    leases: Arc<std::sync::OnceLock<SharedLeases>>,
}

/// The daemon's lease table as shared with the output router.
type SharedLeases = Arc<Mutex<HashMap<String, HeldLease>>>;

/// Cached append state for one pane's scrollback file (M11): the open handle
/// and the byte count as tracked by appends (a `stat` happens only when the
/// count is unknown — first open, or right after a cap replaced the file).
struct ScrollbackAppendState {
    file: File,
    len: u64,
}

impl OutputRouter {
    fn new(scrollback_dir: PathBuf) -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
            scrollback_dir,
            closed: Arc::new(Mutex::new(HashMap::new())),
            next_subscriber_id: Arc::new(AtomicU64::new(1)),
            log_dispatch: Arc::new(std::sync::OnceLock::new()),
            log_workspace_key: Arc::new(std::sync::OnceLock::new()),
            models: Arc::new(Mutex::new(HashMap::new())),
            append_handles: Arc::new(Mutex::new(HashMap::new())),
            agents: Arc::new(Mutex::new(AgentTracker::default())),
            ledger: Arc::new(std::sync::OnceLock::new()),
            leases: Arc::new(std::sync::OnceLock::new()),
            output_guard: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Count hiding tricks in one output chunk; announce (ledger + event) on
    /// the first hit and then at most every `OUTPUT_WARNING_ANNOUNCE_INTERVAL`.
    fn record_output_tricks(&self, pane_id: &str, data: &str) {
        // Cheap pre-check: nothing to find without an ESC or a non-ASCII byte.
        if !data.bytes().any(|byte| byte == 0x1b || byte >= 0x80) {
            return;
        }
        let found = scan_output_tricks(data);
        if found.total() == 0 {
            return;
        }
        let announce = {
            let Ok(mut guard) = self.output_guard.lock() else {
                return;
            };
            let entry = guard.entry(pane_id.to_string()).or_default();
            entry.total.add(&found);
            let due = entry
                .last_announced
                .is_none_or(|at| at.elapsed() >= OUTPUT_WARNING_ANNOUNCE_INTERVAL);
            if due {
                entry.last_announced = Some(Instant::now());
                let added = entry.total.minus(&entry.announced);
                entry.announced = entry.total;
                Some((added, entry.total))
            } else {
                None
            }
        };
        if let Some((added, total)) = announce {
            self.ledger_note(
                pane_id,
                "output.suspicious",
                json!({ "added": added, "total": total, "evidence": "scan" }),
            );
            self.broadcast(&DaemonEvent::OutputWarning {
                pane_id: pane_id.to_string(),
                added,
                total,
            });
        }
    }

    fn output_tricks(&self, pane_id: &str) -> OutputTricks {
        self.output_guard
            .lock()
            .ok()
            .and_then(|guard| guard.get(pane_id).map(|entry| entry.total))
            .unwrap_or_default()
    }

    /// Every pane with at least one counted trick.
    fn output_warnings(&self) -> HashMap<String, OutputTricks> {
        self.output_guard
            .lock()
            .map(|guard| {
                guard
                    .iter()
                    .filter(|(_, entry)| entry.total.total() > 0)
                    .map(|(pane_id, entry)| (pane_id.clone(), entry.total))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn remove_output_guard(&self, pane_id: &str) {
        if let Ok(mut guard) = self.output_guard.lock() {
            guard.remove(pane_id);
        }
    }

    fn set_ledger(&self, sink: Arc<Mutex<LedgerSink>>) {
        let _ = self.ledger.set(sink);
    }

    /// A session proved an agent is running in the pane (a status-line
    /// payload arrived from under it). Sets the agent name when the pane has
    /// none, leaving attention and manual marks alone; broadcasts only when
    /// something changed.
    fn mark_agent_present(&self, pane_id: &str, agent: &str) {
        let changed = self.agents.lock().ok().and_then(|mut tracker| {
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            if entry.ended || entry.agent.is_some() {
                return None;
            }
            entry.agent = Some(agent.to_string());
            Some((entry.attention, entry.mode.clone()))
        });
        if let Some((attention, mode)) = changed {
            self.broadcast(&DaemonEvent::AgentState {
                pane_id: pane_id.to_string(),
                agent: Some(agent.to_string()),
                attention,
                mode,
            });
        }
    }

    fn set_leases(&self, leases: SharedLeases) {
        let _ = self.leases.set(leases);
    }

    /// The keyboard holder of a pane right now, if the lease table is wired.
    fn lease_holder(&self, pane_id: &str) -> Option<String> {
        self.leases
            .get()
            .and_then(|table| table.lock().ok())
            .and_then(|table| table.get(pane_id).map(|held| held.holder.clone()))
    }

    /// Best-effort, non-durable ledger note from the output path. Called with
    /// no other lock held (the ledger is a leaf lock).
    fn ledger_note(&self, pane_id: &str, kind: &str, payload: Value) {
        if let Some(sink) = self.ledger.get() {
            if let Ok(mut sink) = sink.lock() {
                let _ = sink.record(pane_id, kind, payload, false);
            }
        }
    }

    /// Set the tracing dispatch and workspace key so reader/watcher threads can
    /// emit structured log entries. Called once from `DaemonServer::with_config`.
    fn set_log_context(&self, dispatch: tracing::dispatcher::Dispatch, workspace_key: String) {
        let _ = self.log_dispatch.set(dispatch);
        let _ = self.log_workspace_key.set(workspace_key);
    }

    /// Returns a `set_default` guard that activates the tracing subscriber for the
    /// current thread, or `None` if no subscriber was configured (unit tests).
    fn log_guard(&self) -> Option<tracing::dispatcher::DefaultGuard> {
        self.log_dispatch
            .get()
            .map(tracing::dispatcher::set_default)
    }

    fn log_workspace_key(&self) -> &str {
        self.log_workspace_key
            .get()
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// Live subscriber count, derived from the subscriber list itself. Disconnect
    /// watchers remove entries promptly, so this stays accurate even while idle —
    /// which the idle-shutdown logic depends on.
    fn subscriber_count(&self) -> usize {
        self.subscribers
            .lock()
            .map(|subscribers| subscribers.len())
            .unwrap_or(0)
    }

    fn remove_subscriber(&self, id: u64) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.id != id);
        }
    }

    /// Register a subscriber, spawning its writer + disconnect-watcher threads.
    /// Returns the subscriber id, or — over MAX_SUBSCRIBERS — the reason AND
    /// the stream handed back so the caller can still write a clean error
    /// response on it (M5: each subscriber costs two threads and its connection
    /// slot is released at hand-off, so uncapped subscription would exhaust
    /// threads/fds and permanently suppress idle shutdown). The cap check and
    /// the insertion happen under one lock so concurrent subscribes can't both
    /// pass; on rejection no entry, channel, or thread is created and the
    /// handed-back stream is dropped by the caller — no connection leak.
    fn add_subscriber(
        &self,
        mut stream: TransportStream,
        wire_version: u16,
    ) -> Result<u64, (String, TransportStream)> {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = sync_channel::<Arc<Vec<u8>>>(SUBSCRIBER_QUEUE_LIMIT);
        {
            let Ok(mut subscribers) = self.subscribers.lock() else {
                return Err(("subscriber lock poisoned".to_string(), stream));
            };
            if subscribers.len() >= MAX_SUBSCRIBERS {
                return Err((
                    format!("subscriber limit reached ({MAX_SUBSCRIBERS}); try again after a client disconnects"),
                    stream,
                ));
            }
            subscribers.push(Subscriber {
                id,
                sender,
                wire_version,
            });
        }

        // A write timeout bounds the per-subscriber writer thread's lifetime if the
        // peer's socket buffer stays full (so the thread can't leak forever).
        let _ = stream.set_write_timeout(Some(SUBSCRIBER_WRITE_TIMEOUT));

        // A cloned handle drives a watcher thread that detects the peer disconnecting
        // (read returns 0/err) and removes the entry; removal drops the sender, which
        // in turn ends the writer thread instead of leaving it parked on recv().
        // If the clone fails there is no watcher and the entry is pruned on the next
        // broadcast whose try_send fails.
        if let Ok(mut watch_stream) = stream.try_clone() {
            let router = self.clone();
            thread::spawn(move || {
                let _log_guard = router.log_guard();
                let _ = watch_stream.set_read_timeout(None);
                let mut buf = [0_u8; 256];
                loop {
                    match watch_stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                tracing::info!(
                    workspace_key = %router.log_workspace_key(),
                    event = "client_disconnect",
                    "client disconnected"
                );
                router.remove_subscriber(id);
            });
        }

        let router = self.clone();
        thread::spawn(move || {
            while let Ok(payload) = receiver.recv() {
                if stream.write_all(&payload).is_err() || stream.flush().is_err() {
                    break;
                }
            }
            // Shut the socket down so the watcher's blocking read returns even when
            // the peer is wedged (connected but neither reading nor closing), then
            // drop the entry so the count reflects reality.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            router.remove_subscriber(id);
        });

        Ok(id)
    }

    /// Send a single event to one subscriber (by id) without broadcasting to all.
    /// Used by subscribe catch-up to push already-ended pane state to the new
    /// subscriber only, so existing subscribers don't receive duplicate events.
    ///
    /// Unlike `broadcast` (which uses non-blocking `try_send` and drops slow
    /// subscribers), this method is **reliable**: it retries with a bounded total
    /// timeout (`CATCHUP_SEND_TIMEOUT`) so a freshly attached subscriber is
    /// guaranteed to receive every catch-up pane state even when ended panes
    /// exceed the queue limit. If the subscriber's queue stays full for the entire
    /// timeout (a stalled consumer), the subscriber is removed so it never
    /// receives a PARTIAL catch-up that omits panes silently — either every
    /// catch-up event is delivered, or the subscriber is disconnected entirely.
    fn send_to_subscriber(&self, id: u64, event: &DaemonEvent) {
        let Ok(payload_json) = serde_json::to_vec(event) else {
            return;
        };
        // Find the sender + negotiated wire version under the lock, clone the sender,
        // then release the lock before the retry loop so broadcast/add_subscriber
        // aren't blocked while we wait for queue space. The wire version selects the
        // per-subscriber encoding (framed v2 vs newline v1).
        let target = {
            let Ok(subscribers) = self.subscribers.lock() else {
                return;
            };
            subscribers
                .iter()
                .find(|s| s.id == id)
                .map(|s| (s.sender.clone(), s.wire_version))
        };
        let Some((sender, wire_version)) = target else {
            return;
        };
        let Some(payload) = encode_event_for_wire(wire_version, &payload_json) else {
            return;
        };
        let payload = Arc::new(payload);
        let deadline = Instant::now() + CATCHUP_SEND_TIMEOUT;
        loop {
            match sender.try_send(Arc::clone(&payload)) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    if Instant::now() >= deadline {
                        // The subscriber's queue stayed full for the entire timeout:
                        // it's stalled. Remove it so it never receives a partial
                        // catch-up that omits panes silently.
                        self.remove_subscriber(id);
                        return;
                    }
                    // If a concurrent broadcast already removed this subscriber
                    // (it was too slow), abort the catch-up rather than delivering
                    // to a dropped subscriber.
                    let still_present = self
                        .subscribers
                        .lock()
                        .map(|subs| subs.iter().any(|s| s.id == id))
                        .unwrap_or(false);
                    if !still_present {
                        return;
                    }
                    thread::sleep(CATCHUP_SEND_RETRY_INTERVAL);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // The writer thread exited (subscriber gone). Remove the entry.
                    self.remove_subscriber(id);
                    return;
                }
            }
        }
    }

    fn mark_closed(&self, pane_id: &str) {
        if let Ok(mut closed) = self.closed.lock() {
            closed.insert(pane_id.to_string(), Instant::now());
        }
    }

    fn is_closed(&self, pane_id: &str) -> bool {
        self.closed
            .lock()
            .map(|closed| closed.contains_key(pane_id))
            .unwrap_or(false)
    }

    /// Prune closed-pane suppression entries older than `max_age`. An entry is
    /// only needed while the closed pane's reader thread might still be draining
    /// (its child is killed at close, so that window is seconds); pane ids are
    /// never reused (monotonic next_id), so pruning is safe and keeps the set
    /// from growing for the daemon's lifetime (L13).
    fn sweep_closed(&self, max_age: Duration) {
        if let Ok(mut closed) = self.closed.lock() {
            closed.retain(|_, marked_at| marked_at.elapsed() < max_age);
        }
    }

    /// Clone the per-pane model handle out of the map under a tiny lock scope, so
    /// callers lock the (per-pane) parser mutex without holding the map lock.
    fn model_handle(&self, pane_id: &str) -> Option<Arc<Mutex<PaneModel>>> {
        self.models.lock().ok()?.get(pane_id).cloned()
    }

    /// Create the pane's screen model (at spawn). If one already exists (an in-place
    /// restart), reset it to a fresh screen while preserving the monotonic revision.
    fn ensure_model(&self, pane_id: &str, cols: u16, rows: u16) {
        if let Ok(mut models) = self.models.lock() {
            match models.get(pane_id) {
                Some(existing) => {
                    if let Ok(mut model) = existing.lock() {
                        model.reset(cols, rows);
                    }
                }
                None => {
                    models.insert(
                        pane_id.to_string(),
                        Arc::new(Mutex::new(PaneModel::new(cols, rows))),
                    );
                }
            }
        }
        // (T1) A (re)spawn revives automatic classification for the pane:
        // its previous process's `ended` latch (M2) must not outlive it.
        if let Ok(mut tracker) = self.agents.lock() {
            if let Some(entry) = tracker.panes.get_mut(pane_id) {
                entry.ended = false;
                entry.last_attention = None;
            }
        }
    }

    /// Feed raw PTY bytes to the pane's model. The map lock is held only to clone the
    /// per-pane handle; the parser lock is held only for the `process` call. This
    /// touches no other router lock, so parsing never blocks `emit` (Invariant 9).
    /// (T1) The fresh screen is then reclassified for agent state — throttled,
    /// and skipped cheaply for panes with no agent interest.
    fn feed_model(&self, pane_id: &str, bytes: &[u8]) {
        if let Some(model) = self.model_handle(pane_id) {
            if let Ok(mut model) = model.lock() {
                model.process(bytes);
                self.classify_agent_on_output(pane_id, &model, bytes);
            }
        }
    }

    /// (T1) Reclassify a pane's agent state from its CURRENT screen after
    /// output was processed. The caller holds the pane's model lock; the
    /// tracker lock is taken briefly inside (lock order: model → agents).
    ///
    /// Two cheap gates keep the reader hot path clean: a pane with no manual
    /// mark and no detected agent is classified only when the raw output chunk
    /// carries a strong signature marker (plain shell output never allocates
    /// the screen text), and full classification runs at most once per
    /// AGENT_CLASSIFY_INTERVAL per pane, only when the model revision changed.
    /// A chunk skipped by the throttle schedules a trailing-edge deferred
    /// classification (H1) — see `classify_agent_trailing`.
    fn classify_agent_on_output(&self, pane_id: &str, model: &PaneModel, bytes: &[u8]) {
        // (T1) L6a: a closed pane's still-draining reader must not re-create
        // the tracker entry ClosePane dropped (ghost state).
        if self.is_closed(pane_id) {
            return;
        }
        {
            let Ok(mut tracker) = self.agents.lock() else {
                return;
            };
            let interested = tracker
                .panes
                .get(pane_id)
                .is_some_and(|entry| entry.manual || entry.agent.is_some());
            if !interested && !agent_signature_candidate(bytes) {
                return;
            }
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            if entry.last_classified_revision == model.revision || entry.ended {
                return;
            }
            if entry
                .last_classified_at
                .is_some_and(|at| at.elapsed() < AGENT_CLASSIFY_INTERVAL)
            {
                // (T1) H1 trailing edge: the LAST chunk of a burst is exactly
                // where a permission prompt lands — the agent then blocks on
                // stdin and no further chunk ever triggers classification, so
                // the needs_input transition would be lost. Schedule one
                // deferred classification at window expiry (at most one
                // outstanding per pane); it re-checks the revision itself.
                if entry.trailing_scheduled {
                    return;
                }
                entry.trailing_scheduled = true;
                drop(tracker);
                self.spawn_trailing_classification(pane_id);
                return;
            }
            entry.last_classified_at = Some(Instant::now());
            entry.last_classified_revision = model.revision;
        }
        let text = model_screen_text(model);
        self.apply_agent_classification(pane_id, &text);
    }

    /// (T1) The throttle's trailing edge (H1): spawn the deferred
    /// classification off the reader thread. It runs at window expiry and
    /// takes locks in the same order as the feed path (model → agents), so
    /// it can wait on a busy model lock without ever blocking the reader.
    fn spawn_trailing_classification(&self, pane_id: &str) {
        let router = self.clone();
        let pane_id = pane_id.to_string();
        thread::spawn(move || {
            thread::sleep(AGENT_CLASSIFY_INTERVAL);
            router.classify_agent_trailing(&pane_id);
        });
    }

    /// (T1) The deferred classification itself: re-check the model revision
    /// and classify only if it moved since the last classification (a later
    /// unthrottled classification — or a pane close — makes this a no-op;
    /// an ended/closed pane is never reclassified).
    fn classify_agent_trailing(&self, pane_id: &str) {
        if self.is_closed(pane_id) {
            return;
        }
        let Some(model) = self.model_handle(pane_id) else {
            return;
        };
        let text = {
            let Ok(model) = model.lock() else {
                return;
            };
            let Ok(mut tracker) = self.agents.lock() else {
                return;
            };
            let Some(entry) = tracker.panes.get_mut(pane_id) else {
                return;
            };
            entry.trailing_scheduled = false;
            if entry.last_classified_revision == model.revision || entry.ended {
                return;
            }
            entry.last_classified_at = Some(Instant::now());
            entry.last_classified_revision = model.revision;
            model_screen_text(&model)
        };
        self.apply_agent_classification(pane_id, &text);
    }

    /// (T1) Reclassify a pane NOW, bypassing the signature pre-check and the
    /// throttle: a SetPaneAgent mark/unmark must take effect without waiting
    /// for the next output chunk. A pane with no live model classifies against
    /// an empty screen — a manual mark keeps its agent and defaults to Idle.
    fn classify_agent_now(&self, pane_id: &str) {
        let text = self
            .model_handle(pane_id)
            .and_then(|model| {
                model.lock().ok().map(|model| {
                    if let Ok(mut tracker) = self.agents.lock() {
                        let entry = tracker.panes.entry(pane_id.to_string()).or_default();
                        entry.last_classified_at = Some(Instant::now());
                        entry.last_classified_revision = model.revision;
                    }
                    model_screen_text(&model)
                })
            })
            .unwrap_or_default();
        self.apply_agent_classification(pane_id, &text);
    }

    /// (T1) Recompute a pane's (agent, attention) from its rendered screen text
    /// and broadcast an `AgentState` event on TRANSITIONS only. Manual marks
    /// override detection; an unmarked pane re-derives detection from the
    /// screen, clearing to (None, None) when the signature is gone.
    fn apply_agent_classification(&self, pane_id: &str, text: &str) {
        let Ok(mut tracker) = self.agents.lock() else {
            return;
        };
        let entry = tracker.panes.entry(pane_id.to_string()).or_default();
        // (M3b) A fresh official reading outranks the screen heuristic for
        // attention; the permission mode is only ever on the screen, so it
        // still tracks it.
        if entry
            .official_until
            .is_some_and(|until| until > Instant::now())
        {
            let new_mode = entry
                .agent
                .as_ref()
                .and_then(|_| classify_agent_mode(text).map(str::to_string));
            if entry.mode == new_mode {
                return;
            }
            let previous_mode = std::mem::replace(&mut entry.mode, new_mode.clone());
            let agent = entry.agent.clone();
            let attention = entry.attention;
            drop(tracker);
            self.note_mode_change(pane_id, agent.as_deref(), previous_mode, new_mode.clone());
            self.broadcast(&DaemonEvent::AgentState {
                pane_id: pane_id.to_string(),
                agent,
                attention,
                mode: new_mode,
            });
            return;
        }
        let new_agent = if entry.manual {
            entry.agent.clone()
        } else {
            let groups = agent_signature_groups(text);
            if groups == 0 && entry.agent.is_some() {
                // (T1) M4: a torn full-screen redraw passes through a
                // nearly-empty grid; clearing a detected mark on the FIRST
                // signature-free classification would flap the mark off and
                // back on. Require TWO consecutive zero-group results (one
                // throttle window apart); the first leaves the state
                // untouched.
                entry.zero_signature_streak += 1;
                if entry.zero_signature_streak < 2 {
                    return;
                }
                None
            } else {
                entry.zero_signature_streak = 0;
                detect_agent(text, entry.agent.is_some())
            }
        };
        // (T1) The ended latch (set by clear_agent_attention) blocks automatic
        // classify paths; classify_agent_now / SetPaneAgent must honor it too
        // so a preserved final screen cannot resurrect Working/NeedsInput.
        let new_attention = if entry.ended {
            None
        } else {
            new_agent.as_ref().map(|_| classify_agent_attention(text))
        };
        let new_mode = new_agent
            .as_ref()
            .and_then(|_| classify_agent_mode(text).map(str::to_string));
        if entry.agent == new_agent && entry.attention == new_attention && entry.mode == new_mode {
            return;
        }
        let previous_attention = entry.attention;
        let previous_agent = entry.agent.clone();
        let previous_mode = std::mem::replace(&mut entry.mode, new_mode.clone());
        entry.agent = new_agent.clone();
        entry.attention = new_attention;
        drop(tracker);
        // Every transition is ledgered with its evidence so a wrong guess is
        // auditable (docs/design/keyboard-lease-and-ledger.md §6 M3).
        if previous_agent != new_agent || previous_attention != new_attention {
            self.ledger_note(
                pane_id,
                "attention.changed",
                json!({
                    "agent": new_agent,
                    "from": previous_attention,
                    "to": new_attention,
                    "evidence": "screen",
                }),
            );
        }
        if previous_mode != new_mode {
            self.note_mode_change(
                pane_id,
                new_agent.as_deref(),
                previous_mode,
                new_mode.clone(),
            );
        }
        self.broadcast(&DaemonEvent::AgentState {
            pane_id: pane_id.to_string(),
            agent: new_agent,
            attention: new_attention,
            mode: new_mode,
        });
    }

    /// A permission-mode change is its own ledger record: "the agent went
    /// unattended at 14:02" is exactly the line an audit wants to find.
    fn note_mode_change(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        from: Option<String>,
        to: Option<String>,
    ) {
        let unattended = is_unattended_mode(to.as_deref());
        self.ledger_note(
            pane_id,
            "mode.changed",
            json!({
                "agent": agent,
                "from": from,
                "to": to,
                "unattended": unattended,
                "evidence": "screen",
            }),
        );
    }

    /// (M3b) An official reading from `claude agents --json` for a pane whose
    /// process tree contains that session. It outranks screen classification
    /// until `ttl` elapses without a refresh, then the heuristic resumes.
    /// Transitions are ledgered with evidence `claude-agents`.
    /// Returns true when this is the pane's first official reading (the
    /// caller logs the acquisition once).
    fn apply_official_attention(
        &self,
        pane_id: &str,
        agent: &str,
        attention: AgentAttention,
        ttl: Duration,
    ) -> bool {
        self.apply_official_attention_with(pane_id, agent, attention, ttl, "claude-agents")
    }

    fn apply_official_attention_with(
        &self,
        pane_id: &str,
        agent: &str,
        attention: AgentAttention,
        ttl: Duration,
        evidence: &'static str,
    ) -> bool {
        let Ok(mut tracker) = self.agents.lock() else {
            return false;
        };
        let entry = tracker.panes.entry(pane_id.to_string()).or_default();
        if entry.ended {
            return false;
        }
        // First reading, or the first after a lapse: worth one log line.
        let newly_official = entry
            .official_until
            .is_none_or(|until| until <= Instant::now());
        entry.official_until = Some(Instant::now() + ttl);
        let new_agent = Some(agent.to_string());
        let new_attention = Some(attention);
        if entry.agent == new_agent && entry.attention == new_attention {
            return newly_official;
        }
        let previous_attention = entry.attention;
        let mode = entry.mode.clone();
        entry.agent = new_agent.clone();
        entry.attention = new_attention;
        drop(tracker);
        self.ledger_note(
            pane_id,
            "attention.changed",
            json!({
                "agent": new_agent,
                "from": previous_attention,
                "to": new_attention,
                "evidence": evidence,
            }),
        );
        self.broadcast(&DaemonEvent::AgentState {
            pane_id: pane_id.to_string(),
            agent: new_agent,
            attention: new_attention,
            mode,
        });
        newly_official
    }

    /// (M3b) The official session a pane was mapped to is gone from the
    /// listing (two probes in a row): drop the official reading and, unless
    /// the pane is manually marked, clear its agent badge — the screen
    /// heuristic would otherwise keep a stale "claude · idle" over the
    /// shell prompt that replaced the agent.
    fn clear_official_attention(&self, pane_id: &str) {
        let Ok(mut tracker) = self.agents.lock() else {
            return;
        };
        let Some(entry) = tracker.panes.get_mut(pane_id) else {
            return;
        };
        if entry.official_until.is_none() {
            return;
        }
        entry.official_until = None;
        if entry.manual || (entry.agent.is_none() && entry.attention.is_none()) {
            return;
        }
        let previous_agent = entry.agent.take();
        let previous_attention = entry.attention.take();
        entry.mode = None;
        drop(tracker);
        self.ledger_note(
            pane_id,
            "attention.changed",
            json!({
                "agent": previous_agent,
                "from": previous_attention,
                "to": Value::Null,
                "evidence": "claude-agents: session gone",
            }),
        );
        self.broadcast(&DaemonEvent::AgentState {
            pane_id: pane_id.to_string(),
            agent: None,
            attention: None,
            mode: None,
        });
    }

    /// (T1) Set or clear a pane's manual agent mark. `Some(name)` marks the
    /// pane (overriding auto-detection); `None` returns it to auto-detection.
    /// Only the flag/mark is updated here — the caller then runs
    /// `classify_agent_now` to recompute state and broadcast any transition.
    fn set_manual_agent(&self, pane_id: &str, agent: Option<String>) {
        if let Ok(mut tracker) = self.agents.lock() {
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            match agent {
                Some(name) => {
                    entry.manual = true;
                    entry.agent = Some(name);
                }
                None => {
                    entry.manual = false;
                    // (T1) M3: a manual unmark clears the mark itself, so the
                    // next classification re-detects with the FRESH 2-group
                    // threshold — the 1-group hysteresis floor only applies
                    // to genuinely auto-detected panes.
                    entry.agent = None;
                    entry.zero_signature_streak = 0;
                }
            }
        }
    }

    /// (T1) The pane's (manual, agent) mark pair — captured before a
    /// SetPaneAgent so a failed persist can revert (L8).
    fn agent_mark(&self, pane_id: &str) -> (bool, Option<String>) {
        self.agents
            .lock()
            .ok()
            .and_then(|tracker| {
                tracker
                    .panes
                    .get(pane_id)
                    .map(|entry| (entry.manual, entry.agent.clone()))
            })
            .unwrap_or((false, None))
    }

    /// (T1) Restore a (manual, agent) mark pair captured by `agent_mark`:
    /// the revert half of a SetPaneAgent whose persist failed (L8), keeping
    /// disk, memory, and clients from diverging.
    fn restore_agent_mark(&self, pane_id: &str, mark: (bool, Option<String>)) {
        if let Ok(mut tracker) = self.agents.lock() {
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            entry.manual = mark.0;
            entry.agent = mark.1;
            entry.zero_signature_streak = 0;
        }
    }

    /// (T1) A pane's process ended (or was killed for a restart): a dead
    /// agent is neither working nor waiting, so its attention state clears
    /// and no automatic classification runs until the next spawn (the
    /// `ended` flag — a trailing-edge classification must not resurrect a
    /// badge from the preserved final screen). The agent MARK is kept: the
    /// signature is still on screen, and a manual mark outlives its process.
    /// Broadcasts the final AgentState transition if the pane had attention.
    fn clear_agent_attention(&self, pane_id: &str) {
        let cleared = self.agents.lock().ok().and_then(|mut tracker| {
            let entry = tracker.panes.get_mut(pane_id)?;
            entry.ended = true;
            let previous = entry.attention.take()?;
            entry.last_attention = Some(previous);
            Some((entry.agent.clone(), previous, entry.mode.clone()))
        });
        if let Some((Some(agent), previous, mode)) = cleared {
            self.ledger_note(
                pane_id,
                "attention.changed",
                json!({
                    "agent": agent,
                    "from": previous,
                    "to": Value::Null,
                    "evidence": "process ended",
                }),
            );
            self.broadcast(&DaemonEvent::AgentState {
                pane_id: pane_id.to_string(),
                agent: Some(agent),
                attention: None,
                mode,
            });
        }
    }

    /// (T1) The manual agent marks to persist (pane_id → agent name). Detected
    /// (non-manual) state is deliberately excluded: it re-derives from the
    /// screen after a restart.
    fn manual_agent_marks(&self) -> HashMap<String, String> {
        self.agents
            .lock()
            .map(|tracker| {
                tracker
                    .panes
                    .iter()
                    .filter(|(_, entry)| entry.manual)
                    .filter_map(|(pane_id, entry)| {
                        entry.agent.clone().map(|agent| (pane_id.clone(), agent))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (T1) Restore persisted manual marks at daemon startup. Entries start
    /// unclassified (attention None); the pane's next output (or an explicit
    /// SetPaneAgent) classifies from the live screen.
    fn seed_manual_agents(&self, marks: HashMap<String, String>) {
        if let Ok(mut tracker) = self.agents.lock() {
            for (pane_id, agent) in marks {
                tracker.panes.insert(
                    pane_id,
                    AgentPaneState {
                        agent: Some(agent),
                        manual: true,
                        ..Default::default()
                    },
                );
            }
        }
    }

    /// (T1) Current agent info for one pane (default/empty when untracked).
    fn agent_state(&self, pane_id: &str) -> AgentPaneInfo {
        self.agents
            .lock()
            .ok()
            .and_then(|tracker| tracker.panes.get(pane_id).map(AgentPaneState::info))
            .unwrap_or_default()
    }

    /// (T1) Agent info for every tracked pane — find reads it once instead of
    /// locking per pane.
    fn agent_info_map(&self) -> HashMap<String, AgentPaneInfo> {
        self.agents
            .lock()
            .map(|tracker| {
                tracker
                    .panes
                    .iter()
                    .map(|(pane_id, entry)| (pane_id.clone(), entry.info()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (T1) Agent info for panes with a KNOWN agent (manual or detected) — the
    /// bootstrap payload's per-pane agent map.
    fn agent_states(&self) -> HashMap<String, AgentPaneInfo> {
        self.agents
            .lock()
            .map(|tracker| {
                tracker
                    .panes
                    .iter()
                    .filter(|(_, entry)| entry.agent.is_some())
                    .map(|(pane_id, entry)| (pane_id.clone(), entry.info()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (T1) Drop a pane's agent tracking when the pane is closed: its mark dies
    /// with it (pane ids are never reused) and persist stops carrying it.
    fn remove_agent(&self, pane_id: &str) {
        if let Ok(mut tracker) = self.agents.lock() {
            tracker.panes.remove(pane_id);
        }
    }

    /// Track a pane resize in its model so post-resize content lays out on the new grid.
    fn resize_model(&self, pane_id: &str, cols: u16, rows: u16) {
        if let Some(model) = self.model_handle(pane_id) {
            if let Ok(mut model) = model.lock() {
                model.set_size(cols, rows);
            }
        }
    }

    /// Drop a pane's screen model when the pane is closed.
    fn remove_model(&self, pane_id: &str) {
        if let Ok(mut models) = self.models.lock() {
            models.remove(pane_id);
        }
    }

    fn emit(&self, pane_id: &str, data: String) {
        // A pane removed via ClosePane must never recreate its scrollback file or
        // deliver further output to subscribers, even if its reader is still draining.
        if self.is_closed(pane_id) {
            return;
        }
        let _ = self.append_scrollback(pane_id, &data);
        self.record_output_tricks(pane_id, &data);

        let event = DaemonEvent::PtyOutput {
            pane_id: pane_id.to_string(),
            data,
        };
        self.broadcast(&event);
    }

    fn emit_pane_ended(&self, pane_id: &str, exit_code: Option<i32>) {
        if self.is_closed(pane_id) {
            return;
        }
        tracing::info!(
            workspace_key = %self.log_workspace_key(),
            pane_id = %pane_id,
            exit_code = exit_code.unwrap_or(-1),
            event = "pane_end",
            "pane ended"
        );
        self.ledger_note(
            pane_id,
            "pane.ended",
            self.pane_exit_record(pane_id, exit_code),
        );
        let event = DaemonEvent::PaneEnded {
            pane_id: pane_id.to_string(),
            exit_code,
        };
        self.broadcast(&event);
    }

    /// The `pane.ended` payload: the exit code plus what a reviewer (or a
    /// Kranz gate) classifies the run on. `attention` is the state the agent
    /// was in when its process ended (`needs_input` = it was still waiting on
    /// a person), `holder` the keyboard holder at exit, `output_tricks` the
    /// guard's totals when any fired. Every key beyond `exit_code` is
    /// additive; older readers ignore them.
    fn pane_exit_record(&self, pane_id: &str, exit_code: Option<i32>) -> Value {
        let (agent, attention, mode, unattended) = self
            .agents
            .lock()
            .ok()
            .and_then(|tracker| {
                tracker.panes.get(pane_id).map(|entry| {
                    (
                        entry.agent.clone(),
                        entry.last_attention.or(entry.attention),
                        entry.mode.clone(),
                        is_unattended_mode(entry.mode.as_deref()),
                    )
                })
            })
            .unwrap_or((None, None, None, false));
        let mut payload = json!({
            "exit_code": exit_code,
            "agent": agent,
            "attention": attention,
            "mode": mode,
            "unattended": unattended,
            "holder": self.lease_holder(pane_id),
        });
        let tricks = self.output_tricks(pane_id);
        if tricks.total() > 0 {
            payload["output_tricks"] = json!(tricks);
        }
        payload
    }

    fn broadcast(&self, event: &DaemonEvent) {
        // (M11) Skip the serialization entirely when nobody is listening —
        // PtyOutput is the hottest event and the daemon routinely runs with
        // zero subscribers (e.g. between GUI sessions).
        if self
            .subscribers
            .lock()
            .map(|subscribers| subscribers.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        let Ok(payload_json) = serde_json::to_vec(&event) else {
            return;
        };

        // The JSON payload is serialized once; the two wire encodings (newline v1 and
        // framed v2) are built lazily — at most once each per broadcast — so the
        // shared-Arc fan-out is preserved even with mixed-version subscribers.
        let mut newline: Option<Arc<Vec<u8>>> = None;
        let mut framed: Option<Arc<Vec<u8>>> = None;

        // Only a non-blocking try_send happens under the lock; the actual socket write
        // is done by each subscriber's writer thread. A subscriber whose queue is full
        // (a consumer too slow to keep up) or whose writer thread has exited is dropped.
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| {
                let cached = if subscriber.wire_version >= frame::WIRE_VERSION {
                    &mut framed
                } else {
                    &mut newline
                };
                let payload = match cached {
                    Some(payload) => Arc::clone(payload),
                    None => match encode_event_for_wire(subscriber.wire_version, &payload_json) {
                        Some(bytes) => {
                            let arc = Arc::new(bytes);
                            *cached = Some(Arc::clone(&arc));
                            arc
                        }
                        // An event too large to frame is skipped for this subscriber
                        // (rather than corrupting its stream); keep it subscribed.
                        None => return true,
                    },
                };
                !matches!(
                    subscriber.sender.try_send(payload),
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_))
                )
            });
        }
    }

    fn append_scrollback(&self, pane_id: &str, data: &str) -> Result<(), String> {
        // Clone the per-pane append state out of the map under a tiny lock
        // (creating it on first append), then do ALL I/O under the per-pane
        // state lock (M11): no open/close/stat per chunk on the hot path, and
        // the state lock serializes appends with the cap rewrite below so no
        // chunk can land in the cap's read→rename window.
        let state = {
            let mut handles = self
                .append_handles
                .lock()
                .map_err(|_| "scrollback append-handles lock poisoned".to_string())?;
            match handles.get(pane_id) {
                Some(state) => Arc::clone(state),
                None => {
                    let path = scrollback_path(&self.scrollback_dir, pane_id);
                    let file = match open_scrollback_append(&path) {
                        Ok(file) => file,
                        // The scrollback dir vanished at runtime (e.g. manual
                        // cleanup): recreate it once and retry, instead of
                        // paying a mkdir+chmod on every chunk.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ensure_private_dir(&self.scrollback_dir)?;
                            open_scrollback_append(&path)
                                .map_err(|error| format!("failed to open scrollback: {error}"))?
                        }
                        Err(error) => {
                            return Err(format!("failed to open scrollback: {error}"));
                        }
                    };
                    // The byte count is unknown on first open: stat ONCE here to
                    // seed the in-memory count (M11: stat is the fallback when
                    // the count is unknown, never per-chunk).
                    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                    let state = Arc::new(Mutex::new(ScrollbackAppendState { file, len }));
                    handles.insert(pane_id.to_string(), Arc::clone(&state));
                    state
                }
            }
        };

        let mut state = state
            .lock()
            .map_err(|_| "scrollback append lock poisoned".to_string())?;
        state
            .file
            .write_all(data.as_bytes())
            .map_err(|error| format!("failed to write scrollback: {error}"))?;
        state.len += data.len() as u64;

        if state.len > SCROLLBACK_MAX_BYTES {
            // The cap rewrites the file (temp + rename), replacing the inode the
            // cached handle points at. Still holding the state lock — so no
            // append can interleave — cap, then re-open the new file and
            // re-prime the count (the only other stat on this path).
            cap_scrollback_file(&self.scrollback_dir, pane_id)?;
            let path = scrollback_path(&self.scrollback_dir, pane_id);
            let file = open_scrollback_append(&path)
                .map_err(|error| format!("failed to open scrollback: {error}"))?;
            state.len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            state.file = file;
        }
        Ok(())
    }

    /// Drop a pane's cached append handle when its scrollback file is deleted
    /// (ClosePane / create rollback), so a later append can't keep writing to
    /// the unlinked inode (M11). Restart needs no invalidation: the file is
    /// untouched, so the handle and byte count stay valid.
    fn invalidate_append_handle(&self, pane_id: &str) {
        if let Ok(mut handles) = self.append_handles.lock() {
            handles.remove(pane_id);
        }
    }

    /// Drop cached append handles for panes no longer in the registry: a reader
    /// racing ClosePane can re-create a handle (and file) for a dead pane;
    /// swept on the closed-pane cadence so the map can't grow unboundedly (M11).
    fn prune_orphan_append_handles(&self, live_pane_ids: &HashSet<String>) {
        if let Ok(mut handles) = self.append_handles.lock() {
            handles.retain(|pane_id, _| live_pane_ids.contains(pane_id));
        }
    }
}

struct TerminalStore {
    cwd: PathBuf,
    sessions: HashMap<String, TerminalSession>,
    sizes: HashMap<String, PtySize>,
    liveness: Arc<Mutex<HashMap<String, PaneLiveness>>>,
    next_generation: u64,
    router: OutputRouter,
    shell: ShellConfig,
    /// Per-pane shell overrides from named profiles (frozen at create time;
    /// persisted across daemon restarts and preserved across in-process restart).
    pane_shells: HashMap<String, ShellConfig>,
    /// Panes whose spawn (openpty + fork/exec) is currently running WITHOUT the
    /// store lock held (M7). Checked/inserted/removed only under the lock;
    /// paired with DaemonServer::spawn_cvar so a concurrent ensure for the same
    /// pane waits for the in-flight spawn to commit instead of double-spawning.
    spawning: HashSet<String>,
    /// (T2) Live agent sessions by pane id (unix + Windows: agent panes spawn
    /// a headless `claude` CLI with piped stdio; see the T2 section below).
    /// Shares `liveness`, `spawning`, and `next_generation` with PTY sessions
    /// so runtime state, PaneEnded, and the M7 spawn discipline apply
    /// uniformly. On platforms that are neither unix nor Windows the map never
    /// fills (spawn fails with a clean cfg error first).
    #[cfg(any(unix, windows))]
    agent_sessions: HashMap<String, AgentSession>,
    /// (T2) pane id → last known `claude` session id, seeded from persisted
    /// `agents_v2` at daemon start. Consulted for `--resume` when a pane has
    /// no live session to take the id from.
    #[cfg(any(unix, windows))]
    agent_resume: HashMap<String, String>,
    /// Immutable provider/model identity for agent panes, including restored
    /// panes whose process has not been started yet.
    agent_specs: HashMap<String, AgentPaneSpec>,
    /// (T2) Agent spawn config (binary override + permission mode), mirrored
    /// from Config and refreshed by the config file-watch like `shell`.
    /// Read only by the agent-spawn path (unix + Windows).
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    agent_config: AgentSpawnConfig,
    /// (T2) Directory holding per-pane agent conversation logs
    /// (`<data_dir>/agents/<pane-id>.jsonl`); read by the bootstrap replay.
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    agents_dir: PathBuf,
    /// (T2) The daemon's lazy-persist flag: a reader thread sets it when it
    /// records a CLI session id, so agents_v2 reaches workspace.json within a
    /// persist cadence instead of only at shutdown.
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    agent_dirty: Arc<AtomicBool>,
}

fn default_pty_size() -> PtySize {
    pty_size(120, 40)
}

/// Kill a just-spawned child and reap it (spawn partial-failure cleanup): when
/// PTY writer/reader setup fails after `spawn_command` succeeded, the child must
/// not be abandoned — unwatched it would keep running with no liveness entry (a
/// later ensure would spawn a second shell) and un-reaped.
fn kill_and_reap_child(mut child: Box<dyn portable_pty::Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Everything a spawn needs out of the TerminalStore (M7): read once under the
/// store lock so the expensive PTY setup (`execute_spawn`) can run WITHOUT it.
struct SpawnPlan {
    size: PtySize,
    shell: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    scrub_env: Vec<String>,
    cwd: PathBuf,
    command_str: String,
    cwd_str: String,
}

/// A spawned-but-not-yet-committed pane session (M7): the expensive half of a
/// spawn, produced by `execute_spawn` off the store lock and committed under it
/// by `TerminalStore::commit_spawn`.
struct PreparedSpawn {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
    command_str: String,
    cwd_str: String,
}

/// The expensive half of a spawn — openpty + fork/exec + reader/writer setup —
/// run WITHOUT the TerminalStore lock (M7: a slow spawn used to stall input,
/// resize, and liveness for every pane). On a partial failure after
/// `spawn_command` succeeded, the child is killed + reaped (review-low).
/// Whether an `openpty` failure is worth a brief retry: the kernel's pty pool
/// momentarily exhausted (macOS ENXIO "Device not configured", EAGAIN on
/// either platform) rather than a configuration error.
fn is_transient_pty_error(message: &str) -> bool {
    message.contains("Device not configured")
        || message.contains("Resource temporarily unavailable")
        || message.contains("os error 6)")
        || message.contains("os error 11)")
        || message.contains("os error 35)")
}

/// `openpty` with a short bounded retry on transient pool exhaustion (seen
/// under parallel test load on CI runners); anything else fails immediately.
fn open_pty_with_retry(
    pty_system: &dyn portable_pty::PtySystem,
    size: PtySize,
) -> Result<portable_pty::PtyPair, String> {
    let mut attempt: u32 = 0;
    loop {
        match pty_system.openpty(size) {
            Ok(pair) => return Ok(pair),
            Err(error) => {
                let message = error.to_string();
                attempt += 1;
                if !is_transient_pty_error(&message) || attempt >= 8 {
                    return Err(format!("failed to open pty: {message}"));
                }
                thread::sleep(Duration::from_millis(25 * u64::from(attempt)));
            }
        }
    }
}

fn execute_spawn(plan: &SpawnPlan) -> Result<PreparedSpawn, String> {
    let pty_system = native_pty_system();
    let pair = open_pty_with_retry(pty_system.as_ref(), plan.size)?;

    let mut command = CommandBuilder::new(&plan.shell);
    for arg in &plan.args {
        command.arg(arg);
    }
    command.cwd(&plan.cwd);
    // Opt-in environment scrubbing: remove sensitive inherited vars before
    // the pane's shell sees them. Default (empty scrub list) preserves the
    // full inherited environment. Explicit `env` entries (including the
    // TERM/COLORTERM defaults below) are applied AFTER scrubbing, so an
    // operator-set value takes precedence over the scrub list for the same
    // variable name. VAL-SEC-003/004/007.
    for key in INHERITED_SESSION_MARKERS {
        command.env_remove(key);
    }
    for key in &plan.scrub_env {
        command.env_remove(key);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for (key, value) in &plan.env {
        command.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("failed to spawn shell: {error}"))?;
    // Split a killer off the child: the session keeps the killer to terminate
    // the process on close/restart, while the reader thread takes ownership of
    // `child` so it can reap it via `child.wait()` (the authoritative exit code).
    let killer = child.clone_killer();
    // (review-low) A failure setting up the writer/reader AFTER the child
    // spawned must not abandon it: kill + reap, or the shell leaks unwatched
    // (no liveness entry exists yet, so a later ensure would spawn a second).
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
        command_str: plan.command_str.clone(),
        cwd_str: plan.cwd_str.clone(),
    })
}

/// Upper bounds on a pane's grid. Every size — resize requests (u16s straight
/// from a client), persisted sizes, and the vt100 model resize — funnels through
/// `pty_size`, so this is the single clamp that keeps a hostile/hand-edited
/// 65535×65535 from allocating a ~4.3-billion-cell screen model and OOM-killing
/// the daemon (and every shell it owns). 2000×1000 (2M cells) comfortably exceeds
/// any real display — an 8K monitor at a tiny 6 px cell is ~1280 columns — while
/// bounding the worst-case model allocation to tens of megabytes.
const MAX_PTY_COLS: u16 = 2000;
const MAX_PTY_ROWS: u16 = 1000;

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.clamp(1, MAX_PTY_ROWS),
        cols: cols.clamp(2, MAX_PTY_COLS),
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl TerminalStore {
    #[allow(clippy::too_many_arguments)]
    fn new(
        cwd: PathBuf,
        router: OutputRouter,
        sizes: HashMap<String, PtySize>,
        shell: ShellConfig,
        agent_config: AgentSpawnConfig,
        agents_dir: PathBuf,
        agent_dirty: Arc<AtomicBool>,
        agent_specs: HashMap<String, AgentPaneSpec>,
    ) -> Self {
        Self {
            cwd,
            sessions: HashMap::new(),
            sizes,
            liveness: Arc::new(Mutex::new(HashMap::new())),
            next_generation: 0,
            router,
            shell,
            pane_shells: HashMap::new(),
            spawning: HashSet::new(),
            #[cfg(any(unix, windows))]
            agent_sessions: HashMap::new(),
            #[cfg(any(unix, windows))]
            agent_resume: HashMap::new(),
            agent_specs,
            agent_config,
            agents_dir,
            agent_dirty,
        }
    }

    #[cfg(test)]
    fn new_for_tests(cwd: PathBuf) -> Self {
        Self::new(
            cwd,
            OutputRouter::new(std::env::temp_dir()),
            HashMap::new(),
            ShellConfig::default(),
            AgentSpawnConfig::default(),
            std::env::temp_dir().join("sgian-test-agents"),
            Arc::new(AtomicBool::new(false)),
            HashMap::new(),
        )
    }

    /// Seed the liveness map with `ended: true` entries for panes whose Ended state
    /// was persisted across a daemon restart. This ensures `runtime_states` reports
    /// the correct state for restored panes that have not been (re)spawned yet.
    /// A subsequent `spawn_pane` replaces the entry with `ended: false`.
    fn seed_ended_panes(&mut self, pane_ids: &[String]) {
        if pane_ids.is_empty() {
            return;
        }
        if let Ok(mut liveness) = self.liveness.lock() {
            for pane_id in pane_ids {
                // Only seed if there is no existing entry (don't overwrite a live pane).
                liveness.entry(pane_id.clone()).or_insert(PaneLiveness {
                    generation: 0,
                    ended: true,
                    command: None,
                    cwd: None,
                    exit_code: None,
                    pid: None,
                });
            }
        }
    }

    /// A pane is live when its current-generation reader is still running. A session
    /// whose shell exited (reader hit EOF) is reported not-live so it can be respawned.
    fn is_live(&self, pane_id: &str) -> bool {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .get(pane_id)
                    .map(|entry| !entry.ended)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// A pane with a spawn IN FLIGHT (M7) has no liveness entry yet, so `is_live`
    /// reports false during the fork/exec window — `wait --exit` must not resolve a
    /// spurious "exit" there, so callers asking "has this pane ended?" use this.
    fn is_live_or_spawning(&self, pane_id: &str) -> bool {
        self.spawning.contains(pane_id) || self.is_live(pane_id)
    }

    /// Read everything a spawn needs out of the store (M7): the cheap half of a
    /// spawn, taken under the lock so the expensive PTY setup (`execute_spawn`)
    /// can run WITHOUT the store lock.
    fn plan_spawn(&self, pane_id: &str) -> SpawnPlan {
        let cfg = self.pane_shells.get(pane_id).unwrap_or(&self.shell);
        let shell = if cfg.shell.is_empty() {
            default_shell()
        } else {
            cfg.shell.clone()
        };
        // Capture spawn metadata (launched command + resolved working dir) so it is
        // queryable via snapshot/find for the life of the pane — including after it
        // has ended, until it is closed or restarted.
        let command_str = if cfg.args.is_empty() {
            shell.clone()
        } else {
            format!("{} {}", shell, cfg.args.join(" "))
        };
        SpawnPlan {
            size: self
                .sizes
                .get(pane_id)
                .copied()
                .unwrap_or_else(default_pty_size),
            shell,
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            scrub_env: cfg.scrub_env.clone(),
            cwd: self.cwd.clone(),
            command_str,
            cwd_str: self.cwd.to_string_lossy().to_string(),
        }
    }

    /// Single-caller path (unit tests): plan → execute → commit, all under
    /// the caller's store lock. Production spawns go through DaemonServer's
    /// ensure/restart, which run `execute_spawn` WITHOUT the lock (M7) — so
    /// this wrapper is `#[cfg(test)]`, like the other test-only helpers.
    #[cfg(test)]
    fn spawn_pane(&mut self, pane_id: &str) -> Result<(), String> {
        let plan = self.plan_spawn(pane_id);
        let prepared = execute_spawn(&plan)?;
        self.commit_spawn(pane_id, plan.size, prepared);
        Ok(())
    }

    /// Commit a spawned session under the store lock (M7): bump the generation,
    /// install liveness/model/session/sizes, and start the reader thread. This
    /// is the only part of a spawn that mutates the store — the expensive
    /// fork/exec in `execute_spawn` runs without the lock, so a slow spawn no
    /// longer stalls input/resize/liveness for every other pane.
    fn commit_spawn(&mut self, pane_id: &str, size: PtySize, prepared: PreparedSpawn) {
        let PreparedSpawn {
            master,
            child,
            killer,
            writer,
            mut reader,
            command_str,
            cwd_str,
        } = prepared;

        self.next_generation += 1;
        let generation = self.next_generation;
        let child_pid = child.process_id();
        if let Ok(mut liveness) = self.liveness.lock() {
            liveness.insert(
                pane_id.to_string(),
                PaneLiveness {
                    generation,
                    ended: false,
                    command: Some(command_str),
                    cwd: Some(cwd_str),
                    exit_code: None,
                    pid: child_pid,
                },
            );
        }

        // Create (or reset, on an in-place restart) the pane's vt100 screen model
        // before the reader starts feeding it. `ensure_model` preserves the
        // monotonic revision across a restart.
        self.router.ensure_model(pane_id, size.cols, size.rows);

        let output_pane_id = pane_id.to_string();
        let output_router = self.router.clone();
        let liveness = Arc::clone(&self.liveness);
        thread::spawn(move || {
            // Take ownership of the child so this reader can reap it (`child.wait()`)
            // at EOF to capture the authoritative exit status.
            let mut child = child;
            // Activate structured logging for this reader thread (best-effort:
            // no-op if no subscriber was configured, e.g. in unit tests).
            let _log_guard = output_router.log_guard();
            // A reader only emits while it owns the pane's current generation; a
            // restart or close supersedes it, and a superseded reader must not keep
            // leaking its dying session's output into the replacement session's
            // stream. The check is per-chunk (not atomic with the emit), so at most
            // one already-read chunk can slip through on a restart.
            let owns_session = || {
                liveness
                    .lock()
                    .ok()
                    .map(|map| {
                        map.get(&output_pane_id)
                            .is_some_and(|entry| entry.generation == generation)
                    })
                    .unwrap_or(false)
            };

            let mut buffer = [0_u8; 8192];
            let mut pending_utf8 = Vec::new();
            loop {
                let byte_count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(byte_count) => byte_count,
                    Err(_) => break,
                };
                if !owns_session() {
                    // Superseded (restart/close killed our child): still reap it.
                    // Returning without wait() left a zombie for the daemon's
                    // lifetime (M5); the child is already killed, so this is prompt.
                    let _ = child.wait();
                    return;
                }

                // Feed the SAME raw PTY bytes into the screen model. vt100 keeps its
                // own UTF-8 state across calls, so it gets the full byte stream
                // (unlike `emit`, which holds back split multibyte tails). The parser
                // lock is held only for this call, never across the PTY read above.
                output_router.feed_model(&output_pane_id, &buffer[..byte_count]);

                pending_utf8.extend_from_slice(&buffer[..byte_count]);
                for data in drain_complete_utf8(&mut pending_utf8) {
                    output_router.emit(&output_pane_id, data);
                }
            }

            if !pending_utf8.is_empty() && owns_session() {
                let data = String::from_utf8_lossy(&pending_utf8).to_string();
                output_router.emit(&output_pane_id, data);
            }

            // Reap the child to obtain the authoritative exit status. EOF means the
            // slave closed (the process has exited or is exiting), so this returns
            // promptly. Running the reaper on THIS reader thread, at the natural reap
            // point, means the captured code and the single PaneEnded emission can
            // never race a second emitter (Invariant 1); `wait()` is authoritative,
            // and an errored wait falls back to a `None` code without double-emitting.
            let exit_code = match child.wait() {
                Ok(status) => reaped_exit_code(&status),
                Err(_) => None,
            };

            // Only report the pane ended if this reader still owns the live session;
            // a newer session (restart) or a ClosePane will have superseded us, in
            // which case its (replaced/removed) entry's generation no longer matches
            // and we claim nothing — so a stale reader never ends a live pane and
            // PaneEnded fires exactly once per generation.
            let claimed = liveness
                .lock()
                .ok()
                .and_then(|mut liveness| {
                    liveness.get_mut(&output_pane_id).map(|entry| {
                        if entry.generation == generation && !entry.ended {
                            entry.ended = true;
                            entry.exit_code = exit_code;
                            true
                        } else {
                            false
                        }
                    })
                })
                .unwrap_or(false);
            if claimed {
                // (T1) M2: a dead agent must not keep a working/needs-input
                // badge — clear the attention half of its tracked state (the
                // mark itself stays; the signature is on the final screen).
                output_router.clear_agent_attention(&output_pane_id);
                output_router.emit_pane_ended(&output_pane_id, exit_code);
            }
        });

        self.sessions.insert(
            pane_id.to_string(),
            TerminalSession {
                _master: master,
                pid: child_pid,
                #[cfg(windows)]
                _job: child_pid.and_then(KillOnCloseJob::attach),
                killer,
                input: spawn_input_writer(writer),
            },
        );
        self.sizes.insert(pane_id.to_string(), size);
    }

    fn close_pane(&mut self, pane_id: &str) {
        self.sessions.remove(pane_id);
        self.pane_shells.remove(pane_id);
        // (T2) Dropping the AgentSession denies its pending approvals (waking
        // a blocked reader) and kills the CLI (SIGTERM→SIGKILL on unix,
        // TerminateProcess on Windows).
        #[cfg(any(unix, windows))]
        {
            self.agent_sessions.remove(pane_id);
            self.agent_resume.remove(pane_id);
        }
        self.agent_specs.remove(pane_id);
        self.sizes.remove(pane_id);
        if let Ok(mut liveness) = self.liveness.lock() {
            liveness.remove(pane_id);
        }
        // (T1) M2: same attention clear as the reader-EOF path — a restart
        // kills the process without a claimed EOF, and a dead agent must not
        // keep its badge. On ClosePane the tracker entry is already gone
        // (remove_agent runs first), so this is a no-op there.
        self.router.clear_agent_attention(pane_id);
    }

    /// Kill every live session's child directly. Daemon shutdown must not rely
    /// on `TerminalSession::drop` alone: lingering connection threads hold
    /// `Arc<DaemonServer>` clones, which can defer the store's drop indefinitely
    /// and leak SIGHUP-ignoring children past daemon exit (L17).
    fn kill_all_sessions(&mut self) {
        for session in self.sessions.values_mut() {
            #[cfg(unix)]
            if let Some(pid) = session.pid {
                terminate_process_tree(pid);
            }
            let _ = session.killer.kill();
        }
        // (T2) Same for agent CLIs; each AgentSession's Drop also fires, but a
        // deferred store drop would defer it past daemon exit (L17 above).
        #[cfg(any(unix, windows))]
        for session in self.agent_sessions.values() {
            session.kill();
        }
    }

    /// Test-only store-level restart (production restarts go through
    /// DaemonServer::restart_terminal, which runs the fork/exec off the lock —
    /// M7). close_pane drops the sizes entry, which would silently reset the
    /// pane to the default 120x40 on respawn (review-low): preserve the size so
    /// the restarted pane keeps its dimensions (persisted map AND new PTY).
    #[cfg(test)]
    fn restart_pane(&mut self, pane_id: &str) -> Result<(), String> {
        let size = self.sizes.get(pane_id).copied();
        let shell = self.pane_shells.get(pane_id).cloned();
        self.close_pane(pane_id);
        if let Some(size) = size {
            self.sizes.insert(pane_id.to_string(), size);
        }
        if let Some(shell) = shell {
            self.pane_shells.insert(pane_id.to_string(), shell);
        }
        self.spawn_pane(pane_id)
    }

    /// Read a pane's spawn/exit metadata (launched command, working dir, captured
    /// exit code) out of its generation-tagged liveness entry. Returns the default
    /// (all `None`) for an unknown pane. Stays populated for an ended pane until it
    /// is closed/restarted (VAL-TERM-021).
    fn pane_meta(&self, pane_id: &str) -> PaneMeta {
        self.liveness
            .lock()
            .ok()
            .and_then(|liveness| {
                liveness.get(pane_id).map(|entry| PaneMeta {
                    command: entry.command.clone(),
                    cwd: entry.cwd.clone(),
                    exit_code: entry.exit_code,
                })
            })
            .unwrap_or_default()
    }

    fn runtime_states(&self, pane_ids: &[String]) -> HashMap<String, PaneRuntimeState> {
        let liveness = self.liveness.lock().ok();
        pane_ids
            .iter()
            .map(|pane_id| {
                let live = liveness
                    .as_ref()
                    .and_then(|liveness| liveness.get(pane_id))
                    .map(|entry| !entry.ended)
                    .unwrap_or(false);
                let state = if live {
                    PaneRuntimeState::Live
                } else {
                    PaneRuntimeState::Ended
                };
                (pane_id.clone(), state)
            })
            .collect()
    }

    /// Queue input to a pane. The blocking PTY write happens on the pane's own
    /// writer thread — never here, where the caller holds the store mutex (H2).
    fn write_to_pane(&self, pane_id: &str, data: &str) -> Result<(), String> {
        if !self.is_live(pane_id) {
            return Err(format!("terminal session ended: {pane_id}"));
        }

        let session = self
            .sessions
            .get(pane_id)
            .ok_or_else(|| format!("terminal session not found: {pane_id}"))?;
        queue_pane_input(&session.input, pane_id, data)
    }

    /// (M4) Live panes and their spawn cwd (the default Kranz repo).
    fn live_pane_cwds(&self) -> HashMap<String, String> {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .iter()
                    .filter(|(_, entry)| !entry.ended)
                    .filter_map(|(pane_id, entry)| {
                        entry.cwd.clone().map(|cwd| (pane_id.clone(), cwd))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn pane_cwd(&self, pane_id: &str) -> Option<String> {
        self.liveness
            .lock()
            .ok()
            .and_then(|liveness| liveness.get(pane_id).and_then(|entry| entry.cwd.clone()))
    }

    /// (M3b) Live shell panes with a recorded child pid, for the official
    /// agent probe's process-tree mapping.
    fn live_pane_pids(&self) -> Vec<(String, u32)> {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .iter()
                    .filter(|(_, entry)| !entry.ended)
                    .filter_map(|(pane_id, entry)| entry.pid.map(|pid| (pane_id.clone(), pid)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn live_pane_ids(&self) -> Vec<String> {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .iter()
                    .filter(|(_, entry)| !entry.ended)
                    .map(|(pane_id, _)| pane_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Write `data` to every live pane except those in `skip` (best-effort);
    /// returns the panes written to. `skip` carries panes whose keyboard is
    /// held by someone other than the writer.
    fn write_to_live_except(&self, data: &str, skip: &HashSet<String>) -> Vec<String> {
        let mut written = Vec::new();
        for pane_id in self.live_pane_ids() {
            if skip.contains(&pane_id) {
                continue;
            }
            if self.write_to_pane(&pane_id, data).is_ok() {
                written.push(pane_id);
            }
        }
        written
    }

    fn resize_pane(&mut self, pane_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let size = pty_size(cols, rows);

        if let Some(session) = self.sessions.get_mut(pane_id) {
            session
                ._master
                .resize(size)
                .map_err(|error| format!("failed to resize terminal: {error}"))?;
        }

        self.sizes.insert(pane_id.to_string(), size);
        // Resize the screen model to match the PTY so post-resize output lays out on
        // the new grid. This does not bump the model's revision (resize is not output).
        self.router.resize_model(pane_id, size.cols, size.rows);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// (T2) Chat-native agent sessions
//
// An agent pane runs a headless `claude` CLI process instead of a PTY shell:
//
//     claude -p --input-format stream-json --output-format stream-json \
//            --verbose --permission-mode <mode> --permission-prompt-tool stdio \
//            --include-partial-messages [--resume <session_id>]
//
// The protocol shapes below were pinned empirically against claude 2.1.201
// (STEP 0 probes, read-only prompts):
//
//   a) NO `initialize` control_request handshake: the CLI accepts the first
//      user message directly and emits `{"type":"system","subtype":"init"}`
//      on stdout at startup (and again before each subsequent turn), carrying
//      `session_id`, `model`, `tools`, `cwd`.
//   b) Event shapes: assistant content arrives BOTH as `stream_event` partials
//      (raw Anthropic stream events: message_start / content_block_start /
//      content_block_delta{text_delta} / content_block_stop / message_stop —
//      these require --include-partial-messages) AND as complete `assistant`
//      messages (one per content block, full `input` for tool_use). Tool
//      results arrive as `user` messages with `tool_result` content blocks.
//      A turn ends with `result` (subtype "success" | "error_*", with `usage`,
//      `total_cost_usd`, `duration_ms`, `num_turns`). The CLI also emits
//      `rate_limit_event` and `system/thinking_tokens` noise we drop.
//   c) Permission prompts: in `--permission-mode manual` WITHOUT a prompt
//      tool the CLI auto-DENIES tools that need approval (no prompt reaches
//      stdio). With `--permission-prompt-tool stdio` they arrive as
//      `{"type":"control_request","request_id":...,"request":{"subtype":
//      "can_use_tool","tool_name":...,"input":{...},"tool_use_id":...}}` and
//      the CLI BLOCKS until we write a `control_response` line:
//      allow: {"type":"control_response","response":{"request_id":...,
//              "subtype":"success","response":{"behavior":"allow",
//              "updatedInput":<original input>}}}
//      deny:  ... "response":{"behavior":"deny","message":<feedback>}}
//   d) MULTI-TURN: the process stays alive across turns (verified: second
//      user message after `result` gets a fresh init + turn). Closing stdin
//      exits the process with code 0 — persistent process, NOT respawn-per-
//      turn.
//   e) `--resume <session_id>` works with stream-json input: init comes back
//      with the SAME session_id and the conversation continues.
//   f) Interrupt: write {"type":"control_request","request_id":<ours>,
//      "request":{"subtype":"interrupt"}; the CLI answers with a
//      control_response on stdout and ends the turn with
//      `result` subtype "error_during_execution".
//
// Sessions follow the M7 spawn discipline (plan under the lock, fork/exec off
// it, commit under it; in-flight marker + spawn_cvar against double-spawns)
// and share the PTY sessions' generation-tagged `liveness` map, so runtime
// state, PaneEnded, and close/restart races behave identically. Process
// management is per-platform: unix sends SIGTERM with a SIGKILL escalation
// after a grace period (via libc); Windows has no signals, so kills are a
// direct `Child::kill()` (TerminateProcess — immediate, no grace). Remaining
// Windows deltas: TerminateProcess kills only the DIRECT child (claude is a
// node app; Job-Object tree-kill is out of scope, so grandchildren may
// outlive the session), and npm's `claude.cmd` shim must be spawned via
// `cmd.exe /c` (CreateProcess cannot run batch scripts — see
// resolve_agent_bin). On platforms that are neither unix nor Windows,
// CreateAgentPane fails with a clean error.
// ---------------------------------------------------------------------------

/// Permission modes accepted by `claude --permission-mode` (2.1.201). Only
/// `manual` routes approval prompts over the control channel; the others are
/// passed through for operators who want unattended runs.
const AGENT_PERMISSION_MODES: [&str; 6] = [
    "manual",
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "dontAsk",
    "plan",
];

/// (T2) Environment variable overriding the `claude` binary agent panes spawn
/// (operator escape hatch; `agent_claude_bin` config wins when both are set).
#[cfg(any(unix, windows))]
const SGIAN_CLAUDE_BIN_ENV: &str = "SGIAN_CLAUDE_BIN";
const SGIAN_DROID_BIN_ENV: &str = "SGIAN_DROID_BIN";
/// Per-pane conversation log lives at `<data_dir>/agents/<pane-id>.jsonl`.
const AGENT_LOG_DIR: &str = "agents";
/// Conversation log cap, trimmed to HALF on overflow (hysteresis, like
/// scrollback) at a line boundary so every kept line stays parseable.
#[cfg(any(unix, windows))]
const AGENT_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Bootstrap replay bounds: last 1 MiB / 500 normalized events per agent pane.
const AGENT_REPLAY_MAX_BYTES: u64 = 1024 * 1024;
const AGENT_REPLAY_MAX_EVENTS: usize = 500;
/// Cap on a single SendAgentMessage text (and an approval's denial message).
#[cfg(any(unix, windows))]
const AGENT_MESSAGE_MAX_BYTES: usize = 256 * 1024;
/// A permission request denied automatically after this long without an
/// AgentApproval (the CLI blocks on the reply; unbounded waits wedge turns).
/// The wait re-checks child liveness every AGENT_APPROVAL_POLL (H1), so a
/// dead child is detected within ~1s rather than sitting out this timeout.
#[cfg(all(any(unix, windows), not(test)))]
const AGENT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
/// (T2) L5: tests can't sit out the production 10-minute timeout.
#[cfg(all(any(unix, windows), test))]
const AGENT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(2);
/// (T2) Increment of the bounded permission wait: each elapsed increment
/// re-checks child liveness so a CLI that died mid-permission unwedges the
/// pane's reader promptly instead of after the full timeout (H1).
#[cfg(any(unix, windows))]
const AGENT_APPROVAL_POLL: Duration = Duration::from_secs(1);
/// SIGTERM→SIGKILL escalation grace when closing an agent session. unix-only:
/// Windows kills are immediate (TerminateProcess), with nothing to escalate to.
#[cfg(unix)]
const AGENT_KILL_GRACE: Duration = Duration::from_secs(2);
/// A single stdout line longer than this is dropped (with an `error` event)
/// instead of buffering unboundedly against a broken/hostile child.
#[cfg(any(unix, windows))]
const AGENT_OUTPUT_LINE_MAX: usize = 4 * 1024 * 1024;
#[cfg(not(any(unix, windows)))]
const AGENT_UNSUPPORTED: &str =
    "agent panes are not supported on this platform (unix and windows only)";

/// (T2) Request-id sequence for daemon-initiated `interrupt` control requests
/// (ours must not collide with the CLI's own request ids).
#[cfg(any(unix, windows))]
static AGENT_INTERRUPT_SEQ: AtomicU64 = AtomicU64::new(1);

/// (T2) The agent-spawn half of Config, mirrored into the TerminalStore
/// alongside ShellConfig (and refreshed by the config file-watch with it).
/// Read only when spawning agent sessions (unix + Windows).
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
struct AgentSpawnConfig {
    /// Explicit provider binary overrides; None resolves via the matching
    /// SGIAN_*_BIN environment override and then PATH.
    claude_bin: Option<String>,
    droid_bin: Option<String>,
    permission_mode: String,
}

/// (T2) How the resolved `claude` binary must be spawned. Pure decision type,
/// unit-tested on every host (see plan_agent_bin).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentBinPlan {
    /// Spawn the binary directly (a unix path or bare PATH name, or a Windows
    /// .exe / explicit non-script path).
    Direct(String),
    /// A `.cmd`/`.bat` shim (npm installs `claude.cmd` on Windows):
    /// CreateProcess cannot execute batch scripts, so spawn it through
    /// `cmd.exe /c`. Produced only on Windows.
    ViaCmd(PathBuf),
    /// Nothing resolved (the Windows PATH probe found no claude.exe/.cmd/.bat).
    NotFound,
}

/// (T2) Pure: classify an explicit binary override (config/env). A `.cmd` or
/// `.bat` extension (case-insensitive) must go through cmd.exe; anything else
/// spawns directly.
fn classify_agent_bin(candidate: String) -> AgentBinPlan {
    let is_script = Path::new(&candidate)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    if is_script {
        AgentBinPlan::ViaCmd(PathBuf::from(candidate))
    } else {
        AgentBinPlan::Direct(candidate)
    }
}

/// (T2) Pure spawn-plan decision: an explicit override (config, then env —
/// resolved by the caller) wins, classified on Windows (`.cmd`/`.bat` shims
/// need cmd.exe) and spawned directly elsewhere. With no override, unix falls
/// back to the bare `claude` name (execvp searches PATH), while Windows must
/// probe PATH itself: CreateProcess does not search PATHEXT, so the
/// extensionless `claude` npm shim name never resolves to `claude.cmd`.
fn plan_agent_bin(
    candidate: Option<String>,
    probed: Option<AgentBinPlan>,
    windows: bool,
) -> AgentBinPlan {
    match candidate.filter(|bin| !bin.is_empty()) {
        Some(bin) if windows => classify_agent_bin(bin),
        Some(bin) => AgentBinPlan::Direct(bin),
        None if windows => probed.unwrap_or(AgentBinPlan::NotFound),
        None => AgentBinPlan::Direct("claude".to_string()),
    }
}

/// (T2) Pure: walk PATH directories in order looking for `claude.exe`,
/// `claude.cmd`, then `claude.bat` (per directory, in that precedence —
/// matching npm's installed shims). `exists` is injectable so tests exercise
/// the decision logic without a real Windows filesystem.
#[cfg(test)]
fn probe_agent_path(
    path_var: &std::ffi::OsStr,
    exists: impl Fn(&Path) -> bool,
) -> Option<AgentBinPlan> {
    probe_named_agent_path(path_var, "claude", exists)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn probe_named_agent_path(
    path_var: &std::ffi::OsStr,
    name: &str,
    exists: impl Fn(&Path) -> bool,
) -> Option<AgentBinPlan> {
    for dir in std::env::split_paths(path_var) {
        for suffix in ["exe", "cmd", "bat"] {
            let candidate = dir.join(format!("{name}.{suffix}"));
            if exists(&candidate) {
                return Some(classify_agent_bin(candidate.to_string_lossy().into_owned()));
            }
        }
    }
    None
}

/// (T2) Pure: the display form of a bin plan for the liveness `command`
/// string (matches how the process is actually spawned).
fn agent_bin_display(bin: &AgentBinPlan) -> String {
    match bin {
        AgentBinPlan::Direct(bin) => bin.clone(),
        AgentBinPlan::ViaCmd(script) => format!("cmd.exe /c \"{}\"", script.display()),
        AgentBinPlan::NotFound => "claude (not found)".to_string(),
    }
}

/// (T2) Pure: turn a resolved bin plan into the spawn program + full argv.
/// ViaCmd wraps the script in `cmd.exe /c` (CreateProcess cannot run batch
/// scripts); the script path stays a single argv element — std's Windows
/// command-line builder quotes it (and any spaced args) for cmd.exe, and with
/// claude's own flags always following there is no bare-quoted-string /c
/// parsing hazard. NotFound has no command; the caller maps it to the clean
/// CLI-not-found error before spawn.
fn agent_command_argv(bin: &AgentBinPlan, args: &[String]) -> Option<(String, Vec<String>)> {
    match bin {
        AgentBinPlan::Direct(bin) => Some((bin.clone(), args.to_vec())),
        AgentBinPlan::ViaCmd(script) => Some((
            "cmd.exe".to_string(),
            std::iter::once("/c".to_string())
                .chain([script.to_string_lossy().into_owned()])
                .chain(args.iter().cloned())
                .collect(),
        )),
        AgentBinPlan::NotFound => None,
    }
}

#[cfg(any(unix, windows))]
fn resolve_provider_bin(config: &AgentSpawnConfig, backend: AgentBackendKind) -> AgentBinPlan {
    let (configured, env_name, fallback) = match backend {
        AgentBackendKind::Claude => (&config.claude_bin, SGIAN_CLAUDE_BIN_ENV, "claude"),
        AgentBackendKind::Droid => (&config.droid_bin, SGIAN_DROID_BIN_ENV, "droid"),
    };
    let candidate = configured
        .as_ref()
        .filter(|bin| !bin.is_empty())
        .cloned()
        .or_else(|| std::env::var(env_name).ok().filter(|bin| !bin.is_empty()));
    #[cfg(windows)]
    let probed = std::env::var_os("PATH")
        .and_then(|path| probe_named_agent_path(&path, fallback, |p| p.is_file()));
    #[cfg(unix)]
    let probed = None;
    match plan_agent_bin(candidate, probed, cfg!(windows)) {
        AgentBinPlan::Direct(bin) if bin == "claude" && fallback != "claude" => {
            AgentBinPlan::Direct(fallback.to_string())
        }
        plan => plan,
    }
}

/// (T2) State shared between an agent session (daemon side) and its reader
/// thread, behind its own lock so the reader never touches the store lock.
/// Lock order: terminals → shared, never reversed.
#[cfg(any(unix, windows))]
#[derive(Default)]
struct AgentShared {
    /// The CLI's session id from its init event, recorded for `--resume`.
    session_id: Option<String>,
    /// Pending permission requests: request_id → reply channel. The reader
    /// blocks on the receiver until an AgentApproval sends the decision, the
    /// wait times out, the child dies (H1), or the session closes (deny).
    pending: HashMap<String, SyncSender<AgentApprovalDecision>>,
    /// One in-flight turn per pane: set by SendAgentMessage, cleared when the
    /// reader sees the turn's `result`, when the process exits, or when an
    /// interrupt is sent (M2 — a lost `result` line must not wedge the pane).
    turn_running: bool,
}

/// (T2) The daemon's answer to a permission request.
#[cfg(any(unix, windows))]
struct AgentApprovalDecision {
    allow: bool,
    message: Option<String>,
    /// Why the request resolved, echoed as the emitted `permission_resolved`
    /// event's `reason`: "user" | "timeout" | "closed" | "process_exit".
    reason: &'static str,
}

/// (T2) Deny every pending permission request (session close/process exit):
/// a reader blocked waiting for its decision wakes and can observe EOF
/// instead of sitting out the full approval timeout. `reason` is the wire
/// `permission_resolved` reason ("closed" | "process_exit"); the CLI-facing
/// deny message is derived from it.
#[cfg(any(unix, windows))]
fn deny_agent_pending(shared: &Arc<Mutex<AgentShared>>, reason: &'static str) {
    let message = match reason {
        "closed" => "agent session closed",
        _ => "agent process exited",
    };
    let pending = shared
        .lock()
        .map(|mut shared| std::mem::take(&mut shared.pending))
        .unwrap_or_default();
    for (_, sender) in pending {
        let _ = sender.send(AgentApprovalDecision {
            allow: false,
            message: Some(message.to_string()),
            reason,
        });
    }
}

/// (T2) One in-flight turn per pane: begin a turn or report it busy. The CLI
/// protocol has no way to interleave a second user message mid-turn, and a
/// client-side queue would reorder user text against permission prompts.
#[cfg(any(unix, windows))]
fn agent_try_begin_turn(shared: &Arc<Mutex<AgentShared>>) -> bool {
    shared
        .lock()
        .map(|mut shared| {
            if shared.turn_running {
                false
            } else {
                shared.turn_running = true;
                true
            }
        })
        .unwrap_or(false)
}

#[cfg(any(unix, windows))]
fn agent_end_turn(shared: &Arc<Mutex<AgentShared>>) -> bool {
    if let Ok(mut shared) = shared.lock() {
        let was_running = shared.turn_running;
        shared.turn_running = false;
        return was_running;
    }
    false
}

/// (T2) unix: SIGTERM-then-SIGKILL handle for the agent CLI, mirroring
/// `TerminalSession::killer`. The child itself is shared with the reader
/// thread (which reaps it), so kills go through the pid; the `reaped` flag
/// keeps a stale kill from signalling a REUSED pid after the child was
/// already reaped.
#[cfg(unix)]
struct AgentChildKiller {
    pid: u32,
    reaped: Arc<AtomicBool>,
}

#[cfg(unix)]
impl AgentChildKiller {
    fn kill(&self) {
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
        let pid = self.pid;
        let reaped = Arc::clone(&self.reaped);
        thread::spawn(move || {
            thread::sleep(AGENT_KILL_GRACE);
            if !reaped.load(Ordering::SeqCst) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        });
    }
}

/// (T2) Windows: no POSIX signals exist to mirror the unix grace escalation,
/// so the kill is a direct `Child::kill()` (TerminateProcess — immediate).
/// Going through the shared `Child` handle keeps kills handle-based: no
/// pid-reuse hazard, so no `reaped`-style guard is needed (double-kills are
/// harmless no-ops on an exited process). (T2) KNOWN DELTA: TerminateProcess
/// kills only the DIRECT child — claude is a node app, and via the .cmd shim
/// the direct child is cmd.exe, so grandchildren (node) may outlive the
/// session and hold its pipes open. Job-Object tree-kill is out of scope.
#[cfg(windows)]
struct AgentChildKiller {
    child: Arc<Mutex<std::process::Child>>,
}

#[cfg(windows)]
impl AgentChildKiller {
    fn kill(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// (T2) A live agent session: the stdin writer queue (same writer-thread
/// pattern as PTY input — no blocking pipe write under the store lock), the
/// child killer, and the shared state. Dropping it (close/restart/shutdown)
/// denies pending approvals and kills the CLI (SIGTERM→SIGKILL on unix,
/// TerminateProcess on Windows).
#[cfg(any(unix, windows))]
struct AgentSession {
    backend: AgentBackendKind,
    input: SyncSender<Vec<u8>>,
    killer: AgentChildKiller,
    shared: Arc<Mutex<AgentShared>>,
    events: Arc<Mutex<AgentEventLog>>,
}

#[cfg(any(unix, windows))]
impl AgentSession {
    fn kill(&self) {
        self.killer.kill();
    }
}

#[cfg(any(unix, windows))]
impl Drop for AgentSession {
    fn drop(&mut self) {
        deny_agent_pending(&self.shared, "closed");
        self.killer.kill();
    }
}

/// (T2) Queue one JSON line to the agent CLI's stdin writer thread (mirrors
/// `queue_pane_input`, with agent-worded errors). Every write is exactly one
/// line: user messages, control_responses, and interrupt requests.
#[cfg(any(unix, windows))]
fn queue_agent_stdin(input: &SyncSender<Vec<u8>>, pane_id: &str, line: &str) -> Result<(), String> {
    let mut bytes = line.as_bytes().to_vec();
    bytes.push(b'\n');
    match input.try_send(bytes) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(format!(
            "agent stdin backlogged (CLI is not reading): {pane_id}"
        )),
        Err(TrySendError::Disconnected(_)) => Err(format!("agent session ended: {pane_id}")),
    }
}

/// (T2) Everything an agent spawn needs out of the store, read under the lock
/// so the fork/exec runs WITHOUT it (M7), same contract as `SpawnPlan`.
#[cfg(any(unix, windows))]
struct AgentSpawnPlan {
    backend: AgentBackendKind,
    bin: AgentBinPlan,
    args: Vec<String>,
    env: HashMap<String, String>,
    scrub_env: Vec<String>,
    cwd: PathBuf,
    command_str: String,
    cwd_str: String,
    /// Provider-specific first protocol request, queued immediately after the
    /// stdin writer starts. Claude initializes itself; Droid JSON-RPC needs an
    /// explicit initialize/load request.
    initial_input: Option<String>,
}

/// (T2) A spawned-but-not-yet-committed agent child (M7), mirroring
/// `PreparedSpawn`: produced off-lock by `execute_agent_spawn`, committed
/// under the lock by `TerminalStore::commit_agent_spawn`.
#[cfg(any(unix, windows))]
struct PreparedAgentSpawn {
    backend: AgentBackendKind,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    stderr: std::process::ChildStderr,
    command_str: String,
    cwd_str: String,
    initial_input: Option<String>,
}

/// (T2) The expensive half of an agent spawn — process fork/exec with piped
/// stdio — run WITHOUT the store lock (M7). Environment handling matches
/// `execute_spawn`: the daemon's inherited env, minus the config scrub list,
/// plus explicit `env` entries (compute_spawn_env semantics; no TERM/COLORTERM
/// — there is no terminal).
#[cfg(any(unix, windows))]
fn execute_agent_spawn(plan: &AgentSpawnPlan) -> Result<PreparedAgentSpawn, String> {
    let provider = plan.backend.as_str();
    let Some((program, argv)) = agent_command_argv(&plan.bin, &plan.args) else {
        // Windows PATH probe found no provider executable/shim (an explicit
        // config/env override skips the probe and fails at spawn instead).
        return Err(format!(
            "{provider} CLI not found on PATH (configure its agent binary override)"
        ));
    };
    let mut command = Command::new(&program);
    command.args(&argv);
    command.current_dir(&plan.cwd);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // (T2) Windows: the daemon (GUI subsystem) has no console, so a
    // console-subsystem child (cmd.exe / node) would otherwise pop a visible
    // console window per agent pane. Piped stdio is unaffected by the flag.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    for key in INHERITED_SESSION_MARKERS {
        command.env_remove(key);
    }
    for key in &plan.scrub_env {
        command.env_remove(key);
    }
    for (key, value) in &plan.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "{provider} CLI not found: '{}' is not executable or not on PATH",
                program,
            )
        } else {
            format!("failed to spawn agent CLI '{}': {error}", program)
        }
    })?;
    // Partial-failure cleanup mirrors execute_spawn: a child whose pipes
    // could not be taken is killed + reaped, never abandoned.
    let (stdin, stdout, stderr) =
        match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("failed to open agent CLI pipes".to_string());
            }
        };
    Ok(PreparedAgentSpawn {
        backend: plan.backend,
        child,
        stdin,
        stdout,
        stderr,
        command_str: plan.command_str.clone(),
        cwd_str: plan.cwd_str.clone(),
        initial_input: plan.initial_input.clone(),
    })
}

// ---------------------------------------------------------------------------
// (T2) stream-json normalization: raw CLI lines → `kind`-tagged agent events
// ---------------------------------------------------------------------------

/// (T2) Normalize one parsed stream-json object from the `claude` CLI into
/// zero or more `kind`-tagged events (the `DaemonEvent::AgentEvent` payload
/// shape). Unknown/irrelevant line types (rate_limit_event, thinking_tokens,
/// control_responses to our own requests, …) normalize to nothing. See the
/// T2 header comment for the probed protocol shapes.
#[cfg(any(unix, windows))]
fn normalize_agent_event(raw: &Value) -> Vec<Value> {
    let Some(event_type) = raw.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match event_type {
        "system" if raw.get("subtype").and_then(Value::as_str) == Some("init") => {
            let mut event = json!({"kind": "session"});
            if let Some(session_id) = raw.get("session_id").and_then(Value::as_str) {
                event["session_id"] = json!(session_id);
            }
            if let Some(model) = raw.get("model").and_then(Value::as_str) {
                event["model"] = json!(model);
            }
            vec![event]
        }
        "stream_event" => normalize_agent_stream_event(raw),
        "assistant" => normalize_agent_assistant(raw),
        "user" => normalize_agent_tool_results(raw),
        "result" => vec![normalize_agent_result(raw)],
        "control_request" => normalize_agent_control_request(raw),
        _ => Vec::new(),
    }
}

/// Normalize a provider's wire protocol into the stable chat event contract.
/// Claude keeps its existing stream-json path; Droid's long-lived JSON-RPC
/// notifications are translated at this seam.
#[cfg(any(unix, windows))]
fn normalize_provider_agent_event(backend: AgentBackendKind, raw: &Value) -> Vec<Value> {
    match backend {
        AgentBackendKind::Claude => normalize_agent_event(raw),
        AgentBackendKind::Droid => normalize_droid_agent_event(raw),
    }
}

#[cfg(any(unix, windows))]
fn normalize_droid_agent_event(raw: &Value) -> Vec<Value> {
    if let Some(error) = raw.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Droid JSON-RPC request failed");
        return vec![
            json!({"kind": "error", "message": message}),
            json!({"kind": "turn_complete", "subtype": "error_during_execution"}),
        ];
    }

    // initialize_session responses carry the durable Factory session id.
    if let Some(session_id) = raw.pointer("/result/sessionId").and_then(Value::as_str) {
        let mut event = json!({"kind": "session", "session_id": session_id});
        if let Some(model) = raw
            .pointer("/result/settings/modelId")
            .and_then(Value::as_str)
        {
            event["model"] = json!(model);
        }
        return vec![event];
    }

    if raw.get("method").and_then(Value::as_str) == Some("droid.request_permission") {
        let Some(request_id) = raw.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let tool_use = raw
            .pointer("/params/toolUses/0/toolUse")
            .cloned()
            .unwrap_or(Value::Null);
        let name = tool_use
            .get("name")
            .or_else(|| tool_use.get("toolName"))
            .cloned()
            .unwrap_or_else(|| json!("tool"));
        let input = tool_use
            .get("input")
            .or_else(|| tool_use.get("toolInput"))
            .cloned()
            .unwrap_or(Value::Null);
        return vec![json!({
            "kind": "permission_request",
            "request_id": request_id,
            "tool_name": name,
            "input": input,
        })];
    }

    if raw.get("method").and_then(Value::as_str) == Some("droid.ask_user") {
        return vec![json!({
            "kind": "error",
            "message": "Droid requested structured user input; this Sgian build supports tool approvals only",
        })];
    }

    if raw.get("method").and_then(Value::as_str) != Some("droid.session_notification") {
        return Vec::new();
    }
    let Some(notification) = raw.pointer("/params/notification") else {
        return Vec::new();
    };
    match notification.get("type").and_then(Value::as_str) {
        Some("assistant_text_delta") => notification
            .get("textDelta")
            .and_then(Value::as_str)
            .map(|text| vec![json!({"kind": "text_delta", "text": text})])
            .unwrap_or_default(),
        Some("assistant_text_complete") => vec![json!({"kind": "message_complete"})],
        Some("tool_call") => {
            let tool_use = notification.get("toolUse").cloned().unwrap_or(Value::Null);
            vec![json!({
                "kind": "tool_use",
                "id": tool_use.get("id").or_else(|| tool_use.get("toolUseId")).cloned().unwrap_or(Value::Null),
                "name": tool_use.get("name").or_else(|| tool_use.get("toolName")).cloned().unwrap_or_else(|| json!("tool")),
                "input": tool_use.get("input").or_else(|| tool_use.get("toolInput")).cloned().unwrap_or(Value::Null),
            })]
        }
        Some("tool_result") => vec![json!({
            "kind": "tool_result",
            "tool_use_id": notification.get("toolUseId").cloned().unwrap_or(Value::Null),
            "content": notification.get("content").cloned().unwrap_or(Value::Null),
            "is_error": notification.get("isError").and_then(Value::as_bool).unwrap_or(false),
        })],
        Some("error") => vec![json!({
            "kind": "error",
            "message": notification.get("message").cloned().unwrap_or_else(|| json!("Droid error")),
        })],
        Some("droid_working_state_changed")
            if notification.get("newState").and_then(Value::as_str) == Some("idle") =>
        {
            vec![json!({"kind": "turn_complete", "subtype": "success"})]
        }
        _ => Vec::new(),
    }
}

/// (T2) `stream_event` partials (Anthropic raw stream): text comes from these
/// deltas — NOT from the complete `assistant` message — so the GUI can render
/// as the model streams. Probed stable on 2.1.201 with
/// --include-partial-messages; if a future CLI stops emitting partials, drop
/// the flag and synthesize text from complete `assistant` text blocks here.
#[cfg(any(unix, windows))]
fn normalize_agent_stream_event(raw: &Value) -> Vec<Value> {
    let Some(inner) = raw.get("event") else {
        return Vec::new();
    };
    match inner.get("type").and_then(Value::as_str) {
        Some("message_start") => vec![json!({"kind": "message_start", "role": "assistant"})],
        Some("content_block_delta") => {
            let delta = inner.get("delta").cloned().unwrap_or(Value::Null);
            if delta.get("type").and_then(Value::as_str) == Some("text_delta") {
                if let Some(text) = delta.get("text").and_then(Value::as_str) {
                    return vec![json!({"kind": "text_delta", "text": text})];
                }
            }
            Vec::new()
        }
        Some("message_stop") => vec![json!({"kind": "message_complete"})],
        _ => Vec::new(),
    }
}

/// (T2) Complete assistant messages: the source for `tool_use` (the block's
/// `input` is only fully formed here; partials carry it as JSON fragments).
/// Text blocks are skipped — text already streamed via text_delta partials.
#[cfg(any(unix, windows))]
fn normalize_agent_assistant(raw: &Value) -> Vec<Value> {
    let Some(content) = raw.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .map(|block| {
            json!({
                "kind": "tool_use",
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "name": block.get("name").cloned().unwrap_or(Value::Null),
                "input": block.get("input").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// (T2) `user` messages carry tool results. `content` passes through verbatim
/// (string or block array — the CLI emits both shapes); `is_error` defaults
/// false (absent on older shapes).
#[cfg(any(unix, windows))]
fn normalize_agent_tool_results(raw: &Value) -> Vec<Value> {
    let Some(content) = raw.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|block| {
            json!({
                "kind": "tool_result",
                "tool_use_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                "content": block.get("content").cloned().unwrap_or(Value::Null),
                "is_error": block.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// (T2) `result` ends a turn. `subtype` passes through verbatim
/// ("success"/"error_during_execution"/… — an interrupt lands as
/// error_during_execution, which the GUI may want to tell apart from a hard
/// error); `total_cost_usd` maps to the contract's `cost_usd`. Only fields
/// the CLI actually sent are included.
#[cfg(any(unix, windows))]
fn normalize_agent_result(raw: &Value) -> Value {
    let mut event = json!({"kind": "turn_complete"});
    for key in ["subtype", "is_error", "result", "duration_ms", "num_turns"] {
        if let Some(value) = raw.get(key) {
            event[key] = value.clone();
        }
    }
    if let Some(usage) = raw.get("usage") {
        event["usage"] = usage.clone();
    }
    if let Some(cost) = raw.get("total_cost_usd") {
        event["cost_usd"] = cost.clone();
    }
    event
}

/// (T2) `can_use_tool` control requests become `permission_request` events;
/// the reader registers the reply channel and blocks for the AgentApproval.
/// Other control_request subtypes (none observed from the CLI itself on
/// 2.1.201) are dropped — an event without a request_id can never be
/// answered, so it is dropped too.
#[cfg(any(unix, windows))]
fn normalize_agent_control_request(raw: &Value) -> Vec<Value> {
    let Some(request) = raw.get("request") else {
        return Vec::new();
    };
    if request.get("subtype").and_then(Value::as_str) != Some("can_use_tool") {
        return Vec::new();
    }
    let Some(request_id) = raw
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Vec::new();
    };
    vec![json!({
        "kind": "permission_request",
        "request_id": request_id,
        "tool_name": request.get("tool_name").cloned().unwrap_or(Value::Null),
        "input": request.get("input").cloned().unwrap_or(Value::Null),
    })]
}

/// (T2) Read one line (newline-terminated or the EOF tail) from the agent
/// CLI's stdout into `out`, bounded: a line longer than `cap` is TRUNCATED at
/// the cap and flagged `true` (the remainder is consumed up to the newline),
/// so a broken/hostile child cannot grow daemon memory without bound.
/// Returns Ok(None) at clean EOF (no bytes read).
#[cfg(any(unix, windows))]
fn read_capped_line<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Option<bool>> {
    out.clear();
    let mut truncated = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if out.is_empty() && !truncated {
                Ok(None)
            } else {
                Ok(Some(truncated))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map(|index| index + 1).unwrap_or(available.len());
        if !truncated {
            let room = cap.saturating_sub(out.len());
            if take > room {
                out.extend_from_slice(&available[..room]);
                truncated = true;
            } else {
                out.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(truncated));
        }
    }
}

// ---------------------------------------------------------------------------
// (T2) Conversation log persistence (`<data_dir>/agents/<pane-id>.jsonl`)
// ---------------------------------------------------------------------------

/// (T2) The normalized event stream is appended to a per-pane JSONL log: the
/// bootstrap replay reads it back, and it survives daemon restarts and
/// in-place pane restarts (a restarted pane resumes its CLI session, so the
/// log remains its conversation history). It is DELETED when the pane is
/// closed (M3, scrollback parity — a closed pane can never resume: its
/// agents_v2/resume seeds are dropped with it).
fn agent_log_path(agents_dir: &Path, pane_id: &str) -> PathBuf {
    agents_dir.join(format!("{pane_id}.jsonl"))
}

/// (T2) Drop `.jsonl.tmp` cap litter at daemon start (mirror of the
/// scrollback temp sweep): no cap can be in flight before the daemon serves.
fn prune_agent_log_temps(agents_dir: &Path) {
    let Ok(entries) = fs::read_dir(agents_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_cap_temp = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".jsonl.tmp"));
        if is_cap_temp {
            let _ = fs::remove_file(&path);
        }
    }
}

/// (T2) Remove agent conversation logs for panes no longer in the registry
/// (M3, mirror of prune_orphan_scrollback). `.jsonl.tmp` cap litter is not
/// touched here: startup temp pruning lives in prune_agent_log_temps, and a
/// runtime sweep must never delete a temp under an in-flight cap.
fn prune_orphan_agent_logs(agents_dir: &Path, live_pane_ids: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(agents_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
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

/// (T2) Best-effort appender for a pane's conversation log. Owned by the
/// pane's reader thread (the log's single writer), so no locking; every
/// failure mode degrades to "no log" rather than killing the event stream.
#[cfg(any(unix, windows))]
struct AgentLogWriter {
    file: File,
    len: u64,
    pane_id: String,
    agents_dir: PathBuf,
}

#[cfg(any(unix, windows))]
impl AgentLogWriter {
    fn open(agents_dir: &Path, pane_id: &str) -> Option<Self> {
        // Defense in depth (same rule as scrollback): pane ids become file
        // paths, so never open one that isn't the canonical `pane-<n>` shape.
        if !is_valid_pane_id(pane_id) {
            return None;
        }
        let file = open_scrollback_append(&agent_log_path(agents_dir, pane_id)).ok()?;
        let len = file.metadata().ok()?.len();
        Some(Self {
            file,
            len,
            pane_id: pane_id.to_string(),
            agents_dir: agents_dir.to_path_buf(),
        })
    }

    fn append_line(&mut self, line: &str) {
        // (T2) L3: ONE write of line+\n — two separate write_all calls could
        // interleave with another handle appending to the same file (e.g. a
        // superseded reader draining during a respawn).
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        if self.file.write_all(&bytes).is_err() {
            return;
        }
        self.len += bytes.len() as u64;
        if self.len > AGENT_LOG_MAX_BYTES {
            // Cap rewrites the file (temp + rename); re-open and re-prime the
            // byte count afterwards (same dance as append_scrollback, but
            // single-writer so no state lock is needed).
            if cap_agent_log_file(&self.agents_dir, &self.pane_id).is_ok() {
                if let Ok(file) =
                    open_scrollback_append(&agent_log_path(&self.agents_dir, &self.pane_id))
                {
                    self.len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                    self.file = file;
                }
            }
        }
    }
}

/// (T2) Cap a conversation log that grew past AGENT_LOG_MAX_BYTES, trimming
/// to HALF the cap (hysteresis — same rationale as scrollback) and starting
/// the kept tail just past a newline so every surviving line stays parseable.
#[cfg(any(unix, windows))]
fn cap_agent_log_file(agents_dir: &Path, pane_id: &str) -> Result<(), String> {
    let path = agent_log_path(agents_dir, pane_id);
    let Ok(metadata) = fs::metadata(&path) else {
        return Ok(());
    };
    if metadata.len() <= AGENT_LOG_MAX_BYTES {
        return Ok(());
    }
    let mut file =
        File::open(&path).map_err(|error| format!("failed to open agent log for cap: {error}"))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|error| format!("failed to read agent log for cap: {error}"))?;
    if data.len() as u64 <= AGENT_LOG_MAX_BYTES {
        return Ok(());
    }
    let target = (AGENT_LOG_MAX_BYTES / 2).max(1) as usize;
    let keep_from = data.len().saturating_sub(target);
    let keep_from = utf8_boundary_at_or_after(&data, keep_from);
    let keep_from = data[keep_from..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map(|newline| keep_from + newline + 1)
        .filter(|&start| start < data.len())
        .unwrap_or(keep_from);
    let temp_path = path.with_extension("jsonl.tmp");
    write_file_atomic(&temp_path, &path, &data[keep_from..], false)
}

/// (T2) Read the bounded conversation replay for a pane: the tail of its
/// JSONL log (at most `max_bytes`, seeking rather than reading the whole
/// file), parsed into normalized events and capped at the last `max_events`.
/// Malformed lines (a crash-torn tail, hand edits) are skipped, not fatal.
fn read_agent_log_tail(
    agents_dir: &Path,
    pane_id: &str,
    max_bytes: u64,
    max_events: usize,
) -> Vec<Value> {
    if !is_valid_pane_id(pane_id) {
        return Vec::new();
    }
    let Some(mut file) = File::open(agent_log_path(agents_dir, pane_id)).ok() else {
        return Vec::new();
    };
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return Vec::new();
    };
    if len > max_bytes && file.seek(SeekFrom::Start(len - max_bytes)).is_err() {
        return Vec::new();
    }
    let mut data = Vec::new();
    if file.read_to_end(&mut data).is_err() {
        return Vec::new();
    }
    // A seek into the tail can land mid-line; drop the first partial line.
    let start = if len > max_bytes {
        data.iter()
            .position(|&byte| byte == b'\n')
            .map(|newline| newline + 1)
            .unwrap_or(data.len())
    } else {
        0
    };
    let mut events: Vec<Value> = data[start..]
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|event| event.get("kind").is_some())
        .collect();
    if events.len() > max_events {
        events.drain(..events.len() - max_events);
    }
    events
}

/// (T2) The seq a pane's next event should continue from: the highest seq
/// already persisted in its conversation log (0 when there is none). The
/// per-pane sequence keeps increasing across session respawns and daemon
/// restarts, so clients can dedupe replay-vs-live by dropping `seq` values
/// at or below the last replayed one.
#[cfg(any(unix, windows))]
fn agent_log_last_seq(agents_dir: &Path, pane_id: &str) -> u64 {
    read_agent_log_tail(agents_dir, pane_id, AGENT_REPLAY_MAX_BYTES, 1)
        .last()
        .and_then(|event| event.get("seq").and_then(Value::as_u64))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// (T2) Reader thread + store integration
// ---------------------------------------------------------------------------

/// (T2) Pieces the agent reader thread owns: the child (for reaping), its
/// stdout, a stdin queue handle for control_responses, and the shared/liveness
/// arcs — mirroring the PTY reader's ownership split. The child is shared
/// with the session's killer via Arc<Mutex>: the Windows killer needs the
/// handle for TerminateProcess (unix kills go through the pid instead and
/// never touch the mutex). The reader holds the child lock across try_wait/
/// wait; a blocked wait is only ever entered when the child is already
/// exiting (stdout EOF, or a kill that preceded superseding), so the
/// Windows killer's brief lock can never wedge behind it in practice.
#[cfg(any(unix, windows))]
struct AgentReaderCtx {
    pane_id: String,
    backend: AgentBackendKind,
    generation: u64,
    child: Arc<Mutex<std::process::Child>>,
    stdout: std::process::ChildStdout,
    input: SyncSender<Vec<u8>>,
    shared: Arc<Mutex<AgentShared>>,
    liveness: Arc<Mutex<HashMap<String, PaneLiveness>>>,
    router: OutputRouter,
    events: Arc<Mutex<AgentEventLog>>,
    reaped: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
}

/// (T2) Append a normalized event to the pane's JSONL log (best-effort) and
/// broadcast it to subscribers, in that order so the persisted replay is
/// always at least as complete as what clients saw. Every event is stamped
/// with the pane's monotonic `seq` (contract: per-pane u64 from 1, seeded
/// from the persisted log so it survives respawns/restarts) — the SAME
/// stamped object goes to the log and the broadcast, so the bootstrap replay
/// carries identical objects.
#[cfg(any(unix, windows))]
fn append_and_emit_agent_event(
    router: &OutputRouter,
    events: &Mutex<AgentEventLog>,
    pane_id: &str,
    event: Value,
) {
    if let Ok(mut events) = events.lock() {
        events.emit(router, pane_id, event);
    }
}

#[cfg(any(unix, windows))]
struct AgentEventLog {
    log: Option<AgentLogWriter>,
    next_seq: u64,
}

#[cfg(any(unix, windows))]
impl AgentEventLog {
    fn emit(&mut self, router: &OutputRouter, pane_id: &str, mut event: Value) {
        self.next_seq += 1;
        event["seq"] = json!(self.next_seq);
        if let Some(log) = self.log.as_mut() {
            if let Ok(line) = serde_json::to_string(&event) {
                log.append_line(&line);
            }
        }
        router.emit_agent_event(pane_id, event);
    }
}

/// (T2) Non-blocking "has the CLI exited?" through the shared child handle
/// (the Windows killer may hold the lock briefly for TerminateProcess).
#[cfg(any(unix, windows))]
fn agent_child_exited(child: &Arc<Mutex<std::process::Child>>) -> bool {
    child
        .lock()
        .map(|mut child| matches!(child.try_wait(), Ok(Some(_))))
        .unwrap_or(false)
}

/// (T2) Reader-side permission round-trip: the event is emitted first, then
/// the reader waits on the reply channel — in bounded AGENT_APPROVAL_POLL
/// increments, re-checking child liveness each increment (H1) — until an
/// AgentApproval arrives, the cumulative wait hits AGENT_APPROVAL_TIMEOUT
/// (deny), the child dies (deny, "process_exit"), or the session closes
/// (deny, via `deny_agent_pending`). While blocked the reader reads nothing
/// further — fine, because the CLI is itself blocked waiting for this answer
/// (probe c). Every resolution is emitted + logged as a `permission_resolved`
/// event (contract: replay must cancel the permission card too).
#[cfg(any(unix, windows))]
#[allow(clippy::too_many_arguments)]
fn agent_handle_permission_request(
    backend: AgentBackendKind,
    router: &OutputRouter,
    events: &Mutex<AgentEventLog>,
    pane_id: &str,
    input: &SyncSender<Vec<u8>>,
    shared: &Arc<Mutex<AgentShared>>,
    child: &Arc<Mutex<std::process::Child>>,
    reaped: &AtomicBool,
    event: Value,
) {
    let request_id = event
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // (T2) L7: an input-less request gets an allow reply WITHOUT the
    // updatedInput field — a present-but-null input must not echo `null`.
    let tool_input = event.get("input").filter(|input| !input.is_null()).cloned();
    let (sender, receiver) = sync_channel::<AgentApprovalDecision>(1);
    if let Ok(mut shared) = shared.lock() {
        shared.pending.insert(request_id.clone(), sender);
    }
    append_and_emit_agent_event(router, events, pane_id, event);

    let deadline = Instant::now() + AGENT_APPROVAL_TIMEOUT;
    let decision = loop {
        match receiver.recv_timeout(AGENT_APPROVAL_POLL) {
            Ok(decision) => break decision,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // (T2) H1: a dead child can never observe our answer — deny
                // the request and break out so the reader proceeds to
                // EOF/reap/liveness update instead of wedging the pane for
                // the full approval timeout. (Still running, or liveness
                // unknown: keep waiting.)
                if agent_child_exited(child) {
                    // Reaped here; disarm the kill escalation now (the
                    // EOF path stores it again, idempotently).
                    reaped.store(true, Ordering::SeqCst);
                    break AgentApprovalDecision {
                        allow: false,
                        message: Some("agent process exited".to_string()),
                        reason: "process_exit",
                    };
                }
                if Instant::now() >= deadline {
                    break AgentApprovalDecision {
                        allow: false,
                        message: Some(
                            "permission request timed out waiting for approval".to_string(),
                        ),
                        reason: "timeout",
                    };
                }
            }
            // All senders dropped without a decision: the session is gone.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break AgentApprovalDecision {
                    allow: false,
                    message: Some("agent session closed".to_string()),
                    reason: "closed",
                };
            }
        }
    };
    if let Ok(mut shared) = shared.lock() {
        shared.pending.remove(&request_id);
    }

    // The resolution is an event too (contract): a replayed permission card
    // is cancelled by the replayed `permission_resolved`.
    append_and_emit_agent_event(
        router,
        events,
        pane_id,
        json!({
            "kind": "permission_resolved",
            "request_id": request_id,
            "behavior": if decision.allow { "allow" } else { "deny" },
            "reason": decision.reason,
        }),
    );

    // Reply shape pinned by probe c: allow echoes the original input back as
    // updatedInput (omitted when the request carried none — L7); deny carries
    // the operator's feedback message to the model.
    let response = match backend {
        AgentBackendKind::Claude if decision.allow => {
            let mut behavior = json!({"behavior": "allow"});
            if let Some(tool_input) = tool_input {
                behavior["updatedInput"] = tool_input;
            }
            json!({
                "type": "control_response",
                "response": {
                    "request_id": request_id,
                    "subtype": "success",
                    "response": behavior,
                },
            })
        }
        AgentBackendKind::Claude => json!({
            "type": "control_response",
            "response": {
                "request_id": request_id,
                "subtype": "success",
                "response": {
                    "behavior": "deny",
                    "message": decision
                        .message
                        .unwrap_or_else(|| "denied by user".to_string()),
                },
            },
        }),
        AgentBackendKind::Droid => {
            let mut result = json!({
                "selectedOption": if decision.allow { "proceed_once" } else { "cancel" },
            });
            if let Some(message) = decision.message {
                result["comment"] = json!(message);
            }
            json!({
                "jsonrpc": "2.0",
                "factoryApiVersion": "1.0.0",
                "type": "response",
                "id": request_id,
                "result": result,
            })
        }
    };
    if let Ok(line) = serde_json::to_string(&response) {
        // Best-effort: the process may already be gone (close/kill raced us).
        let _ = queue_agent_stdin(input, pane_id, &line);
    }
}

/// (T2) The agent reader thread: line-delimited JSON on stdout → normalize →
/// append to the conversation log + broadcast `AgentEvent`. State side effects
/// (session id, turn completion, permission round-trips) ride the SAME
/// normalized events so the log, the broadcast, and live state can never
/// diverge. Exit handling mirrors the PTY reader: generation-checked single
/// claim, then process_exit + PaneEnded.
#[cfg(any(unix, windows))]
fn agent_reader_main(ctx: AgentReaderCtx) {
    let AgentReaderCtx {
        pane_id,
        backend,
        generation,
        child,
        stdout,
        input,
        shared,
        liveness,
        router,
        events,
        reaped,
        dirty,
    } = ctx;

    // Same ownership rule as the PTY reader: a superseded (restarted/closed)
    // generation must not emit into the replacement session's stream.
    let owns_session = || {
        liveness
            .lock()
            .ok()
            .map(|map| {
                map.get(&pane_id)
                    .is_some_and(|entry| entry.generation == generation)
            })
            .unwrap_or(false)
    };

    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        let truncated = match read_capped_line(&mut reader, &mut buffer, AGENT_OUTPUT_LINE_MAX) {
            Ok(Some(truncated)) => truncated,
            Ok(None) => break,
            Err(_) => break,
        };
        if !owns_session() {
            // Superseded: still reap (M5) and disarm the kill escalation.
            if let Ok(mut child) = child.lock() {
                let _ = child.wait();
            }
            reaped.store(true, Ordering::SeqCst);
            return;
        }
        if truncated {
            tracing::warn!(
                pane_id = %pane_id,
                event = "agent_line_oversized",
                "oversized agent output line dropped"
            );
            append_and_emit_agent_event(
                &router,
                &events,
                &pane_id,
                json!({"kind": "error", "message": "oversized agent output line dropped"}),
            );
            continue;
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw: Value = match serde_json::from_str(line) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(
                    pane_id = %pane_id,
                    event = "agent_line_malformed",
                    %error,
                    "malformed agent output line: {}",
                    &line[..line.len().min(200)]
                );
                continue;
            }
        };
        if backend == AgentBackendKind::Droid
            && raw.get("method").and_then(Value::as_str) == Some("droid.ask_user")
        {
            if let Some(request_id) = raw.get("id").and_then(Value::as_str) {
                let response = json!({
                    "jsonrpc": "2.0",
                    "factoryApiVersion": "1.0.0",
                    "type": "response",
                    "id": request_id,
                    "result": {"cancelled": true, "answers": []},
                });
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = queue_agent_stdin(&input, &pane_id, &line);
                }
            }
        }
        for event in normalize_provider_agent_event(backend, &raw) {
            match event.get("kind").and_then(Value::as_str).unwrap_or("") {
                "session" => {
                    if let Some(session_id) = event.get("session_id").and_then(Value::as_str) {
                        let mut changed = false;
                        if let Ok(mut shared) = shared.lock() {
                            if shared.session_id.as_deref() != Some(session_id) {
                                shared.session_id = Some(session_id.to_string());
                                changed = true;
                            }
                        }
                        // Flag a NEW session id for lazy persist: agents_v2
                        // must reach workspace.json within one cadence (a crash
                        // before then would lose the resume). The CLI re-inits
                        // every turn with the SAME id — only a change marks.
                        if changed {
                            dirty.store(true, Ordering::SeqCst);
                        }
                    }
                }
                "turn_complete" => {
                    // Droid emits an idle notification after initialization.
                    // It is only a turn boundary when SendAgentMessage marked
                    // a turn in flight; otherwise suppress the empty replay
                    // event (and avoid clearing a just-queued first turn).
                    let was_running = agent_end_turn(&shared);
                    if backend == AgentBackendKind::Droid && !was_running {
                        continue;
                    }
                }
                "permission_request" => {
                    agent_handle_permission_request(
                        backend, &router, &events, &pane_id, &input, &shared, &child, &reaped,
                        event,
                    );
                    continue;
                }
                _ => {}
            }
            append_and_emit_agent_event(&router, &events, &pane_id, event);
        }
    }

    // EOF: the CLI exited (it exits cleanly on stdin close — probe d). Reap
    // for the authoritative status, disarm any kill escalation, unblock any
    // approval waiter, and end any dangling turn.
    let exit_code = child
        .lock()
        .ok()
        .and_then(|mut child| child.wait().ok())
        .and_then(|status| status.code());
    reaped.store(true, Ordering::SeqCst);
    deny_agent_pending(&shared, "process_exit");
    agent_end_turn(&shared);

    // Generation-checked single claim, exactly like the PTY reader: only the
    // current generation reports the exit (PaneEnded fires once per
    // generation), and a superseded reader emits nothing.
    let claimed = liveness
        .lock()
        .ok()
        .and_then(|mut liveness| {
            liveness.get_mut(&pane_id).map(|entry| {
                if entry.generation == generation && !entry.ended {
                    entry.ended = true;
                    entry.exit_code = exit_code;
                    true
                } else {
                    false
                }
            })
        })
        .unwrap_or(false);
    if claimed {
        append_and_emit_agent_event(
            &router,
            &events,
            &pane_id,
            json!({"kind": "process_exit", "exit_code": exit_code}),
        );
        // (T1) M2: a dead agent must not keep a working/needs-input badge.
        router.clear_agent_attention(&pane_id);
        router.emit_pane_ended(&pane_id, exit_code);
    }
}

/// (T2) A live session's stdin queue + shared state, cloned out from under
/// the store lock for request handlers.
#[cfg(any(unix, windows))]
type AgentSessionHandles = (
    AgentBackendKind,
    SyncSender<Vec<u8>>,
    Arc<Mutex<AgentShared>>,
    Arc<Mutex<AgentEventLog>>,
);

#[cfg(any(unix, windows))]
impl TerminalStore {
    /// (T2) The cheap half of an agent spawn, read under the store lock (M7).
    /// The CLI session to resume comes from the previous (ended) session when
    /// there is one, else the persisted `agents_v2` seed.
    fn plan_agent_spawn(&self, pane_id: &str) -> AgentSpawnPlan {
        let spec = self.agent_specs.get(pane_id).cloned().unwrap_or_default();
        let bin = resolve_provider_bin(&self.agent_config, spec.backend);
        let resume_session_id = self
            .agent_sessions
            .get(pane_id)
            .and_then(|session| {
                session
                    .shared
                    .lock()
                    .ok()
                    .and_then(|shared| shared.session_id.clone())
            })
            .or_else(|| self.agent_resume.get(pane_id).cloned());
        let mut args = match spec.backend {
            AgentBackendKind::Claude => vec![
                "-p".to_string(),
                "--input-format".to_string(),
                "stream-json".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
                "--permission-mode".to_string(),
                self.agent_config.permission_mode.clone(),
                // Without a prompt tool, manual mode auto-DENIES
                // approval-needing tools instead of prompting.
                "--permission-prompt-tool".to_string(),
                "stdio".to_string(),
                "--include-partial-messages".to_string(),
            ],
            AgentBackendKind::Droid => vec![
                "exec".to_string(),
                "--input-format".to_string(),
                "stream-jsonrpc".to_string(),
                "--output-format".to_string(),
                "stream-jsonrpc".to_string(),
            ],
        };
        if spec.backend == AgentBackendKind::Claude {
            if let Some(model) = &spec.model {
                args.push("--model".to_string());
                args.push(model.clone());
            }
            if let Some(session_id) = &resume_session_id {
                args.push("--resume".to_string());
                args.push(session_id.clone());
            }
        }
        let command_str = format!("{} {}", agent_bin_display(&bin), args.join(" "));
        let initial_input = if spec.backend == AgentBackendKind::Droid {
            let request = if let Some(session_id) = &resume_session_id {
                json!({
                    "jsonrpc": "2.0",
                    "factoryApiVersion": "1.0.0",
                    "type": "request",
                    "id": format!("sgian-load-{pane_id}"),
                    "method": "droid.load_session",
                    "params": {"sessionId": session_id},
                })
            } else {
                let mut params = json!({
                    "machineId": "sgian",
                    "cwd": self.cwd.to_string_lossy(),
                    // Manual approval is the safe common denominator. The
                    // UI answers droid.request_permission over JSON-RPC.
                    "autonomyLevel": "off",
                });
                if let Some(model) = &spec.model {
                    params["modelId"] = json!(model);
                }
                json!({
                    "jsonrpc": "2.0",
                    "factoryApiVersion": "1.0.0",
                    "type": "request",
                    "id": format!("sgian-init-{pane_id}"),
                    "method": "droid.initialize_session",
                    "params": params,
                })
            };
            serde_json::to_string(&request).ok()
        } else {
            None
        };
        AgentSpawnPlan {
            backend: spec.backend,
            bin,
            args,
            env: self.shell.env.clone(),
            scrub_env: self.shell.scrub_env.clone(),
            cwd: self.cwd.clone(),
            command_str,
            cwd_str: self.cwd.to_string_lossy().to_string(),
            initial_input,
        }
    }

    /// (T2) Commit an agent spawn under the store lock (M7): generation bump,
    /// liveness entry, stderr drainer + stdout reader threads, session insert.
    /// No vt100 model is created — agent panes have no terminal screen.
    fn commit_agent_spawn(&mut self, pane_id: &str, prepared: PreparedAgentSpawn) {
        let PreparedAgentSpawn {
            backend,
            child,
            stdin,
            stdout,
            stderr,
            command_str,
            cwd_str,
            initial_input,
        } = prepared;

        self.next_generation += 1;
        let generation = self.next_generation;
        if let Ok(mut liveness) = self.liveness.lock() {
            liveness.insert(
                pane_id.to_string(),
                PaneLiveness {
                    generation,
                    ended: false,
                    command: Some(command_str),
                    cwd: Some(cwd_str),
                    exit_code: None,
                    // Agent panes report attention from their own event
                    // stream; the process-tree probe is for shell panes.
                    pid: None,
                },
            );
        }

        #[cfg(unix)]
        let pid = child.id();
        let reaped = Arc::new(AtomicBool::new(false));
        // The child is shared between the reader (which reaps it) and — on
        // Windows — the killer (TerminateProcess needs the process handle;
        // the unix killer signals by pid and never touches this mutex).
        let child = Arc::new(Mutex::new(child));
        let shared = Arc::new(Mutex::new(AgentShared::default()));
        let events = Arc::new(Mutex::new(AgentEventLog {
            log: AgentLogWriter::open(&self.agents_dir, pane_id),
            next_seq: agent_log_last_seq(&self.agents_dir, pane_id),
        }));
        let input = spawn_input_writer(Box::new(stdin));
        if let Some(line) = initial_input {
            if let Err(error) = queue_agent_stdin(&input, pane_id, &line) {
                tracing::warn!(
                    pane_id,
                    event = "agent_init_write_failed",
                    %error,
                    "failed to queue agent provider initialization"
                );
            }
        }

        // stderr → tracing (diagnostics only; never part of the event stream).
        // (T2) M1: the SAME bounded read as stdout — a broken/hostile child
        // writing a newline-less flood must not grow daemon memory.
        let stderr_router = self.router.clone();
        let stderr_pane_id = pane_id.to_string();
        thread::spawn(move || {
            let _log_guard = stderr_router.log_guard();
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            while let Ok(Some(truncated)) =
                read_capped_line(&mut reader, &mut line, AGENT_OUTPUT_LINE_MAX)
            {
                let text = String::from_utf8_lossy(&line);
                tracing::warn!(
                    pane_id = %stderr_pane_id,
                    event = "agent_stderr",
                    truncated,
                    "agent CLI stderr: {}",
                    text.trim_end()
                );
            }
        });

        let ctx = AgentReaderCtx {
            pane_id: pane_id.to_string(),
            backend,
            generation,
            child: Arc::clone(&child),
            stdout,
            input: input.clone(),
            shared: Arc::clone(&shared),
            liveness: Arc::clone(&self.liveness),
            router: self.router.clone(),
            events: Arc::clone(&events),
            reaped: Arc::clone(&reaped),
            dirty: Arc::clone(&self.agent_dirty),
        };
        let output_router = self.router.clone();
        thread::spawn(move || {
            // Activate structured logging on this thread (best-effort).
            let _log_guard = output_router.log_guard();
            agent_reader_main(ctx);
        });

        // Replacing an ended session drops it (deny + kill are no-ops on an
        // already-reaped child — on unix the killer checks the reaped flag;
        // on Windows TerminateProcess on an exited handle simply fails).
        #[cfg(unix)]
        let killer = AgentChildKiller { pid, reaped };
        #[cfg(windows)]
        let killer = AgentChildKiller { child };
        self.agent_sessions.insert(
            pane_id.to_string(),
            AgentSession {
                backend,
                input,
                killer,
                shared,
                events,
            },
        );
    }

    /// (T2) The live session's stdin handle + shared state for request
    /// handlers (None when the pane has no committed agent session).
    fn agent_session_handles(&self, pane_id: &str) -> Option<AgentSessionHandles> {
        self.agent_sessions.get(pane_id).map(|session| {
            (
                session.backend,
                session.input.clone(),
                Arc::clone(&session.shared),
                Arc::clone(&session.events),
            )
        })
    }

    /// (T2) The pane's last known CLI session id (live session first, then
    /// the persisted seed), recorded into `agents_v2` on persist.
    fn agent_session_id(&self, pane_id: &str) -> Option<String> {
        self.agent_sessions
            .get(pane_id)
            .and_then(|session| {
                session
                    .shared
                    .lock()
                    .ok()
                    .and_then(|shared| shared.session_id.clone())
            })
            .or_else(|| self.agent_resume.get(pane_id).cloned())
    }
}

impl OutputRouter {
    /// (T2) Broadcast a normalized agent event to subscribers, with the same
    /// closed-pane suppression as `emit` (a draining reader of a closed pane
    /// must not deliver further events).
    #[cfg(any(unix, windows))]
    fn emit_agent_event(&self, pane_id: &str, event: Value) {
        if self.is_closed(pane_id) {
            return;
        }
        self.broadcast(&DaemonEvent::AgentEvent {
            pane_id: pane_id.to_string(),
            event,
        });
    }
}

fn emit_pty_output(app: &AppHandle, pane_id: &str, data: String) {
    let _ = app.emit(
        "pty-output",
        PtyOutput {
            pane_id: pane_id.to_string(),
            data,
        },
    );
}

fn drain_complete_utf8(pending: &mut Vec<u8>) -> Vec<String> {
    let mut chunks = Vec::new();

    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                if !valid.is_empty() {
                    chunks.push(valid.to_string());
                }
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&pending[..valid_up_to])
                        .unwrap_or_default()
                        .to_string();
                    if !valid.is_empty() {
                        chunks.push(valid);
                    }
                    pending.drain(..valid_up_to);
                    continue;
                }

                let Some(error_len) = error.error_len() else {
                    break;
                };
                let invalid = String::from_utf8_lossy(&pending[..error_len]).to_string();
                chunks.push(invalid);
                pending.drain(..error_len);
            }
        }
    }

    chunks
}

#[derive(Clone)]
struct DaemonClient {
    cwd: PathBuf,
    socket_path: PathBuf,
    data_dir: PathBuf,
    token: String,
    /// Whether ensure_daemon may spawn a daemon. Read-only ctl commands connect with
    /// this off so `ctl panes` for a typo'd workspace can't create dirs and daemons.
    auto_spawn: bool,
}

impl DaemonClient {
    fn connect_or_spawn(cwd: PathBuf) -> Result<Self, String> {
        let client = Self::new(cwd)?;
        client.ensure_daemon()?;
        Ok(client)
    }

    /// Connect to an already-running daemon, with no side effects: never spawns a
    /// daemon, never creates data dirs or tokens.
    fn connect_existing(cwd: PathBuf) -> Result<Self, String> {
        let key = workspace_key(&cwd);
        let data_dir = workspace_data_dir_for(&cwd, &key);
        let socket_path = workspace_runtime_dir(&key).join(SOCKET_FILE);
        // Verify the persisted workspace cwd matches the connecting cwd before
        // reading the token or pinging. A mismatch (hash collision or data-dir
        // tampering) is refused with a clear error rather than silently serving
        // another workspace's data.
        check_persisted_cwd(&cwd, &data_dir)?;
        // (M6) A remote client has no workspace token file; its per-client
        // credential rides the hello instead and the daemon decides.
        let token = match read_token(&data_dir.join(TOKEN_FILE))? {
            Some(token) => token,
            None if client_token_from_env().is_some() => String::new(),
            None => return Err(no_daemon_error(&cwd)),
        };

        let client = Self {
            cwd,
            socket_path,
            data_dir,
            token,
            auto_spawn: false,
        };
        client.raw_request(DaemonRequest::Ping).map_err(|error| {
            // (M6) A presented credential that the daemon refused is not a
            // missing daemon; say which it was.
            if client_token_from_env().is_some() && error.contains("authentication") {
                format!("client credential refused: {error}")
            } else {
                no_daemon_error(&client.cwd)
            }
        })?;
        Ok(client)
    }

    fn new(cwd: PathBuf) -> Result<Self, String> {
        let workspace_key = workspace_key(&cwd);
        ensure_app_private_roots()?;
        let data_dir = workspace_data_dir_for(&cwd, &workspace_key);
        let runtime_dir = workspace_runtime_dir(&workspace_key);
        ensure_private_dir(&data_dir)?;
        ensure_private_dir(&runtime_dir)?;
        // Verify the persisted workspace cwd matches the connecting cwd before
        // creating/loading a token or spawning a daemon. A mismatch (hash
        // collision or data-dir tampering) is refused with a clear error rather
        // than silently serving another workspace's data.
        check_persisted_cwd(&cwd, &data_dir)?;
        let token = load_or_create_token(&data_dir)?;
        let socket_path = runtime_dir.join(SOCKET_FILE);

        Ok(Self {
            cwd,
            socket_path,
            data_dir,
            token,
            auto_spawn: true,
        })
    }

    fn ensure_daemon(&self) -> Result<(), String> {
        if self.raw_request(DaemonRequest::Ping).is_ok() {
            return Ok(());
        }
        if !self.auto_spawn {
            return Err(no_daemon_error(&self.cwd));
        }

        // (H4) A failed ping means a dead daemon OR a live-but-wedged one — only
        // the first may be replaced. If another process still holds the workspace
        // flock, the daemon is RUNNING but not responding: unlinking its live
        // socket would orphan it (`ctl shutdown` would then report "no daemon
        // running" while the wedged process keeps its shells hostage). Refuse
        // instead. When the lock is free the probe has already dropped its guard,
        // so the daemon spawned below can acquire the lock itself. The last ping
        // error is included to distinguish "daemon broken" from "transport broken".
        if daemon_instance_is_held(&self.cwd, &self.socket_path)? {
            let last_error = self
                .raw_request(DaemonRequest::Ping)
                .err()
                .unwrap_or_else(|| "unknown".to_string());
            return Err(format!(
                "daemon for workspace {} is running but not responding (wedged?); \
                 refusing to replace it — kill the process and retry \
                 (last ping error: {last_error})",
                self.cwd.display()
            ));
        }

        remove_stale_socket(&self.socket_path)?;
        ensure_private_dir(&self.data_dir)?;
        if let Some(parent) = self.socket_path.parent() {
            ensure_private_dir(parent)?;
        }

        let current_exe =
            std::env::current_exe().map_err(|error| format!("failed to locate app: {error}"))?;
        let startup_log_path = reset_daemon_startup_log(&self.data_dir);
        let mut command = Command::new(&current_exe);
        command
            .arg(DAEMON_ARG)
            .arg(WORKSPACE_ARG)
            .arg(&self.cwd)
            .arg(SOCKET_ARG)
            .arg(&self.socket_path)
            .arg(DATA_DIR_ARG)
            .arg(&self.data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_daemon_process(&mut command);
        let mut child = command.spawn().map_err(|error| {
            format!(
                "failed to start daemon executable {}: {error}",
                current_exe.display()
            )
        })?;

        let mut last_error = "no ping attempts made".to_string();
        for _ in 0..DAEMON_CONNECT_RETRIES {
            match self.raw_request(DaemonRequest::Ping) {
                Ok(_) => return Ok(()),
                Err(error) => last_error = error,
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format_daemon_start_failure(
                        &last_error,
                        Some(status.to_string()),
                        &startup_log_path,
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(format!(
                        "failed to inspect spawned daemon process: {error} \
                         (last ping error: {last_error}; startup diagnostics: {})",
                        startup_log_path.display()
                    ));
                }
            }
            thread::sleep(DAEMON_CONNECT_DELAY);
        }

        let child_status = child
            .try_wait()
            .ok()
            .flatten()
            .map(|status| status.to_string());
        Err(format_daemon_start_failure(
            &last_error,
            child_status,
            &startup_log_path,
        ))
    }

    fn request<T: DeserializeOwned>(&self, request: DaemonRequest) -> Result<T, String> {
        self.request_with_timeout(request, Some(CLIENT_READ_TIMEOUT))
    }

    /// Like `request`, but arms the connection/handshake/response read deadline to
    /// `timeout` (or the default client timeout when `None` is not used — pass
    /// `Some(remaining)` to bound a caller-owned overall deadline).
    fn request_with_timeout<T: DeserializeOwned>(
        &self,
        request: DaemonRequest,
        timeout: Option<Duration>,
    ) -> Result<T, String> {
        self.ensure_daemon()?;
        let response = self.raw_request_with_timeout(request, timeout)?;
        if !response.ok {
            return Err(response
                .error
                .unwrap_or_else(|| "daemon request failed".to_string()));
        }
        serde_json::from_value(response.result)
            .map_err(|error| format!("invalid daemon response: {error}"))
    }

    fn raw_request(&self, request: DaemonRequest) -> Result<IpcResponse, String> {
        self.raw_request_with_timeout(request, Some(CLIENT_READ_TIMEOUT))
    }

    fn raw_request_with_timeout(
        &self,
        request: DaemonRequest,
        timeout: Option<Duration>,
    ) -> Result<IpcResponse, String> {
        let mut conn = self.connect_with_timeout(timeout)?;
        conn.request(&request)
    }

    /// Open a fresh connection to this workspace's daemon over the NEGOTIATED wire
    /// protocol (framed v2 against a new daemon, newline v1 against an old one). The
    /// single client entry point `ctl` and the GUI use; the handshake picks the wire
    /// automatically (architecture.md §5.2, Invariant 8).
    fn connect(&self) -> Result<DaemonConnection, String> {
        self.connect_with_timeout(Some(CLIENT_READ_TIMEOUT))
    }

    fn connect_with_timeout(&self, timeout: Option<Duration>) -> Result<DaemonConnection, String> {
        DaemonConnection::connect_with_timeout(&self.socket_path, &self.token, timeout)
    }

    /// Test-only low-level v1 handshake helper. The socket-based `TestDaemon` suite
    /// drives Subscribe/Ping/Shutdown over the LEGACY newline path through this, so
    /// the full `cargo test` run exercises BOTH wires at once (negotiated v2 via
    /// `connect`, newline v1 here) — the standing old-client↔new-daemon regression
    /// proof for Invariant 8.
    #[cfg(test)]
    fn authenticated_stream(&self) -> Result<TransportStream, String> {
        authenticate_stream_at(&self.socket_path, &self.token)
    }

    fn start_subscription(&self, app: AppHandle) {
        let socket_path = self.socket_path.clone();
        let token = self.token.clone();
        thread::spawn(move || {
            let mut backoff = SubscriptionBackoff::new();
            loop {
                // (L19) A session that died younger than SUBSCRIPTION_HEALTHY_MIN
                // counts as a failure: space the next connect by the backoff so a
                // daemon that accepts+acks then immediately EOFs cannot busy-loop
                // reconnects. A healthy long session resets to an immediate
                // reconnect. Connect failures keep their own fixed sleep below.
                let delay = backoff.pre_connect_delay();
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
                let session_started = Instant::now();
                match DaemonConnection::connect(&socket_path, &token) {
                    Ok(mut conn) => {
                        if conn.write_request(&DaemonRequest::Subscribe).is_err()
                            || conn.await_subscribe_ack().is_err()
                        {
                            // Never established: a zero-length session, so the next
                            // iteration takes the short-session backoff (same
                            // cadence as the fixed sleep this replaces).
                            backoff.session_ended(Some(session_started.elapsed()));
                            continue;
                        }
                        // Events are legitimately sparse; only the handshake + ack get
                        // the default read deadline, not the long-lived stream.
                        conn.set_read_timeout(None);
                        while let Ok(Some(event)) = conn.read_event() {
                            emit_daemon_event(&app, event);
                        }
                        backoff.session_ended(Some(session_started.elapsed()));
                    }
                    Err(_) => {
                        // The fixed sleep below already backs this case off; record
                        // no session so the state machine adds nothing on top.
                        backoff.session_ended(None);
                        thread::sleep(SUBSCRIPTION_RECONNECT_BACKOFF);
                    }
                }
            }
        });
    }
}

/// Reconnect-backoff state for the GUI event-subscription loop (L19). A session
/// that ends younger than `SUBSCRIPTION_HEALTHY_MIN` counts as a failure and costs
/// `SUBSCRIPTION_RECONNECT_BACKOFF` before the next connect; a healthy long
/// session resets the backoff so reconnects after a genuine drop stay immediate.
/// Pure decision logic, factored out of `start_subscription`'s loop for tests.
struct SubscriptionBackoff {
    previous_session: Option<Duration>,
}

impl SubscriptionBackoff {
    fn new() -> Self {
        Self {
            previous_session: None,
        }
    }

    /// Delay to apply BEFORE the next connect attempt.
    fn pre_connect_delay(&self) -> Duration {
        match self.previous_session {
            Some(duration) if duration < SUBSCRIPTION_HEALTHY_MIN => SUBSCRIPTION_RECONNECT_BACKOFF,
            _ => Duration::ZERO,
        }
    }

    /// Record the ended session's duration (`None` = the connect itself failed;
    /// the loop's own connect-failure sleep covers that case).
    fn session_ended(&mut self, duration: Option<Duration>) {
        self.previous_session = duration;
    }
}

// ---------------------------------------------------------------------------
// Output guard (docs/design/keyboard-lease-and-ledger.md §7): count the
// terminal tricks an agent can use to hide output from the person watching.
// ---------------------------------------------------------------------------

/// One rate-limit window as Claude Code reports it on its status line:
/// percent consumed (rounded) and when it resets (unix seconds).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimitWindow {
    #[serde(default)]
    pub used_percentage: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// What a Claude Code session says about itself after every turn, read
/// from the status-line payload (`ctl statusline`): the model, how full the
/// context window is, and the account's rate-limit windows (Pro/Max only).
/// Push, not poll: no credentials, no scraping. `updated_at_ms` says how
/// fresh it is; a client should fade a reading older than a few minutes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Percent of the context window in use (input-only, as Claude Code
    /// computes it), rounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_percentage: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<RateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<RateLimitWindow>,
    /// Session cost in US cents (Claude Code reports dollars as a float).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_cents: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub updated_at_ms: u64,
}

impl AgentUsage {
    /// Read the fields we keep out of a status-line payload. Everything is
    /// optional so a payload from a newer CLI still parses; a payload with
    /// nothing we recognise yields `None`.
    fn from_status_payload(payload: &Value) -> Option<AgentUsage> {
        fn percent(value: &Value) -> Option<u8> {
            value
                .as_f64()
                .map(|pct| pct.clamp(0.0, 100.0).round() as u8)
        }
        let window = |value: &Value| -> Option<RateLimitWindow> {
            Some(RateLimitWindow {
                used_percentage: percent(value.get("used_percentage")?)?,
                resets_at: value.get("resets_at").and_then(Value::as_u64),
            })
        };
        let usage = AgentUsage {
            model: payload["model"]["display_name"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(64).collect()),
            model_id: payload["model"]["id"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(128).collect()),
            context_used_percentage: percent(&payload["context_window"]["used_percentage"]),
            context_window_size: payload["context_window"]["context_window_size"].as_u64(),
            five_hour: window(&payload["rate_limits"]["five_hour"]),
            seven_day: window(&payload["rate_limits"]["seven_day"]),
            total_cost_cents: payload["cost"]["total_cost_usd"]
                .as_f64()
                .filter(|usd| usd.is_finite() && *usd >= 0.0)
                .map(|usd| (usd * 100.0).round() as u64),
            session_id: payload["session_id"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(128).collect()),
            updated_at_ms: 0,
        };
        let empty = usage.model.is_none()
            && usage.model_id.is_none()
            && usage.context_used_percentage.is_none()
            && usage.five_hour.is_none()
            && usage.seven_day.is_none()
            && usage.total_cost_cents.is_none();
        (!empty).then_some(usage)
    }

    /// "Opus · 40% context · 5h 23% ↻ 15:00 · 7d 41%": the one-line form the
    /// default status line and `ctl agent` print. `now` is unix seconds.
    fn summary(&self, now: u64) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(model) = &self.model {
            parts.push(model.clone());
        }
        if let Some(pct) = self.context_used_percentage {
            parts.push(format!("{pct}% context"));
        }
        if let Some(window) = &self.five_hour {
            parts.push(format!(
                "5h {}%{}",
                window.used_percentage,
                format_reset(window.resets_at, now)
            ));
        }
        if let Some(window) = &self.seven_day {
            parts.push(format!(
                "7d {}%{}",
                window.used_percentage,
                format_reset(window.resets_at, now)
            ));
        }
        parts.join(" · ")
    }
}

/// " ↻ 2h10m" (time until a window resets) or "" when unknown or past.
fn format_reset(resets_at: Option<u64>, now: u64) -> String {
    let Some(at) = resets_at else {
        return String::new();
    };
    if at <= now {
        return String::new();
    }
    let secs = at - now;
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h >= 48 {
        format!(" ↻ {}d", h / 24)
    } else if h > 0 {
        format!(" ↻ {h}h{m:02}m")
    } else {
        format!(" ↻ {m}m")
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputTricks {
    /// SGR 8 (conceal): text present but invisible.
    #[serde(default)]
    pub conceal: u32,
    /// OSC 52: writing the clipboard from output (exfiltration vector).
    #[serde(default)]
    pub clipboard: u32,
    /// OSC 8 hyperlink whose visible text is a URL on a different host.
    #[serde(default)]
    pub hyperlink_mismatch: u32,
    /// DCS / APC / PM / SOS strings: opaque payloads the emulator swallows.
    #[serde(default)]
    pub string_controls: u32,
    /// Raw C1 control characters (U+0080..U+009F) in the text stream.
    #[serde(default)]
    pub c1_controls: u32,
}

impl OutputTricks {
    fn total(&self) -> u32 {
        self.conceal
            + self.clipboard
            + self.hyperlink_mismatch
            + self.string_controls
            + self.c1_controls
    }

    fn add(&mut self, other: &OutputTricks) {
        self.conceal = self.conceal.saturating_add(other.conceal);
        self.clipboard = self.clipboard.saturating_add(other.clipboard);
        self.hyperlink_mismatch = self
            .hyperlink_mismatch
            .saturating_add(other.hyperlink_mismatch);
        self.string_controls = self.string_controls.saturating_add(other.string_controls);
        self.c1_controls = self.c1_controls.saturating_add(other.c1_controls);
    }

    fn minus(&self, other: &OutputTricks) -> OutputTricks {
        OutputTricks {
            conceal: self.conceal.saturating_sub(other.conceal),
            clipboard: self.clipboard.saturating_sub(other.clipboard),
            hyperlink_mismatch: self
                .hyperlink_mismatch
                .saturating_sub(other.hyperlink_mismatch),
            string_controls: self.string_controls.saturating_sub(other.string_controls),
            c1_controls: self.c1_controls.saturating_sub(other.c1_controls),
        }
    }
}

/// The host of a URL-ish string (`scheme://host[:port]/…` or `www.host…`),
/// lowercased; None when it does not look like a URL.
fn url_host(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let rest = if let Some((_, rest)) = trimmed.split_once("://") {
        rest
    } else if trimmed.starts_with("www.") {
        trimmed
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Scan one output chunk. Sequences split across chunks are missed, which is
/// acceptable for a counter meant to raise a flag, not to censor.
fn scan_output_tricks(text: &str) -> OutputTricks {
    let mut tricks = OutputTricks::default();
    let mut chars = text.chars().peekable();
    // The open OSC 8 target host while inside a hyperlink, and the visible
    // text collected under it.
    let mut link_host: Option<String> = None;
    let mut link_text = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut final_byte = None;
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        params.push(next);
                    }
                    if final_byte == Some('m') {
                        // SGR: a standalone `8` conceals; `38;5;8` (a colour
                        // index) does not.
                        let mut parts = params.split(';');
                        while let Some(part) = parts.next() {
                            match part {
                                "8" => tricks.conceal += 1,
                                "38" | "48" | "58" => match parts.next() {
                                    Some("5") => {
                                        parts.next();
                                    }
                                    Some("2") => {
                                        for _ in 0..3 {
                                            parts.next();
                                        }
                                    }
                                    _ => {}
                                },
                                _ => {}
                            }
                        }
                    }
                }
                Some(']') => {
                    let mut body = String::new();
                    let mut previous_esc = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_esc && next == '\\') {
                            break;
                        }
                        previous_esc = next == '\u{1b}';
                        if !previous_esc {
                            body.push(next);
                        }
                    }
                    if body.starts_with("52;") {
                        tricks.clipboard += 1;
                    } else if let Some(rest) = body.strip_prefix("8;") {
                        let target = rest.split_once(';').map(|(_, t)| t).unwrap_or("");
                        if target.is_empty() {
                            // Closing the link: compare what was shown with where it went.
                            if let (Some(host), Some(shown)) =
                                (link_host.take(), url_host(&link_text))
                            {
                                if shown != host {
                                    tricks.hyperlink_mismatch += 1;
                                }
                            }
                            link_text.clear();
                        } else {
                            link_host = url_host(target);
                            link_text.clear();
                        }
                    }
                }
                Some('P') | Some('_') | Some('^') | Some('X') => {
                    tricks.string_controls += 1;
                    let mut previous_esc = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_esc && next == '\\') {
                            break;
                        }
                        previous_esc = next == '\u{1b}';
                    }
                }
                _ => {}
            },
            '\u{80}'..='\u{9f}' => tricks.c1_controls += 1,
            c => {
                if link_host.is_some() {
                    link_text.push(c);
                }
            }
        }
    }
    tricks
}

/// Announce at most this often per pane (ledger + event); counts always accumulate.
const OUTPUT_WARNING_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct OutputGuardState {
    total: OutputTricks,
    announced: OutputTricks,
    last_announced: Option<Instant>,
}

// ---------------------------------------------------------------------------
// Keyboard lease and session ledger (docs/design/keyboard-lease-and-ledger.md)
//
// Pure state, predicates, and the hash-chained ledger writer/verifier. The
// DaemonServer handlers call these; clients never re-derive the rules.
// ---------------------------------------------------------------------------

/// Per-pane hash-chained ledgers live here beside `agents/` and `scrollback/`.
/// Unlike those two, a ledger survives pane close: it is the audit record.
const LEDGER_DIR: &str = "ledger";
/// Inside the hash input so a record cannot be re-hashed under another
/// version (the same reasoning as Kranz's `kranz.event-log.v2\n`).
const LEDGER_HASH_PREFIX: &str = "sgian.ledger.v1\n";
const HOLDER_MAX_LEN: usize = 64;
const LEASE_NOTE_MAX_BYTES: usize = 4096;
const LEASE_WHY_MAX_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeasePolicy {
    /// An unheld pane accepts input from anyone; a held pane only from its holder.
    Open,
    /// Every write needs the lease.
    Required,
}

impl LeasePolicy {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Required => "required",
        }
    }
}

/// The held half of a pane's lease. Persisted verbatim in workspace.json.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct HeldLease {
    holder: String,
    since_ms: u64,
    #[serde(default)]
    writes: u64,
    #[serde(default)]
    bytes_typed: u64,
    #[serde(default)]
    refused_writes: u64,
    #[serde(default)]
    last_input_ms: Option<u64>,
    /// Monotonic per-workspace lease number. A command that names a
    /// generation is refused when the lease has changed hands since, so a
    /// previous holder's late write, answer or release cannot land on the
    /// current holder's session. 0 for leases persisted before generations.
    #[serde(default)]
    generation: u64,
}

impl HeldLease {
    fn new(holder: &str, since_ms: u64, generation: u64) -> Self {
        Self {
            holder: holder.to_string(),
            since_ms,
            writes: 0,
            bytes_typed: 0,
            refused_writes: 0,
            last_input_ms: None,
            generation,
        }
    }
}

/// Wire shape of a pane's lease (snapshot `leases`, lease responses).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseInfo {
    pub pane_id: String,
    pub policy: String,
    pub holder: Option<String>,
    pub since_ms: Option<u64>,
    pub held_ms: Option<u64>,
    #[serde(default)]
    pub writes: u64,
    #[serde(default)]
    pub bytes_typed: u64,
    #[serde(default)]
    pub refused_writes: u64,
    #[serde(default)]
    pub last_input_ms: Option<u64>,
    /// The lease's generation (see `HeldLease::generation`); present while held.
    #[serde(default)]
    pub generation: Option<u64>,
}

impl LeaseInfo {
    fn from_lease(
        pane_id: &str,
        policy: LeasePolicy,
        lease: Option<&HeldLease>,
        now_ms: u64,
    ) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            policy: policy.as_str().to_string(),
            holder: lease.map(|held| held.holder.clone()),
            since_ms: lease.map(|held| held.since_ms),
            held_ms: lease.map(|held| now_ms.saturating_sub(held.since_ms)),
            writes: lease.map(|held| held.writes).unwrap_or(0),
            bytes_typed: lease.map(|held| held.bytes_typed).unwrap_or(0),
            refused_writes: lease.map(|held| held.refused_writes).unwrap_or(0),
            last_input_ms: lease.and_then(|held| held.last_input_ms),
            generation: lease.map(|held| held.generation),
        }
    }
}

/// Refuse a command that names a lease generation which is no longer the
/// pane's current one (or names one while the pane is unheld).
fn check_generation(lease: Option<&HeldLease>, generation: Option<u64>) -> Result<(), String> {
    match (generation, lease) {
        (None, _) => Ok(()),
        (Some(wanted), Some(held)) if held.generation == wanted => Ok(()),
        (Some(wanted), Some(held)) => Err(format!(
            "stale lease: generation {wanted} is no longer current (now {} held by {})",
            held.generation, held.holder
        )),
        (Some(wanted), None) => Err(format!(
            "stale lease: generation {wanted} is no longer current (pane is unheld)"
        )),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LeaseTransition {
    Taken,
    Released,
    Revoked,
}

/// Holder labels are operator text that ends up in ledgers, status lines and
/// error messages: short, printable ASCII, no whitespace.
fn validate_holder(raw: &str) -> Result<String, String> {
    let holder = raw.trim();
    if holder.is_empty() {
        return Err("holder must not be blank".to_string());
    }
    if holder.len() > HOLDER_MAX_LEN {
        return Err(format!("holder is longer than {HOLDER_MAX_LEN} bytes"));
    }
    if !holder.chars().all(|c| c.is_ascii_graphic()) {
        return Err("holder must be printable ASCII with no whitespace".to_string());
    }
    Ok(holder.to_string())
}

/// Notes and reasons: trimmed, bounded, free text (newlines and tabs allowed,
/// other control characters are not).
fn validate_bounded_text(raw: &str, what: &str, max: usize) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if text.len() > max {
        return Err(format!("{what} is longer than {max} bytes"));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(format!("{what} must not contain control characters"));
    }
    Ok(text.to_string())
}

/// What a permitted `take` does.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TakeOutcome {
    Fresh,
    AlreadyHeld,
    Revoking { previous: String },
}

fn can_take(
    lease: Option<&HeldLease>,
    holder: &str,
    force: bool,
    why: Option<&str>,
) -> Result<TakeOutcome, String> {
    match lease {
        None => Ok(TakeOutcome::Fresh),
        Some(held) if held.holder == holder => Ok(TakeOutcome::AlreadyHeld),
        Some(held) => {
            if !force {
                return Err(format!(
                    "pane keyboard is held by {}; use --force --why REASON to revoke it",
                    held.holder
                ));
            }
            if why.map(str::trim).unwrap_or("").is_empty() {
                return Err("--force requires --why REASON".to_string());
            }
            Ok(TakeOutcome::Revoking {
                previous: held.holder.clone(),
            })
        }
    }
}

fn can_release(lease: Option<&HeldLease>, holder: &str) -> Result<(), String> {
    match lease {
        None => Err("pane keyboard is not held".to_string()),
        Some(held) if held.holder == holder => Ok(()),
        Some(held) => Err(format!(
            "pane keyboard is held by {}, not {holder}",
            held.holder
        )),
    }
}

fn can_write(
    policy: LeasePolicy,
    lease: Option<&HeldLease>,
    holder: Option<&str>,
) -> Result<(), String> {
    match (policy, lease) {
        (_, Some(held)) => {
            if holder == Some(held.holder.as_str()) {
                Ok(())
            } else {
                Err(format!("pane keyboard is held by {}", held.holder))
            }
        }
        (LeasePolicy::Open, None) => Ok(()),
        (LeasePolicy::Required, None) => {
            Err("pane keyboard is unheld and lease_policy is required; take it first".to_string())
        }
    }
}

/// One ledger line. `h` chains over everything else in the record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LedgerRecord {
    seq: u64,
    ts_ms: u64,
    pane_id: String,
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
    prev: String,
    h: String,
}

fn ledger_path(dir: &Path, pane_id: &str) -> PathBuf {
    dir.join(format!("{pane_id}.jsonl"))
}

/// Sorted-key, whitespace-free JSON: the same bytes regardless of the
/// serializer's map ordering feature or the caller's field order.
fn canonical_json(value: &Value) -> String {
    let mut sorted = value.clone();
    sorted.sort_all_objects();
    sorted.to_string()
}

fn ledger_hash(prev: &str, body: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(LEDGER_HASH_PREFIX.as_bytes());
    hasher.update(prev.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    hex_encode(&hasher.finalize())
}

/// The hashed body: the record without `h`, canonicalized.
fn ledger_body(record: &LedgerRecord) -> String {
    let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
    if let Value::Object(ref mut map) = value {
        map.remove("h");
    }
    canonical_json(&value)
}

/// The chain head `(seq, h)` from a ledger's last non-blank line;
/// `(0, "")` for a missing or empty ledger.
fn ledger_head(path: &Path) -> Result<(u64, String), String> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((0, String::new()))
        }
        Err(error) => return Err(format!("failed to read ledger {}: {error}", path.display())),
    };
    match data.lines().rev().find(|line| !line.trim().is_empty()) {
        None => Ok((0, String::new())),
        Some(line) => {
            let record: LedgerRecord = serde_json::from_str(line).map_err(|error| {
                format!("ledger {} tail is unreadable: {error}", path.display())
            })?;
            Ok((record.seq, record.h))
        }
    }
}

/// One workspace's ledger writer: the directory plus cached chain heads,
/// shared by the daemon handlers (lease events, durable) and the output
/// router (attention transitions and pane ends, best-effort). A LEAF lock:
/// `record` does file I/O under it and no caller holds another lock then.
struct LedgerSink {
    dir: PathBuf,
    heads: HashMap<String, (u64, String)>,
}

impl LedgerSink {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            heads: HashMap::new(),
        }
    }

    fn record(
        &mut self,
        pane_id: &str,
        kind: &str,
        payload: Value,
        durable: bool,
    ) -> Result<LedgerRecord, String> {
        ledger_append(&self.dir, &mut self.heads, pane_id, kind, payload, durable)
    }
}

/// Append one record, chaining from the cached head (seeded from disk on
/// first use). One `write_all` of line+'\n'; `durable` adds an fsync (lease
/// events are rare and are the product; attention flaps are frequent and
/// are not).
fn ledger_append(
    dir: &Path,
    heads: &mut HashMap<String, (u64, String)>,
    pane_id: &str,
    kind: &str,
    payload: Value,
    durable: bool,
) -> Result<LedgerRecord, String> {
    let path = ledger_path(dir, pane_id);
    let (seq, prev) = match heads.get(pane_id) {
        Some(head) => head.clone(),
        None => ledger_head(&path)?,
    };
    let mut record = LedgerRecord {
        seq: seq.saturating_add(1),
        ts_ms: now_millis(),
        pane_id: pane_id.to_string(),
        kind: kind.to_string(),
        payload,
        prev,
        h: String::new(),
    };
    record.h = ledger_hash(&record.prev, &ledger_body(&record));
    let line = serde_json::to_string(&record)
        .map_err(|error| format!("failed to encode ledger record: {error}"))?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .private_mode()
        .open(&path)
        .map_err(|error| format!("failed to open ledger {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|_| if durable { file.sync_all() } else { Ok(()) })
        .map_err(|error| format!("failed to append ledger {}: {error}", path.display()))?;
    heads.insert(pane_id.to_string(), (record.seq, record.h.clone()));
    Ok(record)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct LedgerSummary {
    records: u64,
    head: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct LedgerBreak {
    line: usize,
    seq: Option<u64>,
    reason: String,
}

/// Walk a ledger and report the first break: an unparseable line, a sequence
/// gap, a `prev` that does not match, or a record whose bytes no longer hash
/// to `h`. Truncation from the tail is NOT detectable here; pin `head` from a
/// prior run to catch it.
fn ledger_verify(path: &Path) -> Result<LedgerSummary, LedgerBreak> {
    let data = fs::read_to_string(path).map_err(|error| LedgerBreak {
        line: 0,
        seq: None,
        reason: format!("cannot read ledger: {error}"),
    })?;
    let mut prev = String::new();
    let mut expected_seq: u64 = 1;
    let mut records: u64 = 0;
    for (index, line) in data.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let record: LedgerRecord = serde_json::from_str(line).map_err(|error| LedgerBreak {
            line: line_no,
            seq: None,
            reason: format!("unparseable record: {error}"),
        })?;
        if record.seq != expected_seq {
            return Err(LedgerBreak {
                line: line_no,
                seq: Some(record.seq),
                reason: format!("sequence {} where {expected_seq} was expected", record.seq),
            });
        }
        if record.prev != prev {
            return Err(LedgerBreak {
                line: line_no,
                seq: Some(record.seq),
                reason: "prev hash does not match the previous record".to_string(),
            });
        }
        let expected_hash = ledger_hash(&record.prev, &ledger_body(&record));
        if !constant_time_eq(&expected_hash, &record.h) {
            return Err(LedgerBreak {
                line: line_no,
                seq: Some(record.seq),
                reason: "record hash mismatch (content altered)".to_string(),
            });
        }
        prev = record.h;
        expected_seq = expected_seq.saturating_add(1);
        records += 1;
    }
    Ok(LedgerSummary {
        records,
        head: prev,
    })
}

/// The last `limit` records (0 = all) as raw JSON values; unparseable lines
/// are skipped so a torn tail still lists what came before it.
fn read_ledger_tail(path: &Path, limit: usize) -> Vec<Value> {
    let data = fs::read_to_string(path).unwrap_or_default();
    let parsed: Vec<Value> = data
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if limit == 0 || parsed.len() <= limit {
        parsed
    } else {
        parsed[parsed.len() - limit..].to_vec()
    }
}

// ---------------------------------------------------------------------------
// Official agent probe (M3b): `claude agents --json` mapped to panes through
// the process tree. Pure parts here; the loop lives on DaemonServer.
// ---------------------------------------------------------------------------

/// One entry of `claude agents --json`. Only the fields the probe reads;
/// unknown fields are ignored so newer CLIs keep parsing.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
struct AgentProbeEntry {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "waitingFor")]
    waiting_for: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// Map Claude Code's vocabulary (`status`: busy/waiting/idle; `waitingFor`
/// when it needs a person; `state`: working/blocked/done/failed/stopped) onto
/// the pane attention states. `None` = nothing to say.
fn attention_from_probe(entry: &AgentProbeEntry) -> Option<AgentAttention> {
    if entry
        .waiting_for
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Some(AgentAttention::NeedsInput);
    }
    match entry.status.as_deref().map(str::trim) {
        Some("waiting") | Some("blocked") => Some(AgentAttention::NeedsInput),
        Some("busy") | Some("working") | Some("running") => Some(AgentAttention::Working),
        Some("idle") => Some(AgentAttention::Idle),
        _ => match entry.state.as_deref().map(str::trim) {
            Some("blocked") => Some(AgentAttention::NeedsInput),
            Some("working") => Some(AgentAttention::Working),
            Some("done") | Some("failed") | Some("stopped") => Some(AgentAttention::Idle),
            _ => None,
        },
    }
}

/// One `ps -axo pid=,ppid=,args=` snapshot: child → parent, and each pid's
/// command line (empty when `ps` was asked for pids only).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ProcessTable {
    parent: HashMap<u32, u32>,
    args: HashMap<u32, String>,
}

fn parse_process_table(text: &str) -> ProcessTable {
    let mut table = ProcessTable::default();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (
            fields.next().and_then(|field| field.parse::<u32>().ok()),
            fields.next().and_then(|field| field.parse::<u32>().ok()),
        ) else {
            continue;
        };
        table.parent.insert(pid, ppid);
        let args = fields.collect::<Vec<_>>().join(" ");
        if !args.is_empty() {
            table.args.insert(pid, args);
        }
    }
    table
}

/// Whether a command line is a Kranz worker loop (`kranz run` / `exec` /
/// `work`), by its argv[0] basename and first subcommand.
fn is_kranz_worker_command(args: &str) -> bool {
    let mut fields = args.split_whitespace();
    let Some(program) = fields.next() else {
        return false;
    };
    let basename = program.rsplit(['/', '\\']).next().unwrap_or(program);
    if basename != "kranz" && basename != "kranz.exe" {
        return false;
    }
    // Global flags precede the subcommand; `--repo`/`--mission` take a value.
    let mut skip_value = false;
    for field in fields {
        if skip_value {
            skip_value = false;
            continue;
        }
        if let Some(flag) = field.strip_prefix("--") {
            skip_value = matches!(flag, "repo" | "mission");
            continue;
        }
        return matches!(field, "run" | "exec" | "work");
    }
    false
}

/// (M4) Panes whose process tree contains a Kranz worker loop: pane → the
/// worker's pid. A pane with several workers reports the first found.
fn find_kranz_panes(table: &ProcessTable, pane_pids: &[(String, u32)]) -> HashMap<String, u32> {
    let pane_by_pid: HashMap<u32, &str> = pane_pids
        .iter()
        .map(|(pane_id, pid)| (*pid, pane_id.as_str()))
        .collect();
    let mut found: HashMap<String, u32> = HashMap::new();
    for (pid, args) in &table.args {
        if !is_kranz_worker_command(args) {
            continue;
        }
        let mut cursor = *pid;
        for _ in 0..64 {
            if let Some(pane_id) = pane_by_pid.get(&cursor) {
                found.entry((*pane_id).to_string()).or_insert(*pid);
                break;
            }
            match table.parent.get(&cursor) {
                Some(parent) if *parent != cursor && *parent > 1 => cursor = *parent,
                _ => break,
            }
        }
    }
    found
}

/// (M4) Map a Kranz `MissionState` (camelCase JSON, as `kranz status --json`
/// prints it) onto pane attention: anything pending on a person is
/// needs-input, an active loop is working, a terminal state is idle.
fn kranz_attention_from_state(state: &Value) -> Option<AgentAttention> {
    let non_empty = |key: &str| {
        state.get(key).is_some_and(|value| match value {
            Value::Array(items) => !items.is_empty(),
            Value::Null => false,
            Value::Object(_) => true,
            _ => true,
        })
    };
    if non_empty("pendingQuestions")
        || non_empty("pendingGrantRequest")
        || non_empty("pendingRevision")
    {
        return Some(AgentAttention::NeedsInput);
    }
    match state.get("status").and_then(Value::as_str) {
        Some("planning") | Some("approved") | Some("running") | Some("validating") => {
            Some(AgentAttention::Working)
        }
        Some("paused") | Some("blocked") => Some(AgentAttention::NeedsInput),
        Some("complete") | Some("failed") | Some("abandoned") => Some(AgentAttention::Idle),
        _ => None,
    }
}

/// A named group of panes serving one goal (a feature, a migration, a
/// mission): the grouping above panes that lets one page show every worker,
/// its attention, who holds its keyboard, and a merged ledger. Projects
/// persist with the workspace; panes belong to at most one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default)]
    pub panes: Vec<String>,
    pub created_at_ms: u64,
}

const MAX_PROJECTS: usize = 64;
const PROJECT_NAME_MAX_LEN: usize = 64;
const PROJECT_LEDGER_DEFAULT_LIMIT: usize = 50;
const PROJECT_LEDGER_MAX_LIMIT: usize = 500;
/// Scrollback lines per pane in a dossier when the caller does not say.
const PROJECT_DOSSIER_DEFAULT_LINES: usize = 40;
/// The dossier document's format tag; bump when a consumer could misread it.
const PROJECT_DOSSIER_FORMAT: &str = "sgian.dossier.v1";

/// Project names are keys and appear in ledgers and shell output: short,
/// `[A-Za-z0-9._-]`, no leading dot.
fn validate_project_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("project name must not be blank".to_string());
    }
    if name.len() > PROJECT_NAME_MAX_LEN {
        return Err(format!(
            "project name is longer than {PROJECT_NAME_MAX_LEN} bytes"
        ));
    }
    if name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("project name may only contain letters, digits, '.', '_' and '-'".to_string());
    }
    Ok(name.to_string())
}

/// A project with its attention roll-up: what one glance needs to tell.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectSummary {
    pub project: Project,
    pub panes: usize,
    pub live: usize,
    pub needs_input: usize,
    pub working: usize,
    pub idle: usize,
    pub unattended: usize,
    pub held: usize,
    pub holders: Vec<String>,
}

fn project_rollup(
    project: &Project,
    states: &HashMap<String, PaneRuntimeState>,
    agents: &HashMap<String, AgentPaneInfo>,
    leases: &HashMap<String, HeldLease>,
) -> ProjectSummary {
    let mut summary = ProjectSummary {
        project: project.clone(),
        panes: project.panes.len(),
        live: 0,
        needs_input: 0,
        working: 0,
        idle: 0,
        unattended: 0,
        held: 0,
        holders: Vec::new(),
    };
    for pane_id in &project.panes {
        if states.get(pane_id) == Some(&PaneRuntimeState::Live) {
            summary.live += 1;
        }
        if let Some(info) = agents.get(pane_id) {
            match info.attention {
                Some(AgentAttention::NeedsInput) => summary.needs_input += 1,
                Some(AgentAttention::Working) => summary.working += 1,
                Some(AgentAttention::Idle) => summary.idle += 1,
                None => {}
            }
            if info.unattended {
                summary.unattended += 1;
            }
        }
        if let Some(held) = leases.get(pane_id) {
            summary.held += 1;
            if !summary.holders.contains(&held.holder) {
                summary.holders.push(held.holder.clone());
            }
        }
    }
    summary.holders.sort();
    summary
}

/// (M4) A pane bound to a Kranz mission: hand-back notes are mirrored into
/// its inbox and its attention comes from `kranz status`. Auto bindings come
/// from the process tree (a `kranz run` under the pane's shell); manual ones
/// from `ctl kranz bind` and survive the worker exiting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KranzBinding {
    pub repo: String,
    pub manual: bool,
}

/// Attribute each probe entry to the pane whose child process is its ancestor
/// (or itself). When several sessions land in one pane the loudest wins:
/// needs-input over working over idle.
/// The pane whose child process is `pid` or one of its ancestors (at most
/// 64 hops, stopping at init). `parent_of` is child → parent.
fn pane_for_pid(
    mut pid: u32,
    parent_of: &HashMap<u32, u32>,
    pane_pids: &[(String, u32)],
) -> Option<String> {
    let pane_by_pid: HashMap<u32, &str> = pane_pids
        .iter()
        .map(|(pane_id, pid)| (*pid, pane_id.as_str()))
        .collect();
    for _ in 0..64 {
        if let Some(found) = pane_by_pid.get(&pid) {
            return Some((*found).to_string());
        }
        match parent_of.get(&pid) {
            Some(parent) if *parent != pid && *parent > 1 => pid = *parent,
            _ => return None,
        }
    }
    None
}

/// How long a hook's reading outranks the screen heuristic. Long enough to
/// bridge the gap to the next hook or probe round, short enough that a
/// partial hook set (Notification only) cannot pin a stale badge for long.
const HOOK_ATTENTION_TTL: Duration = Duration::from_secs(20);

/// Map a Claude Code hook onto pane attention. `event` is the payload's
/// `hook_event_name`; `notification_type` the Notification kind. `None` =
/// nothing to say (the hook is acknowledged and ignored).
fn attention_from_hook(event: &str, notification_type: Option<&str>) -> Option<AgentAttention> {
    match event.trim() {
        "Notification" => match notification_type.map(str::trim) {
            Some("permission_prompt")
            | Some("idle_prompt")
            | Some("elicitation_dialog")
            | Some("agent_needs_input")
            | Some("needs_input") => Some(AgentAttention::NeedsInput),
            _ => None,
        },
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SubagentStart" => {
            Some(AgentAttention::Working)
        }
        "Stop" | "SubagentStop" => Some(AgentAttention::Idle),
        _ => None,
    }
}

fn map_probe_entries(
    entries: &[AgentProbeEntry],
    parent_of: &HashMap<u32, u32>,
    pane_pids: &[(String, u32)],
) -> HashMap<String, AgentAttention> {
    fn rank(attention: AgentAttention) -> u8 {
        match attention {
            AgentAttention::NeedsInput => 2,
            AgentAttention::Working => 1,
            AgentAttention::Idle => 0,
        }
    }
    let mut mapped: HashMap<String, AgentAttention> = HashMap::new();
    for entry in entries {
        let (Some(pid), Some(attention)) = (entry.pid, attention_from_probe(entry)) else {
            continue;
        };
        let Some(pane) = pane_for_pid(pid, parent_of, pane_pids) else {
            continue;
        };
        let keep = mapped
            .get(&pane)
            .is_none_or(|current| rank(attention) > rank(*current));
        if keep {
            mapped.insert(pane, attention);
        }
    }
    mapped
}

/// Bookkeeping between probe rounds: `previous` holds panes with an official
/// reading and how many rounds in a row they have been missing from the
/// listing. Returns the panes whose reading should now be cleared (missing
/// twice, or no longer live).
fn reconcile_probe_rounds(
    previous: &mut HashMap<String, u8>,
    mapped: &HashMap<String, AgentAttention>,
    live_panes: &[String],
) -> Vec<String> {
    let mut clear = Vec::new();
    for pane_id in mapped.keys() {
        previous.insert(pane_id.clone(), 0);
    }
    previous.retain(|pane_id, misses| {
        if mapped.contains_key(pane_id) {
            return true;
        }
        if !live_panes.iter().any(|live| live == pane_id) {
            clear.push(pane_id.clone());
            return false;
        }
        *misses = misses.saturating_add(1);
        if *misses >= 2 {
            clear.push(pane_id.clone());
            return false;
        }
        true
    });
    clear
}

struct DaemonServer {
    registry: Mutex<PaneRegistry>,
    terminals: Mutex<TerminalStore>,
    layout: Mutex<Option<Value>>,
    persist_path: PathBuf,
    scrollback_dir: PathBuf,
    /// (T2) Per-pane agent conversation logs (`agents/<pane-id>.jsonl`), read
    /// for the bootstrap replay.
    agents_dir: PathBuf,
    /// Per-pane hash-chained ledgers (`ledger/<pane-id>.jsonl`), shared with
    /// the router; kept across pane close. docs/design/keyboard-lease-and-ledger.md.
    ledger: Arc<Mutex<LedgerSink>>,
    /// Held keyboard leases. A LEAF lock: taken alone, never while holding
    /// registry/terminals, and dropped before either is acquired (persist()
    /// takes it last, after registry → terminals).
    leases: Arc<Mutex<HashMap<String, HeldLease>>>,
    workspace_key: String,
    log_dispatch: tracing::dispatcher::Dispatch,
    /// Keeps the non-blocking log writer alive (flushes on drop). Must be held for
    /// the lifetime of the server so log entries are not lost.
    _log_guard: WorkerGuard,
    token: String,
    router: OutputRouter,
    config: RwLock<Config>,
    sync_input: AtomicBool,
    spawn_on_bootstrap: Mutex<bool>,
    shutdown: AtomicBool,
    /// Daemon construction instant; used for monotonic uptime in `status --verbose`.
    started_at: Instant,
    /// Set by handlers whose state changes are too frequent to fsync individually
    /// (resize, focus); the accept loop flushes it on a LAZY_PERSIST_INTERVAL cadence.
    /// Arc'd so agent reader threads can flag a newly-recorded CLI session id
    /// for lazy persist too (T2).
    dirty: Arc<AtomicBool>,
    /// Serializes every write to workspace.json AND config.json (H1): both go
    /// through fixed temp paths (workspace.json.tmp / config.json.tmp), so two
    /// concurrent persists (e.g. the accept loop's lazy flush racing a handler's
    /// immediate persist) could interleave open/write/rename and tear the file.
    /// Lock ordering: this is the OUTERMOST lock — it is only ever taken first,
    /// before (never after) registry → terminals → layout, and no path takes it
    /// while already holding one of those.
    persist_lock: Mutex<()>,
    /// Paired with TerminalStore::spawning (M7): a thread about to spawn a pane
    /// marks the pane in-flight and runs the fork/exec WITHOUT the store lock;
    /// a concurrent ensure for the same pane waits here for the commit instead
    /// of double-spawning. Notified on every in-flight removal.
    spawn_cvar: Condvar,
    /// True when the persisted workspace.json was present but unparseable. The
    /// daemon fell back to a fresh workspace; this flag surfaces a warning in
    /// `run_daemon_with_config` after the tracing dispatcher is active.
    workspace_was_corrupt: bool,
    /// (M3b) The official agent probe warns once about a failing `claude`
    /// invocation and then only logs at debug level.
    probe_warned: AtomicBool,
    /// (M3b) Panes with an official reading → consecutive rounds missing from
    /// the listing (see `reconcile_probe_rounds`). Leaf lock.
    probe_mapped: Mutex<HashMap<String, u8>>,
    /// (M4) Panes bound to a Kranz mission. Leaf lock.
    kranz_bindings: Mutex<HashMap<String, KranzBinding>>,
    /// Named pane groups (name → project). Leaf lock; persist() takes it after
    /// registry → terminals → leases.
    projects: Mutex<HashMap<String, Project>>,
    /// Per-pane usage from status-line payloads (`ctl statusline`). Not
    /// persisted: a fresh daemon waits for the next turn. Leaf lock.
    agent_usage: Mutex<HashMap<String, AgentUsage>>,
    /// (M6) Issued client credentials, mirrored to `clients.json`. Leaf lock.
    clients: Mutex<ClientsFile>,
    clients_path: PathBuf,
    /// The next lease generation (see `HeldLease::generation`); seeded above
    /// every persisted lease so numbers never repeat across restarts.
    next_lease_generation: AtomicU64,
}

/// Removes a pane's in-flight spawn marker and wakes any ensure/restart
/// waiting on it, on EVERY exit path from the spawn (including a panic while
/// the fork/exec ran off-lock) — a leaked marker would wedge later ensures of
/// the same pane behind `spawn_cvar` forever (M7).
struct SpawnInFlight<'a> {
    server: &'a DaemonServer,
    pane_id: String,
}

impl Drop for SpawnInFlight<'_> {
    fn drop(&mut self) {
        if let Ok(mut terminals) = self.server.lock_terminals() {
            terminals.spawning.remove(&self.pane_id);
        }
        self.server.spawn_cvar.notify_all();
    }
}

impl DaemonServer {
    /// Construct with an explicit config. Tests use this so they never read the
    /// developer's real global config.json; `run_daemon_with_config` is the
    /// production path that loads config from disk then delegates here.
    fn with_config(cwd: PathBuf, data_dir: PathBuf, config: Config) -> Result<Self, String> {
        ensure_private_dir(&data_dir)?;
        let scrollback_dir = data_dir.join(SCROLLBACK_DIR);
        ensure_private_dir(&scrollback_dir)?;
        // (T2) The agent conversation-log dir. Logs survive daemon restarts
        // and in-place pane restarts but are deleted on pane close (M3);
        // startup prunes temp litter here and orphans alongside the
        // scrollback orphan sweep below.
        let agents_dir = data_dir.join(AGENT_LOG_DIR);
        ensure_private_dir(&agents_dir)?;
        prune_agent_log_temps(&agents_dir);
        // Ledgers are never pruned: a closed pane's ledger is its record.
        let ledger_dir = data_dir.join(LEDGER_DIR);
        ensure_private_dir(&ledger_dir)?;
        let ledger = Arc::new(Mutex::new(LedgerSink::new(ledger_dir)));
        let token = load_or_create_token(&data_dir)?;
        let clients_path = data_dir.join(CLIENTS_FILE);
        let clients = load_clients_file(&clients_path);

        let persist_path = data_dir.join(WORKSPACE_FILE);
        let loaded = load_workspace(&persist_path, cwd.display().to_string());

        // Defense-in-depth: verify the persisted cwd matches the connecting cwd.
        // The client (DaemonClient) also checks before connecting, but a daemon
        // spawned directly (e.g. via --daemon args) must still refuse a mismatched
        // workspace rather than silently serving another workspace's data.
        if let Some(ref persisted_cwd) = loaded.persisted_cwd {
            let connecting = cwd.display().to_string();
            if !persisted_cwd.is_empty() && !workspace_cwds_match(Path::new(persisted_cwd), &cwd) {
                return Err(format!(
                    "workspace_key collision detected: the persisted workspace cwd '{}' does not \
                     match the connecting cwd '{}'; refusing to serve mismatched workspace data. \
                     If this is intentional, remove the workspace data for this key.",
                    persisted_cwd, connecting
                ));
            }
        }

        let (
            registry,
            sizes,
            layout,
            restored_from_disk,
            pane_states,
            agents,
            agents_v2,
            agent_specs,
            pane_shells,
            leases,
            projects,
            was_corrupt,
        ) = (
            loaded.registry,
            loaded.sizes,
            loaded.layout,
            loaded.restored_from_disk,
            loaded.pane_states,
            loaded.agents,
            loaded.agents_v2,
            loaded.agent_specs,
            loaded.pane_shells,
            loaded.leases,
            loaded.projects,
            loaded.was_corrupt,
        );

        // Remove scrollback for panes no longer in the registry (e.g. a file briefly
        // recreated by a draining reader racing ClosePane), so orphans can't accumulate.
        // Startup is also the only safe time to drop `.ansi.tmp` cap litter: no cap
        // can be in flight yet (the runtime sweep passes include_temps=false).
        let live_pane_ids: HashSet<String> =
            registry.panes.iter().map(|pane| pane.id.clone()).collect();
        prune_orphan_scrollback(&scrollback_dir, &live_pane_ids, true);
        // (T2) M3: same for agent conversation logs (a log briefly recreated
        // by a draining reader racing ClosePane must not accumulate either).
        prune_orphan_agent_logs(&agents_dir, &live_pane_ids);

        let ws_key = workspace_key(&cwd);

        // Set up structured logging via tracing + tracing-appender. The log file
        // lives in the workspace data dir (per-workspace isolation), is opened in
        // append mode (history survives restart), and is rotated on startup if it
        // exceeds LOG_MAX_BYTES (bounded growth). A non-blocking writer ensures
        // logging is best-effort and never panics the daemon.
        let (log_writer, log_guard) = setup_log_writer(&data_dir);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(log_writer)
            .with_ansi(false)
            .with_target(false)
            .finish();
        let log_dispatch = dispatcher::Dispatch::new(subscriber);

        let router = OutputRouter::new(scrollback_dir.clone());
        router.set_log_context(log_dispatch.clone(), ws_key.clone());
        router.set_ledger(Arc::clone(&ledger));
        // (T1) Restore persisted manual agent marks before any pane spawns, so
        // the first classification of a marked pane keeps its agent. Filtered
        // against the registry (L6b — mirroring the agents_v2 seed filter
        // below): a hand-edited mark for a pane that no longer exists must
        // not be resurrected.
        router.seed_manual_agents(
            agents
                .into_iter()
                .filter(|(pane_id, _)| live_pane_ids.contains(pane_id))
                .collect(),
        );
        // (T2) The lazy-persist flag is shared with agent reader threads: a
        // newly-recorded CLI session id must reach workspace.json within one
        // persist cadence (crash loses it otherwise), not only at shutdown.
        let dirty = Arc::new(AtomicBool::new(false));
        let mut terminals = TerminalStore::new(
            cwd,
            router.clone(),
            sizes,
            config.shell_config(),
            config.agent_config(),
            agents_dir.clone(),
            Arc::clone(&dirty),
            agent_specs
                .into_iter()
                .filter(|(pane_id, _)| live_pane_ids.contains(pane_id))
                .collect(),
        );
        terminals.pane_shells = pane_shells
            .into_iter()
            .filter(|(pane_id, _)| live_pane_ids.contains(pane_id))
            .collect();
        for pane in &registry.panes {
            if pane.kind == PaneKind::Agent {
                terminals.agent_specs.entry(pane.id.clone()).or_default();
            }
        }
        // (T2) Seed the CLI session ids persisted in agents_v2 so a respawned
        // agent pane resumes its conversation. Pruned to ids that are both
        // well-formed and present in the registry (a tampered map must not
        // drive a --resume for a pane that doesn't exist).
        #[cfg(any(unix, windows))]
        {
            let registry_ids: HashSet<String> =
                registry.panes.iter().map(|pane| pane.id.clone()).collect();
            terminals.agent_resume = agents_v2
                .into_iter()
                .filter(|(pane_id, _)| registry_ids.contains(pane_id))
                .collect();
        }
        #[cfg(not(any(unix, windows)))]
        drop(agents_v2);

        // Seed the liveness map with persisted Ended entries so a pane that ended
        // before a daemon restart is still reported as Ended on bootstrap (and via
        // subscribe catch-up) — its ended-ness is not silently lost. When a pane is
        // later spawned (auto_respawn or explicit ensure), spawn_pane replaces the
        // entry with ended:false, making it Live.
        let ended_pane_ids: Vec<String> = pane_states
            .iter()
            .filter(|(_, state)| **state == PaneRuntimeState::Ended)
            .map(|(pane_id, _)| pane_id.clone())
            .collect();
        terminals.seed_ended_panes(&ended_pane_ids);

        // Determine whether panes should be auto-spawned on the first BootstrapWorkspace
        // call. A fresh workspace always spawns (to seed the initial live pane). A
        // restored workspace spawns only under `auto_respawn` (the default); under
        // `restore_on_demand`, ended panes are listed but not auto-restarted.
        let policy_is_auto = config.restore_policy_effective() == "auto_respawn";
        let should_spawn_on_bootstrap = !restored_from_disk || policy_is_auto;

        let leases: HashMap<String, HeldLease> = leases
            .into_iter()
            .filter(|(pane_id, _)| live_pane_ids.contains(pane_id))
            .collect();
        let next_lease_generation = leases
            .values()
            .map(|held| held.generation)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        // Shared with the router so `pane.ended` can name the holder at exit.
        let leases = Arc::new(Mutex::new(leases));
        router.set_leases(Arc::clone(&leases));
        Ok(Self {
            registry: Mutex::new(registry),
            terminals: Mutex::new(terminals),
            layout: Mutex::new(layout),
            persist_path,
            scrollback_dir,
            agents_dir,
            ledger,
            // A hand-edited lease for a pane that no longer exists must not be
            // resurrected (same filter as the agent marks above).
            leases,
            next_lease_generation: AtomicU64::new(next_lease_generation),
            workspace_key: ws_key,
            log_dispatch,
            _log_guard: log_guard,
            token,
            router,
            config: RwLock::new(config),
            sync_input: AtomicBool::new(false),
            spawn_on_bootstrap: Mutex::new(should_spawn_on_bootstrap),
            shutdown: AtomicBool::new(false),
            started_at: Instant::now(),
            dirty,
            persist_lock: Mutex::new(()),
            spawn_cvar: Condvar::new(),
            workspace_was_corrupt: was_corrupt,
            probe_warned: AtomicBool::new(false),
            probe_mapped: Mutex::new(HashMap::new()),
            kranz_bindings: Mutex::new(HashMap::new()),
            agent_usage: Mutex::new(HashMap::new()),
            clients: Mutex::new(clients),
            clients_path,
            // Members that no longer exist are dropped on load, like leases.
            projects: Mutex::new(
                projects
                    .into_iter()
                    .map(|(name, mut project)| {
                        project
                            .panes
                            .retain(|pane_id| live_pane_ids.contains(pane_id));
                        (name, project)
                    })
                    .collect(),
            ),
        })
    }

    // ----- Projects (docs/design/keyboard-lease-and-ledger.md §7) -----

    fn lock_projects(&self) -> Result<MutexGuard<'_, HashMap<String, Project>>, String> {
        self.projects
            .lock()
            .map_err(|_| "project lock poisoned".to_string())
    }

    fn projects_snapshot(&self) -> HashMap<String, Project> {
        self.lock_projects()
            .map(|projects| projects.clone())
            .unwrap_or_default()
    }

    fn handle_project_create(
        &self,
        name: &str,
        goal: Option<String>,
        repo: Option<String>,
    ) -> Result<Value, String> {
        let name = validate_project_name(name)?;
        let goal = match goal {
            Some(goal) => Some(validate_bounded_text(&goal, "goal", 4096)?),
            None => None,
        };
        let repo = match repo {
            Some(repo) => Some(validate_bounded_text(&repo, "repo", 4096)?),
            None => None,
        };
        let project = Project {
            name: name.clone(),
            goal,
            repo,
            panes: Vec::new(),
            created_at_ms: now_millis(),
        };
        {
            let mut projects = self.lock_projects()?;
            if projects.len() >= MAX_PROJECTS {
                return Err(format!("project limit reached ({MAX_PROJECTS})"));
            }
            if projects.contains_key(&name) {
                return Err(format!("project '{name}' already exists"));
            }
            projects.insert(name.clone(), project.clone());
        }
        if let Err(error) = self.persist() {
            let _ = self
                .lock_projects()
                .map(|mut projects| projects.remove(&name));
            return Err(error);
        }
        self.broadcast_projects();
        Ok(json!(project))
    }

    fn handle_project_delete(&self, name: &str) -> Result<Value, String> {
        let removed = self.lock_projects()?.remove(name);
        let Some(project) = removed else {
            return Err(format!("unknown project '{name}'"));
        };
        for pane_id in &project.panes {
            let _ = self.ledger_record(pane_id, "project.unassigned", json!({ "project": name }));
        }
        if let Err(error) = self.persist() {
            let _ = self
                .lock_projects()
                .map(|mut projects| projects.insert(name.to_string(), project.clone()));
            return Err(error);
        }
        self.broadcast_projects();
        Ok(json!(project))
    }

    /// Put a pane in a project (a pane belongs to at most one; moving it
    /// leaves the previous project). Both sides are ledgered on the pane so
    /// its record shows which project it served.
    fn handle_project_assign(&self, name: &str, pane_id: &str) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;
        let previous = {
            let mut projects = self.lock_projects()?;
            if !projects.contains_key(name) {
                return Err(format!("unknown project '{name}'"));
            }
            let mut previous = None;
            for (other_name, project) in projects.iter_mut() {
                if other_name != name && project.panes.iter().any(|id| id == pane_id) {
                    project.panes.retain(|id| id != pane_id);
                    previous = Some(other_name.clone());
                }
            }
            let project = projects.get_mut(name).expect("checked above");
            if !project.panes.iter().any(|id| id == pane_id) {
                project.panes.push(pane_id.to_string());
            }
            previous
        };
        if let Some(previous) = &previous {
            let _ = self.ledger_record(
                pane_id,
                "project.unassigned",
                json!({ "project": previous }),
            );
        }
        let _ = self.ledger_record(pane_id, "project.assigned", json!({ "project": name }));
        self.persist()?;
        self.broadcast_projects();
        Ok(json!({ "pane_id": pane_id, "project": name, "previous": previous }))
    }

    fn handle_project_unassign(&self, pane_id: &str) -> Result<Value, String> {
        let mut left = None;
        {
            let mut projects = self.lock_projects()?;
            for (name, project) in projects.iter_mut() {
                if project.panes.iter().any(|id| id == pane_id) {
                    project.panes.retain(|id| id != pane_id);
                    left = Some(name.clone());
                }
            }
        }
        if let Some(name) = &left {
            let _ = self.ledger_record(pane_id, "project.unassigned", json!({ "project": name }));
            self.persist()?;
            self.broadcast_projects();
        }
        Ok(json!({ "pane_id": pane_id, "project": left }))
    }

    /// Drop a closed pane from its project (the caller persists). Returns
    /// whether any project changed so the caller can announce it.
    fn forget_pane_in_projects(&self, pane_id: &str) -> bool {
        let mut changed = false;
        if let Ok(mut projects) = self.lock_projects() {
            for project in projects.values_mut() {
                let before = project.panes.len();
                project.panes.retain(|id| id != pane_id);
                changed |= project.panes.len() != before;
            }
        }
        changed
    }

    /// Tell subscribers the project table changed. Called with no lock held.
    fn broadcast_projects(&self) {
        self.router.broadcast(&DaemonEvent::ProjectsChanged {
            projects: self.projects_snapshot(),
        });
    }

    fn project_summaries(&self) -> Result<Vec<ProjectSummary>, String> {
        let projects = self.projects_snapshot();
        let pane_ids: Vec<String> = projects
            .values()
            .flat_map(|project| project.panes.iter().cloned())
            .collect();
        let states = self.lock_terminals()?.runtime_states(&pane_ids);
        let agents = self.router.agent_info_map();
        let leases = self.lock_leases()?.clone();
        let mut summaries: Vec<ProjectSummary> = projects
            .values()
            .map(|project| project_rollup(project, &states, &agents, &leases))
            .collect();
        summaries.sort_by(|a, b| a.project.name.cmp(&b.project.name));
        Ok(summaries)
    }

    fn handle_project_show(&self, name: &str) -> Result<Value, String> {
        let summary = self
            .project_summaries()?
            .into_iter()
            .find(|summary| summary.project.name == name)
            .ok_or_else(|| format!("unknown project '{name}'"))?;
        let registry = self.lock_registry()?;
        let titles: HashMap<String, (String, PaneKind)> = registry
            .panes
            .iter()
            .map(|pane| (pane.id.clone(), (pane.title.clone(), pane.kind)))
            .collect();
        drop(registry);
        let states = self
            .lock_terminals()?
            .runtime_states(&summary.project.panes);
        let agents = self.router.agent_info_map();
        let leases = self.lock_leases()?.clone();
        let bindings = self.kranz_bindings_snapshot();
        let panes: Vec<Value> = summary
            .project
            .panes
            .iter()
            .map(|pane_id| {
                let (title, kind) = titles
                    .get(pane_id)
                    .cloned()
                    .unwrap_or_else(|| (pane_id.clone(), PaneKind::Shell));
                json!({
                    "id": pane_id,
                    "title": title,
                    "kind": kind,
                    "state": states.get(pane_id).copied().unwrap_or(PaneRuntimeState::Ended),
                    "agent": agents.get(pane_id).cloned().unwrap_or_default(),
                    "holder": leases.get(pane_id).map(|held| held.holder.clone()),
                    "kranz": bindings.get(pane_id).cloned(),
                })
            })
            .collect();
        Ok(json!({ "summary": summary, "panes": panes }))
    }

    /// The project's members' ledgers merged in time order: the project-level
    /// "who did what" a single pane's ledger cannot show.
    fn handle_project_ledger(&self, name: &str, limit: usize) -> Result<Value, String> {
        let project = self
            .projects_snapshot()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown project '{name}'"))?;
        let dir = self
            .ledger
            .lock()
            .map_err(|_| "ledger lock poisoned".to_string())?
            .dir
            .clone();
        let mut records: Vec<Value> = project
            .panes
            .iter()
            .flat_map(|pane_id| read_ledger_tail(&ledger_path(&dir, pane_id), 0))
            .collect();
        records.sort_by_key(|record| {
            (
                record["ts_ms"].as_u64().unwrap_or(0),
                record["pane_id"].as_str().unwrap_or("").to_string(),
                record["seq"].as_u64().unwrap_or(0),
            )
        });
        let limit = match limit {
            0 => PROJECT_LEDGER_DEFAULT_LIMIT,
            n => n.min(PROJECT_LEDGER_MAX_LIMIT),
        };
        let total = records.len();
        if total > limit {
            records = records.split_off(total - limit);
        }
        Ok(json!({ "project": name, "total": total, "records": records }))
    }

    fn agent_usage_snapshot(&self) -> HashMap<String, AgentUsage> {
        self.agent_usage
            .lock()
            .map(|usage| usage.clone())
            .unwrap_or_default()
    }

    /// A status-line payload from a Claude Code session under one of this
    /// daemon's panes. Placed like a hook (process ancestry), stored per
    /// pane, broadcast as `agent_usage`. Also proof that Claude is running
    /// there: a pane with no agent mark gets one (attention untouched; the
    /// status line says nothing about that). Never an error.
    fn handle_agent_status(&self, pid: u32, payload: &Value) -> Value {
        let Some(mut usage) = AgentUsage::from_status_payload(payload) else {
            return json!({ "mapped": false, "reason": "payload carries nothing we keep" });
        };
        let Some(parent_of) = process_parent_snapshot_for_hooks() else {
            return json!({ "mapped": false, "reason": "no process tree on this platform" });
        };
        let pane_pids = match self.lock_terminals() {
            Ok(terminals) => terminals.live_pane_pids(),
            Err(_) => return json!({ "mapped": false, "reason": "terminal store unavailable" }),
        };
        let Some(pane_id) = pane_for_pid(pid, &parent_of, &pane_pids) else {
            return json!({ "mapped": false, "reason": format!("no live pane owns pid {pid}") });
        };
        usage.updated_at_ms = now_millis();
        let changed = match self.agent_usage.lock() {
            Ok(mut table) => {
                let same = table.get(&pane_id).is_some_and(|previous| {
                    AgentUsage {
                        updated_at_ms: 0,
                        ..previous.clone()
                    } == AgentUsage {
                        updated_at_ms: 0,
                        ..usage.clone()
                    }
                });
                table.insert(pane_id.clone(), usage.clone());
                !same
            }
            Err(_) => return json!({ "mapped": false, "reason": "usage table unavailable" }),
        };
        self.router.mark_agent_present(&pane_id, "claude");
        if changed {
            self.router.broadcast(&DaemonEvent::AgentUsage {
                pane_id: pane_id.clone(),
                usage: usage.clone(),
            });
        }
        json!({ "mapped": true, "pane_id": pane_id, "usage": usage, "changed": changed })
    }

    /// (M3) A Claude Code hook fired somewhere under one of this daemon's
    /// panes. Walk the hook process's ancestry to the pane and apply the
    /// hook's reading as official attention (evidence `hook`), which
    /// outranks the screen heuristic for `HOOK_ATTENTION_TTL`. Notification
    /// hooks (a person is wanted) are also ledgered with their message so a
    /// dossier shows what the agent was waiting for. Never an error: a hook
    /// that cannot be placed reports `mapped: false` and a reason, and the
    /// session it came from is unaffected. Kranz-bound panes need no relay
    /// here: a Kranz run registers its own `kranz hook-status` hooks.
    fn handle_agent_signal(
        &self,
        pid: u32,
        event: &str,
        notification_type: Option<&str>,
        message: Option<&str>,
        session_id: Option<&str>,
    ) -> Value {
        let Some(attention) = attention_from_hook(event, notification_type) else {
            return json!({ "mapped": false, "reason": format!("hook event '{event}' carries no attention") });
        };
        let Some(parent_of) = process_parent_snapshot_for_hooks() else {
            return json!({ "mapped": false, "reason": "no process tree on this platform" });
        };
        let pane_pids = match self.lock_terminals() {
            Ok(terminals) => terminals.live_pane_pids(),
            Err(_) => return json!({ "mapped": false, "reason": "terminal store unavailable" }),
        };
        let Some(pane_id) = pane_for_pid(pid, &parent_of, &pane_pids) else {
            return json!({ "mapped": false, "reason": format!("no live pane owns pid {pid}") });
        };
        self.router.apply_official_attention_with(
            &pane_id,
            "claude",
            attention,
            HOOK_ATTENTION_TTL,
            "hook",
        );
        if attention == AgentAttention::NeedsInput {
            // Everything ledgered from a hook payload is bounded: the ledger
            // is append-only and a hook is any process's stdin.
            let clip = |text: &str, max: usize| text.chars().take(max).collect::<String>();
            let _ = self.ledger_record(
                &pane_id,
                "hook.received",
                json!({
                    "event": clip(event, 64),
                    "notification_type": notification_type.map(|t| clip(t, 64)),
                    "message": message.map(|t| clip(t, 200)),
                    "session_id": session_id.map(|t| clip(t, 128)),
                    "attention": attention,
                }),
            );
        }
        json!({ "mapped": true, "pane_id": pane_id, "attention": attention, "evidence": "hook" })
    }

    /// One JSON document a reviewer or a Kranz gate can consume without
    /// touching the daemon again: `project show` plus, per member pane, the
    /// whole ledger (chain verified, break named) and the last `lines` of
    /// scrollback with controls stripped and 1-based line numbers a record
    /// can cite. Ledgers hold counts and transitions, never keystrokes; the
    /// scrollback tail is the only output in the document.
    fn handle_project_dossier(&self, name: &str, lines: usize) -> Result<Value, String> {
        let detail = self.handle_project_show(name)?;
        let dir = self
            .ledger
            .lock()
            .map_err(|_| "ledger lock poisoned".to_string())?
            .dir
            .clone();
        let lines = match lines {
            0 => PROJECT_DOSSIER_DEFAULT_LINES,
            n => n.min(SCROLLBACK_LINES_MAX_PER_REQUEST),
        };
        let panes: Vec<Value> = detail["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|mut pane| {
                let pane_id = pane["id"].as_str().unwrap_or("").to_string();
                let path = ledger_path(&dir, &pane_id);
                let chain = if path.exists() {
                    match ledger_verify(&path) {
                        Ok(summary) => json!({
                            "verified": true,
                            "records": summary.records,
                            "head": summary.head,
                        }),
                        Err(broken) => json!({ "verified": false, "break": broken }),
                    }
                } else {
                    json!({ "verified": true, "records": 0, "head": "" })
                };
                let records = read_ledger_tail(&path, 0);
                let all = scrollback_text_lines(&self.scrollback_dir, &pane_id);
                let total = all.len();
                let start = total.saturating_sub(lines);
                pane["ledger"] = json!({ "chain": chain, "records": records });
                pane["scrollback"] = json!({
                    "total_lines": total,
                    "from": start + 1,
                    "to": total,
                    "lines": all[start..],
                });
                pane
            })
            .collect();
        Ok(json!({
            "format": PROJECT_DOSSIER_FORMAT,
            "generated_at_ms": now_millis(),
            "workspace": self.workspace_key,
            "summary": detail["summary"],
            "panes": panes,
        }))
    }

    // ----- Kranz bindings (M4, docs/design/keyboard-lease-and-ledger.md) -----

    fn lock_kranz(&self) -> Result<MutexGuard<'_, HashMap<String, KranzBinding>>, String> {
        self.kranz_bindings
            .lock()
            .map_err(|_| "kranz binding lock poisoned".to_string())
    }

    fn handle_kranz_bind(&self, pane_id: &str, repo: Option<String>) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;
        let repo = match repo {
            Some(repo) => validate_bounded_text(&repo, "repo", 4096)?,
            None => self
                .lock_terminals()?
                .pane_cwd(pane_id)
                .ok_or_else(|| format!("pane {pane_id} has no recorded cwd; pass --repo PATH"))?,
        };
        let binding = KranzBinding { repo, manual: true };
        self.lock_kranz()?
            .insert(pane_id.to_string(), binding.clone());
        let _ = self.ledger_record(
            pane_id,
            "kranz.bound",
            json!({ "repo": binding.repo, "manual": true }),
        );
        Ok(json!({ "pane_id": pane_id, "binding": binding }))
    }

    fn handle_kranz_unbind(&self, pane_id: &str) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;
        let removed = self.lock_kranz()?.remove(pane_id);
        if let Some(binding) = &removed {
            let _ = self.ledger_record(
                pane_id,
                "kranz.unbound",
                json!({ "repo": binding.repo, "manual": binding.manual }),
            );
        }
        Ok(json!({ "pane_id": pane_id, "binding": removed }))
    }

    fn kranz_bindings_snapshot(&self) -> HashMap<String, KranzBinding> {
        self.lock_kranz()
            .map(|bindings| bindings.clone())
            .unwrap_or_default()
    }

    /// (M4) Reconcile auto bindings with this round's process table: bind
    /// panes that gained a `kranz run` descendant (repo = the pane's cwd) and
    /// drop auto bindings whose worker is gone or whose pane is not live.
    /// Manual bindings are left alone. Returns every current binding.
    fn reconcile_kranz_bindings(
        &self,
        workers: &HashMap<String, u32>,
        live_cwds: &HashMap<String, String>,
    ) -> Vec<(String, KranzBinding)> {
        let Ok(mut bindings) = self.lock_kranz() else {
            return Vec::new();
        };
        let mut newly = Vec::new();
        for pane_id in workers.keys() {
            if bindings.contains_key(pane_id) {
                continue;
            }
            let Some(repo) = live_cwds.get(pane_id) else {
                continue;
            };
            bindings.insert(
                pane_id.clone(),
                KranzBinding {
                    repo: repo.clone(),
                    manual: false,
                },
            );
            newly.push((pane_id.clone(), repo.clone()));
        }
        bindings.retain(|pane_id, binding| {
            binding.manual || (workers.contains_key(pane_id) && live_cwds.contains_key(pane_id))
        });
        let current: Vec<(String, KranzBinding)> = bindings
            .iter()
            .map(|(pane_id, binding)| (pane_id.clone(), binding.clone()))
            .collect();
        drop(bindings);
        for (pane_id, repo) in newly {
            tracing::info!(
                workspace_key = %self.workspace_key,
                pane_id = %pane_id,
                repo = %repo,
                event = "kranz_bound",
                "Kranz worker found under pane; mission state now drives its badge"
            );
            let _ = self.ledger_record(
                &pane_id,
                "kranz.bound",
                json!({ "repo": repo, "manual": false }),
            );
        }
        current
    }

    /// (M4) Read a bound mission's state through the CLI (`kranz status
    /// --json`, read-only, no lock) and apply it as an official reading.
    #[cfg(unix)]
    fn probe_kranz_binding(&self, pane_id: &str, binding: &KranzBinding, ttl: Duration) {
        let bin = self.effective_config().kranz_bin_effective();
        let output = match Command::new(&bin)
            .args(["--repo", &binding.repo, "status", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            Ok(output) => {
                self.note_probe_failure(&format!(
                    "`{bin} --repo {} status --json` exited {}",
                    binding.repo, output.status
                ));
                return;
            }
            Err(error) => {
                self.note_probe_failure(&format!("cannot run `{bin} status --json`: {error}"));
                return;
            }
        };
        let state: Value = match serde_json::from_slice(&output) {
            Ok(state) => state,
            Err(error) => {
                self.note_probe_failure(&format!("unreadable `kranz status --json`: {error}"));
                return;
            }
        };
        if let Some(attention) = kranz_attention_from_state(&state) {
            self.router.apply_official_attention_with(
                pane_id,
                "kranz",
                attention,
                ttl,
                "kranz-status",
            );
        }
    }

    /// (M4) Mirror a hand-back note into the bound mission's inbox via
    /// `kranz msg`, and ledger the outcome either way. The release itself has
    /// already succeeded; a failed mirror is recorded, never surfaced as an
    /// error.
    fn mirror_release_to_kranz(&self, pane_id: &str, holder: &str, note: &str) {
        let binding = match self.lock_kranz() {
            Ok(bindings) => bindings.get(pane_id).cloned(),
            Err(_) => None,
        };
        let Some(binding) = binding else {
            return;
        };
        let bin = self.effective_config().kranz_bin_effective();
        let text = format!("[sgian] {holder} handed back the keyboard: {note}");
        let outcome = Command::new(&bin)
            .args(["--repo", &binding.repo, "msg", &text])
            .stdin(Stdio::null())
            .output();
        let (ok, error) = match outcome {
            Ok(output) if output.status.success() => (true, None),
            Ok(output) => (
                false,
                Some(format!(
                    "{bin} msg exited {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
            ),
            Err(error) => (false, Some(format!("cannot run {bin}: {error}"))),
        };
        if let Some(error) = &error {
            tracing::warn!(
                workspace_key = %self.workspace_key,
                pane_id = %pane_id,
                event = "kranz_mirror_failed",
                error = %error,
                "hand-back note was not mirrored to kranz"
            );
        }
        let _ = self.ledger_record(
            pane_id,
            "kranz.mirrored",
            json!({ "repo": binding.repo, "ok": ok, "error": error }),
        );
    }

    /// (M3b) One round of the official agent probe: run `claude agents --json`,
    /// attribute each session to a live shell pane through the process tree,
    /// and feed the result to the router as an official reading that outlives
    /// two probe intervals. Skipped entirely when no shell pane is live.
    #[cfg(unix)]
    fn run_agent_probe(&self, interval: Duration) {
        let pane_pids = match self.lock_terminals() {
            Ok(terminals) => terminals.live_pane_pids(),
            Err(_) => return,
        };
        if pane_pids.is_empty() {
            return;
        }
        let config = self.effective_config();
        let AgentBinPlan::Direct(bin) =
            resolve_provider_bin(&config.agent_config(), AgentBackendKind::Claude)
        else {
            return;
        };
        let output = match Command::new(&bin)
            .args(["agents", "--json"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            Ok(output) => {
                self.note_probe_failure(&format!("`{bin} agents --json` exited {}", output.status));
                return;
            }
            Err(error) => {
                self.note_probe_failure(&format!("cannot run `{bin} agents --json`: {error}"));
                return;
            }
        };
        let entries: Vec<AgentProbeEntry> = match serde_json::from_slice(&output) {
            Ok(entries) => entries,
            Err(error) => {
                self.note_probe_failure(&format!("unreadable `agents --json` output: {error}"));
                return;
            }
        };
        let table = match Command::new("ps")
            .args(["-axo", "pid=,ppid=,args="])
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => parse_process_table(&String::from_utf8_lossy(&output.stdout)),
            Err(error) => {
                self.note_probe_failure(&format!("cannot run ps: {error}"));
                return;
            }
        };
        let ttl = interval.saturating_mul(2) + Duration::from_millis(500);
        // (M4) Kranz workers under a pane bind it to their mission.
        let workers = find_kranz_panes(&table, &pane_pids);
        let live_cwds = self
            .lock_terminals()
            .map(|terminals| terminals.live_pane_cwds())
            .unwrap_or_default();
        for (pane_id, binding) in self.reconcile_kranz_bindings(&workers, &live_cwds) {
            self.probe_kranz_binding(&pane_id, &binding, ttl);
        }
        let mapped = map_probe_entries(&entries, &table.parent, &pane_pids);
        for (pane_id, attention) in &mapped {
            if self
                .router
                .apply_official_attention(pane_id, "claude", *attention, ttl)
            {
                tracing::info!(
                    workspace_key = %self.workspace_key,
                    pane_id = %pane_id,
                    event = "agent_probe_mapped",
                    "Claude Code session mapped to pane; its own state now drives the badge"
                );
            }
        }
        let live: Vec<String> = pane_pids
            .iter()
            .map(|(pane_id, _)| pane_id.clone())
            .collect();
        let cleared = match self.probe_mapped.lock() {
            Ok(mut previous) => reconcile_probe_rounds(&mut previous, &mapped, &live),
            Err(_) => Vec::new(),
        };
        for pane_id in cleared {
            self.router.clear_official_attention(&pane_id);
        }
    }

    #[cfg(unix)]
    fn note_probe_failure(&self, message: &str) {
        if !self.probe_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                workspace_key = %self.workspace_key,
                event = "agent_probe_failed",
                error = %message,
                "official agent probe failed; falling back to screen classification"
            );
        } else {
            tracing::debug!(
                workspace_key = %self.workspace_key,
                event = "agent_probe_failed",
                error = %message,
                "official agent probe failed"
            );
        }
    }

    /// Dispatch a request with no peer connection attached. Production paths
    /// call `handle_with_peer` with the serving stream; this convenience is
    /// `#[cfg(test)]` so no unused production path trips the dead-code lint.
    #[cfg(test)]
    fn handle(&self, request: DaemonRequest) -> Result<Value, String> {
        self.handle_with_peer(request, None)
    }

    /// Dispatch a request. `peer` is the serving connection, used only by the
    /// Wait handler to notice a client disconnect mid-wait (M6); tests and
    /// internal callers pass `None` via `handle`.
    fn handle_with_peer(
        &self,
        request: DaemonRequest,
        peer: Option<&TransportStream>,
    ) -> Result<Value, String> {
        match request {
            DaemonRequest::Ping => Ok(json!(CommandOk { ok: true })),
            DaemonRequest::BootstrapWorkspace => {
                let snapshot = self.snapshot()?;
                let pane_ids = snapshot
                    .panes
                    .iter()
                    .map(|pane| pane.id.clone())
                    .collect::<Vec<_>>();
                if self.take_spawn_on_bootstrap()? {
                    self.ensure_terminals(&pane_ids)?;
                }
                let mut snapshot = self.snapshot()?;
                let pane_ids = snapshot
                    .panes
                    .iter()
                    .map(|pane| pane.id.clone())
                    .collect::<Vec<_>>();
                snapshot.scrollback = self.read_scrollback_for(&pane_ids);
                self.persist()?;
                Ok(json!(snapshot))
            }
            DaemonRequest::ListPanes => Ok(json!(self.pane_list()?)),
            DaemonRequest::PaneStatus { pane_id } => Ok(json!(self.pane_status(&pane_id)?)),
            DaemonRequest::CreatePane { title, profile } => {
                let shell_override = if let Some(profile_name) = profile.as_deref() {
                    let config = self.effective_config();
                    let profile = config
                        .profile(profile_name)
                        .ok_or_else(|| format!("unknown profile '{profile_name}'"))?;
                    if profile.is_agent_profile() {
                        return Err(format!(
                            "profile '{profile_name}' is an agent profile; use create_agent_pane"
                        ));
                    }
                    Some(config.shell_config_for_profile(profile))
                } else {
                    None
                };
                let (pane, previous_active) = {
                    let mut registry = self.lock_registry()?;
                    // (M4) Cap the pane count: every pane costs a shell, a PTY,
                    // two threads, and a vt100 model, so an unbounded CreatePane
                    // loop exhausts PIDs/fds. A persisted workspace over the cap
                    // still LOADS — only new creation is refused.
                    if registry.panes.len() >= MAX_PANES {
                        return Err(format!(
                            "pane limit reached ({MAX_PANES}); close a pane before creating another"
                        ));
                    }
                    let previous_active = registry.active_pane_id.clone();
                    (registry.create_pane(title), previous_active)
                };
                tracing::info!(
                    workspace_key = %self.workspace_key,
                    pane_id = %pane.id,
                    event = "pane_create",
                    "pane created"
                );
                if let Some(shell) = shell_override {
                    self.lock_terminals()?
                        .pane_shells
                        .insert(pane.id.clone(), shell);
                }
                // Spawn → persist → announce: a failure reported to the client
                // must leave NOTHING committed behind (previously the pane was
                // broadcast and persisted before the error, so a client retry
                // duplicated it). On failure the create is rolled back — no
                // event was ever broadcast, so subscribers have nothing to
                // un-learn. The one accepted race: the fresh shell's first
                // output can now reach subscribers just before PaneCreated
                // (clients tolerate output for a not-yet-known pane).
                let spawned = self.ensure_terminal(&pane.id);
                if let Err(error) = spawned {
                    tracing::warn!(
                        workspace_key = %self.workspace_key,
                        pane_id = %pane.id,
                        event = "pane_end",
                        error = %error,
                        "pane shell spawn failed"
                    );
                    self.rollback_created_pane(&pane.id, previous_active);
                    return Err(error);
                }
                if let Err(error) = self.persist() {
                    self.rollback_created_pane(&pane.id, previous_active);
                    return Err(format!("failed to persist new pane: {error}"));
                }
                // A concurrent ClosePane during the fork/exec window already removed
                // the pane (the spawn commit discarded its session): skip the
                // announcement rather than emit PaneCreated AFTER PaneClosed.
                if self.lock_registry()?.contains_pane(&pane.id) {
                    self.router
                        .broadcast(&DaemonEvent::PaneCreated { pane: pane.clone() });
                }
                Ok(json!(pane))
            }
            DaemonRequest::ClosePane { pane_id } => {
                {
                    let mut registry = self.lock_registry()?;
                    registry.close_pane(&pane_id)?;
                }
                tracing::info!(
                    workspace_key = %self.workspace_key,
                    pane_id = %pane_id,
                    event = "pane_close",
                    "pane closed"
                );
                // Mark closed before teardown so a still-draining reader cannot
                // recreate the scrollback file we are about to delete.
                self.router.mark_closed(&pane_id);
                self.router.remove_model(&pane_id);
                // (T1) The pane's agent state (including a manual mark) dies
                // with it; pane ids are never reused.
                self.router.remove_agent(&pane_id);
                // So does its keyboard lease (ledgered as revoked; the ledger
                // file itself is kept).
                self.revoke_lease_on_close(&pane_id);
                let project_changed = self.forget_pane_in_projects(&pane_id);
                self.router.remove_output_guard(&pane_id);
                if let Ok(mut usage) = self.agent_usage.lock() {
                    usage.remove(&pane_id);
                }
                self.lock_terminals()?.close_pane(&pane_id);
                self.router.invalidate_append_handle(&pane_id);
                let _ = fs::remove_file(scrollback_path(&self.scrollback_dir, &pane_id));
                // (T2) M3: the conversation log dies with the pane (scrollback
                // parity — a closed pane can never resume: its agents_v2 and
                // resume seeds are dropped with it).
                let _ = fs::remove_file(agent_log_path(&self.agents_dir, &pane_id));
                self.persist()?;
                self.router.broadcast(&DaemonEvent::PaneClosed {
                    pane_id: pane_id.clone(),
                });
                if project_changed {
                    self.broadcast_projects();
                }
                // Return the daemon-enriched snapshot (runtime + provider
                // identity), not PaneRegistry's structural-only snapshot.
                // Otherwise closing any pane makes surviving Droid panes look
                // like legacy/default-Claude panes until the next bootstrap.
                let snapshot = self.snapshot()?;
                Ok(json!(snapshot))
            }
            DaemonRequest::RenamePane { pane_id, title } => {
                let pane = {
                    let mut registry = self.lock_registry()?;
                    registry.rename_pane(&pane_id, title)?
                };
                tracing::info!(
                    workspace_key = %self.workspace_key,
                    pane_id = %pane.id,
                    event = "pane_rename",
                    title = %pane.title,
                    "pane renamed"
                );
                self.persist()?;
                self.router
                    .broadcast(&DaemonEvent::PaneRenamed { pane: pane.clone() });
                Ok(json!(pane))
            }
            DaemonRequest::EnsurePaneTerminal { pane_id } => {
                self.ensure_pane_exists(&pane_id)?;
                // ensure_session spawns only when the pane has no live session, so
                // this is a no-op for a running pane and revives a pane whose
                // session exited (or a restored pane after a daemon restart) —
                // with the fork/exec off the store lock (M7). Kind-aware: agent
                // panes spawn their headless CLI, shell panes their PTY.
                self.ensure_session(&pane_id)?;
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::RestartPaneTerminal { pane_id } => {
                self.ensure_pane_exists(&pane_id)?;
                self.restart_session(&pane_id)?;
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::WriteToPane { pane_id, data } => {
                self.write_input(&pane_id, &data, None, None)?;
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::SendInput { pane_id, input } => {
                self.write_input(&pane_id, &input, None, None)?;
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::SendInputAs {
                pane_id,
                input,
                holder,
                generation,
            } => {
                let holder = validate_holder(&holder)?;
                self.write_input(&pane_id, &input, Some(&holder), generation)?;
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::TakeLease {
                pane_id,
                holder,
                force,
                why,
            } => self.handle_take_lease(&pane_id, &holder, force, why.as_deref(), None),
            DaemonRequest::ReleaseLease {
                pane_id,
                holder,
                note,
                generation,
            } => self.handle_release_lease(&pane_id, &holder, &note, generation, None),
            DaemonRequest::IdentityIssue { holder, scopes } => {
                self.handle_identity_issue(&holder, &scopes)
            }
            DaemonRequest::IdentityList => self.handle_identity_list(),
            DaemonRequest::IdentityRevoke { id } => self.handle_identity_revoke(&id),
            DaemonRequest::Whoami => {
                let policy = self.identity_policy();
                Ok(ClientIdentity::root(policy).describe(policy))
            }
            DaemonRequest::KranzBind { pane_id, repo } => self.handle_kranz_bind(&pane_id, repo),
            DaemonRequest::KranzUnbind { pane_id } => self.handle_kranz_unbind(&pane_id),
            DaemonRequest::KranzBindings => Ok(json!(self.kranz_bindings_snapshot())),
            DaemonRequest::ProjectCreate { name, goal, repo } => {
                self.handle_project_create(&name, goal, repo)
            }
            DaemonRequest::ProjectDelete { name } => self.handle_project_delete(&name),
            DaemonRequest::ProjectAssign { name, pane_id } => {
                self.handle_project_assign(&name, &pane_id)
            }
            DaemonRequest::ProjectUnassign { pane_id } => self.handle_project_unassign(&pane_id),
            DaemonRequest::ProjectList => Ok(json!(self.project_summaries()?)),
            DaemonRequest::ProjectShow { name } => self.handle_project_show(&name),
            DaemonRequest::ProjectDossier { name, lines } => {
                self.handle_project_dossier(&name, lines)
            }
            DaemonRequest::AgentStatus { pid, payload } => {
                Ok(self.handle_agent_status(pid, &payload))
            }
            DaemonRequest::AgentSignal {
                pid,
                event,
                notification_type,
                message,
                session_id,
            } => Ok(self.handle_agent_signal(
                pid,
                &event,
                notification_type.as_deref(),
                message.as_deref(),
                session_id.as_deref(),
            )),
            DaemonRequest::ProjectLedger { name, limit } => {
                self.handle_project_ledger(&name, limit)
            }
            DaemonRequest::LeaseStatus { pane_id } => {
                self.ensure_pane_exists(&pane_id)?;
                self.lease_info(&pane_id).map(|info| json!(info))
            }
            DaemonRequest::ResizePaneTerminal {
                pane_id,
                cols,
                rows,
            } => {
                self.ensure_pane_exists(&pane_id)?;
                self.lock_terminals()?
                    .resize_pane(&pane_id, cols.max(2), rows.max(1))?;
                // Resizes arrive per animation frame during a divider drag; persist
                // lazily instead of fsyncing workspace.json for each one.
                self.mark_dirty();
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::SetActivePane { pane_id } => {
                self.lock_registry()?.set_active(&pane_id)?;
                self.mark_dirty();
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::GetScrollback { pane_id } => {
                self.ensure_pane_exists(&pane_id)?;
                let scrollback =
                    read_scrollback(&self.scrollback_dir, &pane_id).unwrap_or_default();
                Ok(json!({ "pane_id": pane_id, "scrollback": scrollback }))
            }
            DaemonRequest::SearchScrollback {
                pane_id,
                needle,
                ignore_case,
                limit,
            } => {
                self.ensure_pane_exists(&pane_id)?;
                let needle = needle.trim();
                if needle.is_empty() {
                    return Err("search needle must not be empty".to_string());
                }
                if needle.len() > SCROLLBACK_SEARCH_NEEDLE_MAX_BYTES {
                    return Err(format!(
                        "search needle is longer than {SCROLLBACK_SEARCH_NEEDLE_MAX_BYTES} bytes"
                    ));
                }
                let limit = match limit {
                    0 => SCROLLBACK_SEARCH_DEFAULT_LIMIT,
                    n => n.min(SCROLLBACK_SEARCH_MAX_LIMIT),
                };
                let lines = scrollback_text_lines(&self.scrollback_dir, &pane_id);
                let matches = search_lines(&lines, needle, ignore_case, limit);
                Ok(json!({
                    "pane_id": pane_id,
                    "total_lines": lines.len(),
                    "truncated": matches.len() >= limit,
                    "matches": matches
                        .into_iter()
                        .map(|(line, text)| json!({ "line": line, "text": text }))
                        .collect::<Vec<_>>(),
                }))
            }
            DaemonRequest::ScrollbackLines { pane_id, from, to } => {
                self.ensure_pane_exists(&pane_id)?;
                if from == 0 || to < from {
                    return Err("line range must be 1-based with from <= to".to_string());
                }
                if to - from + 1 > SCROLLBACK_LINES_MAX_PER_REQUEST {
                    return Err(format!(
                        "at most {SCROLLBACK_LINES_MAX_PER_REQUEST} lines per request"
                    ));
                }
                let lines = scrollback_text_lines(&self.scrollback_dir, &pane_id);
                let total = lines.len();
                let end = to.min(total);
                let slice: Vec<&str> = if from <= total {
                    lines[from - 1..end].iter().map(String::as_str).collect()
                } else {
                    Vec::new()
                };
                Ok(json!({
                    "pane_id": pane_id,
                    "total_lines": total,
                    "from": from,
                    "to": end,
                    "lines": slice,
                }))
            }
            DaemonRequest::UpdateWorkspaceLayout { layout } => {
                let encoded = serde_json::to_vec(&layout)
                    .map_err(|error| format!("invalid layout: {error}"))?;
                if encoded.len() > MAX_LAYOUT_BYTES {
                    return Err("layout exceeds maximum size".to_string());
                }
                *self.lock_layout()? = Some(layout);
                self.persist()?;
                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::GetConfig => Ok(self.effective_config().full_config()),
            DaemonRequest::Broadcast { input } => {
                // A broadcast has no holder, so every held pane is skipped
                // rather than the whole broadcast refused.
                let skip = self.panes_held_by_others(None)?;
                let written = self.lock_terminals()?.write_to_live_except(&input, &skip);
                Ok(json!({ "panes": written }))
            }
            DaemonRequest::SetSyncInput { enabled } => {
                self.sync_input.store(enabled, Ordering::SeqCst);
                Ok(json!({ "sync_input": enabled }))
            }
            DaemonRequest::SetPaneAgent { pane_id, agent } => {
                self.handle_set_pane_agent(&pane_id, agent)
            }
            DaemonRequest::StatusVerbose => {
                let pane_list = self.pane_list()?;
                Ok(json!(VerboseStatus {
                    subscribers: self.router.subscriber_count(),
                    panes: pane_list.panes,
                    active_pane_id: pane_list.active_pane_id,
                    cwd: pane_list.cwd,
                    uptime_secs: self.started_at.elapsed().as_secs(),
                    config: self.effective_config().summary(),
                }))
            }
            DaemonRequest::Wait {
                pane_id,
                condition,
                timeout_ms,
            } => self.handle_wait(&pane_id, &condition, timeout_ms, peer),
            DaemonRequest::Snapshot { pane_id } => self.handle_snapshot(&pane_id),
            DaemonRequest::Find {
                command,
                title,
                cwd,
                state,
            } => self.handle_find(command.as_deref(), title.as_deref(), cwd.as_deref(), state),
            DaemonRequest::Subscribe => {
                Err("subscribe must be handled before dispatch".to_string())
            }
            DaemonRequest::WriteConfig { config } => {
                let config_path = self
                    .persist_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(CONFIG_FILE);

                // M1: a payload that OMITS scrub_env must not erase the workspace
                // file's current scrub list (serde would default the missing field
                // to [] and the atomic write below would persist that). An absent
                // key preserves the existing list; an explicit value — including
                // [] — replaces it. full_config() now carries scrub_env, so honest
                // get→edit→write round-trips are covered either way; this guards
                // clients that build partial payloads (e.g. a settings form
                // without a scrub_env field).
                let mut config = config;
                if config.get("scrub_env").is_none() {
                    if let Some(object) = config.as_object_mut() {
                        let current_scrub = read_config_file(&config_path)
                            .ok()
                            .flatten()
                            .map(|existing| existing.scrub_env)
                            .unwrap_or_default();
                        if !current_scrub.is_empty() {
                            object.insert("scrub_env".to_string(), json!(current_scrub));
                        }
                    }
                }

                // Deserialize the JSON value into a Config (validates types).
                let new_config: Config = serde_json::from_value(config)
                    .map_err(|error| format!("invalid config: {error}"))?;
                // Validate field values (e.g. restore_policy).
                new_config.validate()?;

                // Persist atomically to the per-workspace config.json. The
                // file-watch picks up the change and reloads config into the
                // daemon's mutable shared state + broadcasts ConfigChanged.
                // VAL-CFG-008: atomic write with 0600 perms, no temp left behind.
                // (H1) config.json.tmp is a fixed temp path: serialize the write
                // under the same persist lock as workspace.json so a concurrent
                // WriteConfig (or persist) can't interleave open/write/rename.
                let temp_path = config_path.with_extension("json.tmp");
                let json_data = serde_json::to_vec_pretty(&new_config)
                    .map_err(|error| format!("failed to serialize config: {error}"))?;
                {
                    let _persist_guard = self.lock_persist()?;
                    write_file_atomic(&temp_path, &config_path, &json_data, true)?;
                }

                tracing::info!(
                    workspace_key = %self.workspace_key,
                    event = "config_written",
                    "config.json written via write-config"
                );

                Ok(json!(CommandOk { ok: true }))
            }
            DaemonRequest::CreateAgentPane { title } => {
                self.handle_create_agent_pane(title, AgentPaneSpec::default())
            }
            DaemonRequest::CreateAgentPaneWithSpec {
                title,
                backend,
                model,
            } => self.handle_create_agent_pane(title, AgentPaneSpec::normalized(backend, model)?),
            DaemonRequest::SendAgentMessage {
                pane_id,
                text,
                message_id,
            } => self.handle_send_agent_message(&pane_id, &text, message_id.as_deref()),
            DaemonRequest::AgentApproval {
                pane_id,
                request_id,
                allow,
                message,
            } => self.handle_agent_approval(&pane_id, &request_id, allow, message),
            DaemonRequest::InterruptAgent { pane_id } => self.handle_interrupt_agent(&pane_id),
            DaemonRequest::RunProcess {
                argv,
                cwd,
                timeout_ms,
            } => self.handle_run_process(argv, cwd, timeout_ms),
            DaemonRequest::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(json!(CommandOk { ok: true }))
            }
        }
    }

    /// Non-PTY argv execution for automation (ENHANCEMENTS §3). Captures
    /// stdout/stderr (bounded), honors optional timeout, returns exact exit.
    fn handle_run_process(
        &self,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
    ) -> Result<Value, String> {
        if argv.is_empty() {
            return Err("process requires a non-empty argv".to_string());
        }
        const CAPTURE_CAP: usize = 256 * 1024;
        let started = Instant::now();
        let work_dir = match cwd.as_deref() {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(self.lock_registry()?.cwd.clone()),
        };
        let mut command = std::process::Command::new(&argv[0]);
        if argv.len() > 1 {
            command.args(&argv[1..]);
        }
        command.current_dir(&work_dir);
        // Automation must honor the same environment policy as shell and agent
        // panes. Clone the policy before spawn so no config lock spans execution.
        let config = self.effective_config();
        for key in INHERITED_SESSION_MARKERS {
            command.env_remove(key);
        }
        for key in &config.scrub_env {
            command.env_remove(key);
        }
        command.envs(&config.env);
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        // Own process group on Unix so timeout can kill descendants that still
        // hold the pipes (otherwise read_to_end / reader threads hang forever).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: runs in the child before exec; setpgid(0,0) is the standard
            // "new process group" request and does not touch parent memory.
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to spawn process: {error}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_handle =
            stdout.map(|pipe| thread::spawn(move || read_capped_pipe_tail(pipe, CAPTURE_CAP)));
        let stderr_handle =
            stderr.map(|pipe| thread::spawn(move || read_capped_pipe_tail(pipe, CAPTURE_CAP)));
        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let status = loop {
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    kill_run_process_tree(&mut child);
                    let _ = child.wait();
                    let (stdout, stderr) = finalize_run_process_pipes(
                        &mut child,
                        stdout_handle,
                        stderr_handle,
                        Duration::from_millis(200),
                    );
                    let _ = (stdout, stderr);
                    return Ok(json!({
                        "exit_code": null,
                        "signal": Value::Null,
                        "success": false,
                        "timed_out": true,
                        "elapsed_ms": started.elapsed().as_millis() as u64,
                        "stdout": "",
                        "stderr": "timed out",
                        "argv": argv,
                        "cwd": work_dir.to_string_lossy(),
                    }));
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => {
                    kill_run_process_tree(&mut child);
                    let _ = finalize_run_process_pipes(
                        &mut child,
                        stdout_handle,
                        stderr_handle,
                        Duration::from_millis(200),
                    );
                    return Err(format!("failed to wait for process: {error}"));
                }
            }
        };
        // Parent exited: still kill the process group if pipe readers stall
        // (a descendant may hold stdout/stderr open).
        let (stdout, stderr) = finalize_run_process_pipes(
            &mut child,
            stdout_handle,
            stderr_handle,
            Duration::from_secs(2),
        );
        let (exit_code, signal) = exit_status_fields(&status);
        Ok(json!({
            "exit_code": exit_code,
            "signal": signal,
            "success": status.success(),
            "timed_out": false,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "stdout": stdout,
            "stderr": stderr,
            "argv": argv,
            "cwd": work_dir.to_string_lossy(),
        }))
    }

    /// Block until `condition` is satisfied for `pane_id` or `timeout_ms` elapses, then
    /// return the documented result object `{ matched, reason, revision, exit_code?,
    /// elapsed_ms }` (architecture §6.3).
    ///
    /// The condition is re-evaluated on a fixed poll cadence. Each tick reads the pane's
    /// revision + visible screen under a *tiny* per-pane lock scope and reads liveness
    /// under a brief terminals-lock scope; NO lock is held across the sleep, so a
    /// blocking wait never stalls other daemon operations (Invariant 9 / VAL-PRIM-014).
    /// The full-grid render for text/regex waits is gated on (revision, size): a tick
    /// whose grid provably hasn't changed skips the re-render, the copy, and the match
    /// (a deterministic re-fail), so an idle wait no longer copies ~2M cells per tick.
    ///
    /// Bounded in every edge case: `--timeout` bounds any wait; `--idle` self-bounds (a
    /// quiet/dead pane stops bumping the revision); `--exit` resolves on end (a pane
    /// CLOSED mid-wait resolves distinctly as reason "closed" with a null exit code,
    /// not a spurious "exit"); a disconnected client aborts the wait with a clean error
    /// instead of pinning the handler thread forever (M6, when the serving connection
    /// is passed as `peer`); and a text/regex wait whose pane has *died* resolves
    /// promptly as an unmatched timeout rather than blocking forever, since the frozen
    /// final screen can never gain new output (all output is fed to the model before
    /// the pane is marked ended, so this is race-free — VAL-PRIM-049).
    fn handle_wait(
        &self,
        pane_id: &str,
        condition: &WaitCondition,
        timeout_ms: Option<u64>,
        peer: Option<&TransportStream>,
    ) -> Result<Value, String> {
        // An unknown pane is a clean, immediate error — never a hang (VAL-PRIM-050).
        self.ensure_pane_exists(pane_id)?;

        // Compile the regex ONCE up front so an invalid pattern is a clear error rather
        // than a hang/panic, and is never recompiled per poll (VAL-PRIM-005).
        let regex = match condition {
            WaitCondition::Regex(pattern) => Some(
                Regex::new(pattern).map_err(|error| format!("invalid --regex pattern: {error}"))?,
            ),
            _ => None,
        };
        let needs_text = matches!(condition, WaitCondition::Text(_) | WaitCondition::Regex(_));

        // (T2) L6: an agent pane has no vt100 screen model, so --text/--regex
        // can never match — fail fast instead of silently timing out.
        // --exit/--idle still apply (liveness/revision only).
        if needs_text && self.lock_registry()?.pane_kind(pane_id) == Some(PaneKind::Agent) {
            return Err(format!(
                "agent panes have no screen model; only --exit/--idle apply: {pane_id}"
            ));
        }

        let start = Instant::now();
        let timeout = timeout_ms.map(Duration::from_millis);
        let make_outcome = |matched: bool, reason: &str, revision: u64, exit_code: Option<i32>| {
            let mut outcome = json!({
                "matched": matched,
                "reason": reason,
                "revision": revision,
                "elapsed_ms": start.elapsed().as_millis() as u64,
            });
            // exit_code is part of the documented schema when the wait resolved on pane
            // exit (VAL-PRIM-012); a signal death reports a null code (see
            // reaped_exit_code), as does a pane closed mid-wait (reason "closed").
            if reason == "exit" || reason == "closed" {
                outcome["exit_code"] = match exit_code {
                    Some(code) => json!(code),
                    None => Value::Null,
                };
            }
            outcome
        };

        // Idle tracking: the wait resolves after the revision is unchanged for the idle
        // window. Seeded on the first poll.
        let mut idle_revision: Option<u64> = None;
        let mut idle_since = Instant::now();

        // Screen-text cache for text/regex waits: the grid can only change on a
        // revision bump (new output) or a resize (which deliberately does NOT bump
        // the revision), so the rendered text and the match itself are gated on
        // (revision, rows, cols). A tick whose key is unchanged skips re-rendering
        // the full grid, re-copying it, and re-running a deterministically-failing
        // match — the hot idle path is just a revision read + the 20 ms sleep.
        // (text_cache is assigned before its first read: the key starts unset,
        // so the first text tick always renders into it.)
        let mut text_cache: (String, String);
        let mut text_cache_key: Option<(u64, u16, u16)> = None;

        loop {
            // (M6) A timeout-less wait parks this handler thread inside the
            // condition loop, never reading the socket, so a disconnected client
            // would otherwise pin the connection slot forever: poll the peer
            // each tick and resolve as a clean error on disconnect.
            if let Some(stream) = peer {
                if wait_peer_disconnected(stream) {
                    return Err(format!("wait aborted: client disconnected: {pane_id}"));
                }
            }

            // Read revision (+ screen text for text/regex) under a tiny per-pane lock.
            // `screen_unwrapped` is the rows concatenated WITHOUT separators: a
            // needle that soft-wraps across full-width rows has no row break in
            // the pane's actual byte stream, so it must also match there (L8).
            // `text_fresh` is false when the cached text is provably unchanged
            // since it last failed the match (same revision AND size).
            let (revision, screen_text, screen_unwrapped, text_fresh) =
                match self.router.model_handle(pane_id) {
                    Some(model) => match model.lock() {
                        Ok(model) => {
                            let revision = model.revision;
                            if needs_text {
                                let key = {
                                    let (rows, cols) = model.parser.screen().size();
                                    (revision, rows, cols)
                                };
                                if text_cache_key != Some(key) {
                                    let screen = model.parser.screen();
                                    let grid_rows: Vec<String> = screen.rows(0, key.2).collect();
                                    text_cache = (grid_rows.join("\n"), grid_rows.concat());
                                    text_cache_key = Some(key);
                                    (revision, text_cache.0.clone(), text_cache.1.clone(), true)
                                } else {
                                    (revision, String::new(), String::new(), false)
                                }
                            } else {
                                (revision, String::new(), String::new(), false)
                            }
                        }
                        // A poisoned per-pane model lock never heals; polling it
                        // forever (with no --timeout) was an infinite spin (L20).
                        Err(_) => {
                            return Err(format!(
                                "pane screen model is unavailable (poisoned lock): {pane_id}"
                            ));
                        }
                    },
                    None => (
                        idle_revision.unwrap_or(0),
                        String::new(),
                        String::new(),
                        false,
                    ),
                };
            // In-flight spawns count as live: a wait that lands in the fork/exec
            // window (e.g. `ctl new` immediately followed by `ctl wait --exit`)
            // must not resolve a spurious "exit" before the shell even starts.
            let ended = !self.lock_terminals()?.is_live_or_spawning(pane_id);

            match condition {
                WaitCondition::Text(needle) => {
                    // A stale tick provably re-fails the match on unchanged text.
                    if text_fresh
                        && (screen_text.contains(needle.as_str())
                            || screen_unwrapped.contains(needle.as_str()))
                    {
                        return Ok(make_outcome(true, "text", revision, None));
                    }
                }
                WaitCondition::Regex(_) => {
                    if text_fresh
                        && regex.as_ref().is_some_and(|re| {
                            re.is_match(&screen_text) || re.is_match(&screen_unwrapped)
                        })
                    {
                        return Ok(make_outcome(true, "text", revision, None));
                    }
                }
                WaitCondition::Idle(ms) => {
                    if idle_revision != Some(revision) {
                        idle_revision = Some(revision);
                        idle_since = Instant::now();
                    }
                    if idle_since.elapsed() >= Duration::from_millis(*ms) {
                        return Ok(make_outcome(true, "idle", revision, None));
                    }
                }
                WaitCondition::Exit => {
                    if ended {
                        // A pane CLOSED mid-wait is not an exit: ClosePane removes
                        // it from the registry, while an exited pane stays listed
                        // with its reaped code. Report the distinction instead of
                        // a spurious "exit" (reason "closed", exit_code null).
                        if !self.lock_registry()?.contains_pane(pane_id) {
                            return Ok(make_outcome(true, "closed", revision, None));
                        }
                        let exit_code = self.lock_terminals()?.pane_meta(pane_id).exit_code;
                        return Ok(make_outcome(true, "exit", revision, exit_code));
                    }
                }
            }

            // The timeout bounds the wait and is a distinct result (VAL-PRIM-010/011/013).
            if let Some(limit) = timeout {
                if start.elapsed() >= limit {
                    return Ok(make_outcome(false, "timeout", revision, None));
                }
            }

            // A non-exit text/regex wait whose pane has died can never be satisfied by
            // new output, so resolve it as an unmatched timeout instead of blocking
            // forever (VAL-PRIM-049). `--idle` is excluded: a dead pane is quiet, so it
            // resolves naturally as `idle` above.
            if ended && needs_text {
                return Ok(make_outcome(false, "timeout", revision, None));
            }

            thread::sleep(WAIT_POLL_INTERVAL);
        }
    }

    /// Build the documented `snapshot` struct (architecture §6.3) for `pane_id` by
    /// CONSUMING the existing data layer — the rendered grid/cursor/revision/size/title
    /// from the vt100 model, and command/cwd/exit_code/alive from the terminal store.
    ///
    /// Read-only and point-in-time: the per-pane model lock is held only to copy the
    /// grid out (never the models-map lock while a pane model is locked), and the
    /// terminal-store lock is taken once for liveness + metadata. An unknown pane is a
    /// clean error rather than a zeroed struct (VAL-PRIM-026); an ended-but-known pane
    /// still returns its preserved final screen (VAL-PRIM-051).
    fn handle_snapshot(&self, pane_id: &str) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;

        let model = self
            .router
            .model_handle(pane_id)
            .ok_or_else(|| format!("pane has no screen model: {pane_id}"))?;
        let (cols, rows, lines, title, revision, cursor) = {
            let model = model
                .lock()
                .map_err(|_| format!("pane model lock poisoned: {pane_id}"))?;
            let screen = model.parser.screen();
            let (rows, cols) = screen.size();
            // `rows(0, cols)` yields one plain-text String per visible row (ANSI already
            // interpreted), so `lines.len() == rows` (VAL-PRIM-019/020).
            let lines: Vec<String> = screen.rows(0, cols).collect();
            let (cursor_row, cursor_col) = screen.cursor_position();
            // The OSC window title lives on the model's callbacks, independent of the
            // user-facing pane label (VAL-TERM-008); empty when no title was set.
            let title = model.parser.callbacks().title.clone().unwrap_or_default();
            (
                cols,
                rows,
                lines,
                title,
                model.revision,
                SnapshotCursor {
                    row: cursor_row,
                    col: cursor_col,
                },
            )
        };

        let (alive, meta) = {
            let terminals = self.lock_terminals()?;
            (terminals.is_live(pane_id), terminals.pane_meta(pane_id))
        };
        // Omit exit_code while live; once ended, report the reaped code (or null for a
        // signal death — never a misleading 1). See PaneSnapshot::exit_code.
        let exit_code = if alive { None } else { Some(meta.exit_code) };

        // (T1) Agent state comes from the router's tracker (manual mark or
        // screen-signature detection), not recomputed here.
        let agent_info = self.router.agent_state(pane_id);
        let snapshot = PaneSnapshot {
            pane_id: pane_id.to_string(),
            cols,
            rows,
            lines,
            title,
            revision,
            cursor,
            alive,
            exit_code,
            command: meta.command,
            cwd: meta.cwd,
            agent: agent_info.agent,
            attention: agent_info.attention,
            group: None,
            origin: "User".to_string(),
        };
        Ok(json!(snapshot))
    }

    /// Build the documented `find` result (architecture §6.3): an array of per-pane
    /// metadata for every pane matching ALL supplied filters. CONSUMES the existing
    /// data layer — command/cwd/exit_code from `pane_meta`, liveness from
    /// `runtime_states`, and title/revision/size from the vt100 model — never
    /// recomputing any of it.
    ///
    /// Filters AND together (VAL-PRIM-034): `state` matches the pane's runtime state;
    /// `command`/`title`/`cwd` are substring matches (VAL-PRIM-037). No filters returns
    /// all panes, live and ended (VAL-PRIM-054); no matches returns an empty array, not
    /// an error (VAL-PRIM-036).
    ///
    /// Lock discipline mirrors `handle_snapshot`: pane ids are read under a brief
    /// registry lock; per-pane model state is copied out under the tiny per-pane model
    /// lock (never the models-map lock while a pane model is locked); liveness +
    /// metadata are read under one brief terminals lock. No lock is held across another.
    fn handle_find(
        &self,
        command: Option<&str>,
        title: Option<&str>,
        cwd: Option<&str>,
        state: Option<PaneRuntimeState>,
    ) -> Result<Value, String> {
        let panes = self.lock_registry()?.snapshot().panes;
        let pane_ids: Vec<String> = panes.iter().map(|pane| pane.id.clone()).collect();

        let (states, metas) = {
            let terminals = self.lock_terminals()?;
            let states = terminals.runtime_states(&pane_ids);
            let metas: HashMap<String, PaneMeta> = pane_ids
                .iter()
                .map(|id| (id.clone(), terminals.pane_meta(id)))
                .collect();
            (states, metas)
        };
        // (T1) Agent state read once from the router's tracker.
        let agent_infos = self.router.agent_info_map();
        let agent_usage = self.agent_usage_snapshot();

        let mut entries: Vec<FindEntry> = Vec::new();
        for pane in &panes {
            let pane_state = states
                .get(&pane.id)
                .copied()
                .unwrap_or(PaneRuntimeState::Ended);
            let meta = metas.get(&pane.id).cloned().unwrap_or_default();

            // Title (OSC-captured, like snapshot), revision, and live size come from the
            // vt100 model; a pane without a model (should not happen for a spawned pane)
            // reports neutral defaults rather than failing the whole query.
            let (model_title, revision, cols, rows) = match self.router.model_handle(&pane.id) {
                Some(model) => {
                    let model = model
                        .lock()
                        .map_err(|_| format!("pane model lock poisoned: {}", pane.id))?;
                    let (rows, cols) = model.parser.screen().size();
                    let title = model.parser.callbacks().title.clone().unwrap_or_default();
                    (title, model.revision, cols, rows)
                }
                None => (String::new(), 0, 0, 0),
            };

            if let Some(want) = state {
                if pane_state != want {
                    continue;
                }
            }
            if let Some(needle) = command {
                if !meta.command.as_deref().unwrap_or("").contains(needle) {
                    continue;
                }
            }
            if let Some(needle) = title {
                if !model_title.contains(needle) {
                    continue;
                }
            }
            if let Some(needle) = cwd {
                if !meta.cwd.as_deref().unwrap_or("").contains(needle) {
                    continue;
                }
            }

            // Omit exit_code while live; once ended report the reaped code (or null for a
            // signal death — never a misleading 1). Same semantics as PaneSnapshot.
            let exit_code = match pane_state {
                PaneRuntimeState::Live => None,
                PaneRuntimeState::Ended => Some(meta.exit_code),
            };

            let agent_info = agent_infos.get(&pane.id).cloned().unwrap_or_default();
            entries.push(FindEntry {
                id: pane.id.clone(),
                title: model_title,
                command: meta.command,
                cwd: meta.cwd,
                state: pane_state,
                exit_code,
                agent: agent_info.agent,
                attention: agent_info.attention,
                mode: agent_info.mode,
                unattended: agent_info.unattended,
                output_warnings: {
                    let tricks = self.router.output_tricks(&pane.id);
                    (tricks.total() > 0).then_some(tricks)
                },
                usage: agent_usage.get(&pane.id).cloned(),
                group: None,
                cols,
                rows,
                revision,
            });
        }

        Ok(json!(entries))
    }

    /// Write input to a pane, mirroring it to all live panes when synchronize-input is on.
    /// `holder` attributes the write to a keyboard-lease holder (None = the
    /// legacy unattributed path). The lease gate runs BEFORE the terminal lock
    /// and the lease map is a leaf lock, so lock order stays
    /// registry → terminals with leases only ever taken alone.
    fn write_input(
        &self,
        pane_id: &str,
        data: &str,
        holder: Option<&str>,
        generation: Option<u64>,
    ) -> Result<(), String> {
        self.ensure_pane_exists(pane_id)?;
        if self.sync_input.load(Ordering::SeqCst) {
            // Mirrored input skips panes held by someone else rather than
            // refusing the whole write; the target pane itself is still gated.
            self.check_lease_write(pane_id, holder, generation)?;
            let skip = self.panes_held_by_others(holder)?;
            let written = self.lock_terminals()?.write_to_live_except(data, &skip);
            for written_pane in &written {
                self.note_lease_write(written_pane, data.len());
            }
            Ok(())
        } else {
            self.check_lease_write(pane_id, holder, generation)?;
            self.lock_terminals()?.write_to_pane(pane_id, data)?;
            self.note_lease_write(pane_id, data.len());
            Ok(())
        }
    }

    // ----- Keyboard lease handlers (docs/design/keyboard-lease-and-ledger.md) -----

    fn lease_policy(&self) -> LeasePolicy {
        self.config
            .read()
            .map(|config| config.lease_policy_effective())
            .unwrap_or(LeasePolicy::Open)
    }

    fn lock_leases(&self) -> Result<MutexGuard<'_, HashMap<String, HeldLease>>, String> {
        self.leases
            .lock()
            .map_err(|_| "lease table lock poisoned".to_string())
    }

    /// Append one record to a pane's ledger. Heads are cached per pane after the
    /// first append (seeded from the file's last line), and every append is
    /// fsynced: lease events are rare and the record is the product.
    fn ledger_record(
        &self,
        pane_id: &str,
        kind: &str,
        payload: Value,
    ) -> Result<LedgerRecord, String> {
        self.ledger
            .lock()
            .map_err(|_| "ledger lock poisoned".to_string())?
            .record(pane_id, kind, payload, true)
    }

    fn lease_info(&self, pane_id: &str) -> Result<LeaseInfo, String> {
        let policy = self.lease_policy();
        let leases = self.lock_leases()?;
        Ok(LeaseInfo::from_lease(
            pane_id,
            policy,
            leases.get(pane_id),
            now_millis(),
        ))
    }

    /// Lease info for every HELD pane (the bootstrap snapshot's `leases` map).
    fn lease_infos(&self) -> HashMap<String, LeaseInfo> {
        let policy = self.lease_policy();
        let now = now_millis();
        match self.lock_leases() {
            Ok(leases) => leases
                .iter()
                .map(|(pane_id, held)| {
                    (
                        pane_id.clone(),
                        LeaseInfo::from_lease(pane_id, policy, Some(held), now),
                    )
                })
                .collect(),
            Err(_) => HashMap::new(),
        }
    }

    /// Panes whose keyboard is held by someone other than `holder`.
    fn panes_held_by_others(&self, holder: Option<&str>) -> Result<HashSet<String>, String> {
        let leases = self.lock_leases()?;
        Ok(leases
            .iter()
            .filter(|(_, held)| holder != Some(held.holder.as_str()))
            .map(|(pane_id, _)| pane_id.clone())
            .collect())
    }

    /// Gate one write against the pane's lease. A refusal bumps the holder's
    /// `refused_writes` counter so the eventual release record shows how often
    /// someone else tried to type while the pane was held.
    fn check_lease_write(
        &self,
        pane_id: &str,
        holder: Option<&str>,
        generation: Option<u64>,
    ) -> Result<(), String> {
        let policy = self.lease_policy();
        let mut leases = self.lock_leases()?;
        let verdict = can_write(policy, leases.get(pane_id), holder)
            .and_then(|_| check_generation(leases.get(pane_id), generation));
        match verdict {
            Ok(()) => Ok(()),
            Err(refusal) => {
                if let Some(held) = leases.get_mut(pane_id) {
                    held.refused_writes = held.refused_writes.saturating_add(1);
                }
                Err(format!("{refusal} ({pane_id})"))
            }
        }
    }

    /// Count an accepted write against the pane's lease (no-op when unheld).
    /// Counters reach workspace.json on the lazy-persist cadence, never per
    /// keystroke.
    fn note_lease_write(&self, pane_id: &str, bytes: usize) {
        if let Ok(mut leases) = self.lock_leases() {
            if let Some(held) = leases.get_mut(pane_id) {
                held.writes = held.writes.saturating_add(1);
                held.bytes_typed = held.bytes_typed.saturating_add(bytes as u64);
                held.last_input_ms = Some(now_millis());
                self.dirty.store(true, Ordering::SeqCst);
            }
        }
    }

    fn handle_take_lease(
        &self,
        pane_id: &str,
        holder: &str,
        force: bool,
        why: Option<&str>,
        credential: Option<&str>,
    ) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;
        let holder = validate_holder(holder)?;
        let why = match why {
            Some(reason) => Some(validate_bounded_text(reason, "why", LEASE_WHY_MAX_BYTES)?),
            None => None,
        };
        let now = now_millis();
        let (outcome, previous) = {
            let mut leases = self.lock_leases()?;
            let outcome = can_take(leases.get(pane_id), &holder, force, why.as_deref())?;
            let previous = leases.get(pane_id).cloned();
            if outcome != TakeOutcome::AlreadyHeld {
                let generation = self.next_lease_generation.fetch_add(1, Ordering::SeqCst);
                leases.insert(
                    pane_id.to_string(),
                    HeldLease::new(&holder, now, generation),
                );
            }
            (outcome, previous)
        };
        if outcome == TakeOutcome::AlreadyHeld {
            return self.lease_info(pane_id).map(|info| json!(info));
        }
        // Ledger first: the record is the product. A force-take is two
        // records so the revoked holder's counters are not lost.
        if let (TakeOutcome::Revoking { previous: revoked }, Some(prior)) =
            (&outcome, previous.as_ref())
        {
            self.ledger_record(
                pane_id,
                "lease.revoked",
                json!({
                    "holder": revoked,
                    "by": holder,
                    "credential": credential,
                    "why": why,
                    "held_ms": now.saturating_sub(prior.since_ms),
                    "writes": prior.writes,
                    "bytes_typed": prior.bytes_typed,
                    "refused_writes": prior.refused_writes,
                }),
            )?;
        }
        self.ledger_record(
            pane_id,
            "lease.taken",
            json!({
                "holder": holder,
                "credential": credential,
                "force": force,
                "why": why,
                "previous_holder": previous.as_ref().map(|prior| prior.holder.clone()),
            }),
        )?;
        if let Err(error) = self.persist() {
            // Disk, memory, and clients must not diverge: revert the table and
            // say so in the ledger (best-effort; the persist error is the one
            // reported).
            if let Ok(mut leases) = self.lock_leases() {
                match previous {
                    Some(prior) => leases.insert(pane_id.to_string(), prior),
                    None => leases.remove(pane_id),
                };
            }
            let _ = self.ledger_record(
                pane_id,
                "lease.revoked",
                json!({ "holder": holder, "by": "daemon", "why": format!("persist failed: {error}") }),
            );
            return Err(error);
        }
        if let TakeOutcome::Revoking { previous: revoked } = &outcome {
            tracing::info!(
                workspace_key = %self.workspace_key,
                pane_id = %pane_id,
                event = "lease_revoked",
                holder = %revoked,
                by = %holder,
                "keyboard lease revoked"
            );
            self.router.broadcast(&DaemonEvent::LeaseState {
                pane_id: pane_id.to_string(),
                transition: LeaseTransition::Revoked,
                holder: None,
                since_ms: None,
                note: why.clone(),
            });
        }
        tracing::info!(
            workspace_key = %self.workspace_key,
            pane_id = %pane_id,
            event = "lease_taken",
            holder = %holder,
            "keyboard lease taken"
        );
        self.router.broadcast(&DaemonEvent::LeaseState {
            pane_id: pane_id.to_string(),
            transition: LeaseTransition::Taken,
            holder: Some(holder),
            since_ms: Some(now),
            note: None,
        });
        self.lease_info(pane_id).map(|info| json!(info))
    }

    fn handle_release_lease(
        &self,
        pane_id: &str,
        holder: &str,
        note: &str,
        generation: Option<u64>,
        credential: Option<&str>,
    ) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;
        let holder = validate_holder(holder)?;
        let note = validate_bounded_text(note, "hand-back note", LEASE_NOTE_MAX_BYTES)?;
        let now = now_millis();
        let released = {
            let mut leases = self.lock_leases()?;
            can_release(leases.get(pane_id), &holder)?;
            check_generation(leases.get(pane_id), generation)?;
            leases
                .remove(pane_id)
                .ok_or_else(|| format!("pane keyboard is not held ({pane_id})"))?
        };
        self.ledger_record(
            pane_id,
            "lease.released",
            json!({
                "holder": holder,
                "credential": credential,
                "note": note,
                "held_ms": now.saturating_sub(released.since_ms),
                "writes": released.writes,
                "bytes_typed": released.bytes_typed,
                "refused_writes": released.refused_writes,
            }),
        )?;
        if let Err(error) = self.persist() {
            if let Ok(mut leases) = self.lock_leases() {
                leases.insert(pane_id.to_string(), released);
            }
            let _ = self.ledger_record(
                pane_id,
                "lease.taken",
                json!({ "holder": holder, "force": false, "why": format!("release persist failed: {error}") }),
            );
            return Err(error);
        }
        tracing::info!(
            workspace_key = %self.workspace_key,
            pane_id = %pane_id,
            event = "lease_released",
            holder = %holder,
            "keyboard lease released"
        );
        // (M4) A bound pane's note also reaches the mission's inbox.
        self.mirror_release_to_kranz(pane_id, &holder, &note);
        self.router.broadcast(&DaemonEvent::LeaseState {
            pane_id: pane_id.to_string(),
            transition: LeaseTransition::Released,
            holder: None,
            since_ms: None,
            note: Some(note),
        });
        self.lease_info(pane_id).map(|info| json!(info))
    }

    /// A closed pane's lease dies with it (pane ids are never reused). The
    /// ledger file is kept: it is the audit record, not runtime state.
    fn revoke_lease_on_close(&self, pane_id: &str) {
        let removed = match self.lock_leases() {
            Ok(mut leases) => leases.remove(pane_id),
            Err(_) => None,
        };
        if let Some(held) = removed {
            let now = now_millis();
            let _ = self.ledger_record(
                pane_id,
                "lease.revoked",
                json!({
                    "holder": held.holder,
                    "by": "daemon",
                    "why": "pane closed",
                    "held_ms": now.saturating_sub(held.since_ms),
                    "writes": held.writes,
                    "bytes_typed": held.bytes_typed,
                    "refused_writes": held.refused_writes,
                }),
            );
            self.router.broadcast(&DaemonEvent::LeaseState {
                pane_id: pane_id.to_string(),
                transition: LeaseTransition::Revoked,
                holder: None,
                since_ms: None,
                note: Some("pane closed".to_string()),
            });
        }
    }

    /// (T1) Mark a pane as running an agent CLI (`Some("claude")`) or clear the
    /// mark (`None` → auto-detection resumes). The mark is persisted (manual
    /// marks only), the pane's state is reclassified immediately (broadcasting
    /// an AgentState transition if the effective state changed), and the
    /// pane's current agent state is returned.
    fn handle_set_pane_agent(&self, pane_id: &str, agent: Option<String>) -> Result<Value, String> {
        self.ensure_pane_exists(pane_id)?;
        let agent = match agent {
            Some(name) => {
                // A blank name is a client error, not a silent unmark.
                let name = name.trim().to_string();
                if name.is_empty() {
                    return Err("agent name cannot be blank".to_string());
                }
                // (T1) L8: cap the name — short, shell-safe identifiers only.
                if name.len() > AGENT_NAME_MAX_LEN
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return Err(format!(
                        "invalid agent name '{name}': use at most {AGENT_NAME_MAX_LEN} ASCII letters, digits, '-' or '_'"
                    ));
                }
                Some(name)
            }
            None => None,
        };
        // Set the flag first so persist() writes the new mark, mirror the
        // spawn → persist → announce order used elsewhere: the classification
        // (and its AgentState broadcast) happens after the mark is durable.
        let previous = self.router.agent_mark(pane_id);
        self.router.set_manual_agent(pane_id, agent);
        // (T1) L8: a failed persist must not leave disk/memory/clients
        // diverged — revert the in-memory mark before reporting the error.
        if let Err(error) = self.persist() {
            self.router.restore_agent_mark(pane_id, previous);
            self.router.classify_agent_now(pane_id);
            return Err(error);
        }
        self.router.classify_agent_now(pane_id);
        let info = self.router.agent_state(pane_id);
        Ok(json!({
            "pane_id": pane_id,
            "agent": info.agent,
            "attention": info.attention,
        }))
    }

    /// Read the current effective config (cloned out of the RwLock). Config is
    /// no longer frozen at construction — the file-watch reloads it into the
    /// mutable shared state on change.
    fn effective_config(&self) -> Config {
        self.config
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Reload config from disk (global + per-workspace overlay), update the
    /// mutable shared state, update the TerminalStore's shell config (so
    /// newly-spawned panes use the new shell), and broadcast a `ConfigChanged`
    /// event carrying the new effective config summary. Called by the
    /// file-watch on config.json change. VAL-CFG-011 / VAL-CROSS-007.
    fn reload_config(&self) {
        let data_dir = self
            .persist_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let (new_config, warnings) = load_config(&data_dir);

        // A malformed layer must not be "reloaded" as defaults — that would
        // silently drop scrub_env/shell/idle AND broadcast the defaulted config
        // as a legitimate ConfigChanged (M2). Keep the previous effective config
        // and say why.
        if !warnings.is_empty() {
            for warning in &warnings {
                tracing::warn!(
                    workspace_key = %self.workspace_key,
                    event = "config_reload_skipped",
                    warning = %warning,
                    "config.json is malformed; keeping the previous effective config"
                );
            }
            return;
        }

        // Update the TerminalStore's shell config so newly-spawned panes use
        // the new shell (VAL-CROSS-021). This must happen while holding the
        // terminals lock to avoid a race with a concurrent spawn_pane.
        let new_shell = new_config.shell_config();
        // (T2) Same for the agent spawn config (binary override, permission
        // mode): applies to agent sessions spawned after the reload.
        let new_agent_config = new_config.agent_config();
        if let Ok(mut terminals) = self.lock_terminals() {
            terminals.shell = new_shell;
            terminals.agent_config = new_agent_config;
        }

        // Update the config RwLock.
        if let Ok(mut config) = self.config.write() {
            *config = new_config;
        }

        // Broadcast ConfigChanged with the effective config summary (excludes
        // env values for security — VAL-SEC-010).
        let summary = self.effective_config().summary();
        self.router
            .broadcast(&DaemonEvent::ConfigChanged { config: summary });

        tracing::info!(
            workspace_key = %self.workspace_key,
            event = "config_reloaded",
            "config.json changed on disk; reloaded effective config"
        );
    }

    /// The reaped exit code recorded for a pane (None if live, signal-killed, or
    /// unknown). Used to attach the captured code to a catch-up `PaneEnded`.
    fn pane_exit_code(&self, pane_id: &str) -> Option<i32> {
        self.lock_terminals()
            .ok()
            .and_then(|terminals| terminals.pane_meta(pane_id).exit_code)
    }

    fn snapshot(&self) -> Result<WorkspaceSnapshot, String> {
        let mut snapshot = self.lock_registry()?.snapshot();
        let pane_ids = snapshot
            .panes
            .iter()
            .map(|pane| pane.id.clone())
            .collect::<Vec<_>>();
        let terminals = self.lock_terminals()?;
        snapshot.pane_states = terminals.runtime_states(&pane_ids);
        snapshot.sizes = terminals
            .sizes
            .iter()
            .filter(|(pane_id, _)| pane_ids.contains(pane_id))
            .map(|(pane_id, size)| {
                (
                    pane_id.clone(),
                    PaneSize {
                        cols: size.cols,
                        rows: size.rows,
                    },
                )
            })
            .collect();
        snapshot.agent_specs = terminals
            .agent_specs
            .iter()
            .filter(|(pane_id, _)| pane_ids.contains(pane_id))
            .map(|(pane_id, spec)| (pane_id.clone(), spec.clone()))
            .collect();
        drop(terminals);
        // (T1) Agent info rides the bootstrap payload parallel to pane_states.
        snapshot.agent_states = self.router.agent_states();
        // An agent-kind pane's mode is its configured permission mode, not a
        // screen: overlay it so every pane carries `unattended` the same way.
        let permission_mode = self.effective_config().agent_config().permission_mode;
        for pane in &snapshot.panes {
            if pane.kind != PaneKind::Agent {
                continue;
            }
            let backend = snapshot
                .agent_specs
                .get(&pane.id)
                .map(|spec| spec.backend.as_str().to_string())
                .unwrap_or_else(|| "claude".to_string());
            let entry = snapshot
                .agent_states
                .entry(pane.id.clone())
                .or_insert_with(|| AgentPaneInfo {
                    agent: Some(backend),
                    ..AgentPaneInfo::default()
                });
            entry.mode = Some(permission_mode.clone());
            entry.unattended = is_unattended_mode(Some(&permission_mode));
        }
        // Held keyboard leases ride alongside so a client can render the
        // holder without a second request.
        snapshot.leases = self.lease_infos();
        snapshot.projects = self.projects_snapshot();
        snapshot.output_warnings = self.router.output_warnings();
        snapshot.agent_usage = self.agent_usage_snapshot();
        // (T2) Bounded conversation replay for agent panes, read back from the
        // per-pane JSONL log (covers live, ended, and not-yet-respawned panes).
        for pane in &snapshot.panes {
            if pane.kind != PaneKind::Agent {
                continue;
            }
            let events = read_agent_log_tail(
                &self.agents_dir,
                &pane.id,
                AGENT_REPLAY_MAX_BYTES,
                AGENT_REPLAY_MAX_EVENTS,
            );
            if !events.is_empty() {
                snapshot.agent_events.insert(pane.id.clone(), events);
            }
        }
        snapshot.layout = self.lock_layout()?.clone();
        Ok(snapshot)
    }

    fn pane_list(&self) -> Result<PaneList, String> {
        let snapshot = self.snapshot()?;
        let pane_states = snapshot.pane_states;
        let panes = snapshot
            .panes
            .into_iter()
            .map(|pane| {
                let state = pane_states
                    .get(&pane.id)
                    .copied()
                    .unwrap_or(PaneRuntimeState::Ended);
                PaneStatus { pane, state }
            })
            .collect();

        Ok(PaneList {
            panes,
            active_pane_id: snapshot.active_pane_id,
            cwd: snapshot.cwd,
        })
    }

    fn pane_status(&self, pane_id: &str) -> Result<PaneStatus, String> {
        self.pane_list()?
            .panes
            .into_iter()
            .find(|status| status.pane.id == pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))
    }

    /// Authenticate a hello and negotiate the connection's wire version. The
    /// constant-time token compare still gates the connection, but the legacy
    /// `version` field is NO LONGER hard-rejected (Invariant 8 / VAL-IPC-021):
    /// instead the wire version is negotiated from the additive `max_wire_version`.
    /// Returns the negotiated wire version on success.
    /// The hello gate. A presented per-client credential must be valid and
    /// names the connection (a revoked or unknown one is refused even beside
    /// a valid workspace token: the client chose to be held to it); with no
    /// credential the workspace token is the root credential; else refused.
    /// The generic error never says which check failed (VAL-SEC-009).
    fn authenticate(&self, hello: &IpcHello) -> Result<(u16, ClientIdentity), String> {
        if hello.frame_type != "hello" {
            return Err("first daemon frame must be hello".to_string());
        }
        let policy = self.identity_policy();
        if let Some(presented) = hello.client_token.as_deref().filter(|t| !t.is_empty()) {
            return match self.identity_for_client_token(presented) {
                Some(identity) => Ok((negotiate_wire_version(hello.max_wire_version), identity)),
                None => Err("daemon authentication failed".to_string()),
            };
        }
        if !constant_time_eq(&hello.token, &self.token) {
            return Err("daemon authentication failed".to_string());
        }
        Ok((
            negotiate_wire_version(hello.max_wire_version),
            ClientIdentity::root(policy),
        ))
    }

    fn identity_policy(&self) -> IdentityPolicy {
        self.effective_config().identity_effective()
    }

    fn credential_is_active(&self, id: &str) -> bool {
        self.clients.lock().ok().is_some_and(|clients| {
            clients
                .clients
                .iter()
                .any(|record| record.id == id && record.revoked_at_ms.is_none())
        })
    }

    /// Match a presented client token against the issued records (hash
    /// compare, constant time per record) and note the sighting.
    fn identity_for_client_token(&self, presented: &str) -> Option<ClientIdentity> {
        let hash = client_token_hash(presented);
        let mut clients = self.clients.lock().ok()?;
        let now = now_millis();
        let mut found = None;
        let mut persist = false;
        for record in clients.clients.iter_mut() {
            if record.revoked_at_ms.is_none() && constant_time_eq(&record.token_hash, &hash) {
                // A status line reports every turn; rewrite the file at most
                // once a minute per credential.
                if now.saturating_sub(record.last_seen_ms) >= 60_000 {
                    record.last_seen_ms = now;
                    persist = true;
                }
                found = Some(ClientIdentity::from_record(record));
                break;
            }
        }
        if persist {
            let snapshot = clients.clone();
            drop(clients);
            // Best-effort: a failed last-seen write is not an auth failure.
            let _ = save_clients_file(&self.clients_path, &snapshot);
        }
        found
    }

    fn handle_identity_issue(&self, holder: &str, scopes: &[String]) -> Result<Value, String> {
        let holder = validate_holder(holder)?;
        let mut parsed: Vec<ClientScope> = Vec::new();
        for raw in scopes {
            for part in raw.split(',') {
                if part.trim().is_empty() {
                    continue;
                }
                let scope = ClientScope::parse(part).ok_or_else(|| {
                    format!("unknown scope '{}': read, write or admin", part.trim())
                })?;
                if !parsed.contains(&scope) {
                    parsed.push(scope);
                }
            }
        }
        if parsed.is_empty() {
            parsed.push(ClientScope::Read);
        }
        let mut raw = [0u8; 32];
        fill_secure_random(&mut raw)?;
        let token = format!("{CLIENT_TOKEN_PREFIX}{}", hex_encode(&raw));
        let mut id_bytes = [0u8; 6];
        fill_secure_random(&mut id_bytes)?;
        let id = hex_encode(&id_bytes);
        let record = ClientRecord {
            id: id.clone(),
            holder: holder.clone(),
            scopes: parsed.clone(),
            token_hash: client_token_hash(&token),
            created_at_ms: now_millis(),
            last_seen_ms: 0,
            revoked_at_ms: None,
        };
        let snapshot = {
            let mut clients = self
                .clients
                .lock()
                .map_err(|_| "client table lock poisoned".to_string())?;
            let active = clients
                .clients
                .iter()
                .filter(|c| c.revoked_at_ms.is_none())
                .count();
            if active >= MAX_CLIENT_RECORDS {
                return Err(format!(
                    "credential limit reached ({MAX_CLIENT_RECORDS}); revoke some"
                ));
            }
            clients.clients.push(record.clone());
            clients.clone()
        };
        if let Err(error) = save_clients_file(&self.clients_path, &snapshot) {
            if let Ok(mut clients) = self.clients.lock() {
                clients.clients.retain(|c| c.id != id);
            }
            return Err(error);
        }
        tracing::info!(
            workspace_key = %self.workspace_key,
            event = "identity_issued",
            credential = %id,
            holder = %holder,
            "client credential issued"
        );
        let mut public = record.public();
        public["token"] = json!(token);
        Ok(public)
    }

    fn handle_identity_list(&self) -> Result<Value, String> {
        let clients = self
            .clients
            .lock()
            .map_err(|_| "client table lock poisoned".to_string())?;
        Ok(json!(clients
            .clients
            .iter()
            .map(ClientRecord::public)
            .collect::<Vec<_>>()))
    }

    fn handle_identity_revoke(&self, id: &str) -> Result<Value, String> {
        let (record, snapshot) = {
            let mut clients = self
                .clients
                .lock()
                .map_err(|_| "client table lock poisoned".to_string())?;
            let record = clients
                .clients
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| format!("unknown credential '{id}'"))?;
            if record.revoked_at_ms.is_none() {
                record.revoked_at_ms = Some(now_millis());
            }
            (record.clone(), clients.clone())
        };
        save_clients_file(&self.clients_path, &snapshot)?;
        tracing::info!(
            workspace_key = %self.workspace_key,
            event = "identity_revoked",
            credential = %id,
            "client credential revoked"
        );
        Ok(record.public())
    }

    /// (M6) Dispatch as a known identity: scope check, holder binding, and
    /// the credential id on lease records; everything else as before.
    fn handle_as(
        &self,
        request: DaemonRequest,
        peer: Option<&TransportStream>,
        identity: &ClientIdentity,
    ) -> Result<Value, String> {
        // Revocation takes effect on the next request, not the next hello: a
        // long-lived framed connection from a revoked credential stops here.
        if let Some(id) = identity.credential.as_deref() {
            if !self.credential_is_active(id) {
                return Err("client credential revoked".to_string());
            }
        }
        let needed = request_scope(&request);
        if !identity.has(needed) {
            return Err(format!(
                "read-only credential: '{}' scope required for {}",
                needed.name(),
                request_name(&request)
            ));
        }
        // Hook and status-line reports are reads for the root token (the
        // session's own hooks run with it) but a read-only credential must
        // not set badges or write `hook.received` records on panes it can
        // only watch.
        if matches!(
            request,
            DaemonRequest::AgentSignal { .. } | DaemonRequest::AgentStatus { .. }
        ) && identity.credential.is_some()
            && !identity.has(ClientScope::Write)
        {
            return Err(format!(
                "read-only credential: 'write' scope required for {}",
                request_name(&request)
            ));
        }
        let request = bind_holder(request, identity)?;
        match request {
            DaemonRequest::TakeLease {
                pane_id,
                holder,
                force,
                why,
            } => self.handle_take_lease(
                &pane_id,
                &holder,
                force,
                why.as_deref(),
                identity.credential.as_deref(),
            ),
            DaemonRequest::ReleaseLease {
                pane_id,
                holder,
                note,
                generation,
            } => self.handle_release_lease(
                &pane_id,
                &holder,
                &note,
                generation,
                identity.credential.as_deref(),
            ),
            DaemonRequest::Whoami => Ok(identity.describe(self.identity_policy())),
            other => self.handle_with_peer(other, peer),
        }
    }

    fn ensure_pane_exists(&self, pane_id: &str) -> Result<(), String> {
        if self.lock_registry()?.contains_pane(pane_id) {
            Ok(())
        } else {
            Err(format!("pane not found: {pane_id}"))
        }
    }

    /// Roll back a just-created pane whose spawn/persist failed BEFORE the pane
    /// was announced: kill any spawned session, drop the registry entry
    /// (restoring the previously-active pane), and remove any scrollback the
    /// fresh shell already produced. No PaneCreated was ever broadcast, so
    /// subscribers have nothing to un-learn — and a client retry can't
    /// duplicate a pane that was reported as failed.
    fn rollback_created_pane(&self, pane_id: &str, previous_active: Option<String>) {
        self.router.mark_closed(pane_id);
        self.router.remove_model(pane_id);
        self.router.remove_agent(pane_id);
        if let Ok(mut terminals) = self.lock_terminals() {
            terminals.close_pane(pane_id);
        }
        self.router.invalidate_append_handle(pane_id);
        let _ = fs::remove_file(scrollback_path(&self.scrollback_dir, pane_id));
        // (T2) M3: same for the agent conversation log (ClosePane parity).
        let _ = fs::remove_file(agent_log_path(&self.agents_dir, pane_id));
        if let Ok(mut registry) = self.lock_registry() {
            let was_active = registry.active_pane_id.as_deref() == Some(pane_id);
            registry.remove_pane(pane_id);
            if was_active {
                registry.active_pane_id = previous_active.filter(|id| registry.contains_pane(id));
            }
        }
    }

    /// Spawn a pane's session if it isn't live, running the expensive PTY setup
    /// (openpty + fork/exec) WITHOUT the TerminalStore lock (M7): only the cheap
    /// plan (config/size read) and commit (map insertion + reader-thread start)
    /// run under the lock, so a slow spawn no longer stalls input/resize/
    /// liveness for every other pane. Double-spawn avoidance: the pane is
    /// marked in-flight under the lock, and a concurrent ensure waits on
    /// spawn_cvar for the commit, then sees the pane live.
    fn ensure_terminal(&self, pane_id: &str) -> Result<(), String> {
        let plan = {
            let mut terminals = self.lock_terminals()?;
            loop {
                if terminals.is_live(pane_id) {
                    return Ok(());
                }
                if terminals.spawning.insert(pane_id.to_string()) {
                    break terminals.plan_spawn(pane_id);
                }
                // Another thread is spawning this pane; wait for its commit.
                terminals = self
                    .spawn_cvar
                    .wait(terminals)
                    .map_err(|_| "terminal store lock poisoned".to_string())?;
            }
        };
        // Removes the in-flight marker + notifies waiters on ALL exits (incl. panic).
        let _in_flight = SpawnInFlight {
            server: self,
            pane_id: pane_id.to_string(),
        };
        let prepared = execute_spawn(&plan)?;
        let mut terminals = self.lock_terminals()?;
        // The pane may have been closed, or the daemon asked to shut down, while
        // the spawn ran unlocked: never commit a session for it — kill + reap
        // the child instead of leaking a shell with no registry entry.
        if self.router.is_closed(pane_id) || self.should_shutdown() {
            drop(terminals);
            kill_and_reap_child(prepared.child);
            return Ok(());
        }
        terminals.commit_spawn(pane_id, plan.size, prepared);
        Ok(())
    }

    /// Kill a pane's session and respawn it, with the expensive half off the
    /// store lock like `ensure_terminal` (M7). The pane's size is preserved
    /// across the close (review-low); concurrent spawns of the same pane are
    /// serialized via the in-flight marker + spawn_cvar.
    fn restart_terminal(&self, pane_id: &str) -> Result<(), String> {
        let plan = {
            let mut terminals = self.lock_terminals()?;
            loop {
                if terminals.spawning.insert(pane_id.to_string()) {
                    break;
                }
                // Another spawn/restart of this pane is in flight; wait for it.
                terminals = self
                    .spawn_cvar
                    .wait(terminals)
                    .map_err(|_| "terminal store lock poisoned".to_string())?;
            }
            // close_pane drops the sizes entry, which would silently reset the
            // pane to the default 120x40 on respawn (review-low): preserve it.
            // Same for frozen profile shell overrides (ENHANCEMENTS §4).
            let size = terminals.sizes.get(pane_id).copied();
            let shell = terminals.pane_shells.get(pane_id).cloned();
            terminals.close_pane(pane_id);
            if let Some(size) = size {
                terminals.sizes.insert(pane_id.to_string(), size);
            }
            if let Some(shell) = shell {
                terminals.pane_shells.insert(pane_id.to_string(), shell);
            }
            terminals.plan_spawn(pane_id)
        };
        let _in_flight = SpawnInFlight {
            server: self,
            pane_id: pane_id.to_string(),
        };
        let prepared = execute_spawn(&plan)?;
        let mut terminals = self.lock_terminals()?;
        if self.router.is_closed(pane_id) || self.should_shutdown() {
            drop(terminals);
            kill_and_reap_child(prepared.child);
            return Ok(());
        }
        terminals.commit_spawn(pane_id, plan.size, prepared);
        Ok(())
    }

    /// Ensure every listed pane has a live session (bootstrap auto-spawn, M7:
    /// each spawn now runs its fork/exec off the store lock).
    fn ensure_terminals(&self, pane_ids: &[String]) -> Result<(), String> {
        for pane_id in pane_ids {
            self.ensure_session(pane_id)?;
        }
        Ok(())
    }

    /// (T2) Kind-aware session dispatch: agent panes spawn a headless CLI
    /// (piped stdio), shell panes their PTY. Unknown-kind/unknown panes fall
    /// through to the PTY path, which surfaces its own errors.
    fn ensure_session(&self, pane_id: &str) -> Result<(), String> {
        match self.lock_registry()?.pane_kind(pane_id) {
            Some(PaneKind::Agent) => self.ensure_agent_session(pane_id),
            _ => self.ensure_terminal(pane_id),
        }
    }

    /// (T2) Kind-aware restart dispatch (see ensure_session).
    fn restart_session(&self, pane_id: &str) -> Result<(), String> {
        match self.lock_registry()?.pane_kind(pane_id) {
            Some(PaneKind::Agent) => self.restart_agent_session(pane_id),
            _ => self.restart_terminal(pane_id),
        }
    }

    /// (T2) Ensure the pane exists AND is agent-kind (the shared precondition
    /// of every agent request).
    #[cfg(any(unix, windows))]
    fn ensure_agent_pane(&self, pane_id: &str) -> Result<(), String> {
        match self.lock_registry()?.pane_kind(pane_id) {
            Some(PaneKind::Agent) => Ok(()),
            Some(_) => Err(format!("pane is not an agent pane: {pane_id}")),
            None => Err(format!("pane not found: {pane_id}")),
        }
    }

    /// (T2) Create an agent pane: registry create → spawn → persist →
    /// announce, with the same spawn-failure rollback as CreatePane (and
    /// sharing its MAX_PANES cap — an agent pane costs a process, two threads,
    /// and a log file).
    fn handle_create_agent_pane(
        &self,
        title: Option<String>,
        spec: AgentPaneSpec,
    ) -> Result<Value, String> {
        let (pane, previous_active) = {
            let mut registry = self.lock_registry()?;
            if registry.panes.len() >= MAX_PANES {
                return Err(format!(
                    "pane limit reached ({MAX_PANES}); close a pane before creating another"
                ));
            }
            let previous_active = registry.active_pane_id.clone();
            (
                registry.create_pane_with_kind(title, PaneKind::Agent),
                previous_active,
            )
        };
        self.lock_terminals()?
            .agent_specs
            .insert(pane.id.clone(), spec.clone());
        tracing::info!(
            workspace_key = %self.workspace_key,
            pane_id = %pane.id,
            event = "pane_create",
            kind = "agent",
            backend = spec.backend.as_str(),
            model = spec.model.as_deref().unwrap_or("default"),
            "agent pane created"
        );
        let spawned = self.ensure_agent_session(&pane.id);
        if let Err(error) = spawned {
            tracing::warn!(
                workspace_key = %self.workspace_key,
                pane_id = %pane.id,
                event = "pane_end",
                error = %error,
                "agent CLI spawn failed"
            );
            self.rollback_created_pane(&pane.id, previous_active);
            return Err(error);
        }
        if let Err(error) = self.persist() {
            self.rollback_created_pane(&pane.id, previous_active);
            return Err(format!("failed to persist new pane: {error}"));
        }
        // Same create/close race as CreatePane: skip the announcement if the
        // pane was closed during the spawn window.
        if self.lock_registry()?.contains_pane(&pane.id) {
            self.router
                .broadcast(&DaemonEvent::PaneCreated { pane: pane.clone() });
        }
        Ok(json!(pane))
    }

    /// (T2) Spawn an agent session if the pane has no live one, with the
    /// fork/exec off the store lock (same M7 dance as ensure_terminal —
    /// shared `spawning` marker and spawn_cvar, since pane ids are one space).
    #[cfg(any(unix, windows))]
    fn ensure_agent_session(&self, pane_id: &str) -> Result<(), String> {
        let plan = {
            let mut terminals = self.lock_terminals()?;
            loop {
                if terminals.is_live(pane_id) {
                    return Ok(());
                }
                if terminals.spawning.insert(pane_id.to_string()) {
                    break terminals.plan_agent_spawn(pane_id);
                }
                terminals = self
                    .spawn_cvar
                    .wait(terminals)
                    .map_err(|_| "terminal store lock poisoned".to_string())?;
            }
        };
        let _in_flight = SpawnInFlight {
            server: self,
            pane_id: pane_id.to_string(),
        };
        let prepared = execute_agent_spawn(&plan)?;
        let mut terminals = self.lock_terminals()?;
        // Closed/shutdown during the spawn window: kill + reap, commit nothing
        // (std::process::Child reaps directly — no portable-pty killer here).
        if self.router.is_closed(pane_id) || self.should_shutdown() {
            drop(terminals);
            let mut child = prepared.child;
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        terminals.commit_agent_spawn(pane_id, prepared);
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn ensure_agent_session(&self, _pane_id: &str) -> Result<(), String> {
        Err(AGENT_UNSUPPORTED.to_string())
    }

    /// (T2) Kill the pane's agent session and respawn it. The CLI
    /// session id is stashed BEFORE the close drops the old session, so the
    /// new process resumes the conversation.
    #[cfg(any(unix, windows))]
    fn restart_agent_session(&self, pane_id: &str) -> Result<(), String> {
        let plan = {
            let mut terminals = self.lock_terminals()?;
            loop {
                if terminals.spawning.insert(pane_id.to_string()) {
                    break;
                }
                terminals = self
                    .spawn_cvar
                    .wait(terminals)
                    .map_err(|_| "terminal store lock poisoned".to_string())?;
            }
            // close_pane drops BOTH the session and the agent_resume seed, so
            // capture the id first and re-stash it after (the same dance as
            // PTY restart preserving the pane's size).
            let resume = terminals.agent_session_id(pane_id);
            let spec = terminals.agent_specs.get(pane_id).cloned();
            terminals.close_pane(pane_id);
            if let Some(spec) = spec {
                terminals.agent_specs.insert(pane_id.to_string(), spec);
            }
            if let Some(session_id) = resume {
                terminals
                    .agent_resume
                    .insert(pane_id.to_string(), session_id);
            }
            terminals.plan_agent_spawn(pane_id)
        };
        let _in_flight = SpawnInFlight {
            server: self,
            pane_id: pane_id.to_string(),
        };
        let prepared = execute_agent_spawn(&plan)?;
        let mut terminals = self.lock_terminals()?;
        if self.router.is_closed(pane_id) || self.should_shutdown() {
            drop(terminals);
            let mut child = prepared.child;
            let _ = child.kill();
            let _ = child.wait();
            return Ok(());
        }
        terminals.commit_agent_spawn(pane_id, prepared);
        Ok(())
    }

    #[cfg(not(any(unix, windows)))]
    fn restart_agent_session(&self, _pane_id: &str) -> Result<(), String> {
        Err(AGENT_UNSUPPORTED.to_string())
    }

    /// (T2) Post one user message to an agent pane. Spawns the session first
    /// when the pane has no live one (auto-ensure: an ended pane — or a
    /// restored pane under restore_on_demand — simply resumes its CLI session
    /// on first message, instead of erroring like a dead PTY).
    #[cfg(any(unix, windows))]
    fn handle_send_agent_message(
        &self,
        pane_id: &str,
        text: &str,
        message_id: Option<&str>,
    ) -> Result<Value, String> {
        self.ensure_agent_pane(pane_id)?;
        if text.trim().is_empty() {
            return Err("agent message cannot be empty".to_string());
        }
        if text.len() > AGENT_MESSAGE_MAX_BYTES {
            return Err(format!(
                "agent message exceeds maximum size ({AGENT_MESSAGE_MAX_BYTES} bytes)"
            ));
        }
        if message_id.is_some_and(|id| id.is_empty() || id.len() > 128) {
            return Err("agent message id must contain 1 to 128 bytes".to_string());
        }
        self.ensure_agent_session(pane_id)?;
        let (backend, input, shared, events) = {
            let terminals = self.lock_terminals()?;
            if !terminals.is_live(pane_id) {
                return Err(format!("agent session not found: {pane_id}"));
            }
            terminals
                .agent_session_handles(pane_id)
                .ok_or_else(|| format!("agent session not found: {pane_id}"))?
        };
        // One in-flight turn per pane (rejected, not queued — see
        // agent_try_begin_turn). The turn flag clears when the reader sees the
        // turn's `result` event or the process exits.
        if !agent_try_begin_turn(&shared) {
            return Err(format!(
                "agent turn already in progress: {pane_id} (wait for turn_complete or interrupt)"
            ));
        }
        let seq = AGENT_INTERRUPT_SEQ.fetch_add(1, Ordering::SeqCst);
        let payload = match backend {
            AgentBackendKind::Claude => json!({
                "type": "user",
                "message": {"role": "user", "content": [{"type": "text", "text": text}]},
            }),
            AgentBackendKind::Droid => json!({
                "jsonrpc": "2.0",
                "factoryApiVersion": "1.0.0",
                "type": "request",
                "id": format!("sgian-message-{seq}"),
                "method": "droid.add_user_message",
                "params": {"text": text},
            }),
        };
        let line = serde_json::to_string(&payload)
            .map_err(|error| format!("failed to encode agent message: {error}"))?;
        // Hold the event lock across enqueue + append so even a fast CLI
        // cannot publish its reply ahead of the accepted prompt. Failed
        // enqueues are never written to conversation history.
        let mut events = events.lock().map_err(|_| {
            agent_end_turn(&shared);
            "agent event log lock poisoned".to_string()
        })?;
        if let Err(error) = queue_agent_stdin(&input, pane_id, &line) {
            agent_end_turn(&shared);
            return Err(error);
        }
        events.emit(
            &self.router,
            pane_id,
            json!({
                "kind": "user_message", "text": text, "message_id": message_id,
            }),
        );
        Ok(json!(CommandOk { ok: true }))
    }

    #[cfg(not(any(unix, windows)))]
    fn handle_send_agent_message(
        &self,
        _pane_id: &str,
        _text: &str,
        _message_id: Option<&str>,
    ) -> Result<Value, String> {
        Err(AGENT_UNSUPPORTED.to_string())
    }

    /// (T2) Deliver the operator's decision for a pending permission request.
    /// Removing the pending entry first makes a double-answer a clean "no
    /// pending" error instead of a silently-dropped second reply.
    #[cfg(any(unix, windows))]
    fn handle_agent_approval(
        &self,
        pane_id: &str,
        request_id: &str,
        allow: bool,
        message: Option<String>,
    ) -> Result<Value, String> {
        self.ensure_agent_pane(pane_id)?;
        if let Some(ref message) = message {
            if message.len() > AGENT_MESSAGE_MAX_BYTES {
                return Err(format!(
                    "approval message exceeds maximum size ({AGENT_MESSAGE_MAX_BYTES} bytes)"
                ));
            }
        }
        let sender = {
            let terminals = self.lock_terminals()?;
            let (_, _, shared, _) = terminals
                .agent_session_handles(pane_id)
                .ok_or_else(|| format!("agent session not found: {pane_id}"))?;
            let mut shared = shared
                .lock()
                .map_err(|_| "agent session lock poisoned".to_string())?;
            shared.pending.remove(request_id)
        };
        let Some(sender) = sender else {
            return Err(format!(
                "no pending permission request '{request_id}' for pane {pane_id}"
            ));
        };
        // A send failure means the reader already timed out/exited; the
        // approval is moot either way, so it is not an error.
        let _ = sender.send(AgentApprovalDecision {
            allow,
            message,
            reason: "user",
        });
        Ok(json!(CommandOk { ok: true }))
    }

    #[cfg(not(any(unix, windows)))]
    fn handle_agent_approval(
        &self,
        _pane_id: &str,
        _request_id: &str,
        _allow: bool,
        _message: Option<String>,
    ) -> Result<Value, String> {
        Err(AGENT_UNSUPPORTED.to_string())
    }

    /// (T2) Interrupt the agent's current turn: write the CLI's `interrupt`
    /// control request (probe f). When a turn is live the CLI ends it with an
    /// error-subtype turn_complete; when the turn flag is STALE (the turn's
    /// `result` line was lost — oversized/malformed) the CLI answers a
    /// no-live-turn interrupt with a bare control_response, which normalizes
    /// to nothing. The send path is therefore the recovery point (M2): the
    /// flag clears here, and a later turn_complete just clears it again.
    #[cfg(any(unix, windows))]
    fn handle_interrupt_agent(&self, pane_id: &str) -> Result<Value, String> {
        self.ensure_agent_pane(pane_id)?;
        let (backend, input, shared, _) = {
            let terminals = self.lock_terminals()?;
            if !terminals.is_live(pane_id) {
                return Err(format!("agent session ended: {pane_id}"));
            }
            terminals
                .agent_session_handles(pane_id)
                .ok_or_else(|| format!("agent session not found: {pane_id}"))?
        };
        let seq = AGENT_INTERRUPT_SEQ.fetch_add(1, Ordering::SeqCst);
        let payload = match backend {
            AgentBackendKind::Claude => json!({
                "type": "control_request",
                "request_id": format!("sgian-interrupt-{seq}"),
                "request": {"subtype": "interrupt"},
            }),
            AgentBackendKind::Droid => json!({
                "jsonrpc": "2.0",
                "factoryApiVersion": "1.0.0",
                "type": "request",
                "id": format!("sgian-interrupt-{seq}"),
                "method": "droid.interrupt_session",
                "params": {},
            }),
        };
        let line = serde_json::to_string(&payload)
            .map_err(|error| format!("failed to encode interrupt request: {error}"))?;
        queue_agent_stdin(&input, pane_id, &line)?;
        agent_end_turn(&shared);
        Ok(json!(CommandOk { ok: true }))
    }

    #[cfg(not(any(unix, windows)))]
    fn handle_interrupt_agent(&self, _pane_id: &str) -> Result<Value, String> {
        Err(AGENT_UNSUPPORTED.to_string())
    }

    fn persist(&self) -> Result<(), String> {
        // (H1) Serialize persists: the temp path is fixed (workspace.json.tmp),
        // so two concurrent writers could interleave open/write/rename and tear
        // the file. Outermost lock — taken before registry/terminals/layout.
        let _persist_guard = self.lock_persist()?;
        let registry = self.lock_registry()?;
        let terminals = self.lock_terminals()?;
        let sizes = terminals
            .sizes
            .iter()
            .map(|(pane_id, size)| {
                (
                    pane_id.clone(),
                    PersistedPtySize {
                        cols: size.cols,
                        rows: size.rows,
                    },
                )
            })
            .collect();
        let pane_ids = registry
            .panes
            .iter()
            .map(|pane| pane.id.clone())
            .collect::<Vec<_>>();
        let pane_states = terminals.runtime_states(&pane_ids);
        // (T1) Persist only MANUAL agent marks; detected state re-derives from
        // the screen after a restart. Filtered against the registry (L7 — same
        // rationale as the agents_v2 filter): a stale mark for a dropped pane
        // must not round-trip through workspace.json.
        let agents = self
            .router
            .manual_agent_marks()
            .into_iter()
            .filter(|(pane_id, _)| registry.contains_pane(pane_id))
            .collect();
        // (T2) Persist each agent pane's CLI session id for --resume. Live
        // sessions win over the startup seed (the CLI confirmed the id in its
        // init event). unix + Windows only: elsewhere no session can exist.
        #[cfg(any(unix, windows))]
        let agents_v2: HashMap<String, String> = registry
            .panes
            .iter()
            .filter(|pane| pane.kind == PaneKind::Agent)
            .filter_map(|pane| {
                terminals
                    .agent_session_id(&pane.id)
                    .map(|session_id| (pane.id.clone(), session_id))
            })
            .collect();
        #[cfg(not(any(unix, windows)))]
        let agents_v2: HashMap<String, String> = HashMap::new();
        let persisted = PersistedWorkspace {
            panes: registry.panes.clone(),
            active_pane_id: registry.active_pane_id.clone(),
            cwd: registry.cwd.clone(),
            next_id: registry.next_id,
            layout: self.lock_layout()?.clone(),
            sizes,
            pane_states,
            agents,
            agents_v2,
            agent_specs: terminals
                .agent_specs
                .iter()
                .filter(|(pane_id, _)| registry.contains_pane(pane_id))
                .map(|(pane_id, spec)| (pane_id.clone(), spec.clone()))
                .collect(),
            pane_shells: terminals
                .pane_shells
                .iter()
                .filter(|(pane_id, _)| registry.contains_pane(pane_id))
                .map(|(pane_id, shell)| (pane_id.clone(), shell.clone()))
                .collect(),
            // Leaf lock, taken last (after registry → terminals) and dropped
            // before the write below.
            leases: self
                .lock_leases()?
                .iter()
                .filter(|(pane_id, _)| registry.contains_pane(pane_id))
                .map(|(pane_id, held)| (pane_id.clone(), held.clone()))
                .collect(),
            projects: self
                .lock_projects()?
                .iter()
                .map(|(name, project)| {
                    let mut project = project.clone();
                    project
                        .panes
                        .retain(|pane_id| registry.contains_pane(pane_id));
                    (name.clone(), project)
                })
                .collect(),
        };
        drop(terminals);
        drop(registry);

        let data = serde_json::to_vec_pretty(&persisted)
            .map_err(|error| format!("failed to encode workspace: {error}"))?;
        let temp_path = self.persist_path.with_extension("json.tmp");
        write_file_atomic(&temp_path, &self.persist_path, &data, true)
    }

    fn read_scrollback_for(&self, pane_ids: &[String]) -> HashMap<String, String> {
        // (H2) Bound the AGGREGATE scrollback attached to a bootstrap response.
        // Each pane's read is capped at SCROLLBACK_REPLAY_LIMIT_BYTES, but a
        // framed (v2) response is rejected over MAX_FRAME_BYTES — so without an
        // aggregate bound, enough grown panes make every v2 bootstrap
        // undeliverable. Panes are filled in order until the budget runs out;
        // a pane whose tail doesn't fit is truncated, and later panes simply
        // get no scrollback (they still appear in the snapshot itself).
        //
        // The budget is measured on the SERIALIZED size: String::from_utf8_lossy
        // can expand an invalid-byte tail ~3x, and JSON escaping grows control
        // bytes (ESC is common in scrollback) up to 6x (\u00XX), so raw byte
        // counts can't guarantee the response stays under the frame cap.
        let mut budget = BOOTSTRAP_SCROLLBACK_BUDGET_BYTES;
        let mut scrollback = HashMap::new();
        for pane_id in pane_ids {
            if budget == 0 {
                break;
            }
            let Some(mut data) = read_scrollback_tail(
                &self.scrollback_dir,
                pane_id,
                budget.min(SCROLLBACK_REPLAY_LIMIT_BYTES),
            ) else {
                continue;
            };
            if data.is_empty() {
                continue;
            }
            let mut serialized_len = serialized_json_len(&data);
            while serialized_len > budget && !data.is_empty() {
                let halved = data.len() / 2;
                truncate_scrollback_replay(&mut data, halved);
                serialized_len = serialized_json_len(&data);
            }
            if data.is_empty() {
                continue;
            }
            budget -= serialized_len;
            scrollback.insert(pane_id.clone(), data);
        }
        scrollback
    }

    fn take_spawn_on_bootstrap(&self) -> Result<bool, String> {
        let mut should_spawn = self
            .spawn_on_bootstrap
            .lock()
            .map_err(|_| "daemon bootstrap lock poisoned".to_string())?;
        let value = *should_spawn;
        *should_spawn = false;
        Ok(value)
    }

    fn lock_registry(&self) -> Result<MutexGuard<'_, PaneRegistry>, String> {
        self.registry
            .lock()
            .map_err(|_| "pane registry lock poisoned".to_string())
    }

    fn lock_persist(&self) -> Result<MutexGuard<'_, ()>, String> {
        self.persist_lock
            .lock()
            .map_err(|_| "persist lock poisoned".to_string())
    }

    fn lock_terminals(&self) -> Result<MutexGuard<'_, TerminalStore>, String> {
        self.terminals
            .lock()
            .map_err(|_| "terminal store lock poisoned".to_string())
    }

    fn lock_layout(&self) -> Result<MutexGuard<'_, Option<Value>>, String> {
        self.layout
            .lock()
            .map_err(|_| "layout lock poisoned".to_string())
    }

    fn should_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
    }

    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::SeqCst)
    }
}

pub struct AppState {
    cwd: PathBuf,
    daemon: Mutex<Option<DaemonClient>>,
    subscription_started: Mutex<bool>,
}

impl AppState {
    fn new() -> Self {
        Self {
            cwd: resolve_workspace_dir(),
            daemon: Mutex::new(None),
            subscription_started: Mutex::new(false),
        }
    }

    /// Connect to (or spawn) the daemon lazily, caching the client. A failure is
    /// returned to the calling command — which the frontend surfaces as a boot
    /// error — instead of panicking the GUI process during startup.
    fn client(&self) -> Result<DaemonClient, String> {
        let mut guard = self
            .daemon
            .lock()
            .map_err(|_| "daemon state lock poisoned".to_string())?;
        if let Some(client) = guard.as_ref() {
            return Ok(client.clone());
        }
        let client = DaemonClient::connect_or_spawn(self.cwd.clone())?;
        *guard = Some(client.clone());
        Ok(client)
    }

    fn ensure_subscription(&self, app: AppHandle, client: &DaemonClient) {
        let Ok(mut started) = self.subscription_started.lock() else {
            return;
        };
        if *started {
            return;
        }
        client.start_subscription(app);
        *started = true;
    }
}

#[tauri::command]
fn bootstrap_workspace(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<WorkspaceSnapshot, String> {
    let client = state.client()?;
    let snapshot = client.request(DaemonRequest::BootstrapWorkspace)?;
    state.ensure_subscription(app, &client);
    Ok(snapshot)
}

#[tauri::command]
fn create_pane(
    title: Option<String>,
    profile: Option<String>,
    state: State<'_, AppState>,
) -> Result<Pane, String> {
    state
        .client()?
        .request(DaemonRequest::CreatePane { title, profile })
}

#[tauri::command]
fn close_pane(pane_id: String, state: State<'_, AppState>) -> Result<WorkspaceSnapshot, String> {
    state
        .client()?
        .request(DaemonRequest::ClosePane { pane_id })
}

#[tauri::command]
fn rename_pane(pane_id: String, title: String, state: State<'_, AppState>) -> Result<Pane, String> {
    state
        .client()?
        .request(DaemonRequest::RenamePane { pane_id, title })
}

#[tauri::command]
fn ensure_pane_terminal(pane_id: String, state: State<'_, AppState>) -> Result<CommandOk, String> {
    state
        .client()?
        .request(DaemonRequest::EnsurePaneTerminal { pane_id })
}

#[tauri::command]
fn restart_pane_terminal(pane_id: String, state: State<'_, AppState>) -> Result<CommandOk, String> {
    state
        .client()?
        .request(DaemonRequest::RestartPaneTerminal { pane_id })
}

/// GUI keystrokes are attributed to this machine's operator (the same
/// `user@host` label `ctl` defaults to), so a pane held by someone else
/// refuses them and a pane the operator took accepts them
/// (docs/design/keyboard-lease-and-ledger.md §6 M2). A daemon predating the
/// lease capability rejects the variant with a serde "unknown variant"
/// error; fall back to the unattributed write so the GUI keeps working
/// against it.
#[tauri::command]
fn write_to_pane(
    pane_id: String,
    data: String,
    state: State<'_, AppState>,
) -> Result<CommandOk, String> {
    let client = state.client()?;
    match client.request(DaemonRequest::SendInputAs {
        pane_id: pane_id.clone(),
        input: data.clone(),
        holder: effective_holder(&client),
        generation: None,
    }) {
        Err(error) if error.contains("unknown variant") => {
            client.request(DaemonRequest::WriteToPane { pane_id, data })
        }
        result => result,
    }
}

/// The holder label this client writes and takes leases as: the credential's
/// holder when `SGIAN_CLIENT_TOKEN` names one, else `$SGIAN_HOLDER`/user@host.
#[tauri::command]
fn client_holder(state: State<'_, AppState>) -> String {
    match state.client() {
        Ok(client) => effective_holder(&client),
        Err(_) => default_holder(),
    }
}

#[tauri::command]
fn take_lease(
    pane_id: String,
    force: bool,
    why: Option<String>,
    state: State<'_, AppState>,
) -> Result<LeaseInfo, String> {
    let client = state.client()?;
    let holder = effective_holder(&client);
    client.request(DaemonRequest::TakeLease {
        pane_id,
        holder,
        force,
        why,
    })
}

#[tauri::command]
fn release_lease(
    pane_id: String,
    note: String,
    state: State<'_, AppState>,
) -> Result<LeaseInfo, String> {
    let client = state.client()?;
    let holder = effective_holder(&client);
    client.request(DaemonRequest::ReleaseLease {
        pane_id,
        holder,
        note,
        generation: None,
    })
}

#[tauri::command]
fn lease_status(pane_id: String, state: State<'_, AppState>) -> Result<LeaseInfo, String> {
    state
        .client()?
        .request(DaemonRequest::LeaseStatus { pane_id })
}

#[tauri::command]
fn resize_pane_terminal(
    pane_id: String,
    cols: u16,
    rows: u16,
    state: State<'_, AppState>,
) -> Result<CommandOk, String> {
    state.client()?.request(DaemonRequest::ResizePaneTerminal {
        pane_id,
        cols,
        rows,
    })
}

#[tauri::command]
fn set_active_pane(pane_id: String, state: State<'_, AppState>) -> Result<CommandOk, String> {
    state
        .client()?
        .request(DaemonRequest::SetActivePane { pane_id })
}

#[tauri::command]
fn update_workspace_layout(layout: Value, state: State<'_, AppState>) -> Result<CommandOk, String> {
    state
        .client()?
        .request(DaemonRequest::UpdateWorkspaceLayout { layout })
}

#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Result<Value, String> {
    state.client()?.request(DaemonRequest::GetConfig)
}

#[tauri::command]
fn write_config(config: Value, state: State<'_, AppState>) -> Result<CommandOk, String> {
    state
        .client()?
        .request(DaemonRequest::WriteConfig { config })
}

/// Create an agent pane with an immutable provider/model selection. Older
/// callers that omit both fields continue to get Claude with its default
/// model.
#[tauri::command]
fn create_agent_pane(
    title: Option<String>,
    backend: Option<AgentBackendKind>,
    model: Option<String>,
    state: State<'_, AppState>,
) -> Result<Pane, String> {
    state
        .client()?
        .request(DaemonRequest::CreateAgentPaneWithSpec {
            title,
            backend,
            model,
        })
}

/// (T2) Post one user message to an agent pane's conversation.
#[tauri::command]
fn send_agent_message(
    pane_id: String,
    text: String,
    message_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<CommandOk, String> {
    state.client()?.request(DaemonRequest::SendAgentMessage {
        pane_id,
        text,
        message_id,
    })
}

/// (T2) Answer a pending permission_request event (allow, or deny with
/// feedback to the agent).
#[tauri::command]
fn agent_approval(
    pane_id: String,
    request_id: String,
    allow: bool,
    message: Option<String>,
    state: State<'_, AppState>,
) -> Result<CommandOk, String> {
    state.client()?.request(DaemonRequest::AgentApproval {
        pane_id,
        request_id,
        allow,
        message,
    })
}

/// (T2) Interrupt the agent's current turn.
#[tauri::command]
fn interrupt_agent(pane_id: String, state: State<'_, AppState>) -> Result<CommandOk, String> {
    state
        .client()?
        .request(DaemonRequest::InterruptAgent { pane_id })
}

fn run_daemon_from_args(args: &[String]) -> Result<(), String> {
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
fn configure_daemon_process(command: &mut Command) {
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
fn reset_daemon_startup_log(data_dir: &Path) -> PathBuf {
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
fn record_daemon_startup_error(args: &[String], error: &str) {
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

fn daemon_startup_log_excerpt(path: &Path) -> Option<String> {
    const MAX_EXCERPT_BYTES: usize = 8 * 1024;
    let data = fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let start = data.len().saturating_sub(MAX_EXCERPT_BYTES);
    let excerpt = String::from_utf8_lossy(&data[start..]).trim().to_string();
    (!excerpt.is_empty()).then_some(excerpt)
}

fn format_daemon_start_failure(
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

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Detach the daemon process from the launching terminal session.
///
/// On Unix, calls `setsid()` (with a `fork()` fallback if the process is a
/// session leader). On Windows, this is a no-op: there is no `setsid`/`fork`
/// equivalent, and the spawning client currently sets NO creation flags, so an
/// auto-spawned daemon would inherit the client's console (until the spawn
/// site passes `CREATE_NO_WINDOW` / `DETACHED_PROCESS`). SIGHUP has no Windows
/// analog.
fn detach_from_session() {
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
fn install_shutdown_signal_handlers() {
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
fn setup_log_writer(log_dir: &Path) -> (NonBlocking, WorkerGuard) {
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
fn open_log_file(path: &Path) -> std::io::Result<File> {
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
struct BoundedFileWriter {
    dir: PathBuf,
    file: Option<File>,
    written: u64,
}

impl BoundedFileWriter {
    /// Create a new `BoundedFileWriter` for `dir`. Rotates the pre-existing
    /// `daemon.log` to `daemon.log.old` if it exceeds `LOG_MAX_BYTES` at startup,
    /// then opens (or creates) `daemon.log` in append mode with 0600 perms.
    fn new(dir: &Path) -> std::io::Result<Self> {
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
    fn rotate(&mut self) {
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
enum AcceptErrorClass {
    Transient,
    // Only constructed from unix raw OS errors (EMFILE/ENFILE/ENOBUFS/ENOMEM).
    #[cfg_attr(not(unix), allow(dead_code))]
    ResourcePressure,
    Fatal,
}

fn classify_accept_error(error: &std::io::Error) -> AcceptErrorClass {
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

fn run_daemon(cwd: PathBuf, socket_path: PathBuf, data_dir: PathBuf) -> Result<(), String> {
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
fn setup_config_watcher(
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
fn run_daemon_with_config(
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
fn run_daemon_with_config_and_warnings(
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
                if let Some(uid) = peer_uid(&stream) {
                    // SAFETY: getuid has no preconditions and cannot fail.
                    let own = unsafe { libc::getuid() };
                    if uid != own {
                        tracing::warn!(
                            workspace_key = %server.workspace_key,
                            event = "peer_uid_rejected",
                            peer_uid = uid,
                            "dropping connection from another user"
                        );
                        drop(stream);
                        continue;
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
fn negotiate_wire_version(client_max_wire_version: Option<u16>) -> u16 {
    client_max_wire_version
        .unwrap_or(1)
        .min(DAEMON_MAX_WIRE_VERSION)
}

/// Capabilities the daemon advertises in the handshake response so clients/SDKs can
/// feature-detect (architecture.md §5.2): `framed` = the v2 length-prefixed envelope
/// is supported; `persistent` = a v2 connection may carry multiple sequential
/// requests. Returned alongside `negotiated_wire_version` in the response `result`.
fn daemon_capabilities() -> Vec<String> {
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
fn client_capabilities() -> Vec<String> {
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
fn wait_peer_disconnected(stream: &TransportStream) -> bool {
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
fn wait_peer_disconnected(stream: &TransportStream) -> bool {
    stream.is_peer_disconnected()
}

/// Fallback for any future transport without a non-consuming liveness probe.
#[cfg(not(any(unix, windows)))]
fn wait_peer_disconnected(_stream: &TransportStream) -> bool {
    false
}

fn handle_daemon_client(server: Arc<DaemonServer>, stream: TransportStream) -> Result<(), String> {
    handle_daemon_client_with_handshake_budget(server, stream, HANDSHAKE_READ_TIMEOUT)
}

/// `handshake_budget` is the TOTAL deadline for the hello/auth phase (L18) — a
/// parameter rather than the const directly so tests can drive it with
/// millisecond budgets.
fn handle_daemon_client_with_handshake_budget(
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
        // further requests are served on it.
        begin_subscription(
            &server,
            stream,
            negotiated_wire_version,
            client_wants_subscribe_ack,
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
fn begin_subscription(
    server: &Arc<DaemonServer>,
    stream: TransportStream,
    wire_version: u16,
    send_ack: bool,
) {
    let sub_id = match server.router.add_subscriber(stream, wire_version) {
        Ok(id) => id,
        Err((reason, mut stream)) => {
            // (M5) Over the subscriber cap: report a clean error on the
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
            return;
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
}

/// (L18) Remaining budget for the v1 hello/auth phase: `total` minus the time
/// since `started`, or an error once the phase has overrun. Factored out of
/// `HandshakeDeadline` so tests can drive it with millisecond-scale budgets.
fn handshake_budget_remaining(started: Instant, total: Duration) -> Result<Duration, String> {
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
struct HandshakeDeadline<'a> {
    stream: &'a mut TransportStream,
    started: Instant,
    total: Duration,
}

impl<'a> HandshakeDeadline<'a> {
    fn new(stream: &'a mut TransportStream, started: Instant, total: Duration) -> Self {
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
struct StallReadTimeout<'a> {
    stream: &'a mut TransportStream,
    stall_timeout: Duration,
    deadline: Option<Instant>,
}

impl<'a> StallReadTimeout<'a> {
    fn new(stream: &'a mut TransportStream, stall_timeout: Duration) -> Self {
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
fn serve_framed_connection(
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
            begin_subscription(&server, stream, wire_version, client_wants_subscribe_ack);
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
fn read_ipc_line<R: BufRead>(reader: &mut R) -> Result<String, String> {
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

fn write_json_line<T: Serialize>(stream: &mut TransportStream, value: &T) -> Result<(), String> {
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

/// The v2 length-prefixed framed wire envelope (architecture.md §5.1):
/// `MAGIC b"SGN2" | u16-BE wire version | u32-BE payload length | JSON payload`.
///
/// The JSON payload is the existing `DaemonRequest`/`IpcResponse`/`DaemonEvent`
/// serde representation, byte-for-byte unchanged from the v1 newline path
/// (`read_ipc_line`/`write_json_line`); only the envelope differs. `LENGTH` is
/// bounded by `MAX_FRAME_BYTES` on both encode and decode, and any malformed,
/// oversized, truncated, or unsupported-version frame is a clean bounded error:
/// it never panics, blocks unboundedly on a closed peer, or allocates the
/// declared size before validating it.
///
/// These are the foundation of the v2 protocol. The capability handshake wires
/// `read`/`write` into the live connection path (a v2-negotiated connection serves
/// a framed request/response via `serve_framed_request`); the persistent
/// multi-request loop + framed event streaming extend that path next.
mod frame;

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
fn authenticate_stream_at(socket_path: &Path, token: &str) -> Result<TransportStream, String> {
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
struct DaemonConnection {
    reader: BufReader<TransportStream>,
    /// Negotiated wire version: `min(client_max, daemon_max)`; 1 against a v1 daemon
    /// or when the response omits `negotiated_wire_version` (graceful fallback).
    wire_version: u16,
    /// Whether the daemon advertised the `framed` capability in its handshake
    /// response. Both this and a v2 `wire_version` are required to switch to framing.
    advertises_framed: bool,
    /// Whether the daemon advertised `subscribe-ack`: a Subscribe is then
    /// acknowledged with a first SubscribeAck event once registration is done (M8).
    advertises_subscribe_ack: bool,
}

/// Default client-side read deadline for one request/response round-trip. Without
/// it, a wedged daemon (see H2's history) pins every caller — each GUI invoke
/// thread, every ctl command — in a blocking read forever. Streams with
/// legitimately unbounded gaps (event subscriptions, waits) opt out explicitly
/// via `set_read_timeout`.
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(20);

impl DaemonConnection {
    /// Connect to the daemon at `socket_path` and complete the capability handshake.
    fn connect(socket_path: &Path, token: &str) -> Result<Self, String> {
        Self::connect_with_timeout(socket_path, token, Some(CLIENT_READ_TIMEOUT))
    }

    /// `connect` with an explicit read deadline (tests use a short one to prove a
    /// silent daemon can't hang the client).
    fn connect_with_timeout(
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
    fn set_read_timeout(&self, timeout: Option<Duration>) {
        let _ = self.reader.get_ref().set_read_timeout(timeout);
    }

    /// Drive the client side of the capability handshake over an already-connected
    /// `stream`: send the newline-JSON hello carrying the legacy `version: 1` (so a
    /// v1 daemon still accepts it) plus the additive `max_wire_version` = the framed
    /// wire version, read the newline-JSON handshake response, and record the
    /// negotiated wire version + whether framing was advertised. Hello and response
    /// stay newline-JSON so a v1 peer can read them (VAL-IPC-012/024).
    fn handshake(
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
    fn await_subscribe_ack(&mut self) -> Result<(), String> {
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
    fn uses_framing(&self) -> bool {
        self.wire_version >= frame::WIRE_VERSION && self.advertises_framed
    }

    /// Write one request over the negotiated protocol (framed v2 or newline v1).
    fn write_request(&mut self, request: &DaemonRequest) -> Result<(), String> {
        if self.uses_framing() {
            frame::write(self.reader.get_mut(), request)
        } else {
            write_json_line(self.reader.get_mut(), request)
        }
    }

    /// Read one response over the negotiated protocol; a clean EOF before a response
    /// arrives is a clear error rather than a hang.
    fn read_response(&mut self) -> Result<IpcResponse, String> {
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
    fn request(&mut self, request: &DaemonRequest) -> Result<IpcResponse, String> {
        self.write_request(request)?;
        self.read_response()
    }

    /// Read the next event from a subscribed connection. `Ok(None)` is a clean EOF
    /// (the daemon closed the stream), letting the subscribe loops terminate
    /// cleanly. On the newline path an un-decodable line is skipped (the historical
    /// behavior — the daemon only ever writes well-formed events on a stream); on the
    /// framed path a decode error is terminal (a frame stream cannot resync mid-frame).
    fn read_event(&mut self) -> Result<Option<DaemonEvent>, String> {
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

fn no_daemon_error(cwd: &Path) -> String {
    format!(
        "no daemon running for workspace {} (open the app or run a mutating ctl command to start one)",
        cwd.display()
    )
}

/// Check that the persisted `cwd` in `data_dir/workspace.json` (if any) matches the
/// connecting `cwd`. A mismatch indicates either a workspace_key hash collision
/// (two different cwds hashing to the same key) or data-dir tampering — in either
/// case the client must refuse rather than silently serve another workspace's
/// panes, scrollback, and token. A fresh workspace (no workspace.json) or a
/// corrupt one (unparseable) passes this check; those conditions are handled
/// elsewhere (fresh → seeded, corrupt → logged + reseeded).
fn check_persisted_cwd(cwd: &Path, data_dir: &Path) -> Result<(), String> {
    let persist_path = data_dir.join(WORKSPACE_FILE);
    let data = match fs::read_to_string(&persist_path) {
        Ok(data) => data,
        Err(_) => return Ok(()), // no persisted file — fresh workspace
    };
    let persisted: PersistedWorkspace = match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(_) => return Ok(()), // corrupt — handled by load_workspace's fallback
    };
    let connecting = canonical_workspace_path(cwd);
    if !persisted.cwd.is_empty() && !workspace_cwds_match(Path::new(&persisted.cwd), cwd) {
        return Err(format!(
            "workspace_key collision detected: the persisted workspace cwd '{}' does not match \
             the connecting cwd '{}'; refusing to serve mismatched workspace data. \
             If this is intentional, remove the workspace data for this key.",
            persisted.cwd,
            connecting.display()
        ));
    }
    Ok(())
}

/// Resolve the identity used by both workspace-key derivation and BOTH cwd
/// collision guards. On Windows, canonicalization also folds ordinary
/// case-insensitive path spellings to the filesystem's stored spelling, so
/// `C:\\Craig\\tools\\sgian` and `C:\\craig\\tools\\Sgian` converge. If the
/// path no longer exists, preserve the raw spelling rather than weakening the
/// collision guard with a guessed normalization.
fn canonical_workspace_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn workspace_cwds_match(left: &Path, right: &Path) -> bool {
    canonical_workspace_path(left) == canonical_workspace_path(right)
}

/// Create `path` (recursively) and set owner-only (0700) permissions on Unix.
/// On non-Unix the directory is created without explicit mode (OS-default ACLs).
fn ensure_private_dir(path: &Path) -> Result<(), String> {
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
fn set_private_file_permissions(path: &Path) -> Result<(), String> {
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

fn remove_stale_socket(path: &Path) -> Result<(), String> {
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
fn open_lock_file(path: &Path) -> Result<File, String> {
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
fn acquire_daemon_lock(socket_path: &Path) -> Result<Option<File>, String> {
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
fn daemon_lock_is_held(socket_path: &Path) -> bool {
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
fn daemon_instance_is_held(cwd: &Path, socket_path: &Path) -> Result<bool, String> {
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
struct WindowsDaemonMutex {
    handle: windows_sys::Win32::Foundation::HANDLE,
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
fn acquire_windows_daemon_mutex(workspace_key: &str) -> Result<Option<WindowsDaemonMutex>, String> {
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
fn windows_daemon_mutex_is_held(workspace_key: &str) -> Result<bool, String> {
    match acquire_windows_daemon_mutex(workspace_key)? {
        Some(guard) => {
            drop(guard);
            Ok(false)
        }
        None => Ok(true),
    }
}

fn load_or_create_token(data_dir: &Path) -> Result<String, String> {
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

fn read_token(path: &Path) -> Result<Option<String>, String> {
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
fn load_clients_file(path: &Path) -> ClientsFile {
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
fn save_clients_file(path: &Path, file: &ClientsFile) -> Result<(), String> {
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

/// (M6) The uid of the process at the other end of a Unix socket.
#[cfg(target_os = "macos")]
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: the fd is open for the stream's lifetime and both out-pointers
    // are valid for the call.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    (rc == 0).then_some(uid)
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
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
fn peer_uid(_stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    None
}

/// Fill `bytes` with cryptographically secure randomness from the OS.
#[cfg(unix)]
fn fill_secure_random(bytes: &mut [u8]) -> Result<(), String> {
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(bytes))
        .map_err(|error| format!("failed to read /dev/urandom: {error}"))
}

/// Windows: BCrypt's system-preferred RNG (CNG). `/dev/urandom` does not exist
/// here, so without this branch no daemon token could ever be created on
/// Windows (H5).
#[cfg(windows)]
fn fill_secure_random(bytes: &mut [u8]) -> Result<(), String> {
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

fn create_token() -> Result<String, String> {
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
fn constant_time_eq(left: &str, right: &str) -> bool {
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

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn emit_daemon_event(app: &AppHandle, event: DaemonEvent) {
    match event {
        DaemonEvent::PtyOutput { pane_id, data } => {
            emit_pty_output(app, &pane_id, data);
        }
        DaemonEvent::PaneEnded { pane_id, exit_code } => {
            let _ = app.emit("pane-ended", PaneEnded { pane_id, exit_code });
        }
        DaemonEvent::PaneCreated { pane } => {
            let _ = app.emit("pane-created", pane);
        }
        DaemonEvent::PaneClosed { pane_id } => {
            let _ = app.emit("pane-closed", PaneClosed { pane_id });
        }
        DaemonEvent::PaneRenamed { pane } => {
            let _ = app.emit("pane-renamed", pane);
        }
        DaemonEvent::ConfigChanged { config } => {
            let _ = app.emit("config-changed", config);
        }
        // (T1) Agent state transitions ride through to the frontend verbatim.
        DaemonEvent::AgentState {
            pane_id,
            agent,
            attention,
            mode,
        } => {
            let unattended = is_unattended_mode(mode.as_deref());
            let _ = app.emit(
                "agent-state",
                json!({
                    "pane_id": pane_id,
                    "agent": agent,
                    "attention": attention,
                    "mode": mode,
                    "unattended": unattended,
                }),
            );
        }
        DaemonEvent::OutputWarning {
            pane_id,
            added,
            total,
        } => {
            let _ = app.emit(
                "output-warning",
                json!({ "pane_id": pane_id, "added": added, "total": total }),
            );
        }
        DaemonEvent::AgentUsage { pane_id, usage } => {
            let _ = app.emit("agent-usage", json!({ "pane_id": pane_id, "usage": usage }));
        }
        // The whole project table after a change; the overview groups by it.
        DaemonEvent::ProjectsChanged { projects } => {
            let _ = app.emit("projects-changed", json!({ "projects": projects }));
        }
        // Keyboard lease transitions ride to the frontend as `lease-state`
        // (docs/design/keyboard-lease-and-ledger.md); the M2 client work
        // renders them. Unknown to older frontends, which ignore the name.
        DaemonEvent::LeaseState {
            pane_id,
            transition,
            holder,
            since_ms,
            note,
        } => {
            let _ = app.emit(
                "lease-state",
                json!({
                    "pane_id": pane_id,
                    "transition": transition,
                    "holder": holder,
                    "since_ms": since_ms,
                    "note": note,
                }),
            );
        }
        // (T2) Normalized agent conversation events. The Tauri payload keeps
        // the contract's {pane_id, event} shape (the daemon-wire field is
        // `payload` only because of the enum's internal tag — see the
        // AgentEvent variant's comment).
        DaemonEvent::AgentEvent { pane_id, event } => {
            let _ = app.emit(
                "agent-event",
                json!({
                    "pane_id": pane_id,
                    "event": event,
                }),
            );
        }
        // Consumed by await_subscribe_ack before the event loop starts; if one
        // ever reaches here it carries nothing the GUI needs.
        DaemonEvent::SubscribeAck => {}
    }
}

/// The decoded result of `load_workspace`: the registry, pty sizes, layout,
/// whether a persisted file was found, persisted per-pane runtime states, and
/// whether the persisted file was corrupt (unparseable).
struct LoadedWorkspace {
    registry: PaneRegistry,
    sizes: HashMap<String, PtySize>,
    layout: Option<Value>,
    restored_from_disk: bool,
    pane_states: HashMap<String, PaneRuntimeState>,
    /// (T1) Manual agent marks restored from workspace.json (empty for a fresh
    /// or corrupt workspace).
    agents: HashMap<String, String>,
    /// (T2) Agent-pane CLI session ids restored from workspace.json
    /// (`agents_v2`; empty for a fresh, corrupt, or pre-T2 workspace).
    agents_v2: HashMap<String, String>,
    agent_specs: HashMap<String, AgentPaneSpec>,
    /// Frozen shell profile overrides restored from workspace.json (empty for
    /// a fresh, corrupt, or pre-§4 workspace).
    pane_shells: HashMap<String, ShellConfig>,
    /// Held keyboard leases restored from workspace.json (empty for a fresh,
    /// corrupt, or pre-lease workspace).
    leases: HashMap<String, HeldLease>,
    projects: HashMap<String, Project>,
    was_corrupt: bool,
    /// The cwd recorded in the persisted workspace.json, if the file was parsed
    /// successfully. Used by the daemon-side collision check (defense-in-depth:
    /// the client also checks before connecting).
    persisted_cwd: Option<String>,
}

fn load_workspace(persist_path: &Path, cwd: String) -> LoadedWorkspace {
    let data = match fs::read_to_string(persist_path) {
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

    match serde_json::from_str::<PersistedWorkspace>(&data) {
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

fn read_scrollback(scrollback_dir: &Path, pane_id: &str) -> Option<String> {
    read_scrollback_tail(scrollback_dir, pane_id, SCROLLBACK_REPLAY_LIMIT_BYTES)
}

const SCROLLBACK_SEARCH_NEEDLE_MAX_BYTES: usize = 512;
const SCROLLBACK_SEARCH_DEFAULT_LIMIT: usize = 100;
const SCROLLBACK_SEARCH_MAX_LIMIT: usize = 1000;
const SCROLLBACK_LINES_MAX_PER_REQUEST: usize = 2000;

/// Remove terminal control sequences so search and citation see what a
/// person saw: CSI (`ESC [ … final`), OSC/DCS/APC/PM/SOS strings (to BEL or
/// `ESC \`), two-byte `ESC x` escapes, carriage returns and other C0 bytes
/// (tabs and newlines kept). Malformed sequences are dropped to end of text.
fn strip_terminal_controls(text: &str) -> String {
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
fn scrollback_text_lines(scrollback_dir: &Path, pane_id: &str) -> Vec<String> {
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
fn search_lines(
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
fn read_scrollback_tail(scrollback_dir: &Path, pane_id: &str, limit: usize) -> Option<String> {
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
fn serialized_json_len(data: &str) -> usize {
    serde_json::to_vec(data)
        .map(|encoded| encoded.len())
        .unwrap_or(usize::MAX)
}

/// Truncate `data` to at most `max_bytes`, preferring to cut just after the
/// last newline in range so a truncated scrollback replay doesn't end mid-ANSI
/// escape (same garble concern as the scrollback cap, L9), and always ending on
/// a UTF-8 char boundary. Used by the bootstrap aggregate budget (H2).
fn truncate_scrollback_replay(data: &mut String, max_bytes: usize) {
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

fn open_scrollback_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .private_mode()
        .open(path)
}

/// Advance `index` forward to the next UTF-8 character boundary so a byte-offset
/// slice never starts in the middle of a multibyte sequence (which would render as
/// a replacement glyph). Drops at most 3 bytes.
fn utf8_boundary_at_or_after(data: &[u8], mut index: usize) -> usize {
    while index < data.len() && (data[index] & 0xC0) == 0x80 {
        index += 1;
    }
    index
}

/// Write `data` to `temp_path` and atomically rename it onto `final_path`. The file
/// is created 0600. With `durable`, the temp file and the parent directory are
/// fsynced so the replacement survives a crash/power loss; scrollback caps skip
/// that (best-effort replay data on a hot path).
fn write_file_atomic(
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

fn cap_scrollback_file(scrollback_dir: &Path, pane_id: &str) -> Result<(), String> {
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
fn cap_scrollback_file_to(
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

fn scrollback_path(scrollback_dir: &Path, pane_id: &str) -> PathBuf {
    scrollback_dir.join(format!("{pane_id}.ansi"))
}

/// Remove scrollback files for panes no longer in the registry (orphans from a
/// reader racing ClosePane). With `include_temps` (startup only — no cap can be
/// in flight before the daemon serves), also remove leftover `.ansi.tmp` cap
/// litter from a crash mid-cap; the runtime sweep passes false so an in-flight
/// cap's temp file is never deleted under it.
fn prune_orphan_scrollback(
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
fn workspace_key(path: &Path) -> String {
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
fn legacy_workspace_key(path: &Path) -> String {
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
fn app_support_dir() -> PathBuf {
    app_support_dir_in(&dirs::data_dir().unwrap_or_else(std::env::temp_dir))
}

fn app_support_dir_in(data_root: &Path) -> PathBuf {
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

fn ensure_app_private_roots() -> Result<(), String> {
    let app_dir = app_support_dir();
    ensure_private_dir(&app_dir)?;
    ensure_private_dir(&app_dir.join("workspaces"))?;
    ensure_private_dir(&app_dir.join(RUNTIME_DIR))
}

fn workspace_data_dir(workspace_key: &str) -> PathBuf {
    app_support_dir().join("workspaces").join(workspace_key)
}

fn workspace_data_dir_for(path: &Path, workspace_key: &str) -> PathBuf {
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
fn workspace_data_dir_in(workspaces_root: &Path, path: &Path, workspace_key: &str) -> PathBuf {
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

fn workspace_runtime_dir(workspace_key: &str) -> PathBuf {
    app_support_dir().join(RUNTIME_DIR).join(workspace_key)
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().cloned();
        }
    }
    None
}

fn is_valid_pane_id(pane_id: &str) -> bool {
    pane_id
        .strip_prefix("pane-")
        .map(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or(false)
}

fn next_pane_id_after(panes: &[Pane]) -> u64 {
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

fn clean_title(title: Option<String>) -> Option<String> {
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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn default_shell() -> String {
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
fn default_unix_shell(shell: Option<String>, is_macos: bool) -> String {
    shell.unwrap_or_else(|| {
        if is_macos {
            "/bin/zsh".to_string()
        } else {
            "/bin/sh".to_string()
        }
    })
}

fn resolve_workspace_dir() -> PathBuf {
    let explicit = std::env::var_os("SGIAN_WORKSPACE").map(PathBuf::from);
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return path;
    }

    resolve_workspace_dir_from_cwd(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// The cwd-based half of `resolve_workspace_dir`, factored out so tests don't
/// have to chdir the whole (parallel) test process.
fn resolve_workspace_dir_from_cwd(current: PathBuf) -> PathBuf {
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
struct ControlOptions {
    workspace: PathBuf,
    json: bool,
    args: Vec<String>,
}

#[derive(Debug, Serialize)]
struct WorkspaceInfo {
    key: String,
    cwd: String,
    panes: usize,
    active_pane_id: Option<String>,
}

/// Non-secret discovery metadata for native clients. The authentication token
/// is intentionally never included: clients read it from the owner-private
/// token file after locating that file through this response.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct NativeIpcEndpoint {
    transport: String,
    endpoint: String,
    token_path: String,
    workspace: String,
    workspace_key: String,
    protocol_version: u32,
    capabilities: Vec<String>,
}

fn native_ipc_endpoint(client: &DaemonClient) -> Result<NativeIpcEndpoint, String> {
    let workspace = canonical_workspace_path(&client.cwd);
    Ok(NativeIpcEndpoint {
        transport: if cfg!(windows) {
            "named_pipe".to_string()
        } else {
            "unix_socket".to_string()
        },
        endpoint: transport_endpoint(&client.socket_path)
            .map_err(|error| format!("failed to resolve daemon endpoint: {error}"))?,
        token_path: client.data_dir.join(TOKEN_FILE).display().to_string(),
        workspace: workspace.display().to_string(),
        workspace_key: workspace_key(&workspace),
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec!["subscribe-ack".to_string()],
    })
}

fn control_ipc_endpoint(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let info = native_ipc_endpoint(client)?;
    if json_output {
        return write_json_stdout(&info);
    }

    let mut stdout = std::io::stdout();
    writeln!(stdout, "transport\t{}", info.transport)
        .and_then(|_| writeln!(stdout, "endpoint\t{}", info.endpoint))
        .and_then(|_| writeln!(stdout, "token\t{}", info.token_path))
        .and_then(|_| writeln!(stdout, "workspace\t{}", info.workspace))
        .map_err(|error| format!("failed to write stdout: {error}"))
}

fn is_control_invocation(args: &[String]) -> bool {
    args.get(1).map(String::as_str) == Some(CTL_ARG)
        || args
            .first()
            .and_then(|path| Path::new(path).file_stem())
            .and_then(|name| name.to_str())
            .map(|name| matches!(name, "sgianctl" | "sgian2ctl"))
            .unwrap_or(false)
}

fn daemon_socket_for_key(key: &str) -> PathBuf {
    workspace_runtime_dir(key).join(SOCKET_FILE)
}

fn daemon_token_for_key(key: &str) -> Option<String> {
    read_token(&workspace_data_dir(key).join(TOKEN_FILE))
        .ok()
        .flatten()
}

/// True if a daemon is accepting and authenticating on this workspace's socket.
fn daemon_is_alive(key: &str) -> bool {
    let Some(token) = daemon_token_for_key(key) else {
        return false;
    };
    daemon_is_alive_probed(&daemon_socket_for_key(key), &token)
}

/// (07-19 CLI low) Liveness probe behind `ctl daemons` / `ctl shutdown --all`:
/// probe the workspace flock FIRST. No lock held ⇒ no live daemon ⇒ report
/// not-running WITHOUT pinging — a wedged workspace's ping would otherwise
/// cost up to CLIENT_READ_TIMEOUT per row. Lock held ⇒ ping as before (the
/// daemon may be alive, or wedged; only the ping distinguishes).
fn daemon_is_alive_probed(socket_path: &Path, token: &str) -> bool {
    if !daemon_lock_is_held(socket_path) {
        return false;
    }
    daemon_is_alive_at(socket_path, token)
}

/// Probe liveness + auth over the NEGOTIATED protocol (`DaemonConnection`, which
/// negotiates framed v2 and gracefully falls back to newline v1 against an old
/// daemon). Routing the probe here keeps EVERY ctl path — including `ctl daemons`
/// and `ctl shutdown --all`, which call `daemon_is_alive` — negotiating by default,
/// while still reporting an old v1-only daemon alive.
fn daemon_is_alive_at(socket_path: &Path, token: &str) -> bool {
    DaemonConnection::connect(socket_path, token).is_ok()
}

/// Send a request to an already-running daemon by workspace key (never spawns one).
fn daemon_request_by_key(key: &str, request: DaemonRequest) -> Result<IpcResponse, String> {
    let token = daemon_token_for_key(key).ok_or_else(|| format!("no token for workspace {key}"))?;
    let mut conn = DaemonConnection::connect(&daemon_socket_for_key(key), &token)?;
    conn.request(&request)
}

fn workspace_keys() -> Vec<String> {
    let root = app_support_dir().join("workspaces");
    let mut keys = Vec::new();
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            if let Some(key) = entry.file_name().to_str().map(ToString::to_string) {
                keys.push(key);
            }
        }
    }
    keys.sort();
    keys
}

fn workspace_cwd_for_key(key: &str) -> String {
    fs::read_to_string(workspace_data_dir(key).join(WORKSPACE_FILE))
        .ok()
        .and_then(|data| serde_json::from_str::<PersistedWorkspace>(&data).ok())
        .map(|persisted| persisted.cwd)
        .unwrap_or_default()
}

/// The most `ctl hook` / `ctl statusline` read from stdin: a Claude Code
/// payload is a few KiB; anything past this is not one.
const CTL_STDIN_PAYLOAD_MAX: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct HookArgs {
    event: Option<String>,
    notification_type: Option<String>,
    pid: Option<u32>,
    /// Read the hook payload from stdin (the default; `--no-stdin` for scripts
    /// that pass everything as flags).
    read_stdin: bool,
}

/// `hook [--event NAME] [--type NAME] [--pid N] [--no-stdin]`.
fn parse_hook_args(args: &[String]) -> Result<HookArgs, String> {
    let mut parsed = HookArgs {
        read_stdin: true,
        ..HookArgs::default()
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--event" => {
                parsed.event = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--event requires a NAME".to_string())?,
                );
                index += 1;
            }
            "--type" => {
                parsed.notification_type = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--type requires a NAME".to_string())?,
                );
                index += 1;
            }
            "--pid" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--pid requires a number".to_string())?;
                parsed.pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --pid '{value}'"))?,
                );
                index += 1;
            }
            "--no-stdin" => parsed.read_stdin = false,
            other => return Err(format!("unexpected argument for hook: {other}")),
        }
        index += 1;
    }
    if !parsed.read_stdin && parsed.event.is_none() {
        return Err("--no-stdin needs --event NAME".to_string());
    }
    Ok(parsed)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct StatuslineArgs {
    pid: Option<u32>,
    /// A status-line command to run after reporting, fed the same payload;
    /// its stdout is passed through so the user's own line still shows.
    then: Vec<String>,
}

/// `statusline [--pid N] [--exec COMMAND [ARGS...]]`. (`--` cannot be the
/// separator: the global ctl parser consumes it.)
fn parse_statusline_args(args: &[String]) -> Result<StatuslineArgs, String> {
    let mut parsed = StatuslineArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pid" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--pid requires a number".to_string())?;
                parsed.pid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid --pid '{value}'"))?,
                );
                index += 1;
            }
            "--exec" => {
                parsed.then = args[index + 1..].to_vec();
                if parsed.then.is_empty() {
                    return Err("--exec needs a COMMAND to run".to_string());
                }
                break;
            }
            other => return Err(format!("unexpected argument for statusline: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

/// Send one status-line payload to the daemon that owns the calling process:
/// the payload's own cwd first (a session's cwd is its workspace more often
/// than not), then the ctl workspace, then every running daemon. Returns the
/// daemon's answer, or a `mapped: false` reason.
fn report_status_payload(workspace: PathBuf, pid: u32, payload: &Value) -> Value {
    let request = DaemonRequest::AgentStatus {
        pid,
        payload: payload.clone(),
    };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(cwd) = payload["workspace"]["current_dir"]
        .as_str()
        .or_else(|| payload["cwd"].as_str())
        .filter(|text| !text.is_empty())
    {
        candidates.push(PathBuf::from(cwd));
    }
    candidates.push(workspace);
    let mut tried = std::collections::HashSet::new();
    let mut answer = json!({ "mapped": false, "reason": "no running daemon" });
    for cwd in candidates {
        let key = workspace_key(&cwd);
        if !tried.insert(key.clone()) {
            continue;
        }
        let Ok(client) = DaemonClient::connect_existing(cwd) else {
            continue;
        };
        if let Ok(result) = client.request::<Value>(request.clone()) {
            answer = result;
            if answer["mapped"] == json!(true) {
                answer["workspace_key"] = json!(key);
                return answer;
            }
        }
    }
    for key in workspace_keys() {
        if !tried.insert(key.clone()) || !daemon_is_alive(&key) {
            continue;
        }
        let Ok(response) = daemon_request_by_key(&key, request.clone()) else {
            continue;
        };
        if response.ok && response.result["mapped"] == json!(true) {
            answer = response.result;
            answer["workspace_key"] = json!(key);
            return answer;
        }
    }
    answer
}

/// `ctl statusline`: the command Claude Code's status line runs. Reads the
/// payload from stdin, reports it to the owning daemon, then prints a line:
/// the output of the user's own status command when one follows `--exec` (fed
/// the same payload), else a compact default. Always exits 0 and always
/// prints something, so wiring Sgian in never costs the user their status
/// line. `--json` prints the daemon's answer instead.
fn control_statusline(
    workspace: PathBuf,
    parsed: StatuslineArgs,
    json_output: bool,
) -> Result<(), String> {
    let mut raw = String::new();
    let _ = std::io::stdin()
        .take(CTL_STDIN_PAYLOAD_MAX)
        .read_to_string(&mut raw);
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let answer = if payload.is_object() {
        report_status_payload(
            workspace,
            parsed.pid.unwrap_or(std::process::id()),
            &payload,
        )
    } else {
        json!({ "mapped": false, "reason": "stdin was not a JSON object" })
    };
    if json_output {
        return write_json_stdout(&answer);
    }
    let mut stdout = std::io::stdout();
    if let Some((program, rest)) = parsed.then.split_first() {
        let child = Command::new(program)
            .args(rest)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(raw.as_bytes());
            }
            if let Ok(output) = child.wait_with_output() {
                let _ = stdout.write_all(&output.stdout);
                let _ = stdout.flush();
                return Ok(());
            }
        }
        // The user's command failed: fall through to the default so the
        // status line is never blank because of us.
    }
    let line = AgentUsage::from_status_payload(&payload)
        .map(|usage| usage.summary(now_millis() / 1000))
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "sgian".to_string());
    let _ = writeln!(stdout, "{line}");
    Ok(())
}

/// The fields `ctl hook` reads from a Claude Code hook payload (stdin JSON).
/// Everything is optional so a payload from a newer CLI still parses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
struct HookPayload {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    notification_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

/// Build the daemon request for a hook: flags win over the payload, the pid
/// defaults to this process (the daemon walks up from it to the pane).
fn hook_request(
    parsed: &HookArgs,
    payload: &HookPayload,
    own_pid: u32,
) -> Result<DaemonRequest, String> {
    let event = parsed
        .event
        .clone()
        .or_else(|| payload.hook_event_name.clone())
        .filter(|event| !event.trim().is_empty())
        .ok_or_else(|| "hook payload has no hook_event_name; pass --event NAME".to_string())?;
    Ok(DaemonRequest::AgentSignal {
        pid: parsed.pid.unwrap_or(own_pid),
        event,
        notification_type: parsed
            .notification_type
            .clone()
            .or_else(|| payload.notification_type.clone()),
        message: payload.message.clone(),
        session_id: payload.session_id.clone(),
    })
}

/// `ctl hook`: the command a Claude Code hook runs. Reads the hook payload
/// from stdin, asks the workspace daemon (this cwd first, then every other
/// running daemon) which pane owns the calling process, and hands it the
/// hook's reading. Exits 0 whatever happens: a hook must never fail the
/// session it reports on. `--json` prints the daemon's answer.
fn control_hook(workspace: PathBuf, parsed: HookArgs, json_output: bool) -> Result<(), String> {
    let payload: HookPayload = if parsed.read_stdin {
        let mut raw = String::new();
        let _ = std::io::stdin()
            .take(CTL_STDIN_PAYLOAD_MAX)
            .read_to_string(&mut raw);
        if raw.trim().is_empty() {
            HookPayload::default()
        } else {
            serde_json::from_str(&raw).unwrap_or_default()
        }
    } else {
        HookPayload::default()
    };
    let request = match hook_request(&parsed, &payload, std::process::id()) {
        Ok(request) => request,
        Err(error) => {
            return if json_output {
                write_json_stdout(&json!({ "mapped": false, "reason": error }))
            } else {
                Ok(())
            };
        }
    };
    let mut answer = json!({ "mapped": false, "reason": "no running daemon" });
    let mut tried = std::collections::HashSet::new();
    // This workspace first: a hook fires in the session's cwd, which is the
    // pane's cwd more often than not.
    let own_key = workspace_key(&workspace);
    tried.insert(own_key.clone());
    if let Ok(client) = DaemonClient::connect_existing(workspace) {
        if let Ok(result) = client.request::<Value>(request.clone()) {
            if result["mapped"] == json!(true) {
                answer = result;
                answer["workspace_key"] = json!(own_key);
            } else {
                answer = result;
            }
        }
    }
    if answer["mapped"] != json!(true) {
        for key in workspace_keys() {
            if !tried.insert(key.clone()) || !daemon_is_alive(&key) {
                continue;
            }
            let Ok(response) = daemon_request_by_key(&key, request.clone()) else {
                continue;
            };
            if response.ok && response.result["mapped"] == json!(true) {
                answer = response.result;
                answer["workspace_key"] = json!(key);
                break;
            }
        }
    }
    if json_output {
        return write_json_stdout(&answer);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityVerb {
    List,
    Issue,
    Revoke,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentityArgs {
    verb: IdentityVerb,
    holder: Option<String>,
    scopes: Vec<String>,
    id: Option<String>,
}

/// `identity [list] | issue --holder H [--scope read,write,admin] | revoke ID`.
fn parse_identity_args(args: &[String]) -> Result<IdentityArgs, String> {
    let verb = match args.first().map(String::as_str) {
        None | Some("list") => IdentityVerb::List,
        Some("issue") => IdentityVerb::Issue,
        Some("revoke") => IdentityVerb::Revoke,
        Some(other) => return Err(format!("unknown identity command: {other}")),
    };
    let mut parsed = IdentityArgs {
        verb,
        holder: None,
        scopes: Vec::new(),
        id: None,
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut index = if args.is_empty() { 0 } else { 1 };
    while index < args.len() {
        match args[index].as_str() {
            "--holder" => {
                parsed.holder = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--holder requires a NAME".to_string())?,
                );
                index += 1;
            }
            "--scope" | "--scopes" => {
                parsed.scopes.push(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--scope requires a list".to_string())?,
                );
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for identity: {other}"));
            }
            other => positionals.push(other.to_string()),
        }
        index += 1;
    }
    match verb {
        IdentityVerb::List => {
            if !positionals.is_empty() || parsed.holder.is_some() || !parsed.scopes.is_empty() {
                return Err("identity list takes no arguments".to_string());
            }
        }
        IdentityVerb::Issue => {
            if !positionals.is_empty() {
                return Err("identity issue takes --holder and --scope only".to_string());
            }
            if parsed.holder.is_none() {
                return Err("identity issue needs --holder NAME".to_string());
            }
        }
        IdentityVerb::Revoke => {
            if positionals.len() != 1 || parsed.holder.is_some() || !parsed.scopes.is_empty() {
                return Err("identity revoke needs exactly one credential ID".to_string());
            }
            parsed.id = positionals.pop();
        }
    }
    Ok(parsed)
}

fn control_identity(
    client: &DaemonClient,
    parsed: IdentityArgs,
    json_output: bool,
) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    let scopes_text = |record: &Value| {
        record["scopes"]
            .as_array()
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default()
    };
    match parsed.verb {
        IdentityVerb::List => {
            let records: Value = client.request(DaemonRequest::IdentityList)?;
            if json_output {
                return write_json_stdout(&records);
            }
            for record in records.as_array().into_iter().flatten() {
                writeln!(
                    stdout,
                    "{}\t{}\t{}\t{}",
                    record["id"].as_str().unwrap_or("-"),
                    record["holder"].as_str().unwrap_or("-"),
                    scopes_text(record),
                    if record["revoked_at_ms"].is_null() {
                        "active"
                    } else {
                        "revoked"
                    }
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        IdentityVerb::Issue => {
            let record: Value = client.request(DaemonRequest::IdentityIssue {
                holder: parsed.holder.unwrap_or_default(),
                scopes: parsed.scopes,
            })?;
            if json_output {
                return write_json_stdout(&record);
            }
            writeln!(
                stdout,
                "{}\t{}\t{}\n{}\n(shown once; export SGIAN_CLIENT_TOKEN=… on the client)",
                record["id"].as_str().unwrap_or("-"),
                record["holder"].as_str().unwrap_or("-"),
                scopes_text(&record),
                record["token"].as_str().unwrap_or("-")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))
        }
        IdentityVerb::Revoke => {
            let record: Value = client.request(DaemonRequest::IdentityRevoke {
                id: parsed.id.unwrap_or_default(),
            })?;
            if json_output {
                return write_json_stdout(&record);
            }
            writeln!(
                stdout,
                "{}\t{}\trevoked",
                record["id"].as_str().unwrap_or("-"),
                record["holder"].as_str().unwrap_or("-")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))
        }
    }
}

fn control_list_daemons(json_output: bool) -> Result<(), String> {
    let daemons = workspace_keys()
        .into_iter()
        .map(|key| {
            let cwd = workspace_cwd_for_key(&key);
            let alive = daemon_is_alive(&key);
            json!({ "key": key, "cwd": cwd, "alive": alive })
        })
        .collect::<Vec<_>>();

    if json_output {
        return write_json_stdout(&daemons);
    }
    let mut stdout = std::io::stdout();
    for daemon in &daemons {
        let alive = daemon["alive"].as_bool().unwrap_or(false);
        writeln!(
            stdout,
            "{}\t{}\t{}",
            if alive { "alive" } else { "dead " },
            daemon["key"].as_str().unwrap_or(""),
            daemon["cwd"].as_str().unwrap_or(""),
        )
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

fn control_shutdown_all(json_output: bool) -> Result<(), String> {
    let mut stopped = Vec::new();
    for key in workspace_keys() {
        if !daemon_is_alive(&key) {
            continue;
        }
        if daemon_request_by_key(&key, DaemonRequest::Shutdown)
            .map(|response| response.ok)
            .unwrap_or(false)
        {
            stopped.push(key);
        }
    }

    if json_output {
        write_json_stdout(&json!({ "stopped": stopped }))
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "stopped {} daemon(s)", stopped.len())
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// (H4) The `ctl shutdown` branch for an UNRESPONSIVE daemon (connect failed).
/// A HELD workspace lock means the daemon is alive but wedged — that is a
/// non-zero error, not the documented exit-0 "no daemon running" no-op, which
/// would both misreport and leave the wedged process holding its shells
/// hostage. A FREE (or absent) lock keeps the genuine no-daemon no-op.
fn shutdown_when_unresponsive(socket_path: &Path, json_output: bool) -> Result<(), String> {
    if daemon_lock_is_held(socket_path) {
        return Err(
            "daemon not responding (lock held); the daemon is alive but wedged — kill the process and retry"
                .to_string(),
        );
    }
    if json_output {
        write_json_stdout(&json!({ "ok": true, "stopped": false }))
    } else {
        writeln!(std::io::stdout(), "no daemon running")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn run_control_cli_from_args(args: &[String]) -> Result<(), String> {
    let raw_args = if args.get(1).map(String::as_str) == Some(CTL_ARG) {
        args.iter().skip(2).cloned().collect()
    } else {
        args.iter().skip(1).cloned().collect()
    };
    let options = parse_control_options(raw_args)?;

    let Some(command) = options.args.first().map(String::as_str) else {
        print_control_help()?;
        return Ok(());
    };

    match command {
        "help" | "--help" | "-h" => print_control_help(),
        "workspaces" => {
            ensure_no_extra_args("workspaces", &options.args[1..])?;
            control_list_workspaces(options.json)
        }
        "ipc-endpoint" => {
            ensure_no_extra_args("ipc-endpoint", &options.args[1..])?;
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_ipc_endpoint(&client, options.json)
        }
        "panes" | "list" => {
            ensure_no_extra_args(command, &options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_list_panes(&client, options.json)
        }
        "new" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_new_pane(&client, &options.args[1..], options.json)
        }
        "status" => {
            let client = DaemonClient::connect_existing(options.workspace)?;
            let status_args = &options.args[1..];
            if status_args
                .iter()
                .any(|arg| arg == "--verbose" || arg == "-v")
            {
                if let Some(extra) = status_args
                    .iter()
                    .find(|arg| *arg != "--verbose" && *arg != "-v")
                {
                    return Err(format!("unexpected argument for status: {extra}"));
                }
                control_status_verbose(&client, options.json)
            } else {
                if status_args.len() > 1 {
                    return Err(format!(
                        "unexpected argument for status: {}",
                        status_args[1]
                    ));
                }
                control_pane_status(&client, status_args, options.json)
            }
        }
        "send" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_send_input(&client, &options.args[1..])
        }
        "interrupt" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_interrupt(&client, &options.args[1..])
        }
        "restart" => {
            if options.args.len() > 2 {
                return Err(format!(
                    "unexpected argument for restart: {}",
                    options.args[2]
                ));
            }
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_restart_pane(&client, &options.args[1..], options.json)
        }
        "lease" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_lease_args(&options.args[1..])?;
            // Status never spawns a daemon; take/release start one on demand
            // like the other mutating pane commands.
            let client = if parsed.verb == LeaseVerb::Status {
                DaemonClient::connect_existing(options.workspace)?
            } else {
                DaemonClient::connect_or_spawn(options.workspace)?
            };
            control_lease(&client, parsed, options.json)
        }
        "project" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_project_args(&options.args[1..])?;
            let client = if matches!(
                parsed.verb,
                ProjectVerb::List | ProjectVerb::Show | ProjectVerb::Ledger
            ) {
                DaemonClient::connect_existing(options.workspace)?
            } else {
                DaemonClient::connect_or_spawn(options.workspace)?
            };
            control_project(&client, parsed, options.json)
        }
        "kranz" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_kranz_args(&options.args[1..])?;
            let client = if parsed.verb == KranzVerb::Status {
                DaemonClient::connect_existing(options.workspace)?
            } else {
                DaemonClient::connect_or_spawn(options.workspace)?
            };
            control_kranz(&client, parsed, options.json)
        }
        "search" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_search_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_search(&client, parsed, options.json)
        }
        "lines" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_lines_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_lines(&client, parsed, options.json)
        }
        "ledger" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_ledger_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_ledger(&client, parsed, options.json)
        }
        "attach" => {
            if options.args.len() > 2 {
                return Err(format!(
                    "unexpected argument for attach: {}",
                    options.args[2]
                ));
            }
            // (07-19 CLI low) the global parser consumes --json, but attach
            // streams raw PTY output — there is no JSON shape; fail loudly
            // instead of silently ignoring the flag.
            if options.json {
                return Err("--json is not supported for attach".to_string());
            }
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_attach_pane(&client, &options.args[1..])
        }
        "logs" => {
            // (07-19 CLI low) same silent `--json` trap as attach: the log tail
            // is plain text, so reject the flag instead of ignoring it.
            if options.json {
                return Err("--json is not supported for logs".to_string());
            }
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_logs(&client, &options.args[1..])
        }
        "exec" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_exec(&client, &options.args[1..], options.json)
        }
        "broadcast" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_broadcast(&client, &options.args[1..], options.json)
        }
        "sync" => {
            if options.args.len() > 2 {
                return Err(format!("unexpected argument for sync: {}", options.args[2]));
            }
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_sync_input(&client, &options.args[1..], options.json)
        }
        "run" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_run(&client, &options.args[1..], options.json)
        }
        "process" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_process(&client, &options.args[1..], options.json)
        }
        "diagnostic" | "diagnostics" => {
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_diagnostic(&client, &options.args[1..], options.json)
        }
        "wait" => {
            // wait blocks on an existing pane; never spawn a daemon just to wait.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_wait(&client, &options.args[1..], options.json)
        }
        "snapshot" => {
            // snapshot reads an existing pane; never spawn a daemon just to read it.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_snapshot(&client, &options.args[1..], options.json)
        }
        "find" => {
            // find queries existing panes; never spawn a daemon just to query.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_find(&client, &options.args[1..], options.json)
        }
        "agent" => {
            // agent reads/marks an existing pane; never spawn a daemon for it.
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_agent(&client, &options.args[1..], options.json)
        }
        "daemons" => {
            ensure_no_extra_args("daemons", &options.args[1..])?;
            control_list_daemons(options.json)
        }
        "hook" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_hook_args(&options.args[1..])?;
            control_hook(options.workspace, parsed, options.json)
        }
        "statusline" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_statusline_args(&options.args[1..])?;
            control_statusline(options.workspace, parsed, options.json)
        }
        "identity" => {
            if has_help_flag(&options.args[1..]) {
                return print_control_help();
            }
            let parsed = parse_identity_args(&options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            control_identity(&client, parsed, options.json)
        }
        "whoami" => {
            ensure_no_extra_args("whoami", &options.args[1..])?;
            let client = DaemonClient::connect_existing(options.workspace)?;
            let identity: Value = client.request(DaemonRequest::Whoami)?;
            if options.json {
                return write_json_stdout(&identity);
            }
            let scopes = identity["scopes"]
                .as_array()
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let mut stdout = std::io::stdout();
            writeln!(
                stdout,
                "{}\t{}\t{}\tidentity={}",
                identity["credential"].as_str().unwrap_or("root"),
                identity["holder"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(default_holder),
                scopes,
                identity["identity_policy"].as_str().unwrap_or("open")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))
        }
        "write-config" => {
            let client = DaemonClient::connect_or_spawn(options.workspace)?;
            control_write_config(&client, &options.args[1..], options.json)
        }
        "shutdown" => {
            if let Some(extra) = options.args[1..].iter().find(|arg| *arg != "--all") {
                return Err(format!("unexpected argument for shutdown: {extra}"));
            }
            if options.args.iter().any(|arg| arg == "--all") {
                control_shutdown_all(options.json)
            } else {
                // Stopping a daemon that isn't running is a success, not a reason to
                // spawn one just to kill it.
                let socket_path = daemon_socket_for_key(&workspace_key(&options.workspace));
                match DaemonClient::connect_existing(options.workspace) {
                    Ok(client) => control_shutdown(&client, options.json),
                    Err(_) => shutdown_when_unresponsive(&socket_path, options.json),
                }
            }
        }
        "pane" => control_pane_subcommand(options),
        other => Err(format!("unknown ctl command: {other}")),
    }
}

fn parse_control_options(raw_args: Vec<String>) -> Result<ControlOptions, String> {
    let mut workspace = resolve_workspace_dir();
    let mut json = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut after_double_dash = false;
    let mut freeform = false;
    let mut index = 0;

    while index < raw_args.len() {
        let arg = raw_args[index].as_str();

        if after_double_dash || freeform {
            positionals.push(raw_args[index].clone());
            index += 1;
            continue;
        }

        match arg {
            "--workspace" | "-w" => {
                let value = raw_args
                    .get(index + 1)
                    .ok_or_else(|| "--workspace requires a path".to_string())?;
                workspace = PathBuf::from(value);
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--help" | "-h" => {
                if positionals.first().map(String::as_str) != Some("help") {
                    positionals.insert(0, "help".to_string());
                }
                index += 1;
            }
            "--" => {
                after_double_dash = true;
                index += 1;
            }
            value if value.starts_with('-') && positionals.is_empty() => {
                return Err(format!("unknown ctl option: {value}"));
            }
            value => {
                // The first positional is the subcommand. For commands that take free
                // text (send/exec), stop interpreting later tokens as global flags so a
                // payload containing --json/--workspace is preserved verbatim.
                if positionals.is_empty() && is_freeform_subcommand(value) {
                    freeform = true;
                }
                positionals.push(value.to_string());
                index += 1;
            }
        }
    }

    Ok(ControlOptions {
        workspace,
        json,
        args: positionals,
    })
}

/// The process exit code used when the CLI exits on an error (default 1).
/// `ctl run` stores the failed command's own exit code here so the `ctl run`
/// process mirrors it — scripts branch on `$?` directly instead of parsing
/// stdout/JSON (L2). Clamped to 1..=255 (0 would read as success; >255 wraps).
static CLI_EXIT_CODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// L4: commands with no (or fixed) arguments reject leftovers instead of
/// silently ignoring them — `ctl panes --jsonn` must error, not quietly print
/// the human list because of a typo'd flag.
fn ensure_no_extra_args(command: &str, extra: &[String]) -> Result<(), String> {
    if let Some(unexpected) = extra.first() {
        return Err(format!("unexpected argument for {command}: {unexpected}"));
    }
    Ok(())
}

fn is_freeform_subcommand(name: &str) -> bool {
    // `pane` is freeform so its whole tail reaches the alias re-dispatch
    // verbatim: the second parse then applies the real subcommand's rules,
    // instead of the FIRST parse eating `--json` out of a `pane send` payload
    // (L5 — the alias diverged from the direct command).
    matches!(
        name,
        "send" | "exec" | "broadcast" | "run" | "process" | "pane"
    )
}

/// `ctl write-config` — persist a config to the per-workspace config.json via
/// the daemon's WriteConfig request. The daemon validates and atomically
/// writes the config; the file-watch then live-reloads it and broadcasts a
/// ConfigChanged event. Accepts a file path or an inline JSON string (an
/// argument starting with `{` is treated as inline JSON).
fn control_write_config(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let config_value = if let Some(first) = args.first() {
        if first.starts_with('{') {
            // Inline JSON string.
            serde_json::from_str::<Value>(first)
                .map_err(|error| format!("invalid JSON config: {error}"))?
        } else {
            // File path.
            let content = fs::read_to_string(first)
                .map_err(|error| format!("failed to read config file {first}: {error}"))?;
            serde_json::from_str::<Value>(&content)
                .map_err(|error| format!("invalid JSON config in {first}: {error}"))?
        }
    } else {
        return Err("write-config requires a file path or inline JSON string".to_string());
    };

    let result: CommandOk = client.request(DaemonRequest::WriteConfig {
        config: config_value,
    })?;

    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "config written")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn control_pane_subcommand(mut options: ControlOptions) -> Result<(), String> {
    if options.args.len() < 2 {
        return Err("pane requires a subcommand".to_string());
    }

    let subcommand = options.args.remove(1);
    // `pane` is parsed freeform (L5), so a flag can land in the subcommand slot
    // (`ctl pane --json status`); reject it clearly — global flags go before
    // `pane` or after the real subcommand.
    if subcommand.starts_with('-') {
        return Err(format!(
            "pane requires a subcommand, got {subcommand} (put global flags before 'pane')"
        ));
    }
    options.args[0] = subcommand;
    run_control_cli_from_args(&prepend_ctl_args(options))
}

fn prepend_ctl_args(options: ControlOptions) -> Vec<String> {
    let mut args = vec!["sgian".to_string(), CTL_ARG.to_string()];
    args.push("--workspace".to_string());
    args.push(options.workspace.display().to_string());
    if options.json {
        args.push("--json".to_string());
    }
    args.extend(options.args);
    args
}

fn control_list_workspaces(json_output: bool) -> Result<(), String> {
    let root = app_support_dir().join("workspaces");
    let mut workspaces = Vec::new();

    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(key) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
            else {
                continue;
            };
            let Some(persisted) = fs::read_to_string(path.join(WORKSPACE_FILE))
                .ok()
                .and_then(|data| serde_json::from_str::<PersistedWorkspace>(&data).ok())
            else {
                continue;
            };
            workspaces.push(WorkspaceInfo {
                key,
                cwd: persisted.cwd,
                panes: persisted.panes.len(),
                active_pane_id: persisted.active_pane_id,
            });
        }
    }

    workspaces.sort_by(|left, right| left.cwd.cmp(&right.cwd));
    if json_output {
        write_json_stdout(&workspaces)
    } else {
        let mut stdout = std::io::stdout();
        for workspace in workspaces {
            writeln!(
                stdout,
                "{}\t{}\t{} panes",
                workspace.key, workspace.cwd, workspace.panes
            )
            .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        Ok(())
    }
}

fn control_list_panes(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;
    if json_output {
        return write_json_stdout(&list);
    }

    let mut stdout = std::io::stdout();
    for status in list.panes {
        let active = if list.active_pane_id.as_deref() == Some(status.pane.id.as_str()) {
            "*"
        } else {
            " "
        };
        writeln!(
            stdout,
            "{} {}\t{}\t{:?}",
            active, status.pane.id, status.pane.title, status.state
        )
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

/// Compatibility view used by the original parser tests. Provider-aware
/// parsing below deliberately preserves positional and `--name` title forms.
#[cfg(test)]
fn parse_new_pane_args(args: &[String]) -> Result<(bool, Option<String>), String> {
    let parsed = parse_new_pane_args_with_spec(args)?;
    Ok((parsed.agent, parsed.title))
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedNewPaneArgs {
    agent: bool,
    title: Option<String>,
    backend: Option<AgentBackendKind>,
    model: Option<String>,
    profile: Option<String>,
    /// `--project NAME`: put the new pane in a project right away.
    project: Option<String>,
}

fn parse_new_pane_args_with_spec(args: &[String]) -> Result<ParsedNewPaneArgs, String> {
    let mut agent = false;
    let mut backend = None;
    let mut model = None;
    let mut profile = None;
    let mut project = None;
    let mut title_args = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--agent" => {
                agent = true;
                index += 1;
            }
            "--project" => {
                if project.is_some() {
                    return Err("--project given more than once".to_string());
                }
                project = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--project requires a name".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--profile" => {
                if profile.is_some() {
                    return Err("--profile given more than once".to_string());
                }
                profile = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--profile requires a name".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--backend" => {
                if backend.is_some() {
                    return Err("--backend given more than once".to_string());
                }
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--backend requires claude or droid".to_string())?;
                backend = Some(match value.as_str() {
                    "claude" => AgentBackendKind::Claude,
                    "droid" => AgentBackendKind::Droid,
                    other => {
                        return Err(format!(
                            "unknown agent backend '{other}': expected claude or droid"
                        ))
                    }
                });
                agent = true;
                index += 2;
            }
            "--model" => {
                if model.is_some() {
                    return Err("--model given more than once".to_string());
                }
                model = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--model requires a model id".to_string())?
                        .clone(),
                );
                agent = true;
                index += 2;
            }
            "--name" | "-n" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--name requires a title".to_string())?;
                title_args.push(args[index].clone());
                title_args.push(value.clone());
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected pane option: {other}"));
            }
            _ => {
                // Preserve the long-standing `ctl new NAME` positional form.
                title_args.push(args[index].clone());
                index += 1;
            }
        }
    }
    let title = parse_name_option(&title_args)?;
    Ok(ParsedNewPaneArgs {
        agent,
        title,
        backend,
        model,
        profile,
        project,
    })
}

fn control_new_pane(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let mut parsed = parse_new_pane_args_with_spec(args)?;
    if let Some(profile_name) = parsed.profile.clone() {
        let config_value: Value = client.request(DaemonRequest::GetConfig)?;
        let config: Config = serde_json::from_value(config_value)
            .map_err(|error| format!("invalid config payload: {error}"))?;
        let profile = config
            .profile(&profile_name)
            .ok_or_else(|| format!("unknown profile '{profile_name}'"))?;
        if profile.is_agent_profile() {
            parsed.agent = true;
            if parsed.backend.is_none() {
                parsed.backend = match profile.agent_backend.as_deref() {
                    Some("claude") => Some(AgentBackendKind::Claude),
                    Some("droid") => Some(AgentBackendKind::Droid),
                    _ => None,
                };
            }
            if parsed.model.is_none() {
                parsed.model = profile.agent_model.clone();
            }
        }
        if parsed.title.is_none() {
            parsed.title = Some(profile_name);
        }
    }
    let pane: Pane = if parsed.agent {
        client.request(DaemonRequest::CreateAgentPaneWithSpec {
            title: parsed.title,
            backend: parsed.backend,
            model: parsed.model,
        })?
    } else {
        client.request(DaemonRequest::CreatePane {
            title: parsed.title,
            profile: parsed.profile,
        })?
    };
    if let Some(project) = parsed.project {
        client.request::<Value>(DaemonRequest::ProjectAssign {
            name: project,
            pane_id: pane.id.clone(),
        })?;
    }
    if json_output {
        write_json_stdout(&pane)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "{}\t{}", pane.id, pane.title)
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn control_pane_status(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let pane_ref = args
        .first()
        .map(String::as_str)
        .unwrap_or("active")
        .to_string();
    let pane_id = resolve_pane_ref(client, &pane_ref)?;
    let status: PaneStatus = client.request(DaemonRequest::PaneStatus { pane_id })?;
    if json_output {
        write_json_stdout(&status)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(
            stdout,
            "{}\t{}\t{:?}",
            status.pane.id, status.pane.title, status.state
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Format the human-readable `ctl status --verbose` output (non-JSON form).
/// Includes all editable config fields (VAL-CFG-001): `shell`, `shell_args`,
/// `env` (key names only, values suppressed per VAL-SEC-010), `scrub_env`
/// (variable names by definition), `font_family`, `font_size`, `theme`,
/// `idle_shutdown_secs`, `restore_policy`, `agent_permission_mode`, and the
/// provider binary overrides. Extracted as a pure function so the
/// human-readable form is unit-testable directly.
fn format_status_verbose_human(status: &VerboseStatus) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "subscribers\t{}", status.subscribers);
    let _ = writeln!(out, "uptime\t{}s", status.uptime_secs);
    let _ = writeln!(out, "cwd\t{}", status.cwd);
    for pane in &status.panes {
        let _ = writeln!(
            out,
            "pane\t{}\t{}\t{:?}",
            pane.pane.id, pane.pane.title, pane.state
        );
    }
    let cfg = &status.config;
    // `env` is summarized by `Config::summary` as a map of key→null (values
    // suppressed, VAL-SEC-010). Render its key names only so the human-readable
    // form lists the configured env var names without echoing any values.
    let env_keys = env_key_names_csv(&cfg["env"]);
    let _ = writeln!(
        out,
        "config\tshell={}\tshell_args={}\tenv={}\tscrub_env={}\tfont_family={}\tfont_size={}\ttheme={}\tidle_shutdown_secs={}\trestore_policy={}\tagent_permission_mode={}\tagent_claude_bin={}\tagent_droid_bin={}",
        cfg["shell"],
        cfg["shell_args"],
        env_keys,
        cfg["scrub_env"],
        cfg["font_family"],
        cfg["font_size"],
        cfg["theme"],
        cfg["idle_shutdown_secs"],
        cfg["restore_policy"],
        cfg["agent_permission_mode"],
        cfg["agent_claude_bin"],
        cfg["agent_droid_bin"],
    );
    out
}

/// Render the `env` config-summary value as a comma-separated list of key names.
/// `Config::summary` emits `env` as a JSON object mapping each key to `null`
/// (values suppressed, VAL-SEC-010). This extracts just the key names for the
/// human-readable `status --verbose` line; if the field is absent or not an
/// object, renders `null` to mirror the JSON form. Returns an empty string for
/// an empty env map (no configured env vars).
fn env_key_names_csv(env: &Value) -> String {
    let Some(obj) = env.as_object() else {
        return "null".to_string();
    };
    if obj.is_empty() {
        return String::new();
    }
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn control_status_verbose(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let status: VerboseStatus = client.request(DaemonRequest::StatusVerbose)?;
    if json_output {
        return write_json_stdout(&status);
    }
    let mut stdout = std::io::stdout();
    stdout
        .write_all(format_status_verbose_human(&status).as_bytes())
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    Ok(())
}

// ----- ctl: keyboard lease and ledger (docs/design/keyboard-lease-and-ledger.md) -----

/// The holder label `ctl` attributes its writes and lease claims to:
/// (M6) The holder this process acts as: with a client credential in the
/// environment, the credential's holder (the daemon would refuse any other);
/// else the self-declared default. One `whoami` round trip when credentialed.
fn effective_holder(client: &DaemonClient) -> String {
    if client_token_from_env().is_some() {
        if let Ok(identity) = client.request::<Value>(DaemonRequest::Whoami) {
            if let Some(holder) = identity["holder"].as_str().filter(|h| !h.is_empty()) {
                return holder.to_string();
            }
        }
    }
    default_holder()
}

/// `$SGIAN_HOLDER` when set and valid, else `user@host`.
fn default_holder() -> String {
    if let Ok(configured) = std::env::var("SGIAN_HOLDER") {
        if let Ok(holder) = validate_holder(&configured) {
            return holder;
        }
    }
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "operator".to_string());
    let host = local_hostname();
    let short_host = host.split('.').next().unwrap_or("local");
    validate_holder(&format!("{user}@{short_host}")).unwrap_or_else(|_| "operator".to_string())
}

#[cfg(unix)]
fn local_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into a buffer we
    // own for the duration of the call and NUL-terminates on success; a
    // non-zero return leaves the contents unspecified, so we only read the
    // buffer when it returns 0 and stop at the first NUL.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "local".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() {
        "local".to_string()
    } else {
        name
    }
}

#[cfg(not(unix))]
fn local_hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "local".to_string())
}

/// Pull `--as HOLDER` out of a send/broadcast argument list, stopping at `--`
/// like `parse_lf_flag` so a payload can still contain the literal text.
/// Pull `--generation N` out of a send argument list (before `--`).
fn parse_generation_flag(args: &[String]) -> Result<(Option<u64>, Vec<String>), String> {
    let mut generation = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if passthrough {
            remaining.push(arg.clone());
        } else if arg == "--generation" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--generation requires a number".to_string())?;
            generation = Some(
                value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --generation '{value}'"))?,
            );
            index += 1;
        } else {
            if arg == "--" {
                passthrough = true;
            }
            remaining.push(arg.clone());
        }
        index += 1;
    }
    Ok((generation, remaining))
}

fn parse_as_flag(args: &[String]) -> Result<(Option<String>, Vec<String>), String> {
    let mut holder = None;
    let mut remaining = Vec::with_capacity(args.len());
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if passthrough {
            remaining.push(arg.clone());
        } else if arg == "--as" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--as requires a HOLDER".to_string())?;
            holder = Some(validate_holder(value)?);
            index += 1;
        } else if let Some(value) = arg.strip_prefix("--as=") {
            holder = Some(validate_holder(value)?);
        } else {
            if arg == "--" {
                passthrough = true;
            }
            remaining.push(arg.clone());
        }
        index += 1;
    }
    Ok((holder, remaining))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseVerb {
    Status,
    Take,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseArgs {
    verb: LeaseVerb,
    pane_ref: String,
    holder: Option<String>,
    force: bool,
    why: Option<String>,
    note: Option<String>,
    /// `--generation N`: refuse the release if the lease changed hands.
    generation: Option<u64>,
}

/// `lease [status|take|release] [PANE] [--as HOLDER] [--force --why REASON] [-m NOTE]`.
fn parse_lease_args(args: &[String]) -> Result<LeaseArgs, String> {
    let mut parsed = LeaseArgs {
        verb: LeaseVerb::Status,
        pane_ref: "active".to_string(),
        holder: None,
        force: false,
        why: None,
        note: None,
        generation: None,
    };
    let mut index = 0;
    let mut pane_seen = false;
    if let Some(first) = args.first() {
        match first.as_str() {
            "status" => {
                parsed.verb = LeaseVerb::Status;
                index = 1;
            }
            "take" => {
                parsed.verb = LeaseVerb::Take;
                index = 1;
            }
            "release" => {
                parsed.verb = LeaseVerb::Release;
                index = 1;
            }
            _ => {}
        }
    }
    while index < args.len() {
        let arg = args[index].as_str();
        let take_value = |index: usize, flag: &str| -> Result<String, String> {
            args.get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match arg {
            "--as" => {
                parsed.holder = Some(validate_holder(&take_value(index, "--as")?)?);
                index += 1;
            }
            "--force" => parsed.force = true,
            "--why" => {
                parsed.why = Some(take_value(index, "--why")?);
                index += 1;
            }
            "-m" | "--note" => {
                parsed.note = Some(take_value(index, "-m/--note")?);
                index += 1;
            }
            "--generation" => {
                let value = take_value(index, "--generation")?;
                parsed.generation = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --generation '{value}'"))?,
                );
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for lease: {other}"));
            }
            other => {
                if pane_seen {
                    return Err(format!("unexpected argument for lease: {other}"));
                }
                parsed.pane_ref = other.to_string();
                pane_seen = true;
            }
        }
        index += 1;
    }
    if parsed.verb != LeaseVerb::Take && (parsed.force || parsed.why.is_some()) {
        return Err("--force/--why apply to `lease take`".to_string());
    }
    if parsed.verb == LeaseVerb::Release && parsed.note.is_none() {
        return Err("lease release requires -m NOTE (the hand-back note is mandatory)".to_string());
    }
    if parsed.verb != LeaseVerb::Release && parsed.note.is_some() {
        return Err("-m/--note applies to `lease release`".to_string());
    }
    Ok(parsed)
}

fn format_held_for(held_ms: Option<u64>) -> String {
    match held_ms {
        None => "-".to_string(),
        Some(ms) => {
            let secs = ms / 1000;
            if secs >= 3600 {
                format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
            } else if secs >= 60 {
                format!("{}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            }
        }
    }
}

fn print_lease_info(info: &LeaseInfo, json_output: bool) -> Result<(), String> {
    if json_output {
        return write_json_stdout(info);
    }
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "{}\t{}\t{}\t{}\twrites={}\trefused={}\tgen={}",
        info.pane_id,
        info.policy,
        info.holder.as_deref().unwrap_or("-"),
        format_held_for(info.held_ms),
        info.writes,
        info.refused_writes,
        info.generation
            .map(|generation| generation.to_string())
            .unwrap_or_else(|| "-".to_string())
    )
    .map_err(|error| format!("failed to write stdout: {error}"))
}

/// `ctl lease …` — show, take, or release a pane's keyboard lease. The
/// dispatcher parses first (pure) so status can use a read-only connection.
fn control_lease(
    client: &DaemonClient,
    parsed: LeaseArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let holder = parsed
        .holder
        .clone()
        .unwrap_or_else(|| effective_holder(client));
    let info: LeaseInfo = match parsed.verb {
        LeaseVerb::Status => client.request(DaemonRequest::LeaseStatus { pane_id })?,
        LeaseVerb::Take => client.request(DaemonRequest::TakeLease {
            pane_id,
            holder,
            force: parsed.force,
            why: parsed.why,
        })?,
        LeaseVerb::Release => client.request(DaemonRequest::ReleaseLease {
            pane_id,
            holder,
            note: parsed.note.unwrap_or_default(),
            generation: parsed.generation,
        })?,
    };
    print_lease_info(&info, json_output)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LedgerArgs {
    pane_ref: String,
    limit: usize,
    verify: bool,
}

/// `ledger [PANE] [-n N] [--verify]`.
fn parse_ledger_args(args: &[String]) -> Result<LedgerArgs, String> {
    let mut parsed = LedgerArgs {
        pane_ref: "active".to_string(),
        limit: 0,
        verify: false,
    };
    let mut pane_seen = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--verify" => parsed.verify = true,
            "-n" | "--lines" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "-n requires a count".to_string())?;
                parsed.limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid -n count '{value}'"))?;
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for ledger: {other}"));
            }
            other => {
                if pane_seen {
                    return Err(format!("unexpected argument for ledger: {other}"));
                }
                parsed.pane_ref = other.to_string();
                pane_seen = true;
            }
        }
        index += 1;
    }
    Ok(parsed)
}

/// `ctl ledger …` — print or verify a pane's lease ledger. Reads the file
/// from the workspace data dir directly, so a CLOSED pane's ledger (its id
/// given literally) is still readable; only an open pane needs the daemon to
/// resolve a title.
fn control_ledger(
    client: &DaemonClient,
    parsed: LedgerArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = match resolve_pane_ref(client, &parsed.pane_ref) {
        Ok(pane_id) => pane_id,
        Err(error) => {
            if parsed.pane_ref.starts_with("pane-") {
                parsed.pane_ref.clone()
            } else {
                return Err(error);
            }
        }
    };
    let path = ledger_path(&client.data_dir.join(LEDGER_DIR), &pane_id);
    if parsed.verify {
        return match ledger_verify(&path) {
            Ok(summary) => {
                if json_output {
                    write_json_stdout(&json!({
                        "pane_id": pane_id,
                        "ok": true,
                        "records": summary.records,
                        "head": summary.head,
                    }))
                } else {
                    let mut stdout = std::io::stdout();
                    writeln!(
                        stdout,
                        "ok\t{}\t{} records\thead {}",
                        pane_id,
                        summary.records,
                        if summary.head.is_empty() {
                            "-"
                        } else {
                            summary.head.as_str()
                        }
                    )
                    .map_err(|error| format!("failed to write stdout: {error}"))
                }
            }
            Err(broken) => Err(format!(
                "ledger break in {} at line {}{}: {}",
                path.display(),
                broken.line,
                broken
                    .seq
                    .map(|seq| format!(" (seq {seq})"))
                    .unwrap_or_default(),
                broken.reason
            )),
        };
    }
    let records = read_ledger_tail(&path, parsed.limit);
    if json_output {
        return write_json_stdout(&records);
    }
    let mut stdout = std::io::stdout();
    for record in &records {
        writeln!(stdout, "{record}").map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchArgs {
    pane_ref: String,
    needle: String,
    ignore_case: bool,
    limit: usize,
}

/// `search <PANE> [-i] [-n N] [--] <NEEDLE...>` — flags before the needle;
/// `--` ends flag parsing so a needle can start with `-`.
fn parse_search_args(args: &[String]) -> Result<SearchArgs, String> {
    let mut parsed = SearchArgs {
        pane_ref: String::new(),
        needle: String::new(),
        ignore_case: false,
        limit: 0,
    };
    let mut index = 0;
    let mut needle_parts: Vec<String> = Vec::new();
    let mut passthrough = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if passthrough
            || !needle_parts.is_empty()
            || (arg != "--" && !arg.starts_with('-') && !parsed.pane_ref.is_empty())
        {
            needle_parts.push(arg.to_string());
        } else if arg == "--" {
            passthrough = true;
        } else if arg == "-i" || arg == "--ignore-case" {
            parsed.ignore_case = true;
        } else if arg == "-n" || arg == "--limit" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "-n requires a count".to_string())?;
            parsed.limit = value
                .parse::<usize>()
                .map_err(|_| format!("invalid -n count '{value}'"))?;
            index += 1;
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(format!("unexpected argument for search: {arg}"));
        } else {
            parsed.pane_ref = arg.to_string();
        }
        index += 1;
    }
    if parsed.pane_ref.is_empty() {
        return Err("search requires a pane and a needle".to_string());
    }
    parsed.needle = needle_parts.join(" ");
    if parsed.needle.trim().is_empty() {
        return Err("search requires a needle".to_string());
    }
    Ok(parsed)
}

fn control_search(
    client: &DaemonClient,
    parsed: SearchArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let result: Value = client.request(DaemonRequest::SearchScrollback {
        pane_id,
        needle: parsed.needle,
        ignore_case: parsed.ignore_case,
        limit: parsed.limit,
    })?;
    if json_output {
        return write_json_stdout(&result);
    }
    let mut stdout = std::io::stdout();
    for hit in result["matches"].as_array().into_iter().flatten() {
        writeln!(
            stdout,
            "{}\t{}",
            hit["line"].as_u64().unwrap_or(0),
            hit["text"].as_str().unwrap_or("")
        )
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    if result["truncated"].as_bool().unwrap_or(false) {
        writeln!(stdout, "(more hits; raise -n)")
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinesArgs {
    pane_ref: String,
    from: usize,
    to: usize,
}

/// `lines <PANE> <A>[:<B>]`.
fn parse_lines_args(args: &[String]) -> Result<LinesArgs, String> {
    let [pane_ref, range] = args else {
        return Err("lines requires a pane and a line range A[:B]".to_string());
    };
    let (from, to) = match range.split_once(':') {
        Some((a, b)) => (a, b),
        None => (range.as_str(), range.as_str()),
    };
    let from: usize = from
        .parse()
        .map_err(|_| format!("invalid line number '{from}'"))?;
    let to: usize = to
        .parse()
        .map_err(|_| format!("invalid line number '{to}'"))?;
    if from == 0 || to < from {
        return Err("line range must be 1-based with A <= B".to_string());
    }
    Ok(LinesArgs {
        pane_ref: pane_ref.clone(),
        from,
        to,
    })
}

fn control_lines(
    client: &DaemonClient,
    parsed: LinesArgs,
    json_output: bool,
) -> Result<(), String> {
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let result: Value = client.request(DaemonRequest::ScrollbackLines {
        pane_id,
        from: parsed.from,
        to: parsed.to,
    })?;
    if json_output {
        return write_json_stdout(&result);
    }
    let mut stdout = std::io::stdout();
    let from = result["from"].as_u64().unwrap_or(1) as usize;
    for (offset, line) in result["lines"].as_array().into_iter().flatten().enumerate() {
        writeln!(stdout, "{}\t{}", from + offset, line.as_str().unwrap_or(""))
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectVerb {
    List,
    Show,
    New,
    Add,
    Rm,
    Delete,
    Ledger,
    Dossier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectArgs {
    verb: ProjectVerb,
    name: Option<String>,
    panes: Vec<String>,
    goal: Option<String>,
    repo: Option<String>,
    limit: usize,
    lines: usize,
    out: Option<String>,
}

/// `project list | show NAME | new NAME [--goal TEXT] [--repo PATH] |
/// add NAME PANE... | rm PANE... | delete NAME | ledger NAME [-n N] |
/// dossier NAME [--lines N] [--out FILE]`.
fn parse_project_args(args: &[String]) -> Result<ProjectArgs, String> {
    let verb = match args.first().map(String::as_str) {
        None | Some("list") => ProjectVerb::List,
        Some("show") => ProjectVerb::Show,
        Some("new") | Some("create") => ProjectVerb::New,
        Some("add") | Some("assign") => ProjectVerb::Add,
        Some("rm") | Some("remove") | Some("unassign") => ProjectVerb::Rm,
        Some("delete") => ProjectVerb::Delete,
        Some("ledger") => ProjectVerb::Ledger,
        Some("dossier") => ProjectVerb::Dossier,
        Some(other) => return Err(format!("unknown project command: {other}")),
    };
    let mut parsed = ProjectArgs {
        verb,
        name: None,
        panes: Vec::new(),
        goal: None,
        repo: None,
        limit: 0,
        lines: 0,
        out: None,
    };
    let mut positionals: Vec<String> = Vec::new();
    let mut index = if args.is_empty() { 0 } else { 1 };
    while index < args.len() {
        match args[index].as_str() {
            "--goal" => {
                parsed.goal = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--goal requires TEXT".to_string())?,
                );
                index += 1;
            }
            "--repo" => {
                parsed.repo = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--repo requires PATH".to_string())?,
                );
                index += 1;
            }
            "-n" | "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "-n requires a count".to_string())?;
                parsed.limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid -n count '{value}'"))?;
                index += 1;
            }
            "--lines" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--lines requires a count".to_string())?;
                parsed.lines = value
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --lines count '{value}'"))?;
                if parsed.lines == 0 {
                    return Err("--lines must be at least 1".to_string());
                }
                index += 1;
            }
            "--out" => {
                parsed.out = Some(
                    args.get(index + 1)
                        .cloned()
                        .ok_or_else(|| "--out requires a FILE".to_string())?,
                );
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for project: {other}"));
            }
            other => positionals.push(other.to_string()),
        }
        index += 1;
    }
    match verb {
        ProjectVerb::List => {
            if !positionals.is_empty() {
                return Err("project list takes no arguments".to_string());
            }
        }
        ProjectVerb::Show
        | ProjectVerb::Delete
        | ProjectVerb::New
        | ProjectVerb::Ledger
        | ProjectVerb::Dossier => {
            if positionals.len() != 1 {
                return Err("expected exactly one project NAME".to_string());
            }
            parsed.name = positionals.pop();
        }
        ProjectVerb::Add => {
            if positionals.len() < 2 {
                return Err("project add needs a NAME and at least one PANE".to_string());
            }
            parsed.name = Some(positionals.remove(0));
            parsed.panes = positionals;
        }
        ProjectVerb::Rm => {
            if positionals.is_empty() {
                return Err("project rm needs at least one PANE".to_string());
            }
            parsed.panes = positionals;
        }
    }
    if verb != ProjectVerb::New && (parsed.goal.is_some() || parsed.repo.is_some()) {
        return Err("--goal/--repo apply to `project new`".to_string());
    }
    if verb != ProjectVerb::Ledger && parsed.limit != 0 {
        return Err("-n applies to `project ledger`".to_string());
    }
    if verb != ProjectVerb::Dossier && (parsed.lines != 0 || parsed.out.is_some()) {
        return Err("--lines/--out apply to `project dossier`".to_string());
    }
    Ok(parsed)
}

fn control_project(
    client: &DaemonClient,
    parsed: ProjectArgs,
    json_output: bool,
) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    match parsed.verb {
        ProjectVerb::List => {
            let summaries: Vec<ProjectSummary> = client.request(DaemonRequest::ProjectList)?;
            if json_output {
                return write_json_stdout(&summaries);
            }
            for summary in summaries {
                writeln!(
                    stdout,
                    "{}\tpanes={} live={} needs_input={} working={} idle={} unattended={} held={}{}",
                    summary.project.name,
                    summary.panes,
                    summary.live,
                    summary.needs_input,
                    summary.working,
                    summary.idle,
                    summary.unattended,
                    summary.held,
                    if summary.holders.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", summary.holders.join(", "))
                    }
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Show => {
            let name = parsed.name.unwrap_or_default();
            let detail: Value = client.request(DaemonRequest::ProjectShow { name })?;
            if json_output {
                return write_json_stdout(&detail);
            }
            let summary = &detail["summary"];
            writeln!(
                stdout,
                "{}\t{}",
                summary["project"]["name"].as_str().unwrap_or("-"),
                summary["project"]["goal"].as_str().unwrap_or("")
            )
            .map_err(|error| format!("failed to write stdout: {error}"))?;
            for pane in detail["panes"].as_array().into_iter().flatten() {
                let agent = &pane["agent"];
                writeln!(
                    stdout,
                    "  {}\t{}\t{}\t{}\t{}\t{}{}",
                    pane["id"].as_str().unwrap_or("-"),
                    pane["title"].as_str().unwrap_or("-"),
                    pane["state"].as_str().unwrap_or("-"),
                    agent["agent"].as_str().unwrap_or("-"),
                    agent["attention"].as_str().unwrap_or("-"),
                    pane["holder"].as_str().unwrap_or("-"),
                    if agent["unattended"].as_bool().unwrap_or(false) {
                        "\tUNATTENDED"
                    } else {
                        ""
                    }
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::New => {
            let project: Project = client.request(DaemonRequest::ProjectCreate {
                name: parsed.name.unwrap_or_default(),
                goal: parsed.goal,
                repo: parsed.repo,
            })?;
            if json_output {
                return write_json_stdout(&project);
            }
            writeln!(stdout, "{}", project.name)
                .map_err(|error| format!("failed to write stdout: {error}"))
        }
        ProjectVerb::Add => {
            let name = parsed.name.unwrap_or_default();
            let mut results = Vec::new();
            for pane_ref in &parsed.panes {
                let pane_id = resolve_pane_ref(client, pane_ref)?;
                let result: Value = client.request(DaemonRequest::ProjectAssign {
                    name: name.clone(),
                    pane_id,
                })?;
                results.push(result);
            }
            if json_output {
                return write_json_stdout(&results);
            }
            for result in results {
                writeln!(
                    stdout,
                    "{}\t{}",
                    result["pane_id"].as_str().unwrap_or("-"),
                    result["project"].as_str().unwrap_or("-")
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Rm => {
            let mut results = Vec::new();
            for pane_ref in &parsed.panes {
                let pane_id = resolve_pane_ref(client, pane_ref)?;
                let result: Value = client.request(DaemonRequest::ProjectUnassign { pane_id })?;
                results.push(result);
            }
            if json_output {
                return write_json_stdout(&results);
            }
            for result in results {
                writeln!(
                    stdout,
                    "{}\t{}",
                    result["pane_id"].as_str().unwrap_or("-"),
                    result["project"].as_str().unwrap_or("-")
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Delete => {
            let project: Project = client.request(DaemonRequest::ProjectDelete {
                name: parsed.name.unwrap_or_default(),
            })?;
            if json_output {
                return write_json_stdout(&project);
            }
            writeln!(stdout, "{}\tdeleted", project.name)
                .map_err(|error| format!("failed to write stdout: {error}"))
        }
        ProjectVerb::Ledger => {
            let result: Value = client.request(DaemonRequest::ProjectLedger {
                name: parsed.name.unwrap_or_default(),
                limit: parsed.limit,
            })?;
            if json_output {
                return write_json_stdout(&result);
            }
            for record in result["records"].as_array().into_iter().flatten() {
                writeln!(stdout, "{record}")
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        ProjectVerb::Dossier => {
            let result: Value = client.request(DaemonRequest::ProjectDossier {
                name: parsed.name.unwrap_or_default(),
                lines: parsed.lines,
            })?;
            match parsed.out {
                Some(path) => {
                    let pretty = serde_json::to_string_pretty(&result)
                        .map_err(|error| format!("failed to encode dossier: {error}"))?;
                    fs::write(&path, pretty.as_bytes())
                        .map_err(|error| format!("failed to write {path}: {error}"))?;
                    let panes = result["panes"].as_array().map_or(0, Vec::len);
                    let unverified = result["panes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|pane| pane["ledger"]["chain"]["verified"] == json!(false))
                        .count();
                    if json_output {
                        return write_json_stdout(&json!({
                            "project": result["summary"]["project"]["name"],
                            "file": path,
                            "panes": panes,
                            "unverified_ledgers": unverified,
                        }));
                    }
                    writeln!(
                        stdout,
                        "{}\t{}\t{} pane(s), {} ledger(s) failed verification",
                        result["summary"]["project"]["name"].as_str().unwrap_or("-"),
                        path,
                        panes,
                        unverified
                    )
                    .map_err(|error| format!("failed to write stdout: {error}"))
                }
                None => {
                    if json_output {
                        return write_json_stdout(&result);
                    }
                    let pretty = serde_json::to_string_pretty(&result)
                        .map_err(|error| format!("failed to encode dossier: {error}"))?;
                    writeln!(stdout, "{pretty}")
                        .map_err(|error| format!("failed to write stdout: {error}"))
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KranzVerb {
    Status,
    Bind,
    Unbind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KranzArgs {
    verb: KranzVerb,
    pane_ref: String,
    repo: Option<String>,
}

/// `kranz [status|bind|unbind] [PANE] [--repo PATH]`.
fn parse_kranz_args(args: &[String]) -> Result<KranzArgs, String> {
    let mut parsed = KranzArgs {
        verb: KranzVerb::Status,
        pane_ref: "active".to_string(),
        repo: None,
    };
    let mut index = 0;
    match args.first().map(String::as_str) {
        Some("status") => index = 1,
        Some("bind") => {
            parsed.verb = KranzVerb::Bind;
            index = 1;
        }
        Some("unbind") => {
            parsed.verb = KranzVerb::Unbind;
            index = 1;
        }
        _ => {}
    }
    let mut pane_seen = false;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--repo requires a PATH".to_string())?;
                parsed.repo = Some(value.clone());
                index += 1;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unexpected argument for kranz: {other}"));
            }
            other => {
                if pane_seen {
                    return Err(format!("unexpected argument for kranz: {other}"));
                }
                parsed.pane_ref = other.to_string();
                pane_seen = true;
            }
        }
        index += 1;
    }
    if parsed.verb != KranzVerb::Bind && parsed.repo.is_some() {
        return Err("--repo applies to `kranz bind`".to_string());
    }
    if parsed.verb == KranzVerb::Status && pane_seen {
        return Err("kranz status lists every binding; it takes no PANE".to_string());
    }
    Ok(parsed)
}

fn control_kranz(
    client: &DaemonClient,
    parsed: KranzArgs,
    json_output: bool,
) -> Result<(), String> {
    let result: Value = match parsed.verb {
        KranzVerb::Status => client.request(DaemonRequest::KranzBindings)?,
        KranzVerb::Bind => {
            let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
            client.request(DaemonRequest::KranzBind {
                pane_id,
                repo: parsed.repo,
            })?
        }
        KranzVerb::Unbind => {
            let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
            client.request(DaemonRequest::KranzUnbind { pane_id })?
        }
    };
    if json_output {
        return write_json_stdout(&result);
    }
    let mut stdout = std::io::stdout();
    match parsed.verb {
        KranzVerb::Status => {
            let bindings: HashMap<String, KranzBinding> =
                serde_json::from_value(result).unwrap_or_default();
            let mut rows: Vec<_> = bindings.into_iter().collect();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            for (pane_id, binding) in rows {
                writeln!(
                    stdout,
                    "{pane_id}\t{}\t{}",
                    if binding.manual { "manual" } else { "auto" },
                    binding.repo
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Ok(())
        }
        _ => writeln!(
            stdout,
            "{}\t{}",
            result["pane_id"].as_str().unwrap_or("-"),
            result["binding"]["repo"].as_str().unwrap_or("-")
        )
        .map_err(|error| format!("failed to write stdout: {error}")),
    }
}

fn control_send_input(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    // Help only as the FIRST token (L6): later positions are freeform payload
    // (`send active ls -h` must send "-h").
    if matches!(
        args.first().map(String::as_str),
        Some("--help") | Some("-h")
    ) {
        return print_control_help();
    }
    let (literal_lf, args) = parse_lf_flag(args);
    let (holder, args) = parse_as_flag(&args)?;
    let (generation, args) = parse_generation_flag(&args)?;
    if args.len() < 2 {
        return Err("send requires a pane and input".to_string());
    }

    // (T2) Route by pane kind: to an AGENT pane the text is a chat message
    // (SendAgentMessage — posted verbatim, with no Enter/CR translation and
    // no --lf effect); to a shell pane it stays PTY input.
    let status = resolve_pane_status(client, &args[0])?;
    if status.pane.kind == PaneKind::Agent {
        let text = args[1..].join(" ");
        client.request::<CommandOk>(DaemonRequest::SendAgentMessage {
            message_id: None,
            pane_id: status.pane.id,
            text,
        })?;
        return Ok(());
    }
    let input = decode_cli_text(&args[1..].join(" "), literal_lf);
    match holder {
        Some(holder) => client.request::<CommandOk>(DaemonRequest::SendInputAs {
            pane_id: status.pane.id,
            input,
            holder,
            generation,
        })?,
        None => client.request::<CommandOk>(DaemonRequest::SendInput {
            pane_id: status.pane.id,
            input,
        })?,
    };
    Ok(())
}

/// (T2) `ctl interrupt <pane>` — interrupt an agent pane's current turn.
/// Shell panes have no turn to interrupt; signal them with `send` (Ctrl-C)
/// instead.
fn control_interrupt(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    if args.len() > 1 {
        return Err(format!("unexpected argument for interrupt: {}", args[1]));
    }
    let pane_ref = args.first().map(String::as_str).unwrap_or("active");
    let status = resolve_pane_status(client, pane_ref)?;
    if status.pane.kind != PaneKind::Agent {
        return Err(format!(
            "pane {} is not an agent pane; interrupt applies to agent panes \
             (for shells, send a Ctrl-C instead)",
            status.pane.id
        ));
    }
    client.request::<CommandOk>(DaemonRequest::InterruptAgent {
        pane_id: status.pane.id,
    })?;
    Ok(())
}

fn control_restart_pane(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let pane_ref = args
        .first()
        .map(String::as_str)
        .unwrap_or("active")
        .to_string();
    let pane_id = resolve_pane_ref(client, &pane_ref)?;
    let result: CommandOk = client.request(DaemonRequest::RestartPaneTerminal {
        pane_id: pane_id.clone(),
    })?;
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "restarted {pane_id}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Window of printed scrollback the attach dedupe anchors against. The
/// subscribe→file-read gap is milliseconds of output, so a small window is
/// ample and keeps the (one-time) anchor scan cheap.
const OVERLAP_WINDOW_BYTES: usize = 16 * 1024;
/// Minimum bytes an anchor match must cover before the skipper trusts it —
/// prevents a coincidental short match from suppressing real output.
const OVERLAP_MIN_ANCHOR_BYTES: usize = 32;

/// Dedupe the attach subscribe-then-read overlap (M9): output emitted between
/// the Subscribe registration and the GetScrollback file read is BOTH in the
/// printed scrollback and queued on the subscription, so the first streamed
/// chunks can repeat what was just printed. The skipper anchors the stream's
/// first chunk against a suffix of the printed tail (longest match, with a
/// minimum anchor length) and swallows the stream while it continues that
/// suffix byte-for-byte. Swallowed bytes are by construction identical to bytes
/// already printed, so a mis-anchor's worst case is suppressing output that is
/// indistinguishable from what is already on screen.
struct OverlapSkipper {
    /// Unmatched remainder of the printed tail; empty = dedupe finished.
    remaining: Vec<u8>,
    anchored: bool,
}

impl OverlapSkipper {
    fn new(printed: &[u8]) -> Self {
        let start = printed.len().saturating_sub(OVERLAP_WINDOW_BYTES);
        Self {
            remaining: printed[start..].to_vec(),
            anchored: false,
        }
    }

    /// Return the portion of `chunk` that should be printed.
    fn filter<'a>(&mut self, chunk: &'a [u8]) -> &'a [u8] {
        if self.remaining.is_empty() || chunk.is_empty() {
            self.remaining.clear();
            return chunk;
        }

        if !self.anchored {
            // Anchor: the duplicate stream is some SUFFIX of the printed tail.
            // Find the longest suffix the first chunk continues; require the
            // matched span to be long enough to not be a coincidence.
            for offset in 0..self.remaining.len() {
                let suffix = &self.remaining[offset..];
                let match_len = suffix.len().min(chunk.len());
                if match_len < OVERLAP_MIN_ANCHOR_BYTES {
                    break;
                }
                if suffix.len() >= chunk.len() {
                    if suffix.starts_with(chunk) {
                        self.anchored = true;
                        self.remaining = suffix[chunk.len()..].to_vec();
                        return &[];
                    }
                } else if chunk.starts_with(suffix) {
                    // The duplicate ends inside this chunk.
                    let skip = suffix.len();
                    self.remaining.clear();
                    return &chunk[skip..];
                }
            }
            // No trustworthy anchor: treat the stream as all-new from here on.
            self.remaining.clear();
            return chunk;
        }

        // Anchored: the stream must continue the remainder exactly; any
        // divergence ends the dedupe (print everything from here).
        if self.remaining.len() >= chunk.len() {
            if self.remaining.starts_with(chunk) {
                self.remaining.drain(..chunk.len());
                return &[];
            }
        } else if chunk.starts_with(self.remaining.as_slice()) {
            let skip = self.remaining.len();
            self.remaining.clear();
            return &chunk[skip..];
        }
        self.remaining.clear();
        chunk
    }
}

fn control_attach_pane(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    let pane_ref = args
        .first()
        .map(String::as_str)
        .unwrap_or("active")
        .to_string();
    let pane_id = resolve_pane_ref(client, &pane_ref)?;

    // Subscribe before the liveness check below so a pane that dies in between
    // still delivers its ended/closed event through the subscription (no missed-
    // event hang).
    let mut conn = client.connect()?;
    conn.write_request(&DaemonRequest::Subscribe)?;
    conn.await_subscribe_ack()?;
    // Attached output is legitimately sparse; drop the request read deadline.
    conn.set_read_timeout(None);

    // A dedicated request, not BootstrapWorkspace: bootstrap reads every pane's
    // scrollback and has side effects (first-boot shell spawning, persistence).
    let result: Value = client.request(DaemonRequest::GetScrollback {
        pane_id: pane_id.clone(),
    })?;
    let mut overlap = OverlapSkipper::new(&[]);
    if let Some(scrollback) = result["scrollback"].as_str().filter(|s| !s.is_empty()) {
        let mut stdout = std::io::stdout();
        stdout
            .write_all(scrollback.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("failed to write stdout: {error}"))?;
        // Output emitted between the Subscribe above and this file read is both
        // printed (in the scrollback) and queued (on the subscription) — dedupe
        // it against the printed tail instead of showing it twice (M9).
        overlap = OverlapSkipper::new(scrollback.as_bytes());
    }

    // A pane with no live session will never produce output or an ended event:
    // its scrollback is everything there is, so stop instead of blocking forever.
    let status: PaneStatus = client.request(DaemonRequest::PaneStatus {
        pane_id: pane_id.clone(),
    })?;
    if status.state != PaneRuntimeState::Live {
        return Ok(());
    }

    loop {
        match conn.read_event()? {
            Some(DaemonEvent::PtyOutput { pane_id: id, data }) if id == pane_id => {
                let printable = overlap.filter(data.as_bytes());
                if printable.is_empty() {
                    continue;
                }
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(printable)
                    .and_then(|_| stdout.flush())
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Some(DaemonEvent::PaneEnded { pane_id: id, .. })
            | Some(DaemonEvent::PaneClosed { pane_id: id })
                if id == pane_id =>
            {
                return Ok(());
            }
            Some(_) => {}
            None => return Ok(()),
        }
    }
}

/// The fully-parsed plan for `ctl logs` — options resolved up front. Pure (no
/// I/O, no daemon calls), so it is unit-testable in isolation like
/// `parse_exec_args`.
#[derive(Debug, PartialEq, Eq)]
struct LogsPlan {
    /// Limit output to the last N lines (`-n`/`--lines`). `None` = all lines.
    lines: Option<usize>,
    /// Stream new entries as they are written (`--follow`/`-f`).
    follow: bool,
}

/// Parse `ctl logs` flags into a `LogsPlan`. Pure: no daemon client, no I/O.
/// Flags: `-n N` / `--lines N` (limit to last N lines), `--follow` / `-f`
/// (stream new entries). No positional arguments are accepted.
fn parse_logs_args(args: &[String]) -> Result<LogsPlan, String> {
    let mut lines = None;
    let mut follow = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-n" | "--lines" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{} requires a line count", args[index]))?;
                lines = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("invalid line count: {value}"))?,
                );
                index += 2;
            }
            "--follow" | "-f" => {
                follow = true;
                index += 1;
            }
            other => return Err(format!("unknown logs option: {other}")),
        }
    }
    Ok(LogsPlan { lines, follow })
}

/// Read the last `limit` lines (or all if `limit` is None) from a log file.
/// Returns lines in chronological order (oldest to newest among the tail).
/// Returns an empty vector if the file does not exist or is empty. Pure I/O
/// (no daemon interaction), so it is testable in isolation.
fn read_log_tail(log_path: &Path, limit: Option<usize>) -> Vec<String> {
    let content = fs::read_to_string(log_path).unwrap_or_default();
    let all_lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    let start = limit
        .map(|n| all_lines.len().saturating_sub(n))
        .unwrap_or(0);
    all_lines[start..].iter().map(|s| s.to_string()).collect()
}

/// The fully-parsed plan for `ctl exec` — every flag resolved up front so the
/// action phase never re-parses. Pure (no I/O, no daemon calls), so it is
/// unit-testable in isolation like `parse_control_options`.
#[derive(Debug)]
struct ExecPlan {
    create_new: bool,
    title: Option<String>,
    all: bool,
    panes_list: Option<String>,
    pane_ref: Option<String>,
    command: String,
}

/// Detect `--help` / `-h` only in the option-parsing window, i.e. tokens BEFORE
/// the first bare `--` separator. Tokens after `--` are the user's freeform
/// command payload and must never be intercepted as ctl flags.
fn has_help_flag(args: &[String]) -> bool {
    for arg in args {
        if arg == "--" {
            return false;
        }
        if arg == "--help" || arg == "-h" {
            return true;
        }
    }
    false
}

/// (07-19 CLI low) Validate a `--panes A,B` list: an explicitly-empty list
/// (`--panes ""`, `--panes ","`) splits to zero targets and would exit 0
/// vacuously — a usage error, not a success. Items are trimmed and empty items
/// dropped by the consumers, so at least one item must name a pane. Shared by
/// `parse_exec_args` and `parse_run_args`.
fn require_named_pane(list: &str) -> Result<(), String> {
    if list.split(',').any(|item| !item.trim().is_empty()) {
        Ok(())
    } else {
        Err("--panes requires at least one pane".to_string())
    }
}

/// Parse `ctl exec` flags into an `ExecPlan`. Pure: no daemon client, no I/O.
/// Flags: `--new`, `--all`, `--pane PANE`, `--panes A,B`, `--name NAME`/`-n NAME`,
/// `--` (end of options); the remaining tokens are the command, joined with spaces.
///
/// Targeting flags are mutually exclusive (the same contract `run` pins in
/// `parse_run_args`): `--all`/`--panes`/`--pane` reject each other, and
/// `--new`/`--name` — which only apply to the single-pane path — are rejected
/// under `--all`/`--panes` rather than silently dropped (07-19 CLI low).
fn parse_exec_args(args: &[String]) -> Result<ExecPlan, String> {
    let mut pane_ref: Option<String> = None;
    let mut create_new = false;
    let mut title: Option<String> = None;
    let mut all = false;
    let mut panes_list: Option<String> = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--new" => {
                create_new = true;
                index += 1;
            }
            "--all" => {
                all = true;
                index += 1;
            }
            "--pane" => {
                pane_ref = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--pane requires a pane".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--panes" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--panes requires a comma-separated list".to_string())?;
                require_named_pane(value)?;
                panes_list = Some(value.clone());
                index += 2;
            }
            "--name" | "-n" => {
                title = Some(
                    args.get(index + 1)
                        .ok_or_else(|| "--name requires a title".to_string())?
                        .clone(),
                );
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown exec option: {value}"));
            }
            _ => break,
        }
    }

    // Reject conflicting targeting combinations (previously `--all` silently
    // overrode `--panes` overrode `--pane`, and `--new`/`--name` were silently
    // dropped under `--all`/`--panes`). Checked after the loop so the error is
    // order-independent.
    if all && panes_list.is_some() {
        return Err("--all cannot be combined with --panes".to_string());
    }
    if all && pane_ref.is_some() {
        return Err("--all cannot be combined with --pane".to_string());
    }
    if panes_list.is_some() && pane_ref.is_some() {
        return Err("--panes cannot be combined with --pane".to_string());
    }
    if (all || panes_list.is_some()) && create_new {
        return Err("--new cannot be combined with --all/--panes".to_string());
    }
    if (all || panes_list.is_some()) && title.is_some() {
        return Err("--name cannot be combined with --all/--panes".to_string());
    }

    if index >= args.len() {
        return Err("exec requires a command".to_string());
    }

    let command = args[index..].join(" ");
    Ok(ExecPlan {
        create_new,
        title,
        all,
        panes_list,
        pane_ref,
        command,
    })
}

fn control_exec(client: &DaemonClient, args: &[String], json_output: bool) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let plan = parse_exec_args(args)?;
    let command = plan.command.clone();
    let line = format!("{command}\r");

    if plan.all {
        let result: Value = client.request(DaemonRequest::Broadcast { input: line })?;
        return report_exec_targets(&command, &result["panes"], json_output);
    }

    if let Some(list) = plan.panes_list {
        let mut targets = Vec::new();
        for pane_ref in list
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let pane_id = resolve_pane_ref(client, pane_ref)?;
            client.request::<CommandOk>(DaemonRequest::SendInput {
                pane_id: pane_id.clone(),
                input: line.clone(),
            })?;
            targets.push(pane_id);
        }
        return report_exec_targets(&command, &json!(targets), json_output);
    }

    let pane = if plan.create_new {
        let inferred_title = plan
            .title
            .or_else(|| command.split_whitespace().next().map(String::from));
        Some(client.request::<Pane>(DaemonRequest::CreatePane {
            title: inferred_title,
            profile: None,
        })?)
    } else {
        None
    };
    let pane_id = if let Some(pane) = pane.as_ref() {
        pane.id.clone()
    } else {
        resolve_pane_ref(client, plan.pane_ref.as_deref().unwrap_or("active"))?
    };

    client.request::<CommandOk>(DaemonRequest::SendInput {
        pane_id: pane_id.clone(),
        input: line,
    })?;

    if json_output {
        write_json_stdout(&json!({ "pane_id": pane_id, "command": command }))
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "sent to {pane_id}: {command}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn report_exec_targets(command: &str, panes: &Value, json_output: bool) -> Result<(), String> {
    if json_output {
        write_json_stdout(&json!({ "command": command, "panes": panes }))
    } else {
        let count = panes.as_array().map(|panes| panes.len()).unwrap_or(0);
        let mut stdout = std::io::stdout();
        writeln!(stdout, "sent to {count} pane(s): {command}")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn control_broadcast(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    // Help only as the FIRST token (L6): later positions are freeform payload.
    if matches!(
        args.first().map(String::as_str),
        Some("--help") | Some("-h")
    ) {
        return print_control_help();
    }
    if args.is_empty() {
        return Err("broadcast requires text".to_string());
    }
    let (literal_lf, args) = parse_lf_flag(args);
    if args.is_empty() {
        return Err("broadcast requires text".to_string());
    }
    let input = decode_cli_text(&args.join(" "), literal_lf);
    let result: Value = client.request(DaemonRequest::Broadcast { input })?;
    let count = result["panes"]
        .as_array()
        .map(|panes| panes.len())
        .unwrap_or(0);
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "broadcast to {count} pane(s)")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn control_sync_input(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    let enabled = match args.first().map(String::as_str) {
        Some("on") | Some("true") | Some("1") => true,
        Some("off") | Some("false") | Some("0") => false,
        _ => return Err("sync requires 'on' or 'off'".to_string()),
    };
    let result: Value = client.request(DaemonRequest::SetSyncInput { enabled })?;
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(
            stdout,
            "synchronize-input {}",
            if enabled { "on" } else { "off" }
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// The fully-parsed plan for `ctl run` — pure (no I/O, no daemon calls), unit-tested
/// like `parse_control_options` and `parse_exec_args`.
///
/// `all` and `panes_list` select the batched (waiting, per-pane exit-code-collecting)
/// variant: `--all` targets every live pane; `--panes A,B` targets a named subset.
/// These are mutually exclusive with each other and with `--pane`.
#[derive(Debug)]
struct RunPlan {
    pane_ref: String,
    /// The command tokens, RAW. Quoting happens only after the target shell's
    /// family is resolved (`quote_command_for`): POSIX and fish have different
    /// single-quote escape rules, and quoting for the wrong family corrupts
    /// backslash-bearing args or unbalances the wrapper entirely (H7).
    command_args: Vec<String>,
    all: bool,
    panes_list: Option<String>,
    /// Overall deadline for the run (`--timeout MS`). None = wait indefinitely
    /// (the historical contract); any marker miss then hangs, so scripts should
    /// pass one.
    timeout_ms: Option<u64>,
}

/// The shell family determines the status-variable idiom used in the `ctl run`
/// wrapper. POSIX shells (sh/bash/zsh/dash) use `$?`; fish uses `$status`.
/// Emitting the wrong idiom causes `ctl run` to hang (the marker never prints)
/// because the variable doesn't exist in the other family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellFamily {
    Posix,
    Fish,
}

/// Detect the shell family from the configured shell path. The check is based
/// on the basename: if it is `fish` (or starts with `fish`, e.g.
/// `/opt/homebrew/bin/fish`), the family is `Fish`; otherwise `Posix`. This is
/// a pure function with no I/O.
fn detect_shell_family(shell: &str) -> ShellFamily {
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell);
    if basename == "fish" || basename.starts_with("fish") {
        ShellFamily::Fish
    } else {
        ShellFamily::Posix
    }
}

/// Build the `ctl run` wrapper line that sends the command followed by a
/// unique marker carrying the shell's exit-status variable. The status
/// variable idiom depends on the shell family: POSIX (sh/bash/zsh) uses
/// `"$?"`, while Fish uses `$status` (fish has no `$?`; using it would hang
/// the pane). The marker is unique (pid + timestamp) so the command's own
/// output cannot cross-match it (VAL-ORCH-007). The `\r` at the end submits
/// the line.
fn build_run_wrapper(command: &str, marker: &str, family: ShellFamily) -> String {
    let status_var = match family {
        ShellFamily::Posix => "\"$?\"",
        ShellFamily::Fish => "$status",
    };
    format!("{command}; printf '\\n{marker}:%s\\n' {status_var}\r")
}

/// Characters that need no quoting in either shell family: letters, digits,
/// and punctuation with no special meaning. Deliberately excludes whitespace,
/// glob chars (*?[]), redirection (<>&|;), expansion ($`), quotes, comment (#),
/// and backslash.
///
/// `=` is safe only MID-word (`FOO=bar` keeps its literal assignment shape):
/// at word start zsh performs equals-expansion (`=foo` expands to the path of
/// the `foo` command), so a leading `=` must force quoting (07-19 CLI low).
fn shell_arg_is_safe(arg: &str) -> bool {
    !arg.is_empty()
        && arg.chars().enumerate().all(|(position, c)| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | ',' | '+')
                || (c == '=' && position > 0)
        })
}

/// Shell-quote a single argument for POSIX shells (sh/bash/zsh). Safe args are
/// returned verbatim; anything else is wrapped in single quotes with embedded
/// single quotes escaped via the standard `'\''` idiom. This preserves the
/// argument's identity when the wrapper line is re-parsed by the pane's shell.
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if shell_arg_is_safe(arg) {
        return arg.to_string();
    }
    let escaped = arg.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Shell-quote a single argument for fish. Inside fish single quotes only `\`
/// and `'` are special, each escaped with a backslash. The POSIX `'\''` idiom
/// is wrong here — fish would interpret `\'` inside the quotes, silently
/// corrupting backslash-bearing args, and an arg ending in `\` would unbalance
/// the wrapper so the exit marker never prints (H7).
fn fish_quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if shell_arg_is_safe(arg) {
        return arg.to_string();
    }
    let escaped = arg.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// Quote one argument for the resolved shell family.
fn shell_quote_for(family: ShellFamily, arg: &str) -> String {
    match family {
        ShellFamily::Posix => shell_quote(arg),
        ShellFamily::Fish => fish_quote(arg),
    }
}

/// Join raw command tokens into the wrapper's command string, quoting each for
/// the resolved shell family so the pane's shell re-parses the user's original
/// argument grouping intact (e.g. `run -- sh -c 'exit 7'` keeps `exit 7` as a
/// single argument to `-c`).
fn quote_command_for(family: ShellFamily, args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote_for(family, arg))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parse `ctl run` flags into a `RunPlan`. Pure: no daemon client, no I/O.
/// Flags: `--pane PANE` (default `active`), `--all`, `--panes A,B`,
/// `--timeout MS` (overall deadline for the run; omitted = wait indefinitely),
/// `--` (end of options); the remaining tokens are the command, kept RAW here —
/// they are quoted for the resolved shell family later (`quote_command_for`),
/// since POSIX and fish quoting rules differ (H7).
///
/// `--all` and `--panes` select the batched variant (waits for each pane and
/// reports per-pane exit codes). They are mutually exclusive with `--pane` and
/// with each other.
fn parse_run_args(args: &[String]) -> Result<RunPlan, String> {
    let mut pane_ref = "active".to_string();
    let mut all = false;
    let mut panes_list: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                if all || panes_list.is_some() {
                    return Err("--pane cannot be combined with --all/--panes".to_string());
                }
                pane_ref = args
                    .get(index + 1)
                    .ok_or_else(|| "--pane requires a pane".to_string())?
                    .clone();
                index += 2;
            }
            "--all" => {
                if panes_list.is_some() || pane_ref != "active" {
                    return Err("--all cannot be combined with --pane/--panes".to_string());
                }
                all = true;
                index += 1;
            }
            "--panes" => {
                if all || pane_ref != "active" {
                    return Err("--panes cannot be combined with --pane/--all".to_string());
                }
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--panes requires a comma-separated list".to_string())?;
                require_named_pane(value)?;
                panes_list = Some(value.clone());
                index += 2;
            }
            "--timeout" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--timeout requires a value".to_string())?;
                timeout_ms = Some(value.parse::<u64>().map_err(|_| {
                    format!("--timeout requires a non-negative integer (ms), got {value:?}")
                })?);
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown run option: {value}"));
            }
            _ => break,
        }
    }
    if index >= args.len() {
        return Err("run requires a command".to_string());
    }
    Ok(RunPlan {
        pane_ref,
        command_args: args[index..].to_vec(),
        all,
        panes_list,
        timeout_ms,
    })
}

/// Cap on captured PTY output retained in a `PaneRunResult` (ENHANCEMENTS §3).
/// Large enough for a useful failure tail; small enough for batched JSON.
const RUN_RESULT_TAIL_BYTES: usize = 4096;

/// The result of running a command in a single pane within a batched run.
/// `exit_code` is `Some(code)` when the marker was captured; `None` means the
/// pane failed before a code could be collected (not live, ended, closed, or
/// the daemon dropped the stream). `success` is true only for exit code 0.
/// `timed_out` is true when the wait hit `--timeout`. `tail` is a bounded
/// capture of PTY output (UTF-8 trimmed) for automation; `elapsed_ms` is wall
/// time from subscribe through resolution.
#[derive(Debug, Clone)]
struct PaneRunResult {
    pane_id: String,
    exit_code: Option<i32>,
    success: bool,
    error: Option<String>,
    timed_out: bool,
    elapsed_ms: u64,
    tail: String,
}

impl PaneRunResult {
    fn ok(pane_id: &str, code: i32, elapsed_ms: u64, tail: String) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            exit_code: Some(code),
            success: code == 0,
            error: None,
            timed_out: false,
            elapsed_ms,
            tail,
        }
    }

    fn failed(pane_id: &str, reason: String, elapsed_ms: u64, tail: String) -> Self {
        let timed_out = reason.starts_with("timed out after ");
        Self {
            pane_id: pane_id.to_string(),
            exit_code: None,
            success: false,
            error: Some(reason),
            timed_out,
            elapsed_ms,
            tail,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "pane": self.pane_id,
            "exit_code": self.exit_code,
            "success": self.success,
            "timed_out": self.timed_out,
            "elapsed_ms": self.elapsed_ms,
            "tail": self.tail,
            "error": self.error,
        })
    }
}

/// Run a command in a single pane and collect its exit code (or failure reason).
/// Subscribes to the event stream, sends the wrapper line, and watches for the
/// marker / `PaneEnded` / `PaneClosed`. Returns a `PaneRunResult`; never hangs
/// past the pane's natural completion (the caller may wrap this in a timeout).
/// The `marker_suffix` disambiguates concurrent per-pane markers in the same
/// workspace (each pane gets a distinct marker so sync-input mirroring or
/// shared output cannot cross-match).
fn run_in_pane(
    client: &DaemonClient,
    pane_id: &str,
    command: &str,
    family: ShellFamily,
    marker_suffix: &str,
    timeout: Option<Duration>,
) -> PaneRunResult {
    let started = Instant::now();
    let elapsed_ms = || started.elapsed().as_millis() as u64;
    let deadline = timeout.map(|t| Instant::now() + t);
    let timed_out = |pane_id: &str, tail: String| {
        PaneRunResult::failed(
            pane_id,
            format!(
                "timed out after {}ms waiting for the exit marker",
                timeout.map(|t| t.as_millis()).unwrap_or_default()
            ),
            elapsed_ms(),
            tail,
        )
    };
    let setup_timed_out = || deadline.map(|d| Instant::now() >= d).unwrap_or(false);
    // Remaining time for a setup RPC. When the run has no overall timeout,
    // use the default client read deadline; otherwise use time left (failing
    // immediately if the deadline already elapsed).
    let setup_budget = || -> Result<Option<Duration>, PaneRunResult> {
        match deadline {
            None => Ok(Some(CLIENT_READ_TIMEOUT)),
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    Err(timed_out(pane_id, String::new()))
                } else {
                    Ok(Some(remaining))
                }
            }
        }
    };
    // Fail fast on a dead pane: otherwise we'd send into a void and wait
    // forever for a marker that never prints.
    let status: PaneStatus = match setup_budget().and_then(|budget| {
        client
            .request_with_timeout(
                DaemonRequest::PaneStatus {
                    pane_id: pane_id.to_string(),
                },
                budget,
            )
            .map_err(|error| {
                if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
                    timed_out(pane_id, String::new())
                } else {
                    PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new())
                }
            })
    }) {
        Ok(s) => s,
        Err(result) => return result,
    };
    if status.state != PaneRuntimeState::Live {
        return PaneRunResult::failed(
            pane_id,
            "pane is not live".to_string(),
            elapsed_ms(),
            String::new(),
        );
    }

    // Subscribe before sending so the marker can't be missed.
    let mut conn = match setup_budget().and_then(|budget| {
        client.connect_with_timeout(budget).map_err(|error| {
            if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
                timed_out(pane_id, String::new())
            } else {
                PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new())
            }
        })
    }) {
        Ok(c) => c,
        Err(result) => return result,
    };
    match setup_budget() {
        Ok(Some(remaining)) => conn.set_read_timeout(Some(remaining)),
        Ok(None) => {}
        Err(result) => return result,
    }
    if let Err(error) = conn.write_request(&DaemonRequest::Subscribe) {
        return PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new());
    }
    // Wait for the registration ack (when the daemon supports it) BEFORE the
    // SendInput below: the marker's output can then never be broadcast before
    // this subscriber joined (M8).
    if let Err(error) = conn.await_subscribe_ack() {
        if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
            return timed_out(pane_id, String::new());
        }
        return PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new());
    }
    // A command's output gaps are unbounded; the run deadline (if any) governs,
    // not the per-read request timeout.
    conn.set_read_timeout(None);

    // pid + timestamp + per-pane suffix: unique per invocation, so command
    // output, old scrollback, or a concurrent run cannot cross-match it (L1).
    let marker = format!(
        "__sgian_rc_{}_{}_{}_{}",
        std::process::id(),
        now_millis(),
        marker_suffix,
        pane_id
    );
    let prefix = format!("{marker}:");
    let wrapped = build_run_wrapper(command, &marker, family);
    if let Err(result) = setup_budget().and_then(|budget| {
        client
            .request_with_timeout::<CommandOk>(
                DaemonRequest::SendInput {
                    pane_id: pane_id.to_string(),
                    input: wrapped,
                },
                budget,
            )
            .map_err(|error| {
                if setup_timed_out() || read_error_is_timeout(deadline, Instant::now()) {
                    timed_out(pane_id, String::new())
                } else {
                    PaneRunResult::failed(pane_id, error, elapsed_ms(), String::new())
                }
            })
            .map(|_| ())
    }) {
        return result;
    }

    // `output` is the marker-search window (kept tiny); `tail` is the bounded
    // capture returned to automation (ENHANCEMENTS §3).
    let mut output = String::new();
    let mut tail = String::new();
    loop {
        // Enforce the overall deadline (H7): bound each read by the remaining
        // time so a marker miss (foreign shell, foreground vim, quoting bug)
        // surfaces as a clean timeout instead of an infinite hang.
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return timed_out(pane_id, tail);
            }
            conn.set_read_timeout(Some(remaining));
        }
        match conn.read_event() {
            Ok(Some(DaemonEvent::PtyOutput { pane_id: id, data })) if id == pane_id => {
                output.push_str(&data);
                tail.push_str(&data);
                trim_to_tail(&mut tail, RUN_RESULT_TAIL_BYTES);
                if let Some(code) = parse_exit_marker(&output, &prefix) {
                    return PaneRunResult::ok(pane_id, code, elapsed_ms(), tail);
                }
                trim_to_tail(&mut output, prefix.len() + 64);
            }
            Ok(Some(DaemonEvent::PaneEnded { pane_id: id, .. })) if id == pane_id => {
                return PaneRunResult::failed(
                    pane_id,
                    "pane ended before the command finished".to_string(),
                    elapsed_ms(),
                    tail,
                );
            }
            Ok(Some(DaemonEvent::PaneClosed { pane_id: id })) if id == pane_id => {
                return PaneRunResult::failed(
                    pane_id,
                    "pane closed before the command finished".to_string(),
                    elapsed_ms(),
                    tail,
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return PaneRunResult::failed(
                    pane_id,
                    "daemon closed before the command finished".to_string(),
                    elapsed_ms(),
                    tail,
                )
            }
            Err(error) => {
                // Check the deadline FIRST: a read error at/after it is the
                // timeout firing, not a transport failure.
                if read_error_is_timeout(deadline, Instant::now()) {
                    return timed_out(pane_id, tail);
                }
                return PaneRunResult::failed(pane_id, error, elapsed_ms(), tail);
            }
        }
    }
}

/// Slack for early-firing read timers when mapping a read error onto the run
/// deadline (`read_error_is_timeout`).
const READ_TIMER_EPSILON: Duration = Duration::from_millis(5);

/// (07-19 CLI low) Should a read error in the `run_in_pane` loop surface as
/// the clean "timed out after Nms" deadline error? The per-read timeout is
/// armed to the time REMAINING to the deadline, but the OS timer can fire a
/// hair EARLY — a strict `now >= deadline` check would then surface a raw IO
/// error sub-milliseconds before the deadline passes. Compare with a small
/// epsilon so the clean timeout always wins in that window. A genuine IO
/// error with the deadline further than the epsilon away is reported raw.
/// Pure for tests.
fn read_error_is_timeout(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| now + READ_TIMER_EPSILON >= deadline)
}

/// Resolve the effective shell family from the daemon's config. Used by both
/// the single-pane and batched `ctl run` paths to pick the status-variable
/// idiom (`$?` vs `$status`).
fn resolve_shell_family(client: &DaemonClient) -> Result<ShellFamily, String> {
    let verbose: VerboseStatus = client.request(DaemonRequest::StatusVerbose)?;
    Ok(detect_shell_family(&status_shell_for_family(
        &verbose.config,
    )))
}

/// Pick the shell to family-detect from a status config payload (M10): prefer
/// the daemon-RESOLVED shell (what the daemon would actually spawn) — a ctl
/// whose environment differs from the daemon's must not re-resolve $SHELL
/// locally and diverge. Old daemons don't emit `resolved_shell`; fall back to
/// the raw configured `shell`, then the local default. Pure for tests.
fn status_shell_for_family(config: &Value) -> String {
    config
        .get("resolved_shell")
        .and_then(Value::as_str)
        .or_else(|| config.get("shell").and_then(Value::as_str))
        .map(ToString::to_string)
        .unwrap_or_else(default_shell)
}

/// Run a command in a pane and report its exit code (best-effort). Wraps the command so
/// the shell prints a unique marker plus the shell's status variable, then
/// watches the pane's output for it. Detects the shell family from the
/// daemon's effective config so non-POSIX shells (fish, which uses `$status`
/// instead of `$?`) report the correct code instead of hanging.
///
/// With `--all` or `--panes A,B`, dispatches to the batched variant that runs
/// the command in multiple panes concurrently, collects each pane's exit code,
/// and reports per-pane results (VAL-ORCH-008..013, 028, 030).
/// A parsed `ctl wait` invocation. `pane_ref` is resolved to a pane id by the caller;
/// `condition` is the single required wait condition; `timeout_ms` bounds the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WaitArgs {
    pane_ref: String,
    condition: WaitCondition,
    timeout_ms: Option<u64>,
}

/// Parse `ctl wait <pane> (--text S | --regex RE | --idle MS | --exit) [--timeout MS]`.
/// Pure: no daemon client, no I/O. Enforces EXACTLY one condition (a missing or
/// conflicting condition is a usage error, never an open-ended wait) and a numeric
/// `--idle`/`--timeout` (VAL-PRIM-016 / VAL-PRIM-017).
///
/// `--` escaping (07-19 CLI low): `--help` is otherwise intercepted as a flag
/// (by `parse_control_options` / `has_help_flag`), so there was no way to match
/// the literal text `--help`. A `--` directly after a value-taking flag escapes
/// that flag's value (`--text -- --help` matches the literal text); a
/// standalone `--` ends flag recognition so a later token is a literal pane
/// reference even when it starts with `-`.
fn parse_wait_args(args: &[String]) -> Result<WaitArgs, String> {
    fn set_condition(slot: &mut Option<WaitCondition>, cond: WaitCondition) -> Result<(), String> {
        if slot.is_some() {
            return Err("wait accepts only one of --text/--regex/--idle/--exit".to_string());
        }
        *slot = Some(cond);
        Ok(())
    }

    let mut pane_ref: Option<String> = None;
    let mut condition: Option<WaitCondition> = None;
    let mut timeout_ms: Option<u64> = None;
    let mut index = 0;
    let mut literal = false;

    while index < args.len() {
        let arg = args[index].as_str();
        if literal {
            // After a standalone `--`: only the pane reference is still open.
            if pane_ref.is_some() {
                return Err(format!(
                    "wait accepts a single pane reference; unexpected argument: {arg}"
                ));
            }
            pane_ref = Some(arg.to_string());
            index += 1;
            continue;
        }
        match arg {
            "--" => {
                literal = true;
                index += 1;
            }
            "--text" | "--regex" | "--idle" | "--timeout" => {
                // A `--` in the value slot escapes the NEXT token as the value.
                let value_index = if args.get(index + 1).map(String::as_str) == Some("--") {
                    index + 2
                } else {
                    index + 1
                };
                let value = args
                    .get(value_index)
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                match arg {
                    "--text" => set_condition(&mut condition, WaitCondition::Text(value.clone()))?,
                    "--regex" => {
                        set_condition(&mut condition, WaitCondition::Regex(value.clone()))?
                    }
                    "--idle" => {
                        let ms = value.parse::<u64>().map_err(|_| {
                            format!("--idle requires a non-negative integer (ms), got {value:?}")
                        })?;
                        set_condition(&mut condition, WaitCondition::Idle(ms))?;
                    }
                    "--timeout" => {
                        timeout_ms = Some(value.parse::<u64>().map_err(|_| {
                            format!("--timeout requires a non-negative integer (ms), got {value:?}")
                        })?);
                    }
                    _ => unreachable!(),
                }
                index = value_index + 1;
            }
            "--exit" => {
                set_condition(&mut condition, WaitCondition::Exit)?;
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown wait option: {other}"));
            }
            other => {
                if pane_ref.is_some() {
                    return Err(format!(
                        "wait accepts a single pane reference; unexpected argument: {other}"
                    ));
                }
                pane_ref = Some(other.to_string());
                index += 1;
            }
        }
    }

    let pane_ref = pane_ref.ok_or_else(|| "wait requires a pane id".to_string())?;
    let condition =
        condition.ok_or_else(|| "wait requires one of --text/--regex/--idle/--exit".to_string())?;
    Ok(WaitArgs {
        pane_ref,
        condition,
        timeout_ms,
    })
}

/// `ctl wait <pane> (--text S | --regex RE | --idle MS | --exit) [--timeout MS] [--json]`
/// — block until the condition is met or the timeout elapses. Prints the result
/// (`--json` → the documented `{matched,reason,revision,exit_code?,elapsed_ms}` object;
/// otherwise a short human summary) and maps the outcome to the process exit code: a
/// match exits 0, a timeout exits non-zero, so scripts can branch on `$?`
/// (VAL-PRIM-012 / VAL-PRIM-013) — mirroring `ctl run`'s exit-code mapping.
fn control_wait(client: &DaemonClient, args: &[String], json_output: bool) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_wait_args(args)?;
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    // A wait's response legitimately takes as long as the wait itself: scale the
    // read deadline to the wait's own timeout (plus the normal request margin),
    // or clear it entirely for an untimed wait (the documented block-forever
    // contract) — the default CLIENT_READ_TIMEOUT would cut long waits short.
    let mut conn = client.connect()?;
    conn.set_read_timeout(
        parsed
            .timeout_ms
            .map(|ms| Duration::from_millis(ms) + CLIENT_READ_TIMEOUT),
    );
    let response = conn.request(&DaemonRequest::Wait {
        pane_id,
        condition: parsed.condition,
        timeout_ms: parsed.timeout_ms,
    })?;
    if !response.ok {
        return Err(response
            .error
            .unwrap_or_else(|| "daemon request failed".to_string()));
    }
    let outcome: Value = response.result;

    let matched = outcome
        .get("matched")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = outcome
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if json_output {
        write_json_stdout(&outcome)?;
    } else {
        let mut stdout = std::io::stdout();
        let line = if matched {
            match reason.as_str() {
                "exit" => match outcome.get("exit_code").and_then(Value::as_i64) {
                    Some(code) => format!("matched: exit (code {code})"),
                    None => "matched: exit".to_string(),
                },
                other => format!("matched: {other}"),
            }
        } else {
            "timeout".to_string()
        };
        writeln!(stdout, "{line}").map_err(|error| format!("failed to write stdout: {error}"))?;
    }

    // Scriptable process exit: a match exits 0, a timeout exits non-zero (VAL-PRIM-013).
    if matched {
        Ok(())
    } else {
        Err(format!("wait did not match (reason: {reason})"))
    }
}

/// A parsed `ctl snapshot` invocation: just the pane reference (the global `--json`
/// flag is consumed by `parse_control_options` before the args reach here).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotArgs {
    pane_ref: String,
}

/// Parse `ctl snapshot <pane>`. Pure: no daemon client, no I/O. Requires exactly one
/// pane reference; a missing pane, an extra positional, or an unknown option is a usage
/// error. A standalone `--` ends flag recognition so a pane titled like a flag
/// stays addressable (`snapshot -- --titled`) (07-19 CLI low).
fn parse_snapshot_args(args: &[String]) -> Result<SnapshotArgs, String> {
    let mut pane_ref: Option<String> = None;
    let mut literal = false;
    for arg in args {
        match arg.as_str() {
            other if literal => {
                if pane_ref.is_some() {
                    return Err(format!(
                        "snapshot accepts a single pane reference; unexpected argument: {other}"
                    ));
                }
                pane_ref = Some(other.to_string());
            }
            "--" => literal = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown snapshot option: {other}"));
            }
            other => {
                if pane_ref.is_some() {
                    return Err(format!(
                        "snapshot accepts a single pane reference; unexpected argument: {other}"
                    ));
                }
                pane_ref = Some(other.to_string());
            }
        }
    }
    let pane_ref = pane_ref.ok_or_else(|| "snapshot requires a pane id".to_string())?;
    Ok(SnapshotArgs { pane_ref })
}

/// A parsed `ctl find` invocation: the optional metadata filters (the global `--json`
/// flag is consumed by `parse_control_options` before the args reach here).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FindArgs {
    command: Option<String>,
    title: Option<String>,
    cwd: Option<String>,
    state: Option<PaneRuntimeState>,
}

/// Parse `ctl find [--command S] [--title S] [--cwd S] [--state live|ended]`. Pure: no
/// daemon client, no I/O. Every filter is optional (a bare `find` matches all panes);
/// each value-taking flag requires its value; `--state` accepts only `live`/`ended`
/// (any other value is a usage error — VAL-PRIM-039); positionals and unknown flags are
/// usage errors.
///
/// `--` escaping (07-19 CLI low), same shape as `parse_wait_args`: a `--`
/// directly after a value-taking flag escapes that flag's value
/// (`--title -- --help` filters on the literal title "--help"); a standalone
/// `--` ends flag recognition (find takes no positionals, so later tokens are
/// usage errors either way).
fn parse_find_args(args: &[String]) -> Result<FindArgs, String> {
    let mut parsed = FindArgs::default();
    let mut literal = false;
    let mut index = 0;
    // Value for a flag at `flag_index`: a `--` in the value slot escapes the
    // NEXT token as the literal value. Returns (value, next_index).
    fn flag_value<'a>(
        args: &'a [String],
        flag_index: usize,
        flag: &str,
    ) -> Result<(&'a str, usize), String> {
        let value_index = if args.get(flag_index + 1).map(String::as_str) == Some("--") {
            flag_index + 2
        } else {
            flag_index + 1
        };
        args.get(value_index)
            .map(|value| (value.as_str(), value_index + 1))
            .ok_or_else(|| format!("{flag} requires a value"))
    }

    while index < args.len() {
        let arg = args[index].as_str();
        if literal {
            return Err(format!("find takes no positional arguments; got: {arg}"));
        }
        match arg {
            "--" => {
                literal = true;
                index += 1;
            }
            "--command" => {
                let (value, next) = flag_value(args, index, "--command")?;
                parsed.command = Some(value.to_string());
                index = next;
            }
            "--title" => {
                let (value, next) = flag_value(args, index, "--title")?;
                parsed.title = Some(value.to_string());
                index = next;
            }
            "--cwd" => {
                let (value, next) = flag_value(args, index, "--cwd")?;
                parsed.cwd = Some(value.to_string());
                index = next;
            }
            "--state" => {
                let (value, next) = flag_value(args, index, "--state")?;
                let state = match value {
                    "live" => PaneRuntimeState::Live,
                    "ended" => PaneRuntimeState::Ended,
                    other => {
                        return Err(format!(
                            "invalid --state '{other}': must be 'live' or 'ended'"
                        ));
                    }
                };
                parsed.state = Some(state);
                index = next;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown find option: {other}"));
            }
            other => {
                return Err(format!("find takes no positional arguments; got: {other}"));
            }
        }
    }
    Ok(parsed)
}

/// `ctl snapshot <pane> [--json]` — read a pane's rendered visible screen. With
/// `--json`, prints the documented snapshot struct (architecture §6.3); otherwise
/// prints the screen, one line per visible row, to stdout (VAL-PRIM-025). Reads an
/// existing pane only (never spawns a daemon).
fn control_snapshot(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_snapshot_args(args)?;
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let snapshot: Value = client.request(DaemonRequest::Snapshot { pane_id })?;

    if json_output {
        write_json_stdout(&snapshot)?;
    } else {
        let mut stdout = std::io::stdout();
        if let Some(lines) = snapshot.get("lines").and_then(Value::as_array) {
            for line in lines {
                writeln!(stdout, "{}", line.as_str().unwrap_or(""))
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
        }
    }
    Ok(())
}

/// `ctl find [--command S] [--title S] [--cwd S] [--state live|ended] [--json]` — query
/// the workspace's panes by metadata. Filters AND together and are substring matches; no
/// filters returns every pane (live and ended); no matches returns an empty set (exit 0,
/// not an error). With `--json`, prints the array of per-pane metadata; otherwise prints
/// a compact human line per matched pane. Reads existing panes only (never spawns a
/// daemon). (Architecture §6.3; VAL-PRIM-028..039/052/054.)
fn control_find(client: &DaemonClient, args: &[String], json_output: bool) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_find_args(args)?;
    let result: Value = client.request(DaemonRequest::Find {
        command: parsed.command,
        title: parsed.title,
        cwd: parsed.cwd,
        state: parsed.state,
    })?;

    if json_output {
        write_json_stdout(&result)?;
    } else {
        let mut stdout = std::io::stdout();
        if let Some(entries) = result.as_array() {
            for entry in entries {
                let id = entry["id"].as_str().unwrap_or("");
                let state = entry["state"].as_str().unwrap_or("");
                let title = entry["title"].as_str().unwrap_or("");
                let command = entry["command"].as_str().unwrap_or("");
                writeln!(stdout, "{id}\t{state}\t{title}\t{command}")
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
        }
    }
    Ok(())
}

/// (T1) Parsed `ctl agent` arguments: the pane reference (defaults to the
/// active pane, like every PANE-taking command) and the requested operation.
#[derive(Debug)]
struct AgentArgs {
    pane_ref: String,
    /// `None`: query. `Some(Some("claude"))`: mark. `Some(None)`: unmark.
    mark: Option<Option<String>>,
    /// `--watch`: stream agent-state, lease and pane-end transitions
    /// (docs/design/keyboard-lease-and-ledger.md §6 M3). With no PANE the
    /// stream covers every pane; with one it ends when that pane closes.
    watch: bool,
    /// Whether a PANE was given (a bare `--watch` means every pane).
    pane_given: bool,
}

/// (T1) `ctl agent [PANE] [on|off]` — `on`/`off` as the FIRST positional is
/// the operation on the ACTIVE pane (L9: `ctl agent on` ≡ `ctl agent active
/// on`); any other first positional is the PANE reference and the optional
/// second positional is the operation.
fn parse_agent_args(args: &[String]) -> Result<AgentArgs, String> {
    let watch = args.iter().any(|arg| arg == "--watch");
    let args: Vec<String> = args
        .iter()
        .filter(|arg| *arg != "--watch")
        .cloned()
        .collect();
    let args = args.as_slice();
    let verb_first = matches!(args.first().map(String::as_str), Some("on" | "off"));
    let (pane_ref, op_index) = if verb_first {
        ("active".to_string(), 0)
    } else {
        (
            args.first()
                .cloned()
                .unwrap_or_else(|| "active".to_string()),
            1,
        )
    };
    let mark = match args.get(op_index).map(String::as_str) {
        None => None,
        Some("on") => Some(Some("claude".to_string())),
        Some("off") => Some(None),
        Some(other) => return Err(format!("unexpected argument for agent: {other}")),
    };
    if let Some(extra) = args.get(op_index + 1) {
        return Err(format!("unexpected argument for agent: {extra}"));
    }
    if watch && mark.is_some() {
        return Err("--watch cannot be combined with on/off".to_string());
    }
    let pane_given = !verb_first && !args.is_empty();
    Ok(AgentArgs {
        pane_ref,
        mark,
        watch,
        pane_given,
    })
}

/// One line for `ctl agent --watch`: the agent-state, lease and pane-end
/// transitions a script wants to react to; `None` for everything else.
/// JSON is the daemon's own event shape (`{"event":"agent_state",...}`).
fn format_watch_event(event: &DaemonEvent, json_output: bool) -> Option<String> {
    fn enum_name<T: Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "-".to_string())
    }
    let text = match event {
        DaemonEvent::AgentState {
            pane_id,
            agent,
            attention,
            mode,
        } => format!(
            "{pane_id}\tagent_state\t{}\t{}\t{}",
            agent.as_deref().unwrap_or("-"),
            attention
                .map(|value| enum_name(&value))
                .unwrap_or_else(|| "-".to_string()),
            mode.as_deref().unwrap_or("-")
        ),
        DaemonEvent::LeaseState {
            pane_id,
            transition,
            holder,
            ..
        } => format!(
            "{pane_id}\tlease_{}\t{}",
            enum_name(transition),
            holder.as_deref().unwrap_or("-")
        ),
        DaemonEvent::PaneEnded { pane_id, exit_code } => format!(
            "{pane_id}\tpane_ended\t{}",
            exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_string())
        ),
        DaemonEvent::PaneClosed { pane_id } => format!("{pane_id}\tpane_closed"),
        DaemonEvent::OutputWarning { pane_id, total, .. } => format!(
            "{pane_id}\toutput_warning{}",
            format_output_warning(&serde_json::to_value(total).unwrap_or(Value::Null))
        ),
        DaemonEvent::AgentUsage { pane_id, usage } => format!(
            "{pane_id}\tagent_usage\t{}",
            usage.summary(now_millis() / 1000)
        ),
        _ => return None,
    };
    if json_output {
        serde_json::to_string(event).ok()
    } else {
        Some(text)
    }
}

/// `\tHIDDEN-OUTPUT conceal=2 clipboard=1` for a non-empty counter object, else "".
fn format_output_warning(tricks: &Value) -> String {
    let Some(map) = tricks.as_object() else {
        return String::new();
    };
    let parts: Vec<String> = map
        .iter()
        .filter(|(_, count)| count.as_u64().unwrap_or(0) > 0)
        .map(|(kind, count)| format!("{kind}={}", count.as_u64().unwrap_or(0)))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("\tHIDDEN-OUTPUT {}", parts.join(" "))
    }
}

fn watch_event_pane(event: &DaemonEvent) -> Option<&str> {
    match event {
        DaemonEvent::OutputWarning { pane_id, .. }
        | DaemonEvent::AgentUsage { pane_id, .. }
        | DaemonEvent::AgentState { pane_id, .. }
        | DaemonEvent::LeaseState { pane_id, .. }
        | DaemonEvent::PaneEnded { pane_id, .. }
        | DaemonEvent::PaneClosed { pane_id } => Some(pane_id),
        _ => None,
    }
}

/// `ctl agent --watch [PANE]` — print the current agent state as a baseline,
/// then stream transitions until killed (or, with a PANE, until it closes).
fn control_agent_watch(
    client: &DaemonClient,
    pane_filter: Option<String>,
    json_output: bool,
) -> Result<(), String> {
    // Subscribe first so a transition between the baseline read and the loop
    // is queued rather than missed.
    let mut conn = client.connect()?;
    conn.write_request(&DaemonRequest::Subscribe)?;
    conn.await_subscribe_ack()?;
    conn.set_read_timeout(None);

    let entries: Value = client.request(DaemonRequest::Find {
        command: None,
        title: None,
        cwd: None,
        state: None,
    })?;
    let mut stdout = std::io::stdout();
    for entry in entries.as_array().into_iter().flatten() {
        let Some(pane_id) = entry["id"].as_str() else {
            continue;
        };
        if pane_filter
            .as_deref()
            .is_some_and(|wanted| wanted != pane_id)
        {
            continue;
        }
        let baseline = DaemonEvent::AgentState {
            pane_id: pane_id.to_string(),
            agent: entry["agent"].as_str().map(str::to_string),
            attention: serde_json::from_value(entry["attention"].clone()).ok(),
            mode: entry["mode"].as_str().map(str::to_string),
        };
        if let Some(line) = format_watch_event(&baseline, json_output) {
            writeln!(stdout, "{line}")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
    }
    stdout
        .flush()
        .map_err(|error| format!("failed to write stdout: {error}"))?;

    loop {
        let Some(event) = conn.read_event()? else {
            return Ok(());
        };
        let Some(pane_id) = watch_event_pane(&event) else {
            continue;
        };
        if pane_filter
            .as_deref()
            .is_some_and(|wanted| wanted != pane_id)
        {
            continue;
        }
        if let Some(line) = format_watch_event(&event, json_output) {
            writeln!(stdout, "{line}")
                .and_then(|_| stdout.flush())
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        if pane_filter.is_some() && matches!(event, DaemonEvent::PaneClosed { .. }) {
            return Ok(());
        }
    }
}

/// (T1) Read a pane's agent state via `find` (which — unlike `snapshot` —
/// answers for panes without a live screen model, e.g. restored-never-spawned
/// panes) and shape it like the SetPaneAgent response.
fn query_agent_state(client: &DaemonClient, pane_id: &str) -> Result<Value, String> {
    let result: Value = client.request(DaemonRequest::Find {
        command: None,
        title: None,
        cwd: None,
        state: None,
    })?;
    let entry = result.as_array().and_then(|entries| {
        entries
            .iter()
            .find(|entry| entry["id"].as_str() == Some(pane_id))
    });
    Ok(json!({
        "pane_id": pane_id,
        "agent": entry.and_then(|entry| entry.get("agent")).cloned().unwrap_or(Value::Null),
        "attention": entry.and_then(|entry| entry.get("attention")).cloned().unwrap_or(Value::Null),
        "mode": entry.and_then(|entry| entry.get("mode")).cloned().unwrap_or(Value::Null),
        "unattended": entry
            .and_then(|entry| entry.get("unattended"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "output_warnings": entry
            .and_then(|entry| entry.get("output_warnings"))
            .cloned()
            .unwrap_or(Value::Null),
        "usage": entry
            .and_then(|entry| entry.get("usage"))
            .cloned()
            .unwrap_or(Value::Null),
    }))
}

/// (T1) `ctl agent [PANE] [on|off]` — print a pane's agent + attention state,
/// or override it: `on` marks the pane as running Claude Code (persisted),
/// `off` clears the mark (auto-detection resumes). The current state is
/// printed after a set, consistent with other mutating commands.
fn control_agent(client: &DaemonClient, args: &[String], json_output: bool) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let parsed = parse_agent_args(args)?;
    if parsed.watch {
        let filter = if parsed.pane_given {
            Some(resolve_pane_ref(client, &parsed.pane_ref)?)
        } else {
            None
        };
        return control_agent_watch(client, filter, json_output);
    }
    let pane_id = resolve_pane_ref(client, &parsed.pane_ref)?;
    let state = match parsed.mark {
        Some(agent) => client.request::<Value>(DaemonRequest::SetPaneAgent {
            pane_id: pane_id.clone(),
            agent,
        })?,
        None => query_agent_state(client, &pane_id)?,
    };

    if json_output {
        write_json_stdout(&state)
    } else {
        let mut stdout = std::io::stdout();
        let suffix = format!(
            "{}{}",
            if state["unattended"].as_bool().unwrap_or(false) {
                "\tUNATTENDED"
            } else {
                ""
            },
            format_output_warning(&state["output_warnings"])
        );
        let usage_text = serde_json::from_value::<AgentUsage>(state["usage"].clone())
            .ok()
            .filter(|usage| usage.updated_at_ms > 0)
            .map(|usage| format!("\t{}", usage.summary(now_millis() / 1000)))
            .unwrap_or_default();
        writeln!(
            stdout,
            "{}\t{}\t{}\t{}{}{}",
            pane_id,
            state["agent"].as_str().unwrap_or("-"),
            state["attention"].as_str().unwrap_or("-"),
            state["mode"].as_str().unwrap_or("-"),
            suffix,
            usage_text
        )
        .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

fn control_run(client: &DaemonClient, args: &[String], json_output: bool) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let plan = parse_run_args(args)?;

    if plan.all || plan.panes_list.is_some() {
        return control_run_batched(client, &plan, json_output);
    }

    let pane_id = resolve_pane_ref(client, &plan.pane_ref)?;
    let family = resolve_shell_family(client)?;
    // Quote only now that the target shell's family is known (H7).
    let command = quote_command_for(family, &plan.command_args);
    let timeout = plan.timeout_ms.map(Duration::from_millis);
    let result = run_in_pane(client, &pane_id, &command, family, "single", timeout);

    if json_output {
        // Prefer the durable per-pane object (same shape as batched) while
        // keeping `pane_id` for older consumers of the single-pane form.
        let mut payload = result.to_json();
        if let Some(object) = payload.as_object_mut() {
            object.insert("pane_id".to_string(), json!(pane_id));
        }
        write_json_stdout(&payload)?;
    } else if let Some(code) = result.exit_code {
        writeln!(std::io::stdout(), "exit {code}")
            .map_err(|error| format!("failed to write stdout: {error}"))?;
    }

    match result.exit_code {
        Some(0) => Ok(()),
        Some(code) => {
            // Mirror the command's own exit code as the CLI's process exit (L2),
            // clamped into 1..=255 so a failure can never read as 0 and >255
            // can't wrap. Failures without a captured code (timeout, dead pane)
            // keep the default exit 1.
            CLI_EXIT_CODE.store(code.clamp(1, 255), Ordering::SeqCst);
            Err(format!("command exited with code {code}"))
        }
        None => Err(result.error.unwrap_or_else(|| "run failed".to_string())),
    }
}

/// The batched (waiting, per-pane exit-code-collecting) variant of `ctl run`.
/// Runs the command in every live pane (`--all`) or a named subset
/// (`--panes A,B`) concurrently, collects each pane's exit code, and reports
/// per-pane results. The aggregate process exit is 0 only when every targeted
/// pane succeeded (VAL-ORCH-013). A not-live / dying pane is reported as a
/// failure for that pane without hanging or losing other panes' results
/// (VAL-ORCH-028). Zero live panes yields an empty result set and exit 0
/// (VAL-ORCH-030).
fn control_run_batched(
    client: &DaemonClient,
    plan: &RunPlan,
    json_output: bool,
) -> Result<(), String> {
    let results = collect_batched_results(client, plan)?;

    // Zero live panes: empty result set, vacuously all-success (VAL-ORCH-030).
    if results.is_empty() {
        if json_output {
            write_json_stdout(&json!([]))?;
        }
        return Ok(());
    }

    let any_failed = results.iter().any(|result| !result.success);

    if json_output {
        let json_results: Vec<Value> = results.iter().map(PaneRunResult::to_json).collect();
        write_json_stdout(&json!(json_results))?;
    } else {
        let mut stdout = std::io::stdout();
        for result in &results {
            if let Some(code) = result.exit_code {
                writeln!(
                    stdout,
                    "{} exit {code} ({}ms)",
                    result.pane_id, result.elapsed_ms
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            } else {
                let kind = if result.timed_out {
                    "timeout"
                } else {
                    "failed"
                };
                writeln!(
                    stdout,
                    "{} {kind}: {} ({}ms)",
                    result.pane_id,
                    result.error.as_deref().unwrap_or("unknown"),
                    result.elapsed_ms
                )
                .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
        }
    }

    if any_failed {
        let failures = results.iter().filter(|r| !r.success).count();
        Err(format!("{failures} pane(s) failed"))
    } else {
        Ok(())
    }
}

/// Resolve the target pane set for a batched run and execute the command in
/// each pane concurrently, returning a sorted `Vec<PaneRunResult>`. This is
/// the testable core of `control_run_batched` (no stdout I/O). Returns an
/// error only for resolution failures (e.g. an unknown pane ref in
/// `--panes`); per-pane failures are represented in the result vector, not as
/// an error.
fn collect_batched_results(
    client: &DaemonClient,
    plan: &RunPlan,
) -> Result<Vec<PaneRunResult>, String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;

    // Resolve the target pane set.
    // --all: every LIVE pane (ended panes are excluded, not reported).
    // --panes A,B: the named panes (resolved via match_pane_ref; unknown refs
    //   fail fast with "pane not found" per VAL-ORCH-025). Named panes that
    //   turn out to be Ended are targeted and reported as failures (VAL-ORCH-028).
    let targets: Vec<String> = if plan.all {
        list.panes
            .iter()
            .filter(|status| status.state == PaneRuntimeState::Live)
            .map(|status| status.pane.id.clone())
            .collect()
    } else {
        let list_str = plan
            .panes_list
            .as_ref()
            .ok_or_else(|| "batched run requires --all or --panes".to_string())?;
        let mut ids = Vec::new();
        for pane_ref in list_str
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
        {
            let id = match_pane_ref(&list, pane_ref)?;
            ids.push(id);
        }
        ids
    };

    let family = resolve_shell_family(client)?;

    if targets.is_empty() {
        return Ok(Vec::new());
    }

    // Run each pane concurrently in scoped threads so a slow/dying pane does
    // not block the others (VAL-ORCH-028). Each thread opens its own
    // subscriber stream and watches only its pane's output.
    // Quote only now that the target shell's family is known (H7).
    let command = quote_command_for(family, &plan.command_args);
    let command = command.as_str();
    let timeout = plan.timeout_ms.map(Duration::from_millis);
    let mut results: Vec<PaneRunResult> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(idx, pane_id)| {
                let pane_id = pane_id.as_str();
                // Distinct suffix per pane so markers don't cross-match even
                // under sync-input mirroring.
                let suffix = format!("b{idx}");
                scope.spawn(move || run_in_pane(client, pane_id, command, family, &suffix, timeout))
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    PaneRunResult::failed("?", "run thread panicked".to_string(), 0, String::new())
                })
            })
            .collect()
    });

    // Sort by pane id for deterministic output.
    results.sort_by(|left, right| left.pane_id.cmp(&right.pane_id));
    Ok(results)
}

/// Trim `buffer` from the front so at most `keep` bytes remain, respecting UTF-8
/// character boundaries.
fn trim_to_tail(buffer: &mut String, keep: usize) {
    if buffer.len() <= keep {
        return;
    }
    let mut start = buffer.len() - keep;
    while !buffer.is_char_boundary(start) {
        start += 1;
    }
    buffer.drain(..start);
}

/// Find `prefix` followed by digits in `buffer` (the command's output), ignoring the
/// echoed command line (which contains the literal `%s`, not digits). Digits that run
/// to the end of the buffer are not accepted: the rest of the code may still be in
/// flight (the marker line always ends with a newline), so wait for the terminator.
fn parse_exit_marker(buffer: &str, prefix: &str) -> Option<i32> {
    let mut search_from = 0;
    while let Some(found) = buffer[search_from..].find(prefix) {
        let start = search_from + found + prefix.len();
        let digits: String = buffer[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if !digits.is_empty() && start + digits.len() < buffer.len() {
            if let Ok(code) = digits.parse::<i32>() {
                return Some(code);
            }
        }
        search_from = start.max(search_from + 1);
    }
    None
}

/// `ctl logs` — tail the structured daemon log. Read-only: requires a running
/// daemon (the caller uses `connect_existing`), never spawns one.
///
/// Options (documented in `ctl help`):
///   -n, --lines N   Print the last N lines (default: all)
///   -f, --follow    Stream new entries as they are written (blocks until killed)
fn control_logs(client: &DaemonClient, args: &[String]) -> Result<(), String> {
    let plan = parse_logs_args(args)?;
    let log_path = client.data_dir.join(LOG_FILE);

    // Print the tail (last N lines or all) of the current log file.
    let tail = read_log_tail(&log_path, plan.lines);
    let mut stdout = std::io::stdout();
    for line in &tail {
        writeln!(stdout, "{line}").map_err(|error| format!("failed to write stdout: {error}"))?;
    }
    stdout
        .flush()
        .map_err(|error| format!("failed to write stdout: {error}"))?;

    if plan.follow {
        follow_log_file(&log_path)?;
    }

    Ok(())
}

/// Whether two metadata handles refer to the same underlying file. Unix compares
/// (device, inode); the Windows fallback says "same" (rotation-follow is
/// best-effort there, matching the transport's compiles-only bar).
#[cfg(unix)]
fn is_same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}
#[cfg(windows)]
fn is_same_file(_a: &fs::Metadata, _b: &fs::Metadata) -> bool {
    true
}

/// Stream new log entries as they are written to the file. Seeks to the current
/// end of the file, then polls for new content every 100ms, printing each new
/// line to stdout. Blocks until the process is killed (e.g. Ctrl+C).
///
/// Rotation-aware (M10): `BoundedFileWriter` rotates by renaming the active log
/// aside and creating a fresh file at the same path. The follower tracks the
/// PATH — when the path's identity changes (or the file shrinks), it reopens
/// from the start of the new file instead of reading the renamed inode's silence
/// forever.
fn follow_log_file(log_path: &Path) -> Result<(), String> {
    let file = File::open(log_path).map_err(|error| format!("failed to open log file: {error}"))?;
    let mut reader = BufReader::new(file);
    // Seek to end so only new entries (written after `ctl logs` started) are
    // streamed. The initial tail above already printed existing content.
    let mut position = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("failed to seek log file: {error}"))?;

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // No new content: check for rotation before waiting. A missing
                // path (mid-rotation window) just retries next tick.
                let rotated = match (fs::metadata(log_path), reader.get_ref().metadata()) {
                    (Ok(path_meta), Ok(open_meta)) => {
                        !is_same_file(&path_meta, &open_meta) || path_meta.len() < position
                    }
                    _ => false,
                };
                if rotated {
                    if let Ok(new_file) = File::open(log_path) {
                        reader = BufReader::new(new_file);
                        position = 0;
                        continue;
                    }
                }
                thread::sleep(Duration::from_millis(100));
            }
            Ok(read) => {
                position += read as u64;
                let mut stdout = std::io::stdout();
                stdout
                    .write_all(line.as_bytes())
                    .and_then(|_| stdout.flush())
                    .map_err(|error| format!("failed to write stdout: {error}"))?;
            }
            Err(error) => return Err(format!("failed to read log file: {error}")),
        }
    }
}

fn control_shutdown(client: &DaemonClient, json_output: bool) -> Result<(), String> {
    let result: CommandOk = client.request(DaemonRequest::Shutdown)?;
    if json_output {
        write_json_stdout(&result)
    } else {
        let mut stdout = std::io::stdout();
        writeln!(stdout, "daemon stopped")
            .map_err(|error| format!("failed to write stdout: {error}"))
    }
}

/// Non-PTY argv runner (ENHANCEMENTS §3): `ctl process [--cwd DIR] [--timeout MS] -- <argv...>`.
fn control_process(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    let mut cwd = None;
    let mut timeout_ms = None;
    let mut argv = Vec::new();
    let mut index = 0;
    let mut passthrough = false;
    while index < args.len() {
        if passthrough {
            argv.push(args[index].clone());
            index += 1;
            continue;
        }
        match args[index].as_str() {
            "--" => {
                passthrough = true;
                index += 1;
            }
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--cwd requires a path".to_string())?;
                cwd = Some(value.clone());
                index += 2;
            }
            "--timeout" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--timeout requires milliseconds".to_string())?;
                timeout_ms = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid --timeout value: {value}"))?,
                );
                index += 2;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected process option: {other}"));
            }
            _ => {
                // Allow `ctl process echo hi` without `--`.
                argv.push(args[index].clone());
                index += 1;
            }
        }
    }
    if argv.is_empty() {
        return Err("process requires a command (try: process -- echo hi)".to_string());
    }
    let result: Value = client.request(DaemonRequest::RunProcess {
        argv,
        cwd,
        timeout_ms,
    })?;
    if json_output {
        write_json_stdout(&result)?;
    } else {
        let code = result.get("exit_code");
        let timed_out = result
            .get("timed_out")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let elapsed = result
            .get("elapsed_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if timed_out {
            writeln!(std::io::stdout(), "timeout ({elapsed}ms)")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        } else if let Some(code) = code.and_then(|v| v.as_i64()) {
            writeln!(std::io::stdout(), "exit {code} ({elapsed}ms)")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        } else {
            writeln!(std::io::stdout(), "failed ({elapsed}ms)")
                .map_err(|error| format!("failed to write stdout: {error}"))?;
        }
        if let Some(stdout) = result.get("stdout").and_then(|v| v.as_str()) {
            if !stdout.is_empty() {
                print!("{stdout}");
                if !stdout.ends_with('\n') {
                    println!();
                }
            }
        }
        if let Some(stderr) = result.get("stderr").and_then(|v| v.as_str()) {
            if !stderr.is_empty() && stderr != "timed out" {
                eprint!("{stderr}");
                if !stderr.ends_with('\n') {
                    eprintln!();
                }
            }
        }
    }
    let success = result
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if success {
        Ok(())
    } else if result
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        Err("process timed out".to_string())
    } else if let Some(code) = result.get("exit_code").and_then(|v| v.as_i64()) {
        CLI_EXIT_CODE.store((code as i32).clamp(1, 255), Ordering::SeqCst);
        Err(format!("process exited with code {code}"))
    } else {
        Err("process failed".to_string())
    }
}

/// Privacy-safe diagnostic bundle for support (ENHANCEMENTS §3).
/// Omits prompts, scrollback, tokens, env values, and agent message bodies.
fn control_diagnostic(
    client: &DaemonClient,
    args: &[String],
    json_output: bool,
) -> Result<(), String> {
    if has_help_flag(args) {
        return print_control_help();
    }
    if let Some(extra) = args.first() {
        return Err(format!("unexpected argument for diagnostic: {extra}"));
    }
    let status: VerboseStatus = client.request(DaemonRequest::StatusVerbose)?;
    let panes: PaneList = client.request(DaemonRequest::ListPanes)?;
    let found: Value = client.request(DaemonRequest::Find {
        command: None,
        title: None,
        cwd: None,
        state: None,
    })?;
    let log_path = client.data_dir.join(LOG_FILE);
    let log_tail = read_scrubbed_log_tail(&log_path, 200);
    let find_by_id: HashMap<String, &Value> = found
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(|id| id.as_str())
                .map(|id| (id.to_string(), entry))
        })
        .collect();
    let pane_summaries: Vec<Value> = panes
        .panes
        .iter()
        .map(|pane_status| {
            let meta = find_by_id.get(&pane_status.pane.id);
            json!({
                "id": pane_status.pane.id,
                "title": pane_status.pane.title,
                "kind": pane_status.pane.kind,
                "state": pane_status.state,
                "agent": meta.and_then(|m| m.get("agent")).cloned().unwrap_or(Value::Null),
                "attention": meta.and_then(|m| m.get("attention")).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    let report = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at_ms": now_millis(),
        "cwd": status.cwd,
        "uptime_secs": status.uptime_secs,
        "subscribers": status.subscribers,
        "active_pane_id": status.active_pane_id,
        "config": status.config,
        "panes": pane_summaries,
        "daemon_log_tail": log_tail,
        "notes": [
            "env values omitted; profile env values omitted",
            "agent conversation logs and PTY scrollback are not included",
            "daemon_log_tail lines matching a secret-ish denylist are replaced with [redacted]; this is best-effort, not a guarantee",
        ],
    });
    // Always JSON — this is a machine/support artifact (`--json` is optional).
    let _ = json_output;
    write_json_stdout(&report)
}

/// Last `max_lines` of daemon.log with obvious secret-bearing lines redacted.
fn read_scrubbed_log_tail(path: &Path, max_lines: usize) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..]
        .iter()
        .map(|line| scrub_diagnostic_log_line(line))
        .collect()
}

fn scrub_diagnostic_log_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let suspicious = [
        "token",
        "authorization",
        "password",
        "secret",
        "api_key",
        "apikey",
        "cookie",
        "credential",
        "bearer ",
        "private key",
        "begin rsa",
        "begin openssh",
        "aws_secret",
        "sig=",
        "x-api-key",
    ];
    if suspicious.iter().any(|needle| lower.contains(needle)) {
        "[redacted]".to_string()
    } else {
        line.to_string()
    }
}

/// Bounded pipe reader for `ctl process`: keep at most `cap` bytes (tail) while
/// draining so a chatty child cannot grow daemon memory unboundedly.
fn read_capped_pipe_tail(mut reader: impl std::io::Read, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0_u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > cap {
                    let drop = out.len() - cap;
                    out.drain(..drop);
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn capped_utf8_tail(bytes: &[u8], cap: usize) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let start = if bytes.len() > cap {
        let mut i = bytes.len() - cap;
        while i < bytes.len() && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
            i += 1;
        }
        i
    } else {
        0
    };
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn pipe_reader_finished(handle: &Option<std::thread::JoinHandle<Vec<u8>>>) -> bool {
    handle.as_ref().map(|h| h.is_finished()).unwrap_or(true)
}

fn take_pipe_reader(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> String {
    let Some(handle) = handle else {
        return String::new();
    };
    if !handle.is_finished() {
        // Detach: caller already killed the process group; hanging forever is worse.
        return String::new();
    }
    match handle.join() {
        Ok(bytes) => capped_utf8_tail(&bytes, 256 * 1024),
        Err(_) => String::new(),
    }
}

/// Wait up to `budget` for both pipe readers. If either is still blocked (a
/// descendant holding the pipe), kill the process group and grant a short grace
/// period so readers can unblock before we abandon them.
fn finalize_run_process_pipes(
    child: &mut std::process::Child,
    mut stdout_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    mut stderr_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
    budget: Duration,
) -> (String, String) {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if pipe_reader_finished(&stdout_handle) && pipe_reader_finished(&stderr_handle) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !pipe_reader_finished(&stdout_handle) || !pipe_reader_finished(&stderr_handle) {
        kill_run_process_tree(child);
        let grace = Instant::now() + Duration::from_millis(500);
        while Instant::now() < grace {
            if pipe_reader_finished(&stdout_handle) && pipe_reader_finished(&stderr_handle) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    (
        take_pipe_reader(stdout_handle.take()),
        take_pipe_reader(stderr_handle.take()),
    )
}

fn kill_run_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        if pid > 0 {
            // SAFETY: negative pid kills the process group created in pre_exec.
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

fn exit_status_fields(status: &std::process::ExitStatus) -> (Value, Value) {
    if let Some(code) = status.code() {
        return (json!(code), Value::Null);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (Value::Null, json!(signal));
        }
    }
    (Value::Null, Value::Null)
}

fn parse_name_option(args: &[String]) -> Result<Option<String>, String> {
    let mut title: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        let (value, consumed) = match args[index].as_str() {
            "--name" | "-n" => (
                args.get(index + 1)
                    .ok_or_else(|| "--name requires a title".to_string())?
                    .clone(),
                2,
            ),
            flag if flag.starts_with('-') => {
                return Err(format!("unexpected pane option: {flag}"));
            }
            value => (value.to_string(), 1),
        };
        // Exactly one title source (L7): `new --name a b` used to silently
        // create pane `b` while `new a --name b` errored — order-dependent
        // and lossy. Any second title is now an error regardless of order.
        if title.is_some() {
            return Err(
                "pane title given more than once (use one positional NAME or one --name)"
                    .to_string(),
            );
        }
        title = Some(value);
        index += consumed;
    }
    Ok(title)
}

fn resolve_pane_ref(client: &DaemonClient, pane_ref: &str) -> Result<String, String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;
    match_pane_ref(&list, pane_ref)
}

/// (T2) Resolve a pane reference to its full status (id AND kind), for
/// commands that route by pane kind (`send`, `interrupt`).
fn resolve_pane_status(client: &DaemonClient, pane_ref: &str) -> Result<PaneStatus, String> {
    let list: PaneList = client.request(DaemonRequest::ListPanes)?;
    match_pane_status(&list, pane_ref)
}

/// (T2) Same resolution rules as `match_pane_ref`, returning the matched
/// PaneStatus instead of just the id.
fn match_pane_status(list: &PaneList, pane_ref: &str) -> Result<PaneStatus, String> {
    let pane_id = match_pane_ref(list, pane_ref)?;
    list.panes
        .iter()
        .find(|status| status.pane.id == pane_id)
        .cloned()
        .ok_or_else(|| format!("pane not found: {pane_ref}"))
}

/// Resolve a pane reference: the keyword `active`, an exact pane id, or a unique
/// title. Exact ids win over titles so a pane *titled* like another pane's id
/// cannot shadow or ambiguate the real pane.
fn match_pane_ref(list: &PaneList, pane_ref: &str) -> Result<String, String> {
    if pane_ref == "active" {
        return list
            .active_pane_id
            .clone()
            .ok_or_else(|| "workspace has no active pane".to_string());
    }

    if let Some(status) = list.panes.iter().find(|status| status.pane.id == pane_ref) {
        return Ok(status.pane.id.clone());
    }

    let matches = list
        .panes
        .iter()
        .filter(|status| status.pane.title == pane_ref)
        .map(|status| status.pane.id.clone())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [pane_id] => Ok(pane_id.clone()),
        [] => Err(format!("pane not found: {pane_ref}")),
        _ => Err(format!("pane reference is ambiguous: {pane_ref}")),
    }
}

/// Decode CLI escape sequences in input text.
///
/// By default (`literal_lf = false`), `\n` and `\r` both map to a carriage
/// return (CR, 0x0D) — the "press Enter" semantics a terminal expects when a
/// user submits a line. With `literal_lf = true` (the `--lf`/`--raw` flag),
/// `\n` maps to a literal line-feed (LF, 0x0A) so callers can transmit a raw
/// LF byte to the PTY (e.g. for tools that read fixed-size binary frames).
/// `\r` always maps to CR in both modes.
fn decode_cli_text(input: &str, literal_lf: bool) -> String {
    let mut output = String::new();
    let mut chars = input.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            output.push(char);
            continue;
        }

        match chars.next() {
            Some('n') => output.push(if literal_lf { '\n' } else { '\r' }),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('\\') => output.push('\\'),
            Some(other) => {
                output.push('\\');
                output.push(other);
            }
            None => output.push('\\'),
        }
    }
    output
}

/// Extract the `--lf`/`--raw` flag from a `send`/`broadcast` argument list.
///
/// Returns `(literal_lf, remaining)` where `remaining` preserves the order of
/// every non-flag argument. The flag may appear anywhere in the list (it is
/// pulled out before the pane/text split), so `send <pane> --lf <text>` and
/// `send --lf <pane> <text>` are both accepted.
fn parse_lf_flag(args: &[String]) -> (bool, Vec<String>) {
    let mut literal_lf = false;
    let mut remaining = Vec::with_capacity(args.len());
    let mut passthrough = false;
    for arg in args {
        if passthrough {
            remaining.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--lf" | "--raw" => literal_lf = true,
            // A `--` ends flag recognition so a payload that IS the literal
            // string `--lf`/`--raw`/`--` can still be transmitted (L6).
            "--" => passthrough = true,
            other => remaining.push(other.to_string()),
        }
    }
    (literal_lf, remaining)
}

fn write_json_stdout<T: Serialize>(value: &T) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    serde_json::to_writer_pretty(&mut stdout, value)
        .map_err(|error| format!("failed to write json: {error}"))?;
    stdout
        .write_all(b"\n")
        .map_err(|error| format!("failed to write stdout: {error}"))
}

fn print_control_help() -> Result<(), String> {
    const HELP: &str = r#"Usage: sgian ctl [--workspace PATH] [--json] <command> [args]

Global options (may precede the command; --json/--workspace may also follow
commands that don't take free text. They are recognized even right after a
value-taking flag, so use `--` when a VALUE must literally be one of them):
  -w, --workspace PATH   Target a workspace directory (default: current dir)
      --json             Emit machine-readable JSON
  -h, --help             Show this help
      --                 End global-flag recognition; later tokens pass through

Commands (PANE is a pane id or title; defaults to the active pane):
  workspaces                    List known workspaces
  daemons                       List workspaces and whether their daemon is running
  ipc-endpoint                  Start/locate the daemon and print native-client
                                IPC discovery metadata (never prints the token)
  panes | list                  List panes in the workspace
  new [--agent] [--backend claude|droid] [--model ID] [--profile NAME] [--project NAME] [--name NAME]
                                Create a pane (alias: pane new).
                                  --agent defaults to Claude
                                  --backend selects the agent CLI
                                  --model selects that CLI's model id
                                  --profile applies a named config profile
                                  Provider/model are immutable for the pane.
  status [PANE]                 Show a pane's status   (alias: pane status)
  status --verbose              Show daemon-level runtime detail (subscribers,
                                pane states, uptime, effective config summary)
  restart [PANE]                Restart a pane's shell (alias: pane restart)
  send <PANE> [--lf|--raw] [--as HOLDER] [--generation N] [--] <TEXT...>
                                Send text to a pane    (alias: pane send)
                                  By default \n and \r submit a line as Enter/CR.
                                  --lf / --raw sends a literal LF (0x0A) instead of CR.
                                  --as HOLDER attributes the input to a lease holder
                                  -- ends flag parsing (send a literal "--lf" etc.)
                                  To an agent pane, send posts a chat message
                                  (verbatim; no Enter/CR translation, no --lf).
  agent --watch [PANE]          Stream agent-state, lease and pane-end transitions
                                  (one line each; --json prints the daemon events).
                                  No PANE watches every pane; with one, exits on close.
  hook [--event NAME] [--type NAME] [--pid N] [--no-stdin]
                                The command a Claude Code hook runs: reads the
                                  hook payload from stdin, finds the pane that
                                  owns the calling process (this workspace first,
                                  then every running daemon) and sets its badge
                                  from the hook (Notification → needs input,
                                  UserPromptSubmit/PreToolUse → working, Stop →
                                  idle). Always exits 0; --json prints the answer.
  statusline [--pid N] [--exec COMMAND [ARGS...]]
                                The command Claude Code's status line runs:
                                  reads the payload from stdin, records the
                                  session's model, context fill and rate-limit
                                  windows against the pane that owns the calling
                                  process, then prints your own status command's
                                  output (fed the same payload) or a compact
                                  default line. Always exits 0; --json prints
                                  the daemon's answer instead.
  identity [list]               Issued client credentials (id, holder, scopes)
  identity issue --holder NAME [--scope read,write,admin]
                                Issue a per-client credential; the token is
                                  printed once. The client exports it as
                                  SGIAN_CLIENT_TOKEN (or names a file in
                                  SGIAN_CLIENT_TOKEN_FILE); its holder is then
                                  fixed to NAME and --as anything else is refused.
                                  Default scope: read. Config `identity: required`
                                  makes every write need a credential.
  identity revoke <ID>          Revoke a credential (its connections are cut at
                                  their next request)
  whoami                        This connection's credential, holder, scopes
                                  and the daemon's identity policy
  lease [PANE]                  Show who holds a pane's keyboard (alias: lease status)
  lease take [PANE] [--as HOLDER] [--force --why REASON]
                                Claim the keyboard. While held, input from anyone
                                  else is refused. --force revokes another holder
                                  (REASON is ledgered). HOLDER defaults to
                                  $SGIAN_HOLDER or user@host.
  lease release [PANE] -m NOTE [--as HOLDER] [--generation N]
                                Hand the keyboard back; the note is mandatory.
                                  --generation N (from `lease take --json`) makes
                                  a late command from a previous holder fail as stale.
  project list                  Projects with their attention roll-up
                                  (panes, live, needs input, working, idle,
                                  unattended, keyboard holders)
  project show <NAME>           One project with every member pane's state
  project new <NAME> [--goal TEXT] [--repo PATH]
                                Create a project (a named group of panes
                                  serving one goal; persists with the workspace)
  project add <NAME> <PANE>...  Put panes in a project (a pane is in at most one)
  project rm <PANE>...          Take panes out of their project
  project delete <NAME>         Delete a project (its panes stay open)
  project ledger <NAME> [-n N]  Member panes' ledgers merged in time order
  project dossier <NAME> [--lines N] [--out FILE]
                                One JSON document for a reviewer or a Kranz
                                  gate: the roll-up, every member pane's state,
                                  its full ledger (chain verified) and the last
                                  N scrollback lines (default 40)
  kranz status                  List panes bound to Kranz missions (auto: a
                                  `kranz run` under the pane; manual: bind)
  kranz bind [PANE] [--repo PATH]
                                Bind a pane to the mission at PATH (default: the
                                  pane's cwd). Hand-back notes are mirrored into
                                  its inbox with `kranz msg`; `kranz status`
                                  drives the pane's badge.
  kranz unbind [PANE]           Remove a binding
  search <PANE> [-i] [-n N] [--] <NEEDLE...>
                                Substring search over a pane's whole scrollback
                                  (control sequences stripped): `line<TAB>text`.
                                  -i ignores case; -n caps hits (default 100).
  lines <PANE> <A>[:<B>]        Print scrollback lines A..B (1-based, inclusive):
                                  the range a ledger record or search hit cites.
                                  Numbers are relative to the current file; the
                                  16 MiB cap drops the oldest half and renumbers.
  ledger [PANE] [-n N] [--verify]
                                Print a pane's hash-chained lease ledger (JSONL).
                                  --verify walks the chain and names the first break.
                                  A closed pane's ledger is readable by literal id.
  interrupt [PANE]              Interrupt an agent pane's current turn
                                  (agent panes only; for shells send a Ctrl-C)
  attach [PANE]                 Stream a pane's output (alias: pane attach)
  logs [-n N] [--follow]        Tail the structured daemon log
                                  -n, --lines N   Print the last N lines (default: all)
                                  -f, --follow    Stream new entries (blocks until killed)
  diagnostic                    Privacy-safe support dump (always JSON)
  process [--cwd DIR] [--timeout MS] -- <ARGV...>
                                Non-PTY argv execution in the daemon
  exec [--new] [--all] [--pane PANE] [--panes A,B] [--name NAME] -- <COMMAND...>
                                Run a command in one pane, listed panes, or all
  broadcast [--lf|--raw] [--] <TEXT...>
                                Send text to every live pane
                                  By default \n and \r submit a line as Enter/CR.
                                  --lf / --raw sends a literal LF (0x0A) instead of CR.
                                  -- ends flag parsing (send a literal "--lf" etc.)
  run [--pane PANE] [--all | --panes A,B] [--timeout MS] -- <COMMAND...>
                                Run a command and report its exit code.
                                The ctl process exits with the command's own code
                                  (clamped to 1..255 on failure), so scripts can
                                  branch on $? directly.
                                --timeout MS bounds the wait for the exit marker
                                  (omitted: waits indefinitely).
                                With --all, run in every live pane and report EACH
                                  pane's exit code (waits for all; not fire-and-forget).
                                With --panes A,B, run in only the named subset.
                                Batched --json includes timed_out, elapsed_ms, tail, error.
                                Aggregate exit is 0 only if every targeted pane succeeded.
  wait <PANE> (--text S | --regex RE | --idle MS | --exit) [--timeout MS]
                                Block until a pane satisfies a condition.
                                  --text S / --regex RE  match the visible screen
                                  --idle MS              no new output for MS ms
                                  --exit                 the pane's process ends
                                  --timeout MS           bound the wait
                                  -- ends flag parsing (--text -- --help matches
                                  the literal text "--help")
                                Exits 0 on match, non-zero on timeout; --json prints
                                  {matched,reason,revision,exit_code?,elapsed_ms}.
  snapshot <PANE> [--json]      Read a pane's rendered visible screen.
                                Default prints the screen (one line per visible row);
                                  --json prints the full struct (pane_id, cols, rows,
                                  lines, title, revision, cursor, alive, exit_code?,
                                  command, cwd, ...); an ended pane returns its final
                                  screen + exit code.
  find [--command S] [--title S] [--cwd S] [--state live|ended]
                                Query panes by metadata (filters AND together;
                                  --command/--title/--cwd are substring matches).
                                  No filters lists every pane (live and ended); no
                                  matches is an empty set, not an error. --json prints
                                  per-pane metadata (id, title, command, cwd, state,
                                  exit_code, agent, attention, group, cols, rows,
                                  revision).
  agent [PANE] [on|off]         Show a pane's agent + attention state.
                                  on/off marks/unmarks a terminal pane as running an
                                  agent CLI (persisted); no PANE targets the active pane.
                                  --json prints {pane_id,agent,attention}.
  sync on|off                   Mirror typed input to every live pane
  write-config <FILE>|<JSON>
                                Persist a config to the workspace config.json
                                  (atomically; file-watch live-reloads it)
                                  Pass a file path or an inline JSON string starting with {
  shutdown [--all]              Stop this workspace's daemon (or every daemon)

Read-only commands (panes, status, attach, logs, wait, snapshot, find, agent, lease
status, ledger, shutdown)
need a running daemon; workspaces and daemons only inspect local state (they neither
need nor start one); the other commands start a daemon on demand.

A PANE reference is a pane id or the pane's registry title (the `new --name` /
rename label). `find --title` and snapshot's title field instead report the
terminal's OSC title, which can differ from the registry title.
"#;
    std::io::stdout()
        .write_all(HELP.as_bytes())
        .map_err(|error| format!("failed to write stdout: {error}"))
}

/// Outcome of a background updater check. Errors are intentional skips — the
/// app must stay usable when the feed is down, unsigned, or unreachable
/// (ENHANCEMENTS §5 fault injection).
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateCheckOutcome {
    Available {
        version: String,
        body: Option<String>,
    },
    UpToDate,
    Skipped(String),
}

fn classify_update_check(
    result: Result<Option<(String, Option<String>)>, String>,
) -> UpdateCheckOutcome {
    match result {
        Ok(Some((version, body))) => UpdateCheckOutcome::Available { version, body },
        Ok(None) => UpdateCheckOutcome::UpToDate,
        Err(error) => UpdateCheckOutcome::Skipped(error),
    }
}

/// (M12) Check the updater feed once in the background and, when an update is
/// available, emit `update-available` with `{version, body}` to the main
/// window (the frontend shows a one-click banner wired to `install_update`).
/// EVERY error — no network, dead DNS, an unprovisioned or unsigned feed — is
/// swallowed to debug-level tracing: the app must be unaffected when the feed
/// is down.
fn spawn_update_check(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let checked = async {
            let updater = app.updater().map_err(|error| error.to_string())?;
            let update = updater.check().await.map_err(|error| error.to_string())?;
            Ok(update.map(|update| (update.version.clone(), update.body.clone())))
        }
        .await;
        match classify_update_check(checked) {
            UpdateCheckOutcome::Available { version, body } => {
                let payload = json!({
                    "version": version,
                    "body": body,
                });
                if let Err(error) = app.emit_to("main", "update-available", payload) {
                    tracing::debug!("update-available emit failed: {error}");
                }
            }
            UpdateCheckOutcome::UpToDate => {}
            UpdateCheckOutcome::Skipped(error) => {
                tracing::debug!("update check skipped: {error}");
            }
        }
    });
}

/// (M12) One-click install from the update banner: re-check the feed and, when
/// an update is available, download + install it, then restart the app. No
/// update available (a stale banner or a raced check) is a no-op. Errors
/// surface to the frontend as Err(String).
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|error| error.to_string())?
        .check()
        .await
        .map_err(|error| error.to_string())?;
    if let Some(update) = update {
        update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(|error| error.to_string())?;
        tauri::process::restart(&app.env());
    }
    Ok(())
}

/// ENHANCEMENTS §5: packaged-app UI smoke is env-gated (`SGIAN_UI_SMOKE=1`).
fn ui_smoke_env_enabled() -> bool {
    match std::env::var("SGIAN_UI_SMOKE") {
        Ok(value) => {
            let trimmed = value.trim();
            trimmed == "1"
                || trimmed.eq_ignore_ascii_case("true")
                || trimmed.eq_ignore_ascii_case("yes")
        }
        Err(_) => false,
    }
}

fn ui_smoke_marker_path() -> PathBuf {
    if let Ok(path) = std::env::var("SGIAN_UI_SMOKE_MARKER") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    resolve_workspace_dir().join(".sgian-ui-smoke-ok")
}

fn ui_smoke_error_path(marker: &Path) -> PathBuf {
    let mut path = marker.as_os_str().to_os_string();
    path.push(".err");
    PathBuf::from(path)
}

#[tauri::command]
fn ui_smoke_enabled() -> bool {
    ui_smoke_env_enabled()
}

/// Write the smoke marker (or error sidecar) and exit the GUI process.
#[tauri::command]
fn complete_ui_smoke(ok: bool, error: Option<String>) -> Result<(), String> {
    if !ui_smoke_env_enabled() {
        return Err("packaged UI smoke mode is not enabled".to_string());
    }
    let marker = ui_smoke_marker_path();
    if let Some(parent) = marker.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if ok {
        let _ = fs::write(&marker, b"ok\n");
        std::process::exit(0);
    }
    let err_path = ui_smoke_error_path(&marker);
    let message = error.unwrap_or_else(|| "ui smoke failed".to_string());
    let _ = fs::write(&err_path, format!("{message}\n"));
    let _ = fs::remove_file(&marker);
    std::process::exit(1);
}

pub fn run() {
    let args = std::env::args().collect::<Vec<_>>();
    if is_control_invocation(&args) {
        if let Err(error) = run_control_cli_from_args(&args) {
            let _ = writeln!(std::io::stderr(), "sgianctl: {error}");
            // Default 1; `ctl run` stores the command's own exit code here (L2)
            // so scripts can branch on `$?` without parsing stdout.
            std::process::exit(CLI_EXIT_CODE.load(Ordering::SeqCst));
        }
        return;
    }

    if args.iter().any(|arg| arg == DAEMON_ARG) {
        if let Err(error) = run_daemon_from_args(&args) {
            record_daemon_startup_error(&args, &error);
            let _ = writeln!(std::io::stderr(), "sgian daemon: {error}");
            std::process::exit(1);
        }
        return;
    }

    tauri::Builder::default()
        .manage(AppState::new())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // Background update check (M12); failures are swallowed inside.
            spawn_update_check(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap_workspace,
            create_pane,
            close_pane,
            rename_pane,
            ensure_pane_terminal,
            restart_pane_terminal,
            write_to_pane,
            resize_pane_terminal,
            set_active_pane,
            update_workspace_layout,
            get_config,
            write_config,
            create_agent_pane,
            send_agent_message,
            agent_approval,
            interrupt_agent,
            client_holder,
            take_lease,
            release_lease,
            lease_status,
            install_update,
            ui_smoke_enabled,
            complete_ui_smoke
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Sgian");
}

#[cfg(test)]
mod tests;
