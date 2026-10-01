// clipboard.js — copy/paste logic for the active pane's terminal.
//
// Extracted so the key→action routing and clipboard side effects can be
// unit-tested without DOM or xterm. The module operates on the shared `state`
// object (activePaneId + terminalViews) and the navigator.clipboard API.
//
// Copy: xterm selection → navigator.clipboard.writeText.
// Paste: navigator.clipboard.readText → terminal.paste.
// Bare Ctrl+C / Ctrl+V (no Shift) are NOT handled here — they pass through to
// the PTY as SIGINT / literal input (see keyboard.js guard requiring Shift).
// Clipboard read/write failures are handled gracefully (no throw, returns
// false). Copy and paste act on the current active pane and retarget on active
// change because they read `state.activePaneId` at call time.

/**
 * Returns the active pane's terminal view, or null when none is available.
 *
 * @param {object} state shared app state (activePaneId + terminalViews)
 * @returns {object|null}
 */
export function activeTerminalView(state) {
  return state.terminalViews.get(state.activePaneId) ?? null;
}

/**
 * Synchronously report whether the active pane's terminal has a non-empty
 * selection. Used by the keydown wiring to decide whether mod+Shift+C should
 * consume the keystroke (only when there is a selection) or pass it through.
 *
 * @param {object} state shared app state
 * @returns {boolean}
 */
export function activeHasSelection(state) {
  const view = activeTerminalView(state);
  if (!view?.terminal) return false;
  try {
    return !!view.terminal.hasSelection();
  } catch {
    return false;
  }
}

/**
 * Copy the active pane's xterm selection to the system clipboard.
 *
 * A safe no-op (returns false) when there is no active view, no selection, an
 * empty selection, no clipboard API, or the clipboard write fails. Never
 * throws.
 *
 * @param {object} state shared app state
 * @param {object} [clipboard] navigator.clipboard (defaults to global)
 * @returns {Promise<boolean>} true if the selection was written to the clipboard
 */
export async function copySelection(
  state,
  clipboard = globalThis.navigator?.clipboard,
) {
  const view = activeTerminalView(state);
  if (!view?.terminal) return false;
  if (!activeHasSelection(state)) return false;
  let text = "";
  try {
    text = view.terminal.getSelection() ?? "";
  } catch {
    return false;
  }
  if (!text) return false;
  if (!clipboard?.writeText) return false;
  try {
    await clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/**
 * Paste clipboard text into the active pane's terminal.
 *
 * Reads the system clipboard and passes the resulting text to the active
 * terminal's `paste()`. A safe no-op (returns false) when there is no active
 * view, no clipboard API, the clipboard read fails, or the clipboard is empty.
 * Never throws.
 *
 * @param {object} state shared app state
 * @param {object} [clipboard] navigator.clipboard (defaults to global)
 * @returns {Promise<boolean>} true if text was pasted into the active terminal
 */
export async function pasteClipboard(
  state,
  clipboard = globalThis.navigator?.clipboard,
) {
  const view = activeTerminalView(state);
  if (!view?.terminal) return false;
  if (!clipboard?.readText) return false;
  let text = "";
  try {
    text = await clipboard.readText();
  } catch {
    return false;
  }
  if (!text) return false;
  try {
    view.terminal.paste(text);
    return true;
  } catch {
    return false;
  }
}
