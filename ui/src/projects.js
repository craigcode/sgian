// projects.js — pure helpers for the project board (docs/design/
// keyboard-lease-and-ledger.md §7 / ENHANCEMENTS "projects"): grouping panes
// by project, the per-project attention roll-up the daemon's `project list`
// computes, and the output-guard summary a badge shows. No DOM, no state
// mutation: everything here is testable with plain objects.

/** Output-guard counters the daemon sends (OutputTricks), in display order. */
export const OUTPUT_TRICK_KEYS = [
  ["conceal", "concealed text"],
  ["clipboard", "clipboard writes"],
  ["hyperlink_mismatch", "mismatched links"],
  ["string_controls", "opaque control strings"],
  ["c1_controls", "C1 controls"],
  ["invisible", "invisible characters"],
];

/**
 * Normalize an OutputTricks object (an `output-warning` event's `total`, or a
 * bootstrap `output_warnings` entry) into `{ counts, total }`, or null when
 * nothing fired. Unknown or non-numeric fields count as zero.
 */
export function normalizeOutputWarning(entry) {
  if (!entry || typeof entry !== "object") return null;
  const counts = {};
  let total = 0;
  for (const [key] of OUTPUT_TRICK_KEYS) {
    const value = Number.isFinite(entry[key]) && entry[key] > 0 ? Math.floor(entry[key]) : 0;
    counts[key] = value;
    total += value;
  }
  return total > 0 ? { counts, total } : null;
}

export function outputWarningEquals(a, b) {
  if (!a || !b) return a === b;
  if (a.total !== b.total) return false;
  return OUTPUT_TRICK_KEYS.every(([key]) => a.counts[key] === b.counts[key]);
}

/**
 * "3 concealed text, 1 clipboard write" — the non-zero counters in display
 * order, for a badge title or an overview cell.
 */
export function outputWarningSummary(warning) {
  if (!warning) return "";
  return OUTPUT_TRICK_KEYS.filter(([key]) => warning.counts[key] > 0)
    .map(([key, label]) => `${warning.counts[key]} ${label}`)
    .join(", ");
}

/**
 * Normalize a `projects` map (bootstrap `projects` or a `projects-changed`
 * payload) into a Map name → { name, goal, repo, panes } with pane ids
 * de-duplicated. Malformed entries are dropped.
 */
export function normalizeProjects(entries) {
  const projects = new Map();
  if (!entries || typeof entries !== "object") return projects;
  for (const [name, project] of Object.entries(entries)) {
    if (!name || !project || typeof project !== "object") continue;
    const panes = Array.isArray(project.panes)
      ? Array.from(new Set(project.panes.filter((id) => typeof id === "string" && id)))
      : [];
    projects.set(name, {
      name,
      goal: typeof project.goal === "string" && project.goal ? project.goal : null,
      repo: typeof project.repo === "string" && project.repo ? project.repo : null,
      panes,
    });
  }
  return projects;
}

export function projectsEqual(a, b) {
  if (!a || !b) return a === b;
  if (a.size !== b.size) return false;
  for (const [name, project] of a) {
    const other = b.get(name);
    if (!other) return false;
    if (project.goal !== other.goal || project.repo !== other.repo) return false;
    if (project.panes.length !== other.panes.length) return false;
    if (project.panes.some((id, index) => id !== other.panes[index])) return false;
  }
  return true;
}

/**
 * Group the workspace's panes by project, projects sorted by name, member
 * panes in project order; panes in no project come last under `name: null`
 * (omitted when every pane is assigned). A pane id the daemon lists but the
 * client does not know is skipped: it closed between two events.
 *
 * @param {Map<string, object>} panes state.panes (id → pane)
 * @param {Map<string, object>} projects normalized projects
 * @returns {{ name: string|null, goal: string|null, repo: string|null, panes: object[] }[]}
 */
export function groupPanesByProject(panes, projects) {
  const assigned = new Set();
  const groups = [];
  const names = Array.from(projects?.keys?.() ?? []).sort((a, b) => a.localeCompare(b));
  for (const name of names) {
    const project = projects.get(name);
    const members = [];
    for (const id of project.panes) {
      const pane = panes.get(id);
      if (!pane || assigned.has(id)) continue;
      assigned.add(id);
      members.push(pane);
    }
    groups.push({ name, goal: project.goal, repo: project.repo, panes: members });
  }
  const rest = Array.from(panes.values()).filter((pane) => !assigned.has(pane.id));
  if (rest.length > 0 || groups.length === 0) {
    groups.push({ name: null, goal: null, repo: null, panes: rest });
  }
  return groups;
}

/**
 * The attention roll-up for a set of panes: what one glance needs to tell
 * (mirrors the daemon's ProjectSummary so `ctl project list` and the board
 * agree). `holders` is sorted and de-duplicated.
 */
export function projectRollup(panes, state) {
  const rollup = {
    panes: panes.length,
    live: 0,
    needsInput: 0,
    working: 0,
    idle: 0,
    unattended: 0,
    held: 0,
    holders: [],
    warnings: 0,
  };
  const holders = new Set();
  for (const pane of panes) {
    if ((state.paneStates?.get(pane.id) || "live") === "live") rollup.live += 1;
    const info = state.agentStates?.get(pane.id);
    if (info) {
      if (info.attention === "needs_input") rollup.needsInput += 1;
      else if (info.attention === "working") rollup.working += 1;
      else if (info.attention === "idle") rollup.idle += 1;
      if (info.unattended) rollup.unattended += 1;
    }
    const lease = state.leases?.get(pane.id);
    if (lease) {
      rollup.held += 1;
      holders.add(lease.holder);
    }
    if (state.outputWarnings?.get(pane.id)) rollup.warnings += 1;
  }
  rollup.holders = Array.from(holders).sort((a, b) => a.localeCompare(b));
  return rollup;
}

/**
 * "3 panes · 1 needs input · 1 working · ⚠ 1 unattended · ⌨ alice" — only
 * the parts that are non-zero, for a group heading.
 */
export function rollupText(rollup) {
  const parts = [`${rollup.panes} pane${rollup.panes === 1 ? "" : "s"}`];
  if (rollup.live !== rollup.panes) parts.push(`${rollup.live} live`);
  if (rollup.needsInput) parts.push(`${rollup.needsInput} needs input`);
  if (rollup.working) parts.push(`${rollup.working} working`);
  if (rollup.idle) parts.push(`${rollup.idle} idle`);
  if (rollup.unattended) parts.push(`⚠ ${rollup.unattended} unattended`);
  if (rollup.warnings) parts.push(`${rollup.warnings} with output warnings`);
  if (rollup.holders.length) parts.push(`⌨ ${rollup.holders.join(", ")}`);
  return parts.join(" · ");
}

// ---------------------------------------------------------------------------
// Usage (docs/native-ipc.md "agent_usage"): what a Claude Code session says
// about itself after every turn via `sgian ctl statusline`: model, context
// fill, the account's rate-limit windows. Push from the CLI, never polled.
// ---------------------------------------------------------------------------

/**
 * Normalize an `agent_usage` entry (bootstrap map value or an `agent-usage`
 * event's `usage`) into a plain object, or null when there is nothing to
 * show. Percentages are integers 0..100; `resetsAt` is unix seconds.
 */
export function normalizeUsage(entry) {
  if (!entry || typeof entry !== "object") return null;
  const pct = (value) =>
    Number.isFinite(value) ? Math.max(0, Math.min(100, Math.round(value))) : null;
  const window = (value) => {
    if (!value || typeof value !== "object") return null;
    const used = pct(value.used_percentage);
    if (used === null) return null;
    return {
      used,
      resetsAt: Number.isFinite(value.resets_at) ? Math.floor(value.resets_at) : null,
    };
  };
  const usage = {
    model: typeof entry.model === "string" && entry.model ? entry.model : null,
    context: pct(entry.context_used_percentage),
    fiveHour: window(entry.five_hour),
    sevenDay: window(entry.seven_day),
    updatedAtMs: Number.isFinite(entry.updated_at_ms) ? entry.updated_at_ms : 0,
  };
  if (!usage.model && usage.context === null && !usage.fiveHour && !usage.sevenDay) return null;
  return usage;
}

export function usageEquals(a, b) {
  if (!a || !b) return a === b;
  const win = (x, y) => (!x || !y ? x === y : x.used === y.used && x.resetsAt === y.resetsAt);
  return (
    a.model === b.model &&
    a.context === b.context &&
    win(a.fiveHour, b.fiveHour) &&
    win(a.sevenDay, b.sevenDay) &&
    a.updatedAtMs === b.updatedAtMs
  );
}

/** " ↻ 1h10m" until a window resets, or "" when unknown or past. */
export function formatReset(resetsAt, nowSeconds) {
  if (!Number.isFinite(resetsAt) || resetsAt <= nowSeconds) return "";
  const secs = resetsAt - nowSeconds;
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (h >= 48) return ` ↻ ${Math.floor(h / 24)}d`;
  if (h > 0) return ` ↻ ${h}h${String(m).padStart(2, "0")}m`;
  return ` ↻ ${m}m`;
}

/**
 * "Opus · 40% context · 5h 23% ↻ 1h10m · 7d 41%" — the same line the daemon's
 * `ctl agent` prints, so every surface agrees.
 */
export function usageText(usage, nowSeconds = Math.floor(Date.now() / 1000)) {
  if (!usage) return "";
  const parts = [];
  if (usage.model) parts.push(usage.model);
  if (usage.context !== null) parts.push(`${usage.context}% context`);
  if (usage.fiveHour) parts.push(`5h ${usage.fiveHour.used}%${formatReset(usage.fiveHour.resetsAt, nowSeconds)}`);
  if (usage.sevenDay) parts.push(`7d ${usage.sevenDay.used}%${formatReset(usage.sevenDay.resetsAt, nowSeconds)}`);
  return parts.join(" · ");
}

/**
 * The account-level rate-limit line for a group heading: the freshest
 * reading among the panes (limits are per account, so one line suffices),
 * or "" when no pane has reported.
 */
export function groupLimitText(panes, state, nowSeconds = Math.floor(Date.now() / 1000)) {
  let freshest = null;
  for (const pane of panes) {
    const usage = state.agentUsage?.get(pane.id);
    if (!usage || (!usage.fiveHour && !usage.sevenDay)) continue;
    if (!freshest || usage.updatedAtMs > freshest.updatedAtMs) freshest = usage;
  }
  if (!freshest) return "";
  return usageText({ ...freshest, model: null, context: null }, nowSeconds);
}
