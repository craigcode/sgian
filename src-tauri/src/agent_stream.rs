use super::*;

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
// direct `Child::kill()` (TerminateProcess — immediate, no grace) plus a
// kill-on-close Job Object so `cmd.exe` shim grandchildren die with the
// session. npm's `claude.cmd` shim must be spawned via `cmd.exe /c`
// (CreateProcess cannot run batch scripts — see resolve_agent_bin).
// On platforms that are neither unix nor Windows, CreateAgentPane
// fails with a clean error.
// ---------------------------------------------------------------------------

/// Permission modes accepted by `claude --permission-mode` (2.1.201). Only
/// `manual` routes approval prompts over the control channel; the others are
/// passed through for operators who want unattended runs.
pub(crate) const AGENT_PERMISSION_MODES: [&str; 6] = [
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
pub(crate) const SGIAN_CLAUDE_BIN_ENV: &str = "SGIAN_CLAUDE_BIN";
pub(crate) const SGIAN_DROID_BIN_ENV: &str = "SGIAN_DROID_BIN";
/// Per-pane conversation log lives at `<data_dir>/agents/<pane-id>.jsonl`.
pub(crate) const AGENT_LOG_DIR: &str = "agents";
/// Conversation log cap, trimmed to HALF on overflow (hysteresis, like
/// scrollback) at a line boundary so every kept line stays parseable.
#[cfg(any(unix, windows))]
pub(crate) const AGENT_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Bootstrap replay bounds: last 1 MiB / 500 normalized events per agent pane.
pub(crate) const AGENT_REPLAY_MAX_BYTES: u64 = 1024 * 1024;
pub(crate) const AGENT_REPLAY_MAX_EVENTS: usize = 500;
/// Cap on a single SendAgentMessage text (and an approval's denial message).
#[cfg(any(unix, windows))]
pub(crate) const AGENT_MESSAGE_MAX_BYTES: usize = 256 * 1024;
/// A permission request denied automatically after this long without an
/// AgentApproval (the CLI blocks on the reply; unbounded waits wedge turns).
/// The wait re-checks child liveness every AGENT_APPROVAL_POLL (H1), so a
/// dead child is detected within ~1s rather than sitting out this timeout.
#[cfg(all(any(unix, windows), not(test)))]
pub(crate) const AGENT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
/// (T2) L5: tests can't sit out the production 10-minute timeout.
#[cfg(all(any(unix, windows), test))]
pub(crate) const AGENT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(2);
/// (T2) Increment of the bounded permission wait: each elapsed increment
/// re-checks child liveness so a CLI that died mid-permission unwedges the
/// pane's reader promptly instead of after the full timeout (H1).
#[cfg(any(unix, windows))]
pub(crate) const AGENT_APPROVAL_POLL: Duration = Duration::from_secs(1);
/// SIGTERM→SIGKILL escalation grace when closing an agent session. unix-only:
/// Windows kills are immediate (TerminateProcess), with nothing to escalate to.
#[cfg(unix)]
pub(crate) const AGENT_KILL_GRACE: Duration = Duration::from_secs(2);
/// A single stdout line longer than this is dropped (with an `error` event)
/// instead of buffering unboundedly against a broken/hostile child.
#[cfg(any(unix, windows))]
pub(crate) const AGENT_OUTPUT_LINE_MAX: usize = 4 * 1024 * 1024;
#[cfg(not(any(unix, windows)))]
pub(crate) const AGENT_UNSUPPORTED: &str =
    "agent panes are not supported on this platform (unix and windows only)";

/// (T2) Request-id sequence for daemon-initiated `interrupt` control requests
/// (ours must not collide with the CLI's own request ids).
#[cfg(any(unix, windows))]
pub(crate) static AGENT_INTERRUPT_SEQ: AtomicU64 = AtomicU64::new(1);

/// (T2) The agent-spawn half of Config, mirrored into the TerminalStore
/// alongside ShellConfig (and refreshed by the config file-watch with it).
/// Read only when spawning agent sessions (unix + Windows).
#[derive(Debug, Clone, Default)]
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
pub(crate) struct AgentSpawnConfig {
    /// Explicit provider binary overrides; None resolves via the matching
    /// SGIAN_*_BIN environment override and then PATH.
    pub(crate) claude_bin: Option<String>,
    pub(crate) droid_bin: Option<String>,
    pub(crate) permission_mode: String,
}

/// (T2) How the resolved `claude` binary must be spawned. Pure decision type,
/// unit-tested on every host (see plan_agent_bin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentBinPlan {
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
pub(crate) fn classify_agent_bin(candidate: String) -> AgentBinPlan {
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
pub(crate) fn plan_agent_bin(
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
pub(crate) fn probe_agent_path(
    path_var: &std::ffi::OsStr,
    exists: impl Fn(&Path) -> bool,
) -> Option<AgentBinPlan> {
    probe_named_agent_path(path_var, "claude", exists)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn probe_named_agent_path(
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
pub(crate) fn agent_bin_display(bin: &AgentBinPlan) -> String {
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
pub(crate) fn agent_command_argv(
    bin: &AgentBinPlan,
    args: &[String],
) -> Option<(String, Vec<String>)> {
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
pub(crate) fn resolve_provider_bin(
    config: &AgentSpawnConfig,
    backend: AgentBackendKind,
) -> AgentBinPlan {
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
pub(crate) struct AgentShared {
    /// The CLI's session id from its init event, recorded for `--resume`.
    pub(crate) session_id: Option<String>,
    /// Pending permission requests: request_id → reply channel. The reader
    /// blocks on the receiver until an AgentApproval sends the decision, the
    /// wait times out, the child dies (H1), or the session closes (deny).
    pub(crate) pending: HashMap<String, SyncSender<AgentApprovalDecision>>,
    /// One in-flight turn per pane: set by SendAgentMessage, cleared when the
    /// reader sees the turn's `result`, when the process exits, or when an
    /// interrupt is sent (M2 — a lost `result` line must not wedge the pane).
    pub(crate) turn_running: bool,
}

/// (T2) The daemon's answer to a permission request.
#[cfg(any(unix, windows))]
pub(crate) struct AgentApprovalDecision {
    pub(crate) allow: bool,
    pub(crate) message: Option<String>,
    /// Why the request resolved, echoed as the emitted `permission_resolved`
    /// event's `reason`: "user" | "timeout" | "closed" | "process_exit".
    pub(crate) reason: &'static str,
}

/// (T2) Deny every pending permission request (session close/process exit):
/// a reader blocked waiting for its decision wakes and can observe EOF
/// instead of sitting out the full approval timeout. `reason` is the wire
/// `permission_resolved` reason ("closed" | "process_exit"); the CLI-facing
/// deny message is derived from it.
#[cfg(any(unix, windows))]
pub(crate) fn deny_agent_pending(shared: &Arc<Mutex<AgentShared>>, reason: &'static str) {
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
pub(crate) fn agent_try_begin_turn(shared: &Arc<Mutex<AgentShared>>) -> bool {
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
pub(crate) fn agent_end_turn(shared: &Arc<Mutex<AgentShared>>) -> bool {
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
pub(crate) struct AgentChildKiller {
    pub(crate) pid: u32,
    pub(crate) reaped: Arc<AtomicBool>,
}

#[cfg(unix)]
impl AgentChildKiller {
    pub(crate) fn kill(&self) {
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

/// Windows kills the entire job immediately, then reaps through the shared
/// child handle. Killing the job first releases a reader blocked in wait().
/// The job also kills the tree if the session or daemon drops unexpectedly.
#[cfg(windows)]
pub(crate) struct AgentChildKiller {
    pub(crate) child: Arc<Mutex<std::process::Child>>,
    pub(crate) job: KillOnCloseJob,
}

#[cfg(windows)]
impl AgentChildKiller {
    pub(crate) fn kill(&self) {
        self.job.terminate();
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

/// (T2) A live agent session: the stdin writer queue (same writer-thread
/// pattern as PTY input — no blocking pipe write under the store lock), the
/// child killer, and the shared state. Dropping it (close/restart/shutdown)
/// denies pending approvals and kills the CLI (SIGTERM→SIGKILL on unix,
/// TerminateProcess plus Job Object on Windows).
#[cfg(any(unix, windows))]
pub(crate) struct AgentSession {
    pub(crate) backend: AgentBackendKind,
    /// The permission mode this CLI was started with. A config reload
    /// changes future spawns, not this process, and the badge must say what
    /// is actually running (S9 of the 2026-09-20 review).
    pub(crate) permission_mode: String,
    pub(crate) input: SyncSender<Vec<u8>>,
    pub(crate) killer: AgentChildKiller,
    pub(crate) shared: Arc<Mutex<AgentShared>>,
    pub(crate) events: Arc<Mutex<AgentEventLog>>,
}

#[cfg(any(unix, windows))]
impl AgentSession {
    pub(crate) fn kill(&self) {
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
pub(crate) fn queue_agent_stdin(
    input: &SyncSender<Vec<u8>>,
    pane_id: &str,
    line: &str,
) -> Result<(), String> {
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
pub(crate) struct AgentSpawnPlan {
    pub(crate) backend: AgentBackendKind,
    pub(crate) bin: AgentBinPlan,
    pub(crate) args: Vec<String>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) scrub_env: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) command_str: String,
    pub(crate) cwd_str: String,
    /// Provider-specific first protocol request, queued immediately after the
    /// stdin writer starts. Claude initializes itself; Droid JSON-RPC needs an
    /// explicit initialize/load request.
    pub(crate) initial_input: Option<String>,
}

/// (T2) A spawned-but-not-yet-committed agent child (M7), mirroring
/// `PreparedSpawn`: produced off-lock by `execute_agent_spawn`, committed
/// under the lock by `TerminalStore::commit_agent_spawn`.
#[cfg(any(unix, windows))]
pub(crate) struct PreparedAgentSpawn {
    pub(crate) backend: AgentBackendKind,
    pub(crate) child: std::process::Child,
    #[cfg(windows)]
    pub(crate) job: KillOnCloseJob,
    pub(crate) stdin: std::process::ChildStdin,
    pub(crate) stdout: std::process::ChildStdout,
    pub(crate) stderr: std::process::ChildStderr,
    pub(crate) command_str: String,
    pub(crate) cwd_str: String,
    pub(crate) initial_input: Option<String>,
}

/// (T2) The expensive half of an agent spawn — process fork/exec with piped
/// stdio — run WITHOUT the store lock (M7). Environment handling matches
/// `execute_spawn`: the daemon's inherited env, minus the config scrub list,
/// plus explicit `env` entries (compute_spawn_env semantics; no TERM/COLORTERM
/// — there is no terminal).
#[cfg(any(unix, windows))]
pub(crate) fn execute_agent_spawn(plan: &AgentSpawnPlan) -> Result<PreparedAgentSpawn, String> {
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
    for key in INHERITED_SESSION_MARKERS {
        command.env_remove(key);
    }
    for key in &plan.scrub_env {
        command.env_remove(key);
    }
    for (key, value) in &plan.env {
        command.env(key, value);
    }
    #[cfg(windows)]
    let spawned = KillOnCloseJob::spawn(&mut command);
    #[cfg(unix)]
    let spawned = command.spawn();
    let spawned = spawned.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "{provider} CLI not found: '{}' is not executable or not on PATH",
                program,
            )
        } else {
            format!("failed to spawn agent CLI '{}': {error}", program)
        }
    })?;
    #[cfg(windows)]
    let (mut child, job) = spawned;
    #[cfg(unix)]
    let mut child = spawned;
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
        #[cfg(windows)]
        job,
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
pub(crate) fn normalize_agent_event(raw: &Value) -> Vec<Value> {
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
pub(crate) fn normalize_provider_agent_event(backend: AgentBackendKind, raw: &Value) -> Vec<Value> {
    match backend {
        AgentBackendKind::Claude => normalize_agent_event(raw),
        AgentBackendKind::Droid => normalize_droid_agent_event(raw),
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn normalize_droid_agent_event(raw: &Value) -> Vec<Value> {
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
pub(crate) fn normalize_agent_stream_event(raw: &Value) -> Vec<Value> {
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
pub(crate) fn normalize_agent_assistant(raw: &Value) -> Vec<Value> {
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
pub(crate) fn normalize_agent_tool_results(raw: &Value) -> Vec<Value> {
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
pub(crate) fn normalize_agent_result(raw: &Value) -> Value {
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
pub(crate) fn normalize_agent_control_request(raw: &Value) -> Vec<Value> {
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
pub(crate) fn read_capped_line<R: BufRead>(
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
pub(crate) fn agent_log_path(agents_dir: &Path, pane_id: &str) -> PathBuf {
    agents_dir.join(format!("{pane_id}.jsonl"))
}

/// (T2) Drop `.jsonl.tmp` cap litter at daemon start (mirror of the
/// scrollback temp sweep): no cap can be in flight before the daemon serves.
pub(crate) fn prune_agent_log_temps(agents_dir: &Path) {
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
pub(crate) fn prune_orphan_agent_logs(agents_dir: &Path, live_pane_ids: &HashSet<String>) {
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
pub(crate) struct AgentLogWriter {
    pub(crate) file: File,
    pub(crate) len: u64,
    pub(crate) pane_id: String,
    pub(crate) agents_dir: PathBuf,
}

#[cfg(any(unix, windows))]
impl AgentLogWriter {
    pub(crate) fn open(agents_dir: &Path, pane_id: &str) -> Option<Self> {
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

    pub(crate) fn append_line(&mut self, line: &str) {
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
pub(crate) fn cap_agent_log_file(agents_dir: &Path, pane_id: &str) -> Result<(), String> {
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
pub(crate) fn read_agent_log_tail(
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
pub(crate) fn agent_log_last_seq(agents_dir: &Path, pane_id: &str) -> u64 {
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
pub(crate) struct AgentReaderCtx {
    pub(crate) pane_id: String,
    pub(crate) backend: AgentBackendKind,
    pub(crate) generation: u64,
    pub(crate) child: Arc<Mutex<std::process::Child>>,
    pub(crate) stdout: std::process::ChildStdout,
    pub(crate) input: SyncSender<Vec<u8>>,
    pub(crate) shared: Arc<Mutex<AgentShared>>,
    pub(crate) liveness: Arc<Mutex<HashMap<String, PaneLiveness>>>,
    pub(crate) router: OutputRouter,
    pub(crate) events: Arc<Mutex<AgentEventLog>>,
    pub(crate) reaped: Arc<AtomicBool>,
    pub(crate) dirty: Arc<AtomicBool>,
}

/// (T2) Append a normalized event to the pane's JSONL log (best-effort) and
/// broadcast it to subscribers, in that order so the persisted replay is
/// always at least as complete as what clients saw. Every event is stamped
/// with the pane's monotonic `seq` (contract: per-pane u64 from 1, seeded
/// from the persisted log so it survives respawns/restarts) — the SAME
/// stamped object goes to the log and the broadcast, so the bootstrap replay
/// carries identical objects.
#[cfg(any(unix, windows))]
pub(crate) fn append_and_emit_agent_event(
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
pub(crate) struct AgentEventLog {
    pub(crate) log: Option<AgentLogWriter>,
    pub(crate) next_seq: u64,
    /// An escape sequence the last `text_delta` ended inside, carried into
    /// the next one so it is stripped whole instead of leaking its tail.
    pub(crate) pending_escape: String,
}

#[cfg(any(unix, windows))]
impl AgentEventLog {
    pub(crate) fn emit(&mut self, router: &OutputRouter, pane_id: &str, mut event: Value) {
        // No emulator stands between an agent pane and the person, so the
        // chat text is scrubbed here, before it is logged or shown: escape
        // sequences and controls go, invisible and reordering characters go,
        // and what mattered counts against the pane exactly as a shell
        // pane's output would (ledger `output.suspicious`, badge).
        let (found, sample) = scrub_agent_event(&mut event, &mut self.pending_escape);
        if found.total() > 0 {
            router.note_output_tricks(pane_id, found, sample);
        }
        if matches!(
            event.get("kind").and_then(Value::as_str),
            Some("message_complete") | Some("turn_complete")
        ) {
            // A sequence still open when the message ends never completes.
            self.pending_escape.clear();
        }
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
pub(crate) fn agent_child_exited(child: &Arc<Mutex<std::process::Child>>) -> bool {
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
pub(crate) fn agent_handle_permission_request(
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
pub(crate) fn agent_reader_main(ctx: AgentReaderCtx) {
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
pub(crate) type AgentSessionHandles = (
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
    pub(crate) fn plan_agent_spawn(&self, pane_id: &str) -> AgentSpawnPlan {
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
    pub(crate) fn commit_agent_spawn(&mut self, pane_id: &str, prepared: PreparedAgentSpawn) {
        let PreparedAgentSpawn {
            backend,
            child,
            #[cfg(windows)]
            job,
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
            pending_escape: String::new(),
        }));
        let input = spawn_input_writer_raw(Box::new(stdin), None);
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
        let killer = AgentChildKiller { child, job };
        self.agent_sessions.insert(
            pane_id.to_string(),
            AgentSession {
                backend,
                permission_mode: self.agent_config.permission_mode.clone(),
                input,
                killer,
                shared,
                events,
            },
        );
    }

    /// The permission mode each live agent session was started with.
    pub(crate) fn agent_session_modes(&self) -> HashMap<String, String> {
        self.agent_sessions
            .iter()
            .map(|(pane_id, session)| (pane_id.clone(), session.permission_mode.clone()))
            .collect()
    }

    /// (T2) The live session's stdin handle + shared state for request
    /// handlers (None when the pane has no committed agent session).
    pub(crate) fn agent_session_handles(&self, pane_id: &str) -> Option<AgentSessionHandles> {
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
    pub(crate) fn agent_session_id(&self, pane_id: &str) -> Option<String> {
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
    pub(crate) fn emit_agent_event(&self, pane_id: &str, event: Value) {
        if self.is_closed(pane_id) {
            return;
        }
        self.broadcast(&DaemonEvent::AgentEvent {
            pane_id: pane_id.to_string(),
            event,
        });
    }
}

pub(crate) fn drain_complete_utf8(pending: &mut Vec<u8>) -> Vec<String> {
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
pub(crate) struct DaemonClient {
    pub(crate) cwd: PathBuf,
    pub(crate) socket_path: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) token: String,
    /// Whether ensure_daemon may spawn a daemon. Read-only ctl commands connect with
    /// this off so `ctl panes` for a typo'd workspace can't create dirs and daemons.
    pub(crate) auto_spawn: bool,
}

impl DaemonClient {
    pub(crate) fn connect_or_spawn(cwd: PathBuf) -> Result<Self, String> {
        let client = Self::new(cwd)?;
        client.ensure_daemon()?;
        Ok(client)
    }

    /// Connect to an already-running daemon, with no side effects: never spawns a
    /// daemon, never creates data dirs or tokens.
    pub(crate) fn connect_existing(cwd: PathBuf) -> Result<Self, String> {
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

    pub(crate) fn new(cwd: PathBuf) -> Result<Self, String> {
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

    pub(crate) fn ensure_daemon(&self) -> Result<(), String> {
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

    pub(crate) fn request<T: DeserializeOwned>(&self, request: DaemonRequest) -> Result<T, String> {
        self.request_with_timeout(request, Some(CLIENT_READ_TIMEOUT))
    }

    /// Like `request`, but arms the connection/handshake/response read deadline to
    /// `timeout` (or the default client timeout when `None` is not used — pass
    /// `Some(remaining)` to bound a caller-owned overall deadline).
    pub(crate) fn request_with_timeout<T: DeserializeOwned>(
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

    pub(crate) fn raw_request(&self, request: DaemonRequest) -> Result<IpcResponse, String> {
        self.raw_request_with_timeout(request, Some(CLIENT_READ_TIMEOUT))
    }

    pub(crate) fn raw_request_with_timeout(
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
    pub(crate) fn connect(&self) -> Result<DaemonConnection, String> {
        self.connect_with_timeout(Some(CLIENT_READ_TIMEOUT))
    }

    pub(crate) fn connect_with_timeout(
        &self,
        timeout: Option<Duration>,
    ) -> Result<DaemonConnection, String> {
        DaemonConnection::connect_with_timeout(&self.socket_path, &self.token, timeout)
    }

    /// Test-only low-level v1 handshake helper. The socket-based `TestDaemon` suite
    /// drives Subscribe/Ping/Shutdown over the LEGACY newline path through this, so
    /// the full `cargo test` run exercises BOTH wires at once (negotiated v2 via
    /// `connect`, newline v1 here) — the standing old-client↔new-daemon regression
    /// proof for Invariant 8.
    #[cfg(test)]
    pub(crate) fn authenticated_stream(&self) -> Result<TransportStream, String> {
        authenticate_stream_at(&self.socket_path, &self.token)
    }

    pub(crate) fn start_subscription(&self, app: AppHandle) {
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
pub(crate) struct SubscriptionBackoff {
    pub(crate) previous_session: Option<Duration>,
}

impl SubscriptionBackoff {
    pub(crate) fn new() -> Self {
        Self {
            previous_session: None,
        }
    }

    /// Delay to apply BEFORE the next connect attempt.
    pub(crate) fn pre_connect_delay(&self) -> Duration {
        match self.previous_session {
            Some(duration) if duration < SUBSCRIPTION_HEALTHY_MIN => SUBSCRIPTION_RECONNECT_BACKOFF,
            _ => Duration::ZERO,
        }
    }

    /// Record the ended session's duration (`None` = the connect itself failed;
    /// the loop's own connect-failure sleep covers that case).
    pub(crate) fn session_ended(&mut self, duration: Option<Duration>) {
        self.previous_session = duration;
    }
}
