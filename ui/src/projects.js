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
