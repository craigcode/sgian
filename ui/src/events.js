// events.js — backend event handler functions.
//
// Each handler takes the shared `state` object plus a `callbacks` bag of
// DOM/terminal side-effect functions, making them testable in happy-dom
// without importing the full app.

import {
  pruneLeaf,
  firstPaneId,
  layoutLeafIds,
} from "./layout.js";
import {
  createAgentChat,
  applyAgentEvent,
  clampAgentChatToPaneEnded,
} from "./agent.js";

/**
 * Set a pane's runtime state ("live" or "ended") in the state map.
 */
export function setPaneRuntimeState(state, paneId, runtimeState) {
  if (!paneId || !runtimeState) return;
  state.paneStates.set(paneId, runtimeState);
}

/**
 * Get a pane's runtime state, defaulting to "live".
 */
export function paneRuntimeState(state, paneId) {
  return state.paneStates.get(paneId) || "live";
}

/**
 * True when the pane's runtime state is "ended".
 */
export function paneIsEnded(state, paneId) {
  return paneRuntimeState(state, paneId) === "ended";
}

/**
 * Handle a `pty-output` event: mark the pane live, write data to the terminal.
 *
 * @param {object} state shared app state
 * @param {{ pane_id?: string, paneId?: string, data?: string }} payload
 * @param {{ appendTerminalOutput: Function, render: Function }} callbacks
 */
export function handlePtyOutput(state, payload, callbacks) {
  const paneId = payload.pane_id || payload.paneId;
  if (!paneId) return;
  // Only track runtime state for known panes: trailing output from a deleted
  // pane must not re-insert a paneStates entry (it would leak under churn).
  if (state.panes.has(paneId)) {
    const wasEnded = paneIsEnded(state, paneId);
    setPaneRuntimeState(state, paneId, "live");
    callbacks.appendTerminalOutput(paneId, payload.data || "");
    if (wasEnded) callbacks.render();
    return;
  }
  callbacks.appendTerminalOutput(paneId, payload.data || "");
}

// (T1) Attention values the backend can send (AgentAttention, snake_case).
// Anything else is normalized to null so badge classes always match the contract.
const AGENT_ATTENTION_STATES = new Set(["working", "needs_input", "idle"]);

/**
 * Normalize an `agent-state` payload (or a bootstrap `agent_states` entry) into
 * `{ agent, attention }` — or null when the pane has no known agent. `agent` is
 * the agent CLI name ("claude" today); `attention` is one of
 * "working"/"needs_input"/"idle" or null while unclassified.
 */
export function normalizeAgentState(entry) {
  if (!entry || typeof entry.agent !== "string" || !entry.agent) return null;
  const attention = AGENT_ATTENTION_STATES.has(entry.attention)
    ? entry.attention
    : null;
  // `mode` is the agent's observed permission mode; `unattended` means tools
  // run without a person approving them and the pane must be marked. Both
  // are additive: absent from the object (not null) when the daemon did not
  // send them, so older payloads normalize exactly as before.
  const normalized = { agent: entry.agent, attention };
  if (typeof entry.mode === "string" && entry.mode) normalized.mode = entry.mode;
  if (entry.unattended === true) normalized.unattended = true;
  return normalized;
}

/**
 * True when two normalized agent states render identically.
 */
export function agentStateEquals(a, b) {
  if (!a || !b) return a === b;
  return (
    a.agent === b.agent &&
    a.attention === b.attention &&
    (a.mode ?? null) === (b.mode ?? null) &&
    Boolean(a.unattended) === Boolean(b.unattended)
  );
}

/**
 * Handle an `agent-state` event: set/update/clear the pane's agent badge state
 * and re-render (tabs, pane chrome, and the attention affordance all read
 * state.agentStates at render time).
 */
export function handleAgentState(state, payload, callbacks) {
  const paneId = payload.pane_id || payload.paneId;
  if (!paneId) return;
  // Same guard as handlePtyOutput: a trailing agent-state for a deleted pane
  // must not re-insert an agentStates entry (it would leak under churn).
  if (!state.panes.has(paneId)) return;
  const next = normalizeAgentState(payload);
  const existing = state.agentStates.get(paneId) ?? null;
  if (agentStateEquals(existing, next)) return;
  if (next) {
    state.agentStates.set(paneId, next);
  } else {
    state.agentStates.delete(paneId);
  }
  callbacks.render();
}

/**
 * Normalize a lease payload (a `lease-state` event, a bootstrap `leases`
 * entry, or a take/release response) into `{ holder, sinceMs }`, or null when
 * the pane is not held.
 */
export function normalizeLeaseInfo(entry) {
  if (!entry || typeof entry.holder !== "string" || !entry.holder) return null;
  const sinceMs = Number.isFinite(entry.since_ms) ? entry.since_ms : null;
  return { holder: entry.holder, sinceMs };
}

export function leaseEquals(a, b) {
  if (!a || !b) return a === b;
  return a.holder === b.holder && a.sinceMs === b.sinceMs;
}

/**
 * Handle a `lease-state` event (docs/design/keyboard-lease-and-ledger.md):
 * `taken` records the holder, `released`/`revoked` clear it. Same
 * unknown-pane guard as the other handlers.
 */
export function handleLeaseState(state, payload, callbacks) {
  const paneId = payload.pane_id || payload.paneId;
  if (!paneId) return;
  if (!state.panes.has(paneId)) return;
  if (!state.leases) state.leases = new Map();
  const next = payload.transition === "taken" ? normalizeLeaseInfo(payload) : null;
  const existing = state.leases.get(paneId) ?? null;
  if (leaseEquals(existing, next)) return;
  if (next) state.leases.set(paneId, next);
  else state.leases.delete(paneId);
  callbacks.render();
}

/**
 * Handle an `agent-event` (T2): fold one normalized agent event into the
 * pane's chat state and schedule a (rAF-throttled) chat re-render. Same
 * unknown-pane guard as the other handlers: a trailing event for a deleted
 * pane must not re-insert a chats entry (it would leak under churn).
 */
export function handleAgentEvent(state, payload, callbacks) {
  const paneId = payload.pane_id || payload.paneId;
  if (!paneId) return;
  // A snapshot can replace an agent pane with a shell pane using the same id.
  // Ignore any trailing agent events so they cannot recreate chat state after
  // the kind flip disposed it.
  if (state.panes.get(paneId)?.kind !== "agent") return;
  const event = payload.event;
  if (!event || typeof event.kind !== "string") return;
  if (!state.chats) state.chats = new Map();
  let chat = state.chats.get(paneId);
  if (!chat) {
    chat = createAgentChat();
    state.chats.set(paneId, chat);
  }
  applyAgentEvent(chat, event);
  callbacks.scheduleChatRender?.(paneId);
}

/**
 * Handle a `pane-ended` event: mark the pane ended and re-render.
 */
export function handlePaneEnded(state, payload, callbacks) {
  const paneId = payload.pane_id || payload.paneId;
  if (!paneId) return;
  // Only track runtime state for known panes (same guard as handlePtyOutput):
  // a trailing pane-ended for a deleted pane must not re-insert a paneStates
  // entry (it would leak under churn).
  if (!state.panes.has(paneId)) return;
  setPaneRuntimeState(state, paneId, "ended");
  // Subscription catch-up can report PaneEnded after daemon death without an
  // agent process_exit. Clamp an already-mounted agent chat in the same render
  // as the runtime-state change so the composer cannot remain wedged busy.
  if (state.panes.get(paneId)?.kind === "agent") {
    clampAgentChatToPaneEnded(state.chats?.get(paneId));
  }
  callbacks.render();
}

/**
 * Handle a `pane-created` event: register the pane, update tabs/status, schedule reconcile.
 * Does NOT place the pane in the layout (reconcile handles that).
 */
export function handlePaneCreated(state, payload, callbacks) {
  const pane = payload || {};
  if (!pane.id) return;
  if (!state.panes.has(pane.id)) {
    setPaneRuntimeState(state, pane.id, "live");
  }
  state.panes.set(pane.id, pane);
  callbacks.renderTabs();
  callbacks.renderStatus();
  callbacks.schedulePaneReconcile();
}

/**
 * Handle a `pane-closed` event: dispose the terminal view, remove the pane,
 * prune the layout, refocus, render, and persist.
 */
export function handlePaneClosed(state, payload, callbacks) {
  const paneId = (payload || {}).pane_id;
  if (!paneId || !state.panes.has(paneId)) return;
  callbacks.disposeTerminalView(paneId);
  state.panes.delete(paneId);
  state.paneStates.delete(paneId);
  state.agentStates.delete(paneId);
  state.agentSpecs?.delete(paneId);
  state.lastActivityMs?.delete(paneId);
  // (T2) The pane's chat goes with it (mirrors the agentStates drop).
  state.chats?.delete(paneId);
  callbacks.disposeChatView?.(paneId);
  state.layout = pruneLeaf(state.layout, paneId);
  if (state.activePaneId === paneId) {
    state.activePaneId = firstPaneId(state.layout);
    callbacks.syncActivePane(state.activePaneId);
  }
  callbacks.render();
  callbacks.persistWorkspaceLayout();
}

/**
 * Handle a `pane-renamed` event: update the pane title surgically (in-place
 * title element update) and refresh tabs/status. Does NOT full-render.
 */
export function handlePaneRenamed(state, payload, callbacks) {
  const pane = payload || {};
  if (!pane.id || !state.panes.has(pane.id)) return;
  state.panes.set(pane.id, pane);
  callbacks.updatePaneTitle(pane);
  callbacks.renderTabs();
  callbacks.renderStatus();
}
