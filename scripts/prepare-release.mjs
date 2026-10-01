import { createHash } from "node:crypto";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";

function filesUnder(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isSymbolicLink()) throw Error(`Symlink in release artifacts: ${path}`);
    return entry.isDirectory() ? filesUnder(path) : [path];
  });
}

export function verifySignature(artifact, signature, publicKey) {
  const temporary = mkdtempSync(join(tmpdir(), "sgian-signature-"));
  try {
    const keyPath = join(temporary, "public.key");
    const signaturePath = join(temporary, "artifact.minisig");
    writeFileSync(keyPath, Buffer.from(publicKey, "base64"));
    writeFileSync(signaturePath, Buffer.from(signature, "base64"));
    const result = spawnSync("minisign", ["-V", "-m", artifact, "-p", keyPath, "-x", signaturePath], { encoding: "utf8" });
    if (result.error || result.status !== 0) {
      throw Error(`Signature verification failed for ${basename(artifact)}: ${result.error?.message ?? result.stderr}`);
    }
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

export function prepareRelease(input, output, config, verify = verifySignature) {
  const files = filesUnder(input);
  if (files.some((file) => basename(file) === "STUB-BUILD")) throw Error("Refusing stub build artifacts");
  const named = new Map();
  for (const file of files) {
    const name = basename(file);
    if (named.has(name)) throw Error(`Duplicate release asset: ${name}`);
    named.set(name, file);
  }
  const one = (suffix) => {
    const matches = files.filter((file) => file.endsWith(suffix));
    if (matches.length !== 1) throw Error(`Expected exactly one ${suffix} artifact, found ${matches.length}`);
    return matches[0];
  };
  const macInstaller = one(".dmg");
  const mac = one(".app.tar.gz");
  const windows = one("-setup.exe");
  const linux = one(".AppImage");
  const deb = one(".deb");
  const artifacts = [macInstaller, mac, windows, linux, deb];
  const version = config.version;
  for (const file of [macInstaller, windows, linux, deb]) {
    if (!basename(file).startsWith(`Sgian_${version}_`) && !basename(file).startsWith(`sgian_${version}_`)) {
      throw Error(`Artifact does not match release version ${version}: ${basename(file)}`);
    }
  }
  const architecture = (file) => {
    const match = basename(file).match(/_(aarch64|arm64|x64|x86_64|amd64)(?:[_.-]|$)/);
    if (!match) throw Error(`Unknown artifact architecture: ${basename(file)}`);
    return ["aarch64", "arm64"].includes(match[1]) ? "aarch64" : "x86_64";
  };
  const platforms = {};
  for (const [platform, file, archFile] of [["darwin", mac, macInstaller], ["windows", windows, windows], ["linux", linux, linux]]) {
    const signaturePath = named.get(`${basename(file)}.sig`);
    if (!signaturePath) throw Error(`Missing signature for ${basename(file)}`);
    const signature = readFileSync(signaturePath, "utf8").trim();
    if (!signature) throw Error(`Empty signature for ${basename(file)}`);
    verify(file, signature, config.plugins.updater.pubkey);
    platforms[`${platform}-${architecture(archFile)}`] = {
      signature,
      url: `https://github.com/craigcode/sgian/releases/download/v${version}/${encodeURIComponent(basename(file))}`,
    };
    artifacts.push(signaturePath);
  }
  // All validation happens before generating anything publishable.
  mkdirSync(output, { recursive: true });
  if (readdirSync(output).length) throw Error("Release output directory must be empty");
  for (const file of artifacts) copyFileSync(file, join(output, basename(file)));
  const feed = { version, platforms };
  writeFileSync(join(output, "latest.json"), `${JSON.stringify(feed, null, 2)}\n`);
  const sums = readdirSync(output).sort().map((name) => {
    const hash = createHash("sha256").update(readFileSync(join(output, name))).digest("hex");
    return `${hash}  ${name}`;
  });
  writeFileSync(join(output, "SHA256SUMS.txt"), `${sums.join("\n")}\n`);
  return feed;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  await import("./verify-release-config.mjs");
  const [input, output] = process.argv.slice(2);
  if (!input || !output) throw Error("Usage: node scripts/prepare-release.mjs INPUT OUTPUT");
  const config = JSON.parse(readFileSync(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  const feed = prepareRelease(resolve(input), resolve(output), config);
  console.log(`Verified release ${feed.version}: ${Object.keys(feed.platforms).join(", ")}`);
}
