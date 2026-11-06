# Daemon module map

`src-tauri/src` is one crate, `sgian_lib`, split by concern on 2026-09-19
(PRs #41 to #45). Every child module opens with `use super::*` and lib.rs
re-imports it with `use <name>::*`, so items keep their bare names across
the crate; cross-module visibility is `pub(crate)`. Nothing here is public
API: the wire contract is the JSON in `docs/native-ipc.md`.

| File | Lines | What lives there |
| --- | ---: | --- |
| `lib.rs` | 6,726 | Shared types and the wire contract (`DaemonRequest`, `DaemonEvent`, `WorkspaceSnapshot`, `Config`), `PaneRegistry`, `DaemonServer` and every request handler, the Tauri commands and `run()`. |
| `transport.rs` | 170 | `TransportStream`/`TransportListener` (Unix socket or Windows named pipe), connect/bind, pipe naming. |
| `windows_transport.rs` | 1,090 | The named-pipe implementation (`cfg(windows)`). |
| `frame.rs` | 118 | The v2 length-prefixed frame codec. |
| `identity.rs` | 255 | Per-client credentials (M6): scopes, policy, records, `ClientIdentity`, request scoping, holder binding. |
| `windows_job.rs` | 338 | Windows job ownership, suspended agent launch and process-tree regression tests. |
| `process_tree.rs` | 222 | Fork-free parent snapshots (libproc / procfs), descendant walks, tree termination. |
| `router.rs` | 1,436 | Agent screen classification, `AgentTracker`, `OutputRouter`: scrollback append, output-guard hook, attention and usage state, subscriber fan-out. |
| `terminals.rs` | 800 | `TerminalStore`: PTY spawn, liveness, generation-scoped grant hooks, input queues, kill. |
| `grants.rs` | 1,162 | Kranz execution grants (`docs/design/execution-grants.md`): RFC 2104 HMAC-SHA256 verification, `.kranz/serve.grant.key` (`0600`) loader, canonical `cwd` confinement, `PATH`-only bare binary resolution, strict `env_allow` PTY spawn, `GrantStore`, and `grant_exec`/`grant_output`/`grant_wait`/`grant_kill`/`grant_release`. |
| `agent_stream.rs` | 2,359 | Chat-native agent panes: `AgentBackendAdapter` (`Claude`, `Droid`), spawn planning, sessions and approvals, stream-json/JSON-RPC normalization, the conversation log, the reader thread. |
| `output_guard.rs` | 597 | `OutputTricks` and the scanner for output that hides something from a person. |
| `ledger.rs` | 469 | Keyboard leases (`LeasePolicy`, `HeldLease`, predicates), the hash-chained ledger writer and verifier, `AgentUsage` from the status line. |
| `notes.rs` | 538 | Per-project shared context notes under `<repo>/.sgian/projects/<name>/notes/`: ledgered writes, scrub on read, the `project_notes_changed` watch. |
| `probe.rs` | 450 | `claude agents --json` mapping, process-table and Kranz-worker parsing, projects and roll-ups, hook-to-attention, pid placement. |
| `serve.rs` | 1,501 | `run_daemon`, the accept loop (peer-uid check), the hello gate, subscriptions, the framed request loop. |
| `daemon_client.rs` | 1,495 | `DaemonConnection` and the ctl-side `DaemonClient`, token and data-dir helpers. |
| `ctl.rs` | 6,084 | `sgian ctl`: option parsing, every command, its argument parser and printer, the help text. |
| `serve_http.rs` | 889 | `sgian ctl serve`: the loopback HTTP host for the web client (embedded assets, `/api/invoke`, `/api/events` as SSE, read-only gate, Host check). |
| `tests/` | ~21,000 | The unit and in-process integration suite: `mod.rs` (shared transport helpers), `harness.rs` (`TestDaemon`), and one file per area (terminal, env_scrub, ctl_parsers, ctl_run, observability, lifecycle, config, agents, lease_identity, notes, grants). |

Where to start for a change: a new request is a variant in `lib.rs`, a
scope in `identity.rs::request_scope`, a handler on `DaemonServer`, a ctl
verb in `ctl.rs`, and a test in the matching `tests/` file. A new client-visible field is
additive on the snapshot or an event, mirrored in `docs/native-ipc.md`.
