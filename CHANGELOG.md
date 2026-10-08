# Changelog

All notable changes to Sgian. The format follows Keep a Changelog; versions
follow SemVer once the first public tag exists.

## Unreleased

### Project

- The README shows the macOS client and the served phone view, captured
  from a real session with two Claude Code agents.
- The Windows release job can sign through Azure Trusted Signing when no
  PFX certificate is set: it logs in with an OIDC identity, installs
  Microsoft's signtool plug-in, takes the MSIX publisher from a variable
  and signs the package after building it. The PFX path is unchanged.
  CI also runs weekly on `main`, so a dependency advisory published between
  pushes fails a run instead of waiting for the next change.
- `docs/release-signing-setup.md`: a step-by-step for obtaining the
  eleven signing secrets and the Sparkle public key, including the
  Windows certificate decision the workflow's PFX import now forces.
- `scripts/package-macos-native.sh` gains a rehearsal mode
  (`SGIAN_PACKAGE_REHEARSAL=1`) that runs the bundle checks, the DMG, the
  Sparkle appcast and its verification with ad-hoc signing and no
  notarization, writing under `build/rehearsal`; the first run proved the
  pipeline end to end before any signing credential exists.

### Fixes

- The vendored `portable-pty` fork drops its serial-port module and with it
  the `serial` crate, unmaintained since 2017; Sgian never opened serial
  ports. `.cargo/audit.toml` records the two advisories that remain
  accepted on purpose, each with its reason and removal condition, so a
  new audit warning fails CI instead of joining a known list.
- `ctl send` treats a real line feed in the text as Enter, like the `\n`
  escape, so `sgian ctl send pane $'text\n'` submits to a shell and to a
  full-screen agent input alike; `--lf` and `--raw` still send it as given.
- Panes the native clients did not place themselves (created by `ctl`,
  Kranz or another client) share the width evenly instead of each new one
  taking half and squeezing those before it.
- Windows: an agent profile chosen from the command palette opens an agent
  pane instead of failing; the empty state follows the connection status
  and offers Choose workspace when a workspace is missing; the output
  badge keeps its "first seen" text across a refresh; a stale workspace
  picked while connected no longer shows "Connection failed"; a launch
  from the Start menu opens the user profile rather than the system
  directory; and the unattended flag is derived for an older daemon's
  snapshot as it already was for its events.

## 0.1.0 - 2026-09-30

The first public release, published as source: build it from the `v0.1.0`
tag with the three commands in the README. Signed and notarized installers
with automatic updates follow in a later release, once the signing
credentials exist and an installed update has been exercised on each
platform. Everything below is new relative to the private prototype that
preceded it.

### Fixes

- Saving Settings no longer resets `lease_policy`, `identity`, the agent
  probe interval or the Kranz binary path: `get_config` returns them and
  `write_config` keeps any key the payload omits, replacing only what it
  carries (#32).
- Agent profiles added from the macOS and Windows Settings now save; both
  forms wrote `backend` and `model` where the daemon expects
  `agent_backend` and `agent_model` (#31).
- The web client's Settings form cannot be saved before the configuration
  has loaded, and an edit made while it loads no longer leaves the other
  fields empty (#33).

- Windows agent panes join a kill-on-close job before their code runs; closing,
  restarting or discarding a pending pane terminates its child process tree.
  A failed job assignment refuses the launch.
- Workspace startup checks the previous cwd marker before replacing it and
  refuses corrupt workspace state with no usable marker.
- Revoking a client credential also closes subscriptions still being registered;
  regression checks require connection closure rather than accepting a timeout.

### Clients

- Native macOS (SwiftUI/SwiftTerm) and Windows (WinUI 3) clients beside the
  Tauri Linux client: split workspaces, settings, platform update
  integration and a signed release pipeline (#13).
- A session overview that groups panes by project with an attention roll-up
  per heading (needs input, working, unattended, keyboard holders, output
  warnings) in all three clients, following a `projects_changed` event.
  Output-guard hits show as an amber badge on the pane tab and header; the
  macOS sidebar marks them with an eye-slash icon.
- Per-pane usage beside every pane: the agent's model, context fill and
  rate-limit windows from `ctl statusline`, amber past 80%, with the
  freshest limit line under each project heading.
- Keyboard lease UI: a holder badge, a read-only notice when typing is
  refused, `Ctrl/Cmd+Shift+T` to take a pane and `Ctrl/Cmd+Shift+L` to
  release it with a note (#15).
- Windows and the web client: a saved workspace that no longer exists is
  forgotten at launch in favour of the most recent one that does and the
  empty state offers a Choose workspace button (Windows); every terminal is
  a named group for assistive technology labelled by pane title, with
  xterm's screen-reader mode as a per-client preference (Settings on
  Windows, the command palette in the web client) because it costs on heavy
  output; the output badge names the first opaque string seen; and both
  derive the unattended flag from the mode for an older daemon.
- macOS: a saved workspace path that no longer exists is forgotten at
  launch in favour of the most recent one that does, and the empty state
  offers Choose Workspace instead of a dead spinner; the terminal is a text
  area to accessibility clients, labelled by pane title, with the visible
  screen as its value.
- Client credentials stored per workspace in the macOS login Keychain or
  the Windows credential vault behind Settings → Identity, with a Forget
  button; `SGIAN_CLIENT_TOKEN` overrides when set. A scope refusal shows as
  the read-only notice.
- `ctl serve [--port N] [--allow-write]`: the web client on a loopback port
  for a phone or laptop over an SSH tunnel. Read-only by default, acts as
  the process's credential, never allows admin, embeds its assets, checks
  the Host header, requires a per-run session key (once in the URL, then a
  cookie) and refuses beyond 64 open connections. The page lands on the
  overview and stacks it as cards at phone width.

### Agents

- Official agent signals: `claude agents --json` polled and mapped to panes
  through the process tree, outranking screen scraping while fresh; a
  finished session clears the badge (#15).
- `ctl hook`: the command a Claude Code hook runs. It finds the pane that
  owns the calling process and sets its badge from the hook (Notification →
  needs input, `UserPromptSubmit`/`PreToolUse` → working, `Stop` → idle)
  with evidence `hook`; Notifications are ledgered as `hook.received`. The
  README carries the settings snippet.
- `ctl statusline [--exec CMD]`: the command Claude Code's status line runs.
  It records the session's model, context fill and rate-limit windows
  against the owning pane, then prints your own status line or a compact
  default; `agent_usage` snapshot map and event.
- Unattended-mode badge: the permission mode read off the agent's screen
  (`auto`, `bypass`, `accept-edits`, `plan`) or an agent pane's configured
  mode, flagged `unattended` in every client, in `ctl agent` and in the
  ledger (#17).
- Output guard: per-pane counts of SGR 8 conceal, OSC 52 clipboard writes,
  mismatched OSC 8 hyperlinks, DCS/APC/PM/SOS strings and raw C1 controls,
  ledgered and shown as `HIDDEN-OUTPUT` in `ctl agent` (#21). Terminal
  capability traffic (a terminal's XTVERSION, DECRQSS and XTGETTCAP replies
  echoed before an application goes raw, the queries themselves, and the
  kitty graphics support query) is not counted, so an agent's startup no
  longer trips the badge; the `output.suspicious` record and the
  `output_warning` event name the first opaque string that did count.
- `agent_state` events always carry `unattended`, so a live permission-mode
  transition shows the unattended shield in the native clients without
  waiting for a reattach; the macOS client also derives it from `mode` for
  an older daemon.

### Identity and keyboard leases

- Per-client identity: `ctl identity issue|list|revoke` and `ctl whoami`.
  Credentials carry `read`/`write`/`admin` scopes and fix the holder the
  daemon attributes input and leases to; `client_token` on the hello lets
  a second machine connect over an SSH-forwarded socket without the
  workspace token; `identity: required` makes every write need a
  credential; connections from another uid are dropped.
- Keyboard leases: one holder per pane, `lease_policy` open or required, a
  mandatory hand-back note, per-lease generations that refuse a previous
  holder's late command as stale, and `ctl lease` / `ctl send --as`
  (#15, #22). Agent prompts, approvals and interrupts honour the lease for
  a credentialed connection.
- Revoking a credential cuts its live connections at their next request,
  refuses new subscriptions and ends the event streams it opened.
- Kranz worker runs identify themselves with their own `kranz:<run-id>`
  write credential, issued and revoked by Kranz around each session, so a
  run's panes, leases and ledger records name the run rather than the
  operator.

### Ledger and projects

- A hash-chained, fsynced per-pane ledger (`ledger/<pane>.jsonl`) recording
  lease handovers, attention and permission-mode transitions, pane exits
  with the agent's state at exit, project membership, Kranz mirroring,
  hook notifications and output-guard hits; `ctl ledger --verify` names
  the first broken line (#15, #17, #20, #21).
- Projects: named pane groups with an attention roll-up and the members'
  ledgers merged in time order; `ctl project …`, `ctl new --project` (#20).
- `ctl project dossier NAME [--lines N] [--out FILE]`: one JSON document per
  project (roll-up, each pane's state, the verified ledger, the last N
  scrollback lines with citable numbers) for a reviewer or a Kranz gate.
- Kranz-bound panes: `kranz run` under a pane binds it to its mission,
  `kranz status --json` drives the badge, released notes are mirrored with
  `kranz msg`; `ctl kranz bind|unbind|status` (#15).
- `ctl search` and `ctl lines` over a pane's scrollback with control
  sequences stripped, for citing exact output lines (#19), and
  `ctl agent --watch --json` streaming agent-state, lease and pane-end
  transitions (#15).

### Daemon

- Closing a pane, or the daemon exiting, terminates the pane's whole
  process tree, found without forking (libproc on macOS, /proc on Linux)
  and held in a kill-on-close Job Object on Windows (#15).
- Pane spawns drop Claude Code's child-session markers inherited from the
  daemon's environment; a config reload updates the scrub list profiled
  panes restart with (#15).
- The workspace-key collision guard survives a corrupt `workspace.json`
  via a `workspace.cwd` marker; the daemon socket is created under an
  owner-only umask; the pane input queue is capped at 8 MiB of unwritten
  bytes; IPC line writes are a single syscall and transient `openpty`
  failures are retried briefly (#15).
- An output-flood regression test with a measured throughput baseline (#18).

### Security

- Trust-boundary hardening across the daemon, native bridges and release
  automation (#11); vitest 5.0.0 for GHSA-82fw-gwwq-j7x9 and rustls
  0.23.45 for RUSTSEC-2026-0285 (#15).
- Trust-surface review (`docs/trust-surface-review-2026-09-19.md`): hook
  and status-line reports need `write` from a credential, the legacy
  `write_to_pane` request is bound to the credential's holder, ledgered
  hook strings and status-line names are bounded, `ctl hook` and
  `ctl statusline` cap stdin at 1 MiB, and `clients.json` is rewritten at
  most once a minute.
- Full-repo review (`docs/review-2026-09-20.md`): all fourteen findings
  closed, including the served view's session key, lease enforcement for
  credentialed agent control, a peer uid that cannot be read being refused
  on macOS and Linux, and the web client dropping state for panes that
  vanish on resync.

### Project

- `CONTRIBUTING.md`, issue and pull-request templates, and a README that
  opens with what Sgian is, how to install it and a first five minutes.
