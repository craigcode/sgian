# Public-readiness gate

**Status (2026-09-30):** repository public; `v0.1.0` published as a source
release (`docs/reviews/release-0.1.0.md`). The distribution machinery is
still unexercised: every unticked item below gates the first release that
carries installers, and none gated 0.1.0.

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
- [x] `CHANGELOG.md` finalised for the chosen version: `0.1.0 - 2026-09-30`.
- [x] Version chosen by the owner: `0.1.0`, already set in
      `src-tauri/Cargo.toml`, `package.json` and `src-tauri/tauri.conf.json`
      (`npm run release:check` verifies they agree). The owner creates the
      `v0.1.0` tag after the visibility change.
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
- [ ] **Windows hands-on acceptance** (ENHANCEMENTS §1): Claude Code's
      full-screen TUI in the packaged app through splits and resizes; the
      lease dialog, unattended badge and project views exercised by a person.
      CI compiles the WinUI client; nobody has used these screens on it.
- [x] **macOS architecture scope decided: universal.** One app covers Apple
      Silicon and Intel. The release workflow already builds with
      `SGIAN_MAC_ARCH=universal` (only the per-push validation job builds for
      the runner's architecture), and a local universal build on 2026-10-04
      produced a client and a bundled helper that both carry `x86_64` and
      `arm64` slices. The Intel slice has not been run on Intel hardware.
- [x] Rust, vitest, Swift and Windows protocol tests green on every job for
      every merged pull request; RustSec and npm audits are gates.
- [x] Output-flood, process-tree kill, IPC fault and transport soak
      coverage in the suite.

## Signing and updates

0.1.0 ships as source, so nothing in this section gates it. Every item here
gates the first release that carries installers.

- [ ] The 11 repository secrets and the `SPARKLE_PUBLIC_KEY` variable named
      in `native-release.md` are provisioned. As of 2026-09-16 the
      repository has **zero** secrets and variables.
- [ ] A candidate built by the manual workflow from protected `main`
      installs on clean macOS, Windows and Linux hosts.
- [ ] A second candidate updates an installed first candidate (Sparkle on
      macOS, App Installer on Windows, Tauri updater on Linux). Until an
      installed-version update has been observed, the updater is untested
      code with keys in it.
- [ ] The updater feed URL
      (`https://github.com/craigcode/sgian/releases/latest/download/latest.json`)
      resolves anonymously; it cannot while the repository is private.

## GitHub controls

- [x] `main` ruleset: pull requests, all four CI checks, resolved
      conversations, no force-push or deletion (set 2026-09-05).
- [x] Only administrators create, update or delete `v*` tags.
- [x] The `native-release` environment exists; the release workflow is
      `workflow_dispatch` only.
- [ ] The `native-release` environment requires owner approval for the
      publish job (a paid-plan feature on private repositories; verify after
      the visibility change).
- [x] Private vulnerability reporting enabled (2026-09-30).
- [x] Dependabot alerts and security updates, secret scanning and push
      protection enabled (2026-09-30).

## Visibility change

1. Record the passing audit output and the exact commit.
2. Owner approves public visibility explicitly.
3. Change visibility without pushing a tag or publishing a release.
4. Clone anonymously into a new directory; rerun
   `scripts/audit-public-history.sh`; confirm the README, `SECURITY.md`,
   licence and release list look right to a visitor.
5. Enable private vulnerability reporting and the environment approval rule.
6. Only then follow `releasing.md` for the first tagged release.
