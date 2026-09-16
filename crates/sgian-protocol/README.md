# sgian-protocol

This name is reserved for the typed contract of the Sgian daemon: its
requests and events (panes, keyboard leases and their generations, projects,
the hash-chained ledger, agent attention and the output guard) as Rust types,
so a client or an orchestrator such as Kranz can talk to a workspace daemon
without hand-writing JSON.

The contract currently lives inside the daemon
(<https://github.com/craigcode/sgian>, `docs/native-ipc.md`). It will be
published here when the types are split out. Until then this crate is
intentionally empty.
