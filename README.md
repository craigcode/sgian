# Sgian

Sgian is a Rust/Tauri 2 keyboard-driven pane manager and terminal multiplexer.
A single binary runs in three modes: the **GUI** app, a long-lived **daemon**
(`--daemon`), and a scriptable **control CLI** (`ctl`). Closing the GUI leaves
your shells running, tmux-style.

Panes come in two kinds: **shell** panes (regular PTY terminals) and **agent**
panes — chat-native sessions driving headless Claude Code or Factory Droid
processes with streaming replies, tool-call cards, and in-app permission prompts.
Agent sessions are daemon-owned too, so they survive the GUI closing and resume
where they left off. Shell panes running agent TUIs (e.g. the interactive
`claude` CLI) are auto-detected and badged by attention state
(working / needs input / idle).

Supported desktop targets are macOS 11+, Windows 10 version 1809+ (ConPTY),
and Linux distributions compatible with the Ubuntu 22.04/WebKitGTK 4.1 build
baseline. Every release is gated by native builds on all three platforms.

## Run

```bash
npm install
npm run dev
```

The React/Vite frontend lives in `ui/`; the Tauri backend lives in `src-tauri/`.
Tauri starts Vite automatically in development and builds `dist/` for packaged
applications. Linux development additionally requires Tauri's
[documented system dependencies](https://v2.tauri.app/start/prerequisites/#linux).

### Native macOS and Windows clients

Sgian also includes a SwiftUI/AppKit client in `apps/macos`. It uses the same
Rust daemon and authenticated IPC protocol as the Tauri client, embeds SwiftTerm
for native terminal rendering, and presents shell and agent panes in a macOS
sidebar interface.

`apps/windows` contains the non-Tauri Windows client: a C# WinUI 3 app with
native workspace/sidebar/chat/settings UI and a local WebView2+xterm terminal
surface. It talks to the same Rust daemon over its owner-restricted named pipe,
so ConPTY sessions and agents still survive the window closing.

```bash
# Build and exercise the Swift client against a real temporary daemon
scripts/verify-macos-native.sh

# Produce apps/macos/build/Sgian.app with the Rust daemon helper bundled inside
scripts/build-macos-native.sh
open apps/macos/build/Sgian.app

# On Windows: verify protocol logic, then build a portable app + unsigned MSIX
scripts\verify-windows-native.ps1
scripts\build-windows-native.ps1
```

CI also packages and launches the native app against a temporary daemon, then
uploads an architecture-labelled `sgian-macos-native-*` ZIP. That workflow
artifact is ad-hoc signed for testing; the tag-gated Developer ID/notarization
job still applies to the Tauri macOS bundle until native release signing is
wired explicitly.

The macOS native client requires macOS 13+; the Windows native client targets
Windows 10 version 1809+ and is built with the stable Windows App SDK 2.4. Set
`SGIAN_WORKSPACE=/path/to/project` when launching from a terminal, or choose a
workspace from the app. See
`docs/native-ipc.md` for the client/daemon contract.

## Architecture

A single binary runs in three modes:

- **GUI** - the Tauri app (a thin client).
- **Daemon** (`--daemon`) - a long-lived, per-workspace background process that owns the
  pane registry, PTY sessions, layout, and persistence. It talks to clients over a
  token-authenticated Unix domain socket on macOS/Linux or a per-user named pipe
  on Windows. It detaches from the launching session and outlives the GUI
  (tmux-style), so closing the window leaves your shells running.
- **Control CLI** (`ctl`) - scriptable access to a workspace's daemon.

### Daemon

- Structured logging via `tracing` with bounded log rotation.
- Advisory `flock` locking so only one daemon owns a workspace.
- Subscribe catch-up: new clients receive recent scrollback on attach.
- Restore policy: `auto_respawn` (default, revives ended panes on bootstrap) or
  `restore_on_demand` (ended panes are restored but not auto-restarted until
  `ctl restart`).
- Workspace key collision detection across config layers.
- Workspace identity is canonical: `/a/b`, `/a/b/`, and symlinks to the same
  directory share one workspace and one daemon.
- Env scrubbing: vars in `scrub_env` are removed before spawning PTYs; explicit
  `env` values take precedence over the scrub list.
- Config file-watch with live reload.
- Bounded log rotation so logs do not grow unbounded.
- Agent awareness (shell panes): the daemon classifies a pane's screen for known
  agent TUIs (Claude Code) and broadcasts `agent-state` transitions
  (working / needs input / idle), badged on the pane's tab.
- Agent panes: a daemon-owned Claude stream-json or Factory Droid JSON-RPC
  process per pane, selected when the pane is created. Both feed a normalized
  event stream with permission round-trips, bounded conversation logs, and
  session resume. They use the provider CLI's existing login.

### Control CLI

- Shell-aware exit codes: POSIX shells and fish both see correct status, with
  family-aware argument quoting (fish's single-quote escape rules differ).
- `ctl run --timeout MS` bounds the wait for the exit marker (a marker miss —
  e.g. a foreground TUI in the pane — then fails cleanly instead of hanging).
  Batched runs report per-pane exit code, timeout, elapsed time, and a bounded
  output tail (text or JSON).
- `ctl process` for non-PTY argv execution with exact exit codes (automation).
- `ctl diagnostic` for a privacy-safe support bundle (no prompts, tokens, or
  scrollback).
- `ctl new --profile NAME` to create a pane from a named shell/agent profile.
- Batched multi-pane exec via `--all` / `--panes`.
- `--lf` / `--raw` literal LF flag for precise input control.
- `ctl logs` to tail daemon logs.
- `ctl status --verbose` for detailed daemon state.
- `ctl write-config` to persist config changes.
- `ctl restart` to revive ended panes under `restore_on_demand`.
- `ctl ipc-endpoint --json` to start/locate a daemon and return non-secret
  native-client transport and token-file metadata.

### Cross-Platform

- `dirs` crate for portable data directories.
- Transport trait with `cfg(unix)` / `cfg(windows)` gating.
- macOS runs the complete Rust and frontend suites and builds the native `.app`;
  tagged builds are signed, notarized, and packaged as a DMG.
- Linux runs the complete Rust suite plus a release-binary smoke test covering
  daemon auto-spawn, a 32-client Unix-socket burst, pane create/list/send,
  agent streaming, and shutdown. CI builds both `.deb` and AppImage artifacts
  on Ubuntu 22.04. If `$SHELL` is unset, Linux safely falls back to `/bin/sh`.
- Windows cross-compiles from macOS and also builds natively on
  `windows-latest`. CI compiles the complete Rust test harness, runs the
  Windows-only named-pipe security tests, and performs a runtime smoke covering
  daemon auto-spawn, a 32-client named-pipe burst, pane create/list/send, agent
  streaming, and shutdown. CI produces an **unsigned NSIS installer**. Shells default to
  `%COMSPEC%`/`cmd.exe`. Agent panes work on Windows too
  (the `claude` binary is resolved via `claude.exe`/`claude.cmd`, `.cmd` shims
  spawn through `cmd /c`, and sessions are killed via TerminateProcess — direct
  child only, no graceful SIGTERM analog) and are covered by the same CI smoke
  with a fake driver. CI additionally builds and launches the native WinUI 3
  client, and uploads its portable ZIP and unsigned MSIX beside the Tauri NSIS
  artifact. Release builds are GUI-subsystem, so on Windows
  PowerShell 7 `sgian ctl` is not awaited (output still attaches to the
  console; use `Start-Process -Wait` when scripting exit codes, or cmd.exe).
- Tagged artifacts are not published unless the macOS, Linux, and Windows
  validation jobs have all passed, preventing a knowingly partial release.

### Frontend

The frontend is a React view over an explicit application controller. React owns
the pane tree, agent chat, settings, search, command palette, session overview,
profiles, and application chrome; a dedicated terminal controller owns
persistent xterm instances, output buffering, and serialized PTY resizing so UI
renders never restart a terminal. Pure ES modules still implement layout,
keyboard, focus, config, events, search, clipboard, zoom, settings, and agent
reducers. Vitest, Testing Library, and happy-dom cover both isolated app
instances and the complete Tauri command/event contract.

### Distribution

- MIT license (see [LICENSE](LICENSE)).
- `tauri-plugin-updater` + `tauri-plugin-process` for auto-updates.
- Signing config: entitlements and hardened runtime for macOS builds.
- Tag-gated CI release jobs for macOS DMG, Windows NSIS, Linux Debian/AppImage,
  updater signatures, and per-platform SHA-256 manifests.
- Updater stub feed for local testing.

## Control CLI

```bash
sgian ctl --help                 # full command list
sgian ctl panes                  # list panes (add --json for machine output)
sgian ctl new --name build       # create a pane
sgian ctl new --profile review   # create from a named shell/agent profile
sgian ctl new --agent reviewer   # create an agent pane (chat session)
sgian ctl new --agent fixer --backend droid --model claude-sonnet-4-5
sgian ctl send active "ls\n"     # send input (\n submits a line)
sgian ctl send reviewer "ship it"  # agent panes: text goes to the chat session
sgian ctl interrupt reviewer     # interrupt the agent's current turn
sgian ctl agent reviewer         # show agent + attention state (on/off to override)
sgian ctl exec --new -- cargo test
sgian ctl attach active          # stream a pane's output to stdout
sgian ctl shutdown               # stop this workspace's daemon
```

`active` follows the pane focused in the GUI (the GUI syncs focus changes to the
daemon). Pane references accept a pane id or title; exact ids win over titles.
Read-only commands (`panes`, `status`, `attach`, `logs`, `wait`, `snapshot`,
`find`) require a running daemon and never start one. `shutdown` also never
starts a daemon, but succeeds as a no-op ("no daemon running") when there is
none. The other commands start the workspace daemon on demand.

Orchestration across panes:

```bash
sgian ctl broadcast "git pull\n"     # send to every live pane
sgian ctl exec --all -- cargo build  # run a command in all panes
sgian ctl exec --panes build,test -- make
sgian ctl sync on                    # mirror typed input to all panes (off to stop)
sgian ctl run -- cargo test          # run a command and report its exit code
sgian ctl run --timeout 60000 -- make  # ... bounded: fail cleanly if no exit marker in 60s
sgian ctl process -- echo hi         # non-PTY argv with exact exit code
sgian ctl diagnostic                 # privacy-safe support bundle (JSON)
sgian ctl daemons                    # list workspaces and which daemons are running
sgian ctl shutdown --all             # stop every workspace daemon
```

Global options (`--workspace PATH`, `--json`) may precede the command, and also follow it
for commands that don't take free text.

## Configuration

Optional JSON config is merged from a global platform data directory and an
optional per-workspace override (workspace wins). The global path is
`~/Library/Application Support/Sgian/config.json` on macOS,
`$XDG_DATA_HOME/Sgian/config.json` (normally `~/.local/share/Sgian/config.json`)
on Linux, and `%APPDATA%\Sgian\config.json` on Windows:

Existing installations that already have a `Sgian2` data directory continue
using it automatically, so configurations, pane history, and live daemon
connections survive the rename. Clean installations use the paths above. If
both directories exist, `Sgian` wins; Sgian never merges them implicitly.

```json
{
  "shell": "/bin/zsh",
  "shell_args": ["-l"],
  "env": { "FOO": "bar" },
  "scrub_env": ["SECRET_KEY"],
  "font_family": "Menlo, monospace",
  "font_size": 13,
  "theme": { "background": "#0e0f0c", "foreground": "#e7ecd8" },
  "idle_shutdown_secs": 0,
  "restore_policy": "auto_respawn",
  "agent_permission_mode": "manual",
  "agent_claude_bin": null,
  "agent_droid_bin": null
}
```

All fields are optional. `theme` accepts any [xterm.js theme](https://xtermjs.org/docs/api/terminal/interfaces/itheme/)
keys (merged over the defaults). `idle_shutdown_secs` reaps the daemon after that many
seconds with no client connected (`0` disables it - the default tmux-style persistence).

`restore_policy` can be `"auto_respawn"` (default, revives ended panes on bootstrap) or
`"restore_on_demand"` (ended panes are restored but not auto-restarted until `ctl restart`).
`scrub_env` removes the listed env vars before spawning PTYs; explicit `env` values take
precedence over the scrub list.

`agent_permission_mode` sets the Claude Code permission mode for agent panes
(`"manual"` (default, approvals prompt in-app), `"acceptEdits"`, `"auto"`,
`"plan"`, `"dontAsk"`, or `"bypassPermissions"`). `agent_claude_bin` overrides how the
`claude` binary is located (default: `$SGIAN_CLAUDE_BIN`, then `$PATH`);
`agent_droid_bin` does the same for Factory Droid (default:
`$SGIAN_DROID_BIN`, then `$PATH`).

Shell profiles freeze their shell/args/env onto the pane at create time; that
snapshot is stored in `workspace.json` (`pane_shells`) so daemon restarts and
`ctl restart` keep the same override. Agent backend/model selections persist via
`agent_specs` as before.

## Packaged UI smoke

CI (and local packaged builds) can self-drive a short GUI smoke without WebDriver:

```bash
SGIAN_UI_SMOKE=1 SGIAN_WORKSPACE=/tmp/sgian-ui-smoke \
  SGIAN_UI_SMOKE_MARKER=/tmp/sgian-ui-smoke-ok \
  path/to/Sgian.app/Contents/MacOS/Sgian
# or: bash scripts/ui-smoke.sh path/to/Sgian.app /tmp/sgian-ui-smoke
```

When enabled, the app bootstraps, splits a pane, opens/closes settings, writes
terminal input, re-bootstraps, writes the marker file, and exits 0 (or 1 with a
`.err` sidecar on failure).

## Keyboard Shortcuts

The modifier (`mod`) is `cmd` on macOS and `ctrl` on other platforms.

- `mod+Shift+\` - split active pane into a row
- `mod+Shift+_` - split active pane into a column
- `mod+Shift+W` - close active pane
- `mod+1..9` - focus pane by index
- `mod+Shift+Arrow` - directional focus (left / right / up / down)
- `mod+Shift+Z` - toggle zoom
- `mod+Shift+S` - swap with partner pane
- `mod+Shift+F` - scrollback search
- `mod+Shift+C` / `mod+Shift+V` - copy / paste
- `mod+Shift+R` - rename pane
- `mod+Shift+N` - new agent pane
- `mod+Shift+P` - command palette (also `mod+K` when focus is not in a terminal)

Settings has no keyboard chord; it opens from the toolbar gear.

## Tests

```bash
# Backend (Rust)
cd src-tauri && cargo test          # full Rust suite
cd src-tauri && cargo clippy --all-targets -- -D warnings
cd src-tauri && cargo fmt --check

# Frontend (React/JavaScript)
npm test                             # full Vitest suite
npm run frontend:build               # production Vite bundle

# Packaged transport soak (Linux/macOS binary; Windows: scripts/transport-soak.ps1)
bash scripts/transport-soak.sh path/to/sgian /tmp/sgian-soak

# Cross-platform check
cd src-tauri && cargo check --target x86_64-pc-windows-gnu

# Linux CI additionally builds .deb/AppImage bundles and runs a release-binary
# daemon/agent smoke test on Ubuntu 22.04.
```

## Distribution

- `npx tauri build` produces a signed and notarized app when the following env
  vars are set: `APPLE_SIGNING_IDENTITY`, `APPLE_API_ISSUER`, `APPLE_API_KEY`,
  `APPLE_API_KEY_PATH`, `TAURI_SIGNING_PRIVATE_KEY`.
- The CI release job triggers on `v*` tags, imports the Developer ID cert,
  notarizes via `notarytool`, and uploads release artifacts.
- Updater: the app checks the updater feed once in the background on startup
  and, when an update is available, shows a banner offering one-click install
  + restart (the `install_update` command re-checks, downloads, installs, and
  relaunches). All check errors (offline, dead DNS, unsigned feed) are
  swallowed at debug level — the app is unaffected when the feed is down. The
  committed `tauri.conf.json` ships a production-safe
  `https://` updater endpoint with insecure transport disabled. The
  endpoint domain (`updates.sgian.dev`) MUST be provisioned and serving the
  signed feed before shipping a build: until it is, the startup check silently
  no-ops by design and no shipped build can ever discover an update — and a
  lapsed domain breaks updates for builds already shipped. The
  localhost stub feed (`http://localhost:8787` +
  `dangerousInsecureTransportProtocol`) lives in a dev/stub config overlay,
  `src-tauri/tauri.stub.conf.json`, merged via
  `npx tauri build --config src-tauri/tauri.stub.conf.json --bundles app`. For a real
  release, override the endpoint by supplying your own overlay or by editing
  the production endpoint in `tauri.conf.json` before the release build (no
  `dangerousInsecureTransportProtocol` is needed for an `https://` feed).
- Updater stub feed: `tools/updater-stub/setup-stub-feed.sh --build` builds with
  the stub overlay and serves the feed on `:8787` for local updater testing.
  Real hosting is deferred. Stub-overlay builds write a `STUB-BUILD` marker file
  next to the bundle output — an artifact accompanied by that marker points its
  updater at localhost over insecure transport and must never be released.
- Entitlements: `allow-jit` only (hardened runtime, no sandbox; library
  validation stays enabled).

See [ENHANCEMENTS.md](ENHANCEMENTS.md) for the prioritized roadmap.
