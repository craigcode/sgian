# Trust-surface review — 2026-09-19

Scope: the input surfaces added between 2026-09-13 and 2026-09-18, reviewed
before the public visibility flip: the per-client credential hello (M6),
`ctl hook` and `ctl statusline` payloads, pid-based pane placement, and the
project dossier. Reviewer model: a hostile process running as the same user
(the documented boundary in `SECURITY.md`), and a read-only remote
credential over an SSH-forwarded socket.

## Fixed in this pass

| Finding | Consequence | Fix |
| --- | --- | --- |
| Hook and status-line reports were `read` scope for every identity. | A read-only credential (the phone) could set any pane's badge by naming a pid, and append `hook.received` records to its ledger. | Root keeps them as reads (a session's own hooks run with the workspace token); a credential needs `write`. Test covers both. |
| Revocation only took effect at the next hello. | A revoked credential with a long-lived framed connection kept its scopes until it disconnected. | Every request re-checks the credential is active; a revoked one gets `client credential revoked`. Test covers it. |
| The legacy `write_to_pane` request bypassed holder binding. | A credentialed client could type unattributed through the old request. | Bound like `send_input`: it becomes `send_input_as` with the credential's holder. Test covers it. |
| Hook and status-line strings were ledgered and stored unbounded. | Any process's stdin could append arbitrarily long lines to an append-only ledger, or push a long model name into every snapshot. | `event` 64, `notification_type` 64, `message` 200, `session_id` 128, `model` 64, `model_id` 128 characters. |
| `ctl hook` / `ctl statusline` read stdin without limit. | A pathological payload could make the command allocate before it decided the payload was not JSON. | 1 MiB cap. |
| `clients.json` was rewritten on every hello to record `last_seen`. | With a credentialed status line that is a file write per turn. | At most once a minute per credential. |

## Accepted, and why

- **A caller can name any pid.** `ctl hook --pid N` and the status-line
  report place the caller by process ancestry, and the pid is whatever the
  caller says. Within the same-user boundary this is no worse than the
  caller typing into the pane, and the result is a badge plus a ledger
  record whose `evidence` says `hook`, never a keystroke. A remote
  credential needs `write` to do it at all (fixed above).
- **A presented credential can reach other daemons.** `ctl hook` and
  `ctl statusline` try every running daemon of the same user until one
  owns the pid. A hook payload therefore reaches daemons for workspaces the
  session is not in. They are the same user's daemons; nothing is written
  unless the pid maps.
- **Bearer tokens.** A copied `sgc_…` token is that client. They travel only
  over the local socket or an SSH tunnel; `clients.json` holds hashes only.
  A signed hello can replace them behind the same `client_token` field if
  the threat model changes.
- **Timing on credential lookup.** Records are compared in constant time
  each, but the loop stops at the first match, so a token's position in the
  table is observable in principle. With at most 256 records and a local
  socket this is not a practical oracle.
- **The dossier contains scrollback.** `ctl project dossier` writes the last
  N lines of each member pane's scrollback, which may contain secrets, to
  wherever `--out` says. That is the point of the document and the caller's
  choice; the README says so.
- **Revoked records accumulate.** `clients.json` keeps revoked records
  forever so an old id still resolves in the ledger. Only active records
  count against the 256 limit.

## Verified

`cargo test --lib` 539 passed (the identity round trip now also covers
report refusal for a read-only credential, a write credential reporting,
the legacy write being bound, and a live connection cut after revocation);
clippy `-D warnings` and fmt clean.
