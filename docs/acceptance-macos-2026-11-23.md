# Native release validation — November 23, 2026

Development candidate: `efe664b519d2fd28b993315ad74e2683728b775e`, based on
`026a1fe154c5f34276f335ddbbc1f8cfc653f325`. Documentation may be committed later;
artifact provenance identifies the source used to build each binary.
Version remains `0.1.0`; this does not replace the published source release.
The first installer release must use a new version.

Tracking PR: [#133](https://github.com/craigcode/sgian/pull/133).
Fresh CI: [45276671936](https://github.com/craigcode/sgian/actions/runs/45276671936).
Local host: macOS 26.6.2 (25G83), Apple Silicon. The universal build includes
Intel slices, but Intel hardware execution is not covered by this pass.

## Dependency refresh

The scheduled main-branch run failed on
[RUSTSEC-2026-0592](https://rustsec.org/advisories/RUSTSEC-2026-0592).
Refreshing the advisory database also found
[RUSTSEC-2026-0594](https://rustsec.org/advisories/RUSTSEC-2026-0594), published
on the day of this pass. The candidate patches `url` 2.5.8 → 2.5.9 and
`cookie` 0.18.1 → 0.18.3, with the compatible `form_urlencoded` patch and
Cargo's dependency resolution. No audit exclusions were added.

`cargo audit --file src-tauri/Cargo.lock` passes with zero blocking
vulnerabilities and four existing warnings: chrono (RUSTSEC-2026-0543),
pin-utils (RUSTSEC-2026-0441), rustix (RUSTSEC-2026-0486) and tokio
(RUSTSEC-2026-0386). These warnings remain follow-up work; a passing audit
is not a claim that they have been resolved. `npm audit --audit-level=moderate`
reports zero vulnerabilities.

## Validation results

| Check | Result |
| --- | --- |
| Frontend tests | Pass: 494 tests across 19 files |
| Release configuration consistency | Pass |
| Rust formatting and Clippy | Pass, including all targets with warnings denied |
| Release packaging/signature regression tests | Pass: 6 tests, including rejection of a bad signature |
| Native Windows terminal bridge tests (Node) | Pass: 3 tests |
| Rust tests on macOS | Pass: 595 passed, 1 manual throughput benchmark ignored; bounded to 4 test threads |
| Swift tests with real daemon IPC | Pass: 32 tests |
| Universal native macOS build and bundle signatures | Pass: arm64 + x86_64 client and helper; deep/strict ad-hoc signature verification |
| Packaged native Mac UI smoke | Pass: two rendered terminals, persisted layout and search |
| Live native Mac controls | Pass: keyboard entry and `pwd`, split down, zoom/unzoom, terminal search (1 / 3 matches), white sidebar |
| Concurrent control requests while attached | Pass: all 32 responses in 0.191 seconds |
| Quit/reopen and terminal output isolation | Pass: A → B → A with separate `term-1` markers; original two-pane layout and scrollback survive quit/relaunch |
| Stale search counter regression | Pass: B shows 1 / 2 matches; switching to A closes search and clears the old count |
| Four CI jobs | Pass: core checks/audits/Swift/Windows cross-compile, packaged Mac, native Windows, and Linux builds/smokes |
| Windows native CI build and runtime smoke | Pass: x64 portable ZIP and unsigned MSIX; transport, process-tree, daemon, Tauri and native WinUI smoke checks |
| Physical Windows acceptance | Not run; separate machine, verified transfer kit ready |

Local logs and screenshots are kept in the ignored
`apps/macos/build/acceptance-20261123/` directory. The build uses release
optimization and ad-hoc signatures. It is not Developer ID signed or notarized.
The live controls were driven through background accessibility automation in
an isolated copy of the app and temporary workspaces. The QA app/preferences
were removed and all four test daemons shut down after verification.

## Local deliverables

- Mac: `apps/macos/build/acceptance-20261123/Sgian-macos-universal-efe664b.zip`,
  with its `.sha256`, `BUILD-INFO.json`, instructions and evidence alongside it.
- Windows: `apps/windows/build/Sgian-windows-handoff-20261123-efe664b.zip`,
  with an outer checksum. The kit includes the CI portable ZIP and unsigned
  MSIX, source/run provenance, per-file checksums, smoke script, procedure and
  a results sheet whose hands-on items remain marked not run. GitHub artifact
  `14868971695` was verified against its published SHA-256 digest before
  extraction; the portable executable/helper are x64 and the unsigned MSIX
  manifest has version `0.1.0.0`.

## Finding addressed during the pass

Switching from project A to B with terminal search open retained A's match
count in B, even though B had separate terminal content. The native client
now closes search when resetting its connection and clears the displayed
result when the selected pane or query changes. The final candidate includes
this fix. The final universal build and 32 Swift tests pass; the native
workspace-switch reproduction confirms that the stale search state is gone.
Superseded CI runs were cancelled so the final source could be validated.

## Release gates that remain open

The repository is public. GitHub inspection on November 23 confirms that the
latest `v0.1.0` release has no assets, both repository and `native-release`
environment have zero secrets/variables, and the environment restricts
deployment to protected branches but has no required-reviewer rule.

Still required before public installers:

- Windows hands-on acceptance using [the handoff procedure](windows-acceptance.md),
  including the original ConPTY border/first-column and error-231 reproductions.
- Intel Mac execution, real window-resize interaction, copy/paste, pending
  input isolation, sleep/wake, VoiceOver and Windows Narrator. AX text
  inspection alone is not screen-reader acceptance.
- Current real provider login/streaming and permission prompts. Droid/Factory
  live checks remain deferred because no subscription is available.
- Production credentials and owner approval on the protected environment;
  Windows can use Azure Trusted Signing/OIDC or an existing exportable PFX.
- A fresh version, signed/notarized clean installs, a second signed candidate
  updating an installed first candidate, recovery checks and anonymous feeds
  on all three supported platforms.

No production credentials, release tags, published assets or updater feeds
were created by this validation pass. See [public-readiness.md](public-readiness.md)
for the release gate checklist.
