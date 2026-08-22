import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  activeTerminalView,
  activeHasSelection,
  copySelection,
  pasteClipboard,
} from "../ui/src/clipboard.js";

function makeTerminal({ hasSelection = false, selection = "" } = {}) {
  return {
    hasSelection: vi.fn(() => hasSelection),
    getSelection: vi.fn(() => selection),
    paste: vi.fn(() => {}),
  };
}

function makeClipboard({ readText = "pasted-text", writeText = vi.fn(() => Promise.resolve()) } = {}) {
  return {
    writeText: vi.fn(() => writeText() ?? Promise.resolve()),
    readText: vi.fn(() => Promise.resolve(readText)),
  };
}

function makeState({ activePaneId = "pane-1", views = {} } = {}) {
  const terminalViews = new Map();
  for (const [id, terminal] of Object.entries(views)) {
    terminalViews.set(id, { terminal });
  }
  return { activePaneId, terminalViews };
}

describe("activeTerminalView", () => {
  it("returns the active pane's view", () => {
    const tA = makeTerminal();
    const tB = makeTerminal();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": tA, "pane-b": tB },
    });
    expect(activeTerminalView(state)?.terminal).toBe(tA);
  });

  it("retargets when the active pane changes", () => {
    const tA = makeTerminal();
    const tB = makeTerminal();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": tA, "pane-b": tB },
    });
    state.activePaneId = "pane-b";
    expect(activeTerminalView(state)?.terminal).toBe(tB);
  });

  it("returns null when the active pane has no view", () => {
    const state = makeState({ activePaneId: "pane-z", views: {} });
    expect(activeTerminalView(state)).toBeNull();
  });
});

describe("activeHasSelection", () => {
  it("returns true when the active terminal has a selection", () => {
    const state = makeState({
      views: { "pane-1": makeTerminal({ hasSelection: true }) },
    });
    expect(activeHasSelection(state)).toBe(true);
  });

  it("returns false when the active terminal has no selection", () => {
    const state = makeState({
      views: { "pane-1": makeTerminal({ hasSelection: false }) },
    });
    expect(activeHasSelection(state)).toBe(false);
  });

  it("returns false when there is no active view", () => {
    const state = makeState({ activePaneId: "pane-z", views: {} });
    expect(activeHasSelection(state)).toBe(false);
  });

  it("returns false (no throw) when hasSelection throws", () => {
    const terminal = makeTerminal();
    terminal.hasSelection.mockImplementation(() => {
      throw new Error("boom");
    });
    const state = makeState({ views: { "pane-1": terminal } });
    expect(() => activeHasSelection(state)).not.toThrow();
    expect(activeHasSelection(state)).toBe(false);
  });
});

describe("copySelection", () => {
  it("writes getSelection() to the clipboard when there is a selection", async () => {
    const terminal = makeTerminal({ hasSelection: true, selection: "hello" });
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard();
    const result = await copySelection(state, clipboard);
    expect(result).toBe(true);
    expect(terminal.getSelection).toHaveBeenCalled();
    expect(clipboard.writeText).toHaveBeenCalledWith("hello");
  });

  it("does not write to the clipboard when there is no selection", async () => {
    const terminal = makeTerminal({ hasSelection: false, selection: "" });
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard();
    const result = await copySelection(state, clipboard);
    expect(result).toBe(false);
    expect(clipboard.writeText).not.toHaveBeenCalled();
  });

  it("does not write when the selection text is empty", async () => {
    const terminal = makeTerminal({ hasSelection: true, selection: "" });
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard();
    const result = await copySelection(state, clipboard);
    expect(result).toBe(false);
    expect(clipboard.writeText).not.toHaveBeenCalled();
  });

  it("does not throw and returns false when clipboard.writeText rejects", async () => {
    const terminal = makeTerminal({ hasSelection: true, selection: "hello" });
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard();
    clipboard.writeText.mockRejectedValue(new Error("denied"));
    const result = await copySelection(state, clipboard);
    expect(result).toBe(false);
  });

  it("returns false when no clipboard API is provided", async () => {
    const terminal = makeTerminal({ hasSelection: true, selection: "hello" });
    const state = makeState({ views: { "pane-1": terminal } });
    const result = await copySelection(state, null);
    expect(result).toBe(false);
  });

  it("returns false when there is no active view", async () => {
    const state = makeState({ activePaneId: "pane-z", views: {} });
    const clipboard = makeClipboard();
    const result = await copySelection(state, clipboard);
    expect(result).toBe(false);
    expect(clipboard.writeText).not.toHaveBeenCalled();
  });

  it("only copies from the active pane and retargets on active change", async () => {
    const tA = makeTerminal({ hasSelection: true, selection: "A-sel" });
    const tB = makeTerminal({ hasSelection: true, selection: "B-sel" });
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": tA, "pane-b": tB },
    });
    const clipboard = makeClipboard();
    await copySelection(state, clipboard);
    expect(clipboard.writeText).toHaveBeenCalledWith("A-sel");
    state.activePaneId = "pane-b";
    await copySelection(state, clipboard);
    expect(clipboard.writeText).toHaveBeenCalledWith("B-sel");
  });

  it("does not throw when getSelection throws", async () => {
    const terminal = makeTerminal({ hasSelection: true, selection: "" });
    terminal.getSelection.mockImplementation(() => {
      throw new Error("boom");
    });
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard();
    await expect(copySelection(state, clipboard)).resolves.toBe(false);
    expect(clipboard.writeText).not.toHaveBeenCalled();
  });
});

describe("pasteClipboard", () => {
  it("reads the clipboard and pastes the text into the active terminal", async () => {
    const terminal = makeTerminal();
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard({ readText: "clipboard-text" });
    const result = await pasteClipboard(state, clipboard);
    expect(result).toBe(true);
    expect(clipboard.readText).toHaveBeenCalled();
    expect(terminal.paste).toHaveBeenCalledWith("clipboard-text");
  });

  it("only pastes into the active pane and retargets on active change", async () => {
    const tA = makeTerminal();
    const tB = makeTerminal();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": tA, "pane-b": tB },
    });
    const clipboard = makeClipboard({ readText: "x" });
    await pasteClipboard(state, clipboard);
    expect(tA.paste).toHaveBeenCalledWith("x");
    expect(tB.paste).not.toHaveBeenCalled();
    state.activePaneId = "pane-b";
    await pasteClipboard(state, clipboard);
    expect(tB.paste).toHaveBeenCalledWith("x");
  });

  it("does not throw and returns false when clipboard.readText rejects", async () => {
    const terminal = makeTerminal();
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard();
    clipboard.readText.mockRejectedValue(new Error("denied"));
    const result = await pasteClipboard(state, clipboard);
    expect(result).toBe(false);
    expect(terminal.paste).not.toHaveBeenCalled();
  });

  it("returns false when no clipboard API is provided", async () => {
    const terminal = makeTerminal();
    const state = makeState({ views: { "pane-1": terminal } });
    const result = await pasteClipboard(state, null);
    expect(result).toBe(false);
    expect(terminal.paste).not.toHaveBeenCalled();
  });

  it("returns false and does not paste when there is no active view", async () => {
    const state = makeState({ activePaneId: "pane-z", views: {} });
    const clipboard = makeClipboard();
    const result = await pasteClipboard(state, clipboard);
    expect(result).toBe(false);
  });

  it("does not paste when the clipboard text is empty", async () => {
    const terminal = makeTerminal();
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard({ readText: "" });
    const result = await pasteClipboard(state, clipboard);
    expect(result).toBe(false);
    expect(terminal.paste).not.toHaveBeenCalled();
  });

  it("does not throw when terminal.paste throws", async () => {
    const terminal = makeTerminal();
    terminal.paste.mockImplementation(() => {
      throw new Error("boom");
    });
    const state = makeState({ views: { "pane-1": terminal } });
    const clipboard = makeClipboard({ readText: "x" });
    await expect(pasteClipboard(state, clipboard)).resolves.toBe(false);
  });
});
