# Public-readiness gate

**Status (2026-09-16):** repository private; code and CI green; distribution
machinery unexercised. Making Sgian public exposes every reachable Git object
and every release asset. It is an operator action, separate from merging
ordinary changes. Keep the repository private until every item below is true,
then follow `releasing.md`.

Kranz needed a history rewrite into a fresh public origin. Sgian does not:
`scripts/audit-public-history.sh` passes on the existing origin (gitleaks over
every ref, no personal markers in patches, messages or identity headers, all
human commits under the GitHub noreply identity, no blobs over 5 MiB).

## Repository content

- [x] `LICENSE` (MIT), `SECURITY.md` with the security model and private
      reporting, `README.md` leading with the claim and the control plane.
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
- [ ] `CHANGELOG.md` finalised for the chosen version (draft exists).
- [ ] Version chosen and set in `src-tauri/Cargo.toml`, `package.json` and
      `src-tauri/tauri.conf.json` by the owner (`npm run release:check`
      verifies they agree). No tag exists yet.
- [ ] Repository description on GitHub reviewed; the current one predates the
      lease/ledger work.
- [x] Dependabot pull requests merged or closed so the lockfiles are stable
      for the version pull request (all seven landed 2026-09-16; vite 8 and
      plugin-react 6 went in together because each peers on the other).

## Working and stable

- [ ] **Windows hands-on acceptance** (ENHANCEMENTS §1): Claude Code's
      full-screen TUI in the packaged app through splits and resizes; the
      lease dialog, unattended badge and project views exercised by a person.
      CI compiles the WinUI client; nobody has used these screens on it.
- [ ] **macOS architecture scope decided**: the workflow ships
      runner-architecture builds; the runbook promises a universal DMG. Make
      the workflow match the decision.
- [x] Rust, vitest, Swift and Windows protocol tests green on every job for
      every merged pull request; RustSec and npm audits are gates.
- [x] Output-flood, process-tree kill, IPC fault and transport soak
      coverage in the suite.

## Signing and updates

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
- [ ] Private vulnerability reporting enabled (unavailable while private).
- [ ] Dependabot alerts and security updates, secret scanning and push
      protection enabled where the plan permits.

## Visibility change

1. Record the passing audit output and the exact commit.
2. Owner approves public visibility explicitly.
3. Change visibility without pushing a tag or publishing a release.
4. Clone anonymously into a new directory; rerun
   `scripts/audit-public-history.sh`; confirm the README, `SECURITY.md`,
   licence and release list look right to a visitor.
5. Enable private vulnerability reporting and the environment approval rule.
6. Only then follow `releasing.md` for the first tagged release.
