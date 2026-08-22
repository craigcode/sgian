import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  resolveSearchKeyAction,
  activeSearchAddon,
  findNext,
  findPrevious,
  createSearchState,
  openSearch,
  closeSearch,
} from "../ui/src/search.js";

function makeEvent(opts) {
  return {
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    key: "",
    preventDefault: () => {},
    ...opts,
  };
}

function makeAddon() {
  return {
    findNext: vi.fn(() => true),
    findPrevious: vi.fn(() => true),
    clearDecorations: vi.fn(),
    clearActiveDecoration: vi.fn(),
    dispose: vi.fn(),
  };
}

function makeState({ activePaneId = "pane-1", views = {} } = {}) {
  const terminalViews = new Map();
  for (const [id, addon] of Object.entries(views)) {
    terminalViews.set(id, { terminal: {}, searchAddon: addon });
  }
  return { activePaneId, terminalViews };
}

describe("resolveSearchKeyAction", () => {
  it("maps Enter to search-next", () => {
    expect(resolveSearchKeyAction(makeEvent({ key: "Enter" }))).toEqual({
      type: "search-next",
    });
  });

  it("maps Shift+Enter to search-previous", () => {
    expect(
      resolveSearchKeyAction(makeEvent({ shiftKey: true, key: "Enter" })),
    ).toEqual({ type: "search-previous" });
  });

  it("maps Escape to search-close", () => {
    expect(resolveSearchKeyAction(makeEvent({ key: "Escape" }))).toEqual({
      type: "search-close",
    });
  });

  it("returns null for unbound keys", () => {
    expect(resolveSearchKeyAction(makeEvent({ key: "a" }))).toBeNull();
    expect(resolveSearchKeyAction(makeEvent({ key: "Tab" }))).toBeNull();
  });
});

describe("activeSearchAddon", () => {
  it("returns the active pane's search addon", () => {
    const addonA = makeAddon();
    const addonB = makeAddon();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": addonA, "pane-b": addonB },
    });
    expect(activeSearchAddon(state)).toBe(addonA);
  });

  it("retargets when the active pane changes", () => {
    const addonA = makeAddon();
    const addonB = makeAddon();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": addonA, "pane-b": addonB },
    });
    expect(activeSearchAddon(state)).toBe(addonA);
    state.activePaneId = "pane-b";
    expect(activeSearchAddon(state)).toBe(addonB);
  });

  it("returns null when the active pane has no view", () => {
    const state = makeState({ activePaneId: "pane-z", views: {} });
    expect(activeSearchAddon(state)).toBeNull();
  });
});

describe("findNext", () => {
  it("calls findNext on the active pane's addon with the query", () => {
    const addon = makeAddon();
    const state = makeState({ views: { "pane-1": addon } });
    findNext(state, "error");
    expect(addon.findNext).toHaveBeenCalledWith("error", expect.any(Object));
  });

  it("returns true when a match is found", () => {
    const addon = makeAddon();
    addon.findNext.mockReturnValue(true);
    const state = makeState({ views: { "pane-1": addon } });
    expect(findNext(state, "error")).toBe(true);
  });

  it("returns false when no match is found (safe no-op)", () => {
    const addon = makeAddon();
    addon.findNext.mockReturnValue(false);
    const state = makeState({ views: { "pane-1": addon } });
    expect(findNext(state, "nomatch")).toBe(false);
    expect(addon.findNext).toHaveBeenCalled();
  });

  it("does not call the addon when the query is empty", () => {
    const addon = makeAddon();
    const state = makeState({ views: { "pane-1": addon } });
    expect(findNext(state, "")).toBe(false);
    expect(addon.findNext).not.toHaveBeenCalled();
  });

  it("does not call the addon when no active view exists", () => {
    const state = makeState({ activePaneId: "pane-z", views: {} });
    expect(findNext(state, "error")).toBe(false);
  });

  it("only drives the active pane's addon, not siblings", () => {
    const addonA = makeAddon();
    const addonB = makeAddon();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": addonA, "pane-b": addonB },
    });
    findNext(state, "x");
    expect(addonA.findNext).toHaveBeenCalled();
    expect(addonB.findNext).not.toHaveBeenCalled();
  });

  it("retargets when the active pane changes", () => {
    const addonA = makeAddon();
    const addonB = makeAddon();
    const state = makeState({
      activePaneId: "pane-a",
      views: { "pane-a": addonA, "pane-b": addonB },
    });
    findNext(state, "x");
    expect(addonA.findNext).toHaveBeenCalledTimes(1);
    expect(addonB.findNext).not.toHaveBeenCalled();
    state.activePaneId = "pane-b";
    findNext(state, "x");
    expect(addonB.findNext).toHaveBeenCalledTimes(1);
  });

  it("does not throw when the addon throws", () => {
    const addon = makeAddon();
    addon.findNext.mockImplementation(() => {
      throw new Error("boom");
    });
    const state = makeState({ views: { "pane-1": addon } });
    expect(() => findNext(state, "error")).not.toThrow();
    expect(findNext(state, "error")).toBe(false);
  });

  it("wraps around at the end of the match set (repeated findNext keeps cycling)", () => {
    const addon = makeAddon();
    // Simulate wrap: first call finds, subsequent calls keep finding (addon wraps internally).
    addon.findNext.mockReturnValue(true);
    const state = makeState({ views: { "pane-1": addon } });
    for (let i = 0; i < 5; i++) findNext(state, "term");
    expect(addon.findNext).toHaveBeenCalledTimes(5);
  });
});

describe("findPrevious", () => {
  it("calls findPrevious on the active pane's addon with the query", () => {
    const addon = makeAddon();
    const state = makeState({ views: { "pane-1": addon } });
    findPrevious(state, "error");
    expect(addon.findPrevious).toHaveBeenCalledWith(
      "error",
      expect.any(Object),
    );
  });

  it("does not call the addon when the query is empty", () => {
    const addon = makeAddon();
    const state = makeState({ views: { "pane-1": addon } });
    expect(findPrevious(state, "")).toBe(false);
    expect(addon.findPrevious).not.toHaveBeenCalled();
  });

  it("returns false and does not throw when no match", () => {
    const addon = makeAddon();
    addon.findPrevious.mockReturnValue(false);
    const state = makeState({ views: { "pane-1": addon } });
    expect(findPrevious(state, "nomatch")).toBe(false);
  });

  it("wraps around at the beginning of the match set", () => {
    const addon = makeAddon();
    addon.findPrevious.mockReturnValue(true);
    const state = makeState({ views: { "pane-1": addon } });
    for (let i = 0; i < 5; i++) findPrevious(state, "term");
    expect(addon.findPrevious).toHaveBeenCalledTimes(5);
  });
});

describe("search bar state", () => {
  it("createSearchState starts closed with an empty query", () => {
    const s = createSearchState();
    expect(s.open).toBe(false);
    expect(s.query).toBe("");
  });

  it("openSearch sets open=true", () => {
    const s = createSearchState();
    openSearch(s);
    expect(s.open).toBe(true);
  });

  it("closeSearch sets open=false and clears the query", () => {
    const s = createSearchState();
    openSearch(s);
    s.query = "abc";
    closeSearch(s);
    expect(s.open).toBe(false);
    expect(s.query).toBe("");
  });
});
