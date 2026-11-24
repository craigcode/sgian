# Sgian 0.2.0 release readiness — November 24, 2026

Baseline source: `2946326b4b2b9b4f5910595a4877748ea370d144` (merged #133).
Its four main-branch CI jobs passed in
[run 45297524152](https://github.com/craigcode/sgian/actions/runs/45297524152),
including packaged macOS, Windows and Linux smoke checks. This pass adds
release tooling checks and a small native chat layout correction described
below. Version remains `0.2.0`, prepared but not tagged or published.

Host: Apple Silicon, macOS 26.6.2 (25G83), with Rosetta already installed.
Windows hands-on testing is deferred at the owner's request. Its existing
`0.2.0` transfer kit remains available; this pass does not claim it was run.

## Packaging rehearsal

Built the universal native client and bundled Rust daemon, then packaged a
DMG with `SGIAN_PACKAGE_REHEARSAL=1` and a matching ephemeral Sparkle key pair.
The key was generated in a temporary directory without modifying Keychain;
the private half was deleted after verification. Production credentials were
not used. The rehearsal artifacts remain under ignored build directories.

Passed:

- Both client and helper carry `arm64` and `x86_64` slices, with deep/strict
  ad-hoc signature validation and version `0.2.0`.
- DMG integrity verification, read-only mount, copy into an isolated install
  directory and signature verification of the copied app.
- Sparkle appcast generation, independent verification against the public
  key embedded in the app, and rejection with the wrong public key.
- Intel-only copies of both client and helper ran the native two-terminal,
  persisted split-layout and terminal-search smoke under Rosetta.

The initial rehearsal DMG, before the narrow-chat fix, has SHA-256
`fb9e7c9f20560725a3e44bb9e604c042a9dd82380e8907a3cffb469fb4656f96`.
Its source identity stays at the baseline above; it is not relabelled as the
later UI fix. Rosetta execution is not physical Intel hardware acceptance.
Ad-hoc packaging is not Developer ID signing, notarization, Gatekeeper trust,
a clean production installation or an installed Sparkle update.

## Live native acceptance

Used separately identified QA apps and new temporary workspaces through the
background accessibility driver, with no explicit app activation requested.
No private project files were sent to the provider services; prompts requested
only fixed response markers and explicitly forbade tools or file inspection.

| Check | Result |
| --- | --- |
| Claude live response through the bundled daemon | Pass; streamed marker and successful turn recorded, then displayed in the native client |
| Codex default local model | Correctly displayed a provider error: the local default model is unavailable to the logged-in ChatGPT account |
| Codex with locally advertised `gpt-5.6-sol` | Pass; streamed marker and successful turn recorded |
| Native Codex composer → provider → rendered reply | Pass; follow-up sent with the UI Send button, busy/interrupt state observed, completed reply displayed |
| Keyboard take and hand-back | Pass through the command palette and release dialog; release disabled without a note, enabled with a note; daemon holder cleared afterward |
| Quit and reopen | Pass; pane layout and provider history restored; zooming the Codex pane revealed both completed replies |
| Gemini with installed CLI 0.42.0 | Blocked by provider-side `UNSUPPORTED_CLIENT`, plus an untrusted temporary-workspace diagnostic |
| Gemini with isolated current CLI 0.71.1 | Blocked by the same provider-side `UNSUPPORTED_CLIENT` after explicitly trusting only the empty test process |
| Droid/Factory | Deferred; no valid subscription |

The Gemini retry used a temporary npm prefix and process-scoped trust, leaving
the user's global CLI, credentials and trust configuration unchanged. Google's
service rejected this account/client combination; this is not evidence of a
successful Gemini session. Sgian displayed the error and completed the failed
turn instead of remaining busy.

## Finding and release checks

Five narrow panes exposed a fixed 100-point spacer before user chat messages:
at those widths it consumed nearly all text space, rendering long blank strips.
Reducing the minimum spacer to 12 points lets it expand on wide panes while
keeping narrow messages readable. Conversation history itself was intact.
The rebuilt universal app passes the same five-pane reproduction: both user
prompts and completed replies are visible at the original window dimensions.
The final build passes deep/strict ad-hoc signature checks for both slices.
Final binary source is `e5ca2e8af083a9fbad6c8ecd85c76686447b0099`; the following
documentation update does not change those binaries. Packaged native smoke
passes on Apple Silicon and with the Intel-only client/helper under Rosetta.
The final rehearsal DMG also passes packaging, Sparkle verification and DMG
integrity, with SHA-256
`49b13fdadd845171e8550ede7c0d69c8712486d5382b64878f943b69a2cc6f01`.
It is saved under `final-rehearsal/`, distinct from the initial DMG above.
Tracking PR: [#136](https://github.com/craigcode/sgian/pull/136).

`release:check` now validates the npm root lock entry, the Sgian Cargo lock
entry, both macOS version fields and the Windows Identity version, alongside
the existing manifests. It rejects leading-zero/prerelease versions and values
outside Windows' package-version range. Ten regressions cover these guards.

`release:preflight` reads GitHub environment protection, secret names and public
variables only. It reports missing inputs, handles PFX precedence over Azure,
checks basic public-variable validity, and never dispatches a workflow or
reads private-key values. Seven tests cover both signing paths and failures.
A passing metadata check would still not prove the signing credentials work.

Local checks: **511 frontend tests**, **32 Swift tests** including a live daemon
round-trip, and **6 packaging/signature tests** pass. The signing guide now uses
a matching throwaway key pair for rehearsals; the previous instruction to mix
a production public key with a throwaway private key would correctly fail.

## Environment and remaining gates

Configured and read back the `native-release` environment's required reviewer
as `craigcode`, retaining protected-branch deployment. Self-review is enabled
so the sole owner can approve their own dispatched run. This does not approve
any run or publish any artifact.

The repository and release environment still have no signing secrets or
variables. This Mac has an Apple Development identity but no Developer ID
Application identity. The read-only preflight fails with the missing signing
inputs as intended.

Remaining work before public installers:

- Owner provisions Apple Developer ID/notarization, Sparkle, Linux updater
  credentials and either Azure Trusted Signing or an existing Windows PFX.
- Production-signed candidate installation and an installed update on each
  supported platform; public updater feeds must resolve after publication.
- Physical Windows and Intel Mac checks, real sleep/wake, VoiceOver/Narrator,
  and authenticated provider tool permission prompts.
- Resolve Gemini account/service access before claiming its live acceptance.
  Droid/Factory remains deferred without a subscription.

Evidence is stored locally in `apps/macos/build/acceptance-0.2.0-20261124/`:
build/test/preflight logs, sanitized provider outcomes, screenshots, DMG
metadata and the Intel smoke marker. See [public-readiness.md](public-readiness.md)
for the current checklist and [release-signing-setup.md](release-signing-setup.md)
for the owner's credential steps.
