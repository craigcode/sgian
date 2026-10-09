use super::*;

// ---------------------------------------------------------------------------
// Official agent probe (M3b): `claude agents --json` mapped to panes through
// the process tree. Pure parts here; the loop lives on DaemonServer.
// ---------------------------------------------------------------------------

/// One entry of `claude agents --json`. Only the fields the probe reads;
/// unknown fields are ignored so newer CLIs keep parsing.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct AgentProbeEntry {
    #[serde(default)]
    pub(crate) pid: Option<u32>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default, rename = "waitingFor")]
    pub(crate) waiting_for: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<String>,
}

/// Map Claude Code's vocabulary (`status`: busy/waiting/idle; `waitingFor`
/// when it needs a person; `state`: working/blocked/done/failed/stopped) onto
/// the pane attention states. `None` = nothing to say.
pub(crate) fn attention_from_probe(entry: &AgentProbeEntry) -> Option<AgentAttention> {
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
pub(crate) struct ProcessTable {
    pub(crate) parent: HashMap<u32, u32>,
    pub(crate) args: HashMap<u32, String>,
}

pub(crate) fn parse_process_table(text: &str) -> ProcessTable {
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
pub(crate) fn is_kranz_worker_command(args: &str) -> bool {
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
pub(crate) fn find_kranz_panes(
    table: &ProcessTable,
    pane_pids: &[(String, u32)],
) -> HashMap<String, u32> {
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
pub(crate) fn kranz_attention_from_state(state: &Value) -> Option<AgentAttention> {
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

pub(crate) const MAX_PROJECTS: usize = 64;
pub(crate) const PROJECT_NAME_MAX_LEN: usize = 64;
pub(crate) const PROJECT_LEDGER_DEFAULT_LIMIT: usize = 50;
pub(crate) const PROJECT_LEDGER_MAX_LIMIT: usize = 500;
/// Scrollback lines per pane in a dossier when the caller does not say.
pub(crate) const PROJECT_DOSSIER_DEFAULT_LINES: usize = 40;
/// The dossier document's format tag; bump when a consumer could misread it.
pub(crate) const PROJECT_DOSSIER_FORMAT: &str = "sgian.dossier.v1";

/// The ledger a project's own records (notes) chain in: `project-<name>`,
/// beside the pane ledgers and disjoint from their `pane-N` ids.
pub(crate) fn project_ledger_key(name: &str) -> String {
    format!("project-{name}")
}

/// Project names are keys and appear in ledgers and shell output: short,
/// `[A-Za-z0-9._-]`, no leading dot.
pub(crate) fn validate_project_name(raw: &str) -> Result<String, String> {
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

pub(crate) fn project_rollup(
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
pub(crate) fn pane_for_pid(
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
pub(crate) const HOOK_ATTENTION_TTL: Duration = Duration::from_secs(20);

/// Map a Claude Code hook onto pane attention. `event` is the payload's
/// `hook_event_name`; `notification_type` the Notification kind. `None` =
/// nothing to say (the hook is acknowledged and ignored).
pub(crate) fn attention_from_hook(
    event: &str,
    notification_type: Option<&str>,
) -> Option<AgentAttention> {
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

pub(crate) fn map_probe_entries(
    entries: &[AgentProbeEntry],
    parent_of: &HashMap<u32, u32>,
    pane_pids: &[(String, u32)],
) -> HashMap<String, AgentAttention> {
    pub(crate) fn rank(attention: AgentAttention) -> u8 {
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
pub(crate) fn reconcile_probe_rounds(
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
