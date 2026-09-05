import { readFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const json = (path) => JSON.parse(readFileSync(resolve(root, path), "utf8"));
const config = json("src-tauri/tauri.conf.json");
const version = config.version;
if (!/^\d+\.\d+\.\d+$/.test(version)) throw Error("Public releases require a stable x.y.z version");
const cargo = readFileSync(resolve(root, "src-tauri/Cargo.toml"), "utf8");
for (const [name, actual] of [
  ["package.json", json("package.json").version],
  ["package-lock.json", json("package-lock.json").version],
  ["Cargo.toml", cargo.match(/^version = "([^"]+)"/m)?.[1]],
]) {
  if (actual !== version) throw Error(`${name} version ${actual} differs from Tauri ${version}`);
}
if (process.env.GITHUB_REF_TYPE === "tag" && process.env.GITHUB_REF_NAME !== `v${version}`) {
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
console.log(`Release configuration verified: ${version}`);
