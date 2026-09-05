import { readFileSync } from "node:fs";

const assets = [
  ["xterm.js", "@xterm/xterm/lib/xterm.js"],
  ["xterm.css", "@xterm/xterm/css/xterm.css"],
  ["addon-fit.js", "@xterm/addon-fit/lib/addon-fit.js"],
  ["addon-search.js", "@xterm/addon-search/lib/addon-search.js"],
];
for (const [vendored, upstream] of assets) {
  const actual = readFileSync(new URL(`../ui/vendor/xterm/${vendored}`, import.meta.url));
  const expected = readFileSync(new URL(`../node_modules/${upstream}`, import.meta.url));
  if (!actual.equals(expected)) throw Error(`Vendored ${vendored} differs from locked ${upstream}; update both together`);
}
console.log("Vendored xterm assets match the audited lockfile");
