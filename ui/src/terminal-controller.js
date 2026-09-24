import {
  MAX_TERMINAL_ROWS,
  applyScreenBackground,
  liveReapplyAppearance,
  ptyColsForTerminal,
  resolveFontFamily,
  resolveFontSize,
  resolveTheme,
} from "./config.js";
import { searchAddonFactory } from "./search.js";

export const MAX_BUFFERED_OUTPUT_CHARS = 256 * 1024;
const RESIZE_RETRY_MS = 250;
const RESIZE_MAX_ATTEMPTS = 5;

export function escapeAwareSliceStart(text, target) {
  const esc = text.lastIndexOf("\x1b", target - 1);
  if (esc === -1) return target;

  let end = esc + 2;
  const kind = text[esc + 1];
  if (kind === "[") {
    end = text.length;
    for (let i = esc + 2; i < text.length; i += 1) {
      const code = text.charCodeAt(i);
      if (code >= 0x40 && code <= 0x7e) {
        end = i + 1;
        break;
      }
    }
  } else if (kind === "]") {
    end = text.length;
    for (let i = esc + 2; i < text.length; i += 1) {
      if (text[i] === "\x07") {
        end = i + 1;
        break;
      }
      if (text[i] === "\x1b" && text[i + 1] === "\\") {
        end = i + 2;
        break;
      }
    }
  }

  return esc < target && end > target ? end : target;
}

export function capBufferedOutput(text) {
  if (text.length <= MAX_BUFFERED_OUTPUT_CHARS) return text;
  const target = text.length - MAX_BUFFERED_OUTPUT_CHARS;
  const newline = text.indexOf("\n", target);
  return newline === -1
    ? text.slice(escapeAwareSliceStart(text, target))
    : text.slice(newline + 1);
}

export function createTerminalController({
  state,
  invoke,
  write,
  getAppearance,
  hasPane,
}) {
  function available() {
    return typeof globalThis.Terminal === "function";
  }

  function syncSize(paneId, cols, rows) {
    const ptyCols = ptyColsForTerminal(cols);
    const ptyRows = Math.max(1, rows);
    const key = `${ptyCols}x${ptyRows}`;
    if (state.terminalSizes.get(paneId) === key) return;

    state.terminalSizes.set(paneId, key);
    let sync = state.terminalResizeSyncs.get(paneId);
    if (!sync) {
      sync = { inFlight: false, pending: null, appliedKey: null, retryTimer: null };
      state.terminalResizeSyncs.set(paneId, sync);
    }
    sync.pending = { cols: ptyCols, rows: ptyRows, key, attempt: 0 };
    void drainResize(paneId, sync);
  }

  async function drainResize(paneId, sync) {
    if (
      sync.inFlight ||
      sync.retryTimer ||
      !sync.pending ||
      state.terminalResizeSyncs.get(paneId) !== sync
    ) {
      return;
    }

    const request = sync.pending;
    sync.pending = null;
    if (request.key === sync.appliedKey) return;

    sync.inFlight = true;
    let sendNewerImmediately = true;
    try {
      await invoke("resize_pane_terminal", {
        paneId,
        cols: request.cols,
        rows: request.rows,
      });
      sync.appliedKey = request.key;
    } catch (error) {
      console.warn("failed to resize terminal", error);
      const hasNewer = sync.pending && sync.pending.key !== request.key;
      if (!hasNewer && request.attempt + 1 < RESIZE_MAX_ATTEMPTS) {
        request.attempt += 1;
        sync.pending = request;
        sendNewerImmediately = false;
        const delay = RESIZE_RETRY_MS * 2 ** (request.attempt - 1);
        sync.retryTimer = window.setTimeout(() => {
          sync.retryTimer = null;
          void drainResize(paneId, sync);
        }, delay);
      } else if (!hasNewer && state.terminalSizes.get(paneId) === request.key) {
        state.terminalSizes.delete(paneId);
      }
    } finally {
      sync.inFlight = false;
      if (
        sendNewerImmediately &&
        sync.pending &&
        state.terminalResizeSyncs.get(paneId) === sync
      ) {
        void drainResize(paneId, sync);
      }
    }
  }

  function scheduleFit(paneId) {
    const view = state.terminalViews.get(paneId);
    if (!view || view.fitFrame) return;
    view.fitFrame = window.requestAnimationFrame(() => {
      const current = state.terminalViews.get(paneId);
      if (!current) return;
      current.fitFrame = null;
      fit(paneId);
    });
  }

  function fit(paneId) {
    const view = state.terminalViews.get(paneId);
    if (!view?.fitAddon || !view.host?.isConnected) return;
    try {
      view.fitAddon.fit();
      syncSize(paneId, view.terminal.cols, view.terminal.rows);
    } catch (error) {
      console.warn("failed to fit terminal", error);
    }
  }

  function create(paneId, host) {
    const appearance = getAppearance();
    const terminal = new globalThis.Terminal({
      allowProposedApi: false,
      convertEol: true,
      customGlyphs: true,
      cursorBlink: true,
      cursorStyle: "block",
      drawBoldTextInBrightColors: true,
      fontFamily: resolveFontFamily(appearance),
      fontSize: resolveFontSize(appearance),
      lineHeight: 1,
      macOptionIsMeta: true,
      scrollback: MAX_TERMINAL_ROWS,
      // Exposes the rows to assistive technology; a per-client preference
      // because xterm documents a cost on heavy output.
      screenReaderMode: appearance?.screenReader === true,
      theme: resolveTheme(appearance),
    });
    const FitAddon = globalThis.FitAddon?.FitAddon;
    const fitAddon = FitAddon ? new FitAddon() : null;
    if (fitAddon) terminal.loadAddon(fitAddon);
    const SearchAddon = searchAddonFactory();
    const searchAddon = SearchAddon ? new SearchAddon() : null;
    if (searchAddon) terminal.loadAddon(searchAddon);

    terminal.open(host);
    applyScreenBackground(terminal, appearance);
    terminal.onData((data) => void write(paneId, data));
    terminal.onResize(({ cols, rows }) => syncSize(paneId, cols, rows));

    const resizeObserver = new ResizeObserver(() => scheduleFit(paneId));
    resizeObserver.observe(host);
    const view = {
      terminal,
      fitAddon,
      searchAddon,
      fitFrame: null,
      resizeObserver,
      observedContainer: host,
      host,
    };
    state.terminalViews.set(paneId, view);
    const pending = state.terminalBuffers.get(paneId);
    if (pending) terminal.write(pending);
    scheduleFit(paneId);
    return view;
  }

  function mount(paneId, host) {
    if (!host || !available()) return null;
    host.dataset.paneId = paneId;
    let view = state.terminalViews.get(paneId);
    if (!view) return create(paneId, host);

    if (view.terminal.element && view.terminal.element.parentElement !== host) {
      host.append(view.terminal.element);
    }
    if (view.observedContainer !== host) {
      if (view.observedContainer) view.resizeObserver?.unobserve(view.observedContainer);
      view.resizeObserver?.observe(host);
      view.observedContainer = host;
    }
    view.host = host;
    scheduleFit(paneId);
    return view;
  }

  function detach(paneId, host) {
    const view = state.terminalViews.get(paneId);
    if (!view || view.host !== host) return;
    view.resizeObserver?.unobserve(host);
    view.observedContainer = null;
    view.host = null;
  }

  function dispose(paneId) {
    const view = state.terminalViews.get(paneId);
    if (view) {
      if (view.fitFrame) window.cancelAnimationFrame(view.fitFrame);
      view.resizeObserver?.disconnect();
      view.terminal.dispose();
    }
    const sync = state.terminalResizeSyncs.get(paneId);
    if (sync?.retryTimer) window.clearTimeout(sync.retryTimer);
    state.terminalResizeSyncs.delete(paneId);
    state.terminalViews.delete(paneId);
    state.terminalSizes.delete(paneId);
    state.terminalBuffers.delete(paneId);
  }

  function setText(paneId, text) {
    state.terminalBuffers.set(paneId, text);
    const view = state.terminalViews.get(paneId);
    if (view) {
      view.terminal.reset();
      view.terminal.write(text);
    }
  }

  function append(paneId, data) {
    if (!paneId || !data) return;
    const view = state.terminalViews.get(paneId);
    if (view) {
      view.terminal.write(data);
      return;
    }
    if (!hasPane(paneId)) return;
    const current = state.terminalBuffers.get(paneId) ?? "";
    state.terminalBuffers.set(paneId, capBufferedOutput(current + data));
  }

  function focus(paneId) {
    const view = state.terminalViews.get(paneId);
    if (!view?.terminal.element?.isConnected) return false;
    view.terminal.focus();
    scheduleFit(paneId);
    return true;
  }

  function applyAppearance(appearance) {
    liveReapplyAppearance(state.terminalViews.values(), appearance);
    for (const view of state.terminalViews.values()) {
      if (view?.terminal?.options) view.terminal.options.screenReaderMode = appearance?.screenReader === true;
    }
    for (const paneId of state.terminalViews.keys()) scheduleFit(paneId);
  }

  function resizeVisible() {
    for (const [paneId, view] of state.terminalViews) {
      if (view?.host?.isConnected) scheduleFit(paneId);
    }
  }

  function disposeAll() {
    for (const paneId of Array.from(state.terminalViews.keys())) dispose(paneId);
  }

  return {
    available,
    mount,
    detach,
    dispose,
    disposeAll,
    setText,
    append,
    focus,
    fit,
    scheduleFit,
    syncSize,
    applyAppearance,
    resizeVisible,
  };
}
