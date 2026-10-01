# xterm assets

These unchanged UMD bundles come from the exact npm dependencies in
`package-lock.json`: `@xterm/xterm` 6.0.0, `@xterm/addon-fit` 0.11.0,
and `@xterm/addon-search` 0.16.0. Their npm integrity hashes are recorded in
the lockfile. `scripts/verify-vendored-xterm.mjs` compares every JS/CSS file
with the installed packages before a frontend build.

When upgrading, update the package versions, lockfile, and corresponding
`lib/*.js` / `css/xterm.css` copies together, then run tests and the packaged
UI smoke. See LICENSE for the upstream MIT notice.
