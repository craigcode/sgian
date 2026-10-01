// web-bridge.js — the bridge the page uses when it is served by `sgian serve`
// (docs/design/served-view.md) instead of hosted by Tauri. Same shape as the
// Tauri bridge the controller expects: `invoke(command, args)` and
// `listen(name, handler)`. Requests go over `fetch` to `/api/invoke`; events
// arrive on one shared EventSource at `/api/events`, dispatched by name.

/**
 * True when the page is served over HTTP rather than hosted by Tauri.
 */
export function isServedPage() {
  return typeof window !== "undefined" && !window.__TAURI__ && /^https?:$/.test(window.location?.protocol ?? "");
}

/**
 * Build the served-page bridge. `fetchImpl` and `EventSourceImpl` are
 * injectable for tests.
 *
 * @returns {{ nativeInvoke: Function, nativeListen: Function, close: Function }}
 */
export function createWebBridge({
  fetchImpl = (...args) => globalThis.fetch(...args),
  EventSourceImpl = globalThis.EventSource,
  base = "",
} = {}) {
  const handlers = new Map();
  let source = null;

  async function nativeInvoke(command, args = {}) {
    const response = await fetchImpl(`${base}/api/invoke`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ command, args }),
    });
    if (!response.ok) throw new Error(`${command}: HTTP ${response.status}`);
    const body = await response.json();
    if (!body || body.ok !== true) throw new Error(body?.error || `${command} failed`);
    return body.result;
  }

  function ensureSource() {
    if (source) return source;
    source = new EventSourceImpl(`${base}/api/events`);
    return source;
  }

  async function nativeListen(name, handler) {
    const es = ensureSource();
    let set = handlers.get(name);
    if (!set) {
      set = new Set();
      handlers.set(name, set);
      es.addEventListener(name, (event) => {
        let payload = null;
        try {
          payload = JSON.parse(event.data);
        } catch {
          return;
        }
        for (const fn of handlers.get(name) ?? []) fn({ event: name, payload });
      });
    }
    set.add(handler);
    return () => {
      set.delete(handler);
    };
  }

  function close() {
    source?.close?.();
    source = null;
    handlers.clear();
  }

  return { nativeInvoke, nativeListen, close };
}
