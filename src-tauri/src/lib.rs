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
/// The workspace path on its own, written beside `workspace.json` at daemon
/// start, so the workspace-key collision guard still has something to check
/// when `workspace.json` is unparseable (S6 of the 2026-09-20 review).
const WORKSPACE_CWD_FILE: &str = "workspace.cwd";
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

mod transport;
use transport::*;
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

impl DaemonEvent {
    /// An `agent_state` event with `unattended` derived from `mode`.
    pub(crate) fn agent_state(
        pane_id: String,
        agent: Option<String>,
        attention: Option<AgentAttention>,
        mode: Option<String>,
    ) -> Self {
        let unattended = is_unattended_mode(mode.as_deref());
        DaemonEvent::AgentState {
            pane_id,
            agent,
            attention,
            mode,
            unattended,
        }
    }
}

/// Copy into `incoming` every key of the workspace config file at
/// `config_path` that `incoming` does not mention and that holds something
/// (not null, not an empty list or map). See the `WriteConfig` handler.
fn preserve_omitted_config_keys(incoming: &mut Value, config_path: &Path) {
    let Some(object) = incoming.as_object_mut() else {
        return;
    };
    let Some(existing) = read_config_file(config_path).ok().flatten() else {
        return;
    };
    let Ok(Value::Object(current)) = serde_json::to_value(&existing) else {
        return;
    };
    for (key, value) in current {
        if object.contains_key(&key) {
            continue;
        }
        let empty = match &value {
            Value::Null => true,
            Value::Array(items) => items.is_empty(),
            Value::Object(entries) => entries.is_empty(),
            _ => false,
        };
        if !empty {
            object.insert(key, value);
        }
    }
}

/// Modes in which an agent runs tools without a person approving them.
pub(crate) fn is_unattended_mode(mode: Option<&str>) -> bool {
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
            // The coordination and identity settings ride along as stored
            // (null when unset) so a get → edit → write round-trip from a
            // settings form cannot drop them (issue #32).
            "lease_policy": self.lease_policy,
            "agent_probe_interval_ms": self.agent_probe_interval_ms,
            "kranz_bin": self.kranz_bin,
            "identity": self.identity,
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
    /// Shared context notes (docs/design/shared-context-notes.md): short
    /// Markdown files under the project's repo that every pane in the
    /// project can read. `holder` is the writer; a credentialed connection
    /// must match its own holder, as for input and leases.
    ProjectNoteAdd {
        name: String,
        title: String,
        body: String,
        holder: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
    },
    ProjectNotes {
        name: String,
    },
    ProjectNoteRemove {
        name: String,
        file: String,
        holder: String,
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
        /// Derived from `mode` (see `is_unattended_mode`) and always sent, so
        /// a live transition and the bootstrap snapshot agree; additive.
        #[serde(default)]
        unattended: bool,
    },
    /// Output-guard hit: `added` since the last announcement, `total` so far
    /// (docs/design/keyboard-lease-and-ledger.md §7). Rate-limited per pane.
    OutputWarning {
        pane_id: String,
        added: OutputTricks,
        total: OutputTricks,
        /// What the pane's first opaque string control looked like (its kind
        /// and a bounded, escaped prefix of its body), so a badge can say what
        /// was seen. Absent when every hit was self-describing; additive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample: Option<String>,
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
    /// A shared context note was written, changed or removed
    /// (docs/design/shared-context-notes.md): the project, the file and the
    /// content hash of what is now there, `null` once it is gone. The
    /// daemon's own writes announce synchronously; a file watch covers
    /// edits made outside it. Never carries the note's contents: a client
    /// or agent decides to read, it is not fed.
    ProjectNotesChanged {
        project: String,
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
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

mod identity;
use identity::*;
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

mod process_tree;
use process_tree::*;
#[cfg(windows)]
mod windows_job;
#[cfg(windows)]
use windows_job::*;

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
    input: InputQueue,
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
/// Bytes queued but not yet written, per pane. 256 entries of up to a frame
/// each could otherwise pin gigabytes behind a pane that stopped reading
/// (S11 of the 2026-09-20 review).
const PANE_INPUT_QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// A pane's input queue: the channel into its writer thread plus the bytes
/// currently queued, so the cap is on memory, not only entry count.
struct InputQueue {
    sender: SyncSender<Vec<u8>>,
    queued_bytes: Arc<AtomicUsize>,
}

/// Spawn the dedicated writer thread that drains a pane's input queue into its
/// PTY. The thread exits when the session drops (sender dropped → recv errs) or
/// when a write fails (PTY gone); after that, queued sends error `Disconnected`.
fn spawn_input_writer(writer: Box<dyn Write + Send>) -> InputQueue {
    let queued_bytes = Arc::new(AtomicUsize::new(0));
    let sender = spawn_input_writer_raw(writer, Some(Arc::clone(&queued_bytes)));
    InputQueue {
        sender,
        queued_bytes,
    }
}

/// The writer thread alone. Agent stdin uses this directly: its messages are
/// bounded per message (`AGENT_MESSAGE_MAX_BYTES`), not per queue.
pub(crate) fn spawn_input_writer_raw(
    mut writer: Box<dyn Write + Send>,
    drained: Option<Arc<AtomicUsize>>,
) -> SyncSender<Vec<u8>> {
    let (sender, receiver) = sync_channel::<Vec<u8>>(PANE_INPUT_QUEUE_LIMIT);
    thread::spawn(move || {
        while let Ok(chunk) = receiver.recv() {
            let len = chunk.len();
            let ok = writer.write_all(&chunk).is_ok() && writer.flush().is_ok();
            if let Some(counter) = &drained {
                counter.fetch_sub(len, Ordering::SeqCst);
            }
            if !ok {
                break;
            }
        }
    });
    sender
}

/// Queue input for a pane's writer thread, failing fast when the pane has
/// stopped draining (queue full by count or by bytes) or its writer thread
/// has exited.
fn queue_pane_input(input: &InputQueue, pane_id: &str, data: &str) -> Result<(), String> {
    let len = data.len();
    let before = input.queued_bytes.fetch_add(len, Ordering::SeqCst);
    if before.saturating_add(len) > PANE_INPUT_QUEUE_BYTES {
        input.queued_bytes.fetch_sub(len, Ordering::SeqCst);
        return Err(format!(
            "terminal input backlogged (pane is not reading stdin): {pane_id}"
        ));
    }
    match input.sender.try_send(data.as_bytes().to_vec()) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            input.queued_bytes.fetch_sub(len, Ordering::SeqCst);
            Err(format!(
                "terminal input backlogged (pane is not reading stdin): {pane_id}"
            ))
        }
        Err(TrySendError::Disconnected(_)) => {
            input.queued_bytes.fetch_sub(len, Ordering::SeqCst);
            Err(format!("terminal session ended: {pane_id}"))
        }
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

mod agent_stream;
use agent_stream::*;
mod output_guard;
use output_guard::*;
mod ledger;
use ledger::*;
mod notes;
mod probe;
use probe::*;
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
mod router;
use router::*;
mod terminals;
use terminals::*;
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
    /// The workspace directory: the default root for a project's notes.
    cwd: PathBuf,
    /// The file watch over every notes root, and the paths it reports; the
    /// accept loop drains them into `project_notes_changed` events.
    notes_watch: Mutex<Option<notes::NotesWatch>>,
    notes_changes: Mutex<std::sync::mpsc::Receiver<PathBuf>>,
    /// Notes the daemon itself just wrote or removed, by (project, file) →
    /// resulting hash, so the watch does not announce them a second time.
    notes_own_writes: Mutex<HashMap<(String, String), Option<String>>>,
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
    /// Live subscriptions per credential id, so a revocation can end the
    /// event streams a credential opened, not only its next request. Leaf lock.
    credential_subscribers: Mutex<HashMap<String, Vec<u64>>>,
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
        // Collision check uses the existing marker / persist file. Writing the
        // marker first would stamp a colliding cwd onto a corrupt data dir and
        // skip the guard (S6 follow-up).
        refuse_workspace_cwd_mismatch(&cwd, &data_dir, &loaded)?;
        write_workspace_cwd_marker(&data_dir, &cwd);

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
        let cwd_for_notes = cwd.clone();
        let (notes_tx, notes_rx) = std::sync::mpsc::channel::<PathBuf>();
        let notes_watch = notes::NotesWatch::new(notes_tx);
        if notes_watch.is_none() {
            tracing::warn!(
                event = "notes_watch_init_failed",
                "failed to initialize the notes file watcher; only the daemon's own writes will announce"
            );
        }

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
            cwd: cwd_for_notes,
            notes_watch: Mutex::new(notes_watch),
            notes_changes: Mutex::new(notes_rx),
            notes_own_writes: Mutex::new(HashMap::new()),
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
            credential_subscribers: Mutex::new(HashMap::new()),
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
        self.refresh_notes_watch();
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
        let mut keys = project.panes.clone();
        keys.push(project_ledger_key(name));
        let mut records: Vec<Value> = keys
            .iter()
            .flat_map(|key| read_ledger_tail(&ledger_path(&dir, key), 0))
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
        let project_ledger = ledger_path(&dir, &project_ledger_key(name));
        let chain = if project_ledger.exists() {
            match ledger_verify(&project_ledger) {
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
        // The notes directory lives in the repo, outside the daemon's own
        // files: an unreadable one must not take the roll-up, the ledgers
        // and the scrollback down with it.
        let notes = self
            .handle_project_notes(name)
            .unwrap_or_else(|error| json!({ "error": error }));
        Ok(json!({
            "format": PROJECT_DOSSIER_FORMAT,
            "generated_at_ms": now_millis(),
            "workspace": self.workspace_key,
            "summary": detail["summary"],
            "ledger": {
                "chain": chain,
                "records": read_ledger_tail(&project_ledger, 0),
            },
            "notes": notes,
            "panes": panes,
        }))
    }

    // ----- Shared context notes (docs/design/shared-context-notes.md) -----

    /// Where a project's notes live: under its `repo` when it names one,
    /// the workspace directory otherwise. A relative `repo` is taken from
    /// the workspace. The root must exist and be either the workspace or a
    /// git repository (it has a `.git` entry): `repo` is free text any
    /// write-scoped client can set, and notes are the first thing the daemon
    /// WRITES under it, so it must not become a way to create files in an
    /// arbitrary directory.
    fn project_notes_dir(&self, name: &str) -> Result<PathBuf, String> {
        let project = self
            .projects_snapshot()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown project '{name}'"))?;
        let root = self.notes_root(project.repo.as_deref())?;
        Ok(notes::notes_dir(&root, name))
    }

    /// The root a project's notes live under, with the rules above applied.
    fn notes_root(&self, repo: Option<&str>) -> Result<PathBuf, String> {
        let Some(repo) = repo else {
            return Ok(self.cwd.clone());
        };
        let candidate = self.cwd.join(repo);
        let root = candidate
            .canonicalize()
            .map_err(|error| format!("project repo {}: {error}", candidate.display()))?;
        if !root.is_dir() {
            return Err(format!(
                "project repo {} is not a directory",
                root.display()
            ));
        }
        let workspace = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        if root != workspace && !root.join(".git").exists() {
            return Err(format!(
                "project repo {} is not a git repository; notes live in one",
                root.display()
            ));
        }
        Ok(root)
    }

    /// Point the file watch at every root a project's notes can live under:
    /// the workspace and each project's repo. Called when the project table
    /// changes and after the daemon writes a note (the directory may be new).
    fn refresh_notes_watch(&self) {
        let mut roots = vec![self.cwd.clone()];
        for project in self.projects_snapshot().values() {
            if let Ok(root) = self.notes_root(project.repo.as_deref()) {
                roots.push(root);
            }
        }
        if let Ok(mut watch) = self.notes_watch.lock() {
            if let Some(watch) = watch.as_mut() {
                watch.sync_roots(roots);
            }
        }
    }

    /// Announce a note the daemon itself wrote or removed, and remember it
    /// so the file watch's echo of the same change is dropped.
    fn announce_own_note(&self, project: &str, file: &str, hash: Option<String>) {
        if let Ok(mut own) = self.notes_own_writes.lock() {
            own.insert((project.to_string(), file.to_string()), hash.clone());
        }
        self.router.broadcast(&DaemonEvent::ProjectNotesChanged {
            project: project.to_string(),
            file: file.to_string(),
            hash,
        });
        self.refresh_notes_watch();
    }

    /// Turn the watch's pending paths into `project_notes_changed` events:
    /// one per (project, file), with the hash of what is there now. Runs on
    /// the accept loop's idle tick, so it never blocks a request.
    pub(crate) fn drain_notes_changes(&self) {
        let mut changed: std::collections::BTreeSet<(String, String)> =
            std::collections::BTreeSet::new();
        if let Ok(receiver) = self.notes_changes.lock() {
            while let Ok(path) = receiver.try_recv() {
                if let Some(change) = notes::note_change_from_path(&path) {
                    changed.insert(change);
                }
            }
        }
        for (project, file) in changed {
            let Ok(dir) = self.project_notes_dir(&project) else {
                continue;
            };
            let hash = fs::read(dir.join(&file))
                .ok()
                .map(|raw| notes::note_hash(&raw));
            // The daemon's own write already announced exactly this state.
            let own = self
                .notes_own_writes
                .lock()
                .ok()
                .and_then(|mut own| own.remove(&(project.clone(), file.clone())));
            if own.as_ref() == Some(&hash) {
                continue;
            }
            self.router.broadcast(&DaemonEvent::ProjectNotesChanged {
                project,
                file,
                hash,
            });
        }
    }

    fn handle_project_note_add(
        &self,
        name: &str,
        title: &str,
        body: &str,
        holder: &str,
        pane_id: Option<&str>,
    ) -> Result<Value, String> {
        let dir = self.project_notes_dir(name)?;
        let title = validate_bounded_text(title, "title", notes::NOTE_TITLE_MAX_BYTES)?;
        if title.contains('\n') {
            return Err("title must be one line".to_string());
        }
        let body = validate_bounded_text(body, "body", notes::NOTE_MAX_BYTES)?;
        let holder = validate_holder(holder)?;
        if let Some(pane_id) = pane_id {
            self.ensure_pane_exists(pane_id)?;
        }
        let meta = notes::NoteMeta {
            title: title.clone(),
            holder: holder.clone(),
            pane: pane_id.map(str::to_string),
            written_at_ms: now_millis(),
        };
        let written = notes::add_note(&dir, &meta, &body)?;
        // One receipt: what the ledger records is what the client is told.
        let mut receipt = json!({
            "project": name,
            "file": written.file,
            "title": title,
            "holder": holder,
            "pane_id": pane_id,
            "hash": written.hash,
            "bytes": written.bytes,
        });
        let _ = self.ledger_record(&project_ledger_key(name), "note.added", receipt.clone());
        self.announce_own_note(name, &written.file, Some(written.hash.clone()));
        receipt["path"] = json!(written.path.display().to_string());
        Ok(receipt)
    }

    fn handle_project_notes(&self, name: &str) -> Result<Value, String> {
        let dir = self.project_notes_dir(name)?;
        let listing = notes::list_notes(&dir, name)?;
        Ok(json!(listing))
    }

    fn handle_project_note_remove(
        &self,
        name: &str,
        file: &str,
        holder: &str,
    ) -> Result<Value, String> {
        let dir = self.project_notes_dir(name)?;
        let holder = validate_holder(holder)?;
        let (hash, bytes) = notes::remove_note(&dir, file)?;
        let _ = self.ledger_record(
            &project_ledger_key(name),
            "note.removed",
            json!({ "project": name, "file": file, "holder": holder, "hash": hash, "bytes": bytes }),
        );
        self.announce_own_note(name, file, None);
        Ok(json!({ "project": name, "file": file, "holder": holder, "hash": hash, "bytes": bytes }))
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
            DaemonRequest::ProjectNoteAdd {
                name,
                title,
                body,
                holder,
                pane_id,
            } => self.handle_project_note_add(&name, &title, &body, &holder, pane_id.as_deref()),
            DaemonRequest::ProjectNotes { name } => self.handle_project_notes(&name),
            DaemonRequest::ProjectNoteRemove { name, file, holder } => {
                self.handle_project_note_remove(&name, &file, &holder)
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

                // A key the payload OMITS keeps the workspace file's current
                // value; a key it carries, including an explicit null, [] or
                // {}, replaces it. Serde would otherwise default every missing
                // field and the atomic write below would persist that, so a
                // settings form that knows nothing of `lease_policy`,
                // `identity`, `scrub_env` or a field added later would reset
                // it on an unrelated save (M1, issue #32), and a save sent
                // before the form had loaded would wipe the file (issue #33).
                let mut config = config;
                preserve_omitted_config_keys(&mut config, &config_path);

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
            terminals.apply_reloaded_config(new_shell, new_agent_config);
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
        let live_modes = terminals.agent_session_modes();
        drop(terminals);
        // (T1) Agent info rides the bootstrap payload parallel to pane_states.
        snapshot.agent_states = self.router.agent_states();
        // An agent-kind pane's mode is the permission mode its CLI was started
        // with (the configured one for a pane not running yet), not a screen:
        // overlay it so every pane carries `unattended` the same way.
        let configured_mode = self.effective_config().agent_config().permission_mode;
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
            let permission_mode = live_modes
                .get(&pane.id)
                .cloned()
                .unwrap_or_else(|| configured_mode.clone());
            entry.unattended = is_unattended_mode(Some(&permission_mode));
            entry.mode = Some(permission_mode);
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

    /// The gate `Subscribe` passes before it becomes an event stream: the same
    /// revocation and scope checks as any other request (S1 of the 2026-09-20
    /// review; `Subscribe` never reaches `handle_as`).
    fn authorize_subscribe(&self, identity: &ClientIdentity) -> Result<(), String> {
        if let Some(id) = identity.credential.as_deref() {
            if !self.credential_is_active(id) {
                return Err("client credential revoked".to_string());
            }
        }
        if !identity.has(ClientScope::Read) {
            return Err("read-only credential: 'read' scope required for subscribe".to_string());
        }
        Ok(())
    }

    /// Recheck revocation and register atomically, before the router can deliver
    /// any output. Revoke takes clients before removing registered subscribers.
    fn register_subscription(
        &self,
        stream: TransportStream,
        wire_version: u16,
        credential: Option<&str>,
    ) -> Result<u64, (String, TransportStream)> {
        let Some(id) = credential else {
            return self.router.add_subscriber(stream, wire_version);
        };
        let Ok(clients) = self.clients.lock() else {
            return Err(("client table lock poisoned".into(), stream));
        };
        if !clients
            .clients
            .iter()
            .any(|record| record.id == id && record.revoked_at_ms.is_none())
        {
            return Err(("client credential revoked".into(), stream));
        }
        let Ok(mut table) = self.credential_subscribers.lock() else {
            return Err(("credential subscriber table lock poisoned".into(), stream));
        };
        let sub_id = self.router.add_subscriber(stream, wire_version)?;
        table.entry(id.to_string()).or_default().push(sub_id);
        Ok(sub_id)
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
        // End the event streams this credential opened; its requests are
        // refused by `handle_as` from now on.
        let streams = self
            .credential_subscribers
            .lock()
            .ok()
            .and_then(|mut table| table.remove(id))
            .unwrap_or_default();
        for sub_id in &streams {
            self.router.remove_subscriber(*sub_id);
        }
        tracing::info!(
            workspace_key = %self.workspace_key,
            event = "identity_revoked",
            credential = %id,
            subscriptions_ended = streams.len(),
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
        // Agent control (prompt, approve, interrupt) is a write to the pane
        // like a keystroke and honours the keyboard lease the same way for a
        // credentialed connection (S3 of the 2026-09-20 review). The root
        // token carries no holder on these requests; under `identity: open`
        // the lease is coordination for it, and under `required` root cannot
        // write at all.
        if identity.credential.is_some() {
            if let DaemonRequest::SendAgentMessage { pane_id, .. }
            | DaemonRequest::AgentApproval { pane_id, .. }
            | DaemonRequest::InterruptAgent { pane_id } = &request
            {
                self.check_lease_write(pane_id, identity.holder.as_deref(), None)?;
            }
        }
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

/// A project's shared context notes, newest first, bodies scrubbed
/// (docs/design/shared-context-notes.md).
#[tauri::command]
fn project_notes(name: String, state: State<'_, AppState>) -> Result<Value, String> {
    state
        .client()?
        .request(DaemonRequest::ProjectNotes { name })
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

mod serve;
use serve::*;
mod daemon_client;
use daemon_client::*;
mod ctl;
use ctl::*;
mod serve_http;
use serve_http::*;
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
            project_notes,
            install_update,
            ui_smoke_enabled,
            complete_ui_smoke
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Sgian");
}

#[cfg(test)]
mod tests;
