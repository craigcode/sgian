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
  `pane.ended { exit_code, agent, attention, mode, unattended, holder,
  output_tricks? }`, best-effort and unsynced (lease events stay fsynced).
  A closed pane's ledger is its full session record.
- **`ctl project dossier NAME [--lines N] [--out FILE]`.** One JSON document
  per project for a reviewer or a Kranz gate: the roll-up, each member
  pane's state, its full ledger with the chain verified, and the last N
  scrollback lines with citable numbers (`docs/design/execution-grants.md`).
- **`ctl agent --watch [PANE] [--json]`.** Streams agent-state, lease and
  pane-end transitions after a baseline line per pane, so CI, notification
  glue, or a mission orchestrator can react to needs-input without polling.

Shipped (M3b, 2026-09-14):

- **Official agent signals.** On Unix the daemon polls `claude agents --json`
  (every `agent_probe_interval_ms`, default 2000; `0` disables) and maps each
  session to the shell pane whose child process is its ancestor. While a
  reading is fresh it outranks screen classification, transitions are
  ledgered with evidence `claude-agents`, and a session that disappears from
  the listing for two rounds clears the badge (evidence
  `claude-agents: session gone`) instead of leaving a stale "claude · idle"
  over the shell prompt. Manual marks are untouched. Verified live against
  an interactive Claude Code session inside a pane.

Shipped (M4, lean, 2026-09-14):

- **Kranz-bound panes.** A shell pane whose process tree contains a Kranz
  worker loop (`kranz run` / `exec` / `work`) is bound to the mission at the
  pane's cwd; `ctl kranz bind [PANE] [--repo PATH]` binds by hand and
  `ctl kranz status` lists bindings. While bound, `kranz status --json`
  (read-only, no lock) drives the pane's badge as an official reading
  (`needs input` for a pending question, grant or revision, or a paused or
  blocked mission), and a released lease's hand-back note is mirrored into
  the mission inbox with `kranz msg`; `kranz.bound` / `kranz.mirrored` land
  in the ledger with the outcome. This uses the Kranz CLI in the sibling
  checkout rather than an HTTP client, so the daemon gains no async runtime;
  the WebSocket transcript tail and message mode from the draft remain open.

Shipped (2026-09-15):

- **Unattended-mode badge.** The permission mode is read off a Claude Code
  screen (`⏵⏵ auto mode on`, `⏵⏵ bypass permissions on`, `accept edits on`,
  `plan mode on`) and an agent pane's configured mode is overlaid at
  bootstrap; `AgentPaneInfo` gains `mode` and `unattended`, every client
  marks an unattended pane (amber ⚠ / shield), `ctl agent` prints
  `UNATTENDED`, and mode transitions are ledgered as `mode.changed`.
  Verified live by cycling modes with Shift+Tab in a real session.

Shipped (2026-09-15):

- **Scrollback search and citations.** `search_scrollback` and
  `scrollback_lines` requests, `ctl search <PANE> [-i] [-n N] <NEEDLE>` and
  `ctl lines <PANE> A:B`, over the whole scrollback file with CSI/OSC/DCS and
  other control sequences stripped so search sees what a person saw.
- **Output-flood regression test** with a measured 1.7 MB/s baseline (#18).

Shipped (2026-09-16):

- **Projects.** A named group of panes serving one goal (borrowed from the
  shape of Cursor's Projects, 2026-09-10, minus the cloud coordinator):
  persisted with the workspace, a pane in at most one, `project.assigned` /
  `project.unassigned` on the pane's ledger, an attention roll-up (panes,
  live, needs input, working, idle, unattended, keyboard holders), a
  per-pane detail view, and the members' ledgers merged in time order —
  the project-level "who did what" Cursor does not record. Daemon requests
  plus `ctl project …` and `ctl new --project`; a `projects_changed` event
  carries the whole table after any change, and the Tauri session overview
  is the board: one group per project with the roll-up in its heading,
  keyboard-holder and output-guard columns.
- **Output guard.** Per-pane counts of output tricks that hide content from
  a human (conceal, clipboard write, mismatched hyperlink, string controls,
  C1), rate-limited into the ledger (`output.suspicious`) and an
  `output_warning` event, in `find --json` and `ctl agent`
  (`HIDDEN-OUTPUT …`), and in the bootstrap snapshot's `output_warnings`.

- **Served view (M7).** `ctl serve` hosts the web client on loopback for
  a device that reaches the machine over `ssh -L`: the same controller over
  fetch + SSE, read-only unless `--allow-write`, acting as the process's
  credential so every action is attributed (`docs/design/served-view.md`).
- **Per-client identity (M6).** `ctl identity issue|list|revoke` mints
  bearer credentials with `read`/`write`/`admin` scopes; a credential fixes
  its holder (the daemon rewrites unattributed input to it and refuses any
  other `--as`), rides the hello as `client_token`, and lands as
  `credential` on lease ledger records. `identity: required` turns the
  workspace token into read+admin so every write is attributed. Peer-uid
  check on the socket. Remote is an SSH-forwarded socket, never a listener.
  Kranz runs are their own holders; there is no impersonate scope.
- **Usage from the status line.** `ctl statusline` is the command Claude
  Code's status line runs: it records the session's model, context fill and
  rate-limit windows against the pane that owns the calling process (same
  process-tree placement as hooks), then prints the user's own status
  command's output or a compact default so nothing is lost. `agent_usage`
  in the snapshot and as an event; one shared summary line across `ctl
  agent`, the Tauri badge and overview, the macOS row and the Windows
  subtitle; the freshest rate-limit line per project heading. Push, not
  poll: no credentials, no scraping. Enjoy's "usage left, resets at"
  without its account coupling.
- **Hook ingestion.** `ctl hook` is the command a Claude Code hook runs:
  it reads the payload from stdin, finds the pane that owns the calling
  process by walking its ancestry (this workspace's daemon first, then
  every running daemon) and applies the hook as an official reading
  (Notification → needs input, `UserPromptSubmit`/`PreToolUse` → working,
  `Stop` → idle; evidence `hook`, 20 s over the screen heuristic;
  `hook.received` with the message for Notifications). Sub-second
  needs-input without polling. Kranz-bound panes need no relay: the run's
  own `kranz hook-status` hooks post to Kranz's lane.
- **Lease generations.** Every held lease carries a monotonic generation
  (never repeated across restarts); `send --generation N` and
  `lease release --generation N` are refused as stale once the lease has
  changed hands, so a previous holder's late command cannot land on the
  current holder's session. `docs/design/execution-grants.md` records the
  Kranz-authorizes / Sgian-executes boundary agreed with the Kranz side.

Open, in order:

- **Execution grants** (`docs/design/execution-grants.md`): the Sgian half
  once `kranz-acp` and Kranz's `TerminalProvider` seam exist.
- **Output-guard badges and project rendering in the native clients**
  (the Tauri client has both: `⚠ N` badge, overview grouped by project with
  the roll-up); stripping for agent panes; per-project shared context notes
  under git.
- **M5 SSM / ECS Exec target**.

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
