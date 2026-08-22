// focus.js — focus resolution: linear cycle + directional (geometric) navigation.
//
// The linear cycle uses the in-order leaf list from the layout tree.
// Directional focus resolves the nearest neighbor via DOM getBoundingClientRect,
// stubbed in tests.

import { layoutLeafIds } from "./layout.js";

/**
 * Resolve a pane id by visible tab index (0-based).
 *
 * The index maps to the visible tab numbering: tab N (1-based) corresponds to
 * index N-1. Out-of-range indices return null (no-op).
 *
 * @param {string[]} ids ordered pane ids (same order as renderTabs)
 * @param {number} index 0-based index
 * @returns {string|null} the pane id at that index, or null if out of range
 */
export function resolveFocusIndex(ids, index) {
  if (!Array.isArray(ids) || index < 0 || index >= ids.length) return null;
  return ids[index];
}

/**
 * Resolve the index in the leaf list for an adjacent-pane focus move.
 *
 * @param {string[]} leafIds in-order leaf ids
 * @param {string} currentId active pane id
 * @param {number} offset +1 or -1
 * @returns {number} index of the next pane, or -1 if there's nowhere to go
 */
export function resolveAdjacentIndex(leafIds, currentId, offset) {
  if (leafIds.length <= 1) return -1;
  const current = leafIds.indexOf(currentId);
  const base = current === -1 ? 0 : current;
  return (base + offset + leafIds.length) % leafIds.length;
}

/**
 * Resolve the nearest pane in a cardinal direction using on-screen geometry.
 *
 * @param {Array<{ id: string, rect: { left: number, top: number, right: number, bottom: number } }>} paneRects
 * @param {string} activeId
 * @param {'left'|'right'|'up'|'down'} direction
 * @returns {string|null} the nearest pane id, or null if none in that direction
 */
export function resolveDirectionalFocus(paneRects, activeId, direction) {
  const active = paneRects.find((p) => p.id === activeId);
  if (!active) return null;

  const activeRect = active.rect;
  const activeCenterX = (activeRect.left + activeRect.right) / 2;
  const activeCenterY = (activeRect.top + activeRect.bottom) / 2;

  // Filter to panes that lie in the requested direction relative to the active pane.
  let candidates = paneRects.filter((p) => p.id !== activeId);

  if (direction === "right") {
    candidates = candidates.filter((p) => p.rect.left >= activeRect.right - 1);
  } else if (direction === "left") {
    candidates = candidates.filter((p) => p.rect.right <= activeRect.left + 1);
  } else if (direction === "down") {
    candidates = candidates.filter((p) => p.rect.top >= activeRect.bottom - 1);
  } else if (direction === "up") {
    candidates = candidates.filter((p) => p.rect.bottom <= activeRect.top + 1);
  }

  if (candidates.length === 0) return null;

  // Score each candidate by perpendicular distance, then by overlap, then by
  // parallel distance. The nearest pane is the one with the smallest score.
  let best = null;
  let bestScore = Infinity;

  for (const cand of candidates) {
    const candRect = cand.rect;
    const candCenterX = (candRect.left + candRect.right) / 2;
    const candCenterY = (candRect.top + candRect.bottom) / 2;

    let primaryDist; // distance along the direction axis
    let perpDist; // distance perpendicular to the direction axis
    let overlap; // overlap along the perpendicular axis

    if (direction === "right") {
      primaryDist = candRect.left - activeRect.right;
      perpDist = Math.abs(candCenterY - activeCenterY);
      overlap = Math.min(activeRect.bottom, candRect.bottom) - Math.max(activeRect.top, candRect.top);
    } else if (direction === "left") {
      primaryDist = activeRect.left - candRect.right;
      perpDist = Math.abs(candCenterY - activeCenterY);
      overlap = Math.min(activeRect.bottom, candRect.bottom) - Math.max(activeRect.top, candRect.top);
    } else if (direction === "down") {
      primaryDist = candRect.top - activeRect.bottom;
      perpDist = Math.abs(candCenterX - activeCenterX);
      overlap = Math.min(activeRect.right, candRect.right) - Math.max(activeRect.left, candRect.left);
    } else {
      // up
      primaryDist = activeRect.top - candRect.bottom;
      perpDist = Math.abs(candCenterX - activeCenterX);
      overlap = Math.min(activeRect.right, candRect.right) - Math.max(activeRect.left, candRect.left);
    }

    // Penalize perpendicular distance heavily; prefer aligned panes.
    // Overlap reduces the effective perpendicular distance.
    const effectivePerp = overlap > 0 ? 0 : perpDist;
    const score = effectivePerp * 1000 + primaryDist;

    // Tie-break: prefer topmost then leftmost.
    const tieBreak = candRect.top * 10000 + candRect.left;

    if (score < bestScore || (score === bestScore && tieBreak < (best?.tieBreak ?? Infinity))) {
      best = { id: cand.id, score, tieBreak };
      bestScore = score;
    }
  }

  return best?.id ?? null;
}
