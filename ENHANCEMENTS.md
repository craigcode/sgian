# Sgian — Enhancement Roadmap

This roadmap lists work that remains after the React migration and the macOS,
Linux, and Windows platform pass. Completed capabilities are documented in the
[README](README.md); they are not repeated here as future work.

## 1. Windows acceptance and hardening (release blocker)

The codebase now has a native Windows build, per-user named-pipe transport,
Windows agent panes, and a native WinUI3/MSIX client. The Tauri NSIS
installer remains a compatibility validation target. The current branch also:

- creates ConPTY with the same default flags as node-pty/VS Code instead of
  portable-pty's undocumented resize/input flags;
- treats `ERROR_PIPE_BUSY` as a bounded listener hand-off race and recreates
  unlimited owner-restricted pipe instances correctly; and
- exercises a 32-client named-pipe burst in native Windows CI.

What remains is acceptance on the Windows machine that originally reproduced
the failures:

- Run Claude Code's full-screen TUI from the packaged app at several pane sizes,
  including repeated splits and resizes. Confirm that borders no longer
  staircase and that no row loses its first character.
- Repeat GUI attach/detach, pane creation, and concurrent `ctl panes` bursts.
  Confirm that error 231 does not recur and that the existing daemon remains
  responsive.
- If the TUI still staggers, capture the raw `.ansi` stream and run the same
  Claude build in VS Code's terminal. A clean VS Code control isolates the
  remaining difference to Sgian/ConPTY configuration; corruption in both
  terminals points to Claude Code/Ink.
- Keep the vendored portable-pty fork minimal and remove it as soon as upstream
  exposes supported ConPTY flags or adopts the default-zero behavior.

## 2. Release engineering

Native macOS (SwiftUI/AppKit/SwiftTerm) and Windows (WinUI3/WebView2 terminal)
are the shipping clients. Linux continues to use Tauri. See the
[native release runbook](docs/native-release.md) for the candidate workflow,
signing configuration, installation/update acceptance, and recovery.

- Provision the `native-release` signing environment and stable Sparkle public
  key. No production signing credentials are stored in this repository.
- Run the manual candidate workflow from protected main. It builds a notarized
  universal macOS DMG, signed Windows MSIX/App Installer, and signed Linux
  packages, and assembles a draft only after all platform artifacts validate.
- Install each candidate on clean machines and exercise an installed-version
  update. Complete the native acceptance checklist before publishing the draft.
- Ensure published GitHub release downloads are accessible to customers. This
  private repository's unauthenticated update URLs require a public release
  channel before external distribution.

## 3. Agent and orchestration improvements

Shipped:

- **Structured multi-pane results.** Batched `ctl run` / exec reports per-pane
  exit code, timeout, elapsed time, and a bounded output tail in text and JSON.
  `--timeout` also bounds setup (status/subscribe) before the run loop.
- **Non-PTY execution channel.** `ctl process [--cwd DIR] [--timeout MS] --
  <argv…>` runs exact argv outside a PTY with concurrent bounded stdout/stderr
  capture, process-group kill on timeout (Unix), and null exit code + signal
  metadata when the child is signal-terminated.
- **Agent diagnostics.** `ctl diagnostic` exports a support bundle (versions,
  pane/agent state, denylist-scrubbed log tail) without prompts, scrollback, or
  environment values. Scrubbing is best-effort, not a cryptographic guarantee.

## 3a. Keyboard lease and session ledger

Design and milestones: [docs/design/keyboard-lease-and-ledger.md](docs/design/keyboard-lease-and-ledger.md).

Shipped (M1, 2026-09-13):

- **Per-pane write lease in the daemon.** `take_lease` / `release_lease` /
  `lease_status` requests, `send_input_as` for attributed input, a
  `lease-state` event, `leases` in the bootstrap snapshot, persistence in
  `workspace.json`, and `lease_policy` (`open` default / `required`) in config.
- **Hash-chained ledger** at `ledger/<pane-id>.jsonl` (SHA-256 over
  sorted-key JSON with a version prefix), fsynced per record, kept across
  pane close. `ctl ledger --verify` names the first broken line.
- **ctl surface**: `lease`, `lease take`, `lease release -m`, `ledger`,
  `send --as`.

Shipped (M2, 2026-09-14):

- **Clients.** All three clients write as `user@host` (the `ctl` default, so
  the operator is one principal across surfaces) via `send_input_as`, with a
  fallback to `write_to_pane` against a pre-lease daemon. The pane header
  shows the holder (`you` when it is this client), a refused keystroke shows a
  transient read-only notice instead of an error, `Ctrl/Cmd+Shift+T` takes
  (opening a why prompt when someone else holds it) and `Ctrl/Cmd+Shift+L`
  releases with a mandatory note. Tauri: vitest; macOS: `swift test` plus the
  live daemon round trip; Windows: compiled and exercised only by the Windows
  CI job (no .NET toolchain on the development Mac).

Shipped (M3a, 2026-09-14):

- **Attention in the ledger.** Every agent-attention transition is appended
  as `attention.changed { agent, from, to, evidence }` (`screen` today,
  `process ended` when the pane's process exits) and every pane exit as
  `pane.ended { exit_code }`, best-effort and unsynced (lease events stay
  fsynced). A closed pane's ledger is its full session record.
- **`ctl agent --watch [PANE] [--json]`.** Streams agent-state, lease and
  pane-end transitions after a baseline line per pane, so CI, notification
  glue, or a mission orchestrator can react to needs-input without polling.

Open, in order:

- **M3b official agent signals.** Poll `claude agents --json` (pid, cwd,
  status) and map entries to panes through the process tree so the daemon
  prefers Claude Code's own state to screen scraping while the probe is fresh;
  Notification-hook ingestion and the relay to Kranz's hook-status lane
  follow once a pane can be bound to a run.
- **M4 Kranz target**, **M5 SSM / ECS Exec target**, **M6 per-client
  identity** (the lease becomes a boundary).

## 4. Workbench UX

Shipped:

- **Command discovery.** Command palette via `⌘/Ctrl+Shift+P` always, and
  `⌘/Ctrl+K` when focus is not inside a terminal (so shell kill-line stays
  intact). Combobox + `aria-activedescendant` for the filtered list.
- **Profiles.** Named shell/agent profiles in config/settings; toolbar select
  and palette “New pane with profile”; `ctl new --profile NAME`. Shell profiles
  apply shell/args/env at create time as a frozen per-pane override; that
  snapshot persists across daemon restarts and in-process pane restarts.
  Agent profiles select backend/model (already persisted via `agent_specs`).
  Contradictory kind/field mixes are rejected.
- **Session overview.** Compact workspace view for pane type, runtime state,
  and activity since this GUI attach, with focus / restart / close actions.
  Closing a pane keeps focus inside the overview dialog.
- **Modal accessibility.** Dialog roles, labelled titles, Tab focus traps, Escape
  close, and focus restoration for settings, palette, and session overview.
- **Keyboard split resize.** Separators expose `role="separator"` with arrow-key
  resize and `aria-valuenow`; Vitest covers keyboard resize and modal Tab traps.

Still open (manual):

- Real assistive-tech validation (VoiceOver / Narrator / Orca) on packaged
  builds across macOS, Windows, and Linux — not CI-automatable in this suite.

## 5. Reliability and test depth

Shipped:

- Framed IPC fault coverage (truncated/oversized/stalled frames); subscriber
  connect/disconnect soak returns to a zero baseline; `ctl process` unit
  coverage; palette + large-layout filter/reconcile performance budgets in
  Vitest; bootstrap/reattach wall-clock baseline in Rust.
- **Long-running transport soak.** `scripts/transport-soak.{sh,ps1}` run
  multi-round 32-client bursts with pane create/restart/send and loose
  handle/thread checks; wired into Linux and Windows CI after the packaged
  daemon smoke.
- **Packaged-app UI smoke.** Env-gated self-test (`SGIAN_UI_SMOKE=1`) splits,
  opens/closes settings, writes terminal input, re-bootstraps, writes a marker,
  and exits. CI launches the packaged macOS `.app`, Linux binary under `xvfb`,
  and Windows NSIS-installed `sgian.exe` via `scripts/ui-smoke.{sh,ps1}`.
- **Broader fault injection.** Daemon death mid-agent-turn recovers on restart;
  Windows listener left handle-less after recreate failure fails the next accept
  cleanly; updater check classification skips network/signature errors without
  panicking.

## 6. Competitive positioning — Warp scan candidates (2026-08-04)

Source: [REVIEW-2026-08-04-warp.md](REVIEW-2026-08-04-warp.md) — Warp's
standalone Agent CLI launched 2026-08-04, planting a funded incumbent
directly in sgian's lane (persistent sessions + supervised agents).
Sgian's defensible ground: local-first (no account, no cloud, MIT) and a
scriptable control plane the Warp CLI lacks entirely (no headless mode, no
JSON, no hooks at launch). Candidates, cheapest first — none committed:

- **Lead with the control plane.** README/positioning currently leads with
  panes; `ctl --json` + exact exit codes + bounded runs is the
  differentiator no competitor shipped. A short "drive sgian from scripts"
  doc section (or demo) makes it legible.
- **Local-first positioning statement.** One paragraph: no account, no
  cloud, transcripts never leave the machine. The OpenWarp fork's traction
  (209 HN points for "Warp without the cloud") is the demand evidence.
- **Permission-visibility pass.** Warp shipped auto-approve bypassing its
  denylist by default and replace-not-extend denylists. Sgian's defaults
  are safer, but: badge any pane running under `auto` / `dontAsk` /
  `bypassPermissions` so unattended modes are always visible, and audit
  config surfaces for replace-vs-extend semantics (`scrub_env` vs `env`
  precedence is documented; make the audit deliberate, not assumed).
- **Machine-readable agent-state stream.** The daemon already broadcasts
  working / needs-input / idle transitions; expose them to scripts
  (`ctl agent --watch --json` or an equivalent subscription) so external
  tooling — CI, notification glue, or a mission orchestrator like kranz —
  can react to needs-input without polling.
- **Additional agent backends.** Codex / Gemini CLI panes behind the same
  normalized event stream would strengthen the vendor-neutral claim that
  Warp's own-agent-first launch makes newly legible. Medium effort; only
  worth it once §1 clears.

## Prioritization note

Windows acceptance (§1) remains the release blocker for a public ship —
and gains urgency from the Warp scan: Warp's CLI ships Windows day one.
The Warp client's now-open ConPTY handling is a directly relevant
reference for §1 (study only — AGPL; sgian is MIT).
