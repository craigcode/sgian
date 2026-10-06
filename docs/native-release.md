# Native desktop release

The shipping macOS client is SwiftUI/AppKit with SwiftTerm. The shipping Windows
client is WinUI 3 with a restricted local WebView2/xterm terminal surface. Both
use the bundled Rust daemon. Linux continues to use Tauri. These are release
candidates until the production signing and installed-update acceptance below
have been completed; an ad-hoc macOS app or unsigned MSIX is a development build.

## Client workflows

Both native clients provide persistent horizontal/vertical splits, draggable
separators, pane zoom, keyboard navigation, terminal search, command discovery,
recent workspaces, terminal and Claude/Droid chat panes, permission decisions,
restart/rename/close, and workspace settings with named profiles. Layouts use
the same daemon schema as Tauri, so switching clients preserves the arrangement.
Closing the window leaves the daemon and sessions running. Closing a pane stops
its process and requires confirmation. Each macOS window owns its terminal
views; commands and Settings follow the active window.

The settings forms edit shell arguments, provider executable paths, permission
mode, restoration policy and profiles. They preserve configuration fields they
do not expose, including environment variables and scrub lists. Font size is a
client preference. Shell/provider changes affect newly started sessions.

| Action | macOS | Windows |
| --- | --- | --- |
| New terminal / split right | Command-D (also Command-T) | Control-Shift-D (new terminal: Control-T) |
| Split down | Command-Shift-D | Control-Shift-E |
| Zoom / show all panes | Command-Shift-Z | Control-Shift-Z |
| Next / previous pane | Command-Shift-] / [ | Control-Tab / Control-Shift-Tab |
| Find in terminal | Command-Shift-F | Control-Shift-F |
| Commands and pane search | Command-Shift-P | Control-Shift-P |
| Close pane | Command-Shift-W | Close button |

On macOS, recent workspaces are in the Workspace menu. On Windows, recent
workspaces are searchable in Commands. Terminal keyboard behavior remains
owned by the terminal; for example, Control-C still interrupts the shell.

## Build and validate

```sh
npm ci --ignore-scripts
scripts/verify-macos-native.sh
scripts/build-macos-native.sh
python3 scripts/test-native-release.py
```

The macOS builder defaults to the host architecture and release optimization.
`SGIAN_MAC_ARCH=universal` builds both Apple Silicon and Intel slices, including
the Rust helper. `SGIAN_BUILD_CONFIGURATION=debug` selects a development build.
The builder embeds Sparkle, its nested helpers, resources and licenses, then
signs nested code before the outer app. Versions come from `package.json`.

The packaging step can be rehearsed without Apple credentials:

```sh
SGIAN_MAC_ARCH=universal SGIAN_SPARKLE_PUBLIC_KEY=<throwaway public key> scripts/build-macos-native.sh
SGIAN_PACKAGE_REHEARSAL=1 SGIAN_SPARKLE_PRIVATE_KEY_FILE=<throwaway key file> scripts/package-macos-native.sh
```

A rehearsal runs the bundle checks, builds the DMG, generates the Sparkle
appcast and verifies the archive against the embedded public key with a
throwaway Ed25519 key pair, with ad-hoc signing and no notarization. Its
output lands in `apps/macos/build/rehearsal`, which the release workflow
never uploads from, and nothing in it may be published. Use a throwaway
key pair, never the release key: `generate_keys --account sgian-rehearsal
-x <file>` from Sparkle's `bin` directory makes one under its own keychain
account and exports it. To make one outside the keychain instead, the file
Sparkle reads is the base64 of 96 bytes: the 64-byte private key in the
form its bundled Ed25519 library uses (SHA-512 of the 32-byte seed, clamped,
with the second half as the prefix) followed by the 32-byte public key. A
file holding the raw seed signs with the wrong key and fails the
verification step, which is the check working as intended.

On Windows with Rust, Node 22, .NET 8 and the Windows SDK:

```powershell
./scripts/verify-windows-native.ps1
./scripts/build-windows-native.ps1
./scripts/smoke-windows-native.ps1 -Exe apps/windows/build/win-x64/Sgian.Windows.exe -Workspace "$env:TEMP/sgian-native-test" -Marker "$env:TEMP/sgian-native-test-ok"
```

Public Windows packages target x64 (usable under Windows ARM emulation).
`-RuntimeIdentifier win-arm64` also selects an ARM64 Rust helper for development;
native ARM64 public distribution requires its own Windows runtime acceptance.
Portable ZIPs are development artifacts. Signed MSIX installed through the
App Installer descriptor is the supported Windows distribution/update path.

CI exercises two native terminal surfaces, persisted split layout and terminal
search, in addition to protocol tests and daemon smoke checks. It does not
replace real provider-TUI, accessibility or installed-update acceptance.

## Signing configuration

Configure a protected GitHub Actions environment named `native-release` and
restrict it to the protected `main` branch. `release-signing-setup.md`
walks through obtaining each credential below. The environment needs:

| Secret / variable | Purpose |
| --- | --- |
| `APPLE_CERTIFICATE` | Base64 Developer ID Application P12 |
| `APPLE_CERTIFICATE_PASSWORD` | P12 password |
| `APPLE_SIGNING_IDENTITY` | Developer ID Application identity |
| `APPLE_API_KEY_P8` | Base64 App Store Connect P8 key |
| `APPLE_API_KEY`, `APPLE_API_ISSUER` | Notary API identifiers |
| `SPARKLE_PRIVATE_KEY` | Sparkle Ed25519 private key exported as base64 text |
| **Variable** `SPARKLE_PUBLIC_KEY` | Corresponding 32-byte public key, base64 |
| `WINDOWS_CERTIFICATE` | Base64 Windows Authenticode PFX |
| `WINDOWS_CERTIFICATE_PASSWORD` | PFX password |
| `TAURI_SIGNING_PRIVATE_KEY`, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | Existing Linux updater signing key |

Keep signing keys in their protected secret store and retain secure backups.
Do not paste them into an issue or PR. The Windows builder derives the package
Publisher from the certificate, stamps the package version, signs the daemon
and portable app, creates and verifies the signed MSIX, and restores the source
manifest afterward. It removes only certificates imported by that build.

## Create a candidate

1. Bump `package.json`, the npm lockfile, Cargo package/lockfile and Tauri version
   together. macOS bundle and Windows package versions are stamped at build time.
2. Merge the reviewed source through the four required CI checks.
3. Run **Native release candidate** from `main`. It re-runs validation against
   that commit, then builds from clean source without compiled signing caches.
4. macOS produces a universal, Developer ID signed and notarized DMG. The workflow
   staples and validates notarization tickets on both the app and DMG, generates
   a Sparkle appcast, and independently verifies the archive against the public
   key embedded in the app.
5. Windows produces an Authenticode-signed MSIX; Linux produces a signed-updater
   AppImage and Debian package. Final assembly verifies signatures, checksums,
   versions and download URLs before preparing any output.
6. The workflow creates or updates a **draft** `vX.Y.Z` release. Re-runs refuse to
   replace a published release or a draft tag that points at another commit.
   No workflow automatically publishes the candidate.

Release assets include `appcast.xml`, `Sgian.appinstaller`, Linux `latest.json`,
and `SHA256SUMS.txt`. Asset download URLs are pinned to their versioned release.
The stable feed URLs use GitHub's latest release. This repository and its release
assets must be anonymously readable before public distribution, or the feed
and asset URLs must be migrated to a public distribution repository together.

## Required installed acceptance before publication

- Download the notarized DMG using a browser on clean Apple Silicon and Intel
  Macs. Install in Applications and launch through normal Gatekeeper checks.
- Install the Windows MSIX through `Sgian.appinstaller` on a clean Windows
  machine. Verify publisher identity and the bundled helper's launch.
- Exercise real Claude/Droid login and streaming, allow/deny prompts, terminal
  TUIs, resize/copy/paste/search, close/reopen and sleep/wake. Test VoiceOver and
  Narrator, including pane focus and splitter adjustment.
- Switch between two projects with identically named/numbered panes. Confirm
  neither terminal content nor pending input crosses workspaces.
- Install an older production-signed native version and update to the candidate
  using a staging feed. Check preserved layouts, conversations, existing daemon
  sessions and rollback/recovery behavior. A new client must remain compatible
  with an older running daemon; a protocol-breaking release requires an explicit
  migration rather than silently killing sessions.
- Verify Linux installation and its version-to-version update.
- Review release notes, supported OS/architecture claims, checksums, private
  vulnerability reporting and support contact. Publish the reviewed draft as
  the latest release only after these checks pass.

## Update and recovery policy

Sparkle checks automatically and asks before installation. Windows App Installer
checks on launch and in the background; Commands also offers a manual check.
Development builds clearly report that production updates are unavailable.

Do not overwrite a published binary or reuse its version. If a release fails,
stop promoting it, restore the previous known-good release as latest if safe,
and ship the fix at a higher version. A feed rollback does not downgrade users
who already installed the faulty version. Retain old signed installers and
state backups for deliberate recovery. Rotate one trust identity at a time and
prove a signed bridge update before retiring an old key/certificate.
