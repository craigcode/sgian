import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { readFileSync } from "node:fs";

export default defineConfig({
  root: "ui",
  // Tauri serves the bundle through a platform-specific application origin.
  // Relative URLs work for macOS/Linux custom protocols and Windows WebView2
  // without assuming that `/` maps to the packaged asset root.
  base: "./",
  // Vendored xterm is imported into the application module graph. Do not also
  // copy it as public files, which would reintroduce an independent load path.
  publicDir: false,
  plugins: [react(), {
    name: "xterm-license",
    generateBundle() {
      this.emitFile({
        type: "asset",
        fileName: "licenses/xterm.txt",
        source: readFileSync(new URL("./ui/vendor/xterm/LICENSE", import.meta.url), "utf8"),
      });
    },
  }],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    // xterm's classic UMD bundles are loaded by URL so they execute with their
    // browser-global semantics. Keep even the small add-ons as same-origin
    // files; data: scripts are intentionally forbidden by the production CSP.
    assetsInlineLimit: 0,
  },
});
