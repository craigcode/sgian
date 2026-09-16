import React from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "../ui/src/app.jsx";
import { createAppController } from "../ui/src/app-controller.js";

class FakeTerminal {
  static instances = [];

  constructor(options) {
    this.options = options;
    this.cols = 80;
    this.rows = 24;
    this.disposed = false;
    this.input = null;
    FakeTerminal.instances.push(this);
  }

  open(host) {
    this.element = document.createElement("div");
    this.input = document.createElement("textarea");
    this.input.className = "xterm-helper-textarea";
    this.element.append(this.input);
    host.append(this.element);
  }

  loadAddon() {}
  onData(callback) {
    this.onDataCallback = callback;
  }
  onResize(callback) {
    this.onResizeCallback = callback;
  }
  write() {}
  reset() {}
  focus() {
    this.input?.focus();
  }
  refresh() {}
  hasSelection() {
    return false;
  }
  dispose() {
    this.disposed = true;
  }
}

class FakeResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

function createHarness({
  panes = [{ id: "pane-a", title: "term-a", kind: "shell", created_at_ms: 1 }],
  activePaneId = panes[0]?.id ?? null,
  leases = {},
  writeError = null,
} = {}) {
  const listeners = new Map();
  const unlisten = vi.fn();
  let paneNumber = 1;
  const invoke = vi.fn(async (command) => {
    if (command === "bootstrap_workspace") {
      return {
        panes,
        pane_states: Object.fromEntries(panes.map((pane) => [pane.id, "live"])),
        active_pane_id: activePaneId,
        cwd: "/workspace/fresh",
        layout: null,
        scrollback: Object.fromEntries(
          panes.filter((pane) => pane.kind !== "agent").map((pane) => [pane.id, "ready\r\n"]),
        ),
        agent_states: {},
        agent_events: {},
        leases,
      };
    }
    if (command === "client_holder") return "me@test";
    if (command === "write_to_pane" && writeError) throw new Error(writeError);
    if (command === "take_lease") {
      return { pane_id: "pane-a", holder: "me@test", since_ms: 1 };
    }
    if (command === "release_lease") return { pane_id: "pane-a", holder: null };
    if (command === "get_config") return { font_size: 13 };
    if (command === "ui_smoke_enabled") return false;
    if (command === "create_pane") {
      paneNumber += 1;
      return {
        id: `pane-${paneNumber}`,
        title: `term-${paneNumber}`,
        kind: "shell",
        created_at_ms: paneNumber,
      };
    }
    return { ok: true };
  });
  const listen = vi.fn(async (name, handler) => {
    listeners.set(name, handler);
    return unlisten;
  });
  const controller = createAppController({ nativeInvoke: invoke, nativeListen: listen });
  return { controller, invoke, listeners, unlisten };
}

beforeEach(() => {
  FakeTerminal.instances = [];
  globalThis.Terminal = FakeTerminal;
  globalThis.FitAddon = { FitAddon: class { fit() {} } };
  globalThis.ResizeObserver = FakeResizeObserver;
});

afterEach(() => {
  cleanup();
  delete globalThis.Terminal;
  delete globalThis.FitAddon;
  delete globalThis.ResizeObserver;
});

describe("React application instances", () => {
  it("boots from an injected bridge and keeps xterm mounted across UI renders", async () => {
    const { controller } = createHarness();
    const view = render(<App controller={controller} />);

    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    expect(view.getByText("/workspace/fresh")).toBeTruthy();
    // One short-lived startup placeholder plus the snapshot pane. Only the
    // latter remains live after the snapshot merge.
    expect(FakeTerminal.instances).toHaveLength(2);
    expect(FakeTerminal.instances.filter((terminal) => !terminal.disposed)).toHaveLength(1);

    const host = view.container.querySelector('.terminal[data-pane-id="pane-a"]');
    controller.notify();
    expect(view.container.querySelector('.terminal[data-pane-id="pane-a"]')).toBe(host);
    expect(FakeTerminal.instances).toHaveLength(2);
  });

  it("routes React toolbar intent and cleans up native subscriptions on unmount", async () => {
    const user = userEvent.setup();
    const { controller, invoke, unlisten } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));

    await user.click(view.getByRole("button", { name: "Split right" }));
    await waitFor(() => expect(view.container.querySelector('[data-pane-id="pane-2"]')).toBeTruthy());
    expect(invoke).toHaveBeenCalledWith("create_pane", { title: undefined, profile: null });

    view.unmount();
    expect(unlisten).toHaveBeenCalled();
    expect(FakeTerminal.instances.every((terminal) => terminal.disposed)).toBe(true);
  });

  it("does not share pane or modal state between controller instances", () => {
    const first = createHarness().controller;
    const second = createHarness().controller;
    first.state.panes.set("only-first", { id: "only-first", title: "one" });
    first.openSettingsModal();

    expect(first.state.panes.has("only-first")).toBe(true);
    expect(second.state.panes.has("only-first")).toBe(false);
    expect(first.state.settingsModalOpen).toBe(true);
    expect(second.state.settingsModalOpen).toBe(false);

    first.stop();
    second.stop();
  });

  it("restores focus to the settings opener when the modal closes", async () => {
    const user = userEvent.setup();
    const { controller } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));

    const opener = view.getByRole("button", { name: "Settings" });
    await user.click(opener);
    await waitFor(() => expect(view.getByRole("dialog")).toBeTruthy());
    await user.click(view.getByRole("button", { name: "Close settings" }));

    await waitFor(() => expect(document.activeElement).toBe(opener));
  });

  it("opens the command palette from the keyboard shortcut and restores focus", async () => {
    const user = userEvent.setup();
    const { controller } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));

    const opener = view.getByRole("button", { name: "Settings" });
    opener.focus();
    await user.keyboard("{Control>}k{/Control}");
    await waitFor(() =>
      expect(view.getByRole("dialog", { name: "Command palette" })).toBeTruthy(),
    );
    expect(controller.state.paletteOpen).toBe(true);

    await user.keyboard("{Escape}");
    await waitFor(() => expect(controller.state.paletteOpen).toBe(false));
    await waitFor(() => expect(document.activeElement).toBe(opener));
  });

  it("opens session overview from the toolbar and restores focus", async () => {
    const user = userEvent.setup();
    const { controller } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));

    const opener = view.getByRole("button", { name: "Session overview" });
    await user.click(opener);
    await waitFor(() =>
      expect(view.getByRole("dialog", { name: "Session overview" })).toBeTruthy(),
    );
    const overview = view.getByRole("dialog", { name: "Session overview" });
    expect(overview.querySelector("td")?.textContent).toBe("term-a");

    await user.click(view.getByRole("button", { name: "Close session overview" }));
    await waitFor(() => expect(controller.state.overviewOpen).toBe(false));
    await waitFor(() => expect(document.activeElement).toBe(opener));
  });

  it("passes selected shell profile name into create_pane", async () => {
    const user = userEvent.setup();
    const { controller, invoke } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    controller.state.profiles = [{ name: "dev", kind: "shell", shell: "/bin/zsh" }];
    controller.setSelectedProfile("dev");
    await user.click(view.getByRole("button", { name: "Split right" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("create_pane", {
        title: null,
        profile: "dev",
      }),
    );
  });

  it("keeps focus inside overview after closing a pane from the table", async () => {
    const user = userEvent.setup();
    const panes = [
      { id: "pane-a", title: "term-a", kind: "shell", created_at_ms: 1 },
      { id: "pane-b", title: "term-b", kind: "shell", created_at_ms: 2 },
    ];
    const { controller } = createHarness({ panes });
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));

    await user.click(view.getByRole("button", { name: "Session overview" }));
    await waitFor(() =>
      expect(view.getByRole("dialog", { name: "Session overview" })).toBeTruthy(),
    );
    const closeButtons = view.getAllByRole("button", { name: "Close" });
    const rowClose = closeButtons.find((button) =>
      button.classList.contains("overview-action"),
    );
    expect(rowClose).toBeTruthy();
    await user.click(rowClose);
    await waitFor(() => expect(controller.state.panes.size).toBe(1));
    expect(controller.state.overviewOpen).toBe(true);
    await waitFor(() =>
      expect(
        view.getByRole("dialog", { name: "Session overview" }).contains(document.activeElement),
      ).toBe(true),
    );
  });

  it("does not open the palette with Ctrl+K while a terminal is focused", async () => {
    const user = userEvent.setup();
    const { controller } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    const helper = view.container.querySelector(".xterm-helper-textarea");
    expect(helper).toBeTruthy();
    helper.focus();
    await user.keyboard("{Control>}k{/Control}");
    expect(controller.state.paletteOpen).toBe(false);
    await user.keyboard("{Control>}{Shift>}p{/Shift}{/Control}");
    await waitFor(() => expect(controller.state.paletteOpen).toBe(true));
  });

  it("resizes a split via the separator keyboard controls", async () => {
    const user = userEvent.setup();
    const panes = [
      { id: "pane-a", title: "term-a", kind: "shell", created_at_ms: 1 },
      { id: "pane-b", title: "term-b", kind: "shell", created_at_ms: 2 },
    ];
    const { controller } = createHarness({ panes });
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    // Ensure a row split exists (bootstrap with two panes yields a default layout).
    await waitFor(() =>
      expect(view.getByRole("separator", { name: "Resize split" })).toBeTruthy(),
    );
    const separator = view.getByRole("separator", { name: "Resize split" });
    const before = Number(separator.getAttribute("aria-valuenow"));
    expect(before).toBeGreaterThan(0);
    separator.focus();
    await user.keyboard("{ArrowRight}");
    await waitFor(() =>
      expect(Number(separator.getAttribute("aria-valuenow"))).toBe(before + 2),
    );
    await user.keyboard("{Shift>}{ArrowLeft}{/Shift}");
    await waitFor(() =>
      expect(Number(separator.getAttribute("aria-valuenow"))).toBe(before + 2 - 10),
    );
  });

  it("keeps Tab focus trapped inside settings and palette dialogs", async () => {
    const user = userEvent.setup();
    const { controller } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));

    await user.click(view.getByRole("button", { name: "Settings" }));
    const settings = await waitFor(() => view.getByRole("dialog", { name: "Settings" }));
    const settingsFocusable = settings.querySelectorAll(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    );
    expect(settingsFocusable.length).toBeGreaterThan(1);
    settingsFocusable[settingsFocusable.length - 1].focus();
    await user.keyboard("{Tab}");
    expect(settings.contains(document.activeElement)).toBe(true);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(controller.state.settingsModalOpen).toBe(false));

    await user.keyboard("{Control>}{Shift>}p{/Shift}{/Control}");
    const palette = await waitFor(() =>
      view.getByRole("dialog", { name: "Command palette" }),
    );
    const paletteFocusable = palette.querySelectorAll(
      'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
    );
    expect(paletteFocusable.length).toBeGreaterThan(0);
    paletteFocusable[paletteFocusable.length - 1].focus();
    await user.keyboard("{Tab}");
    expect(palette.contains(document.activeElement)).toBe(true);
    await user.keyboard("{Escape}");
    await waitFor(() => expect(controller.state.paletteOpen).toBe(false));
  });

  it("runs the packaged UI smoke sequence when enabled", async () => {
    const { controller, invoke } = createHarness();
    invoke.mockImplementation(async (command, args) => {
      if (command === "ui_smoke_enabled") return true;
      if (command === "complete_ui_smoke") return { ok: true };
      if (command === "bootstrap_workspace") {
        return {
          panes: [{ id: "pane-a", title: "term-a", kind: "shell", created_at_ms: 1 }],
          pane_states: { "pane-a": "live" },
          active_pane_id: "pane-a",
          cwd: "/workspace/fresh",
          layout: null,
          scrollback: { "pane-a": "ready\r\n" },
          agent_states: {},
          agent_events: {},
        };
      }
      if (command === "get_config") return { font_size: 13 };
      if (command === "create_pane") {
        return { id: "pane-2", title: "term-2", kind: "shell", created_at_ms: 2 };
      }
      if (command === "write_to_pane") return { ok: true };
      return { ok: true };
    });
    render(<App controller={controller} />);
    await waitFor(() => expect(controller.state.booted).toBe(true));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "complete_ui_smoke",
        expect.objectContaining({ ok: true }),
      ),
    );
    expect(invoke).toHaveBeenCalledWith("create_pane", expect.anything());
    expect(invoke).toHaveBeenCalledWith(
      "write_to_pane",
      expect.objectContaining({ data: "echo sgian-ui-smoke\n" }),
    );
  });

  it("fails the packaged UI smoke when the split cannot create a pane", async () => {
    const { controller, invoke } = createHarness();
    const baseImplementation = invoke.getMockImplementation();
    invoke.mockImplementation(async (command, args) => {
      if (command === "ui_smoke_enabled") return true;
      if (command === "create_pane") throw new Error("create failed");
      if (command === "complete_ui_smoke") return { ok: true };
      return baseImplementation(command, args);
    });

    render(<App controller={controller} />);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "complete_ui_smoke",
        expect.objectContaining({ ok: false, error: expect.stringContaining("split") }),
      ),
    );
    expect(invoke).not.toHaveBeenCalledWith(
      "complete_ui_smoke",
      expect.objectContaining({ ok: true }),
    );
  });

  it("keeps workspace and agent messaging available when xterm is missing", async () => {
    delete globalThis.Terminal;
    const panes = [
      { id: "pane-shell", title: "shell", kind: "shell", created_at_ms: 1 },
      { id: "pane-agent", title: "agent", kind: "agent", created_at_ms: 2 },
    ];
    const { controller, invoke } = createHarness({ panes, activePaneId: "pane-agent" });
    const view = render(<App controller={controller} />);

    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    expect(controller.state.terminalEngineAvailable).toBe(false);
    expect(view.getByText("/workspace/fresh")).toBeTruthy();
    expect(view.getByRole("alert").textContent).toContain("Terminal engine unavailable");

    controller.sendAgentMessage("pane-agent", "still works");
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("send_agent_message", {
        paneId: "pane-agent",
        text: "still works",
        messageId: expect.any(String),
      }),
    );
    await controller.restartPane("pane-shell");
    expect(controller.state.bootStatus).toBe("terminal engine unavailable");
  });
});

describe("keyboard lease UI", () => {
  it("shows the holder badge from the bootstrap snapshot and clears it on release", async () => {
    const { controller, listeners } = createHarness({
      leases: { "pane-a": { holder: "bob", since_ms: 1 } },
    });
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    const badge = view.container.querySelector('.lease-badge[data-holder="bob"]');
    expect(badge).toBeTruthy();
    expect(badge.textContent).toContain("bob");

    listeners.get("lease-state")({
      payload: { pane_id: "pane-a", transition: "released", holder: null, note: "done" },
    });
    await waitFor(() => expect(view.container.querySelector(".lease-badge")).toBeNull());
    view.unmount();
  });

  it("labels the client's own lease as you", async () => {
    const { controller, listeners } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    await waitFor(() => expect(controller.state.holder).toBe("me@test"));
    listeners.get("lease-state")({
      payload: { pane_id: "pane-a", transition: "taken", holder: "me@test", since_ms: 2 },
    });
    await waitFor(() =>
      expect(view.container.querySelector(".lease-badge-mine")?.textContent).toContain("you"),
    );
    view.unmount();
  });

  it("turns a refused keystroke into a toast, not terminal output", async () => {
    const { controller } = createHarness({
      writeError: "pane keyboard is held by bob (pane-a)",
    });
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    const terminal = FakeTerminal.instances.find((instance) => !instance.disposed);
    terminal.onDataCallback("x");
    await waitFor(() => expect(view.container.querySelector(".lease-toast")).toBeTruthy());
    expect(view.container.querySelector(".lease-toast").textContent).toContain("held by bob");
    expect(controller.state.terminalBuffers.get("pane-a") || "").not.toContain("failed to write");
    view.unmount();
  });

  it("requires a note to release and sends it to the backend", async () => {
    const user = userEvent.setup();
    const { controller, invoke } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    controller.openReleaseDialog("pane-a");
    await waitFor(() => expect(view.getByRole("dialog")).toBeTruthy());
    const submit = view.getByRole("button", { name: "Release keyboard" });
    expect(submit.disabled).toBe(true);
    await user.type(view.getByLabelText(/Hand-back note/), "answered the prompt");
    expect(submit.disabled).toBe(false);
    await user.click(submit);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("release_lease", {
        paneId: "pane-a",
        note: "answered the prompt",
      }),
    );
    await waitFor(() => expect(controller.state.leaseDialog).toBeNull());
    view.unmount();
  });

  it("opens the take dialog when the daemon asks for --force", async () => {
    const user = userEvent.setup();
    const { controller, invoke } = createHarness({
      leases: { "pane-a": { holder: "bob", since_ms: 1 } },
    });
    invoke.mockImplementationOnce(async () => ({
      panes: [{ id: "pane-a", title: "term-a", kind: "shell", created_at_ms: 1 }],
      pane_states: { "pane-a": "live" },
      active_pane_id: "pane-a",
      cwd: "/workspace/fresh",
      layout: null,
      scrollback: { "pane-a": "" },
      agent_states: {},
      agent_events: {},
      leases: { "pane-a": { holder: "bob", since_ms: 1 } },
    }));
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    invoke.mockImplementationOnce(async () => {
      throw new Error("pane keyboard is held by bob; use --force --why REASON to revoke it");
    });
    await controller.takeLease("pane-a");
    await waitFor(() => expect(view.getByRole("dialog")).toBeTruthy());
    expect(view.getByRole("dialog").textContent).toContain("from bob");
    await user.type(view.getByLabelText(/Why are you taking/), "bob is away");
    await user.click(view.getByRole("button", { name: "Take keyboard" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("take_lease", {
        paneId: "pane-a",
        force: true,
        why: "bob is away",
      }),
    );
    view.unmount();
  });
});

describe("unattended agent badge", () => {
  it("marks a pane whose agent runs without approval", async () => {
    const { controller, listeners } = createHarness();
    const view = render(<App controller={controller} />);
    await waitFor(() => expect(view.container.querySelector("#app").dataset.ready).toBe("true"));
    listeners.get("agent-state")({
      payload: { pane_id: "pane-a", agent: "claude", attention: "idle", mode: "auto", unattended: true },
    });
    await waitFor(() => expect(view.container.querySelector(".agent-badge-unattended")).toBeTruthy());
    const badge = view.container.querySelector(".agent-badge-unattended");
    expect(badge.getAttribute("title")).toContain("without your approval");
    expect(badge.dataset.mode).toBe("auto");
    listeners.get("agent-state")({
      payload: { pane_id: "pane-a", agent: "claude", attention: "idle", mode: "plan", unattended: false },
    });
    await waitFor(() => expect(view.container.querySelector(".agent-badge-unattended")).toBeNull());
    view.unmount();
  });
});
