# Execution grants: Kranz authorizes, Sgian executes and displays

**Status:** design note for review by the Kranz side, 2026-09-16
**Depends on:** a `kranz-acp` crate (ACP client transport + neutral events)
extracted from `kranz-engine`, and a `TerminalProvider` seam in Kranz's ACP
client that advertises `terminal` capability only when a provider is
configured.

## 1. Boundary

Kranz decides what an agent may do and records what it did. Sgian shows a
human what is happening in a terminal and lets them take the keyboard. When
an ACP worker under Kranz asks for a terminal, the command runs in a Sgian
pane, and both sides keep a receipt that names the same grant.

Three corrections from the Kranz review are baked in:

- A terminal provider gives visibility into **routed** commands only. An
  adapter routes through `terminal/*` when it chooses to; anything the agent
  runs internally stays invisible to both projects. Containment stays with
  Kranz's sandboxing; Sgian's process-tree kill and output guard are defence
  in depth for what does flow through a pane.
- ACP terminals are create / output / wait_for_exit / kill / release. They
  have no stdin and no resize. The lease on an ACP session therefore governs
  prompt submission, permission answers and cancellation, not keystrokes.
  A pane showing an agent-created terminal is **read-only to humans by
  default**; typing into it is a takeover outside the protocol and needs an
  explicit `lease take` with a why, which is ledgered.
- A permission request proves that one request is pending. Its absence
  proves nothing, so the screen and probe heuristics stay alongside
  protocol signals rather than being replaced by them.

## 2. The grant

Kranz mints a grant per terminal request and Sgian verifies it before
executing anything. A grant is a compact JSON object plus an HMAC over its
canonical form with a key both sides hold (see §4):

```json
{"grant":"g-01J9…","mission":"m-3cda6a","run":"r-07","repo":"/abs/path",
 "cwd":"/abs/path/sub","argv":["cargo","test"],"env_allow":["PATH","HOME"],
 "expires_ms":1789600000000,"issued_ms":1789599990000}
```

Sgian's execution endpoint refuses the request unless:

- the HMAC verifies and `expires_ms` is in the future;
- `repo` is a workspace Sgian serves and `cwd` is inside it after
  canonicalisation (no symlink escape);
- `argv` is executed verbatim, never through a shell;
- the environment is Sgian's configured spawn environment filtered to
  `env_allow`, never values from the request;
- the grant id has not been used before (a small replay set, bounded by
  expiry).

Nothing in the request can widen what Kranz allowed; Sgian can only narrow
it (refuse). This is why the grant is a real boundary where per-client
identity for humans on one machine was not: the grant is a capability Kranz
mints, not a claim a caller makes.

## 3. The Sgian surface

Daemon requests, additive:

| Request | Effect |
|---|---|
| `grant_exec { grant, signature, pane? }` | verify, spawn in a fresh pane (or the given one) as a PTY child, return `{ terminal_id, pane_id }` |
| `grant_output { terminal_id }` | output so far (bounded, control sequences intact), `exit` when finished |
| `grant_wait { terminal_id, timeout_ms? }` | block until exit or timeout |
| `grant_kill { terminal_id }` | SIGTERM then SIGKILL after a grace, same as pane close |
| `grant_release { terminal_id }` | drop the record; the pane and its ledger stay |

Kranz's `TerminalProvider` implementation maps `terminal/create` →
`grant_exec`, `terminal/output` → `grant_output`, `terminal/wait_for_exit` →
`grant_wait`, `terminal/kill` → `grant_kill`, `terminal/release` →
`grant_release`. Sgian's existing `RunProcess` (non-PTY exec, bounded
capture, timeout, kill) is the closest code; the delta is a PTY-backed
variant with output-by-id and release semantics. "A few hundred lines plus
tests" is an estimate until it is written.

## 4. Receipts

Sgian ledgers, on the pane the command ran in:

- `grant.exec { grant, mission, run, argv, cwd }` when accepted (durable);
- `grant.refused { grant?, reason }` when refused;
- `pane.ended { exit_code }` as today, which closes the receipt;
- `output.suspicious` as today if the command's output hides anything.

Kranz records the grant it issued and the terminal id it received. The two
records share the grant id, so an auditor can walk from a mission event to
the exact pane, its scrollback lines (`ctl lines`), and who held the
keyboard at the time.

Key distribution: Kranz already writes a read token to
`.kranz/serve.read.token` (0600). The grant key is a sibling file the
daemon reads at bind time (`ctl kranz bind --repo`), rotated by Kranz. Not
a network exchange.

## 5. Not in scope

Interactive takeover of an agent-created terminal (no ACP contract for it),
cloud execution, and any change to Kranz's policy or gate model.
