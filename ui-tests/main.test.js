// main.test.js — happy-dom integration test for the app wiring in main.js.
//
// Boots the real main.js module against a stubbed window.__TAURI__ whose invoke
// only recognizes the exact command names registered by the backend
// (src-tauri: tauri::generate_handler![...]). Any invoke with a name outside
// that list throws — pinning the invoke boundary that H1 fell through
// ("write-config" vs write_config), plus boot ordering, the merge-safe
// bootstrap (M11), startup-pane view disposal (M14), and the resync path (M12).
//
// main.js runs its wiring at import time, so this file boots the app once in
// beforeAll and runs its assertions sequentially against that single instance.

import { describe, it, expect, beforeAll, afterAll, vi } from "vitest";

// Exact copy of the backend invoke_handler registration (lib.rs).
const KNOWN_COMMANDS = [
  "bootstrap_workspace",
  "create_pane",
  "close_pane",
  "rename_pane",
  "ensure_pane_terminal",
  "restart_pane_terminal",
  "write_to_pane",
  "resize_pane_terminal",
  "set_active_pane",
  "update_workspace_layout",
  "get_config",
  "write_config",
  "create_agent_pane",
  "send_agent_message",
  "agent_approval",
  "interrupt_agent",
  "install_update",
  "ui_smoke_enabled",
  "complete_ui_smoke",
];
const KNOWN_COMMAND_SET = new Set(KNOWN_COMMANDS);

// --- DOM skeleton (the ids main.js queries, mirroring ui/index.html) ---

const APP_HTML = `
  <main id="app" class="app-shell" data-ready="false">
    <header class="topbar">
      <p id="workspace-cwd">starting workspace</p>
      <div class="toolbar" aria-label="Pane actions">
        <button id="split-right" type="button">|</button>
        <button id="split-down" type="button">-</button>
        <button id="new-agent" type="button">Agent</button>
        <button id="close-pane" type="button">x</button>
        <button id="open-settings" type="button">S</button>
      </div>
    </header>
    <div id="pane-tabs"></div>
    <div id="layout-root"></div>
    <footer>
      <span id="pane-count">1 pane</span>
      <span id="active-pane">term-1</span>
      <span id="boot-status">booting</span>
    </footer>
  </main>
  <div id="search-bar" hidden>
    <input id="search-input" type="text" />
    <button id="search-prev" type="button"></button>
    <button id="search-next" type="button"></button>
    <button id="search-close" type="button"></button>
  </div>
  <div id="settings-overlay" hidden>
    <div id="settings-modal">
      <button id="settings-close" type="button"></button>
      <form id="settings-form">
        <label class="form-field"><input id="cfg-shell" type="text" /></label>
        <label class="form-field"><textarea id="cfg-shell-args"></textarea></label>
        <label class="form-field"><textarea id="cfg-env"></textarea></label>
        <label class="form-field"><input id="cfg-font-family" type="text" /></label>
        <label class="form-field"><input id="cfg-font-size" type="number" /></label>
        <label class="form-field"><textarea id="cfg-theme"></textarea></label>
        <label class="form-field"><input id="cfg-idle-shutdown" type="number" /></label>
        <label class="form-field"><select id="cfg-restore-policy"><option value=""></option></select></label>
        <div id="settings-error" hidden></div>
        <button id="settings-cancel" type="button"></button>
        <button id="settings-save" type="submit"></button>
      </form>
    </div>
  </div>
`;

// --- xterm / observer stubs ---

class FakeTerminal {
  static instances = [];

  constructor(options) {
    this.options = options;
    this.cols = 80;
    this.rows = 24;
    this.element = null;
    this.paneId = null;
    this.disposed = false;
    this.writes = [];
    this.focusCalls = 0;
    FakeTerminal.instances.push(this);
  }

  open(container) {
    this.paneId = container.dataset.paneId ?? null;
    this.element = document.createElement("div");
    container.append(this.element);
  }

  loadAddon() {}
  write(data) {
    this.writes.push(data);
  }
  reset() {}
  focus() {
    this.focusCalls += 1;
  }
  refresh() {}
  dispose() {
    this.disposed = true;
  }
  onData(cb) {
    this._onData = cb;
  }
  onResize(cb) {
    this._onResize = cb;
  }
}

// Records observed targets so tests can prove the observer is re-targeted to
// the live container after render() rebuilds the layout DOM (H3).
class FakeResizeObserver {
  constructor(callback) {
    this.callback = callback;
    this.targets = new Set();
  }
  observe(target) {
    this.targets.add(target);
  }
  unobserve(target) {
    this.targets.delete(target);
  }
  disconnect() {
    this.targets.clear();
  }
}

function terminalFor(paneId) {
  // Latest non-disposed instance opened for the pane.
  const matches = FakeTerminal.instances.filter(
    (t) => t.paneId === paneId && !t.disposed,
  );
  return matches[matches.length - 1] ?? null;
}

// Click the pencil rename button in the header of the currently active pane
// (the affordance that replaced the toolbar's "A" button).
function clickActivePaneRename() {
  const id = app.__test.state.activePaneId;
  const btn = document.querySelector(`.pane[data-pane-id="${id}"] .pane-rename`);
  expect(btn, `rename button for active pane ${id}`).not.toBeNull();
  btn.click();
}

// --- Stubbed native bridge ---

const invoked = [];
const unknownCommands = [];
const listeners = {};

let paneCounter = 0;
const backendPanes = new Map();
let backendActivePaneId = null;
let backendScrollback = {};
// (T1) agent_states served by bootstrap_workspace; tests mutate it to drive
// badge seeding and the resync refresh path.
let backendAgentStates = {};
// (T2) agent_events served by bootstrap_workspace (the per-pane replay tail).
let backendAgentEvents = {};
// (H2) Panes the daemon reports as "ended" in bootstrap's pane_states.
const backendEndedPanes = new Set();
// The config served by get_config; tests swap it to drive the settings modal.
let backendConfig = {};
// When true, get_config rejects (a wedged daemon) — pins the M2 save block.
let failGetConfig = false;
// (M2) send_agent_message injection: "fail" rejects immediately, "hang"
// never settles (drives the 60s invoke-timeout path).
let sendAgentMessageMode = "ok";
// (L3) When set, the invoke returns this promise so a test can close the
// pane while the call is still in flight.
let approvalGate = null;
let interruptGate = null;
const resizeGates = [];
let resizeFailuresRemaining = 0;

function backendPane(id, title) {
  const pane = { id, title, kind: "shell", created_at_ms: Date.now() };
  backendPanes.set(id, pane);
  return pane;
}

function backendSnapshot() {
  // Copy the pane records: the real bridge serializes the snapshot, so the
  // merge never shares object identity with the backend's bookkeeping (a
  // shared reference would mask kind/title flips from the upsert diff, L6).
  const panes = Array.from(backendPanes.values()).map((pane) => ({ ...pane }));
  return {
    panes,
    pane_states: Object.fromEntries(
      panes.map((p) => [p.id, backendEndedPanes.has(p.id) ? "ended" : "live"]),
    ),
    active_pane_id: backendPanes.has(backendActivePaneId)
      ? backendActivePaneId
      : panes[0]?.id ?? null,
    cwd: "/tmp/test-workspace",
    layout: null,
    scrollback: backendScrollback,
    agent_states: backendAgentStates,
    agent_events: backendAgentEvents,
  };
}

// Gate so the test can inject a pane-created event while the initial
// bootstrap_workspace response is still "in flight" (pins M11).
let releaseBootstrap;
const bootstrapGate = new Promise((resolve) => {
  releaseBootstrap = resolve;
});
let bootstrapCalls = 0;

async function stubInvoke(command, args = {}) {
  invoked.push({ command, args });
  if (!KNOWN_COMMAND_SET.has(command)) {
    unknownCommands.push(command);
    throw new Error(`Command ${command} not found`);
  }

  switch (command) {
    case "bootstrap_workspace":
      bootstrapCalls += 1;
      if (bootstrapCalls === 1) await bootstrapGate;
      return backendSnapshot();
    case "create_pane": {
      paneCounter += 1;
      return backendPane(`pane-created-${paneCounter}`, args.title || `term-${paneCounter}`);
    }
    case "create_agent_pane": {
      paneCounter += 1;
      const pane = backendPane(
        `agent-created-${paneCounter}`,
        args.title || `agent-${paneCounter}`,
      );
      pane.kind = "agent";
      return pane;
    }
    case "close_pane":
      backendPanes.delete(args.paneId);
      return null;
    case "rename_pane": {
      const pane = backendPanes.get(args.paneId);
      if (!pane) throw new Error(`Unknown pane: ${args.paneId}`);
      pane.title = args.title;
      return { ...pane };
    }
    case "get_config":
      if (failGetConfig) throw new Error("daemon wedged");
      return backendConfig;
    case "send_agent_message":
      if (sendAgentMessageMode === "fail") throw new Error("daemon dead");
      if (sendAgentMessageMode === "ack-then-fail") {
        listeners["agent-event"]({ payload: { pane_id: args.paneId, event: {
          kind: "user_message", text: args.text, message_id: args.messageId,
        } } });
        throw new Error("response connection lost");
      }
      if (sendAgentMessageMode === "hang") return new Promise(() => {});
      return { ok: true };
    case "agent_approval":
      if (approvalGate) return approvalGate;
      return { ok: true };
    case "interrupt_agent":
      if (interruptGate) return interruptGate;
      return { ok: true };
    case "resize_pane_terminal":
      if (resizeGates.length > 0) await resizeGates.shift();
      if (resizeFailuresRemaining > 0) {
        resizeFailuresRemaining -= 1;
        throw new Error("transient resize failure");
      }
      return { ok: true };
    default:
      // ensure/restart/write/resize/set-active/update-layout/write_config
      return { ok: true };
  }
}

// --- Helpers ---

async function waitFor(condition, what, timeoutMs = 3000) {
  const start = Date.now();
  while (!condition()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

function commandsInvoked(name) {
  return invoked.filter((call) => call.command === name);
}

function resizeCommandsFor(paneId) {
  return commandsInvoked("resize_pane_terminal").filter(
    (call) => call.args.paneId === paneId,
  );
}

function firstIndexOf(name) {
  return invoked.findIndex((call) => call.command === name);
}

let app; // the imported main.js module (test hooks under app.__test)

beforeAll(async () => {
  // React owns the complete application DOM; each mounted application starts
  // from a single explicit root rather than a pre-populated singleton shell.
  document.body.innerHTML = '<div id="root"></div>';

  globalThis.Terminal = FakeTerminal;
  globalThis.FitAddon = {
    FitAddon: class {
      fit() {}
    },
  };
  globalThis.ResizeObserver = FakeResizeObserver;

  window.__TAURI__ = {
    core: { invoke: stubInvoke },
    event: {
      listen: async (name, handler) => {
        listeners[name] = handler;
        return () => {
          delete listeners[name];
        };
      },
    },
  };

  backendPane("pane-a", "term-a");
  backendPane("pane-b", "term-b");
  // (T2) pane-c is an agent pane with a replayable conversation at boot.
  backendPane("pane-c", "agent-c");
  backendPanes.get("pane-c").kind = "agent";
  backendActivePaneId = "pane-a";
  backendScrollback = { "pane-a": "hello from a\r\n" };
  // (T1) pane-a has a classified agent at boot; the badge must appear from the
  // bootstrap snapshot without any agent-state event.
  backendAgentStates = { "pane-a": { agent: "claude", attention: "working" } };
  // (T2) The daemon's replay tail for pane-c: one completed assistant turn.
  backendAgentEvents = {
    "pane-c": [
      { kind: "session", session_id: "s-boot", model: "claude-x" },
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "replayed hello" },
      { kind: "message_complete" },
      { kind: "turn_complete", subtype: "success", cost_usd: 0.01, duration_ms: 1200 },
    ],
  };

  app = await import("../ui/src/main.js");
  await app.ready;

  // The initial snapshot request is now pending on the gate. Deliver a
  // pane-created event (a `ctl new` racing the snapshot) before releasing it.
  await waitFor(
    () => commandsInvoked("bootstrap_workspace").length === 1,
    "initial bootstrap_workspace invoke",
  );
  expect(listeners["pane-created"]).toBeTypeOf("function");
  listeners["pane-created"]({
    payload: { id: "pane-x", title: "ctl-new", kind: "shell", created_at_ms: Date.now() },
  });
  releaseBootstrap();

  await waitFor(
    () => document.querySelector("#app").dataset.ready === "true",
    "app ready",
  );
  await waitFor(
    () => commandsInvoked("ensure_pane_terminal").length >= 2,
    "boot ensure_pane_terminal calls",
  );
});

afterAll(() => {
  app?.__test.stopPeriodicResync();
});

describe("main.js boot against the stubbed native bridge", () => {
  it("boots to ready with the snapshot workspace", () => {
    expect(document.querySelector("#app").dataset.ready).toBe("true");
    expect(document.querySelector("#workspace-cwd").textContent).toBe("/tmp/test-workspace");
    expect(document.querySelector("#boot-status").textContent).toBe("native");
    expect(app.__test.state.panes.has("pane-a")).toBe(true);
    expect(app.__test.state.panes.has("pane-b")).toBe(true);
  });

  it("pins boot ordering: snapshot first, then config, then shell ensures", () => {
    expect(invoked[0].command).toBe("bootstrap_workspace");
    const bootstrapIdx = firstIndexOf("bootstrap_workspace");
    expect(firstIndexOf("get_config")).toBeGreaterThan(bootstrapIdx);
    expect(firstIndexOf("ensure_pane_terminal")).toBeGreaterThan(firstIndexOf("get_config"));
    const ensured = commandsInvoked("ensure_pane_terminal").map((c) => c.args.paneId);
    expect(ensured).toContain("pane-a");
    expect(ensured).toContain("pane-b");
  });

  it("creates terminals with convertEol: true (Windows ConPTY bare-LF streams)", () => {
    // Windows ConPTY passes VT-emitting TUIs (Claude Code/Ink) through with
    // bare \n; convertEol: false renders them as a staggered staircase. Unix
    // PTYs already deliver \r\n, so true is a no-op there.
    expect(FakeTerminal.instances.length).toBeGreaterThan(0);
    for (const term of FakeTerminal.instances) {
      expect(term.options.convertEol).toBe(true);
    }
  });

  it("exposes a pencil rename button in every pane header (toolbar rename removed)", () => {
    // The toolbar's old "A" button is gone; rename lives in the pane header.
    expect(document.querySelector("#rename-pane")).toBeNull();
    const headers = document.querySelectorAll(".pane-header");
    expect(headers.length).toBeGreaterThan(0);
    for (const header of headers) {
      const btn = header.querySelector(".pane-rename");
      expect(btn, "header has a rename button").not.toBeNull();
      expect(btn.textContent).toBe("✎");
      expect(btn.getAttribute("aria-label")).toBe("Rename pane");
      // Right-cluster order: rename is left-most (before badge and meta).
      const order = Array.from(header.children);
      const renameIdx = order.indexOf(btn);
      expect(renameIdx).toBeGreaterThan(-1);
      expect(renameIdx).toBeLessThan(order.indexOf(header.querySelector(".pane-meta")));
      const badge = header.querySelector(".agent-badge");
      if (badge) expect(renameIdx).toBeLessThan(order.indexOf(badge));
    }
  });

  it("keeps a pane created while the snapshot was in flight (M11)", async () => {
    expect(app.__test.state.panes.has("pane-x")).toBe(true);
    expect(app.__test.state.paneStates.get("pane-x")).toBe("live");
    // The pane is placed in the layout and rendered (reconcile or merge).
    await waitFor(
      () => document.querySelector('.pane[data-pane-id="pane-x"]'),
      "pane-x rendered",
    );
  });

  it("disposes the startup placeholder pane-1 view after bootstrap (M14)", () => {
    expect(app.__test.state.panes.has("pane-1")).toBe(false);
    expect(app.__test.state.terminalViews.has("pane-1")).toBe(false);
    const startupTerminals = FakeTerminal.instances.filter((t) => t.paneId === "pane-1");
    expect(startupTerminals.length).toBeGreaterThan(0);
    expect(startupTerminals.every((t) => t.disposed)).toBe(true);
  });

  it("drives split/write/resize/rename/restart/close through backend command names", async () => {
    // Split: create_pane + ensure_pane_terminal for the new pane.
    document.querySelector("#split-right").click();
    await waitFor(() => commandsInvoked("create_pane").length === 1, "create_pane");
    // The invoke is recorded when issued; wait for the response to land (the
    // new pane becomes active and is rendered) before driving it further.
    await waitFor(
      () => document.querySelector('.pane[data-pane-id^="pane-created-"]'),
      "created pane rendered",
    );
    const created = document.querySelector('.pane[data-pane-id^="pane-created-"]')
      .dataset.paneId;
    await waitFor(
      () => app.__test.state.activePaneId === created,
      "created pane active",
    );

    // Keystrokes: terminal onData → write_to_pane.
    const term = terminalFor(created);
    expect(term).not.toBeNull();
    term._onData("echo hi\n");
    await waitFor(() => commandsInvoked("write_to_pane").length >= 1, "write_to_pane");
    expect(commandsInvoked("write_to_pane")[0].args).toEqual({
      paneId: created,
      data: "echo hi\n",
    });

    // Grid change: terminal onResize → resize_pane_terminal.
    term._onResize({ cols: 120, rows: 40 });
    await waitFor(
      () =>
        commandsInvoked("resize_pane_terminal").some(
          (c) => c.args.paneId === created && c.args.rows === 40,
        ),
      "resize_pane_terminal",
    );

    // Inline rename via the pane-header pencil: Enter commits → rename_pane.
    clickActivePaneRename();
    const renameInput = document.querySelector(".pane-title-input");
    expect(renameInput).not.toBeNull();
    renameInput.value = "renamed-pane";
    renameInput.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
    );
    await waitFor(() => commandsInvoked("rename_pane").length === 1, "rename_pane");
    expect(commandsInvoked("rename_pane")[0].args).toEqual({
      paneId: created,
      title: "renamed-pane",
    });

    // pane-ended event → restart button → restart_pane_terminal.
    listeners["pane-ended"]({ payload: { pane_id: created } });
    await waitFor(
      () =>
        document.querySelector(
          `.pane[data-pane-id="${created}"] .pane-restore-button`,
        ),
      "restore button",
    );
    document
      .querySelector(`.pane[data-pane-id="${created}"] .pane-restore-button`)
      .click();
    await waitFor(
      () => commandsInvoked("restart_pane_terminal").length === 1,
      "restart_pane_terminal",
    );
    expect(commandsInvoked("restart_pane_terminal")[0].args).toEqual({ paneId: created });

    // Close the active pane: close_pane + set_active_pane for the survivor.
    document.querySelector("#close-pane").click();
    await waitFor(() => commandsInvoked("close_pane").length === 1, "close_pane");
    expect(commandsInvoked("close_pane")[0].args).toEqual({ paneId: created });
    await waitFor(() => commandsInvoked("set_active_pane").length >= 1, "set_active_pane");
    expect(app.__test.state.panes.has(created)).toBe(false);
  });

  it("serializes terminal resizes and applies only the newest queued geometry", async () => {
    // This assertion exercises the serializer directly. Drain/cancel any fit
    // work left by an earlier React render so an unrelated xterm fit cannot
    // become the newest queued geometry on a slower CI runner.
    const view = app.__test.state.terminalViews.get("pane-a");
    if (view) {
      // Keep the fake xterm's observable geometry aligned with the final
      // explicit resize. A fit that legitimately races this integration test
      // will then coalesce with, rather than overwrite, that final request.
      view.terminal.cols = 210;
      view.terminal.rows = 60;
    }
    if (view?.fitFrame) {
      window.cancelAnimationFrame(view.fitFrame);
      view.fitFrame = null;
    }
    await waitFor(() => {
      const sync = app.__test.state.terminalResizeSyncs.get("pane-a");
      return !sync?.inFlight && !sync?.pending && !sync?.retryTimer;
    }, "settled automatic terminal resize");

    let releaseFirst;
    resizeGates.push(
      new Promise((resolve) => {
        releaseFirst = resolve;
      }),
    );
    const before = resizeCommandsFor("pane-a").length;

    app.__test.syncTerminalSize("pane-a", 211, 61);
    await waitFor(
      () => resizeCommandsFor("pane-a").length === before + 1,
      "first serialized resize",
    );
    app.__test.syncTerminalSize("pane-a", 210, 60);
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(resizeCommandsFor("pane-a")).toHaveLength(before + 1);

    releaseFirst();
    await waitFor(
      () => resizeCommandsFor("pane-a").length === before + 2,
      "newest queued resize",
    );
    const calls = resizeCommandsFor("pane-a");
    expect(calls[calls.length - 1].args).toEqual({
      paneId: "pane-a",
      // The existing one-column xterm edge guard maps 210 visible columns to
      // a 209-column PTY; this assertion is about ordering, not guard policy.
      cols: 209,
      rows: 60,
    });
  });

  it("retries a transient terminal resize failure", async () => {
    resizeFailuresRemaining = 1;
    const before = resizeCommandsFor("pane-a").length;
    app.__test.syncTerminalSize("pane-a", 209, 59);
    await waitFor(
      () => resizeCommandsFor("pane-a").length >= before + 2,
      "terminal resize retry",
      1500,
    );
    const calls = resizeCommandsFor("pane-a").slice(before);
    expect(calls[0].args).toEqual(calls[1].args);
  });

  it("saves settings via write_config — the exact backend command name (H1)", async () => {
    document.querySelector("#open-settings").click();
    // The modal fetches the current config before it can be saved.
    await waitFor(() => commandsInvoked("get_config").length >= 2, "settings get_config");

    document.querySelector("#cfg-font-size").value = "14";
    document
      .querySelector("#settings-form")
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));

    await waitFor(() => commandsInvoked("write_config").length === 1, "write_config");
    expect(commandsInvoked("write_config")[0].args).toEqual({
      config: { font_size: 14 },
    });
    // A successful save closes the modal.
    await waitFor(
      () => document.querySelector("#settings-overlay").hidden === true,
      "settings modal closed",
    );
  });

  it("resync converges on the daemon's pane set after a restart (M12)", async () => {
    // Simulate a daemon restart that lost pane-b and pane-x but gained pane-r.
    backendPanes.delete("pane-b");
    backendPanes.delete("pane-x");
    backendPane("pane-r", "term-r");
    backendScrollback = { "pane-r": "restored\r\n" };

    const ensuresBefore = commandsInvoked("ensure_pane_terminal").length;
    await app.__test.resyncWorkspace();

    expect(app.__test.state.panes.has("pane-r")).toBe(true);
    expect(app.__test.state.panes.has("pane-b")).toBe(false);
    expect(app.__test.state.panes.has("pane-x")).toBe(false);
    expect(app.__test.state.paneStates.has("pane-b")).toBe(false);
    // Views for removed panes are disposed; the new pane gets a shell ensure.
    expect(app.__test.state.terminalViews.has("pane-b")).toBe(false);
    const paneBTerminals = FakeTerminal.instances.filter((t) => t.paneId === "pane-b");
    expect(paneBTerminals.length).toBeGreaterThan(0);
    expect(paneBTerminals.every((t) => t.disposed)).toBe(true);
    await waitFor(
      () =>
        commandsInvoked("ensure_pane_terminal").length > ensuresBefore &&
        commandsInvoked("ensure_pane_terminal").some((c) => c.args.paneId === "pane-r"),
      "ensure for pane-r",
    );
    await waitFor(
      () => document.querySelector('.pane[data-pane-id="pane-r"]'),
      "pane-r rendered",
    );
    // The surviving active pane still exists.
    expect(app.__test.state.panes.has(app.__test.state.activePaneId)).toBe(true);
  });

  it("resync pushes kept local focus back to a diverged daemon", async () => {
    const focused = app.__test.state.activePaneId;
    expect(app.__test.state.panes.has(focused)).toBe(true);
    // Point the daemon's active pane somewhere else (a restarted daemon's
    // default choice), keeping the user's focus where it is.
    const other = Array.from(app.__test.state.panes.keys()).find(
      (id) => id !== focused,
    );
    expect(other).toBeTruthy();
    backendActivePaneId = other;

    const before = commandsInvoked("set_active_pane").length;
    await app.__test.resyncWorkspace();

    // The GUI keeps the user's focus and re-asserts it to the daemon.
    expect(app.__test.state.activePaneId).toBe(focused);
    await waitFor(
      () => commandsInvoked("set_active_pane").length > before,
      "set_active_pane after resync divergence",
    );
    const calls = commandsInvoked("set_active_pane");
    expect(calls[calls.length - 1].args).toEqual({ paneId: focused });
    backendActivePaneId = focused;
  });

  it("resync defers while an inline rename is in progress", async () => {
    clickActivePaneRename();
    const renameInput = document.querySelector(".pane-title-input");
    expect(renameInput).not.toBeNull();

    backendPane("pane-z", "term-z");
    await app.__test.resyncWorkspace();
    // Deferred: the new pane is not merged while the rename is open.
    expect(app.__test.state.panes.has("pane-z")).toBe(false);
    expect(document.querySelector(".pane-title-input")).not.toBeNull();

    renameInput.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    await waitFor(
      () => !document.querySelector(".pane-title-input"),
      "rename input closed",
    );

    await app.__test.resyncWorkspace();
    expect(app.__test.state.panes.has("pane-z")).toBe(true);
    await waitFor(
      () => document.querySelector('.pane[data-pane-id="pane-z"]'),
      "pane-z rendered after rename closed",
    );
  });

  it("keeps the terminal container and ResizeObserver stable across React renders", () => {
    const view = app.__test.state.terminalViews.get("pane-a");
    expect(view).toBeTruthy();
    const observer = view.resizeObserver;
    const before = document.querySelector('.pane[data-pane-id="pane-a"] .terminal');
    expect(before).not.toBeNull();
    expect(observer.targets.has(before)).toBe(true);

    // React preserves the keyed pane and its imperative xterm host.
    app.__test.render();

    const after = document.querySelector('.pane[data-pane-id="pane-a"] .terminal');
    expect(after).not.toBeNull();
    expect(after).toBe(before);
    expect(after.isConnected).toBe(true);
    // The observer remains on the same live host; unrelated renders do not
    // detach xterm or cause spurious PTY lifecycle/resize churn.
    expect(observer.targets.has(before)).toBe(true);
    expect(observer.targets.has(after)).toBe(true);
  });

  it("keeps env values through a scrubbed config-changed + save round-trip (M1)", async () => {
    backendConfig = { env: { FOO: "bar" }, font_size: 12 };
    document.querySelector("#open-settings").click();
    await waitFor(
      () => document.querySelector("#cfg-env").value === "FOO=bar",
      "env populated from get_config",
    );

    // The daemon's config-changed summary nulls env values ({"FOO": null}).
    // It must act only as a trigger for a fresh get_config — never as form
    // data — or the next save writes the values back as empty strings.
    const getConfigBefore = commandsInvoked("get_config").length;
    listeners["config-changed"]({ payload: { env: { FOO: null } } });
    await waitFor(
      () => commandsInvoked("get_config").length > getConfigBefore,
      "config-changed triggers a get_config refetch",
    );
    // Let the refetch's populate land before asserting on the field.
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(document.querySelector("#cfg-env").value).toBe("FOO=bar");

    // Edit another field and save: the written payload keeps the real env.
    document.querySelector("#cfg-font-size").value = "15";
    document
      .querySelector("#cfg-font-size")
      .dispatchEvent(new Event("input", { bubbles: true }));
    const writesBefore = commandsInvoked("write_config").length;
    document
      .querySelector("#settings-form")
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await waitFor(
      () => commandsInvoked("write_config").length > writesBefore,
      "write_config after config-changed",
    );
    const writes = commandsInvoked("write_config");
    const lastWrite = writes[writes.length - 1];
    expect(lastWrite.args.config.env).toEqual({ FOO: "bar" });
    expect(lastWrite.args.config.font_size).toBe(15);
    await waitFor(
      () => document.querySelector("#settings-overlay").hidden === true,
      "settings modal closed",
    );
    backendConfig = {};
  });

  it("preserves config fields not exposed by the settings form", async () => {
    backendConfig = {
      font_size: 12,
      scrub_env: ["SECRET_TOKEN"],
      agent_permission_mode: "plan",
      agent_claude_bin: "C:\\tools\\claude.exe",
    };
    document.querySelector("#open-settings").click();
    await waitFor(
      () => document.querySelector("#cfg-font-size").value === "12",
      "full config populated",
    );

    document.querySelector("#cfg-font-size").value = "16";
    document
      .querySelector("#cfg-font-size")
      .dispatchEvent(new Event("input", { bubbles: true }));
    const writesBefore = commandsInvoked("write_config").length;
    document
      .querySelector("#settings-form")
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await waitFor(
      () => commandsInvoked("write_config").length > writesBefore,
      "write_config with passthrough fields",
    );
    const writes = commandsInvoked("write_config");
    expect(writes[writes.length - 1].args.config).toMatchObject({
      font_size: 16,
      scrub_env: ["SECRET_TOKEN"],
      agent_permission_mode: "plan",
      agent_claude_bin: "C:\\tools\\claude.exe",
    });
    await waitFor(
      () => document.querySelector("#settings-overlay").hidden === true,
      "settings modal closed",
    );
    backendConfig = {};
  });

  it("blocks saving when the config failed to load into the modal (M2)", async () => {
    failGetConfig = true;
    const writesBefore = commandsInvoked("write_config").length;
    document.querySelector("#open-settings").click();
    await waitFor(
      () => document.querySelector("#settings-error").hidden === false,
      "load error surfaced in the modal",
    );
    expect(document.querySelector("#settings-error").textContent).toContain(
      "failed to load",
    );
    expect(document.querySelector("#settings-save").disabled).toBe(true);

    // Submitting in this state must not write anything (the form never saw
    // the daemon's config; a write would replace it with near-defaults).
    document
      .querySelector("#settings-form")
      .dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(commandsInvoked("write_config").length).toBe(writesBefore);

    // A successful (re)load re-enables Save.
    failGetConfig = false;
    document.querySelector("#settings-close").click();
    document.querySelector("#open-settings").click();
    await waitFor(
      () => document.querySelector("#settings-save").disabled === false,
      "save re-enabled after a successful reload",
    );
    document.querySelector("#settings-close").click();
  });

  it("does not steal focus from the open settings modal on background renders (M3)", async () => {
    document.querySelector("#open-settings").click();
    await waitFor(
      () => document.querySelector("#settings-overlay").hidden === false,
      "settings modal open",
    );
    const term = terminalFor(app.__test.state.activePaneId);
    expect(term).not.toBeNull();
    const focusBefore = term.focusCalls;

    // A background pane-ended renders while the modal is open; the queued
    // focus microtask must no-op instead of yanking focus into the terminal.
    listeners["pane-ended"]({ payload: { pane_id: "pane-r" } });
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(term.focusCalls).toBe(focusBefore);

    // Revive the pane and close the modal so later tests see a clean state.
    listeners["pty-output"]({ payload: { pane_id: "pane-r", data: "" } });
    document.querySelector("#settings-close").click();
  });

  it("cleans up the divider drag on pointercancel", async () => {
    const divider = document.querySelector(".divider");
    expect(divider).not.toBeNull();
    divider.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true, cancelable: true }));
    expect(app.__test.state.drag).not.toBeNull();

    const persistsBefore = commandsInvoked("update_workspace_layout").length;
    window.dispatchEvent(new Event("pointercancel"));
    expect(app.__test.state.drag).toBeNull();
    await waitFor(
      () => commandsInvoked("update_workspace_layout").length > persistsBefore,
      "layout persisted after pointercancel",
    );

    // The drag listeners are gone: a stray pointermove must not revive it.
    window.dispatchEvent(new MouseEvent("pointermove"));
    expect(app.__test.state.drag).toBeNull();
  });

  it("capBufferedOutput does not start the kept text mid-ANSI-sequence", () => {
    const cap = 256 * 1024;
    // No newline anywhere, so the cap falls back to a raw slice; the cut lands
    // inside the escape sequence's parameter run.
    const tail = "b".repeat(cap - 5);

    const csiText = `aa\x1b[999999999m${tail}`;
    const cappedCsi = app.__test.capBufferedOutput(csiText);
    expect(cappedCsi.length).toBeLessThanOrEqual(cap);
    // Skips the whole straddled CSI sequence instead of keeping its tail.
    expect(cappedCsi).toBe(tail);

    const oscText = `aa\x1b]8;;http://example.com\x07${tail}`;
    const cappedOsc = app.__test.capBufferedOutput(oscText);
    expect(cappedOsc.length).toBeLessThanOrEqual(cap);
    expect(cappedOsc).toBe(tail);

    // A sequence that ends before the cut is unaffected.
    const safeText = `\x1b[31m${"a".repeat(cap)}`;
    expect(app.__test.capBufferedOutput(safeText)).toBe("a".repeat(cap));
  });

  it("defers the reconcile render while an inline rename is in progress", async () => {
    clickActivePaneRename();
    const renameInput = document.querySelector(".pane-title-input");
    expect(renameInput).not.toBeNull();

    listeners["pane-created"]({
      payload: { id: "pane-q", title: "term-q", kind: "shell", created_at_ms: Date.now() },
    });
    expect(app.__test.state.panes.has("pane-q")).toBe(true);
    // The 120ms reconcile debounce fires while the rename is open and must
    // defer — a render would destroy the typed title.
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(document.querySelector('.pane[data-pane-id="pane-q"]')).toBeNull();
    expect(document.querySelector(".pane-title-input")).not.toBeNull();

    // Finishing the rename lets the rescheduled reconcile place the pane.
    renameInput.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }),
    );
    await waitFor(
      () => document.querySelector('.pane[data-pane-id="pane-q"]'),
      "pane-q placed after the rename settled",
    );
  });

  it("shows the update banner on update-available and invokes install_update on click (M12)", async () => {
    expect(listeners["update-available"]).toBeTypeOf("function");
    listeners["update-available"]({ payload: { version: "0.2.0", body: null } });

    const banner = document.querySelector("#update-banner");
    expect(banner).not.toBeNull();
    const updateButton = banner.querySelector(".update-banner-button");
    expect(updateButton.textContent).toBe("Update to v0.2.0 and restart");

    updateButton.click();
    await waitFor(
      () => commandsInvoked("install_update").length === 1,
      "install_update invoked",
    );
  });

  it("dismisses the update banner without invoking install_update", () => {
    listeners["update-available"]({ payload: { version: "0.3.0", body: "notes" } });
    const banner = document.querySelector("#update-banner");
    expect(banner).not.toBeNull();
    expect(banner.querySelector(".update-banner-button").textContent).toBe(
      "Update to v0.3.0 and restart",
    );

    banner.querySelector(".update-banner-dismiss").click();
    expect(document.querySelector("#update-banner")).toBeNull();
    // Still exactly the one invoke from the previous test.
    expect(commandsInvoked("install_update").length).toBe(1);
  });
});

// (T1) Agent-aware panes: bootstrap seeding, agent-state event transitions,
// badge lifecycle, and the needs_input attention affordance. Runs against the
// same booted app instance; at this point the workspace holds pane-a (seeded
// with a working agent), pane-c (T2 agent pane), pane-r, pane-z, and pane-q.
describe("(T1) agent-aware panes", () => {
  // Tabs carry no pane id; find one by its title attribute.
  function tabFor(title) {
    return (
      Array.from(document.querySelectorAll("#pane-tabs .pane-tab")).find(
        (tab) => tab.title === title,
      ) ?? null
    );
  }

  function emitAgentState(paneId, agent, attention) {
    listeners["agent-state"]({
      payload: { pane_id: paneId, agent, attention },
    });
  }

  it("seeds agent badges from the bootstrap snapshot", () => {
    expect(listeners["agent-state"]).toBeTypeOf("function");
    expect(app.__test.state.agentStates.get("pane-a")).toEqual({
      agent: "claude",
      attention: "working",
    });

    // The rail tab carries the badge with the contract attention class.
    const tab = tabFor("term-a");
    expect(tab).not.toBeNull();
    const badge = tab.querySelector(".agent-badge");
    expect(badge).not.toBeNull();
    expect(badge.textContent).toBe("◆");
    expect(badge.classList.contains("agent-badge-working")).toBe(true);

    // The pane header mirrors the same badge.
    const headerBadge = document.querySelector(
      '.pane[data-pane-id="pane-a"] .pane-header .agent-badge',
    );
    expect(headerBadge).not.toBeNull();
    expect(headerBadge.classList.contains("agent-badge-working")).toBe(true);
  });

  it("applies the contract classes on working/needs_input/idle transitions", () => {
    for (const attention of ["needs_input", "idle", "working"]) {
      emitAgentState("pane-r", "claude", attention);
      const badge = tabFor("term-r").querySelector(".agent-badge");
      expect(badge).not.toBeNull();
      expect(badge.classList.contains(`agent-badge-${attention}`)).toBe(true);
    }
    // The header mirror follows the latest transition.
    const headerBadge = document.querySelector(
      '.pane[data-pane-id="pane-r"] .pane-header .agent-badge',
    );
    expect(headerBadge.classList.contains("agent-badge-working")).toBe(true);
    // Settle to a non-attention state for later tests.
    emitAgentState("pane-r", "claude", "idle");
  });

  it("clears the badge when the agent goes away", () => {
    emitAgentState("pane-r", "claude", "working");
    expect(tabFor("term-r").querySelector(".agent-badge")).not.toBeNull();

    emitAgentState("pane-r", null, null);
    expect(app.__test.state.agentStates.has("pane-r")).toBe(false);
    expect(tabFor("term-r").querySelector(".agent-badge")).toBeNull();
    expect(
      document.querySelector(
        '.pane[data-pane-id="pane-r"] .pane-header .agent-badge',
      ),
    ).toBeNull();
  });

  it("ignores agent-state events for unknown panes", () => {
    const badgesBefore = document.querySelectorAll(".agent-badge").length;
    emitAgentState("ghost-pane", "claude", "working");
    expect(app.__test.state.agentStates.has("ghost-pane")).toBe(false);
    expect(document.querySelector('.pane[data-pane-id="ghost-pane"]')).toBeNull();
    // No render, no new badge anywhere.
    expect(document.querySelectorAll(".agent-badge").length).toBe(badgesBefore);
  });

  it("drops the badge state when the pane closes", () => {
    listeners["pane-created"]({
      payload: {
        id: "pane-agent-tmp",
        title: "term-agent-tmp",
        kind: "shell",
        created_at_ms: Date.now(),
      },
    });
    emitAgentState("pane-agent-tmp", "claude", "working");
    expect(app.__test.state.agentStates.has("pane-agent-tmp")).toBe(true);

    listeners["pane-closed"]({ payload: { pane_id: "pane-agent-tmp" } });
    expect(app.__test.state.agentStates.has("pane-agent-tmp")).toBe(false);
    expect(tabFor("term-agent-tmp")).toBeNull();
  });

  it("surfaces a global affordance when a background agent needs input, dismissed by focusing", () => {
    const active = app.__test.state.activePaneId;
    const target = Array.from(app.__test.state.panes.keys()).find(
      (id) => id !== active,
    );
    expect(target).toBeTruthy();

    emitAgentState(target, "claude", "needs_input");
    const badge = document.querySelector(".agent-attention-badge");
    expect(badge).not.toBeNull();
    expect(badge.textContent).toBe("◆ 1 agent needs input");

    // Clicking the affordance focuses the pane, which dismisses it.
    badge.click();
    expect(app.__test.state.activePaneId).toBe(target);
    expect(document.querySelector(".agent-attention-badge")).toBeNull();

    // needs_input on the ACTIVE pane surfaces nothing — the user is looking at it.
    emitAgentState(target, "claude", "needs_input");
    expect(document.querySelector(".agent-attention-badge")).toBeNull();
    // Cleanup: settle the pane back to idle.
    emitAgentState(target, "claude", "idle");
  });

  it("aggregates multiple background needs_input panes into one affordance", () => {
    const active = app.__test.state.activePaneId;
    const others = Array.from(app.__test.state.panes.keys()).filter(
      (id) => id !== active,
    );
    expect(others.length).toBeGreaterThanOrEqual(2);
    const [first, second] = others;

    emitAgentState(first, "claude", "needs_input");
    emitAgentState(second, "claude", "needs_input");
    expect(document.querySelector(".agent-attention-badge").textContent).toBe(
      "◆ 2 agents need input",
    );

    // One settling drops the count; the last clearing removes the badge.
    emitAgentState(first, "claude", "working");
    expect(document.querySelector(".agent-attention-badge").textContent).toBe(
      "◆ 1 agent needs input",
    );
    emitAgentState(second, "claude", "working");
    expect(document.querySelector(".agent-attention-badge")).toBeNull();
    // Cleanup.
    emitAgentState(first, "claude", "idle");
    emitAgentState(second, "claude", "idle");
  });

  it("refreshes agent states from the snapshot on resync", async () => {
    // The (possibly restarted) daemon now reports pane-a idle (was working) and
    // pane-r with an agent; every other snapshot-covered pane has none.
    backendAgentStates = {
      "pane-a": { agent: "claude", attention: "idle" },
      "pane-r": { agent: "claude", attention: "working" },
    };
    await app.__test.resyncWorkspace();

    expect(app.__test.state.agentStates.get("pane-a")).toEqual({
      agent: "claude",
      attention: "idle",
    });
    expect(app.__test.state.agentStates.get("pane-r")).toEqual({
      agent: "claude",
      attention: "working",
    });
    // Snapshot-covered panes absent from agent_states converge to cleared, so a
    // stale badge can't survive a missed agent-state event.
    expect(app.__test.state.agentStates.has("pane-z")).toBe(false);

    const badge = tabFor("term-a").querySelector(".agent-badge");
    expect(badge.classList.contains("agent-badge-idle")).toBe(true);

    // Restore the seeded state so the run ends where it started.
    backendAgentStates = { "pane-a": { agent: "claude", attention: "working" } };
    await app.__test.resyncWorkspace();
    expect(app.__test.state.agentStates.get("pane-a")).toEqual({
      agent: "claude",
      attention: "working",
    });
    expect(app.__test.state.agentStates.has("pane-r")).toBe(false);
  });
});


// (T2) Agent chat panes: boot replay seeding, live agent-event streaming, the
// composer (send/busy/interrupt), permission cards, and the create-agent
// affordance. Runs against the same booted app instance; pane-c is the
// boot-time agent pane with a replayed conversation.
describe("(T2) agent chat panes", () => {
  function agentEvent(paneId, event) {
    listeners["agent-event"]({ payload: { pane_id: paneId, event } });
  }

  function chatRoot(paneId) {
    return document.querySelector(`.pane[data-pane-id="${paneId}"] .chat-root`);
  }

  it("boots the agent pane with a replayed chat view — no xterm, no shell wiring", () => {
    const chat = app.__test.state.chats.get("pane-c");
    expect(chat).toBeTruthy();
    expect(chat.sessionId).toBe("s-boot");
    expect(chat.model).toBe("claude-x");
    expect(chat.messages).toHaveLength(1);
    expect(chat.messages[0]).toEqual({
      type: "assistant",
      text: "replayed hello",
      open: false,
    });
    expect(chat.busy).toBe(false);
    expect(chat.lastTurn.subtype).toBe("success");

    // The replayed conversation is rendered (markdown into the bubble).
    const root = chatRoot("pane-c");
    expect(root).not.toBeNull();
    expect(
      root.querySelector(".chat-msg-assistant .chat-bubble").textContent,
    ).toBe("replayed hello");
    expect(root.querySelector(".chat-turn-footer").textContent).toBe(
      "success · $0.0100 · 1.2s",
    );
    // Idle after the replayed turn: composer usable.
    expect(root.querySelector(".chat-input").disabled).toBe(false);
    expect(root.querySelector(".chat-send").textContent).toBe("Send");
    // Pre-provider snapshots default agent panes to Claude.
    expect(
      document.querySelector('.pane[data-pane-id="pane-c"] .pane-meta').textContent,
    ).toBe("claude");

    // No terminal was created or wired for the agent pane.
    expect(document.querySelector('.pane[data-pane-id="pane-c"] .terminal-xterm')).toBeNull();
    expect(terminalFor("pane-c")).toBeNull();
    expect(
      commandsInvoked("ensure_pane_terminal").every((c) => c.args.paneId !== "pane-c"),
    ).toBe(true);
    expect(
      commandsInvoked("resize_pane_terminal").every((c) => c.args.paneId !== "pane-c"),
    ).toBe(true);
    expect(app.__test.state.terminalViews.has("pane-c")).toBe(false);
  });

  it("ignores agent-event payloads for unknown panes", () => {
    agentEvent("ghost-pane", { kind: "text_delta", text: "boo" });
    expect(app.__test.state.chats.has("ghost-pane")).toBe(false);
  });

  it("streams a live agent-event turn into the chat view", async () => {
    agentEvent("pane-c", { kind: "message_start", role: "assistant" });
    agentEvent("pane-c", { kind: "text_delta", text: "live " });
    agentEvent("pane-c", { kind: "text_delta", text: "stream" });
    await waitFor(() => {
      const bubbles = chatRoot("pane-c").querySelectorAll(
        ".chat-msg-assistant .chat-bubble",
      );
      return bubbles[bubbles.length - 1]?.textContent === "live stream";
    }, "live assistant bubble");
    expect(app.__test.state.chats.get("pane-c").busy).toBe(true);

    agentEvent("pane-c", { kind: "message_complete" });
    agentEvent("pane-c", { kind: "turn_complete", subtype: "success" });
    await waitFor(
      () => app.__test.state.chats.get("pane-c").busy === false,
      "turn settled",
    );
  });

  it("composer Enter sends the message, appends the user bubble, and gates on busy", async () => {
    const root = chatRoot("pane-c");
    const input = root.querySelector(".chat-input");
    input.value = "hello agent";
    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
    );
    await waitFor(
      () => commandsInvoked("send_agent_message").length === 1,
      "send_agent_message",
    );
    expect(commandsInvoked("send_agent_message")[0].args).toEqual({
      paneId: "pane-c",
      text: "hello agent",
      messageId: expect.any(String),
    });
    await waitFor(
      () =>
        root.querySelector(".chat-msg-user .chat-bubble")?.textContent ===
        "hello agent",
      "user bubble",
    );
    // The composer clears and disables while the turn runs; the action
    // button becomes Interrupt.
    expect(input.value).toBe("");
    await waitFor(() => input.disabled === true, "composer disabled while busy");
    expect(root.querySelector(".chat-send").textContent).toBe("Interrupt");
  });

  it("Interrupt while busy invokes interrupt_agent; turn_complete re-enables", async () => {
    const root = chatRoot("pane-c");
    root.querySelector(".chat-send").click();
    await waitFor(
      () => commandsInvoked("interrupt_agent").length === 1,
      "interrupt_agent",
    );
    expect(commandsInvoked("interrupt_agent")[0].args).toEqual({ paneId: "pane-c" });

    agentEvent("pane-c", { kind: "turn_complete", subtype: "error_during_execution" });
    await waitFor(
      () => root.querySelector(".chat-input").disabled === false,
      "composer re-enabled",
    );
    expect(root.querySelector(".chat-send").textContent).toBe("Send");
  });

  it("permission card Allow invokes agent_approval and clears the card", async () => {
    agentEvent("pane-c", {
      kind: "permission_request",
      request_id: "req-1",
      tool_name: "Bash",
      input: { command: "ls -la" },
    });
    await waitFor(
      () => chatRoot("pane-c").querySelector(".chat-permission"),
      "permission card",
    );
    const card = chatRoot("pane-c").querySelector(".chat-permission");
    expect(card.querySelector(".chat-permission-title").textContent).toBe(
      "Bash wants permission",
    );
    expect(card.querySelector(".chat-tool-pre").textContent).toContain("ls -la");

    card.querySelector(".chat-allow").click();
    await waitFor(
      () => commandsInvoked("agent_approval").length === 1,
      "agent_approval",
    );
    expect(commandsInvoked("agent_approval")[0].args).toEqual({
      paneId: "pane-c",
      requestId: "req-1",
      allow: true,
      message: null,
    });
    await waitFor(
      () => !chatRoot("pane-c").querySelector(".chat-permission"),
      "card cleared after approval",
    );
  });

  it("permission card Deny collects a reason inline (no prompt)", async () => {
    agentEvent("pane-c", {
      kind: "permission_request",
      request_id: "req-2",
      tool_name: "Write",
      input: { path: "/tmp/x" },
    });
    await waitFor(
      () => chatRoot("pane-c").querySelector(".chat-permission"),
      "permission card",
    );

    chatRoot("pane-c").querySelector(".chat-deny").click();
    await waitFor(
      () => chatRoot("pane-c").querySelector(".chat-deny-input"),
      "deny reason input",
    );
    const reason = chatRoot("pane-c").querySelector(".chat-deny-input");
    expect(reason).not.toBeNull();
    reason.value = "not safe";
    reason.dispatchEvent(new Event("input", { bubbles: true }));
    chatRoot("pane-c").querySelector(".chat-deny-send").click();

    await waitFor(
      () => commandsInvoked("agent_approval").length === 2,
      "deny approval",
    );
    expect(commandsInvoked("agent_approval")[1].args).toEqual({
      paneId: "pane-c",
      requestId: "req-2",
      allow: false,
      message: "not safe",
    });
    await waitFor(
      () => !chatRoot("pane-c").querySelector(".chat-permission"),
      "card cleared after denial",
    );
  });

  it("renders tool cards with results from the event stream", async () => {
    agentEvent("pane-c", {
      kind: "tool_use",
      id: "toolu-9",
      name: "Read",
      input: { file_path: "/tmp/readme" },
    });
    agentEvent("pane-c", {
      kind: "tool_result",
      tool_use_id: "toolu-9",
      content: "file body",
      is_error: false,
    });
    await waitFor(
      () => chatRoot("pane-c").querySelector("details.chat-tool"),
      "tool card",
    );
    const card = chatRoot("pane-c").querySelector("details.chat-tool");
    expect(card.querySelector(".chat-tool-name").textContent).toBe("Read");
    const pres = card.querySelectorAll(".chat-tool-pre");
    expect(pres[0].textContent).toContain("/tmp/readme");
    expect(pres[1].textContent).toBe("file body");
  });

  it("Agent toolbar button creates an agent pane with a chat view", async () => {
    document.querySelector("#new-agent").click();
    await waitFor(
      () => commandsInvoked("create_agent_pane").length === 1,
      "create_agent_pane",
    );
    expect(commandsInvoked("create_agent_pane")[0].args).toEqual({
      title: null,
      backend: "claude",
      model: null,
    });

    await waitFor(() => {
      const active = app.__test.state.activePaneId;
      return active?.startsWith("agent-created-") && chatRoot(active);
    }, "agent pane active with chat view");
    const agentPaneId = app.__test.state.activePaneId;
    expect(app.__test.state.panes.get(agentPaneId).kind).toBe("agent");
    // A fresh agent pane gets the empty-state hint, no xterm, no ensure.
    expect(chatRoot(agentPaneId).querySelector(".chat-empty")).not.toBeNull();
    expect(terminalFor(agentPaneId)).toBeNull();
    expect(
      commandsInvoked("ensure_pane_terminal").every((c) => c.args.paneId !== agentPaneId),
    ).toBe(true);
  });

  it("creates a Droid pane with an explicit routed model", async () => {
    const backend = document.querySelector("#new-agent-backend");
    backend.value = "droid";
    backend.dispatchEvent(new Event("change", { bubbles: true }));
    await waitFor(
      () => app.__test.state.newAgentBackend === "droid",
      "provider control update",
    );

    // The provider change re-renders the toolbar, so target the current input.
    const model = document.querySelector("#new-agent-model");
    const setInputValue = Object.getOwnPropertyDescriptor(
      window.HTMLInputElement.prototype,
      "value",
    ).set;
    setInputValue.call(model, "custom:Fireworks-Qwen-0");
    model.dispatchEvent(new Event("input", { bubbles: true }));
    await waitFor(
      () => app.__test.state.newAgentModel === "custom:Fireworks-Qwen-0",
      "model control update",
    );

    document.querySelector("#new-agent").click();
    await waitFor(
      () => commandsInvoked("create_agent_pane").length === 2,
      "provider-aware create_agent_pane",
    );
    expect(commandsInvoked("create_agent_pane")[1].args).toEqual({
      title: null,
      backend: "droid",
      model: "custom:Fireworks-Qwen-0",
    });

    await waitFor(() => {
      const active = app.__test.state.activePaneId;
      return (
        active?.startsWith("agent-created-") &&
        app.__test.state.agentSpecs.get(active)?.backend === "droid"
      );
    }, "Droid pane active");
    const paneId = app.__test.state.activePaneId;
    expect(
      document.querySelector(`.pane[data-pane-id="${paneId}"] .pane-meta`).textContent,
    ).toBe("droid · custom:Fireworks-Qwen-0");
  });

  it("surfaces error lines and the exited hint", async () => {
    const paneId = app.__test.state.activePaneId; // the agent-created pane
    agentEvent(paneId, { kind: "error", message: "stream broke" });
    agentEvent(paneId, { kind: "process_exit", exit_code: 3 });
    await waitFor(
      () => chatRoot(paneId).querySelector(".chat-exited"),
      "exited hint",
    );
    expect(chatRoot(paneId).querySelector(".chat-exited").textContent).toBe(
      "agent exited — send a message to restart it",
    );
    expect(chatRoot(paneId).querySelector(".chat-error").textContent).toBe(
      "stream broke",
    );
    expect(app.__test.state.chats.get(paneId).exitCode).toBe(3);
  });

  it("drops the chat state when the agent pane closes", () => {
    const paneId = app.__test.state.activePaneId; // the agent-created pane
    expect(app.__test.state.chats.has(paneId)).toBe(true);
    listeners["pane-closed"]({ payload: { pane_id: paneId } });
    expect(app.__test.state.chats.has(paneId)).toBe(false);
    expect(app.__test.state.chatViews.has(paneId)).toBe(false);
    expect(chatRoot(paneId)).toBeNull();
  });
});

// (Review fixes) H1/H2/M1/M2/M3/L1/L2/L3/L4/L6 regression coverage. Runs
// against the same booted app instance; at this point the workspace holds
// pane-a, pane-c (agent, idle), pane-r, and pane-z.
describe("(T2) review fixes", () => {
  function agentEvent(paneId, event) {
    listeners["agent-event"]({ payload: { pane_id: paneId, event } });
  }

  function chatRoot(paneId) {
    return document.querySelector(`.pane[data-pane-id="${paneId}"] .chat-root`);
  }

  // Create an agent pane through the toolbar flow and wait until it is the
  // active pane with a mounted chat view. Returns its id.
  async function createScratchAgentPane() {
    document.querySelector("#new-agent").click();
    await waitFor(() => {
      const active = app.__test.state.activePaneId;
      return active?.startsWith("agent-created-") && chatRoot(active);
    }, "scratch agent pane active with chat view");
    return app.__test.state.activePaneId;
  }

  // Mirror the daemon agreeing with the GUI on a closed pane.
  function closePaneEverywhere(paneId) {
    listeners["pane-closed"]({ payload: { pane_id: paneId } });
    backendPanes.delete(paneId);
    delete backendAgentEvents[paneId];
  }

  it("clamps a freshly-seeded chat when the snapshot says the pane ended (H2)", async () => {
    // Prune backend panes the GUI already closed (earlier suites closed panes
    // via events only) so this resync does not "revive" them.
    for (const id of Array.from(backendPanes.keys())) {
      if (!app.__test.state.panes.has(id)) backendPanes.delete(id);
    }
    // The daemon died mid-turn: the replay tail ends mid-stream with a still
    // pending permission request, and pane_states reports the pane ended.
    const dead = backendPane("pane-dead", "agent-dead");
    dead.kind = "agent";
    backendEndedPanes.add("pane-dead");
    backendAgentEvents["pane-dead"] = [
      { kind: "session", session_id: "s-dead", model: "claude-x" },
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "partial answer" },
      { kind: "permission_request", request_id: "req-dead", tool_name: "Bash" },
    ];
    await app.__test.resyncWorkspace();

    const chat = app.__test.state.chats.get("pane-dead");
    expect(chat).toBeTruthy();
    expect(chat.messages).toHaveLength(1);
    expect(chat.busy).toBe(false); // clamped — the composer is not wedged
    expect(chat.pendingPermission).toBeNull(); // zombie card gone
    expect(chat.exited).toBe(true); // surfaces the "send to restart" hint
    expect(chat.messages.at(-1).open).toBe(false); // no stale streaming bubble

    const root = chatRoot("pane-dead");
    expect(root).not.toBeNull();
    expect(root.querySelector(".chat-msg-open")).toBeNull();
    expect(root.querySelector(".chat-exited")).not.toBeNull();
    expect(root.querySelector(".chat-input").disabled).toBe(false);
  });

  it("keeps a mid-turn chat busy while the pane is still live (H2)", async () => {
    // A mid-turn tail on a LIVE pane stays busy (no clamping when healthy).
    const live = backendPane("pane-live-mid", "agent-live-mid");
    live.kind = "agent";
    backendAgentEvents["pane-live-mid"] = [
      { kind: "session", session_id: "s-live" },
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "still streaming" },
    ];
    await app.__test.resyncWorkspace();
    const chat = app.__test.state.chats.get("pane-live-mid");
    expect(chat.busy).toBe(true);
    expect(chat.exited).toBe(false);

    // Resync while still live must not clamp an already-mounted chat that
    // is ahead of the snapshot tail.
    await app.__test.resyncWorkspace();
    expect(app.__test.state.chats.get("pane-live-mid").busy).toBe(true);
    expect(app.__test.state.chats.get("pane-live-mid").exited).toBe(false);

    agentEvent("pane-live-mid", { kind: "turn_complete", subtype: "success" });
    await waitFor(
      () => app.__test.state.chats.get("pane-live-mid").busy === false,
      "turn settled",
    );
  });

  it("clamps an already-mounted chat when a later snapshot says the pane ended (H2)", async () => {
    // Seed a mid-turn chat on a live pane, then report the process ended
    // without a live process_exit (daemon-death catch-up shape).
    const live = backendPane("pane-live-ended", "agent-live-ended");
    live.kind = "agent";
    backendAgentEvents["pane-live-ended"] = [
      { kind: "session", session_id: "s-ended" },
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "still streaming" },
      { kind: "permission_request", request_id: "req-live", tool_name: "Bash" },
    ];
    await app.__test.resyncWorkspace();
    const chat = app.__test.state.chats.get("pane-live-ended");
    expect(chat.busy).toBe(true);
    expect(chat.pendingPermission).not.toBeNull();
    expect(chat.exited).toBe(false);

    backendEndedPanes.add("pane-live-ended");
    await app.__test.resyncWorkspace();
    expect(chat.busy).toBe(false);
    expect(chat.pendingPermission).toBeNull();
    expect(chat.exited).toBe(true);
    expect(chat.messages.at(-1).open).toBe(false);

    const root = chatRoot("pane-live-ended");
    expect(root.querySelector(".chat-msg-open")).toBeNull();
    expect(root.querySelector(".chat-exited")).not.toBeNull();
    expect(root.querySelector(".chat-input").disabled).toBe(false);
  });

  it("clamps the mounted chat in the pane-ended catch-up render (H2)", async () => {
    const pane = backendPane("pane-catchup-ended", "agent-catchup-ended");
    pane.kind = "agent";
    backendAgentEvents[pane.id] = [
      { kind: "session", session_id: "s-catchup" },
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "partial" },
      { kind: "permission_request", request_id: "req-catchup", tool_name: "Bash" },
    ];
    await app.__test.resyncWorkspace();
    const chat = app.__test.state.chats.get(pane.id);
    expect(chat.busy).toBe(true);

    // This is the reconnect order: subscription catch-up first, then the same
    // ended state appears in the next bootstrap snapshot.
    backendEndedPanes.add(pane.id);
    listeners["pane-ended"]({ payload: { pane_id: pane.id } });
    await waitFor(
      () => chatRoot(pane.id)?.querySelector(".chat-input").disabled === false,
      "catch-up PaneEnded rendered an enabled composer",
    );
    expect(chat.pendingPermission).toBeNull();
    expect(chat.exited).toBe(true);
    expect(chat.messages.at(-1).open).toBe(false);

    await app.__test.resyncWorkspace();
    expect(chatRoot(pane.id).querySelector(".chat-input").disabled).toBe(false);
  });

  it("renders a chat-only clamp when pane state is already ended (H2)", async () => {
    const pane = backendPane("pane-same-state-ended", "agent-same-state-ended");
    pane.kind = "agent";
    backendAgentEvents[pane.id] = [
      { kind: "session", session_id: "s-same-state" },
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "partial" },
    ];
    await app.__test.resyncWorkspace();
    const chat = app.__test.state.chats.get(pane.id);
    expect(chat.busy).toBe(true);

    // Reproduce the state that previously suppressed notify(): the event has
    // already recorded "ended", but its chat mutation still needs rendering.
    backendEndedPanes.add(pane.id);
    app.__test.state.paneStates.set(pane.id, "ended");
    const revisionBefore = app.__test.controller.getSnapshot();
    await app.__test.resyncWorkspace();

    expect(app.__test.controller.getSnapshot()).toBeGreaterThan(revisionBefore);
    expect(chat.busy).toBe(false);
    expect(chat.exited).toBe(true);
    expect(chat.messages.at(-1).open).toBe(false);
    expect(chatRoot(pane.id).querySelector(".chat-input").disabled).toBe(false);
  });

  it("dedupes a live event that the replay tail already folded (M1)", async () => {
    const pane = backendPane("pane-seq", "agent-seq");
    pane.kind = "agent";
    backendAgentEvents["pane-seq"] = [
      { kind: "session", session_id: "s-seq", seq: 1 },
      { kind: "message_start", role: "assistant", seq: 2 },
      { kind: "text_delta", text: "replayed", seq: 3 },
      { kind: "message_complete", seq: 4 },
    ];
    await app.__test.resyncWorkspace();
    const chat = app.__test.state.chats.get("pane-seq");
    expect(chat.lastSeq).toBe(4);
    expect(chat.messages).toHaveLength(1);

    // The live stream re-delivers seq 3 and 4 (already folded): no double append.
    agentEvent("pane-seq", { kind: "text_delta", text: "replayed", seq: 3 });
    agentEvent("pane-seq", { kind: "message_complete", seq: 4 });
    expect(chat.messages).toHaveLength(1);
    expect(chat.messages[0].text).toBe("replayed");

    // …while the NEXT seq applies normally (gaps are fine).
    agentEvent("pane-seq", { kind: "text_delta", text: " live", seq: 6 });
    expect(chat.messages).toHaveLength(2);
    expect(chat.messages[1].text).toBe(" live");
    expect(chat.lastSeq).toBe(6);

    // Settle.
    agentEvent("pane-seq", { kind: "message_complete", seq: 7 });
    agentEvent("pane-seq", { kind: "turn_complete", subtype: "success", seq: 8 });
  });

  it("disposes the old-kind view when a snapshot flips a pane's kind (L6)", async () => {
    expect(app.__test.state.panes.get("pane-z").kind).toBe("shell");
    expect(app.__test.state.terminalViews.has("pane-z")).toBe(true);

    // shell → agent: the xterm view is disposed, a chat view mounts instead.
    backendPanes.get("pane-z").kind = "agent";
    await app.__test.resyncWorkspace();
    expect(app.__test.state.panes.get("pane-z").kind).toBe("agent");
    expect(app.__test.state.terminalViews.has("pane-z")).toBe(false);
    const zTerminals = FakeTerminal.instances.filter((t) => t.paneId === "pane-z");
    expect(zTerminals.length).toBeGreaterThan(0);
    expect(zTerminals.every((t) => t.disposed)).toBe(true);
    await waitFor(() => chatRoot("pane-z"), "chat view after flip to agent");
    expect(app.__test.state.chatViews.has("pane-z")).toBe(true);
    expect(terminalFor("pane-z")).toBeNull();

    // agent → shell: the chat view (and its state) goes, a terminal mounts.
    backendPanes.get("pane-z").kind = "shell";
    await app.__test.resyncWorkspace();
    expect(app.__test.state.chatViews.has("pane-z")).toBe(false);
    expect(app.__test.state.chats.has("pane-z")).toBe(false);
    expect(chatRoot("pane-z")).toBeNull();
    await waitFor(
      () => app.__test.state.terminalViews.has("pane-z"),
      "terminal view after flip back",
    );
  });

  it("restores the composer draft and drops the phantom bubble when a send fails (M2)", async () => {
    sendAgentMessageMode = "fail";
    try {
      const root = chatRoot("pane-c");
      const input = root.querySelector(".chat-input");
      const chat = app.__test.state.chats.get("pane-c");
      input.value = "will fail";
      input.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
      );

      await waitFor(() => input.value === "will fail", "draft restored");
      expect(chat.busy).toBe(false); // a real rejection ends the turn locally
      // The phantom user bubble is gone; an error line took its place.
      expect(
        chat.messages.filter((m) => m.type === "user" && m.text === "will fail"),
      ).toHaveLength(0);
      expect(chat.messages.at(-1).type).toBe("error");
      expect(chat.messages.at(-1).message).toContain("daemon dead");
    } finally {
      sendAgentMessageMode = "ok";
    }
  });

  it("keeps an accepted prompt and busy state when the response connection fails", async () => {
    sendAgentMessageMode = "ack-then-fail";
    try {
      const input = chatRoot("pane-c").querySelector(".chat-input");
      const chat = app.__test.state.chats.get("pane-c");
      input.value = "accepted before disconnect";
      input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
      await waitFor(() => chat.messages.at(-1)?.type === "error", "response error shown");
      expect(chat.messages.filter((message) => message.type === "user" && message.text === "accepted before disconnect")).toHaveLength(1);
      expect(chat.busy).toBe(true);
      expect(input.value).toBe("");
    } finally {
      listeners["agent-event"]({ payload: { pane_id: "pane-c", event: { kind: "turn_complete" } } });
      sendAgentMessageMode = "ok";
    }
  });

  it("gives send_agent_message a 60s timeout and does not force-clear busy on timeout (M2)", async () => {
    vi.useFakeTimers();
    try {
      sendAgentMessageMode = "hang";
      const root = chatRoot("pane-c");
      const input = root.querySelector(".chat-input");
      const chat = app.__test.state.chats.get("pane-c");
      input.value = "slow send";
      input.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
      );
      expect(commandsInvoked("send_agent_message").at(-1).args).toEqual({
        paneId: "pane-c",
        text: "slow send",
        messageId: expect.any(String),
      });
      expect(input.value).toBe(""); // cleared optimistically by the composer
      expect(chat.busy).toBe(true);

      // Still in flight at 59s: the default 5s timeout must NOT have fired.
      await vi.advanceTimersByTimeAsync(59_000);
      expect(input.value).toBe("");
      expect(
        chat.messages.filter((m) => m.type === "user" && m.text === "slow send"),
      ).toHaveLength(1);

      // Crossing 60s: the invoke times out — keep the user bubble (daemon
      // never echoes user messages; the turn may still complete) and leave
      // the composer empty (draft was already committed to the transcript).
      await vi.advanceTimersByTimeAsync(2_000);
      expect(input.value).toBe("");
      expect(
        chat.messages.filter((m) => m.type === "user" && m.text === "slow send"),
      ).toHaveLength(1);
      expect(chat.messages.at(-1).type).toBe("error");
      expect(chat.messages.at(-1).message).toContain("timed out");
      // …but busy is NOT force-cleared: the daemon may still be working the
      // turn; its events will settle the composer.
      expect(chat.busy).toBe(true);
      // Flush the pending rAF render before returning to real timers.
      await vi.advanceTimersByTimeAsync(100);
    } finally {
      vi.useRealTimers();
      sendAgentMessageMode = "ok";
    }
    // Settle the wedged turn so later tests find an idle composer.
    agentEvent("pane-c", { kind: "turn_complete", subtype: "success" });
    await waitFor(
      () => app.__test.state.chats.get("pane-c").busy === false,
      "turn settled after timeout",
    );
  });

  it("does not re-create the chat when an approval resolves after the pane closed (L3)", async () => {
    const paneId = await createScratchAgentPane();
    agentEvent(paneId, { kind: "permission_request", request_id: "req-late", tool_name: "Bash" });
    await waitFor(() => chatRoot(paneId).querySelector(".chat-permission"), "card");

    let settle;
    approvalGate = new Promise((resolve, reject) => {
      settle = { resolve, reject };
    });
    try {
      chatRoot(paneId).querySelector(".chat-allow").click();
      await waitFor(
        () => commandsInvoked("agent_approval").some((c) => c.args.requestId === "req-late"),
        "approval invoke in flight",
      );
      closePaneEverywhere(paneId);
      expect(app.__test.state.chats.has(paneId)).toBe(false);

      settle.resolve({ ok: true });
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(app.__test.state.chats.has(paneId)).toBe(false); // NOT re-created
    } finally {
      approvalGate = null;
    }
  });

  it("does not re-create the chat when an approval rejects after the pane closed (L3)", async () => {
    const paneId = await createScratchAgentPane();
    agentEvent(paneId, { kind: "permission_request", request_id: "req-late-2", tool_name: "Bash" });
    await waitFor(() => chatRoot(paneId).querySelector(".chat-permission"), "card");

    let settle;
    approvalGate = new Promise((resolve, reject) => {
      settle = { resolve, reject };
    });
    try {
      chatRoot(paneId).querySelector(".chat-allow").click();
      await waitFor(
        () => commandsInvoked("agent_approval").some((c) => c.args.requestId === "req-late-2"),
        "approval invoke in flight",
      );
      closePaneEverywhere(paneId);

      settle.reject(new Error("daemon dead"));
      await new Promise((resolve) => setTimeout(resolve, 20));
      // No chat re-created, so no stale error line either.
      expect(app.__test.state.chats.has(paneId)).toBe(false);
    } finally {
      approvalGate = null;
    }
  });

  it("does not re-create the chat when an interrupt rejects after the pane closed (L3)", async () => {
    const paneId = await createScratchAgentPane();
    agentEvent(paneId, { kind: "message_start", role: "assistant" });
    await waitFor(
      () => chatRoot(paneId).querySelector(".chat-send").textContent === "Interrupt",
      "busy composer",
    );

    let settle;
    interruptGate = new Promise((resolve, reject) => {
      settle = { resolve, reject };
    });
    try {
      chatRoot(paneId).querySelector(".chat-send").click();
      await waitFor(
        () => commandsInvoked("interrupt_agent").some((c) => c.args.paneId === paneId),
        "interrupt invoke in flight",
      );
      closePaneEverywhere(paneId);

      settle.reject(new Error("daemon dead"));
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(app.__test.state.chats.has(paneId)).toBe(false);
    } finally {
      interruptGate = null;
    }
  });

  it("does not re-create the chat when a send fails after the pane closed (L3)", async () => {
    const paneId = await createScratchAgentPane();
    sendAgentMessageMode = "fail";
    try {
      const input = chatRoot(paneId).querySelector(".chat-input");
      input.value = "too late";
      input.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }),
      );
      // The pane closes synchronously before the rejection lands.
      closePaneEverywhere(paneId);
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(app.__test.state.chats.has(paneId)).toBe(false);
    } finally {
      sendAgentMessageMode = "ok";
    }
  });

  it("renders a null-request-id permission card as informational, not actionable (L4)", async () => {
    agentEvent("pane-c", {
      kind: "permission_request",
      tool_name: "Bash",
      input: { command: "ls" },
    });
    await waitFor(() => chatRoot("pane-c").querySelector(".chat-permission"), "card");
    const card = chatRoot("pane-c").querySelector(".chat-permission");
    expect(card.querySelector(".chat-allow")).toBeNull();
    expect(card.querySelector(".chat-deny")).toBeNull();
    expect(card.querySelector(".chat-permission-note").textContent).toContain(
      "cannot be answered",
    );

    // turn_complete sweeps the unanswerable card (H1).
    agentEvent("pane-c", { kind: "turn_complete", subtype: "success" });
    await waitFor(
      () => !chatRoot("pane-c").querySelector(".chat-permission"),
      "card cleared by turn_complete",
    );
  });

  it("composer Enter during IME composition confirms the candidate instead of sending (L2)", async () => {
    const root = chatRoot("pane-c");
    const input = root.querySelector(".chat-input");
    const sendsBefore = commandsInvoked("send_agent_message").length;
    input.value = "かんじ";
    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        isComposing: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(commandsInvoked("send_agent_message").length).toBe(sendsBefore);
    expect(input.value).toBe("かんじ"); // the draft belongs to the IME

    // The Enter after composition ends sends normally.
    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        isComposing: false,
        bubbles: true,
        cancelable: true,
      }),
    );
    await waitFor(
      () => commandsInvoked("send_agent_message").length === sendsBefore + 1,
      "send after composition ended",
    );
    expect(commandsInvoked("send_agent_message").at(-1).args.text).toBe("かんじ");
    agentEvent("pane-c", { kind: "turn_complete", subtype: "success" });
    await waitFor(
      () => app.__test.state.chats.get("pane-c").busy === false,
      "turn settled",
    );
  });

  it("lets mod+Shift chords and mod+digit through the focused composer (L1)", async () => {
    const input = chatRoot("pane-c").querySelector(".chat-input");
    await waitFor(() => input.disabled === false, "composer idle before key chords");
    input.focus();
    expect(document.activeElement).toBe(input);

    // Plain typing is untouched by the keymap (no binding, not consumed).
    const createsBefore = commandsInvoked("create_agent_pane").length;
    const plain = new KeyboardEvent("keydown", { key: "n", bubbles: true, cancelable: true });
    window.dispatchEvent(plain);
    expect(plain.defaultPrevented).toBe(false);
    expect(commandsInvoked("create_agent_pane").length).toBe(createsBefore);

    // mod+Shift+N reaches the keymap: a new agent pane is created.
    const chord = new KeyboardEvent("keydown", {
      key: "N",
      ctrlKey: true,
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    });
    window.dispatchEvent(chord);
    expect(chord.defaultPrevented).toBe(true);
    await waitFor(
      () => commandsInvoked("create_agent_pane").length === createsBefore + 1,
      "create_agent_pane via composer chord",
    );
    await waitFor(() => {
      const active = app.__test.state.activePaneId;
      return active?.startsWith("agent-created-") && chatRoot(active);
    }, "chord-created agent pane active");
    const chordPane = app.__test.state.activePaneId;

    // mod+digit reaches the keymap: focus moves to the numbered pane (the
    // index maps to state.panes insertion order, like the tabs).
    const ids = Array.from(app.__test.state.panes.keys());
    const targetIndex = ids[0] === chordPane ? 1 : 0;
    const digit = new KeyboardEvent("keydown", {
      key: String(targetIndex + 1),
      ctrlKey: true,
      bubbles: true,
      cancelable: true,
    });
    window.dispatchEvent(digit);
    expect(digit.defaultPrevented).toBe(true);
    expect(app.__test.state.activePaneId).toBe(ids[targetIndex]);

    closePaneEverywhere(chordPane);
    input.blur();
  });

  it("caps the chat and renders an elided-history marker (M3)", async () => {
    const paneId = await createScratchAgentPane();
    for (let i = 1; i <= 505; i += 1) {
      agentEvent(paneId, { kind: "error", message: `e${i}` });
    }
    const chat = app.__test.state.chats.get(paneId);
    expect(chat.messages).toHaveLength(500);
    expect(chat.messages[0]).toEqual({ type: "elided", count: 6 });
    expect(chat.messages.at(-1)).toEqual({ type: "error", message: "e505" });

    await waitFor(() => {
      const marker = chatRoot(paneId)?.querySelector(".chat-elided");
      return marker && marker.textContent.includes("6");
    }, "elided marker rendered");

    closePaneEverywhere(paneId);
  });
});


describe("ui smoke commands", () => {
  it("registers ui_smoke_enabled and complete_ui_smoke with the bridge", async () => {
    // Packaged smoke exits the process in production; here we only prove the
    // command names are wired through the same invoke boundary as the rest.
    await window.__TAURI__.core.invoke("ui_smoke_enabled", {});
    await window.__TAURI__.core.invoke("complete_ui_smoke", {
      ok: true,
      error: null,
    });
    expect(commandsInvoked("ui_smoke_enabled").length).toBeGreaterThan(0);
    expect(commandsInvoked("complete_ui_smoke").length).toBeGreaterThan(0);
  });
});

// Boundary tally — last, so it covers every invoke in the whole run.
describe("invoke boundary", () => {
  it("only ever invoked command names the backend registers", () => {
    expect(unknownCommands).toEqual([]);
    for (const call of invoked) {
      expect(KNOWN_COMMAND_SET.has(call.command), `unknown command: ${call.command}`).toBe(true);
    }
    // Every backend command is exercised by the app at least once in this run.
    const used = new Set(invoked.map((call) => call.command));
    for (const command of KNOWN_COMMANDS) {
      expect(used.has(command), `command never invoked: ${command}`).toBe(true);
    }
  });
});
