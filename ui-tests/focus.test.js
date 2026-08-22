import { describe, it, expect } from "vitest";
import {
  resolveAdjacentIndex,
  resolveDirectionalFocus,
  resolveFocusIndex,
} from "../ui/src/focus.js";

describe("resolveFocusIndex", () => {
  it("returns the pane id at a valid index", () => {
    expect(resolveFocusIndex(["a", "b", "c"], 0)).toBe("a");
    expect(resolveFocusIndex(["a", "b", "c"], 1)).toBe("b");
    expect(resolveFocusIndex(["a", "b", "c"], 2)).toBe("c");
  });

  it("returns null for an out-of-range high index", () => {
    expect(resolveFocusIndex(["a", "b", "c"], 3)).toBeNull();
    expect(resolveFocusIndex(["a", "b", "c"], 9)).toBeNull();
  });

  it("returns null for a negative index", () => {
    expect(resolveFocusIndex(["a", "b", "c"], -1)).toBeNull();
  });

  it("returns null for an empty array", () => {
    expect(resolveFocusIndex([], 0)).toBeNull();
  });

  it("returns null for a non-array input", () => {
    expect(resolveFocusIndex(null, 0)).toBeNull();
    expect(resolveFocusIndex(undefined, 0)).toBeNull();
  });

  it("handles a single-pane array", () => {
    expect(resolveFocusIndex(["only"], 0)).toBe("only");
    expect(resolveFocusIndex(["only"], 1)).toBeNull();
  });
});

describe("resolveAdjacentIndex", () => {
  it("returns -1 for a single pane", () => {
    expect(resolveAdjacentIndex(["a"], "a", 1)).toBe(-1);
  });

  it("returns -1 for empty list", () => {
    expect(resolveAdjacentIndex([], "a", 1)).toBe(-1);
  });

  it("cycles forward", () => {
    expect(resolveAdjacentIndex(["a", "b", "c"], "a", 1)).toBe(1);
    expect(resolveAdjacentIndex(["a", "b", "c"], "b", 1)).toBe(2);
  });

  it("wraps around forward", () => {
    expect(resolveAdjacentIndex(["a", "b", "c"], "c", 1)).toBe(0);
  });

  it("cycles backward", () => {
    expect(resolveAdjacentIndex(["a", "b", "c"], "c", -1)).toBe(1);
    expect(resolveAdjacentIndex(["a", "b", "c"], "b", -1)).toBe(0);
  });

  it("wraps around backward", () => {
    expect(resolveAdjacentIndex(["a", "b", "c"], "a", -1)).toBe(2);
  });

  it("handles unknown current id by starting at 0", () => {
    expect(resolveAdjacentIndex(["a", "b", "c"], "unknown", 1)).toBe(1);
  });
});

describe("resolveDirectionalFocus", () => {
  // 2x2 layout:
  // +---+---+
  // | A | B |  (top row)
  // +---+---+
  // | C | D |  (bottom row)
  // +---+---+
  const paneRects2x2 = [
    { id: "A", rect: { left: 0, top: 0, right: 100, bottom: 100 } },
    { id: "B", rect: { left: 100, top: 0, right: 200, bottom: 100 } },
    { id: "C", rect: { left: 0, top: 100, right: 100, bottom: 200 } },
    { id: "D", rect: { left: 100, top: 100, right: 200, bottom: 200 } },
  ];

  it("from top-left, right goes to top-right", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "A", "right")).toBe("B");
  });

  it("from top-left, down goes to bottom-left", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "A", "down")).toBe("C");
  });

  it("from top-right, left goes to top-left", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "B", "left")).toBe("A");
  });

  it("from bottom-right, up goes to top-right", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "D", "up")).toBe("B");
  });

  it("from bottom-right, left goes to bottom-left", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "D", "left")).toBe("C");
  });

  it("returns null at an edge with no neighbor", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "A", "left")).toBeNull();
    expect(resolveDirectionalFocus(paneRects2x2, "A", "up")).toBeNull();
  });

  it("returns null for unknown active id", () => {
    expect(resolveDirectionalFocus(paneRects2x2, "unknown", "right")).toBeNull();
  });

  it("uses geometry not linear order (nested splits)", () => {
    // Layout: A on left half, B-C split on right half (B top, C bottom)
    // +-----+-----+
    // |     |  B  |
    // |  A  +-----+
    // |     |  C  |
    // +-----+-----+
    const nested = [
      { id: "A", rect: { left: 0, top: 0, right: 100, bottom: 200 } },
      { id: "B", rect: { left: 100, top: 0, right: 200, bottom: 100 } },
      { id: "C", rect: { left: 100, top: 100, right: 200, bottom: 200 } },
    ];
    // From A, right should go to B (top-right, vertically closest to A's center)
    // even though C is also to the right.
    expect(resolveDirectionalFocus(nested, "A", "right")).toBe("B");
    // From C, up should go to B (geometric neighbor above C), not A.
    expect(resolveDirectionalFocus(nested, "C", "up")).toBe("B");
  });

  it("handles 3-pane row layout", () => {
    // +---+---+---+
    // | A | B | C |
    // +---+---+---+
    const row3 = [
      { id: "A", rect: { left: 0, top: 0, right: 100, bottom: 100 } },
      { id: "B", rect: { left: 100, top: 0, right: 200, bottom: 100 } },
      { id: "C", rect: { left: 200, top: 0, right: 300, bottom: 100 } },
    ];
    expect(resolveDirectionalFocus(row3, "A", "right")).toBe("B");
    expect(resolveDirectionalFocus(row3, "B", "right")).toBe("C");
    expect(resolveDirectionalFocus(row3, "C", "left")).toBe("B");
    expect(resolveDirectionalFocus(row3, "C", "left")).not.toBe("A");
  });

  it("tie-breaks deterministically among equidistant candidates", () => {
    // Two panes (B, C) stacked on the right of A, both equidistant.
    // +-----+-----+
    // |  A  |  B  |
    // |     +-----+
    // |     |  C  |
    // +-----+-----+
    // A's center Y = 100. B's center Y = 50. C's center Y = 150.
    // Both have perpendicular distance 50 from A's center, both overlap A
    // fully (overlap > 0), and both have the same primary distance (0).
    // The tie-break prefers topmost then leftmost → B wins.
    const nested = [
      { id: "A", rect: { left: 0, top: 0, right: 100, bottom: 200 } },
      { id: "B", rect: { left: 100, top: 0, right: 200, bottom: 100 } },
      { id: "C", rect: { left: 100, top: 100, right: 200, bottom: 200 } },
    ];
    const result1 = resolveDirectionalFocus(nested, "A", "right");
    const result2 = resolveDirectionalFocus(nested, "A", "right");
    expect(result1).toBe("B");
    expect(result2).toBe("B"); // deterministic: same result every time
  });

  it("handles column-split layout (vertical neighbors)", () => {
    // +---+
    // | A |
    // +---+
    // | B |
    // +---+
    // | C |
    // +---+
    const col3 = [
      { id: "A", rect: { left: 0, top: 0, right: 100, bottom: 100 } },
      { id: "B", rect: { left: 0, top: 100, right: 100, bottom: 200 } },
      { id: "C", rect: { left: 0, top: 200, right: 100, bottom: 300 } },
    ];
    expect(resolveDirectionalFocus(col3, "A", "down")).toBe("B");
    expect(resolveDirectionalFocus(col3, "B", "down")).toBe("C");
    expect(resolveDirectionalFocus(col3, "C", "up")).toBe("B");
    expect(resolveDirectionalFocus(col3, "A", "up")).toBeNull();
    expect(resolveDirectionalFocus(col3, "C", "down")).toBeNull();
  });

  it("directional focus works across mixed nesting at 4 panes", () => {
    // 2x2 grid via nested splits:
    // +---+---+
    // | A | B |
    // +---+---+
    // | C | D |
    // +---+---+
    const mixed4 = [
      { id: "A", rect: { left: 0, top: 0, right: 100, bottom: 100 } },
      { id: "B", rect: { left: 100, top: 0, right: 200, bottom: 100 } },
      { id: "C", rect: { left: 0, top: 100, right: 100, bottom: 200 } },
      { id: "D", rect: { left: 100, top: 100, right: 200, bottom: 200 } },
    ];
    // From A: right→B, down→C
    expect(resolveDirectionalFocus(mixed4, "A", "right")).toBe("B");
    expect(resolveDirectionalFocus(mixed4, "A", "down")).toBe("C");
    // From D: left→C, up→B
    expect(resolveDirectionalFocus(mixed4, "D", "left")).toBe("C");
    expect(resolveDirectionalFocus(mixed4, "D", "up")).toBe("B");
    // From B: left→A, down→D
    expect(resolveDirectionalFocus(mixed4, "B", "left")).toBe("A");
    expect(resolveDirectionalFocus(mixed4, "B", "down")).toBe("D");
    // From C: right→D, up→A
    expect(resolveDirectionalFocus(mixed4, "C", "right")).toBe("D");
    expect(resolveDirectionalFocus(mixed4, "C", "up")).toBe("A");
  });
});
