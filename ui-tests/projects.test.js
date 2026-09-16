import { describe, it, expect } from "vitest";
import {
  normalizeOutputWarning,
  outputWarningEquals,
  outputWarningSummary,
  normalizeProjects,
  projectsEqual,
  groupPanesByProject,
  projectRollup,
  rollupText,
} from "../ui/src/projects.js";

describe("output warning normalization", () => {
  it("keeps the daemon's counters and drops empty or malformed entries", () => {
    const warning = normalizeOutputWarning({ conceal: 2, clipboard: 1, bogus: 9 });
    expect(warning).toEqual({
      counts: { conceal: 2, clipboard: 1, hyperlink_mismatch: 0, string_controls: 0, c1_controls: 0 },
      total: 3,
    });
    expect(normalizeOutputWarning({ conceal: 0 })).toBeNull();
    expect(normalizeOutputWarning({ conceal: -4, clipboard: "x" })).toBeNull();
    expect(normalizeOutputWarning(null)).toBeNull();
    expect(normalizeOutputWarning("nope")).toBeNull();
    expect(outputWarningEquals(warning, normalizeOutputWarning({ conceal: 2, clipboard: 1 }))).toBe(true);
    expect(outputWarningEquals(warning, normalizeOutputWarning({ conceal: 3 }))).toBe(false);
    expect(outputWarningEquals(null, null)).toBe(true);
    expect(outputWarningEquals(warning, null)).toBe(false);
  });

  it("summarises the non-zero counters in display order", () => {
    expect(outputWarningSummary(normalizeOutputWarning({ c1_controls: 1, conceal: 2 }))).toBe(
      "2 concealed text, 1 C1 controls",
    );
    expect(outputWarningSummary(null)).toBe("");
  });
});

describe("projects", () => {
  it("normalizes the daemon's project table and compares it structurally", () => {
    const projects = normalizeProjects({
      feat: { name: "feat", goal: "ship", repo: "/r", panes: ["p1", "p1", "p2"], created_at_ms: 1 },
      bare: { panes: null },
      "": { panes: [] },
      junk: "no",
    });
    expect(Array.from(projects.keys())).toEqual(["feat", "bare"]);
    expect(projects.get("feat")).toEqual({ name: "feat", goal: "ship", repo: "/r", panes: ["p1", "p2"] });
    expect(projects.get("bare")).toEqual({ name: "bare", goal: null, repo: null, panes: [] });
    expect(projectsEqual(projects, normalizeProjects({
      feat: { goal: "ship", repo: "/r", panes: ["p1", "p2"] },
      bare: {},
    }))).toBe(true);
    expect(projectsEqual(projects, normalizeProjects({ feat: { panes: ["p2", "p1"] }, bare: {} }))).toBe(false);
    expect(projectsEqual(projects, normalizeProjects({}))).toBe(false);
    expect(projectsEqual(undefined, new Map())).toBe(false);
    expect(normalizeProjects(undefined).size).toBe(0);
  });

  it("groups panes by project in name order with unassigned panes last", () => {
    const panes = new Map([
      ["p1", { id: "p1", title: "one" }],
      ["p2", { id: "p2", title: "two" }],
      ["p3", { id: "p3", title: "three" }],
    ]);
    const projects = normalizeProjects({
      zeta: { panes: ["p3", "gone"] },
      alpha: { panes: ["p2"] },
      empty: { panes: [] },
    });
    const groups = groupPanesByProject(panes, projects);
    expect(groups.map((group) => group.name)).toEqual(["alpha", "empty", "zeta", null]);
    expect(groups[0].panes.map((pane) => pane.id)).toEqual(["p2"]);
    expect(groups[1].panes).toEqual([]);
    expect(groups[2].panes.map((pane) => pane.id)).toEqual(["p3"]);
    expect(groups[3].panes.map((pane) => pane.id)).toEqual(["p1"]);
    // Every pane assigned: no trailing "no project" group. No projects: one.
    const all = groupPanesByProject(panes, normalizeProjects({ a: { panes: ["p1", "p2", "p3"] } }));
    expect(all.map((group) => group.name)).toEqual(["a"]);
    const none = groupPanesByProject(panes, new Map());
    expect(none).toHaveLength(1);
    expect(none[0].name).toBeNull();
    expect(none[0].panes).toHaveLength(3);
  });

  it("rolls attention, leases and warnings up like the daemon's project list", () => {
    const panes = [{ id: "p1" }, { id: "p2" }, { id: "p3" }, { id: "p4" }];
    const state = {
      paneStates: new Map([["p3", "ended"]]),
      agentStates: new Map([
        ["p1", { agent: "claude", attention: "needs_input" }],
        ["p2", { agent: "claude", attention: "working", mode: "auto", unattended: true }],
        ["p3", { agent: "claude", attention: "idle" }],
      ]),
      leases: new Map([
        ["p1", { holder: "bob", sinceMs: 1 }],
        ["p2", { holder: "alice", sinceMs: 1 }],
        ["p4", { holder: "bob", sinceMs: 1 }],
      ]),
      outputWarnings: new Map([["p2", { counts: {}, total: 1 }]]),
    };
    const rollup = projectRollup(panes, state);
    expect(rollup).toEqual({
      panes: 4,
      live: 3,
      needsInput: 1,
      working: 1,
      idle: 1,
      unattended: 1,
      held: 3,
      holders: ["alice", "bob"],
      warnings: 1,
    });
    expect(rollupText(rollup)).toBe(
      "4 panes · 3 live · 1 needs input · 1 working · 1 idle · ⚠ 1 unattended · 1 with output warnings · ⌨ alice, bob",
    );
    expect(rollupText(projectRollup([{ id: "p9" }], { paneStates: new Map() }))).toBe("1 pane");
    expect(projectRollup([], {}).holders).toEqual([]);
  });
});
