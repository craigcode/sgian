# Native client IPC contract

Sgian's desktop clients are intentionally thin. The Rust daemon owns workspace
state, PTYs, agent processes, scrollback and persistence; a client discovers the
workspace endpoint and communicates over authenticated JSON IPC.

This document records the stable subset used by the native macOS and Windows
clients. The authoritative server types remain
`DaemonRequest`, `DaemonEvent` and `WorkspaceSnapshot` in
`src-tauri/src/lib.rs`.

## Endpoint discovery

Native clients may ask the bundled helper to start/locate the daemon and return
non-secret discovery metadata:

```text
sgian ctl --workspace <path> --json ipc-endpoint
```

The response includes `transport`, the concrete `endpoint`, `token_path`, the
canonical `workspace`, `workspace_key`, `protocol_version`, and supported native
client capabilities. It never includes the token. On Windows, `endpoint` is the
owner-SID-scoped `\\.\pipe\...` name produced by the same Rust function used by
the daemon. This avoids duplicating its SID lookup and pipe-name hash in C#.

The macOS client also knows the stable Unix layout directly: canonicalize the
workspace, compute the 64-bit FNV-1a key, resolve the current/legacy application
support directory, read `daemon.token`, and connect to `daemon.sock`.

The macOS app asks its bundled Rust helper to run `ctl --workspace <path> sync
off` when no daemon is reachable. The Windows app uses `ipc-endpoint`, which
performs the same startup and legacy-data migration work before returning.

## Authentication and wire format

Every connection begins with one newline-terminated JSON hello:

```json
{"type":"hello","version":1,"token":"<workspace token>"}
```

The daemon answers with an `IpcResponse`. The native client deliberately uses
the compatibility v1 wire: one newline-delimited request and response per
connection. A subscription sends `{"command":"subscribe"}` after the hello and
then receives newline-delimited events until disconnect. The native client
advertises `subscribe-ack`; when the daemon advertises the same capability, the
first event confirms registration before the client treats the stream as live.
Older daemons retain the historical acknowledgement-free behavior. The daemon's
v2 framed wire remains available for clients that need persistent request
connections.

Responses have this envelope:

```json
{"ok":true,"result":{},"error":null}
```

Clients must ignore additive object fields and unknown event tags. Agent events
carry a monotonically increasing per-pane `seq`; clients discard replayed events
whose sequence is not newer than the last applied sequence.

## Native client requests

The native clients currently use:

- `ping`, `bootstrap_workspace`, `subscribe`
- `update_workspace_layout`, `get_config`, `write_config`
- `create_pane`, `close_pane`, `rename_pane`
- `ensure_pane_terminal`, `restart_pane_terminal`
- `write_to_pane`, `resize_pane_terminal`, `set_active_pane`
- `create_agent_pane_with_spec`, `send_agent_message`
- `agent_approval`, `interrupt_agent`

Daemons advertising the `lease` capability also accept
`take_lease {pane_id, holder, force?, why?}`, `release_lease {pane_id, holder,
note}`, `lease_status {pane_id}`, and `send_input_as {pane_id, input, holder}`;
the bootstrap snapshot carries a `leases` map for held panes and subscribers
receive `lease_state` events. `write_to_pane` carries no holder and is refused
while a pane is held, so a client that wants to type into a held pane must
take the lease and use `send_input_as`. See
`docs/design/keyboard-lease-and-ledger.md`.

The bootstrap snapshot may carry `output_warnings` (pane_id → per-kind
counts of output that hides content: `conceal`, `clipboard`,
`hyperlink_mismatch`, `string_controls`, `c1_controls`), and subscribers may
receive `output_warning` events with `added` and `total`; both additive, and a
client should mark such a pane.

`agent_states` entries and `agent_state` events may carry `mode` (the
agent's observed permission mode) and `unattended` (true when tools run
without a person approving them); both are additive and clients must mark an
unattended pane visibly.

The bootstrap snapshot supplies the pane registry, active pane, scrollback,
recorded PTY dimensions, runtime state, agent attention, provider/model specs
and bounded normalized agent-event replay. Clients size their terminal emulator
to those dimensions before replaying raw ANSI scrollback.

`send_agent_message` accepts an optional `message_id` (1–128 bytes). Clients
generate a unique ID for each prompt and attach it to the optimistic user bubble.
After enqueueing the provider input, the daemon persists and broadcasts an
`agent_event` with `kind: "user_message"`, `text`, the supplied `message_id`
(or null), and the next pane sequence. This shares the provider-output log and
ordering lock, so the accepted prompt precedes its reply in live and replayed
history. A rejected enqueue does not create a history entry. The ID correlates
the event with a local bubble; it is not a server-side idempotency key.

Clients reconcile by ID rather than text, preserving distinct identical prompts
sent by different clients. Only pending local bubbles may be rolled back after
a failed request. Older daemons ignore the additive ID, and their history cannot
restore prompts that were never saved.

## Event handling

The native client handles `pty_output`, `pane_ended`, `pane_created`,
`pane_closed`, `pane_renamed`, `agent_state` and `agent_event`. Unknown events
are ignored for forward compatibility. Subscription disconnects are retried;
the daemon and its sessions are never tied to the client process lifetime.

Native split layouts use the same binary JSON tree as Tauri. Clients validate
tree depth, directions, ratios and unique pane IDs, reconcile stale membership,
and debounce persisted resize changes. See [native release](native-release.md).
