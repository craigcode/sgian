# Per-client identity and a read-mostly remote view (M6)

Status: decided and built, 2026-09-18 (decisions: bearer tokens, `open` by
default, Kranz runs are their own holders with no impersonation, remote is an
SSH-forwarded socket). Shipped: `ctl identity issue|list|revoke`, `ctl
whoami`, `client_token` on the hello, scopes `read`/`write`/`admin`, holder
bound to the credential (`--as` anything else refused; unattributed input
becomes attributed), `credential` on lease ledger records, a peer-uid check
on the Unix socket, `identity: open | required`. Not built, by decision: an
`impersonate` scope. The text below is the proposal as written.

## What is wrong today

Every client presents the same workspace token, an owner-only file under the
platform data directory. Whoever can read that file is fully trusted, and the
`holder` on a lease is whatever the caller typed after `--as`. So the lease is
coordination: it stops two people treading on one pane by convention, but the
ledger's "alice took the keyboard" means "something that read the token said
it was alice". Two consequences:

1. **Attribution is not evidence.** `lease.taken { holder }` and the refusals
   the generation check produces cannot be trusted by a Kranz gate, because
   the label is self-declared.
2. **There is no remote client.** A second device has no credential of its
   own and no transport; the only path is copying the token.

`SECURITY.md` already says a workspace is not an isolation boundary against
another process running as the same user. M6 does not change that. Its goals
are honest attribution and a second device that can watch and, with the
lease, type.

## Design

**Credentials.** The daemon issues per-client tokens.

```
ctl identity issue --holder craig@phone --scope read          # prints once
ctl identity issue --holder kranz --scope write,impersonate   # for Kranz runs
ctl identity list | revoke ID
```

Stored in `clients.json` beside the workspace token, owner-only: id, holder,
scopes, created, last seen, revoked. The hello gains an additive
`client_token`; an old daemon ignores it, an old client omits it.

**Holder is derived, not declared.** With a client token, `holder` on every
lease and write is the token's holder; `--as` is refused unless the token
carries `impersonate` (Kranz acting for a run). The workspace token keeps
working as the root credential for local `ctl`, with holder `user@host` as
today, so nothing breaks the day this lands.

**Scopes.** `read` (snapshot, subscribe, search, dossier), `write` (input,
leases, pane lifecycle), `admin` (identity, config, shutdown), `impersonate`.
A read-only client sees the board and scrollback and cannot type; typing
needs `write` and, as now, the lease. Refusals name the scope.

**Local belt and braces.** On Unix the daemon checks the peer uid on the
socket matches its own (`getpeereid` / `SO_PEERCRED`); Windows already
checks pipe ownership. Cheap, and it makes the same-user boundary explicit.

**Ledger.** `lease.taken`, `lease.released`, `lease.revoked` and refused
writes record `credential: <id>` alongside `holder`. A dossier then says
which credential acted, which is what the execution-grants receipt needs.

**Policy.** Config `identity: open | required` (default `open`). Under
`required`, a hello without a client token gets `read` only. `lease_policy:
required` plus `identity: required` is the "lease is a boundary" mode.

**Remote transport: none new.** Sgian stays a local-socket daemon. A remote
client forwards the socket over SSH (`ssh -L`) and presents its client
token; the daemon never listens on TCP. This gives a laptop-to-desktop
remote today. A phone needs a served web UI, which Sgian does not have
(the Tauri bundle is not a web server); that is a separate lane (M7,
"`sgian serve` over the tunnel") and out of scope here.

## Alternatives considered

- **Signed hello (ed25519 keypair per client, allowlist of public keys).**
  Stronger than bearer tokens (no replay from a captured token) but adds a
  crypto dependency, key storage on three platforms, and a rotation story.
  Worth it only if tokens ever cross a network unencrypted, which the SSH
  design rules out. Can replace tokens later behind the same `client_token`
  field.
- **OS identity only (peer uid, pipe SID).** Free, but every process of the
  user is the same identity, so it cannot distinguish a person from an
  agent or a phone. Kept as the local check, not as the identity.
- **Relay through the Kranz server.** Kranz has HTTP and per-run tokens, but
  putting keystrokes through it inverts the agreed boundary (Kranz
  authorizes; Sgian executes).

## Client work

Tauri: store the token in the app data dir; native macOS: Keychain; Windows:
DPAPI. Each client shows its own holder and scope in the status area and
renders `read` as the read-only notice the lease already uses. `ctl` reads
`SGIAN_CLIENT_TOKEN` or a `--identity FILE`.

## Cost

Daemon and ctl about two days; clients one to two; docs half a day. All
additive: no wire break, no persisted-format change beyond `clients.json`.

## Decisions for the owner

1. Bearer tokens now, signed hello later if needed (recommended), or signed
   hello from the start.
2. Default `identity: open` with an announced move to `required` at 1.0, or
   `required` from the first public release.
3. Grant Kranz `impersonate`, or make Kranz runs their own holders
   (`kranz:<run-id>`) with no impersonation at all (cleaner ledger; the
   execution-grants doc already leans this way).
4. Confirm remote means "SSH-forwarded socket" for M6 and that the phone
   lane waits for a served UI.
