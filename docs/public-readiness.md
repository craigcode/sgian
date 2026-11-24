# Public-readiness gate

**Status (2026-11-24):** repository public; `v0.1.0` is still the latest
source-only release and has no downloadable assets. Version `0.2.0` is
prepared in source with [draft release notes](releases/0.2.0.md) and a
[dependency review](reviews/dependencies-0.2.0.md); it is not published. See the
[0.2.0 Mac acceptance report](acceptance-macos-2026-11-24.md) and
[Windows handoff](windows-acceptance.md). Every unticked item below gates
the first installer release; local and CI development builds do not complete
production signing, clean-machine installation or installed-update acceptance.

Kranz needed a history rewrite into a fresh public origin. Sgian does not:
`scripts/audit-public-history.sh` passes on the existing origin (gitleaks over
every ref, no personal markers in patches, messages or identity headers, all
human commits under the GitHub noreply identity, no blobs over 5 MiB).

## Repository content

- [x] `LICENSE` (MIT), `SECURITY.md` with the security model and private
      reporting, `README.md` leading with the claim and the control plane.
- [x] External full-repo review (`docs/review-2026-09-20.md`): three P1s and
      eleven P2/P3s fixed in #53 and #54; accepted risks recorded.
- [x] Trust-surface review of the credential hello, hook and status-line
      inputs and the dossier (`docs/trust-surface-review-2026-09-19.md`);
      six findings fixed, six risks recorded as accepted with reasons.
- [x] `scripts/audit-public-history.sh` passes from a full clone with every
      branch and tag (`git fetch --all --tags`), with gitleaks installed.
      CI runs the same scan over every fetched ref, so a hit on any pushed
      branch fails the check job on every open pull request until it is
      fixed. A reviewed false positive is pinned by its exact fingerprint in
      `.gitleaksignore` (commit, file, rule, line); never by path or rule.
- [x] crates.io names held by the owner: `sgian` 0.0.1, `sgian-pty` 0.0.1,
      `sgian-protocol` 0.0.1 (placeholders whose READMEs state the intent).
- [x] `CHANGELOG.md` and draft release notes prepared for `0.2.0`, marked
      unreleased until the actual publication date is known.
- [x] Version `0.2.0` approved by the owner and set in Cargo, npm, Tauri and
      native manifest defaults, including both lockfiles.
      `npm run release:check` verifies all app manifests and lockfile versions
      agree, including native defaults and Windows package version limits.
- [x] Repository description and topics set on 2026-09-30: "Terminal multiplexer for supervising
      coding agents: a Rust daemon keeps shells and agent sessions alive per
      workspace, with native macOS and Windows clients, keyboard leases, a
      hash-chained ledger and a scriptable ctl." Suggested topics:
      `terminal-multiplexer`, `coding-agents`, `claude-code`, `rust`,
      `tauri`, `swiftui`, `winui3`, `agent-supervision`.
- [x] Dependabot pull requests merged or closed so the lockfiles are stable
      for the version pull request (all seven landed 2026-09-16; vite 8 and
      plugin-react 6 went in together because each peers on the other).

## Working and stable

- [x] macOS hands-on acceptance of the packaged native app with a real Claude
      Code session (`docs/acceptance-macos-2026-09-23.md`): passed, with four
      non-blocking findings and the step list the Windows pass should follow.
      Packaged build, terminal controls, workspace output isolation and
      quit/reopen checks were refreshed through background automation on
      [November 23](acceptance-macos-2026-11-23.md); this does not refresh
      authenticated provider or screen-reader acceptance.
      The [November 24 pass](acceptance-macos-2026-11-24.md) additionally covers
      live Claude and Codex replies, the native chat composer, keyboard
      take/hand-back and conversation restoration. Permission prompts and
      screen-reader acceptance remain open.
- [ ] **Windows hands-on acceptance** (ENHANCEMENTS §1): Claude Code's
      full-screen TUI in the packaged app through splits and resizes; the
      lease dialog, unattended badge and project views exercised by a person.
      CI builds and runs the native two-terminal smoke and daemon checks;
      physical acceptance on the separate Windows machine remains pending.
      Use [windows-acceptance.md](windows-acceptance.md) and the dated kit.
- [x] **macOS architecture scope decided: universal.** One app covers Apple
      Silicon and Intel. The release workflow already builds with
      `SGIAN_MAC_ARCH=universal` (only the per-push validation job builds for
      the runner's architecture), and a local universal build on 2026-11-23
      produced a client and a bundled helper that both carry `x86_64` and
      `arm64` slices. Intel-only client/helper smoke passed under Rosetta on
      November 24; execution on physical Intel hardware is still pending.
- [x] Merged `0.2.0` commit `2946326` passes all four CI jobs, including Rust, vitest,
      Swift, Windows protocol/runtime checks and dependency audits:
      [run 45297524152](https://github.com/craigcode/sgian/actions/runs/45297524152).
      Earlier `0.1.0` artifacts retain their original source identity.
      The `0.2.0` preparation additionally patches Rustix and Tokio and removes
      `pin-utils`; only inactive optional Chrono remains as an unignored
      informational warning. See the dependency review. Subsequent fixes must
      pass the same four required checks before merging and making a candidate.
- [x] Output-flood, process-tree kill, IPC fault and transport soak
      coverage in the suite.

## Signing and updates

0.1.0 ships as source, so nothing in this section gates it. Every item here
gates the first release that carries installers. The macOS packaging path
was rehearsed again for `0.2.0` on November 24 with a matching throwaway
Sparkle key pair: universal bundle checks, DMG integrity/mount/copy, appcast,
independent signature verification and wrong-key rejection passed. This is
development packaging evidence; it does not complete notarization, clean
production installation or installed-update acceptance.

- [ ] Provision the protected environment's credentials from
      [native-release.md](native-release.md); [release-signing-setup.md](release-signing-setup.md)
      is the walk-through. As of November 24, both the repository and
      `native-release` environment have **zero secrets and variables**.
      Windows supports Azure Trusted Signing through OIDC or an existing
      exportable PFX; choose and provision one path. Apple Developer ID,
      notarization, Sparkle and Linux updater credentials are also required.
      `npm run release:preflight` reports the missing environment inputs
      without reading secret values or dispatching a workflow.
- [ ] A candidate built by the manual workflow from protected `main`
      installs on clean macOS, Windows and Linux hosts.
- [ ] A second candidate updates an installed first candidate (Sparkle on
      macOS, App Installer on Windows, Tauri updater on Linux). Until an
      installed-version update has been observed, the updater is untested
      code with keys in it.
- [ ] The stable Sparkle `appcast.xml`, Windows `Sgian.appinstaller` and
      Linux `latest.json` URLs under
      `https://github.com/craigcode/sgian/releases/latest/download/` resolve
      anonymously and reference verified assets. The repository is public;
      the missing installer release and feed assets are the remaining issue.

## GitHub controls

- [x] `main` ruleset: pull requests, all four CI checks, resolved
      conversations, no force-push or deletion (set 2026-09-05).
- [x] Only administrators create, update or delete `v*` tags.
- [x] The `native-release` environment exists; the release workflow is
      `workflow_dispatch` only.
- [x] The `native-release` environment requires `craigcode` approval for the
      release workflow, configured and verified November 24. Protected-branch
      deployment is preserved; self-review is allowed for the sole owner.
      The workflow creates a draft; publication remains separate.
- [x] Private vulnerability reporting enabled (2026-09-30).
- [x] Dependabot alerts and security updates, secret scanning and push
      protection enabled (2026-09-30).

## Remaining acceptance scope

- [ ] Physical Intel Mac execution (Rosetta smoke passed), real sleep/wake
      and VoiceOver.
- [ ] Current authenticated provider sessions and permission prompts on both
      native clients. Live Claude and Codex responses passed on macOS;
      tool permission prompts remain open. Gemini is blocked by Google's
      `UNSUPPORTED_CLIENT` response for the available account with both CLI
      0.42.0 and an isolated current 0.71.1 installation. Droid/Factory
      live acceptance is **deferred because no subscription is available**;
      protocol tests do not replace it. Do not claim it passed.
- [ ] Clean signed installs and a second signed candidate updating the first
      on macOS, Windows and Linux, including recovery and preserved sessions.

Public visibility and the source release were completed in September 2026;
see [the source-release review](reviews/release-0.1.0.md). Do not repeat that
transition or reuse its published version for installer artifacts.
