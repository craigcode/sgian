import { existsSync, readFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const indexPath = resolve(root, "dist/index.html");

function fail(message) {
  console.error(`frontend bundle verification failed: ${message}`);
  process.exitCode = 1;
}

if (!existsSync(indexPath)) {
  fail("missing dist/index.html");
} else {
  const html = readFileSync(indexPath, "utf8");
  const moduleIndex = html.indexOf('<script type="module"');
  if (moduleIndex === -1) fail("missing Vite module entry");

  const rootRelativeAsset = /(?:src|href)="\/(?!\/)/.exec(html);
  if (rootRelativeAsset) {
    fail(`root-relative packaged asset URL near ${rootRelativeAsset[0]}`);
  }

  if (/src="[^"]*xterm\//.test(html)) {
    fail("xterm is still loaded as an independent HTML script");
  }

  for (const [, url] of html.matchAll(/(?:src|href)="(\.\/[^"?#]+)["?#]/g)) {
    if (!existsSync(resolve(root, "dist", url))) fail(`missing generated asset ${url}`);
  }

  const assets = readdirSync(resolve(root, "dist/assets"));
  for (const pattern of [/^xterm-.*\.js$/, /^addon-fit-.*\.js$/, /^addon-search-.*\.js$/]) {
    if (!assets.some((name) => pattern.test(name))) fail(`missing emitted asset ${pattern}`);
  }
}

if (!process.exitCode) console.log("frontend bundle assets verified");
