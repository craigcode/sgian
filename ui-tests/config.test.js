import { describe, it, expect } from "vitest";
import {
  DEFAULT_TERMINAL_FONT,
  DEFAULT_TERMINAL_FONT_SIZE,
  DEFAULT_TERMINAL_THEME,
  mergeAppearance,
  resolveTheme,
  resolveFontFamily,
  resolveFontSize,
  applyAppearanceToTerminal,
  liveReapplyAppearance,
  ptyColsForTerminal,
  rightEdgeGuardCols,
} from "../ui/src/config.js";

describe("mergeAppearance", () => {
  it("returns null appearance for null config", () => {
    expect(mergeAppearance(null)).toEqual({
      fontFamily: null,
      fontSize: null,
      theme: null,
    });
  });

  it("maps backend config keys to appearance keys", () => {
    const theme = { background: "#000" };
    const config = {
      font_family: "Fira Code",
      font_size: 14,
      theme,
    };
    expect(mergeAppearance(config)).toEqual({
      fontFamily: "Fira Code",
      fontSize: 14,
      theme,
    });
  });

  it("handles missing fields", () => {
    expect(mergeAppearance({})).toEqual({
      fontFamily: null,
      fontSize: null,
      theme: null,
    });
  });
});

describe("resolveTheme", () => {
  it("returns default theme when appearance has no theme", () => {
    expect(resolveTheme({ theme: null })).toBe(DEFAULT_TERMINAL_THEME);
  });

  it("merges configured theme over defaults", () => {
    const appearance = { theme: { background: "#000", foreground: "#fff" } };
    const theme = resolveTheme(appearance);
    expect(theme.background).toBe("#000");
    expect(theme.foreground).toBe("#fff");
    // Default keys preserved
    expect(theme.cursor).toBe(DEFAULT_TERMINAL_THEME.cursor);
  });
});

describe("resolveFontFamily", () => {
  it("returns default when no appearance", () => {
    expect(resolveFontFamily(null)).toBe(DEFAULT_TERMINAL_FONT);
  });

  it("returns configured font family", () => {
    expect(resolveFontFamily({ fontFamily: "Fira Code" })).toBe("Fira Code");
  });
});

describe("resolveFontSize", () => {
  it("returns default when no appearance", () => {
    expect(resolveFontSize(null)).toBe(DEFAULT_TERMINAL_FONT_SIZE);
  });

  it("returns configured font size", () => {
    expect(resolveFontSize({ fontSize: 16 })).toBe(16);
  });
});

describe("applyAppearanceToTerminal", () => {
  it("sets theme, fontFamily, and fontSize on the terminal options", () => {
    const terminal = { options: {} };
    const appearance = {
      fontFamily: "Fira Code",
      fontSize: 14,
      theme: { background: "#000" },
    };
    applyAppearanceToTerminal(terminal, appearance);
    expect(terminal.options.fontFamily).toBe("Fira Code");
    expect(terminal.options.fontSize).toBe(14);
    expect(terminal.options.theme.background).toBe("#000");
  });

  it("is a no-op for null terminal", () => {
    expect(() => applyAppearanceToTerminal(null, { fontFamily: "x" })).not.toThrow();
  });
});

describe("ptyColsForTerminal", () => {
  it("subtracts the guard column", () => {
    expect(ptyColsForTerminal(80)).toBe(79);
  });

  it("enforces a minimum of 2", () => {
    expect(ptyColsForTerminal(1)).toBe(2);
    expect(ptyColsForTerminal(0)).toBe(2);
  });

  it("uses the same guard on every platform (no Windows special case)", () => {
    // The 2-col Windows experiment was reverted: the stagger root cause is in
    // the app/ConPTY stream, so sizing guards can't address it.
    expect(rightEdgeGuardCols()).toBe(1);
    expect(ptyColsForTerminal(80)).toBe(79);
  });
});

// Helper: create a stub terminal view for live-reapply tests.
function makeStubView(opts = {}) {
  const terminal = {
    options: {
      theme: { background: "#fff" },
      fontFamily: "old-font",
      fontSize: 10,
    },
    rows: 24,
    _written: [],
    _refreshCalls: 0,
    write(data) { this._written.push(data); },
    reset() { this._reset = true; },
    clear() { this._clear = true; },
    dispose() { this._disposed = true; },
    refresh(start, end) { this._refreshCalls++; this._refreshRange = [start, end]; },
  };
  const fitAddon = opts.withFit
    ? { _fitCalls: 0, fit() { this._fitCalls++; } }
    : null;
  return { terminal, fitAddon };
}

describe("liveReapplyAppearance", () => {
  it("sets new theme/fontFamily/fontSize on every existing view and calls fit (VAL-CFG-013)", () => {
    const viewA = makeStubView({ withFit: true });
    const viewB = makeStubView({ withFit: true });
    const appearance = {
      fontFamily: "Fira Code",
      fontSize: 16,
      theme: { background: "#000", foreground: "#0f0" },
    };

    const count = liveReapplyAppearance([viewA, viewB], appearance);

    expect(count).toBe(2);
    // Both views get the new options.
    for (const { terminal } of [viewA, viewB]) {
      expect(terminal.options.fontFamily).toBe("Fira Code");
      expect(terminal.options.fontSize).toBe(16);
      expect(terminal.options.theme.background).toBe("#000");
      expect(terminal.options.theme.foreground).toBe("#0f0");
    }
    // Both fit addons were called (refit triggered).
    expect(viewA.fitAddon._fitCalls).toBe(1);
    expect(viewB.fitAddon._fitCalls).toBe(1);
    // Both terminals were refreshed (repaint with new theme).
    expect(viewA.terminal._refreshCalls).toBe(1);
    expect(viewB.terminal._refreshCalls).toBe(1);
  });

  it("does not dispose, reset, clear, or recreate any terminal (VAL-CFG-013)", () => {
    const view = makeStubView({ withFit: true });
    const appearance = { fontFamily: "x", fontSize: 20, theme: { background: "#111" } };

    liveReapplyAppearance([view], appearance);

    expect(view.terminal._reset).toBeUndefined();
    expect(view.terminal._clear).toBeUndefined();
    expect(view.terminal._disposed).toBeUndefined();
    // refresh IS called (it's a repaint, not a teardown) — this is expected.
    expect(view.terminal._refreshCalls).toBe(1);
  });

  it("preserves in-flight output: does not call reset/clear/write/dispose (VAL-CFG-021)", () => {
    const view = makeStubView({ withFit: true });
    // Simulate mid-output: terminal already has written content.
    view.terminal._written.push("partial output line 1\n");
    view.terminal._written.push("partial output line 2");

    const appearance = { fontFamily: "y", fontSize: 18, theme: { background: "#222" } };
    liveReapplyAppearance([view], appearance);

    // No new write calls (no reset-and-replay), no clear, no dispose.
    expect(view.terminal._written).toEqual([
      "partial output line 1\n",
      "partial output line 2",
    ]);
    expect(view.terminal._reset).toBeUndefined();
    expect(view.terminal._clear).toBeUndefined();
    expect(view.terminal._disposed).toBeUndefined();
    // But options WERE updated (re-apply happened).
    expect(view.terminal.options.fontSize).toBe(18);
    expect(view.terminal.options.theme.background).toBe("#222");
  });

  it("handles a view without a fit addon (refit skipped, options still set)", () => {
    const view = makeStubView({ withFit: false });
    expect(view.fitAddon).toBe(null);

    const appearance = { fontFamily: "z", fontSize: 14, theme: { background: "#333" } };
    const count = liveReapplyAppearance([view], appearance);

    expect(count).toBe(1);
    expect(view.terminal.options.fontFamily).toBe("z");
    expect(view.terminal.options.fontSize).toBe(14);
    expect(view.terminal.options.theme.background).toBe("#333");
  });

  it("handles a fit that throws without breaking other views", () => {
    const badView = {
      terminal: { options: {} },
      fitAddon: { fit() { throw new Error("fit failed"); } },
    };
    const goodView = makeStubView({ withFit: true });
    const appearance = { fontFamily: "f", fontSize: 12, theme: { background: "#444" } };

    const count = liveReapplyAppearance([badView, goodView], appearance);

    expect(count).toBe(2);
    expect(goodView.fitAddon._fitCalls).toBe(1);
    expect(goodView.terminal.options.theme.background).toBe("#444");
  });

  it("returns 0 for null/empty views or null appearance", () => {
    expect(liveReapplyAppearance(null, { fontSize: 12 })).toBe(0);
    expect(liveReapplyAppearance([], { fontSize: 12 })).toBe(0);
    expect(liveReapplyAppearance([makeStubView()], null)).toBe(0);
  });

  it("works with a Map values iterator (as used by state.terminalViews)", () => {
    const viewA = makeStubView({ withFit: true });
    const viewB = makeStubView({ withFit: true });
    const views = new Map([
      ["pane-1", viewA],
      ["pane-2", viewB],
    ]);
    const appearance = { fontFamily: "map", fontSize: 13, theme: { background: "#555" } };

    const count = liveReapplyAppearance(views.values(), appearance);

    expect(count).toBe(2);
    expect(viewA.terminal.options.fontFamily).toBe("map");
    expect(viewB.terminal.options.fontFamily).toBe("map");
    expect(viewA.fitAddon._fitCalls).toBe(1);
    expect(viewB.fitAddon._fitCalls).toBe(1);
  });
});
