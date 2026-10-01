// search.js — scrollback search control logic.
//
// Extracted so the key→action routing and search navigation can be unit-tested
// without DOM or xterm. The module exposes pure functions that operate on a
// shared `state` object (with `activePaneId` + `terminalViews`) and a mutable
// search-bar state object (`createSearchState`).
//
// The actual xterm search is performed by the vendored @xterm/addon-search,
// loaded per terminal in createTerminalView. Each terminal view stores its
// `searchAddon` on `view.searchAddon`. The addon's findNext/findPrevious wrap
// around the match set natively, so repeated calls cycle through all matches.

/**
 * Returns the SearchAddon class from the global UMD bundle, or null when the
 * addon script has not been loaded (e.g. in vitest/happy-dom).
 */
export function searchAddonFactory() {
  return globalThis.SearchAddon?.SearchAddon ?? null;
}

/**
 * Resolve a search-bar keydown into an action descriptor.
 *
 * Used by the search input's keydown handler (only active while the bar is
 * open). Returns null for unbound keys so they pass through to the input.
 *
 * @param {KeyboardEvent} event
 * @returns {{ type: string } | null}
 *   - { type: 'search-next' }      (Enter)
 *   - { type: 'search-previous' }  (Shift+Enter)
 *   - { type: 'search-close' }     (Escape)
 *   - null when the key is unbound
 */
export function resolveSearchKeyAction(event) {
  if (event.key === "Escape") {
    return { type: "search-close" };
  }
  if (event.key === "Enter") {
    return event.shiftKey
      ? { type: "search-previous" }
      : { type: "search-next" };
  }
  return null;
}

/**
 * Get the search addon for the currently active pane's terminal.
 *
 * @param {object} state shared app state (activePaneId + terminalViews)
 * @returns {object|null} the search addon, or null when none is available
 */
export function activeSearchAddon(state) {
  const view = state.terminalViews.get(state.activePaneId);
  return view?.searchAddon ?? null;
}

/**
 * Run findNext on the active pane's search addon with the current query.
 *
 * A safe no-op when the query is empty, no addon is available, or the addon
 * throws. The addon wraps around the match set natively, so repeated calls
 * cycle through all matches.
 *
 * @param {object} state shared app state
 * @param {string} query the search term
 * @param {object} [options] search options forwarded to the addon
 * @returns {boolean} true if a match was found, false otherwise
 */
export function findNext(state, query, options = {}) {
  if (!query) return false;
  const addon = activeSearchAddon(state);
  if (!addon) return false;
  try {
    return !!addon.findNext(query, options);
  } catch {
    return false;
  }
}

/**
 * Run findPrevious on the active pane's search addon with the current query.
 *
 * A safe no-op when the query is empty, no addon is available, or the addon
 * throws. The addon wraps around the match set natively.
 *
 * @param {object} state shared app state
 * @param {string} query the search term
 * @param {object} [options] search options forwarded to the addon
 * @returns {boolean} true if a match was found, false otherwise
 */
export function findPrevious(state, query, options = {}) {
  if (!query) return false;
  const addon = activeSearchAddon(state);
  if (!addon) return false;
  try {
    return !!addon.findPrevious(query, options);
  } catch {
    return false;
  }
}

/**
 * Create a fresh mutable search-bar state object.
 *
 * @returns {{ open: boolean, query: string }}
 */
export function createSearchState() {
  return { open: false, query: "" };
}

/**
 * Mark the search bar as open.
 *
 * @param {{ open: boolean, query: string }} search
 */
export function openSearch(search) {
  search.open = true;
}

/**
 * Mark the search bar as closed and clear the current query.
 *
 * @param {{ open: boolean, query: string }} search
 */
export function closeSearch(search) {
  search.open = false;
  search.query = "";
}
