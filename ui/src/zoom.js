// zoom.js — transient pane-zoom state machine.
//
// Zoom is a TRANSIENT maximize of a single leaf: it is NEVER persisted to the
// layout tree. The zoom state is carried by a small mutable object (`createZoomState`)
// and surfaced in the DOM as `#layout-root[data-zoom="<paneId>"]`. All functions
// here are pure (no DOM, no Tauri) so they can be unit-tested directly.
//
// Invariants:
//   - Zoom preserves the active-pane identity (VAL-UX-015): toggling zoom on and
//     off does not change `state.activePaneId`.
//   - While zoomed, the maximized pane stays in sync with the active pane
//     (VAL-UX-036): a focus change while zoomed retargets the zoom to the newly
//     active pane.
//   - Closing the zoomed pane exits zoom and restores the remaining layout
//     (VAL-UX-034 / VAL-CROSS-024).
//   - A `pane-ended` event for the zoomed pane keeps it zoomed with the restart
//     affordance (VAL-UX-037) — the pane still exists, only its runtime state
//     changed.
//   - Creating a split while zoomed resolves to a consistent visible state
//     (VAL-UX-035): the zoom is cleared so the new split is visible (and the
//     newly created pane becomes the active surface).

/**
 * Create a fresh mutable zoom-state object.
 *
 * @returns {{ paneId: string|null }}
 */
export function createZoomState() {
  return { paneId: null };
}

/**
 * True when a pane is currently zoomed.
 *
 * @param {{ paneId: string|null }} zoom
 * @returns {boolean}
 */
export function isZoomed(zoom) {
  return zoom.paneId != null;
}

/**
 * The id of the currently zoomed pane, or null.
 *
 * @param {{ paneId: string|null }} zoom
 * @returns {string|null}
 */
export function zoomedPaneId(zoom) {
  return zoom.paneId;
}

/**
 * Enter zoom on the given pane id. The caller passes the active pane id so the
 * zoomed pane and the active pane start in sync.
 *
 * @param {{ paneId: string|null }} zoom
 * @param {string} paneId
 */
export function enterZoom(zoom, paneId) {
  if (!paneId) return;
  zoom.paneId = paneId;
}

/**
 * Exit zoom (clear the zoomed pane). The layout tree is restored by the caller
 * via a normal re-render — zoom never mutated the tree.
 *
 * @param {{ paneId: string|null }} zoom
 */
export function exitZoom(zoom) {
  zoom.paneId = null;
}

/**
 * Toggle zoom on the given pane id (the active pane).
 *
 * - When not zoomed: enter zoom on the pane.
 * - When zoomed on the same pane: exit zoom (restore the prior layout).
 * - When zoomed on a different pane: retarget the zoom to the new pane
 *   (this keeps the zoomed pane in sync with the active pane, VAL-UX-036).
 *
 * Returns "entered" | "exited" | "retargeted" so the caller knows whether to
 * persist (never for zoom) or just re-render.
 *
 * @param {{ paneId: string|null }} zoom
 * @param {string} paneId active pane id
 * @returns {"entered"|"exited"|"retargeted"}
 */
export function toggleZoom(zoom, paneId) {
  if (!isZoomed(zoom)) {
    enterZoom(zoom, paneId);
    return "entered";
  }
  if (zoom.paneId === paneId) {
    exitZoom(zoom);
    return "exited";
  }
  // Zoomed on a different pane: retarget to the new active pane so the
  // maximized pane stays in sync with the active pane (VAL-UX-036).
  zoom.paneId = paneId;
  return "retargeted";
}

/**
 * Keep the zoomed pane in sync with the active pane after a focus change.
 *
 * While zoomed, the maximized pane must equal the active pane (VAL-UX-036).
 * When the active pane changes, the zoom retargets to the new active pane.
 * When not zoomed, this is a no-op.
 *
 * @param {{ paneId: string|null }} zoom
 * @param {string} activePaneId the new active pane id
 */
export function syncZoomWithActive(zoom, activePaneId) {
  if (!isZoomed(zoom)) return;
  if (!activePaneId) {
    exitZoom(zoom);
    return;
  }
  zoom.paneId = activePaneId;
}

/**
 * Handle a pane-closed event for the zoomed pane: exit zoom so the remaining
 * layout renders normally. The caller is responsible for pruning the layout,
 * refocusing a survivor, and persisting (this only clears the transient zoom
 * flag).
 *
 * Returns true if zoom was cleared (the closed pane was the zoomed one).
 *
 * @param {{ paneId: string|null }} zoom
 * @param {string} closedPaneId
 * @returns {boolean}
 */
export function handleZoomedPaneClosed(zoom, closedPaneId) {
  if (!isZoomed(zoom)) return false;
  if (zoom.paneId !== closedPaneId) return false;
  exitZoom(zoom);
  return true;
}

/**
 * Resolve the swap partner for the active pane.
 *
 * The swap action exchanges the active pane's position with a documented
 * partner. With exactly two panes the partner is the other pane. With more than
 * two, the partner is the next pane in the in-order leaf list (wrapping around)
 * — the same neighbor the linear focus cycle visits. Returns null when there is
 * only one pane (swap is a no-op, VAL-UX-038) or when the active pane id is not
 * in the list.
 *
 * @param {string[]} leafIds in-order leaf ids
 * @param {string} activePaneId
 * @returns {string|null} the swap partner id, or null
 */
export function swapPartner(leafIds, activePaneId) {
  if (!leafIds || leafIds.length < 2) return null;
  const idx = leafIds.indexOf(activePaneId);
  if (idx === -1) return null;
  const next = (idx + 1) % leafIds.length;
  return leafIds[next];
}
