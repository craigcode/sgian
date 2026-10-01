# Cutting a release

Sgian ships a notarized macOS DMG, a signed Windows MSIX plus App Installer
feed, and signed Linux packages, all from one manual GitHub workflow that
assembles a draft release only after every platform artifact validates. There
is no crates.io publication: the reserved crate names are placeholders.

## 0. Prerequisites

Every item in `public-readiness.md` is true. In particular the repository is
public, the signing secrets exist, and an installed-version update has been
exercised at least once on each platform.

These are operator gates. Neither Sgian nor a coding-agent session pushes
tags or publishes releases.

## Source release

A release without installers, as 0.1.0 is. It needs none of the signing
prerequisites above: the version pull request below, a passing
`scripts/audit-public-history.sh`, the visibility change in
`public-readiness.md`, then the owner creates the `v<version>` tag on the
merge commit and publishes a release whose notes are that version's
changelog section, with no artifacts attached. The README tells readers to
build from the tag. Sections 2 and 3 do not apply.

## 1. Version pull request

- The owner picks the version. Set it in `src-tauri/Cargo.toml`,
  `package.json` and `src-tauri/tauri.conf.json`; `npm run release:check`
  verifies they agree.
- Move the `Unreleased` section of `CHANGELOG.md` under the version and
  date. Keep the `Unreleased` heading.
- Merge through the normal pull-request path; all four CI checks must pass
  on the merge commit.

## 2. Candidate

- Run `scripts/audit-public-history.sh` from a full clone.
- Dispatch the native release workflow from protected `main`
  (`native-release.md`, "Create a candidate"). It builds, signs and verifies
  every platform artifact against the embedded keys and uploads a draft.
- Do not publish the draft yet.

## 3. Installed acceptance

Follow "Required installed acceptance before publication" in
`native-release.md` on clean hosts: install each artifact, run the packaged UI
smoke, exercise a shell pane and an agent pane, take and release a lease,
and confirm the previous candidate updates to this one.

## 4. Tag and publish

- Create the `v<version>` tag on the exact merge commit (administrators
  only, by the ruleset).
- Publish the draft release. Verify the updater feed resolves anonymously
  and that a previously installed build offers the update.

## 5. Close

- Note the tag, artifact digests and acceptance results in
  `docs/reviews/`.
- Open the next `Unreleased` section.
