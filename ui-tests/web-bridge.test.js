import { describe, it, expect, vi } from "vitest";
import { createWebBridge, isServedPage } from "../ui/src/web-bridge.js";

class FakeEventSource {
  static instances = [];
  constructor(url) {
    this.url = url;
    this.listeners = new Map();
    this.closed = false;
    FakeEventSource.instances.push(this);
  }
  addEventListener(name, fn) {
    this.listeners.set(name, fn);
  }
  emit(name, data) {
    this.listeners.get(name)?.({ data });
  }
  close() {
    this.closed = true;
  }
}

describe("web bridge", () => {
  it("posts invokes to /api/invoke and unwraps the daemon's answer", async () => {
    const calls = [];
    const fetchImpl = vi.fn(async (url, init) => {
      calls.push({ url, body: JSON.parse(init.body) });
      const { command } = JSON.parse(init.body);
      if (command === "boom") return { ok: true, json: async () => ({ ok: false, error: "read-only view: boom" }) };
      if (command === "http") return { ok: false, status: 500, json: async () => ({}) };
      return { ok: true, json: async () => ({ ok: true, result: { command } }) };
    });
    const bridge = createWebBridge({ fetchImpl, EventSourceImpl: FakeEventSource, base: "http://x" });
    expect(await bridge.nativeInvoke("bootstrap_workspace")).toEqual({ command: "bootstrap_workspace" });
    expect(calls[0]).toEqual({ url: "http://x/api/invoke", body: { command: "bootstrap_workspace", args: {} } });
    await expect(bridge.nativeInvoke("boom", { paneId: "p" })).rejects.toThrow("read-only view: boom");
    await expect(bridge.nativeInvoke("http")).rejects.toThrow("HTTP 500");
    expect(calls[1].body.args).toEqual({ paneId: "p" });
  });

  it("shares one EventSource and dispatches parsed payloads by event name", async () => {
    FakeEventSource.instances = [];
    const bridge = createWebBridge({ fetchImpl: vi.fn(), EventSourceImpl: FakeEventSource });
    const seen = [];
    const off = await bridge.nativeListen("pty-output", (e) => seen.push(["a", e.payload]));
    await bridge.nativeListen("pty-output", (e) => seen.push(["b", e.payload]));
    await bridge.nativeListen("lease-state", (e) => seen.push(["lease", e.payload]));
    expect(FakeEventSource.instances).toHaveLength(1);
    const es = FakeEventSource.instances[0];
    expect(es.url).toBe("/api/events");
    es.emit("pty-output", JSON.stringify({ pane_id: "p", data: "x" }));
    es.emit("lease-state", "not json");
    es.emit("lease-state", JSON.stringify({ pane_id: "p", transition: "taken" }));
    expect(seen).toEqual([
      ["a", { pane_id: "p", data: "x" }],
      ["b", { pane_id: "p", data: "x" }],
      ["lease", { pane_id: "p", transition: "taken" }],
    ]);
    off();
    es.emit("pty-output", JSON.stringify({ pane_id: "p", data: "y" }));
    expect(seen.filter(([who]) => who === "a")).toHaveLength(1);
    expect(seen.filter(([who]) => who === "b")).toHaveLength(2);
    bridge.close();
    expect(es.closed).toBe(true);
  });

  it("claims to be served only over http without the Tauri bridge", () => {
    // happy-dom loads the page at an http: origin, like `sgian serve` does.
    expect(isServedPage()).toBe(true);
    window.__TAURI__ = {};
    try {
      expect(isServedPage()).toBe(false);
    } finally {
      delete window.__TAURI__;
    }
  });
});
