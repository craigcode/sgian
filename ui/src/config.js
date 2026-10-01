// config.js — terminal appearance defaults and config-to-appearance mapping.
//
// Keeps the font/theme constants and the pure mapping from a backend config
// object to the `appearance` state used at terminal-creation time.

// Leave xterm one visual guard column so full-width TUIs don't clip at the WebView edge.
// (The Windows TUI stagger was root-caused to the app/ConPTY stream itself, not
// to display-side sizing — no guard width can prevent an app drawing into the
// buffer's last column, so there is deliberately no Windows special case here.)
export const TERMINAL_RIGHT_EDGE_GUARD_COLS = 1;

export function rightEdgeGuardCols() {
  return TERMINAL_RIGHT_EDGE_GUARD_COLS;
}

export const MAX_TERMINAL_ROWS = 2000;

export const DEFAULT_TERMINAL_FONT = 'Menlo, Monaco, "SFMono-Regular", Consolas, monospace';
export const DEFAULT_TERMINAL_FONT_SIZE = 12;

export const DEFAULT_TERMINAL_THEME = {
  background: "#0e0f0c",
  black: "#11120f",
  blue: "#65a7ff",
  brightBlack: "#696c50",
  brightBlue: "#8fbfff",
  brightCyan: "#9ce6d4",
  brightGreen: "#99e69b",
  brightMagenta: "#d9a4ff",
  brightRed: "#ff918f",
  brightWhite: "#ffffff",
  brightYellow: "#ffd77c",
  cursor: "#f0be5a",
  cursorAccent: "#10110f",
  cyan: "#72d6c2",
  foreground: "#e7ecd8",
  green: "#79d27c",
  magenta: "#c687f0",
  red: "#ef6f6c",
  selectionBackground: "#37533a",
  scrollbarSliderActiveBackground: "rgba(0, 0, 0, 0)",
  scrollbarSliderBackground: "rgba(0, 0, 0, 0)",
  scrollbarSliderHoverBackground: "rgba(0, 0, 0, 0)",
  white: "#f1f2df",
  yellow: "#f0be5a",
};

/**
 * Map a backend config object (from `get_config`) to an `appearance` state object.
 * Returns a null-padded appearance when config is missing.
 *
 * @param {{ font_family?: string, font_size?: number, theme?: object } | null} config
 * @returns {{ fontFamily: string|null, fontSize: number|null, theme: object|null }}
 */
export function mergeAppearance(config) {
  if (!config) return { fontFamily: null, fontSize: null, theme: null };
  return {
    fontFamily: config.font_family || null,
    fontSize: config.font_size || null,
    theme: config.theme || null,
  };
}

/**
 * Build the xterm Terminal options.theme value, merging defaults with any
 * configured theme overrides.
 */
export function resolveTheme(appearance) {
  return appearance?.theme
    ? { ...DEFAULT_TERMINAL_THEME, ...appearance.theme }
    : DEFAULT_TERMINAL_THEME;
}

/**
 * Build the xterm Terminal font-family option value.
 */
export function resolveFontFamily(appearance) {
  return appearance?.fontFamily || DEFAULT_TERMINAL_FONT;
}

/**
 * Build the xterm Terminal font-size option value.
 */
export function resolveFontSize(appearance) {
  return appearance?.fontSize || DEFAULT_TERMINAL_FONT_SIZE;
}

/**
 * Live re-apply appearance to an existing xterm terminal instance.
 * Sets options.theme, fontFamily, and fontSize on the terminal without
 * recreating it. The caller should trigger a refit afterwards.
 *
 * @param {object} terminal xterm Terminal instance
 * @param {{ fontFamily: string|null, fontSize: number|null, theme: object|null }} appearance
 */
export function applyAppearanceToTerminal(terminal, appearance) {
  if (!terminal || !appearance) return;
  terminal.options.theme = resolveTheme(appearance);
  terminal.options.fontFamily = resolveFontFamily(appearance);
  terminal.options.fontSize = resolveFontSize(appearance);
}

/**
 * Apply the resolved theme's background color to the .xterm-screen DOM element.
 *
 * The xterm DOM renderer (used in headless Chromium / happy-dom) does not always
 * apply the theme background to .xterm-screen on option changes or even at
 * construction time. The canvas renderer (real WKWebView) handles this correctly,
 * so this is a safe no-op there (same color applied twice).
 *
 * @param {object} terminal xterm Terminal instance (must have .element after open())
 * @param {{ fontFamily: string|null, fontSize: number|null, theme: object|null }} appearance
 */
export function applyScreenBackground(terminal, appearance) {
  if (!terminal || !appearance) return;
  const theme = resolveTheme(appearance);
  try {
    const screenEl = terminal.element?.querySelector?.(".xterm-screen");
    if (screenEl && theme.background) {
      screenEl.style.backgroundColor = theme.background;
    }
  } catch {
    // DOM access is best-effort
  }
}

/**
 * Live re-apply appearance to ALL existing terminal views and refit each one.
 *
 * Iterates the given terminal views, sets the new theme/fontFamily/fontSize on
 * each terminal's options (via `applyAppearanceToTerminal`), and triggers a
 * refit (via `view.fitAddon.fit()`) for each view that has a fit addon. Terminals
 * are NOT disposed, reset, cleared, or recreated — in-flight output is preserved.
 *
 * After setting options, `terminal.refresh(0, rows-1)` is called to force the
 * xterm renderer to repaint with the new theme colors. Additionally, the
 * `.xterm-screen` element's background-color is set directly because the xterm
 * DOM renderer does not re-apply it on a bare option change (the canvas renderer
 * used in a real WKWebView handles this correctly, but the DOM renderer used in
 * the headless test harness does not).
 *
 * @param {Iterable<{ terminal: object, fitAddon?: object }>} views terminal views
 * @param {{ fontFamily: string|null, fontSize: number|null, theme: object|null }} appearance
 * @returns {number} the number of views re-applied
 */
export function liveReapplyAppearance(views, appearance) {
  if (!views || !appearance) return 0;
  const theme = resolveTheme(appearance);
  let count = 0;
  for (const view of views) {
    if (!view?.terminal) continue;
    applyAppearanceToTerminal(view.terminal, appearance);
    // Force the renderer to repaint with the new theme colors. The DOM renderer
    // does not always re-apply the .xterm-screen background on a bare option
    // change, so an explicit refresh is needed for a full recolor.
    try {
      view.terminal.refresh?.(0, (view.terminal.rows || 1) - 1);
    } catch {
      // refresh is best-effort
    }
    // The xterm DOM renderer (used in headless Chromium) does not update the
    // .xterm-screen background-color on a theme option change. Set it directly
    // from the resolved theme so the recolor is visible. The canvas renderer
    // (real WKWebView) handles this via the theme service, so this is a safe
    // no-op there (same color applied twice).
    applyScreenBackground(view.terminal, appearance);
    if (view.fitAddon) {
      try {
        view.fitAddon.fit();
      } catch {
        // refit is best-effort; a failed fit must not break other views
      }
    }
    count++;
  }
  return count;
}

export function ptyColsForTerminal(cols) {
  return Math.max(2, cols - rightEdgeGuardCols());
}
