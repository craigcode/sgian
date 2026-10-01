import { describe, it, expect } from "vitest";
import { resolveKeyAction } from "../ui/src/keyboard.js";

function makeEvent(opts) {
  return {
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    key: "",
    ...opts,
  };
}

describe("resolveKeyAction", () => {
  it("returns null for unbound keys (no mod)", () => {
    expect(resolveKeyAction(makeEvent({ key: "a" }))).toBeNull();
  });

  it("returns null for mod without shift", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "w" })),
    ).toBeNull();
  });

  it("returns null for shift without mod", () => {
    expect(
      resolveKeyAction(makeEvent({ shiftKey: true, key: "w" })),
    ).toBeNull();
  });

  it("maps mod+Shift+\\ to split row", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "\\" })),
    ).toEqual({ type: "split", direction: "row" });
  });

  it("maps mod+Shift+| (shifted backslash) to split row", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "|" })),
    ).toEqual({ type: "split", direction: "row" });
  });

  it("maps mod+Shift+_ to split column", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "_" })),
    ).toEqual({ type: "split", direction: "column" });
  });

  it("maps mod+Shift+W to close", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "W" })),
    ).toEqual({ type: "close" });
  });

  it("maps mod+Shift+R to rename", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "R" })),
    ).toEqual({ type: "rename" });
  });

  it("maps mod+Shift+F to search-open", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "F" })),
    ).toEqual({ type: "search-open" });
  });

  it("maps mod+Shift+f (lowercase) to search-open", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "f" })),
    ).toEqual({ type: "search-open" });
  });

  it("maps mod+Shift+C to copy", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "C" })),
    ).toEqual({ type: "copy" });
  });

  it("maps mod+Shift+c (lowercase) to copy", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "c" })),
    ).toEqual({ type: "copy" });
  });

  it("maps mod+Shift+V to paste", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "V" })),
    ).toEqual({ type: "paste" });
  });

  it("maps mod+Shift+v (lowercase) to paste", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "v" })),
    ).toEqual({ type: "paste" });
  });

  it("bare Ctrl+C (no Shift) is not intercepted as copy", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "c" })),
    ).toBeNull();
  });

  it("bare Ctrl+V (no Shift) is not intercepted as paste", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "v" })),
    ).toBeNull();
  });

  it("bare Cmd+C (no Shift) is not intercepted as copy", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, key: "c" })),
    ).toBeNull();
  });

  it("bare Cmd+V (no Shift) is not intercepted as paste", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, key: "v" })),
    ).toBeNull();
  });

  // --- mod+1..9 focus-by-index ---

  it("maps mod+1 to focus-index 0", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "1" })),
    ).toEqual({ type: "focus-index", index: 0 });
  });

  it("maps mod+9 to focus-index 8", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, key: "9" })),
    ).toEqual({ type: "focus-index", index: 8 });
  });

  it("maps mod+5 to focus-index 4", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "5" })),
    ).toEqual({ type: "focus-index", index: 4 });
  });

  it("maps mod+3 with metaKey to focus-index 2", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, key: "3" })),
    ).toEqual({ type: "focus-index", index: 2 });
  });

  it("does not map mod+0 to focus-index (tabs are 1..9)", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "0" })),
    ).toBeNull();
  });

  it("does not map mod+Shift+1 to focus-index (Shift changes the path)", () => {
    // mod+Shift+1 goes through the Shift guard; "1" with Shift is "!" on US
    // layouts, which is not a bound mod+Shift chord.
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "!" })),
    ).toBeNull();
  });

  it("bare digit 1 (no mod) is not intercepted", () => {
    expect(resolveKeyAction(makeEvent({ key: "1" }))).toBeNull();
  });

  it("Shift+1 (no mod) is not intercepted", () => {
    expect(
      resolveKeyAction(makeEvent({ shiftKey: true, key: "!" })),
    ).toBeNull();
  });

  it("maps mod+Shift+ArrowRight to focus-directional right", () => {
    expect(
      resolveKeyAction(
        makeEvent({ ctrlKey: true, shiftKey: true, key: "ArrowRight" }),
      ),
    ).toEqual({ type: "focus-directional", direction: "right" });
  });

  it("maps mod+Shift+ArrowLeft to focus-directional left", () => {
    expect(
      resolveKeyAction(
        makeEvent({ ctrlKey: true, shiftKey: true, key: "ArrowLeft" }),
      ),
    ).toEqual({ type: "focus-directional", direction: "left" });
  });

  it("maps mod+Shift+ArrowDown to focus-directional down", () => {
    expect(
      resolveKeyAction(
        makeEvent({ ctrlKey: true, shiftKey: true, key: "ArrowDown" }),
      ),
    ).toEqual({ type: "focus-directional", direction: "down" });
  });

  it("maps mod+Shift+ArrowUp to focus-directional up", () => {
    expect(
      resolveKeyAction(
        makeEvent({ ctrlKey: true, shiftKey: true, key: "ArrowUp" }),
      ),
    ).toEqual({ type: "focus-directional", direction: "up" });
  });

  it("maps mod+Shift+Z to zoom-toggle", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "Z" })),
    ).toEqual({ type: "zoom-toggle" });
  });

  it("maps mod+Shift+z (lowercase) to zoom-toggle", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "z" })),
    ).toEqual({ type: "zoom-toggle" });
  });

  it("maps mod+Shift+S to swap", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "S" })),
    ).toEqual({ type: "swap" });
  });

  it("maps mod+Shift+s (lowercase) to swap", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "s" })),
    ).toEqual({ type: "swap" });
  });

  it("maps mod+Shift+N to new-agent", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "N" })),
    ).toEqual({ type: "new-agent" });
  });

  it("maps mod+Shift+n (lowercase) to new-agent", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "n" })),
    ).toEqual({ type: "new-agent" });
  });

  it("maps mod+Shift+P to command-palette", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "P" })),
    ).toEqual({ type: "command-palette" });
  });

  it("maps mod+Shift+p (lowercase) to command-palette", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "p" })),
    ).toEqual({ type: "command-palette" });
  });

  it("maps mod+K to command-palette", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, key: "k" })),
    ).toEqual({ type: "command-palette" });
  });

  it("maps mod+K (uppercase) to command-palette", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, key: "K" })),
    ).toEqual({ type: "command-palette" });
  });

  it("returns null for mod+Shift with unbound key", () => {
    expect(
      resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "x" })),
    ).toBeNull();
  });

  it("works with metaKey (Cmd) as mod", () => {
    expect(
      resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "\\" })),
    ).toEqual({ type: "split", direction: "row" });
  });
});

describe("keyboard lease chords", () => {
  it("maps mod+Shift+T to lease-take and mod+Shift+L to lease-release", () => {
    expect(resolveKeyAction(makeEvent({ ctrlKey: true, shiftKey: true, key: "T" }))).toEqual({
      type: "lease-take",
    });
    expect(resolveKeyAction(makeEvent({ metaKey: true, shiftKey: true, key: "l" }))).toEqual({
      type: "lease-release",
    });
    expect(resolveKeyAction(makeEvent({ ctrlKey: true, key: "t" }))).toBeNull();
  });
});
