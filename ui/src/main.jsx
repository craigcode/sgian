import { loadBundledXtermRuntime } from "./xterm-runtime.js";
import React from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app.jsx";
import { createAppController } from "./app-controller.js";
import { createWebBridge, isServedPage } from "./web-bridge.js";
import "./styles.css";

export let controller = null;
export let root = null;
export let __test = null;

async function initializeApplication() {
  try {
    await loadBundledXtermRuntime();
  } catch (error) {
    // The controller degrades shell panes explicitly while keeping the native
    // workspace and agent panes functional, so an asset error is not fatal.
    console.error("[sgian] failed to initialize xterm", error);
  }

  const rootElement = document.querySelector("#root");
  if (!rootElement) throw new Error("Sgian root element is missing");

  // Served by `sgian ctl serve` (no Tauri): the same controller over fetch +
  // SSE, landing on the overview (the board is what a phone wants first).
  controller = createAppController(
    isServedPage()
      ? (() => {
          const bridge = createWebBridge();
          return { ...bridge, nativeClose: bridge.close, landing: "overview" };
        })()
      : {},
  );
  // Begin native event subscription/snapshot loading before the initial xterm
  // layout effect can emit a resize. start() is idempotent when App mounts.
  controller.start();
  root = createRoot(rootElement);
  root.render(<App controller={controller} />);

  __test = {
    state: controller.state,
    syncTerminalSize: controller.syncTerminalSize,
    applyWorkspaceSnapshot: controller.applyWorkspaceSnapshot,
    resyncWorkspace: controller.resyncWorkspace,
    startPeriodicResync: controller.startPeriodicResync,
    stopPeriodicResync: controller.stopPeriodicResync,
    render: controller.notify,
    capBufferedOutput: controller.capBufferedOutput,
    controller,
  };
}

export const ready = initializeApplication();
