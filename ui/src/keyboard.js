// keyboard.js — pure key→action mapping for the workbench keyboard handler.
//
// The mapping is extracted so it can be unit-tested without DOM or Tauri.
// `resolveKeyAction` takes a keydown-event-like object and returns an action
// descriptor (or null when the key is unbound).
//
// Keymap (mod = Ctrl | Cmd):
//   mod+1..9        → focus-index (focus pane under visible tab N)
//   mod+Shift+\|    → split row
//   mod+Shift+_     → split column
//   mod+Shift+W     → close
//   mod+Shift+R     → rename
//   mod+Shift+F     → search-open
//   mod+Shift+C     → copy (only when selection exists; otherwise passes through)
//   mod+Shift+V     → paste
//   mod+Shift+Z     → zoom-toggle
//   mod+Shift+S     → swap
//   mod+Shift+N     → new-agent (create an agent pane in a new split)
//   mod+Shift+P     → command-palette
//   mod+K           → command-palette (without Shift)
//   mod+Shift+Arrow → focus-directional (geometric nearest neighbor)
//
// Bare Ctrl+C / Ctrl+V (no Shift) are NOT copy/paste — they pass through to
// the PTY (Ctrl+C is SIGINT). Ordinary typing (no mod), bare arrow keys, and
// unbound mod+Shift chords are not intercepted (return null).

/**
 * Resolve a keyboard event into an action descriptor.
 *
 * @param {KeyboardEvent} event
 * @returns {{ type: string, direction?: string, offset?: number, index?: number } | null}
 *   - { type: 'focus-index', index: number }     (mod+1..9)
 *   - { type: 'focus-directional', direction: 'left'|'right'|'up'|'down' }
 *   - { type: 'split', direction: 'row' | 'column' }
 *   - { type: 'close' }
 *   - { type: 'rename' }
 *   - { type: 'search-open' }
 *   - { type: 'copy' }       (mod+Shift+C)
 *   - { type: 'paste' }      (mod+Shift+V)
 *   - { type: 'zoom-toggle' } (mod+Shift+Z)
 *   - { type: 'swap' }        (mod+Shift+S)
 *   - { type: 'new-agent' }   (mod+Shift+N)
 *   - { type: 'command-palette' } (mod+Shift+P, mod+K)
 *   - null when the key is unbound (should pass through to the terminal)
 */
export function resolveKeyAction(event) {
  const mod = event.ctrlKey || event.metaKey;
  if (!mod) return null;

  // mod+1..9 (no Shift) → focus by index. Checked before the Shift guard
  // since these chords don't use Shift. mod+0 is NOT a focus-index action
  // (visible tabs are numbered 1..9).
  if (!event.shiftKey && /^[1-9]$/.test(event.key)) {
    return { type: "focus-index", index: Number(event.key) - 1 };
  }

  if (!event.shiftKey) {
    if (event.key.toLowerCase() === "k") {
      return { type: "command-palette" };
    }
    return null;
  }

  if (event.key.toLowerCase() === "p") {
    return { type: "command-palette" };
  }

  // With Shift held, "\" reports as "|" on most layouts.
  if (event.key === "\\" || event.key === "|") {
    return { type: "split", direction: "row" };
  }
  if (event.key === "_") {
    return { type: "split", direction: "column" };
  }
  if (event.key.toLowerCase() === "w") {
    return { type: "close" };
  }
  if (event.key.toLowerCase() === "r") {
    return { type: "rename" };
  }
  if (event.key.toLowerCase() === "f") {
    return { type: "search-open" };
  }
  if (event.key.toLowerCase() === "c") {
    return { type: "copy" };
  }
  if (event.key.toLowerCase() === "v") {
    return { type: "paste" };
  }
  if (event.key.toLowerCase() === "z") {
    return { type: "zoom-toggle" };
  }
  if (event.key.toLowerCase() === "s") {
    return { type: "swap" };
  }
  if (event.key.toLowerCase() === "n") {
    return { type: "new-agent" };
  }
  // Directional focus uses on-screen geometry (DOM getBoundingClientRect
  // nearest-neighbor), NOT the linear leaf cycle. This replaces the prior
  // linear focus-adjacent mapping for arrow keys.
  if (event.key === "ArrowRight") {
    return { type: "focus-directional", direction: "right" };
  }
  if (event.key === "ArrowLeft") {
    return { type: "focus-directional", direction: "left" };
  }
  if (event.key === "ArrowDown") {
    return { type: "focus-directional", direction: "down" };
  }
  if (event.key === "ArrowUp") {
    return { type: "focus-directional", direction: "up" };
  }

  return null;
}
