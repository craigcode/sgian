# sgian-pty

This name is reserved for Sgian's PTY layer: `CreatePseudoConsole` plus a
kill-on-close Job Object on Windows, `openpty`/`fork`/`execvp` with a process
group on POSIX, and the kill-tree semantics a closed pane needs.

Today that code lives inside the Sgian daemon
(<https://github.com/craigcode/sgian>, `src-tauri/`). It will be published
here when it is split out so that other projects, Kranz first, can depend on
it from crates.io. Until then this crate is intentionally empty.
