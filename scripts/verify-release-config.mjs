import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export function verifyReleaseConfig(root = resolve(import.meta.dirname, ".."), env = process.env) {
  const json = (path) => JSON.parse(readFileSync(resolve(root, path), "utf8"));
  const text = (path) => readFileSync(resolve(root, path), "utf8");
  const config = json("src-tauri/tauri.conf.json");
  const version = config.version;
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) throw Error("Public releases require a stable x.y.z version");
  if (version.split(".").some((part) => Number(part) > 65535)) throw Error("Version exceeds the Windows package version limit");
  const cargo = text("src-tauri/Cargo.toml");
  const cargoPackage = cargo.split(/^\[package\]\s*$/m)[1]?.split(/^\[/m)[0];
  const lockedPackage = text("src-tauri/Cargo.lock").split(/^\[\[package\]\]\s*$/m)
    .find((entry) => /^name = "sgian"$/m.test(entry));
  const mac = text("apps/macos/Info.plist");
  const plistVersion = (key) => mac.match(new RegExp(`<key>${key}</key>\\s*<string>([^<]+)</string>`))?.[1];
  const windows = text("apps/windows/Sgian.Windows/Package.appxmanifest")
    .match(/<Identity\b[^>]*\bVersion="([^"]+)"/)?.[1];
  for (const [name, actual] of [
    ["package.json", json("package.json").version],
    ["package-lock.json", json("package-lock.json").version],
    ["package-lock.json root package", json("package-lock.json").packages?.[""]?.version],
    ["Cargo.toml", cargoPackage?.match(/^version = "([^"]+)"/m)?.[1]],
    ["Cargo.lock sgian package", lockedPackage?.match(/^version = "([^"]+)"/m)?.[1]],
    ["macOS short version", plistVersion("CFBundleShortVersionString")],
    ["macOS build version", plistVersion("CFBundleVersion")],
  ]) {
    if (actual !== version) throw Error(`${name} version ${actual} differs from Tauri ${version}`);
  }
  if (windows !== `${version}.0`) throw Error(`Windows package version ${windows} differs from ${version}.0`);
  if (env.GITHUB_REF_TYPE === "tag" && env.GITHUB_REF_NAME !== `v${version}`) {
    throw Error(`Release tag must be v${version}`);
  }
  const updater = config.plugins.updater;
  if (updater.dangerousInsecureTransportProtocol || config.bundle.createUpdaterArtifacts !== true) {
    throw Error("Release must generate signed updater artifacts with secure transport");
  }
  const endpoint = "https://github.com/craigcode/sgian/releases/latest/download/latest.json";
  if (updater.endpoints.length !== 1 || updater.endpoints[0] !== endpoint) {
    throw Error("Updater endpoint must match the release workflow's static feed");
  }
  const key = Buffer.from(updater.pubkey, "base64").toString("utf8").trim().split("\n");
  if (key.length !== 2 || Buffer.from(key[1], "base64").length !== 42) {
    throw Error("Invalid embedded minisign public key");
  }
  if (!config.app.security.csp || config.app.security.csp.includes("'unsafe-eval'")) {
    throw Error("Release requires a restrictive content security policy");
  }
  return version;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  console.log(`Release configuration verified: ${verifyReleaseConfig()}`);
}
