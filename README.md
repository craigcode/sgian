# Sgian

Sgian is a keyboard-driven pane manager and terminal multiplexer with native
macOS and Windows clients, a Tauri Linux client, and a shared Rust daemon.
The daemon (`--daemon`) and scriptable control CLI (`ctl`) keep sessions
independent of the desktop client. Closing the GUI leaves your shells running,
tmux-style.

Panes come in two kinds: **shell** panes (regular PTY terminals) and **agent**
panes — chat-native sessions driving headless Claude Code or Factory Droid
processes with streaming replies, tool-call cards, and in-app permission prompts.
Agent sessions are daemon-owned too, so they survive the GUI closing and resume
where they left off. Shell panes running agent TUIs (e.g. the interactive
`claude` CLI) are auto-detected and badged by attention state
(working / needs input / idle).

Supported native desktop targets are macOS 13+, Windows 10 version 1809+ (ConPTY),
and Linux distributions compatible with the Ubuntu 22.04/WebKitGTK 4.1 build
baseline. Every release is gated by native builds on all three platforms.

## Run

```bash
npm ci --ignore-scripts
npm run dev
```

Use Node.js 22.12+ and the Rust toolchain pinned in `rust-toolchain.toml`
(with rustup ahead of any system Rust installation on PATH).

The React/Vite frontend lives in `ui/`; the Tauri backend lives in `src-tauri/`.
Tauri starts Vite automatically in development and builds `dist/` for packaged
applications. Linux development additionally requires Tauri's
[documented system dependencies](https://v2.tauri.app/start/prerequisites/#linux).

### Native macOS and Windows clients

The macOS client in `apps/macos` uses SwiftUI/AppKit and the same
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

CI packages and launches the native app against a temporary daemon, then
uploads an architecture-labelled `sgian-macos-native-*` ZIP. Those validation
artifacts use ad-hoc signing. Production native signing/notarization and updater
feeds are wired through the separate draft release workflow.

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
  `env` values take precedence over the scrub list. Claude Code's own
  child-session markers (`CLAUDECODE`, `CLAUDE_CODE_CHILD_SESSION`,
  `CLAUDE_CODE_ENTRYPOINT`) are always dropped, so a daemon started from
  inside a Claude Code session does not make every pane's `claude` a child
  session with transcripts off.
- Closing a pane (or the daemon exiting) terminates the pane's whole process
  tree, not just its shell: descendants are found without forking (libproc on
  macOS, /proc on Linux) and signalled (SIGTERM, then SIGKILL after a grace
  period); on Windows the child is held in a kill-on-close Job Object.
- Config file-watch with live reload.
- Bounded log rotation so logs do not grow unbounded.
- Agent awareness (shell panes): the daemon classifies a pane's screen for known
  agent TUIs (Claude Code) and broadcasts `agent-state` transitions
  (working / needs input / idle), badged on the pane's tab. On Unix it also
  polls `claude agents --json` (`agent_probe_interval_ms`, default 2000) and
  maps sessions to panes through the process tree, so Claude Code's own
  state outranks screen scraping while fresh and a finished session clears
  the badge. The agent's permission mode is read off the same screen
  (`auto`, `bypass`, `accept-edits`, `plan`); a pane whose agent runs tools
  without approval is flagged `unattended` in every client, in `ctl agent`,
  and in the ledger as `mode.changed`, so an unattended run is never
  invisible. Agent-kind panes carry their configured permission mode the
  same way.
- Keyboard lease: one holder per pane. While a pane is held, input from
  anyone else is refused (`lease_policy: "open"`, the default) or every write
  needs the lease (`"required"`). Takeovers, forced revocations, and releases
  are appended to a per-pane hash-chained ledger with a mandatory hand-back
  note; keystrokes are never recorded, only counts. The lease is a
  coordination and audit record: every client still shares one workspace
  token, so it is not yet a security boundary. See
  [docs/design/keyboard-lease-and-ledger.md](docs/design/keyboard-lease-and-ledger.md).
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
- `ctl lease take|release|status` to claim, hand back (with a note), or show a
  pane's keyboard lease; `ctl send --as HOLDER` attributes input to a holder.
  In the clients: `Ctrl/Cmd+Shift+T` takes the active pane's keyboard,
  `Ctrl/Cmd+Shift+L` releases it with a note.
- `ctl ledger [PANE] [--verify]` to print or verify a pane's hash-chained
  ledger (lease handovers, attention transitions, pane exits); a closed
  pane's ledger stays readable by id.
- `ctl agent --watch [PANE] --json` to stream agent-state, lease and pane-end
  transitions to a script instead of polling.
- `ctl project new|list|show|add|rm|delete|ledger` for projects: a named group
  of panes serving one goal, persisted with the workspace, with an attention
  roll-up (needs input / working / idle / unattended / keyboard holders) and
  the member panes' ledgers merged in time order. `ctl new --project NAME`
  creates a pane straight into one.
- Output guard: every pane's output is scanned for the tricks an agent can
  use to hide things from the person watching (SGR 8 conceal, OSC 52
  clipboard writes, OSC 8 hyperlinks whose visible text is a URL on another
  host, DCS/APC/PM/SOS strings, raw C1 controls). Counts are per pane, land
  in the ledger as `output.suspicious` (rate-limited), ride an
  `output_warning` event, and show as `HIDDEN-OUTPUT` in `ctl agent`.
  Ordinary redraws are never counted.
- `ctl search <PANE> [-i] [-n N] <NEEDLE>` for a substring search over a pane's
  whole scrollback with control sequences stripped, and `ctl lines <PANE> A:B`
  to print the cited range, so a ledger record or a search hit can point at
  exact output lines.
- `ctl kranz status|bind|unbind` for panes bound to a Kranz mission: the
  mission's pending questions and grants drive the badge, and a hand-back
  note is mirrored into the mission inbox with `kranz msg`.
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
  native release candidates are signed, notarized, and packaged as a DMG.
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
- Tagged artifacts are staged until all validation and signing jobs pass.
  A final job verifies updater signatures against the embedded public key,
  assembles all platforms and the update feed, uploads to a draft, then publishes.

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

The native SwiftUI macOS and WinUI Windows clients are the shipping targets;
Linux retains the Tauri client. The **Native release candidate** workflow runs
from protected `main`, validates the complete source, signs/packages all three
platforms, and assembles a draft release for acceptance before publication.

- macOS: universal Apple Silicon/Intel app, Developer ID signing, notarized DMG,
  Sparkle signed updates and a native Check for Updates menu.
- Windows: signed x64 MSIX installed through `Sgian.appinstaller`, with Windows
  App Installer updates and a manual check in Commands.
- Linux: Debian package and AppImage with the Tauri signed updater.

Production credentials and installed version-to-version acceptance remain
required before public distribution. Local ad-hoc/unsigned builds and portable
CI ZIPs are development artifacts. See [the native release runbook](docs/native-release.md)
for build commands, signing configuration, release promotion and recovery.

See [ENHANCEMENTS.md](ENHANCEMENTS.md) for the prioritized roadmap.

See [SECURITY.md](SECURITY.md) for the security model and private reporting,
and [the release review](docs/release-review-2026-09-04.md) for verified fixes,
remaining limitations, and the public-release checklist.
