import { afterAll, describe, expect, it } from "vitest";

const runtime = await import("../ui/src/xterm-runtime.js");

afterAll(() => {
  delete globalThis.Terminal;
  delete globalThis.FitAddon;
  delete globalThis.SearchAddon;
});

describe("bundled xterm runtime", () => {
  it("loads the terminal and add-on assets in dependency order before bootstrap", async () => {
    delete globalThis.Terminal;
    const loaded = [];
    await runtime.loadBundledXtermRuntime(async (url) => {
      loaded.push(url);
      if (url === runtime.xtermAssetUrls[0]) globalThis.Terminal = class {};
    });

    expect(loaded).toEqual(runtime.xtermAssetUrls);
    expect(runtime.bundledXtermRuntimeAvailable()).toBe(true);
  });

  it("does not load duplicate assets when a terminal runtime is injected", async () => {
    globalThis.Terminal = class {};
    const loadScript = () => {
      throw new Error("must not load");
    };
    await expect(runtime.loadBundledXtermRuntime(loadScript)).resolves.toBe(true);
  });
});
