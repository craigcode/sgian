import { describe, it, expect, vi } from "vitest";
import {
  buildPaletteCommands,
  filterPaletteCommands,
} from "../ui/src/palette.js";

function createController({
  updateVersion = null,
  focusNextAttentionPane = true,
  profiles = [],
} = {}) {
  const state = { activePaneId: "pane-1", updateVersion, profiles };
  const controller = {
    state,
    splitActive: vi.fn(),
    closeActive: vi.fn(),
    beginRename: vi.fn(),
    openSearchBar: vi.fn(),
    toggleZoomActive: vi.fn(),
    swapActiveWithPartner: vi.fn(),
    newAgentPane: vi.fn(),
    newPaneWithProfile: vi.fn(),
    openSettingsModal: vi.fn(),
    openOverview: vi.fn(),
    installUpdate: vi.fn(),
    takeLease: vi.fn(),
    openReleaseDialog: vi.fn(),
  };
  if (focusNextAttentionPane) {
    controller.focusNextAttentionPane = vi.fn();
  }
  return controller;
}

describe("filterPaletteCommands", () => {
  const commands = [
    { id: "split-row", label: "Split pane right", keywords: ["split"] },
    { id: "settings", label: "Open settings", keywords: ["preferences"] },
    { id: "search", label: "Search scrollback", keywords: ["find"] },
  ];

  it("filters a large command list within a baseline budget", () => {
    const large = Array.from({ length: 500 }, (_, index) => ({
      id: `cmd-${index}`,
      label: `Command number ${index} for workbench`,
      keywords: [`kw${index % 17}`, "pane"],
    }));
    const started = performance.now();
    for (let i = 0; i < 50; i += 1) {
      expect(filterPaletteCommands(large, "workbench").length).toBeGreaterThan(0);
    }
    expect(performance.now() - started).toBeLessThan(250);
  });

  it("returns all commands for an empty query", () => {
    expect(filterPaletteCommands(commands, "")).toHaveLength(3);
    expect(filterPaletteCommands(commands, "   ")).toHaveLength(3);
  });
  it("filters by substring on label", () => {
    const result = filterPaletteCommands(commands, "settings");
    expect(result.map((command) => command.id)).toEqual(["settings"]);
  });

  it("filters by keyword and id", () => {
    expect(filterPaletteCommands(commands, "find").map((command) => command.id)).toEqual([
      "search",
    ]);
    expect(filterPaletteCommands(commands, "split-row").map((command) => command.id)).toEqual([
      "split-row",
    ]);
  });

  it("supports fuzzy character-order matching", () => {
    expect(filterPaletteCommands(commands, "scrl").map((command) => command.id)).toEqual([
      "search",
    ]);
  });
});

describe("buildPaletteCommands", () => {
  it("includes core workbench commands", () => {
    const controller = createController();
    const ids = buildPaletteCommands(controller).map((command) => command.id);
    expect(ids).toEqual(
      expect.arrayContaining([
        "split-row",
        "split-column",
        "close-pane",
        "rename-pane",
        "search",
        "zoom-toggle",
        "swap-panes",
        "new-agent",
        "settings",
        "session-overview",
        "focus-attention",
        "lease-take",
        "lease-release",
      ]),
    );
  });

  it("omits focus-attention when controller lacks the method", () => {
    const controller = createController({ focusNextAttentionPane: false });
    const ids = buildPaletteCommands(controller).map((command) => command.id);
    expect(ids).not.toContain("focus-attention");
  });

  it("includes install-update when a version is present", () => {
    const controller = createController({ updateVersion: "1.2.3" });
    const command = buildPaletteCommands(controller).find(
      (entry) => entry.id === "install-update",
    );
    expect(command?.label).toContain("1.2.3");
    command.run();
    expect(controller.installUpdate).toHaveBeenCalledOnce();
  });

  it("wires split-row to controller.splitActive", () => {
    const controller = createController();
    const command = buildPaletteCommands(controller).find(
      (entry) => entry.id === "split-row",
    );
    command.run();
    expect(controller.splitActive).toHaveBeenCalledWith("row");
  });

  it("wires session-overview to controller.openOverview", () => {
    const controller = createController();
    const command = buildPaletteCommands(controller).find(
      (entry) => entry.id === "session-overview",
    );
    command.run();
    expect(controller.openOverview).toHaveBeenCalledOnce();
  });

  it("includes new-pane-with-profile commands for configured profiles", () => {
    const controller = createController({
      profiles: [
        { name: "dev-shell", kind: "shell" },
        { name: "code-agent", kind: "agent", agent_backend: "claude" },
      ],
    });
    const commands = buildPaletteCommands(controller);
    expect(commands.map((command) => command.id)).toEqual(
      expect.arrayContaining([
        "new-pane-profile-dev-shell",
        "new-pane-profile-code-agent",
      ]),
    );
    const shellCommand = commands.find((entry) => entry.id === "new-pane-profile-dev-shell");
    shellCommand.run();
    expect(controller.newPaneWithProfile).toHaveBeenCalledWith("dev-shell");
  });
});

describe("keyboard lease palette commands", () => {
  it("route to the controller with the active pane", () => {
    const controller = createController();
    const commands = buildPaletteCommands(controller);
    commands.find((command) => command.id === "lease-take").run();
    expect(controller.takeLease).toHaveBeenCalledWith("pane-1");
    commands.find((command) => command.id === "lease-release").run();
    expect(controller.openReleaseDialog).toHaveBeenCalledWith("pane-1");
  });
});
