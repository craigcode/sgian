// Vite owns the URLs for these vendored classic scripts, so every packaged
// platform gets same-origin hashed assets. They are loaded sequentially and
// awaited before controller bootstrap; this preserves the UMD browser-global
// behavior without relying on HTML module/defer timing.
import "../vendor/xterm/xterm.css";
import terminalUrl from "../vendor/xterm/xterm.js?url";
import fitAddonUrl from "../vendor/xterm/addon-fit.js?url";
import searchAddonUrl from "../vendor/xterm/addon-search.js?url";

export const xtermAssetUrls = [terminalUrl, fitAddonUrl, searchAddonUrl];

export function bundledXtermRuntimeAvailable() {
  return typeof globalThis.Terminal === "function";
}

function appendClassicScript(url) {
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = url;
    script.async = false;
    script.addEventListener("load", resolve, { once: true });
    script.addEventListener(
      "error",
      () => reject(new Error(`failed to load terminal asset: ${url}`)),
      { once: true },
    );
    document.head.append(script);
  });
}

export async function loadBundledXtermRuntime(loadScript = appendClassicScript) {
  // Tests and embedding hosts may deliberately provide a compatible terminal
  // constructor. Preserve that supported injection boundary.
  if (bundledXtermRuntimeAvailable()) return true;
  for (const url of xtermAssetUrls) await loadScript(url);
  if (!bundledXtermRuntimeAvailable()) {
    throw new Error("terminal asset loaded without registering globalThis.Terminal");
  }
  return true;
}
