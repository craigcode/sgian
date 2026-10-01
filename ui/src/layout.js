// layout.js — pure binary layout-tree operations.
//
// The layout is a binary tree: leaves carry a pane id, splits carry a direction
// ("row"|"column"), a ratio (first child's fraction, clamped 0.18–0.82), and
// exactly two children. All functions here are pure (no DOM, no Tauri) so they
// can be unit-tested directly.

let idCounter = 0;

/**
 * Generate a unique id with the given prefix. Uses crypto.randomUUID when
 * available, falling back to a counter + timestamp.
 */
export function makeUniqueId(prefix) {
  const randomId = globalThis.crypto?.randomUUID?.();
  if (randomId) return `${prefix}-${randomId}`;

  idCounter += 1;
  return `${prefix}-${Date.now()}-${idCounter}`;
}

/** Create a leaf node referencing pane `id`. */
export function leaf(id) {
  return { type: "leaf", id };
}

/** Create a split node with the given direction and two children. */
export function split(direction, first, second) {
  return {
    type: "split",
    id: makeUniqueId("split"),
    direction,
    ratio: 0.5,
    first,
    second,
  };
}

/** In-order traversal: visit every leaf node. */
export function walkLeaves(node, visit) {
  if (!node) return;
  if (node.type === "leaf") {
    visit(node);
    return;
  }
  walkLeaves(node.first, visit);
  walkLeaves(node.second, visit);
}

/** Return a new tree with the leaf matching `paneId` replaced by `replacement`. */
export function replaceLeaf(node, paneId, replacement) {
  if (!node) return node;
  if (node.type === "leaf") {
    return node.id === paneId ? replacement : node;
  }
  return {
    ...node,
    first: replaceLeaf(node.first, paneId, replacement),
    second: replaceLeaf(node.second, paneId, replacement),
  };
}

/**
 * Return a new tree with the leaf matching `paneId` pruned. If a split loses a
 * child, it collapses to the surviving sibling.
 */
export function pruneLeaf(node, paneId) {
  if (!node) return null;
  if (node.type === "leaf") {
    return node.id === paneId ? null : node;
  }

  const first = pruneLeaf(node.first, paneId);
  const second = pruneLeaf(node.second, paneId);
  if (!first) return second;
  if (!second) return first;
  return { ...node, first, second };
}

/** Return the first (leftmost) leaf id in the tree, or null. */
export function firstPaneId(node) {
  let found = null;
  walkLeaves(node, (leafNode) => {
    if (!found) found = leafNode.id;
  });
  return found;
}

/** Return an in-order array of all leaf ids in the tree. */
export function layoutLeafIds(node) {
  const ids = [];
  walkLeaves(node, (leafNode) => ids.push(leafNode.id));
  return ids;
}

/**
 * Structurally validate a (possibly persisted, possibly tampered) layout node.
 * Leaves must carry a non-empty string id; splits must carry both children and
 * a finite ratio strictly inside (0, 1); leaf ids must be unique (a tampered
 * tree repeating an id would render the same pane twice and corrupt every
 * id-keyed map). Anything else (missing children, NaN/null ratio, unknown
 * type) is invalid — callers fall back to a fresh layout instead of throwing
 * mid-render.
 *
 * @param {object|null} node layout tree node
 * @returns {boolean}
 */
export function isValidLayoutNode(node) {
  if (!isStructurallyValid(node)) return false;
  const ids = layoutLeafIds(node);
  return new Set(ids).size === ids.length;
}

function isStructurallyValid(node) {
  if (!node || typeof node !== "object") return false;
  if (node.type === "leaf") {
    return typeof node.id === "string" && node.id.length > 0;
  }
  if (node.type === "split") {
    return (
      Number.isFinite(node.ratio) &&
      node.ratio > 0 &&
      node.ratio < 1 &&
      isStructurallyValid(node.first) &&
      isStructurallyValid(node.second)
    );
  }
  return false;
}

/**
 * Clamp a split ratio into the allowed 0.18–0.82 band. Non-finite input
 * (NaN from a missing persisted ratio, division by a zero-sized bound)
 * resets to 0.5 so `ratio + delta` arithmetic can never poison the tree.
 *
 * @param {number} value candidate ratio
 * @returns {number}
 */
export function clampRatio(value) {
  if (!Number.isFinite(value)) return 0.5;
  return Math.min(0.82, Math.max(0.18, value));
}

/**
 * Check whether a layout tree's leaf ids exactly match a set of panes.
 * Structurally invalid trees (see isValidLayoutNode) never match, so a
 * malformed persisted layout falls back to a fresh one instead of rendering.
 * @param layout layout tree (or null)
 * @param panes array of pane objects with `.id`
 */
export function layoutMatchesPanes(layout, panes) {
  if (!isValidLayoutNode(layout)) return false;

  const layoutIds = new Set(layoutLeafIds(layout));
  if (layoutIds.size !== panes.length) return false;
  return panes.every((pane) => layoutIds.has(pane.id));
}

/**
 * Build a default row-split layout for a set of panes, with the active pane first.
 * Returns null for an empty pane set.
 */
export function defaultLayoutForPanes(panes, activePaneId) {
  if (panes.length === 0) return null;

  const ordered = [...panes].sort((a, b) => {
    if (a.id === activePaneId) return -1;
    if (b.id === activePaneId) return 1;
    return 0;
  });
  return ordered.slice(1).reduce(
    (layout, pane) => split("row", layout, leaf(pane.id)),
    leaf(ordered[0].id),
  );
}

/**
 * Return a new tree with the leaf ids `idA` and `idB` exchanged. Leaves hold a
 * pane id and views re-associate by `paneId` on render, so swapping leaf ids is
 * sufficient to trade two panes' positions while each pane keeps its session.
 *
 * The tree structure (split directions, ratios, and other leaf ids) is
 * preserved exactly; only the two targeted leaves trade ids. When `idA` and
 * `idB` refer to the same leaf (or one is absent) the tree is returned
 * unchanged (referentially equal when nothing was swapped).
 *
 * @param {object|null} node layout tree
 * @param {string} idA first leaf id
 * @param {string} idB second leaf id
 * @returns {object|null} a new tree with the two leaf ids swapped
 */
export function swapLeaves(node, idA, idB) {
  if (!node || idA === idB) return node;
  return swapLeavesRec(node, idA, idB);
}

function swapLeavesRec(node, idA, idB) {
  if (node.type === "leaf") {
    if (node.id === idA) return { ...node, id: idB };
    if (node.id === idB) return { ...node, id: idA };
    return node;
  }
  const first = swapLeavesRec(node.first, idA, idB);
  const second = swapLeavesRec(node.second, idA, idB);
  if (first === node.first && second === node.second) return node;
  return { ...node, first, second };
}

/**
 * Fold panes that the GUI didn't place (e.g. created via `ctl new`) into the
 * layout, and drop leaves whose pane is gone. Returns { layout, changed, newActiveId }.
 *
 * @param layout current layout tree
 * @param paneIds iterable of current pane ids
 * @param activePaneId current active pane id
 * @returns {{ layout: object|null, changed: boolean, newActiveId: string|null }}
 */
export function reconcileLayout(layout, paneIds, activePaneId) {
  const leafIds = new Set(layoutLeafIds(layout));
  const paneIdSet = new Set(paneIds);
  let newLayout = layout;
  let changed = false;

  for (const paneId of paneIdSet) {
    if (!leafIds.has(paneId)) {
      newLayout = newLayout
        ? split("row", newLayout, leaf(paneId))
        : leaf(paneId);
      changed = true;
    }
  }
  for (const leafId of leafIds) {
    if (!paneIdSet.has(leafId)) {
      newLayout = pruneLeaf(newLayout, leafId);
      changed = true;
    }
  }

  let newActiveId = null;
  if (changed && !paneIdSet.has(activePaneId)) {
    newActiveId = firstPaneId(newLayout);
  }

  return { layout: newLayout, changed, newActiveId };
}
