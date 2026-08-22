// tauri.js — injectable boundary for window.__TAURI__ access.
//
// All access to the Tauri native bridge goes through this module so that tests
// (vitest + happy-dom) can stub window.__TAURI__ without touching logic code.

const NATIVE_COMMAND_TIMEOUT_MS = 5000;

/**
 * Returns the native invoke function if the Tauri bridge is present, otherwise null.
 */
export function getTauriInvoke() {
  return window.__TAURI__?.core?.invoke ?? null;
}

/**
 * Returns the native event listen function if the Tauri bridge is present, otherwise null.
 */
export function getTauriListen() {
  return window.__TAURI__?.event?.listen ?? null;
}

/**
 * True when the Tauri native bridge is available (not browser-preview mode).
 */
export function hasNativeBridge() {
  return !!getTauriInvoke();
}

/**
 * Race a promise against a timeout, rejecting with "<command> timed out".
 */
export function withTimeout(promise, command, ms = NATIVE_COMMAND_TIMEOUT_MS) {
  let timeoutId;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timeoutId = setTimeout(() => reject(new Error(`${command} timed out`)), ms);
    }),
  ]).finally(() => clearTimeout(timeoutId));
}
