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
#[cfg(any(unix, windows))]
pub(crate) const SGIAN_DROID_BIN_ENV: &str = "SGIAN_DROID_BIN";
#[cfg(any(unix, windows))]
pub(crate) const SGIAN_CODEX_BIN_ENV: &str = "SGIAN_CODEX_BIN";
#[cfg(any(unix, windows))]
pub(crate) const SGIAN_GEMINI_BIN_ENV: &str = "SGIAN_GEMINI_BIN";
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
    pub(crate) codex_bin: Option<String>,
    pub(crate) gemini_bin: Option<String>,
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

/// Provider protocol adapter seam for chat-native agent panes (`ENHANCEMENTS.md` §6).
///
/// Consolidates every provider-specific wire decision — binary resolution, CLI
/// argv and initial stdin handshake, user-message/approval/interrupt request
/// framing, immediate non-blocking protocol replies, and stdout event
/// normalization — into one self-contained table per backend.
#[cfg(any(unix, windows))]
pub(crate) type PerTurnSpawnArgsFn = fn(
    cwd: &Path,
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String>;

#[cfg(any(unix, windows))]
pub(crate) struct AgentBackendAdapter {
    pub(crate) kind: AgentBackendKind,
    pub(crate) default_bin: &'static str,
    pub(crate) env_override: &'static str,
    /// True when the CLI runs one OS process per user turn (`codex exec --json`,
    /// `gemini --output-format stream-json`), reading the prompt from stdin
    /// until EOF and resuming Turn 2+ via `--resume` / `exec resume`.
    pub(crate) per_turn_process: bool,
    /// True when the CLI emits an initial idle notification before any user
    /// turn is sent (Droid JSON-RPC), which must not surface an empty
    /// `turn_complete` event.
    pub(crate) suppress_idle_turn_complete: bool,
    pub(crate) configured_bin: fn(&AgentSpawnConfig) -> Option<&str>,
    /// Effective permission/sandbox mode surfaced on `AgentPaneInfo.mode` (and
    /// evaluated by `is_unattended_mode`) when spawned under `permission_mode`.
    pub(crate) effective_permission_mode: fn(&str) -> &'static str,
    pub(crate) spawn_args: fn(
        permission_mode: &str,
        model: Option<&str>,
        resume_session_id: Option<&str>,
    ) -> Vec<String>,
    /// Optional cwd-aware argv builder for per-turn providers (Codex pins
    /// `sandbox_workspace_write.writable_roots=[cwd]` when writable).
    pub(crate) per_turn_spawn_args: Option<PerTurnSpawnArgsFn>,
    pub(crate) initial_stdin: fn(
        pane_id: &str,
        cwd: &Path,
        model: Option<&str>,
        resume_session_id: Option<&str>,
    ) -> Option<String>,
    pub(crate) encode_user_message: fn(seq: u64, text: &str) -> Value,
    pub(crate) encode_approval_response: fn(
        request_id: &str,
        tool_input: Option<Value>,
        allow: bool,
        message: Option<String>,
    ) -> Value,
    pub(crate) encode_interrupt_request: fn(seq: u64) -> Value,
    pub(crate) immediate_reply: fn(raw: &Value) -> Option<Value>,
    pub(crate) normalize_event: fn(raw: &Value) -> Vec<Value>,
}

#[cfg(any(unix, windows))]
fn claude_configured_bin(config: &AgentSpawnConfig) -> Option<&str> {
    config.claude_bin.as_deref()
}

#[cfg(any(unix, windows))]
fn claude_spawn_args(
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--permission-mode".to_string(),
        permission_mode.to_string(),
        // Without a prompt tool, manual mode auto-DENIES approval-needing tools
        // instead of prompting.
        "--permission-prompt-tool".to_string(),
        "stdio".to_string(),
        "--include-partial-messages".to_string(),
    ];
    if let Some(model) = model.filter(|m| is_safe_cli_token(m)) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(session_id) = resume_session_id.filter(|id| is_safe_cli_token(id)) {
        args.push("--resume".to_string());
        args.push(session_id.to_string());
    }
    args
}

#[cfg(any(unix, windows))]
fn claude_initial_stdin(
    _pane_id: &str,
    _cwd: &Path,
    _model: Option<&str>,
    _resume_session_id: Option<&str>,
) -> Option<String> {
    None
}

#[cfg(any(unix, windows))]
fn claude_encode_user_message(_seq: u64, text: &str) -> Value {
    json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
}

#[cfg(any(unix, windows))]
fn claude_encode_approval_response(
    request_id: &str,
    tool_input: Option<Value>,
    allow: bool,
    message: Option<String>,
) -> Value {
    if allow {
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
    } else {
        json!({
            "type": "control_response",
            "response": {
                "request_id": request_id,
                "subtype": "success",
                "response": {
                    "behavior": "deny",
                    "message": message.unwrap_or_else(|| "denied by user".to_string()),
                },
            },
        })
    }
}

#[cfg(any(unix, windows))]
fn claude_encode_interrupt_request(seq: u64) -> Value {
    json!({
        "type": "control_request",
        "request_id": format!("sgian-interrupt-{seq}"),
        "request": {"subtype": "interrupt"},
    })
}

#[cfg(any(unix, windows))]
fn claude_immediate_reply(_raw: &Value) -> Option<Value> {
    None
}

#[cfg(any(unix, windows))]
fn droid_configured_bin(config: &AgentSpawnConfig) -> Option<&str> {
    config.droid_bin.as_deref()
}

#[cfg(any(unix, windows))]
fn droid_spawn_args(
    _permission_mode: &str,
    _model: Option<&str>,
    _resume_session_id: Option<&str>,
) -> Vec<String> {
    vec![
        "exec".to_string(),
        "--input-format".to_string(),
        "stream-jsonrpc".to_string(),
        "--output-format".to_string(),
        "stream-jsonrpc".to_string(),
    ]
}

#[cfg(any(unix, windows))]
fn droid_initial_stdin(
    pane_id: &str,
    cwd: &Path,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Option<String> {
    let request = if let Some(session_id) = resume_session_id.filter(|id| is_safe_cli_token(id)) {
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
            "cwd": cwd.to_string_lossy(),
            // Manual approval is the safe common denominator. The UI answers
            // droid.request_permission over JSON-RPC.
            "autonomyLevel": "off",
        });
        if let Some(model) = model.filter(|m| is_safe_cli_token(m)) {
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
}

#[cfg(any(unix, windows))]
fn droid_encode_user_message(seq: u64, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "factoryApiVersion": "1.0.0",
        "type": "request",
        "id": format!("sgian-message-{seq}"),
        "method": "droid.add_user_message",
        "params": {"text": text},
    })
}

#[cfg(any(unix, windows))]
fn droid_encode_approval_response(
    request_id: &str,
    _tool_input: Option<Value>,
    allow: bool,
    message: Option<String>,
) -> Value {
    let mut result = json!({
        "selectedOption": if allow { "proceed_once" } else { "cancel" },
    });
    if let Some(message) = message {
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

#[cfg(any(unix, windows))]
fn droid_encode_interrupt_request(seq: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "factoryApiVersion": "1.0.0",
        "type": "request",
        "id": format!("sgian-interrupt-{seq}"),
        "method": "droid.interrupt_session",
        "params": {},
    })
}

#[cfg(any(unix, windows))]
fn droid_immediate_reply(raw: &Value) -> Option<Value> {
    if raw.get("method").and_then(Value::as_str) != Some("droid.ask_user") {
        return None;
    }
    let request_id = raw.get("id").and_then(Value::as_str)?;
    Some(json!({
        "jsonrpc": "2.0",
        "factoryApiVersion": "1.0.0",
        "type": "response",
        "id": request_id,
        "result": {"cancelled": true, "answers": []},
    }))
}

#[cfg(any(unix, windows))]
fn claude_effective_permission_mode(permission_mode: &str) -> &'static str {
    match permission_mode {
        "acceptEdits" => "acceptEdits",
        "auto" => "auto",
        "bypassPermissions" => "bypassPermissions",
        "dontAsk" => "dontAsk",
        "plan" => "plan",
        _ => "manual",
    }
}

#[cfg(any(unix, windows))]
fn droid_effective_permission_mode(_permission_mode: &str) -> &'static str {
    // Droid runs with `autonomyLevel: "off"` and routes tool approvals over JSON-RPC.
    "manual"
}

#[cfg(any(unix, windows))]
fn codex_configured_bin(config: &AgentSpawnConfig) -> Option<&str> {
    config.codex_bin.as_deref()
}

#[cfg(any(unix, windows))]
fn codex_effective_permission_mode(permission_mode: &str) -> &'static str {
    match permission_mode {
        "bypassPermissions" | "bypass" | "yolo" | "danger-full-access" => "danger-full-access",
        "auto" | "dontAsk" | "acceptEdits" | "workspace-write" => "workspace-write",
        "plan" => "plan",
        _ => "read-only",
    }
}

#[cfg(any(unix, windows))]
fn toml_basic_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(any(unix, windows))]
fn codex_build_args(
    cwd: Option<&Path>,
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    let resume = resume_session_id.filter(|id| is_safe_cli_token(id));
    let mut args = vec!["exec".to_string()];
    if resume.is_some() {
        args.push("resume".to_string());
    }
    args.push("--json".to_string());
    args.push("--skip-git-repo-check".to_string());
    match codex_effective_permission_mode(permission_mode) {
        "danger-full-access" => {
            args.push("--dangerously-bypass-approvals-and-sandbox".to_string());
        }
        "workspace-write" => {
            args.push("-c".to_string());
            args.push("sandbox_mode=\"workspace-write\"".to_string());
            if let Some(cwd) = cwd {
                let root = toml_basic_string(&cwd.to_string_lossy());
                args.push("-c".to_string());
                args.push(format!("sandbox_workspace_write.writable_roots=[{root}]"));
            }
        }
        _ => {
            args.push("-c".to_string());
            args.push("sandbox_mode=\"read-only\"".to_string());
        }
    }
    if let Some(model) = model.filter(|m| is_safe_cli_token(m)) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(session_id) = resume {
        args.push(session_id.to_string());
    }
    // Read user prompt from stdin until EOF so prompts never sit on argv / `ps`.
    args.push("-".to_string());
    args
}

#[cfg(any(unix, windows))]
fn codex_spawn_args(
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    codex_build_args(None, permission_mode, model, resume_session_id)
}

#[cfg(any(unix, windows))]
fn codex_per_turn_spawn_args(
    cwd: &Path,
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    codex_build_args(Some(cwd), permission_mode, model, resume_session_id)
}

#[cfg(any(unix, windows))]
fn gemini_configured_bin(config: &AgentSpawnConfig) -> Option<&str> {
    config.gemini_bin.as_deref()
}

#[cfg(any(unix, windows))]
fn gemini_effective_permission_mode(permission_mode: &str) -> &'static str {
    match permission_mode {
        "auto" | "dontAsk" | "bypassPermissions" | "bypass" | "yolo" => "yolo",
        "acceptEdits" | "auto_edit" => "acceptEdits",
        "plan" => "plan",
        _ => "manual",
    }
}

#[cfg(any(unix, windows))]
fn gemini_spawn_args(
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["--output-format".to_string(), "stream-json".to_string()];
    match gemini_effective_permission_mode(permission_mode) {
        "yolo" => {
            args.push("--skip-trust".to_string());
            args.push("--approval-mode".to_string());
            args.push("yolo".to_string());
        }
        "acceptEdits" => {
            args.push("--skip-trust".to_string());
            args.push("--approval-mode".to_string());
            args.push("auto_edit".to_string());
        }
        "plan" => {
            args.push("--skip-trust".to_string());
            args.push("--approval-mode".to_string());
            args.push("plan".to_string());
        }
        _ => {
            args.push("--approval-mode".to_string());
            args.push("default".to_string());
        }
    }
    if let Some(model) = model.filter(|m| is_safe_cli_token(m)) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(session_id) = resume_session_id.filter(|id| is_safe_cli_token(id)) {
        args.push("--resume".to_string());
        args.push(session_id.to_string());
    }
    args
}

#[cfg(any(unix, windows))]
fn gemini_per_turn_spawn_args(
    _cwd: &Path,
    permission_mode: &str,
    model: Option<&str>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    gemini_spawn_args(permission_mode, model, resume_session_id)
}

#[cfg(any(unix, windows))]
fn noop_encode_user_message(_seq: u64, text: &str) -> Value {
    json!({"type": "stdin_prompt", "text": text})
}

#[cfg(any(unix, windows))]
fn noop_encode_approval_response(
    request_id: &str,
    _tool_input: Option<Value>,
    allow: bool,
    message: Option<String>,
) -> Value {
    json!({"request_id": request_id, "allow": allow, "message": message})
}

#[cfg(any(unix, windows))]
fn noop_encode_interrupt_request(seq: u64) -> Value {
    json!({"id": format!("sgian-interrupt-{seq}"), "type": "process_signal"})
}

#[cfg(any(unix, windows))]
pub(crate) static CLAUDE_BACKEND_ADAPTER: AgentBackendAdapter = AgentBackendAdapter {
    kind: AgentBackendKind::Claude,
    default_bin: "claude",
    env_override: SGIAN_CLAUDE_BIN_ENV,
    per_turn_process: false,
    suppress_idle_turn_complete: false,
    configured_bin: claude_configured_bin,
    effective_permission_mode: claude_effective_permission_mode,
    spawn_args: claude_spawn_args,
    per_turn_spawn_args: None,
    initial_stdin: claude_initial_stdin,
    encode_user_message: claude_encode_user_message,
    encode_approval_response: claude_encode_approval_response,
    encode_interrupt_request: claude_encode_interrupt_request,
    immediate_reply: claude_immediate_reply,
    normalize_event: normalize_agent_event,
};

#[cfg(any(unix, windows))]
pub(crate) static DROID_BACKEND_ADAPTER: AgentBackendAdapter = AgentBackendAdapter {
    kind: AgentBackendKind::Droid,
    default_bin: "droid",
    env_override: SGIAN_DROID_BIN_ENV,
    per_turn_process: false,
    suppress_idle_turn_complete: true,
    configured_bin: droid_configured_bin,
    effective_permission_mode: droid_effective_permission_mode,
    spawn_args: droid_spawn_args,
    per_turn_spawn_args: None,
    initial_stdin: droid_initial_stdin,
    encode_user_message: droid_encode_user_message,
    encode_approval_response: droid_encode_approval_response,
    encode_interrupt_request: droid_encode_interrupt_request,
    immediate_reply: droid_immediate_reply,
    normalize_event: normalize_droid_agent_event,
};

#[cfg(any(unix, windows))]
pub(crate) static CODEX_BACKEND_ADAPTER: AgentBackendAdapter = AgentBackendAdapter {
    kind: AgentBackendKind::Codex,
    default_bin: "codex",
    env_override: SGIAN_CODEX_BIN_ENV,
    per_turn_process: true,
    suppress_idle_turn_complete: false,
    configured_bin: codex_configured_bin,
    effective_permission_mode: codex_effective_permission_mode,
    spawn_args: codex_spawn_args,
    per_turn_spawn_args: Some(codex_per_turn_spawn_args),
    initial_stdin: claude_initial_stdin,
    encode_user_message: noop_encode_user_message,
    encode_approval_response: noop_encode_approval_response,
    encode_interrupt_request: noop_encode_interrupt_request,
    immediate_reply: claude_immediate_reply,
    normalize_event: normalize_codex_agent_event,
};

#[cfg(any(unix, windows))]
pub(crate) static GEMINI_BACKEND_ADAPTER: AgentBackendAdapter = AgentBackendAdapter {
    kind: AgentBackendKind::Gemini,
    default_bin: "gemini",
    env_override: SGIAN_GEMINI_BIN_ENV,
    per_turn_process: true,
    suppress_idle_turn_complete: false,
    configured_bin: gemini_configured_bin,
    effective_permission_mode: gemini_effective_permission_mode,
    spawn_args: gemini_spawn_args,
    per_turn_spawn_args: Some(gemini_per_turn_spawn_args),
    initial_stdin: claude_initial_stdin,
    encode_user_message: noop_encode_user_message,
    encode_approval_response: noop_encode_approval_response,
    encode_interrupt_request: noop_encode_interrupt_request,
    immediate_reply: claude_immediate_reply,
    normalize_event: normalize_gemini_agent_event,
};

#[cfg(any(unix, windows))]
pub(crate) fn agent_backend_adapter(backend: AgentBackendKind) -> &'static AgentBackendAdapter {
    match backend {
        AgentBackendKind::Claude => &CLAUDE_BACKEND_ADAPTER,
        AgentBackendKind::Droid => &DROID_BACKEND_ADAPTER,
        AgentBackendKind::Codex => &CODEX_BACKEND_ADAPTER,
        AgentBackendKind::Gemini => &GEMINI_BACKEND_ADAPTER,
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn resolve_provider_bin(
    config: &AgentSpawnConfig,
    backend: AgentBackendKind,
) -> AgentBinPlan {
    let adapter = backend.adapter();
    debug_assert_eq!(adapter.kind, backend);
    let fallback = adapter.default_bin;
    let candidate = (adapter.configured_bin)(config)
        .filter(|bin| !bin.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(adapter.env_override)
                .ok()
                .filter(|bin| !bin.is_empty())
        });
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
    /// Set by `InterruptAgent` on per-turn providers so the turn reader emits
    /// `subtype: "interrupted"` rather than surfacing SIGTERM exit as a crash.
    pub(crate) interrupted: bool,
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
                shared.interrupted = false;
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
/// already reaped. When `process_group` is true (per-turn spawns created with
/// `process_group(0)`), signals target `-pid` so wrapper scripts (`codex.js`)
/// and their vendor binaries terminate together.
#[cfg(unix)]
pub(crate) struct AgentChildKiller {
    pub(crate) pid: u32,
    pub(crate) reaped: Arc<AtomicBool>,
    pub(crate) process_group: bool,
}

#[cfg(unix)]
impl AgentChildKiller {
    pub(crate) fn kill(&self) {
        if self.reaped.load(Ordering::SeqCst) {
            return;
        }
        let target = if self.process_group {
            -(self.pid as libc::pid_t)
        } else {
            self.pid as libc::pid_t
        };
        unsafe {
            libc::kill(target, libc::SIGTERM);
        }
        let reaped = Arc::clone(&self.reaped);
        thread::spawn(move || {
            thread::sleep(AGENT_KILL_GRACE);
            if !reaped.load(Ordering::SeqCst) {
                unsafe {
                    libc::kill(target, libc::SIGKILL);
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

/// (T2) A live agent session: the stdin writer queue (for persistent stdio
/// providers) or active per-turn child killer (for per-turn providers like
/// Codex and Gemini), plus the shared state and conversation log. Dropping it
/// (close/restart/shutdown) denies pending approvals and kills any live CLI.
#[cfg(any(unix, windows))]
pub(crate) struct AgentSession {
    pub(crate) backend: AgentBackendKind,
    pub(crate) generation: u64,
    /// The effective permission mode this CLI session runs under. A config
    /// reload changes future spawns, not this session, and the badge must say
    /// what is actually active (S9 of the 2026-09-20 review).
    pub(crate) permission_mode: String,
    pub(crate) input: Option<SyncSender<Vec<u8>>>,
    pub(crate) killer: Arc<Mutex<Option<AgentChildKiller>>>,
    pub(crate) shared: Arc<Mutex<AgentShared>>,
    pub(crate) events: Arc<Mutex<AgentEventLog>>,
}

#[cfg(any(unix, windows))]
impl AgentSession {
    pub(crate) fn kill(&self) {
        if let Ok(mut guard) = self.killer.lock() {
            if let Some(killer) = guard.take() {
                killer.kill();
            }
        }
    }
}

#[cfg(any(unix, windows))]
impl Drop for AgentSession {
    fn drop(&mut self) {
        deny_agent_pending(&self.shared, "closed");
        self.kill();
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

/// Normalize a provider's wire protocol into the stable chat event contract
/// through its `AgentBackendAdapter`.
#[cfg(any(unix, windows))]
pub(crate) fn normalize_provider_agent_event(backend: AgentBackendKind, raw: &Value) -> Vec<Value> {
    (backend.adapter().normalize_event)(raw)
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

#[cfg(any(unix, windows))]
pub(crate) fn normalize_codex_agent_event(raw: &Value) -> Vec<Value> {
    let Some(event_type) = raw.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match event_type {
        "thread.started" => {
            let mut event = json!({"kind": "session"});
            if let Some(thread_id) = raw.get("thread_id").and_then(Value::as_str) {
                event["session_id"] = json!(thread_id);
            }
            if let Some(model) = raw.get("model").and_then(Value::as_str) {
                event["model"] = json!(model);
            }
            vec![event]
        }
        "item.started" => {
            let Some(item) = raw.get("item") else {
                return Vec::new();
            };
            let id = item.get("id").cloned().unwrap_or(Value::Null);
            match item.get("type").and_then(Value::as_str) {
                Some("command_execution") => vec![json!({
                    "kind": "tool_use",
                    "id": id,
                    "name": "command_execution",
                    "input": {"command": item.get("command").cloned().unwrap_or_else(|| json!(""))},
                })],
                Some("mcp_tool_call") => {
                    let server = item.get("server").and_then(Value::as_str).unwrap_or("");
                    let tool = item
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("mcp_tool");
                    let name = if server.is_empty() {
                        tool.to_string()
                    } else {
                        format!("{server}:{tool}")
                    };
                    vec![json!({
                        "kind": "tool_use",
                        "id": id,
                        "name": name,
                        "input": item.get("arguments").cloned().unwrap_or(Value::Null),
                    })]
                }
                Some("file_change") => vec![json!({
                    "kind": "tool_use",
                    "id": id,
                    "name": "file_change",
                    "input": {"changes": item.get("changes").cloned().unwrap_or_else(|| json!([]))},
                })],
                Some("web_search") => vec![json!({
                    "kind": "tool_use",
                    "id": id,
                    "name": "web_search",
                    "input": {"query": item.get("query").cloned().unwrap_or_else(|| json!(""))},
                })],
                Some("collab_tool_call") => vec![json!({
                    "kind": "tool_use",
                    "id": id,
                    "name": item.get("tool").cloned().unwrap_or_else(|| json!("collab_tool")),
                    "input": {"prompt": item.get("prompt").cloned().unwrap_or(Value::Null)},
                })],
                _ => Vec::new(),
            }
        }
        "item.completed" => {
            let Some(item) = raw.get("item") else {
                return Vec::new();
            };
            let id = item.get("id").cloned().unwrap_or(Value::Null);
            let status = item.get("status").and_then(Value::as_str).unwrap_or("");
            match item.get("type").and_then(Value::as_str) {
                Some("agent_message") => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![
                            json!({"kind": "message_start", "role": "assistant"}),
                            json!({"kind": "text_delta", "text": text}),
                            json!({"kind": "message_complete"}),
                        ]
                    }
                }
                Some("command_execution") => {
                    let exit_code = item.get("exit_code").and_then(Value::as_i64);
                    let is_error = matches!(status, "failed" | "declined")
                        || exit_code.is_some_and(|code| code != 0);
                    vec![json!({
                        "kind": "tool_result",
                        "tool_use_id": id,
                        "content": item.get("aggregated_output").cloned().unwrap_or(Value::Null),
                        "is_error": is_error,
                    })]
                }
                Some("mcp_tool_call") => {
                    let is_error = status == "failed"
                        || item.get("error").is_some_and(|error| !error.is_null());
                    let content = item
                        .pointer("/error/message")
                        .or_else(|| item.pointer("/result/content"))
                        .or_else(|| item.get("result"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    vec![json!({
                        "kind": "tool_result",
                        "tool_use_id": id,
                        "content": content,
                        "is_error": is_error,
                    })]
                }
                Some("file_change") => vec![json!({
                    "kind": "tool_result",
                    "tool_use_id": id,
                    "content": item.get("changes").cloned().unwrap_or_else(|| json!([])),
                    "is_error": status == "failed",
                })],
                Some("web_search") => vec![json!({
                    "kind": "tool_result",
                    "tool_use_id": id,
                    "content": item.get("query").cloned().unwrap_or(Value::Null),
                    "is_error": false,
                })],
                Some("collab_tool_call") => vec![json!({
                    "kind": "tool_result",
                    "tool_use_id": id,
                    "content": item.get("agents_states").cloned().unwrap_or(Value::Null),
                    "is_error": status == "failed",
                })],
                Some("error") => vec![json!({
                    "kind": "error",
                    "message": item.get("message").cloned().unwrap_or_else(|| json!("Codex item error")),
                })],
                _ => Vec::new(),
            }
        }
        "turn.completed" => {
            let mut event = json!({
                "kind": "turn_complete",
                "subtype": "success",
                "is_error": false,
            });
            if let Some(usage) = raw.get("usage") {
                event["usage"] = usage.clone();
            }
            vec![event]
        }
        "turn.failed" => {
            let message = raw
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Codex turn failed");
            vec![
                json!({"kind": "error", "message": message}),
                json!({"kind": "turn_complete", "subtype": "error_during_execution", "is_error": true}),
            ]
        }
        "error" => {
            let message = raw
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Codex error");
            vec![json!({"kind": "error", "message": message})]
        }
        _ => Vec::new(),
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn normalize_gemini_agent_event(raw: &Value) -> Vec<Value> {
    let Some(event_type) = raw.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    match event_type {
        "init" => {
            let mut event = json!({"kind": "session"});
            if let Some(session_id) = raw.get("session_id").and_then(Value::as_str) {
                event["session_id"] = json!(session_id);
            }
            if let Some(model) = raw.get("model").and_then(Value::as_str) {
                event["model"] = json!(model);
            }
            vec![event]
        }
        "message" => {
            if raw.get("role").and_then(Value::as_str) != Some("assistant") {
                return Vec::new();
            }
            let content = raw.get("content").and_then(Value::as_str).unwrap_or("");
            if content.is_empty() {
                return Vec::new();
            }
            if raw.get("delta").and_then(Value::as_bool) == Some(true) {
                vec![json!({"kind": "text_delta", "text": content})]
            } else {
                vec![
                    json!({"kind": "message_start", "role": "assistant"}),
                    json!({"kind": "text_delta", "text": content}),
                    json!({"kind": "message_complete"}),
                ]
            }
        }
        "tool_use" => vec![json!({
            "kind": "tool_use",
            "id": raw.get("tool_id").cloned().unwrap_or(Value::Null),
            "name": raw.get("tool_name").cloned().unwrap_or_else(|| json!("tool")),
            "input": raw.get("parameters").cloned().unwrap_or(Value::Null),
        })],
        "tool_result" => {
            let is_error = raw.get("status").and_then(Value::as_str) == Some("error");
            let content = if is_error {
                raw.pointer("/error/message")
                    .or_else(|| raw.get("output"))
                    .cloned()
                    .unwrap_or_else(|| json!("Tool error"))
            } else {
                raw.get("output").cloned().unwrap_or(Value::Null)
            };
            vec![json!({
                "kind": "tool_result",
                "tool_use_id": raw.get("tool_id").cloned().unwrap_or(Value::Null),
                "content": content,
                "is_error": is_error,
            })]
        }
        "error" => {
            let message = raw
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Gemini error");
            vec![json!({"kind": "error", "message": message})]
        }
        "result" => {
            let is_error = raw.get("status").and_then(Value::as_str) == Some("error");
            let mut out = Vec::new();
            if is_error {
                if let Some(message) = raw.pointer("/error/message").and_then(Value::as_str) {
                    out.push(json!({"kind": "error", "message": message}));
                }
            }
            let mut complete = json!({
                "kind": "turn_complete",
                "subtype": if is_error { "error_during_execution" } else { "success" },
                "is_error": is_error,
            });
            if let Some(stats) = raw.get("stats") {
                if let Some(duration_ms) = stats.get("duration_ms").and_then(Value::as_u64) {
                    complete["duration_ms"] = json!(duration_ms);
                }
                complete["usage"] = stats.clone();
            }
            out.push(complete);
            out
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
    let response = (backend.adapter().encode_approval_response)(
        &request_id,
        tool_input,
        decision.allow,
        decision.message,
    );
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
        let adapter = backend.adapter();
        if let Some(response) = (adapter.immediate_reply)(&raw) {
            if let Ok(line) = serde_json::to_string(&response) {
                let _ = queue_agent_stdin(&input, &pane_id, &line);
            }
        }
        for event in normalize_provider_agent_event(backend, &raw) {
            match event.get("kind").and_then(Value::as_str).unwrap_or("") {
                "session" => {
                    if let Some(session_id) = event
                        .get("session_id")
                        .and_then(Value::as_str)
                        .filter(|id| is_safe_cli_token(id))
                    {
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
                    // Providers with `suppress_idle_turn_complete` (Droid) emit
                    // an idle notification after initialization before any turn
                    // ran; suppress that empty replay event while preserving
                    // real in-flight turn boundaries.
                    let was_running = agent_end_turn(&shared);
                    if adapter.suppress_idle_turn_complete && !was_running {
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

/// Reject CLI flag injection (`--...` or `-...`), whitespace, and control bytes
/// in tokens passed as positional values to `--model` or `--resume` (L-01).
pub(crate) fn is_safe_cli_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TITLE_CHARS
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@' | '/'))
}

#[cfg(any(unix, windows))]
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (meta.permissions().mode() & 0o111) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Validate that a per-turn provider's CLI binary resolves to an executable
/// file without spawning an idle process at pane creation / bootstrap time.
#[cfg(any(unix, windows))]
pub(crate) fn verify_agent_bin_exists(plan: &AgentSpawnPlan) -> Result<(), String> {
    let provider = plan.backend.as_str();
    match &plan.bin {
        AgentBinPlan::NotFound => Err(format!(
            "{provider} CLI not found on PATH (configure its agent binary override)"
        )),
        AgentBinPlan::ViaCmd(script) => {
            if script.is_file() {
                Ok(())
            } else {
                Err(format!(
                    "{provider} CLI not found: '{}' is not executable or not on PATH",
                    script.display()
                ))
            }
        }
        AgentBinPlan::Direct(bin) => {
            if bin.contains('/') || bin.contains('\\') {
                if is_executable_file(Path::new(bin)) {
                    Ok(())
                } else {
                    Err(format!(
                        "{provider} CLI not found: '{bin}' is not executable or not on PATH"
                    ))
                }
            } else {
                let effective_path = if let Some(explicit) = plan.env.get("PATH") {
                    Some(std::ffi::OsString::from(explicit))
                } else if plan.scrub_env.iter().any(|k| k == "PATH") {
                    None
                } else {
                    std::env::var_os("PATH")
                };
                let found = effective_path.is_some_and(|path_var| {
                    std::env::split_paths(&path_var).any(|dir| is_executable_file(&dir.join(bin)))
                });
                if found {
                    Ok(())
                } else {
                    Err(format!(
                        "{provider} CLI not found: '{bin}' is not executable or not on PATH"
                    ))
                }
            }
        }
    }
}

#[cfg(any(unix, windows))]
pub(crate) struct PreparedPerTurnSpawn {
    pub(crate) backend: AgentBackendKind,
    pub(crate) child: std::process::Child,
    #[cfg(windows)]
    pub(crate) job: KillOnCloseJob,
    pub(crate) stdout: std::process::ChildStdout,
    pub(crate) stderr: std::process::ChildStderr,
}

/// Spawn one turn of a per-turn provider (`codex exec`, `gemini`), streaming
/// `prompt` into `stdin` on a helper thread and closing `stdin` (`EOF`)
/// immediately so even 256 KiB prompts never block pipe buffers.
#[cfg(any(unix, windows))]
pub(crate) fn execute_per_turn_agent_spawn(
    plan: &AgentSpawnPlan,
    prompt: &str,
) -> Result<PreparedPerTurnSpawn, String> {
    let provider = plan.backend.as_str();
    let Some((program, argv)) = agent_command_argv(&plan.bin, &plan.args) else {
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
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
    #[cfg(windows)]
    let spawned = KillOnCloseJob::spawn(&mut command);
    #[cfg(unix)]
    let spawned = command.spawn();
    let spawned = spawned.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            format!("{provider} CLI not found: '{program}' is not executable or not on PATH")
        } else {
            format!("failed to spawn agent CLI '{program}': {error}")
        }
    })?;
    #[cfg(windows)]
    let (mut child, job) = spawned;
    #[cfg(unix)]
    let mut child = spawned;
    let (mut stdin, stdout, stderr) =
        match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(stdin), Some(stdout), Some(stderr)) => (stdin, stdout, stderr),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("failed to open agent CLI pipes".to_string());
            }
        };
    let mut payload = prompt.as_bytes().to_vec();
    if !payload.ends_with(b"\n") {
        payload.push(b'\n');
    }
    thread::spawn(move || {
        let _ = std::io::Write::write_all(&mut stdin, &payload);
        let _ = std::io::Write::flush(&mut stdin);
        // Dropping `stdin` closes the pipe so the per-turn CLI sees EOF.
    });
    Ok(PreparedPerTurnSpawn {
        backend: plan.backend,
        child,
        #[cfg(windows)]
        job,
        stdout,
        stderr,
    })
}

#[cfg(any(unix, windows))]
struct PerTurnReaderCtx {
    pane_id: String,
    backend: AgentBackendKind,
    generation: u64,
    child: Arc<Mutex<std::process::Child>>,
    stdout: std::process::ChildStdout,
    stderr_handle: thread::JoinHandle<String>,
    killer_slot: Arc<Mutex<Option<AgentChildKiller>>>,
    shared: Arc<Mutex<AgentShared>>,
    liveness: Arc<Mutex<HashMap<String, PaneLiveness>>>,
    router: OutputRouter,
    events: Arc<Mutex<AgentEventLog>>,
    reaped: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
}

#[cfg(any(unix, windows))]
fn per_turn_agent_reader_main(ctx: PerTurnReaderCtx) {
    let PerTurnReaderCtx {
        pane_id,
        backend,
        generation,
        child,
        stdout,
        stderr_handle,
        killer_slot,
        shared,
        liveness,
        router,
        events,
        reaped,
        dirty,
    } = ctx;

    let owns_session = || {
        liveness
            .lock()
            .ok()
            .map(|map| {
                map.get(&pane_id)
                    .is_some_and(|entry| entry.generation == generation && !entry.ended)
            })
            .unwrap_or(false)
    };

    let mut saw_turn_complete = false;
    let mut saw_error_event = false;
    let mut reader = BufReader::new(stdout);
    let mut buffer = Vec::new();
    loop {
        let truncated = match read_capped_line(&mut reader, &mut buffer, AGENT_OUTPUT_LINE_MAX) {
            Ok(Some(truncated)) => truncated,
            Ok(None) => break,
            Err(_) => break,
        };
        if !owns_session() {
            if let Ok(mut child) = child.lock() {
                let _ = child.wait();
            }
            reaped.store(true, Ordering::SeqCst);
            let _ = stderr_handle.join();
            return;
        }
        if truncated {
            saw_error_event = true;
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
        for event in normalize_provider_agent_event(backend, &raw) {
            match event.get("kind").and_then(Value::as_str).unwrap_or("") {
                "session" => {
                    if let Some(session_id) = event
                        .get("session_id")
                        .and_then(Value::as_str)
                        .filter(|id| is_safe_cli_token(id))
                    {
                        let mut changed = false;
                        if let Ok(mut shared) = shared.lock() {
                            if shared.session_id.as_deref() != Some(session_id) {
                                shared.session_id = Some(session_id.to_string());
                                changed = true;
                            }
                        }
                        if changed {
                            dirty.store(true, Ordering::SeqCst);
                        }
                    }
                }
                "error" => {
                    saw_error_event = true;
                }
                "turn_complete" => {
                    saw_turn_complete = true;
                    agent_end_turn(&shared);
                }
                _ => {}
            }
            append_and_emit_agent_event(&router, &events, &pane_id, event);
        }
    }

    let exit_code = child
        .lock()
        .ok()
        .and_then(|mut child| child.wait().ok())
        .and_then(|status| status.code());
    reaped.store(true, Ordering::SeqCst);
    if let Ok(mut guard) = killer_slot.lock() {
        *guard = None;
    }
    let stderr_tail = stderr_handle.join().unwrap_or_default();
    let was_interrupted = shared
        .lock()
        .map(|mut s| {
            s.turn_running = false;
            std::mem::replace(&mut s.interrupted, false)
        })
        .unwrap_or(false);

    if !owns_session() {
        return;
    }

    if !saw_turn_complete {
        if was_interrupted {
            append_and_emit_agent_event(
                &router,
                &events,
                &pane_id,
                json!({
                    "kind": "turn_complete",
                    "subtype": "interrupted",
                    "is_error": true,
                }),
            );
        } else {
            let trimmed_stderr = stderr_tail.trim();
            if !saw_error_event {
                let message = if !trimmed_stderr.is_empty() {
                    trimmed_stderr.to_string()
                } else if let Some(code) = exit_code {
                    format!("{} CLI exited with code {code}", backend.as_str())
                } else {
                    format!("{} CLI terminated before completing turn", backend.as_str())
                };
                append_and_emit_agent_event(
                    &router,
                    &events,
                    &pane_id,
                    json!({"kind": "error", "message": message}),
                );
            }
            append_and_emit_agent_event(
                &router,
                &events,
                &pane_id,
                json!({
                    "kind": "turn_complete",
                    "subtype": "error_during_execution",
                    "is_error": true,
                }),
            );
        }
    }
}

#[cfg(any(unix, windows))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_per_turn_agent_workers(
    pane_id: String,
    generation: u64,
    prepared: PreparedPerTurnSpawn,
    killer_slot: Arc<Mutex<Option<AgentChildKiller>>>,
    shared: Arc<Mutex<AgentShared>>,
    liveness: Arc<Mutex<HashMap<String, PaneLiveness>>>,
    router: OutputRouter,
    events: Arc<Mutex<AgentEventLog>>,
    dirty: Arc<AtomicBool>,
) {
    let PreparedPerTurnSpawn {
        backend,
        child,
        #[cfg(windows)]
        job,
        stdout,
        stderr,
    } = prepared;

    #[cfg(unix)]
    let pid = child.id();
    let reaped = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(child));

    #[cfg(unix)]
    let turn_killer = AgentChildKiller {
        pid,
        reaped: Arc::clone(&reaped),
        process_group: true,
    };
    #[cfg(windows)]
    let turn_killer = AgentChildKiller {
        child: Arc::clone(&child),
        job,
    };
    if let Ok(mut guard) = killer_slot.lock() {
        *guard = Some(turn_killer);
    }

    let stderr_router = router.clone();
    let stderr_pane_id = pane_id.clone();
    let stderr_handle = thread::spawn(move || {
        let _log_guard = stderr_router.log_guard();
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        let mut tail = String::new();
        while let Ok(Some(truncated)) =
            read_capped_line(&mut reader, &mut line, AGENT_OUTPUT_LINE_MAX)
        {
            let text = String::from_utf8_lossy(&line);
            let trimmed = text.trim_end();
            tracing::warn!(
                pane_id = %stderr_pane_id,
                event = "agent_stderr",
                truncated,
                "agent CLI stderr: {}",
                trimmed
            );
            if !trimmed.is_empty() {
                if !tail.is_empty() {
                    tail.push('\n');
                }
                tail.push_str(trimmed);
                if tail.chars().count() > 1024 {
                    let chars: Vec<char> = tail.chars().collect();
                    tail = chars[chars.len() - 1024..].iter().collect();
                }
            }
        }
        tail
    });

    let ctx = PerTurnReaderCtx {
        pane_id,
        backend,
        generation,
        child,
        stdout,
        stderr_handle,
        killer_slot,
        shared,
        liveness,
        router: router.clone(),
        events,
        reaped,
        dirty,
    };
    thread::spawn(move || {
        let _log_guard = router.log_guard();
        per_turn_agent_reader_main(ctx);
    });
}

/// (T2) A live session's handles, cloned out from under the store lock for
/// request handlers.
#[cfg(any(unix, windows))]
pub(crate) type AgentSessionHandles = (
    AgentBackendKind,
    u64,
    Option<SyncSender<Vec<u8>>>,
    Arc<Mutex<Option<AgentChildKiller>>>,
    Arc<Mutex<AgentShared>>,
    Arc<Mutex<AgentEventLog>>,
);

#[cfg(any(unix, windows))]
impl TerminalStore {
    /// (T2) The cheap half of an agent spawn, read under the store lock (M7).
    /// The CLI session to resume comes from the previous (or active per-turn)
    /// session when there is one, else the persisted `agents_v2` seed.
    pub(crate) fn plan_agent_spawn(&self, pane_id: &str) -> AgentSpawnPlan {
        let spec = self.agent_specs.get(pane_id).cloned().unwrap_or_default();
        let adapter = spec.backend.adapter();
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
            .or_else(|| self.agent_resume.get(pane_id).cloned())
            .filter(|id| is_safe_cli_token(id));
        let args = if let Some(per_turn_args) = adapter.per_turn_spawn_args {
            per_turn_args(
                &self.cwd,
                &self.agent_config.permission_mode,
                spec.model.as_deref(),
                resume_session_id.as_deref(),
            )
        } else {
            (adapter.spawn_args)(
                &self.agent_config.permission_mode,
                spec.model.as_deref(),
                resume_session_id.as_deref(),
            )
        };
        let command_str = format!("{} {}", agent_bin_display(&bin), args.join(" "));
        let initial_input = (adapter.initial_stdin)(
            pane_id,
            &self.cwd,
            spec.model.as_deref(),
            resume_session_id.as_deref(),
        );
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

    /// Commit a logical session for a per-turn provider (`codex`, `gemini`):
    /// bumps generation, marks the pane live, seeds `session_id` from any
    /// persisted `agent_resume` entry, and opens the durable event log without
    /// spawning an idle OS process before the first user prompt.
    pub(crate) fn commit_logical_agent_session(&mut self, pane_id: &str, plan: &AgentSpawnPlan) {
        self.next_generation += 1;
        let generation = self.next_generation;
        if let Ok(mut liveness) = self.liveness.lock() {
            liveness.insert(
                pane_id.to_string(),
                PaneLiveness {
                    generation,
                    ended: false,
                    command: Some(plan.command_str.clone()),
                    cwd: Some(plan.cwd_str.clone()),
                    exit_code: None,
                    exit_signal: None,
                    pid: None,
                },
            );
        }
        let resume_seed = self
            .agent_resume
            .get(pane_id)
            .cloned()
            .filter(|id| is_safe_cli_token(id));
        let shared = Arc::new(Mutex::new(AgentShared {
            session_id: resume_seed,
            ..Default::default()
        }));
        let events = Arc::new(Mutex::new(AgentEventLog {
            log: AgentLogWriter::open(&self.agents_dir, pane_id),
            next_seq: agent_log_last_seq(&self.agents_dir, pane_id),
            pending_escape: String::new(),
        }));
        let effective_mode =
            (plan.backend.adapter().effective_permission_mode)(&self.agent_config.permission_mode)
                .to_string();
        self.agent_sessions.insert(
            pane_id.to_string(),
            AgentSession {
                backend: plan.backend,
                generation,
                permission_mode: effective_mode,
                input: None,
                killer: Arc::new(Mutex::new(None)),
                shared,
                events,
            },
        );
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
                    exit_signal: None,
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
        let killer = AgentChildKiller {
            pid,
            reaped,
            process_group: false,
        };
        #[cfg(windows)]
        let killer = AgentChildKiller { child, job };
        let effective_mode =
            (backend.adapter().effective_permission_mode)(&self.agent_config.permission_mode)
                .to_string();
        self.agent_sessions.insert(
            pane_id.to_string(),
            AgentSession {
                backend,
                generation,
                permission_mode: effective_mode,
                input: Some(input),
                killer: Arc::new(Mutex::new(Some(killer))),
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

    /// Live attention classification for a `PaneKind::Agent` session (`None`
    /// once the session has ended or before its first spawn).
    pub(crate) fn agent_session_attention(&self, pane_id: &str) -> Option<AgentAttention> {
        if !self.is_live(pane_id) {
            return None;
        }
        let session = self.agent_sessions.get(pane_id)?;
        let shared = session.shared.lock().ok()?;
        if !shared.pending.is_empty() {
            Some(AgentAttention::NeedsInput)
        } else if shared.turn_running {
            Some(AgentAttention::Working)
        } else {
            Some(AgentAttention::Idle)
        }
    }

    /// (T2) The live session's stdin handle + shared state for request
    /// handlers (None when the pane has no committed agent session).
    pub(crate) fn agent_session_handles(&self, pane_id: &str) -> Option<AgentSessionHandles> {
        self.agent_sessions.get(pane_id).map(|session| {
            (
                session.backend,
                session.generation,
                session.input.clone(),
                Arc::clone(&session.killer),
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
    /// Explicit per-client credential (e.g. from `<app_support>/machines/<name>.token`).
    pub(crate) client_token: Option<String>,
    /// Optional loopback TCP endpoint (`127.0.0.1:<port>`) used for local SSH
    /// tunnel forwarding on Windows (#114) and loopback attach tests (#116).
    pub(crate) tcp_endpoint: Option<std::net::SocketAddr>,
}

impl DaemonClient {
    pub(crate) fn has_client_token(&self) -> bool {
        self.client_token.is_some() || client_token_from_env().is_some()
    }
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
            client_token: None,
            tcp_endpoint: None,
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
            client_token: None,
            tcp_endpoint: None,
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
        DaemonConnection::connect_with_options(
            &self.socket_path,
            self.tcp_endpoint,
            &self.token,
            self.client_token.as_deref(),
            timeout,
        )
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
        let tcp_endpoint = self.tcp_endpoint;
        let token = self.token.clone();
        let client_token = self.client_token.clone();
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
                match DaemonConnection::connect_with_options(
                    &socket_path,
                    tcp_endpoint,
                    &token,
                    client_token.as_deref(),
                    Some(CLIENT_READ_TIMEOUT),
                ) {
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
