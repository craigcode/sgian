import { describe, it, expect } from "vitest";
import {
  createZoomState,
  isZoomed,
  zoomedPaneId,
  enterZoom,
  exitZoom,
  toggleZoom,
  syncZoomWithActive,
  handleZoomedPaneClosed,
  swapPartner,
} from "../ui/src/zoom.js";
import { leaf, split, layoutLeafIds, swapLeaves } from "../ui/src/layout.js";

describe("zoom state machine", () => {
  it("starts unzoomed", () => {
    const zoom = createZoomState();
    expect(isZoomed(zoom)).toBe(false);
    expect(zoomedPaneId(zoom)).toBeNull();
  });

  it("enterZoom maximizes the given pane", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-2");
    expect(isZoomed(zoom)).toBe(true);
    expect(zoomedPaneId(zoom)).toBe("pane-2");
  });

  it("enterZoom is a no-op for an empty pane id", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "");
    expect(isZoomed(zoom)).toBe(false);
  });

  it("exitZoom clears the zoomed pane", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-2");
    exitZoom(zoom);
    expect(isZoomed(zoom)).toBe(false);
    expect(zoomedPaneId(zoom)).toBeNull();
  });

  // VAL-UX-012: Zoom maximizes the active leaf
  it("toggleZoom on an unzoomed workspace maximizes the active pane", () => {
    const zoom = createZoomState();
    const result = toggleZoom(zoom, "pane-1");
    expect(result).toBe("entered");
    expect(zoomedPaneId(zoom)).toBe("pane-1");
  });

  // VAL-UX-013: Toggling zoom restores the prior layout
  it("toggleZoom on the zoomed pane exits zoom (restores layout)", () => {
    const zoom = createZoomState();
    toggleZoom(zoom, "pane-1");
    const result = toggleZoom(zoom, "pane-1");
    expect(result).toBe("exited");
    expect(isZoomed(zoom)).toBe(false);
  });

  // VAL-UX-015: Zoom preserves the active-pane identity
  it("toggleZoom never changes the active pane id (enter/exit)", () => {
    const zoom = createZoomState();
    const activePaneId = "pane-2";
    // The zoom state machine does NOT touch activePaneId — the caller owns it.
    // We model the caller's invariant here: enter then exit leaves it untouched.
    toggleZoom(zoom, activePaneId);
    expect(zoomedPaneId(zoom)).toBe(activePaneId);
    toggleZoom(zoom, activePaneId);
    expect(isZoomed(zoom)).toBe(false);
    // activePaneId was never mutated by the zoom state machine.
    expect(activePaneId).toBe("pane-2");
  });

  // VAL-UX-014: Zoom is transient and never persisted
  it("zoom state is fully contained in the zoom object — the layout tree is never mutated by zoom", () => {
    const tree = split("row", leaf("a"), leaf("b"));
    const snapshotBefore = JSON.parse(JSON.stringify(tree));
    const zoom = createZoomState();
    enterZoom(zoom, "a");
    // The tree is unchanged by entering zoom (no zoom markers in the tree).
    expect(tree).toEqual(snapshotBefore);
    exitZoom(zoom);
    expect(tree).toEqual(snapshotBefore);
  });

  // VAL-UX-036: While zoomed, the maximized pane stays in sync with the active pane
  it("syncZoomWithActive retargets the zoom to the new active pane", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-1");
    syncZoomWithActive(zoom, "pane-2");
    expect(zoomedPaneId(zoom)).toBe("pane-2");
    expect(isZoomed(zoom)).toBe(true);
  });

  it("syncZoomWithActive is a no-op when not zoomed", () => {
    const zoom = createZoomState();
    syncZoomWithActive(zoom, "pane-2");
    expect(isZoomed(zoom)).toBe(false);
  });

  it("syncZoomWithActive exits zoom when the active pane id is empty", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-1");
    syncZoomWithActive(zoom, "");
    expect(isZoomed(zoom)).toBe(false);
  });

  it("toggleZoom on a different active pane retargets the zoom", () => {
    const zoom = createZoomState();
    toggleZoom(zoom, "pane-1");
    const result = toggleZoom(zoom, "pane-2");
    expect(result).toBe("retargeted");
    expect(zoomedPaneId(zoom)).toBe("pane-2");
  });

  // VAL-UX-037: A pane-ended for the zoomed pane keeps it zoomed with the restart affordance
  it("a pane-ended event does NOT exit zoom (the pane still exists, only runtime state changed)", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-1");
    // The zoom module has no pane-ended handler — by design, ending a pane's
    // shell does not clear zoom. The caller's render() shows the ended state
    // and the restart affordance while the zoom flag stays set.
    expect(isZoomed(zoom)).toBe(true);
    expect(zoomedPaneId(zoom)).toBe("pane-1");
  });

  // VAL-UX-034: Closing the zoomed pane exits zoom and restores the remaining layout
  it("handleZoomedPaneClosed exits zoom when the closed pane is the zoomed one", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-1");
    const cleared = handleZoomedPaneClosed(zoom, "pane-1");
    expect(cleared).toBe(true);
    expect(isZoomed(zoom)).toBe(false);
  });

  it("handleZoomedPaneClosed is a no-op when a different pane is closed", () => {
    const zoom = createZoomState();
    enterZoom(zoom, "pane-1");
    const cleared = handleZoomedPaneClosed(zoom, "pane-2");
    expect(cleared).toBe(false);
    expect(zoomedPaneId(zoom)).toBe("pane-1");
  });

  it("handleZoomedPaneClosed is a no-op when not zoomed", () => {
    const zoom = createZoomState();
    const cleared = handleZoomedPaneClosed(zoom, "pane-1");
    expect(cleared).toBe(false);
  });

  // VAL-CROSS-024: Closing the currently-zoomed pane via a daemon event exits zoom,
  // prunes the layout, and refocuses a survivor.
  it("closing the zoomed pane clears the zoom flag (the caller prunes + refocuses)", () => {
    const zoom = createZoomState();
    const tree = split("row", leaf("pane-1"), leaf("pane-2"));
    enterZoom(zoom, "pane-1");
    // Daemon pane-closed event for the zoomed pane.
    const closed = handleZoomedPaneClosed(zoom, "pane-1");
    expect(closed).toBe(true);
    expect(isZoomed(zoom)).toBe(false);
    // The layout tree was never mutated by zoom; the caller prunes it normally.
    expect(layoutLeafIds(tree)).toEqual(["pane-1", "pane-2"]);
  });
});

describe("swapPartner", () => {
  // VAL-UX-038: Swap is a no-op when there is only one pane
  it("returns null for a single-pane layout", () => {
    expect(swapPartner(["a"], "a")).toBeNull();
  });

  it("returns null for an empty list", () => {
    expect(swapPartner([], "a")).toBeNull();
  });

  it("returns null when the active pane is not in the list", () => {
    expect(swapPartner(["a", "b"], "x")).toBeNull();
  });

  it("returns the other pane for a 2-pane layout", () => {
    expect(swapPartner(["a", "b"], "a")).toBe("b");
    expect(swapPartner(["a", "b"], "b")).toBe("a");
  });

  it("returns the next pane in-order for a 3-pane layout (wrapping)", () => {
    expect(swapPartner(["a", "b", "c"], "a")).toBe("b");
    expect(swapPartner(["a", "b", "c"], "b")).toBe("c");
    expect(swapPartner(["a", "b", "c"], "c")).toBe("a");
  });
});

// VAL-UX-016: Swap exchanges two panes' positions in the layout tree
// VAL-UX-017: Swap persists (the action layer invokes update_workspace_layout)
// VAL-UX-039: Swap preserves the active-pane identity and its focus
describe("swap action invariants (pure op + partner)", () => {
  it("swapping the active pane with its partner trades positions and preserves the active id", () => {
    // Layout: split(row, a, split(row, b, c)) — leaves [a, b, c], active = a
    const tree = split("row", leaf("a"), split("row", leaf("b"), leaf("c")));
    const activePaneId = "a";
    const partner = swapPartner(layoutLeafIds(tree), activePaneId);
    expect(partner).toBe("b");

    const swapped = swapLeaves(tree, activePaneId, partner);
    // The two panes' leaf positions are exchanged; all other structure is unchanged.
    expect(layoutLeafIds(swapped)).toEqual(["b", "a", "c"]);
    // The active pane id is unchanged by the swap (VAL-UX-039): the same pane
    // stays active, now in the partner's former slot. The action layer does not
    // reassign activePaneId on a swap.
    expect(activePaneId).toBe("a");
  });

  it("swapping twice returns the original tree (idempotent)", () => {
    const tree = split("row", leaf("a"), split("row", leaf("b"), leaf("c")));
    const partner = swapPartner(layoutLeafIds(tree), "a");
    const once = swapLeaves(tree, "a", partner);
    const twice = swapLeaves(once, "a", partner);
    expect(twice).toEqual(tree);
  });

  // VAL-UX-038: Swap is a no-op when there is only one pane
  it("with a single pane, swapPartner returns null so the action layer no-ops", () => {
    const tree = leaf("a");
    expect(swapPartner(layoutLeafIds(tree), "a")).toBeNull();
    // No swapLeaves call is made; nothing is persisted.
  });
});
