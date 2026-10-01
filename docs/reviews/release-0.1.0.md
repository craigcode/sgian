# Release 0.1.0

The first public release, published as source on 2026-09-30.

| | |
| --- | --- |
| Tag | `v0.1.0`, annotated, on the repository's initial commit |
| Release | <https://github.com/craigcode/sgian/releases/tag/v0.1.0>, notes from the changelog's 0.1.0 section, no artifacts |
| Visibility | Made public the same day, before the tag, by the owner's instruction |
| Version files | `src-tauri/Cargo.toml`, `package.json`, `package-lock.json`, `src-tauri/tauri.conf.json` all `0.1.0`; `npm run release:check` verified |

## History

`main` begins at the release. The development history that led to it, 58
commits merged through pull requests #1 to #73, was squashed into the
initial commit on 2026-09-30 by the owner's decision, a few hours after the
repository became public and while it had no forks. The pull requests remain
on GitHub with their diffs and discussion and are the record the changelog
and the review documents cite by number. Commit hashes quoted in documents
written before the release refer to that earlier history and are no longer
on a branch.

`v0.1.0` was first published on the last pre-squash commit of the release
itself and was moved to the initial commit the same day. The two trees
differ only by this record, the matching lines in `SECURITY.md` and the
readiness checklist, and a lockfile update of `serde_with` for
GHSA-7gcf-g7xr-8hxj, which reaches Sgian only through Tauri's build tooling.

## Checks

- The same source passed all four required CI checks before the squash,
  including the packaged-app smokes on macOS, Linux and Windows, and CI runs
  again on the initial commit.
- `scripts/audit-public-history.sh` passed in the working clone before the
  visibility change and again in an anonymous clone after it (no leaks,
  every human commit under a GitHub noreply identity).
- After the change: the repository, the release page and the latest-release
  API all answer without credentials.

## Enabled at the visibility change

Private vulnerability reporting, secret scanning, push protection, Dependabot
alerts and Dependabot security updates. The description and topics were set
at the same time.

## What 0.1.0 is not

It carries no installers. The signing secrets do not exist, no candidate has
been installed on a clean machine, no installed update has been observed, and
the Windows client has not been used by hand. Those items remain open in
`public-readiness.md` and gate the first installer release. The macOS client
had a hands-on pass (`docs/acceptance-macos-2026-09-23.md`).

## Last changes before the release

- #70: Tauri 2.12 and its plugins.
- #71: Settings no longer resets configuration it does not show, agent
  profiles from native Settings save, and the web Settings form cannot be
  saved before it has loaded (issues #31, #32, #33).
