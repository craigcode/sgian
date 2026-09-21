## What and why

<!-- One or two sentences. The PR title becomes the squash-commit subject. -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` pass in `src-tauri`
- [ ] `npm test` passes
- [ ] `CHANGELOG.md` has a line under `Unreleased` if a user or integrator would notice
- [ ] Wire contract changes (requests, events, snapshot fields, `ctl` verbs) are recorded in `docs/native-ipc.md` and reflected in the native clients
- [ ] Security-relevant changes cite `SECURITY.md` or a note under `docs/design/`
- [ ] No version bumps or tags
