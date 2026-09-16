# Keyboard lease and session ledger

**Status:** design accepted for implementation; M1 in progress
**Date:** 2026-09-13
**Supersedes:** the standalone "Sgian v0.1 — Design" draft (2026-09-13) where the two differ

## 1. What the v0.1 draft got right, and what it got wrong

The draft's claim stands: no surveyed tool puts human/agent keyboard ownership
*in the pane* with a durable record of every handover. A fresh scan on
2026-09-13 (Warp/Oz, Conductor, Claude Squad, CAO, AgentsRoom, Zellij 0.45,
tmux 3.8 RC, iTerm2 3.7, Cursor CLI, Codex app-server, OpenCode, Devin)
confirms it: "takeover" everywhere means attach-to-tmux or push-to-cloud.
Nobody has a per-pane write lease or a mandatory hand-back note.

The draft's premise about this repository is wrong. It assumes a greenfield
four-crate attach client and says the existing code may be replaced. What
exists is a shipped multiplexer: a ~31k-line Rust daemon with a `ctl --json`
control plane, native SwiftUI and WinUI 3 clients, a Tauri Linux client,
ConPTY through a vendored `portable-pty` fork, persisted per-pane scrollback
with catch-up on attach, advisory `flock` daemon locking, Claude and Droid
agent panes with permission round-trips, screen-derived agent attention
badges, two deep reviews, and a release-hardening pass. Most of the draft's
M0–M3 already exist here in some form:

| Draft item | Already in the daemon |
|---|---|
| ConPTY + POSIX PTY, kill-tree, resize | `portable-pty` fork; Windows CI burst tests |
| Backscroll ring + persistent file + attach replay | `scrollback/<pane>.ansi`, 16 MiB cap, 2 MiB replay, subscribe catch-up |
| Lock-file arbitration | `daemon.lock` (one daemon per workspace) |
| Occupant state heuristics | `classify_agent_attention` (working / needs input / idle) and `AgentState` events |
| Headless CLI | `ctl attach` streams output; `ctl send`, `run`, `wait`, `snapshot`, `find` |
| Structured event stream instead of scraping | agent panes speak stream-json / Droid RPC with `seq` |
| Diagnostics without secrets | `ctl diagnostic` |

Rewriting that to reach the draft's M3 would cost three weeks to land where
the repo already is. The novelty is the lease and the ledger, so that is what
gets built, as an extension of the daemon.

Factual corrections to the draft, verified against `~/Data/kranz` on
2026-09-13:

- Kranz is at 0.2.0 with no git tags, and it is public with published crates
  (`kranz`, `kranz-engine`, `kranz-server`, `kranz-slack`).
- There is no `GET /api/read-token`. The read token is written to disk
  (`.kranz/serve.read.token`, `~/.kranz/serve/<endpoint>.read.token`, 0600)
  and pasted into the dashboard. The `x-kranz-token` header and the
  read/mutation split are real.
- Control bodies are `{"kind":"msg","text":"…","interrupt":false}`, tagged by
  `kind` in kebab-case, not `type: "Msg"`.
- `MissionState` is a struct. The status enum is `MissionStatus`
  (planning, approved, running, paused, blocked, validating, complete,
  failed, abandoned). "Pending grant / question / revision" are fields on the
  state, not statuses.
- Kranz already has an ephemeral hook-status lane: Claude Code hooks relay
  `running / needs-input / interrupted / turn-finished` per run through
  `POST /api/hook-status` with a per-run capability token, and
  `GET /api/missions/:id/hook-status` projects them (never authoritative).
- Kranz's roadmap explicitly parks the terminal and agentic-IDE lane for
  Sgian, so the boundary the draft describes is agreed on both sides.
- Kranz's WS replay gap (5,000), transcript tolerance of torn lines, chain
  hash construction (`kranz.event-log.v2\n` prefix, sorted-key JSON), unix-only
  `pty_harness`, Job-Object AppContainer launcher, and `scrub.rs` are all as
  the draft says.

Two draft assumptions to drop:

- "Writable is a lease, never a flag, and `ReadOnly<T>` is the whole safety
  story." With one shared workspace token, every client is the same
  principal. A lease built on self-asserted holder names is a coordination and
  audit mechanism, not a security boundary. Per-client tokens are the
  follow-on that makes it one (§6).
- "Read-only by default." For an attach client that is right. For a
  multiplexer where you spawn your own shells it is hostile. Default policy is
  `open`: an unheld pane behaves as today; a held pane is exclusive. A
  `required` policy gives the draft's semantics per workspace.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| **Holder** | A short label naming who holds a pane's keyboard, e.g. `craig@mbp`, `ctl:ci`, `agent:claude`. Client-asserted. |
| **Lease** | The single write claim on a pane. Zero or one holder. |
| **Hand-back note** | Free text the holder must supply on release. Mandatory. |
| **Ledger** | Append-only, hash-chained JSONL per pane at `ledger/<pane-id>.jsonl` under the workspace data dir. |
| **Policy** | `open` (default) or `required`, per workspace config. |

## 3. Lease semantics

States are `Unheld` and `Held { holder, since_ms, counters }`. The draft's
transitional states (`Requested`, `Releasing`) exist for transports with a
round trip; local IPC has none, so they are deferred to the SSM and Kranz
transports.

| Operation | Allowed when | Refused with |
|---|---|---|
| `take(holder)` | Unheld, or Held by the same holder (idempotent) | `held by H since …; use --force --why` |
| `take(holder, force, why)` | Held by another | requires non-empty `why`; ledgers `lease.revoked` then `lease.taken` |
| `release(holder, note)` | Held by `holder` | not the holder; empty note |
| `write(holder?)` under `open` | Unheld, or Held by `holder` | `pane keyboard is held by H` |
| `write(holder?)` under `required` | Held by `holder` only | as above, or `pane keyboard is unheld; take it first` |
| `broadcast` / sync input | writes skip every held pane | never refused, reports which panes were written |

Predicates are pure functions on `(policy, lease, holder)` and are unit tested
in isolation. Handlers call them; clients never re-derive the rules.

Holder labels: 1 to 64 bytes, printable ASCII, no whitespace or control
characters. Notes: trimmed, 1 to 4,096 bytes.

The lease is per pane, not per process: a pane whose shell exits keeps its
lease until released, revoked, or the pane is closed. Closing a held pane
ledgers `lease.revoked { why: "pane closed" }`. Leases persist in
`workspace.json` (additive `leases` map) so a daemon restart does not silently
drop who was in control.

## 4. Ledger format

One JSON object per line:

```json
{"seq":3,"ts_ms":1789600000123,"pane_id":"pane-2","type":"lease.released",
 "payload":{"holder":"craig@mbp","note":"answered the y/N, agent can carry on",
            "held_ms":41230,"writes":12,"bytes_typed":37,"refused_writes":2},
 "prev":"<hex>","h":"<hex>"}
```

```
h = sha256_hex("sgian.ledger.v1\n" + prev + "\n" + canonical_json(record without h))
```

`canonical_json` is serde_json with all object keys sorted and no whitespace.
The genesis record has `prev = ""`. The version prefix is inside the hash so
a downgrade cannot be forged by editing a field, the same reasoning as
Kranz's `kranz.event-log.v2\n`. Appends are one `write_all` followed by
`fsync`; lease events are rare enough that this costs nothing.

Event vocabulary for M1:

| `type` | payload |
|---|---|
| `lease.taken` | `holder`, `force`, `why?`, `previous_holder?` |
| `lease.released` | `holder`, `note`, `held_ms`, `writes`, `bytes_typed`, `refused_writes` |
| `lease.revoked` | `holder`, `by`, `why` |

M3 added `attention.changed { agent, from, to, evidence }` and
`pane.ended { exit_code }`, written by the output router without an fsync
(they are frequent and are not the product). Keystrokes are never in the
ledger; only counts.
Output bytes are never in the ledger; they stay in scrollback.

`ctl ledger <pane> --verify` walks the chain and names the first broken
sequence number. Truncation from the tail is not detectable without an
external copy of the head hash; `ctl ledger --verify --json` prints the head
so a script can pin it.

The IETF draft `draft-sharif-agent-audit-trail-00` (2026-03) uses SHA-256
over JCS-canonical JSON with `prev_hash`, an `escalation` action type, and a
`human_override` field with operator id and reasoning. The M1 shape maps onto
it directly; an export command in that vocabulary is a cheap later addition.

## 5. Surfaces

Daemon requests (additive; old clients unaffected):

- `take_lease { pane_id, holder, force?, why? }` → `LeaseInfo`
- `release_lease { pane_id, holder, note }` → `LeaseInfo`
- `lease_status { pane_id }` → `LeaseInfo`
- `send_input_as { pane_id, input, holder }` carries the holder; the legacy
  `write_to_pane` / `send_input` stay unchanged and are refused while a pane
  is held (no struct churn across the 28 existing call sites)
- `WorkspaceSnapshot.leases: { pane_id → LeaseInfo }`
- `DaemonEvent::LeaseState { pane_id, transition, holder?, since_ms?, note? }`

`LeaseInfo`: `pane_id, policy, holder?, since_ms?, held_ms?, writes,
bytes_typed, refused_writes, last_input_ms?`.

`ctl`:

```
sgian ctl lease [PANE]                                  # status
sgian ctl lease take [PANE] [--as HOLDER] [--force --why REASON]
sgian ctl lease release [PANE] -m NOTE [--as HOLDER]
sgian ctl send <PANE> [--as HOLDER] ... <TEXT>
sgian ctl ledger [PANE] [-n N] [--verify]
```

The default holder is `$SGIAN_HOLDER`, else `<user>@<host>`.

Config: `"lease_policy": "open" | "required"` (workspace or global layer).

## 6. Milestones

**M1 — daemon lease + ledger (this change).** State machine and predicates,
requests, event, snapshot field, persistence, ledger writer and verifier,
`ctl lease` / `ctl ledger` / `send --as`, unit and in-process integration
tests. Demo: two `ctl` holders contend for one pane; `ctl ledger --verify`
passes; flip one byte and it names the line.

**M2 — clients (shipped 2026-09-14).** All three clients send their holder
label with every write and show the lease in the pane header; a refused
keystroke shows a one-line "held by H" notice; take opens a why prompt only
when someone else holds the pane; release opens the note field and an empty
note cannot submit. Shortcuts are `Ctrl/Cmd+Shift+T` (take) and
`Ctrl/Cmd+Shift+L` (release): the draft's `Ctrl-Shift-R` is already rename.
Deferred from the draft: the hollow read-only cursor, take-on-first-keystroke,
and the "needs input" window title.

**M3 — official agent signals before heuristics (shipped 2026-09-14;
Notification-hook ingestion and the Kranz relay remain open).** Claude Code's
`claude agents --json` (`state`, `status`, `waitingFor`) and the Notification
hook (`agent_needs_input`, `permission_prompt`, `idle_prompt`) are the
supported ways to read session state; screen classification becomes the
fallback, marked low-confidence. Ledger `attention.changed` with evidence.
Expose transitions to scripts: `ctl agent --watch --json`. Relay the same
signals to a Kranz run via its hook-status lane when the pane belongs to one.

**M4 — Kranz target (lean version shipped 2026-09-14).** Shipped: a shell
pane running `kranz run` is bound to its mission automatically (or by
`ctl kranz bind`), `kranz status --json` drives its badge, and a released
note is mirrored with `kranz msg`, all through the CLI so the daemon needs
no HTTP client. Remaining from the draft: a pane kind that tails a mission run over
`GET /api/missions/:id/ws?since=` with the read token from disk, renders the
transcript as backscroll, maps `MissionStatus` plus pending fields to
attention, and turns `take` into message mode posting
`{"kind":"msg"}` to `/api/missions/:id/control`. Release mirrors the note as
a final message and ledgers `kranz.mirrored`. Verify every route against
`docs/protocol.md` in the Kranz checkout at implementation time.

**M5 — SSM / ECS Exec target.** Evaluate `aws-ssm-bridge` 0.5 (MIT, binary
framing, ack and retransmit, KMS) before writing a frame parser. Ack
`output_stream_data` only after scrollback accepts the bytes so backpressure
propagates. `ResumeSession` on blips; fresh session on termination with the
right ledger events. ECS Exec targets need verification against the crate.
This is the milestone that makes Windows and WorkSpaces operators
first-class and it is the one with real network risk; fuzz whatever framing
is hand-rolled.

**M6 — per-client identity.** Per-client tokens or a signed hello so the
lease becomes a boundary rather than a convention. This is also what makes
"agent tried to type while a human held the pane" a refusal the ledger can
attribute honestly.

## 7. Other things worth building, from the research pass

Ranked by leverage against effort. None are committed beyond M1.

1. **Read-side backpressure.** The PTY reader has no watermark: a slow
   subscriber is dropped, never stalls the pane, which is the right default
   for a GUI. The draft's torture test is now a regression test
   (`output_flood_keeps_the_daemon_responsive_bounded_and_killable`): 18 MiB
   of `yes` through one pane on the development Mac took about 11 s (roughly
   1.7 MB/s through PTY, vt100 model, scrollback append and fan-out), the
   slowest concurrent request was 110 ms, the scrollback file never exceeded
   its 16 MiB cap, and an unbounded `yes` died with its pane. The throughput
   figure is the number to beat if a network producer (M5) ever needs more.
2. **Scrollback search and permalinks (shipped 2026-09-15).** `ctl search`
   and `ctl lines` over the scrollback file with control sequences stripped;
   line numbers are relative to the current file and the 16 MiB cap
   renumbers, which `total_lines` lets a script notice. Originally: `Find`
   filters metadata only. A substring search over the scrollback file with
   `ctl lines <pane> a:b`
   is small and makes the ledger's `seq` range citable.
3. **Terminal-output injection guard (shipped 2026-09-16).** ATR-2026-00259
   documents agents hiding content from human review with OSC and cursor
   moves. The daemon now counts, per pane, SGR 8 conceal, OSC 52 clipboard
   writes, OSC 8 hyperlinks whose visible text names another host,
   DCS/APC/PM/SOS strings and raw C1 controls; ordinary redraws are not
   counted, since agents repaint constantly. Hits are ledgered as
   `output.suspicious` at most every five seconds per pane, broadcast as
   `output_warning`, and shown by `ctl agent`. Stripping (rather than
   flagging) and client badges are still open.
4. **Unattended-mode badge.** Any pane running under `auto`, `dontAsk`, or
   `bypassPermissions` must be visibly marked. Warp shipped auto-approve
   that bypassed its own denylist; the Claude Code deny-rule skip after 50
   subcommands (CVE-2026-40068 era) shows why visibility matters.
5. **Codex app-server and ACP backends.** Codex exposes JSON-RPC over stdio
   with approvals and interrupt; ACP wraps Claude, Codex, and others with the
   same shape. One adapter behind the existing normalized event stream makes
   the vendor-neutral claim real.
6. **Ledger export in the agent-audit-trail draft vocabulary**, optionally
   Ed25519-signed with a per-workspace key. EU AI Act Article 12 logging is
   in force since 2026-08-02; a portable, verifiable record is a selling
   point for the WorkSpaces audience.
7. **`vt100` replacement.** The crate is stale (2025-07) and a third-party
   fuzz run reported panics. `alacritty_terminal` 0.26 (2026-04) is
   maintained and exposes grid, scrollback, and regex search. Not urgent
   while vt100 only drives classification and snapshots, but it should be
   on the list before it drives a renderer.
8. **Split `lib.rs`.** Everything lives in one flat module. The lease and
   ledger code lands as its own banner section with pure functions so it can
   be the first thing moved into a module when the split happens.

## 8. Security and privacy posture (unchanged from the daemon's)

- Holder names and notes are operator text: bounded, validated, never
  interpreted.
- A closed pane takes its whole process tree with it (Unix: descendants from
  a `ps` snapshot, SIGTERM then SIGKILL; Windows: kill-on-close Job Object),
  so a takeover cannot leave an agent running blind after the pane is gone.
- Keystrokes are never recorded; the ledger stores counts and timestamps.
- Ledger and scrollback files are owner-only (0600 / current-user ACL).
- The lease is coordination until M6 lands. Say so in the README.
