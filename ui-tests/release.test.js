// @vitest-environment node
import { afterEach, expect, it, vi } from "vitest";
import { mkdtempSync, mkdirSync, writeFileSync, existsSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { prepareRelease } from "../scripts/prepare-release.mjs";

const roots = [];
afterEach(() => roots.splice(0).forEach((root) => rmSync(root, { recursive: true, force: true })));
function fixture() {
  const root = mkdtempSync(join(tmpdir(), "sgian-release-test-"));
  roots.push(root);
  const input = join(root, "input");
  const output = join(root, "output");
  mkdirSync(input);
  const names = ["Sgian_0.1.0_aarch64.dmg", "Sgian.app.tar.gz", "Sgian_0.1.0_x64-setup.exe", "Sgian_0.1.0_amd64.AppImage", "sgian_0.1.0_amd64.deb"];
  for (const name of names) writeFileSync(join(input, name), name);
  for (const name of names.slice(1, 4)) writeFileSync(join(input, `${name}.sig`), `signature-${name}`);
  return { input, output, config: { version: "0.1.0", plugins: { updater: { pubkey: "test-key" } } } };
}
it("assembles all platforms only after verifying each updater signature", () => {
  const { input, output, config } = fixture();
  const verify = vi.fn();
  const feed = prepareRelease(input, output, config, verify);
  expect(verify).toHaveBeenCalledTimes(3);
  expect(Object.keys(feed.platforms)).toEqual(["darwin-aarch64", "windows-x86_64", "linux-x86_64"]);
  expect(feed.platforms["windows-x86_64"].url).toMatch(/-setup\.exe$/);
  expect(JSON.parse(readFileSync(join(output, "latest.json"), "utf8"))).toEqual(feed);
  expect(readFileSync(join(output, "SHA256SUMS.txt"), "utf8")).toMatch(/[a-f0-9]{64}  latest\.json/);
});
it.each(["missing-platform", "missing-signature", "wrong-version", "stub", "bad-signature", "duplicate"])("refuses %s without preparing publishable output", (problem) => {
  const { input, output, config } = fixture();
  const verify = vi.fn();
  if (problem === "missing-platform") rmSync(join(input, "Sgian_0.1.0_amd64.AppImage"));
  if (problem === "missing-signature") rmSync(join(input, "Sgian.app.tar.gz.sig"));
  if (problem === "wrong-version") config.version = "0.2.0";
  if (problem === "stub") writeFileSync(join(input, "STUB-BUILD"), "");
  if (problem === "bad-signature") verify.mockImplementation(() => { throw Error("invalid signature"); });
  if (problem === "duplicate") {
    mkdirSync(join(input, "nested"));
    writeFileSync(join(input, "nested", "Sgian.app.tar.gz"), "duplicate");
  }
  expect(() => prepareRelease(input, output, config, verify)).toThrow();
  expect(existsSync(output)).toBe(false);
});
