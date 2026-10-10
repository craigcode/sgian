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
{"type":"hello","version":1,"token":"<workspace token>","client_token":"<optional per-client credential>"}
```

The daemon answers with an `IpcResponse`. The native client deliberately uses
the compatibility v1 wire: one newline-delimited request and response per
connection. `client_token` is additive (docs/design/client-identity.md): a
credential issued by `ctl identity issue` names the connection, fixes its
holder and limits it to its scopes (`read`, `write`, `admin`); with one, the
workspace token may be empty, which is how a client on another machine
connects over an SSH-forwarded socket. A presented credential is held to: a
revoked or unknown one is refused even beside a valid workspace token, and
a revocation cuts a live connection at its next request. Hook and
status-line reports (`agent_signal`, `agent_status`) are reads for the
workspace token but need `write` from a credential. The hello response carries
`identity` (`credential`, `holder`, `scopes`, `root`, `identity_policy`).
Under `identity: required` the workspace token has `read` and `admin` only.
A refused request says `read-only credential: '<scope>' scope required for
<command>`; clients show it as the same read-only notice a lease refusal
gets. Clients read `SGIAN_CLIENT_TOKEN` (or `SGIAN_CLIENT_TOKEN_FILE`), then
a per-workspace stored credential (macOS login Keychain, Windows credential
vault), and take their holder from the hello.
A subscription sends `{"command":"subscribe"}` after the hello and
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

`get_config` returns the effective configuration a settings form edits,
including the coordination keys a form may not show (`lease_policy`,
`agent_probe_interval_ms`, `kranz_bin`, `identity`, `scrub_env`).
`write_config` replaces the workspace file with the payload, with one rule a
client can rely on: a key the payload omits keeps its current value, and a
key it carries, including an explicit `null`, `[]` or `{}`, replaces it. A
form should still load before it lets anyone save. Agent profiles name their
fields `agent_backend` and `agent_model`; unknown profile fields are
rejected.

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

The bootstrap snapshot may carry `projects` (name → `{ name, goal?, repo?,
panes, created_at_ms }`), additive; the `project_*` requests (`project_create`,
`project_delete`, `project_assign`, `project_unassign`, `project_list`,
`project_show`, `project_ledger`, `project_dossier`) manage them. Subscribers
may receive `projects_changed` with the whole `projects` map after any change
(create, delete, assign, unassign, a member pane closing); replace, do not
diff. The Tauri client groups its session overview by project with a per-
project roll-up; the macOS sidebar groups its sections by project with the
roll-up in each header, and the Windows sidebar shows one roll-up line per
project above the list and names each pane's project in its subtitle.
`project_note_add {name, title, body, holder, pane_id?}`, `project_notes
{name}` and `project_note_remove {name, file, holder}` manage a project's
shared context notes (`docs/design/shared-context-notes.md`): Markdown files
under `<repo>/.sgian/projects/<name>/notes/` (the workspace directory when
the project names no repo; a relative `repo` is taken from the workspace, and
the root must exist and be the workspace or a git repository, since `repo` is
free text any write-scoped client can set). `project_notes` returns `{format:
"sgian.notes.v1", project, dir, total, bytes, tricks?, notes: [{file, title,
evidence, holder?, pane?, written_at_ms?, bytes, hash, body, tricks?}]}`,
newest first, with bodies scrubbed as agent output is and the hidden-text
tricks counted per note and in all; `evidence` is `daemon` when the file
carries the front matter the daemon wrote and `file` when something else
wrote it. Every write is a `note.added` / `note.removed` record with the
content hash in the project's own ledger (`ledger/project-<name>.jsonl`,
`pane_id: "project-<name>"`), which `project_ledger` merges with the member
panes' and `project_dossier` carries as `ledger`, beside the listing as
`notes`. A credentialed connection's `holder` must match its own, as for
input and leases; a read-only credential can list but not write.
Subscribers receive `project_notes_changed {project, file, hash?}` when a
note is written, changed or removed: the daemon's own writes announce at
once, and a file watch over each root's `.sgian/projects` tree covers edits
made outside it (an agent's file tools, an editor). `hash` is the SHA-256 of
the file now there and is absent once it is gone. The event never carries
the note's contents. The web bridge forwards it as `project-notes-changed`
and exposes `project_notes {name}` as a frontend command. All three clients
show a project's notes beside it on the board: a summary ("2 notes · 1 hid
text") and the newest few as one line each (title, writer or `file`, date,
⚠ when the guard removed hidden text), re-read on the event; bodies never
reach the board.
The bootstrap snapshot may carry `output_warnings` (pane_id → per-kind
counts of output that hides content: `conceal`, `clipboard`,
`hyperlink_mismatch`, `string_controls`, `c1_controls`, and `invisible` for
bidi overrides and zero-width characters, which the daemon counts in agent
panes), and subscribers may
receive `output_warning` events with `added` and `total`, plus `sample` (the
kind and a bounded, escaped prefix of the pane's first opaque DCS/APC/PM/SOS
string, present only when one was counted) so a badge can say what was
seen; all additive, and a client should mark such a pane (the Tauri client shows an amber `⚠ N` badge
beside the agent badge, with the per-kind counts in its title; macOS an
orange eye-slash icon in the pane row; Windows `⚠ N hidden` in the subtitle).

Agent events (`agent_event`) are scrubbed by the daemon before they are
logged or sent: escape sequences and control characters are removed from
every string, and the characters the `invisible` counter covers are removed
too. When anything was counted, the event carries `scrubbed`, an
`OutputTricks` object with those counts, so a client can mark the text
beside which characters were removed. Additive; the per-pane conversation
log holds the scrubbed text.

The bootstrap snapshot may carry `agent_usage` (pane_id → `{ model?,
model_id?, context_used_percentage?, context_window_size?, five_hour?,
seven_day?, total_cost_cents?, session_id?, updated_at_ms }`, windows as
`{ used_percentage, resets_at? }`), fed by `sgian ctl statusline` from a
session's status-line payload; subscribers receive `agent_usage { pane_id,
usage }` when a reading changes. Additive. A client shows it as one line
("Opus · 40% context · 5h 23% ↻ 1h10m") beside the pane and treats a
reading past 80% as hot; the Tauri overview and the macOS sidebar also show
the freshest rate-limit line once per project heading (limits are per
account).

`agent_states` entries and `agent_state` events may carry `mode` (the
agent's observed permission mode) and `unattended` (true when tools run
without a person approving them); both are additive and clients must mark an
unattended pane visibly. A current daemon always sends `unattended` on the
event; a client talking to an older daemon should derive it from `mode`
(`auto`, `bypass`, `bypassPermissions`, `dontAsk`) when the key is absent, so
a live transition and the bootstrap snapshot agree.

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

The native clients handle `pty_output`, `pane_ended`, `pane_created`,
`pane_closed`, `pane_renamed`, `agent_state`, `agent_event`, `lease_state`,
`projects_changed`, `output_warning` and `agent_usage`. Unknown events are
ignored for forward compatibility. Subscription disconnects are retried;
the daemon and its sessions are never tied to the client process lifetime.

Native split layouts use the same binary JSON tree as Tauri. Clients validate
tree depth, directions, ratios and unique pane IDs, reconcile stale membership,
and debounce persisted resize changes. See [native release](native-release.md).
