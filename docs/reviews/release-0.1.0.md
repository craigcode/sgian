# Release 0.1.0

The first public release, published as source on 2026-09-30.

| | |
| --- | --- |
| Tag | `v0.1.0`, annotated, on `775c23479ced72ed4f9c14807c78c5bf15bc25d1` |
| Release | <https://github.com/craigcode/sgian/releases/tag/v0.1.0>, notes from the changelog's 0.1.0 section, no artifacts |
| Visibility | Made public the same day, before the tag, by the owner's instruction |
| Version files | `src-tauri/Cargo.toml`, `package.json`, `package-lock.json`, `src-tauri/tauri.conf.json` all `0.1.0`; `npm run release:check` verified |

## Checks on the tagged commit

- All four required CI checks passed on `775c234` as the merge commit of #71,
  including the packaged-app smokes on macOS, Linux and Windows.
- `scripts/audit-public-history.sh` passed in the working clone before the
  visibility change and again in an anonymous clone after it (56 commits,
  no leaks, every human commit under a GitHub noreply identity).
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

## Landed between the release paperwork and the tag

- #70: Tauri 2.12 and its plugins.
- #71: Settings no longer resets configuration it does not show, agent
  profiles from native Settings save, and the web Settings form cannot be
  saved before it has loaded (issues #31, #32, #33).
