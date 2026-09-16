import { describe, it, expect, beforeEach } from "vitest";
import { leaf, split, layoutLeafIds } from "../ui/src/layout.js";
import { createAgentChat, applyAgentEvent } from "../ui/src/agent.js";
import {
  handlePtyOutput,
  handlePaneEnded,
  handlePaneCreated,
  handlePaneClosed,
  handlePaneRenamed,
  handleAgentState,
  handleAgentEvent,
  handleLeaseState,
  normalizeLeaseInfo,
  leaseEquals,
  normalizeAgentState,
  agentStateEquals,
  setPaneRuntimeState,
  paneRuntimeState,
  paneIsEnded,
} from "../ui/src/events.js";

// Helper: create a fresh state object for testing.
function makeState(panes = [], opts = {}) {
  const state = {
    panes: new Map(panes.map((p) => [p.id, p])),
    paneStates: new Map(opts.paneStates || []),
    agentStates: new Map(opts.agentStates || []),
    leases: new Map(opts.leases || []),
    chats: new Map(opts.chats || []),
    activePaneId: opts.activePaneId || panes[0]?.id || null,
    layout: opts.layout || null,
    terminalViews: new Map(),
  };
  return state;
}

// Helper: create a callbacks bag that records calls.
function makeCallbacks(overrides = {}) {
  const calls = {
    render: 0,
    renderTabs: 0,
    renderStatus: 0,
    schedulePaneReconcile: 0,
    persistWorkspaceLayout: 0,
    syncActivePane: [],
    disposeTerminalView: [],
    appendTerminalOutput: [],
    updatePaneTitle: [],
  };
  return {
    calls,
    render: () => { calls.render++; },
    renderTabs: () => { calls.renderTabs++; },
    renderStatus: () => { calls.renderStatus++; },
    schedulePaneReconcile: () => { calls.schedulePaneReconcile++; },
    persistWorkspaceLayout: () => { calls.persistWorkspaceLayout++; },
    syncActivePane: (id) => { calls.syncActivePane.push(id); },
    disposeTerminalView: (id) => { calls.disposeTerminalView.push(id); },
    appendTerminalOutput: (id, data) => { calls.appendTerminalOutput.push({ id, data }); },
    updatePaneTitle: (pane) => { calls.updatePaneTitle.push(pane); },
    ...overrides,
  };
}

describe("paneRuntimeState / paneIsEnded", () => {
  it("defaults to live", () => {
    const state = makeState();
    expect(paneRuntimeState(state, "any")).toBe("live");
  });

  it("returns the set state", () => {
    const state = makeState();
    setPaneRuntimeState(state, "a", "ended");
    expect(paneRuntimeState(state, "a")).toBe("ended");
    expect(paneIsEnded(state, "a")).toBe(true);
  });
});

describe("handlePaneCreated", () => {
  it("adds the pane to state and updates tabs/status", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handlePaneCreated(state, { id: "b", title: "b", kind: "shell" }, cb);
    expect(state.panes.has("b")).toBe(true);
    expect(state.paneStates.get("b")).toBe("live");
    expect(cb.calls.renderTabs).toBe(1);
    expect(cb.calls.renderStatus).toBe(1);
    expect(cb.calls.schedulePaneReconcile).toBe(1);
  });

  it("does not set live state if pane already exists", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a"), paneStates: [["a", "ended"]] },
    );
    const cb = makeCallbacks();
    handlePaneCreated(state, { id: "a", title: "a-updated", kind: "shell" }, cb);
    // The pane was already known (e.g. ended), so its state should not be reset to live.
    expect(state.paneStates.get("a")).toBe("ended");
    // But the pane data is updated.
    expect(state.panes.get("a").title).toBe("a-updated");
  });

  it("ignores events without an id", () => {
    const state = makeState();
    const cb = makeCallbacks();
    handlePaneCreated(state, {}, cb);
    expect(state.panes.size).toBe(0);
    expect(cb.calls.renderTabs).toBe(0);
  });
});

describe("handlePaneEnded", () => {
  it("sets the pane state to ended and renders", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handlePaneEnded(state, { pane_id: "a" }, cb);
    expect(state.paneStates.get("a")).toBe("ended");
    expect(cb.calls.render).toBe(1);
  });

  it("clamps a mounted agent chat before rendering catch-up PaneEnded", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    applyAgentEvent(chat, {
      kind: "permission_request",
      request_id: "req-1",
      tool_name: "Bash",
    });
    const state = makeState(
      [{ id: "agent-a", title: "agent", kind: "agent" }],
      { layout: leaf("agent-a"), chats: [["agent-a", chat]] },
    );
    const cb = makeCallbacks();

    handlePaneEnded(state, { pane_id: "agent-a" }, cb);

    expect(state.paneStates.get("agent-a")).toBe("ended");
    expect(chat.busy).toBe(false);
    expect(chat.pendingPermission).toBeNull();
    expect(chat.exited).toBe(true);
    expect(chat.messages.at(-1).open).toBe(false);
    expect(cb.calls.render).toBe(1);
  });

  it("ignores empty payload", () => {
    const state = makeState();
    const cb = makeCallbacks();
    handlePaneEnded(state, {}, cb);
    expect(cb.calls.render).toBe(0);
  });

  it("ignores a trailing pane-ended for an unknown pane (no paneStates re-insert)", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handlePaneEnded(state, { pane_id: "deleted-pane" }, cb);
    expect(state.paneStates.has("deleted-pane")).toBe(false);
    expect(cb.calls.render).toBe(0);
  });
});

describe("handlePaneClosed", () => {
  it("removes the pane, prunes layout, refocuses, renders, and persists", () => {
    const state = makeState(
      [
        { id: "a", title: "a", kind: "shell" },
        { id: "b", title: "b", kind: "shell" },
      ],
      {
        layout: split("row", leaf("a"), leaf("b")),
        activePaneId: "a",
      },
    );
    const cb = makeCallbacks();
    handlePaneClosed(state, { pane_id: "a" }, cb);
    expect(state.panes.has("a")).toBe(false);
    expect(state.paneStates.has("a")).toBe(false);
    expect(cb.calls.disposeTerminalView).toEqual(["a"]);
    expect(layoutLeafIds(state.layout)).toEqual(["b"]);
    // Active pane should switch to the survivor.
    expect(state.activePaneId).toBe("b");
    expect(cb.calls.syncActivePane).toEqual(["b"]);
    expect(cb.calls.render).toBe(1);
    expect(cb.calls.persistWorkspaceLayout).toBe(1);
  });

  it("ignores unknown pane id", () => {
    const state = makeState([{ id: "a", title: "a", kind: "shell" }]);
    const cb = makeCallbacks();
    handlePaneClosed(state, { pane_id: "unknown" }, cb);
    expect(state.panes.size).toBe(1);
    expect(cb.calls.render).toBe(0);
  });

  it("does not change active pane when a non-active pane closes", () => {
    const state = makeState(
      [
        { id: "a", title: "a", kind: "shell" },
        { id: "b", title: "b", kind: "shell" },
      ],
      {
        layout: split("row", leaf("a"), leaf("b")),
        activePaneId: "a",
      },
    );
    const cb = makeCallbacks();
    handlePaneClosed(state, { pane_id: "b" }, cb);
    expect(state.activePaneId).toBe("a");
    expect(cb.calls.syncActivePane).toEqual([]);
  });
});

describe("handlePaneRenamed", () => {
  it("updates the pane title surgically without full render", () => {
    const state = makeState(
      [{ id: "a", title: "old", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handlePaneRenamed(state, { id: "a", title: "new", kind: "shell" }, cb);
    expect(state.panes.get("a").title).toBe("new");
    // Renamed is surgical: no full render, just tabs/status + title update.
    expect(cb.calls.render).toBe(0);
    expect(cb.calls.renderTabs).toBe(1);
    expect(cb.calls.renderStatus).toBe(1);
    expect(cb.calls.updatePaneTitle).toEqual([{ id: "a", title: "new", kind: "shell" }]);
  });

  it("ignores unknown pane", () => {
    const state = makeState();
    const cb = makeCallbacks();
    handlePaneRenamed(state, { id: "unknown", title: "x" }, cb);
    expect(cb.calls.renderTabs).toBe(0);
  });
});

describe("handlePtyOutput", () => {
  it("marks pane live and appends output", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a"), paneStates: [["a", "ended"]] },
    );
    const cb = makeCallbacks();
    handlePtyOutput(state, { pane_id: "a", data: "hello" }, cb);
    expect(state.paneStates.get("a")).toBe("live");
    expect(cb.calls.appendTerminalOutput).toEqual([{ id: "a", data: "hello" }]);
    // A revived pane (was ended) triggers a full render.
    expect(cb.calls.render).toBe(1);
  });

  it("does not full-render when pane was already live", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handlePtyOutput(state, { pane_id: "a", data: "hello" }, cb);
    expect(cb.calls.render).toBe(0);
  });

  it("does not create a paneStates entry for an unknown pane", () => {
    // Trailing output after a close must not re-insert state for the deleted
    // pane (paneStates would grow without bound under pane churn).
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handlePtyOutput(state, { pane_id: "ghost", data: "late output" }, cb);
    expect(state.paneStates.has("ghost")).toBe(false);
    // Output is still forwarded; appendTerminalOutput drops it for unknown panes.
    expect(cb.calls.appendTerminalOutput).toEqual([{ id: "ghost", data: "late output" }]);
    expect(cb.calls.render).toBe(0);
  });

  it("still marks a known ended pane live on output", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a"), paneStates: [["a", "ended"]] },
    );
    const cb = makeCallbacks();
    handlePtyOutput(state, { pane_id: "a", data: "revived" }, cb);
    expect(state.paneStates.get("a")).toBe("live");
    expect(cb.calls.render).toBe(1);
  });
});

describe("normalizeAgentState / agentStateEquals", () => {
  it("returns null when there is no known agent", () => {
    expect(normalizeAgentState(null)).toBe(null);
    expect(normalizeAgentState({})).toBe(null);
    expect(normalizeAgentState({ agent: null, attention: "working" })).toBe(null);
    expect(normalizeAgentState({ agent: "", attention: "working" })).toBe(null);
  });

  it("keeps contract attention values, normalizes others to null", () => {
    expect(normalizeAgentState({ agent: "claude", attention: "working" })).toEqual({
      agent: "claude",
      attention: "working",
    });
    expect(normalizeAgentState({ agent: "claude", attention: "needs_input" })).toEqual({
      agent: "claude",
      attention: "needs_input",
    });
    expect(normalizeAgentState({ agent: "claude", attention: "idle" })).toEqual({
      agent: "claude",
      attention: "idle",
    });
    // Absent or off-contract attention (unclassified, or a newer daemon)
    // renders as the plain agent glyph.
    expect(normalizeAgentState({ agent: "claude" })).toEqual({
      agent: "claude",
      attention: null,
    });
    expect(normalizeAgentState({ agent: "claude", attention: "banana" })).toEqual({
      agent: "claude",
      attention: null,
    });
  });

  it("compares normalized states", () => {
    expect(agentStateEquals(null, null)).toBe(true);
    expect(agentStateEquals({ agent: "claude", attention: null }, null)).toBe(false);
    expect(
      agentStateEquals(
        { agent: "claude", attention: "idle" },
        { agent: "claude", attention: "idle" },
      ),
    ).toBe(true);
    expect(
      agentStateEquals(
        { agent: "claude", attention: "idle" },
        { agent: "claude", attention: "working" },
      ),
    ).toBe(false);
  });
});

describe("handleAgentState", () => {
  it("sets the agent state for a known pane and renders", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handleAgentState(state, { pane_id: "a", agent: "claude", attention: "working" }, cb);
    expect(state.agentStates.get("a")).toEqual({ agent: "claude", attention: "working" });
    expect(cb.calls.render).toBe(1);
  });

  it("updates the attention on a transition", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      {
        layout: leaf("a"),
        agentStates: [["a", { agent: "claude", attention: "working" }]],
      },
    );
    const cb = makeCallbacks();
    handleAgentState(state, { pane_id: "a", agent: "claude", attention: "needs_input" }, cb);
    expect(state.agentStates.get("a")).toEqual({ agent: "claude", attention: "needs_input" });
    expect(cb.calls.render).toBe(1);
  });

  it("clears the agent state when agent becomes null", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      {
        layout: leaf("a"),
        agentStates: [["a", { agent: "claude", attention: "idle" }]],
      },
    );
    const cb = makeCallbacks();
    handleAgentState(state, { pane_id: "a", agent: null, attention: null }, cb);
    expect(state.agentStates.has("a")).toBe(false);
    expect(cb.calls.render).toBe(1);
  });

  it("ignores events for unknown panes (no agentStates re-insert)", () => {
    // Same guard style as handlePtyOutput: a trailing agent-state for a deleted
    // pane must not re-insert an entry (it would leak under churn).
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handleAgentState(state, { pane_id: "ghost", agent: "claude", attention: "working" }, cb);
    expect(state.agentStates.has("ghost")).toBe(false);
    expect(cb.calls.render).toBe(0);
  });

  it("ignores empty payloads and no-change events", () => {
    const state = makeState(
      [
        { id: "a", title: "a", kind: "shell" },
        { id: "b", title: "b", kind: "shell" },
      ],
      {
        layout: split("row", leaf("a"), leaf("b")),
        agentStates: [["a", { agent: "claude", attention: "idle" }]],
      },
    );
    const cb = makeCallbacks();
    handleAgentState(state, {}, cb);
    expect(cb.calls.render).toBe(0);
    // Same state as already tracked: no render.
    handleAgentState(state, { pane_id: "a", agent: "claude", attention: "idle" }, cb);
    expect(cb.calls.render).toBe(0);
    // Clearing an already-clear known pane: no render.
    handleAgentState(state, { pane_id: "b", agent: null, attention: null }, cb);
    expect(cb.calls.render).toBe(0);
  });
});

describe("handlePaneClosed agent cleanup", () => {
  it("drops the pane's agent state along with the pane", () => {
    const state = makeState(
      [
        { id: "a", title: "a", kind: "shell" },
        { id: "b", title: "b", kind: "shell" },
      ],
      {
        layout: split("row", leaf("a"), leaf("b")),
        activePaneId: "a",
        agentStates: [
          ["a", { agent: "claude", attention: "needs_input" }],
          ["b", { agent: "claude", attention: "working" }],
        ],
      },
    );
    const cb = makeCallbacks();
    handlePaneClosed(state, { pane_id: "a" }, cb);
    expect(state.agentStates.has("a")).toBe(false);
    // The surviving pane's badge state is untouched.
    expect(state.agentStates.get("b")).toEqual({ agent: "claude", attention: "working" });
  });
});


describe("handleAgentEvent", () => {
  it("folds the event into the pane's chat and schedules a chat render", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "agent" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks({
      scheduleChatRender: (id) => (cb.calls.scheduleChatRender ??= []).push(id),
    });
    handleAgentEvent(
      state,
      { pane_id: "a", event: { kind: "session", session_id: "s-1", model: "m" } },
      cb,
    );
    handleAgentEvent(
      state,
      { pane_id: "a", event: { kind: "text_delta", text: "hi" } },
      cb,
    );
    const chat = state.chats.get("a");
    expect(chat.sessionId).toBe("s-1");
    expect(chat.messages).toEqual([{ type: "assistant", text: "hi", open: true }]);
    expect(chat.busy).toBe(true);
    expect(cb.calls.scheduleChatRender).toEqual(["a", "a"]);
  });

  it("ignores events for unknown panes (no chat leak)", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "agent" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks({
      scheduleChatRender: (id) => (cb.calls.scheduleChatRender ??= []).push(id),
    });
    handleAgentEvent(
      state,
      { pane_id: "ghost", event: { kind: "text_delta", text: "boo" } },
      cb,
    );
    expect(state.chats.has("ghost")).toBe(false);
    expect(cb.calls.scheduleChatRender ?? []).toEqual([]);
  });

  it("ignores a trailing agent event after a pane changes to shell kind", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "shell" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks({
      scheduleChatRender: (id) => (cb.calls.scheduleChatRender ??= []).push(id),
    });
    handleAgentEvent(
      state,
      { pane_id: "a", event: { kind: "text_delta", text: "late" } },
      cb,
    );
    expect(state.chats.has("a")).toBe(false);
    expect(cb.calls.scheduleChatRender ?? []).toEqual([]);
  });

  it("ignores malformed payloads", () => {
    const state = makeState(
      [{ id: "a", title: "a", kind: "agent" }],
      { layout: leaf("a") },
    );
    const cb = makeCallbacks();
    handleAgentEvent(state, {}, cb);
    handleAgentEvent(state, { pane_id: "a" }, cb);
    handleAgentEvent(state, { pane_id: "a", event: { noKind: true } }, cb);
    expect(state.chats.has("a")).toBe(false);
  });
});

describe("handlePaneClosed chat cleanup", () => {
  it("drops the pane's chat state and view along with the pane", () => {
    const state = makeState(
      [
        { id: "a", title: "a", kind: "agent" },
        { id: "b", title: "b", kind: "shell" },
      ],
      {
        layout: split("row", leaf("a"), leaf("b")),
        activePaneId: "a",
        chats: [["a", { sessionId: "s-1", messages: [] }]],
      },
    );
    const disposed = [];
    const cb = makeCallbacks({
      disposeChatView: (id) => disposed.push(id),
    });
    handlePaneClosed(state, { pane_id: "a" }, cb);
    expect(state.chats.has("a")).toBe(false);
    expect(disposed).toEqual(["a"]);
  });
});

describe("keyboard lease events", () => {
  it("normalizes held and unheld lease payloads", () => {
    expect(normalizeLeaseInfo({ holder: "bob", since_ms: 5 })).toEqual({ holder: "bob", sinceMs: 5 });
    expect(normalizeLeaseInfo({ holder: "bob" })).toEqual({ holder: "bob", sinceMs: null });
    expect(normalizeLeaseInfo({ holder: null })).toBeNull();
    expect(normalizeLeaseInfo(undefined)).toBeNull();
    expect(leaseEquals({ holder: "a", sinceMs: 1 }, { holder: "a", sinceMs: 1 })).toBe(true);
    expect(leaseEquals({ holder: "a", sinceMs: 1 }, { holder: "a", sinceMs: 2 })).toBe(false);
    expect(leaseEquals(null, null)).toBe(true);
  });

  it("records a taken lease and clears it on release or revoke", () => {
    const state = makeState([{ id: "pane-1" }]);
    const callbacks = makeCallbacks();
    handleLeaseState(
      state,
      { pane_id: "pane-1", transition: "taken", holder: "bob", since_ms: 9 },
      callbacks,
    );
    expect(state.leases.get("pane-1")).toEqual({ holder: "bob", sinceMs: 9 });
    expect(callbacks.calls.render).toBe(1);
    handleLeaseState(
      state,
      { pane_id: "pane-1", transition: "taken", holder: "bob", since_ms: 9 },
      callbacks,
    );
    expect(callbacks.calls.render).toBe(1);
    handleLeaseState(
      state,
      { pane_id: "pane-1", transition: "released", holder: null, note: "done" },
      callbacks,
    );
    expect(state.leases.has("pane-1")).toBe(false);
    expect(callbacks.calls.render).toBe(2);
    handleLeaseState(
      state,
      { pane_id: "pane-1", transition: "taken", holder: "amy", since_ms: 10 },
      callbacks,
    );
    handleLeaseState(state, { pane_id: "pane-1", transition: "revoked", holder: null }, callbacks);
    expect(state.leases.has("pane-1")).toBe(false);
  });

  it("ignores lease events for unknown panes", () => {
    const state = makeState([{ id: "pane-1" }]);
    const callbacks = makeCallbacks();
    handleLeaseState(
      state,
      { pane_id: "pane-9", transition: "taken", holder: "bob", since_ms: 1 },
      callbacks,
    );
    expect(state.leases.size).toBe(0);
    expect(callbacks.calls.render).toBe(0);
  });
});

describe("agent permission mode", () => {
  it("normalizes mode and the unattended flag", () => {
    expect(normalizeAgentState({ agent: "claude", attention: "idle" })).toEqual({
      agent: "claude",
      attention: "idle",
    });
    expect(
      normalizeAgentState({ agent: "claude", attention: "working", mode: "auto", unattended: true }),
    ).toEqual({ agent: "claude", attention: "working", mode: "auto", unattended: true });
    const odd = normalizeAgentState({ agent: "claude", mode: 7, unattended: "yes" });
    expect("mode" in odd).toBe(false);
    expect("unattended" in odd).toBe(false);
    expect(
      agentStateEquals(
        { agent: "claude", attention: "idle" },
        { agent: "claude", attention: "idle", mode: "auto", unattended: true },
      ),
    ).toBe(false);
    expect(
      agentStateEquals(
        { agent: "claude", attention: "idle" },
        { agent: "claude", attention: "idle", unattended: false },
      ),
    ).toBe(true);
  });

  it("a mode-only agent-state event re-renders", () => {
    const state = makeState([{ id: "pane-1" }]);
    const callbacks = makeCallbacks();
    handleAgentState(state, { pane_id: "pane-1", agent: "claude", attention: "idle" }, callbacks);
    handleAgentState(
      state,
      { pane_id: "pane-1", agent: "claude", attention: "idle", mode: "bypass", unattended: true },
      callbacks,
    );
    expect(callbacks.calls.render).toBe(2);
    expect(state.agentStates.get("pane-1").unattended).toBe(true);
  });
});
