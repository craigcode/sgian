// @vitest-environment node
import { afterEach, expect, it } from "vitest";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { verifyReleaseConfig } from "../scripts/verify-release-config.mjs";

const roots = [];
afterEach(() => roots.splice(0).forEach((root) => rmSync(root, { recursive: true, force: true })));
const repo = resolve(import.meta.dirname, "..");
function fixture() {
  const root = mkdtempSync(join(tmpdir(), "sgian-version-check-"));
  roots.push(root);
  for (const file of ["package.json", "package-lock.json", "src-tauri/Cargo.toml", "src-tauri/Cargo.lock",
    "src-tauri/tauri.conf.json", "apps/macos/Info.plist", "apps/windows/Sgian.Windows/Package.appxmanifest"]) {
    mkdirSync(dirname(join(root, file)), { recursive: true });
    copyFileSync(join(repo, file), join(root, file));
  }
  return root;
}
function edit(root, file, change) {
  const path = join(root, file);
  writeFileSync(path, change(readFileSync(path, "utf8")));
}

it("accepts the release sources and the matching tag", () => {
  const version = JSON.parse(readFileSync(join(repo, "package.json"), "utf8")).version;
  expect(verifyReleaseConfig(repo, { GITHUB_REF_TYPE: "tag", GITHUB_REF_NAME: `v${version}` })).toBe(version);
});

it.each([
  ["npm root lock entry", "package-lock.json", (text) => {
    const data = JSON.parse(text); data.packages[""].version = "0.0.0"; return JSON.stringify(data);
  }],
  ["Cargo root lock entry", "src-tauri/Cargo.lock", (text) => text.replace(/(name = "sgian"\nversion = ")[^"]+/, "$10.0.0")],
  ["Mac display version", "apps/macos/Info.plist", (text) => text.replace(/(<key>CFBundleShortVersionString<\/key>\s*<string>)[^<]+/, "$10.0.0")],
  ["Mac build version", "apps/macos/Info.plist", (text) => text.replace(/(<key>CFBundleVersion<\/key>\s*<string>)[^<]+/, "$10.0.0")],
  ["Windows package version", "apps/windows/Sgian.Windows/Package.appxmanifest", (text) => text.replace(/(\bVersion=")[^"]+/, "$10.0.0.0")],
])("refuses a stale %s even when the main manifests agree", (_, file, change) => {
  const root = fixture(); edit(root, file, change);
  expect(() => verifyReleaseConfig(root, {})).toThrow(/version.*differs/);
});

it.each(["01.2.0", "0.2.0-rc.1", "65536.0.0"])("rejects an unshippable native version %s", (version) => {
  const root = fixture();
  edit(root, "src-tauri/tauri.conf.json", (text) => JSON.stringify({ ...JSON.parse(text), version }));
  expect(() => verifyReleaseConfig(root, {})).toThrow(/stable x.y.z|Windows package version limit/);
});

it("refuses a mismatched release tag", () => {
  expect(() => verifyReleaseConfig(repo, { GITHUB_REF_TYPE: "tag", GITHUB_REF_NAME: "v0.0.0" })).toThrow(/Release tag/);
});
