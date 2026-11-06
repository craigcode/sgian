# Sgian

A tmux for coding agents: a Rust daemon (src-tauri) with native macOS (apps/macos) and Windows (apps/windows) clients, a Tauri Linux client and a served web client (ui).

## Map
- src-tauri/src: the daemon. Module guide: docs/daemon-modules.md
- crates/sgian-protocol, crates/sgian-pty: published crates
- ui: shared web UI; tests in ui-tests (vitest)
- apps: native clients
- scripts: build, verify and release helpers
- docs/native-ipc.md: the wire contract
- docs/design: design notes
- Roadmap: ENHANCEMENTS.md. Releases: docs/releasing.md and docs/public-readiness.md

## Rules
- Do not bump versions or suggest tags unless asked. CHANGELOG entries go under Unreleased.
- Base PRs on main. Branch protection is strict, so each PR needs a rebase before merge.
- Use `git commit -F -` with a heredoc; quotes inside `-m "..."` break.

Kranz lives in ~/Data/kranz.
