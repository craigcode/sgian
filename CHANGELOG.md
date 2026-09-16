# Changelog

All notable changes to Sgian. The format follows Keep a Changelog; versions
follow SemVer once the first public tag exists.

## Unreleased

### Added

- `ctl project dossier NAME [--lines N] [--out FILE]`: one JSON document per
  project (roll-up, each pane's state, full ledger with the chain verified,
  last N scrollback lines with citable numbers) for a reviewer or a Kranz
  gate. `pane.ended` ledger records now carry the agent, its attention at
  exit, mode, unattended flag, keyboard holder and output-guard totals.
- Keyboard leases: one holder per pane, `lease_policy` open/required, a
  mandatory hand-back note, per-lease generations that refuse a previous
  holder's late command as stale, and `ctl lease` / `ctl send --as` (#15, #22).
- A hash-chained, fsynced per-pane ledger (`ledger/<pane>.jsonl`) recording
  lease handovers, attention and permission-mode transitions, pane exits,
  project membership, Kranz mirroring and output-guard hits; `ctl ledger
  --verify` names the first broken line (#15, #17, #20, #21).
- Lease UI in all three clients: holder badge, read-only notice when typing
  is refused, `Ctrl/Cmd+Shift+T` to take and `Ctrl/Cmd+Shift+L` to release
  (#15).
- Official agent signals: `claude agents --json` polled and mapped to panes
  through the process tree, outranking screen scraping while fresh; a finished
  session clears the badge (#15).
- Unattended-mode badge: the agent's permission mode read off the screen
  (`auto`, `bypass`, `accept-edits`, `plan`) or an agent pane's configured
  mode, with `unattended` flagged everywhere (#17).
- Kranz-bound panes: `kranz run` under a pane binds it to its mission;
  `kranz status --json` drives the badge and released notes are mirrored with
  `kranz msg`; `ctl kranz bind|unbind|status` (#15).
- Projects: named pane groups with an attention roll-up and the members'
  ledgers merged in time order; `ctl project …`, `ctl new --project` (#20).
- Output guard: per-pane counts of SGR 8 conceal, OSC 52 clipboard writes,
  mismatched OSC 8 hyperlinks, DCS/APC/PM/SOS strings and raw C1 controls,
  ledgered and shown as `HIDDEN-OUTPUT` in `ctl agent` (#21).
- `ctl search` and `ctl lines` over a pane's scrollback with control
  sequences stripped, for citing exact output lines (#19).
- `ctl agent --watch --json` streaming agent-state, lease and pane-end
  transitions (#15).
- Native macOS (SwiftUI/SwiftTerm) and Windows (WinUI 3) clients, split
  workspaces, settings, platform update integration and a signed release
  pipeline (#13).
- An output-flood regression test with a measured throughput baseline (#18).

### Changed

- Closing a pane (or daemon exit) terminates the pane's whole process tree,
  found without forking (libproc on macOS, /proc on Linux) and held in a
  kill-on-close Job Object on Windows (#15).
- Pane spawns drop Claude Code's child-session markers inherited from the
  daemon's environment (#15).
- IPC line writes are a single syscall; transient `openpty` failures are
  retried briefly (#15).

### Security

- Trust-boundary hardening across the daemon, native bridges and release
  automation (#11); vitest 5.0.0 for GHSA-82fw-gwwq-j7x9 and rustls 0.23.45
  for RUSTSEC-2026-0285 (#15).
