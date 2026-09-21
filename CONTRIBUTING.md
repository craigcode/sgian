# Contributing to Sgian

Thanks for looking. Sgian is a single Rust daemon with three thin clients, and
most changes land in the daemon or the shared React frontend. This page says
how to build each piece, what CI expects, and the few conventions that are not
obvious from the code.

## Build and run

Node.js 22.12+ and the Rust toolchain pinned in `rust-toolchain.toml` are the
only hard requirements. Keep rustup ahead of any system Rust on `PATH`.

```bash
npm ci --ignore-scripts      # frontend and Tauri tooling
npm run dev                  # Tauri app, starts Vite and a daemon for you
```

| Piece | Where | Build and check |
| --- | --- | --- |
| Daemon, `ctl`, Tauri backend | `src-tauri/` | `cargo test` in `src-tauri` |
| Frontend (React, shared by Tauri and the served view) | `ui/` | `npm test` |
| macOS client (SwiftUI) | `apps/macos/` | `scripts/verify-macos-native.sh`, then `scripts/build-macos-native.sh` |
| Windows client (WinUI 3) | `apps/windows/` | `scripts\verify-windows-native.ps1`, then `scripts\build-windows-native.ps1` |
| Linux bundles | Tauri | `npm run tauri build -- --bundles deb,appimage` |

Linux development needs Tauri's
[system dependencies](https://v2.tauri.app/start/prerequisites/#linux). The
Windows client builds only on Windows; the macOS client only on macOS. CI
covers the platforms you cannot, so a daemon-only change is fine to send from
any of them.

## Before you open a pull request

The four required checks on `main` are the ones to run locally:

1. `fmt + clippy + test + vitest`: from `src-tauri`, `cargo fmt --check`,
   `cargo clippy --locked --all-targets -- -D warnings` and
   `cargo test --locked`; from the repo root, `npm test`,
   `npm run release:check`, `python3 scripts/test-native-release.py` and
   `node --test scripts/test-native-terminal.mjs`. The same job runs
   `cargo audit`, `npm audit --audit-level=moderate` and the secret scan
   (`scripts/audit-public-history.sh`, which needs `gitleaks`).
2. `build app bundle`: the Tauri `.app` plus the native macOS app, each
   launched against a temporary daemon.
3. `build linux bundles + smoke`: the Linux Rust tests and packaged bundles.
4. `build windows installer + smoke`: the WinUI app and MSIX.

If you only have one platform, run the first check and let CI run the rest.

## Conventions

- **Squash merges.** Every pull request lands as one commit whose subject is
  the PR title followed by `(#N)`. Write the title as the changelog line you
  would want to read.
- **Branch protection.** `main` requires the four checks and an up-to-date
  branch. Rebase or merge `main` into your branch before asking for review.
- **Changelog.** Add a line under `Unreleased` in `CHANGELOG.md` for anything
  a user or an integrator would notice.
- **Versions are the maintainer's call.** Do not bump `Cargo.toml`,
  `package.json`, `tauri.conf.json` or the native project versions, and do not
  propose tags. Releases follow `docs/releasing.md`.
- **Wire contract changes go in `docs/native-ipc.md`.** A new `DaemonRequest`,
  `DaemonEvent`, snapshot field or `ctl` verb changes what the macOS and
  Windows clients see. Document it there in the same PR and, when it affects
  them, update `apps/macos` and `apps/windows` too. The served view's
  command bridge (`serve_http.rs`) shares the Tauri command names, so a new
  Tauri command needs a mapping there as well.
- **Where things live.** `docs/daemon-modules.md` maps every file under
  `src-tauri/src` and says where a new request, scope, handler, `ctl` verb
  and test go. Child modules use `use super::*;` and `pub(crate)` on moved
  items; tests sit under `src-tauri/src/tests/` by feature.
- **Security-sensitive changes.** Anything touching the socket, tokens,
  credentials, the served view or process spawning should cite the relevant
  section of `SECURITY.md` or one of the design notes under `docs/design/`
  in the PR description. Report vulnerabilities privately as `SECURITY.md`
  describes, never in a public issue.
- **No secrets in fixtures.** The secret scan runs on every push and fails
  the build; keep tokens and prompts out of tests and docs.

## Design notes

The design notes under `docs/design/` record the decisions behind the
keyboard lease and ledger, execution grants, per-client identity and the
served view. Read the one that touches your change before proposing a
different shape, and add a note when you introduce a new boundary.
