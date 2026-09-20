# Daemon module map

`src-tauri/src` is one crate, `sgian_lib`, split by concern on 2026-09-19
(PRs #41 to #45). Every child module opens with `use super::*` and lib.rs
re-imports it with `use <name>::*`, so items keep their bare names across
the crate; cross-module visibility is `pub(crate)`. Nothing here is public
API: the wire contract is the JSON in `docs/native-ipc.md`.

| File | Lines | What lives there |
| --- | ---: | --- |
| `lib.rs` | 6,045 | Shared types and the wire contract (`DaemonRequest`, `DaemonEvent`, `WorkspaceSnapshot`, `Config`), `PaneRegistry`, `DaemonServer` and every request handler, the Tauri commands and `run()`. |
| `transport.rs` | 159 | `TransportStream`/`TransportListener` (Unix socket or Windows named pipe), connect/bind, pipe naming. |
| `windows_transport.rs` | 1,071 | The named-pipe implementation (`cfg(windows)`). |
| `frame.rs` | 118 | The v2 length-prefixed frame codec. |
| `identity.rs` | 252 | Per-client credentials (M6): scopes, policy, records, `ClientIdentity`, request scoping, holder binding. |
| `process_tree.rs` | 157 | Fork-free parent snapshots (libproc / procfs), descendant walks, tree termination. |
| `router.rs` | 1,421 | Agent screen classification, `AgentTracker`, `OutputRouter`: scrollback append, output-guard hook, attention and usage state, subscriber fan-out. |
| `terminals.rs` | 700 | `TerminalStore`: PTY spawn, liveness, input queues, kill. |
| `agent_stream.rs` | 2,121 | Chat-native agent panes: spawn planning, sessions and approvals, stream-json normalization, the conversation log, the reader thread. |
| `output_guard.rs` | 319 | `OutputTricks` and the scanner for output that hides something from a person. |
| `ledger.rs` | 469 | Keyboard leases (`LeasePolicy`, `HeldLease`, predicates), the hash-chained ledger writer and verifier, `AgentUsage` from the status line. |
| `probe.rs` | 386 | `claude agents --json` mapping, process-table and Kranz-worker parsing, projects and roll-ups, hook-to-attention, pid placement. |
| `serve.rs` | 1,453 | `run_daemon`, the accept loop (peer-uid check), the hello gate, subscriptions, the framed request loop. |
| `daemon_client.rs` | 1,418 | `DaemonConnection` and the ctl-side `DaemonClient`, token and data-dir helpers. |
| `ctl.rs` | 5,370 | `sgian ctl`: option parsing, every command, its argument parser and printer, the help text. |
| `tests/` | 19,704 | The unit and in-process integration suite: `mod.rs` (shared transport helpers), `harness.rs` (`TestDaemon`), and one file per area (terminal, env_scrub, ctl_parsers, ctl_run, observability, lifecycle, config, agents, lease_identity). |

Where to start for a change: a new request is a variant in `lib.rs`, a
scope in `identity.rs::request_scope`, a handler on `DaemonServer`, a ctl
verb in `ctl.rs`, and a test in the matching `tests/` file. A new client-visible field is
additive on the snapshot or an event, mirrored in `docs/native-ipc.md`.
