import { describe, it, expect } from "vitest";
import {
  leaf,
  split,
  walkLeaves,
  replaceLeaf,
  pruneLeaf,
  firstPaneId,
  layoutLeafIds,
  layoutMatchesPanes,
  defaultLayoutForPanes,
  reconcileLayout,
  swapLeaves,
  isValidLayoutNode,
  clampRatio,
} from "../ui/src/layout.js";

describe("layout tree ops", () => {
  it("reconciles and builds large layouts within a baseline budget", () => {
    // ENHANCEMENTS §5: generous ceiling for noisy CI runners.
    const panes = Array.from({ length: 64 }, (_, index) => ({
      id: `pane-${index + 1}`,
    }));
    const started = performance.now();
    for (let i = 0; i < 40; i += 1) {
      const layout = defaultLayoutForPanes(panes, "pane-1");
      const result = reconcileLayout(layout, panes.map((pane) => pane.id), "pane-1");
      expect(layoutLeafIds(result.layout).length).toBe(64);
      const grown = reconcileLayout(
        result.layout,
        [...panes.map((pane) => pane.id), "pane-extra"],
        "pane-1",
      );
      expect(layoutLeafIds(grown.layout)).toContain("pane-extra");
    }
    expect(performance.now() - started).toBeLessThan(500);
  });

  describe("leaf", () => {
    it("creates a leaf node with the given id", () => {
      const node = leaf("pane-1");
      expect(node).toEqual({ type: "leaf", id: "pane-1" });
    });
  });

  describe("split", () => {
    it("creates a split node with direction and two children", () => {
      const node = split("row", leaf("pane-1"), leaf("pane-2"));
      expect(node.type).toBe("split");
      expect(node.direction).toBe("row");
      expect(node.ratio).toBe(0.5);
      expect(node.first).toEqual({ type: "leaf", id: "pane-1" });
      expect(node.second).toEqual({ type: "leaf", id: "pane-2" });
      expect(node.id).toMatch(/^split-/);
    });
  });

  describe("walkLeaves", () => {
    it("visits leaves in order", () => {
      const tree = split("row", leaf("a"), split("row", leaf("b"), leaf("c")));
      const visited = [];
      walkLeaves(tree, (l) => visited.push(l.id));
      expect(visited).toEqual(["a", "b", "c"]);
    });

    it("handles null", () => {
      const visited = [];
      walkLeaves(null, (l) => visited.push(l.id));
      expect(visited).toEqual([]);
    });
  });

  describe("replaceLeaf", () => {
    it("replaces the matching leaf", () => {
      const tree = split("row", leaf("a"), leaf("b"));
      const result = replaceLeaf(tree, "b", split("row", leaf("b"), leaf("c")));
      expect(layoutLeafIds(result)).toEqual(["a", "b", "c"]);
    });

    it("returns the same node when no match", () => {
      const tree = leaf("a");
      expect(replaceLeaf(tree, "x", leaf("y"))).toBe(tree);
    });
  });

  describe("pruneLeaf", () => {
    it("removes a leaf and collapses the parent split", () => {
      const tree = split("row", leaf("a"), leaf("b"));
      expect(pruneLeaf(tree, "b")).toEqual({ type: "leaf", id: "a" });
    });

    it("removes a leaf from a nested split", () => {
      const tree = split("row", leaf("a"), split("row", leaf("b"), leaf("c")));
      const result = pruneLeaf(tree, "b");
      expect(layoutLeafIds(result)).toEqual(["a", "c"]);
    });

    it("returns null when pruning the only leaf", () => {
      expect(pruneLeaf(leaf("a"), "a")).toBeNull();
    });

    it("handles null", () => {
      expect(pruneLeaf(null, "a")).toBeNull();
    });
  });

  describe("firstPaneId", () => {
    it("returns the first leaf id", () => {
      const tree = split("row", leaf("a"), split("row", leaf("b"), leaf("c")));
      expect(firstPaneId(tree)).toBe("a");
    });

    it("returns null for null tree", () => {
      expect(firstPaneId(null)).toBeNull();
    });
  });

  describe("layoutLeafIds", () => {
    it("returns in-order leaf ids", () => {
      const tree = split("row", leaf("a"), split("row", leaf("b"), leaf("c")));
      expect(layoutLeafIds(tree)).toEqual(["a", "b", "c"]);
    });
  });

  describe("layoutMatchesPanes", () => {
    it("matches when leaf ids equal pane ids", () => {
      const tree = split("row", leaf("a"), leaf("b"));
      const panes = [{ id: "a" }, { id: "b" }];
      expect(layoutMatchesPanes(tree, panes)).toBe(true);
    });

    it("does not match when counts differ", () => {
      const tree = leaf("a");
      const panes = [{ id: "a" }, { id: "b" }];
      expect(layoutMatchesPanes(tree, panes)).toBe(false);
    });

    it("does not match when ids differ", () => {
      const tree = leaf("a");
      const panes = [{ id: "b" }];
      expect(layoutMatchesPanes(tree, panes)).toBe(false);
    });

    it("returns false for null layout", () => {
      expect(layoutMatchesPanes(null, [])).toBe(false);
    });

    it("returns false for a structurally invalid persisted layout even when leaf ids match", () => {
      // A persisted split with a null ratio (JSON.stringify of NaN) must not
      // match — it would render `--first-size: NaN%` and corrupt further.
      const tree = {
        type: "split",
        id: "split-x",
        direction: "row",
        ratio: null,
        first: leaf("a"),
        second: leaf("b"),
      };
      expect(layoutMatchesPanes(tree, [{ id: "a" }, { id: "b" }])).toBe(false);
    });

    it("returns false for a split missing a child", () => {
      const tree = {
        type: "split",
        id: "split-x",
        direction: "row",
        ratio: 0.5,
        first: leaf("a"),
      };
      expect(layoutMatchesPanes(tree, [{ id: "a" }])).toBe(false);
    });

    it("returns false for a tree with duplicate leaf ids even when the pane set matches", () => {
      // A tampered tree repeating "a" twice has a leaf-id Set of size 1, which
      // would match a single-pane set without the duplicate check.
      const tree = split("row", leaf("a"), leaf("a"));
      expect(layoutMatchesPanes(tree, [{ id: "a" }])).toBe(false);
    });
  });

  describe("isValidLayoutNode", () => {
    it("accepts a leaf and a well-formed tree", () => {
      expect(isValidLayoutNode(leaf("a"))).toBe(true);
      expect(
        isValidLayoutNode(split("row", leaf("a"), split("column", leaf("b"), leaf("c")))),
      ).toBe(true);
    });

    it("rejects null, non-objects, and unknown node types", () => {
      expect(isValidLayoutNode(null)).toBe(false);
      expect(isValidLayoutNode("leaf")).toBe(false);
      expect(isValidLayoutNode({ type: "grid", id: "x" })).toBe(false);
      expect(isValidLayoutNode({})).toBe(false);
    });

    it("rejects a leaf without a non-empty string id", () => {
      expect(isValidLayoutNode({ type: "leaf" })).toBe(false);
      expect(isValidLayoutNode({ type: "leaf", id: "" })).toBe(false);
      expect(isValidLayoutNode({ type: "leaf", id: 7 })).toBe(false);
    });

    it("rejects a split with a missing, NaN, or out-of-range ratio", () => {
      const base = { type: "split", id: "s", direction: "row", first: leaf("a"), second: leaf("b") };
      expect(isValidLayoutNode({ ...base })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: NaN })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: null })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: Infinity })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: 0 })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: 1 })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: -0.5 })).toBe(false);
      expect(isValidLayoutNode({ ...base, ratio: 0.5 })).toBe(true);
    });

    it("rejects duplicate leaf ids", () => {
      expect(isValidLayoutNode(split("row", leaf("a"), leaf("a")))).toBe(false);
      expect(
        isValidLayoutNode(split("row", leaf("a"), split("column", leaf("b"), leaf("a")))),
      ).toBe(false);
      expect(isValidLayoutNode(split("row", leaf("a"), leaf("b")))).toBe(true);
    });

    it("rejects a split missing children or with an invalid nested child", () => {
      expect(
        isValidLayoutNode({ type: "split", id: "s", direction: "row", ratio: 0.5, first: leaf("a") }),
      ).toBe(false);
      expect(
        isValidLayoutNode({
          type: "split",
          id: "s",
          direction: "row",
          ratio: 0.5,
          first: leaf("a"),
          second: { type: "split", id: "s2", direction: "row", ratio: NaN, first: leaf("b"), second: leaf("c") },
        }),
      ).toBe(false);
    });
  });

  describe("clampRatio", () => {
    it("clamps into the 0.18–0.82 band", () => {
      expect(clampRatio(0.05)).toBe(0.18);
      expect(clampRatio(0.95)).toBe(0.82);
      expect(clampRatio(0.5)).toBe(0.5);
      expect(clampRatio(0.18)).toBe(0.18);
      expect(clampRatio(0.82)).toBe(0.82);
    });

    it("recovers non-finite input to 0.5 (NaN ratio + delta must never poison the tree)", () => {
      expect(clampRatio(NaN)).toBe(0.5);
      expect(clampRatio(NaN + 0.02)).toBe(0.5);
      expect(clampRatio(Infinity)).toBe(0.5);
      expect(clampRatio(-Infinity)).toBe(0.5);
      expect(clampRatio(undefined)).toBe(0.5);
    });
  });

  describe("defaultLayoutForPanes", () => {
    it("returns null for empty panes", () => {
      expect(defaultLayoutForPanes([], null)).toBeNull();
    });

    it("places the active pane first", () => {
      const panes = [{ id: "a" }, { id: "b" }, { id: "c" }];
      const layout = defaultLayoutForPanes(panes, "c");
      expect(layoutLeafIds(layout)).toEqual(["c", "a", "b"]);
    });

    it("creates a single leaf for one pane", () => {
      const panes = [{ id: "a" }];
      expect(defaultLayoutForPanes(panes, "a")).toEqual({ type: "leaf", id: "a" });
    });
  });
});

describe("reconcileLayout", () => {
  it("folds in a new pane without disturbing existing panes", () => {
    const layout = split("row", leaf("a"), leaf("b"));
    const result = reconcileLayout(layout, ["a", "b", "c"], "a");
    expect(result.changed).toBe(true);
    expect(layoutLeafIds(result.layout)).toContain("a");
    expect(layoutLeafIds(result.layout)).toContain("b");
    expect(layoutLeafIds(result.layout)).toContain("c");
    // The existing leaves should still be in a split together
    const ids = layoutLeafIds(result.layout);
    expect(ids).toEqual(["a", "b", "c"]);
  });

  it("does not change when layout already matches panes", () => {
    const layout = split("row", leaf("a"), leaf("b"));
    const result = reconcileLayout(layout, ["a", "b"], "a");
    expect(result.changed).toBe(false);
    expect(result.layout).toBe(layout);
  });

  it("prunes leaves whose pane is gone", () => {
    const layout = split("row", leaf("a"), leaf("b"));
    const result = reconcileLayout(layout, ["a"], "a");
    expect(result.changed).toBe(true);
    expect(layoutLeafIds(result.layout)).toEqual(["a"]);
  });

  it("suggests a new active id when current active is gone", () => {
    const layout = split("row", leaf("a"), leaf("b"));
    const result = reconcileLayout(layout, ["b"], "a");
    expect(result.newActiveId).toBe("b");
  });

  it("creates a leaf from null layout", () => {
    const result = reconcileLayout(null, ["a"], null);
    expect(result.changed).toBe(true);
    expect(result.layout).toEqual({ type: "leaf", id: "a" });
  });
});

describe("swapLeaves", () => {
  it("exchanges two leaf positions and preserves all other tree structure", () => {
    // Tree: split(row, split(row, a, b), c) — leaves [a, b, c]
    const tree = split("row", split("row", leaf("a"), leaf("b")), leaf("c"));
    const swapped = swapLeaves(tree, "a", "c");
    expect(layoutLeafIds(swapped)).toEqual(["c", "b", "a"]);
    // The split directions + ratios are preserved (only the targeted ids trade).
    expect(swapped.direction).toBe("row");
    expect(swapped.first.direction).toBe("row");
    expect(swapped.first.ratio).toBe(tree.first.ratio);
    expect(swapped.ratio).toBe(tree.ratio);
  });

  it("swapping the same pair twice returns the original tree", () => {
    const tree = split("row", split("row", leaf("a"), leaf("b")), leaf("c"));
    const once = swapLeaves(tree, "a", "c");
    const twice = swapLeaves(once, "a", "c");
    expect(twice).toEqual(tree);
  });

  it("returns the same node reference when neither id is present", () => {
    const tree = split("row", leaf("a"), leaf("b"));
    expect(swapLeaves(tree, "x", "y")).toBe(tree);
  });

  it("returns the same node when idA === idB", () => {
    const tree = split("row", leaf("a"), leaf("b"));
    expect(swapLeaves(tree, "a", "a")).toBe(tree);
  });

  it("returns null for null tree", () => {
    expect(swapLeaves(null, "a", "b")).toBeNull();
  });

  it("swaps two leaves in a simple 2-pane split", () => {
    const tree = split("row", leaf("a"), leaf("b"));
    expect(layoutLeafIds(swapLeaves(tree, "a", "b"))).toEqual(["b", "a"]);
  });

  it("is a no-op on a single-pane layout when ids match (idA === idB)", () => {
    const tree = leaf("a");
    expect(swapLeaves(tree, "a", "a")).toBe(tree);
    // The single-pane no-op guarantee (>= 2 panes required) is asserted at the
    // action layer, which guards on pane count before invoking this pure op.
  });
});
