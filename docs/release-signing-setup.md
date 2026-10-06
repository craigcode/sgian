# Provisioning the release signing credentials

The native release workflow needs eleven secrets and one variable, all on the
`native-release` GitHub environment. This is the walk-through for obtaining
each one, in the order that avoids waiting on anyone twice. Nothing here is
run by Sgian or by an agent session; these are operator steps, and every
secret is created by you and pasted by you.

Budget about half a day the first time, most of it waiting on Apple and on
the Windows certificate authority. The Apple Developer Program costs US$99 a
year; a Windows code-signing certificate costs a few hundred dollars a year
from a certificate authority, or about US$10 a month through Azure Trusted
Signing.

## What goes where

| Name | Kind | Holds |
| --- | --- | --- |
| `APPLE_CERTIFICATE` | secret | Developer ID Application certificate and key, as a base64 PKCS#12 file |
| `APPLE_CERTIFICATE_PASSWORD` | secret | The password that file was exported with |
| `APPLE_SIGNING_IDENTITY` | secret | The certificate's exact common name, `Developer ID Application: <Name> (<TEAMID>)` |
| `APPLE_API_KEY_P8` | secret | App Store Connect API key, the `.p8` file, base64 |
| `APPLE_API_KEY` | secret | That key's ID, ten characters |
| `APPLE_API_ISSUER` | secret | The issuer ID shown beside the key |
| `SPARKLE_PRIVATE_KEY` | secret | The Sparkle private key as exported by `generate_keys -x`, verbatim text |
| `SPARKLE_PUBLIC_KEY` | **variable** | The matching 32-byte public key, base64 |
| `WINDOWS_CERTIFICATE` | secret | Authenticode code-signing certificate and key, as a base64 PFX file |
| `WINDOWS_CERTIFICATE_PASSWORD` | secret | The PFX password |
| `TAURI_SIGNING_PRIVATE_KEY` | secret | The Tauri updater (minisign) private key file, verbatim text |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | secret | Its password, empty if none |

Set each one on the environment, not on the repository, so only the release
job can read it:

```bash
gh secret set APPLE_CERTIFICATE --env native-release < cert.p12.b64
gh variable set SPARKLE_PUBLIC_KEY --env native-release --body "<public key>"
```

`gh secret set NAME --env native-release` with no input prompts for the
value, which is the right way to enter a password or an ID. Never put a
value on a command line that a shell history would keep.

## 0. The environment itself

The `native-release` environment exists and is limited to protected
branches. Add yourself as a required reviewer so the publish job waits for
an explicit approval: Settings, Environments, `native-release`, Required
reviewers. This is free on a public repository.

## 1. Apple: Developer ID certificate

Needs an active Apple Developer Program membership for the account that
owns the team ID.

1. On the Mac you will keep the key on, open Keychain Access. From the
   Keychain Access menu choose Certificate Assistant, then Request a
   Certificate From a Certificate Authority. Enter your email and name, pick
   Saved to disk, and save the request file. This creates the private key in
   your login keychain.
2. At <https://developer.apple.com/account/resources/certificates/add>
   choose **Developer ID Application**, upload the request file, and
   download the certificate.
3. Double-click the downloaded `.cer` so it pairs with the key in Keychain
   Access. In My Certificates you should now see
   `Developer ID Application: <Name> (<TEAMID>)` with a disclosure
   triangle and a private key under it.
4. Confirm the identity and copy its exact name:

   ```bash
   security find-identity -v -p codesigning
   ```

   The quoted string is `APPLE_SIGNING_IDENTITY`.
5. Export: right-click the certificate entry, Export, format Personal
   Information Exchange (.p12), and give it a strong password. Keychain
   Access includes the private key when you export the certificate entry.
   If a later `codesign` on the runner fails with "unable to build chain",
   select both the certificate and the **Developer ID Certification
   Authority** certificate before exporting so the intermediate travels
   with it.
6. Encode and set:

   ```bash
   base64 -i DeveloperID.p12 -o cert.p12.b64
   gh secret set APPLE_CERTIFICATE --env native-release < cert.p12.b64
   gh secret set APPLE_CERTIFICATE_PASSWORD --env native-release
   gh secret set APPLE_SIGNING_IDENTITY --env native-release
   rm cert.p12.b64
   ```

Keep the `.p12` in your password manager and delete it from disk. The
certificate is valid for five years; the renewal is the same steps.

## 2. Apple: notarization key

Notarization uses an App Store Connect API key, not your Apple ID.

1. At <https://appstoreconnect.apple.com/access/integrations/api> under
   Team Keys, generate a key named for its purpose, for example
   `sgian-notarization`, with the **Developer** role.
2. Download the `.p8` file. Apple lets you download it once; store it in
   your password manager immediately.
3. Note the **Key ID** on that row and the **Issuer ID** above the table.
4. Check it works before storing it, from a Mac with Xcode:

   ```bash
   xcrun notarytool history --key AuthKey_XXXXXXXXXX.p8 --key-id XXXXXXXXXX --issuer <issuer id>
   ```

   An empty history is a success; an authentication error means the role or
   the IDs are wrong.
5. Encode and set:

   ```bash
   base64 -i AuthKey_XXXXXXXXXX.p8 -o key.p8.b64
   gh secret set APPLE_API_KEY_P8 --env native-release < key.p8.b64
   gh secret set APPLE_API_KEY --env native-release
   gh secret set APPLE_API_ISSUER --env native-release
   rm key.p8.b64
   ```

## 3. Sparkle update signing key, macOS

The key that signs every macOS update. Losing it means shipped apps can
never update to a build signed by a new key, so back it up before anything
else.

1. Build the app once so Sparkle's tools are on disk:

   ```bash
   SGIAN_MAC_ARCH=universal scripts/build-macos-native.sh
   cd apps/macos/.build/native-x86_64/artifacts/sparkle/Sparkle/bin
   ```

2. Generate the pair under its own keychain account, so it never mixes
   with a rehearsal key:

   ```bash
   ./generate_keys --account sgian-release
   ```

   It prints the public key as a `SUPublicEDKey` line. That base64 string is
   `SPARKLE_PUBLIC_KEY`. Running the command again prints the same key; it
   does not replace it.
3. Export the private key and store it:

   ```bash
   ./generate_keys --account sgian-release -x sparkle-release.key
   gh secret set SPARKLE_PRIVATE_KEY --env native-release < sparkle-release.key
   gh variable set SPARKLE_PUBLIC_KEY --env native-release --body "<the SUPublicEDKey value>"
   ```

   The workflow writes the secret to a file verbatim and hands that file to
   `generate_appcast`, so the secret is the exported text exactly as the
   file holds it, not re-encoded. Put `sparkle-release.key` in your password
   manager, then delete it from disk.

The public key is embedded in the app at build time from the variable, and
the packaging step verifies every archive against it independently of
Sparkle, so a mismatched pair fails the build rather than shipping.

## 4. Tauri updater key, Linux

The Linux bundles use Tauri's updater, which signs with a minisign key.
`src-tauri/tauri.conf.json` already carries a public key, and the release
needs the private half of that same pair. Its minisign comment names the
key ID, so you can match it against a key file you hold:

```bash
python3 -c "import json,base64; print(base64.b64decode(json.load(open('src-tauri/tauri.conf.json'))['plugins']['updater']['pubkey']).decode().splitlines()[0])"
```

If you have that private key:

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY --env native-release < ~/.tauri/sgian.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --env native-release
```

If you do not, generate a new pair and replace the public key in
`tauri.conf.json` in a pull request before the first candidate. No Linux
build has shipped, so nothing in the field depends on the old key:

```bash
npm run tauri signer generate -- -w ~/.tauri/sgian.key
```

It prints the new public key to paste into `plugins.updater.pubkey`. Set the
password secret to an empty value if you generated the key without one.

## 5. Windows code-signing certificate

This is the one that needs a decision, because the industry changed under
the workflow's assumptions. Since mid-2023, code-signing certificates from
public authorities must keep their private key in hardware or a cloud
service, so a plain exportable PFX is no longer something a new certificate
gives you. The workflow as written imports a PFX. Three ways through:

- **A certificate you already hold as a PFX**, issued before the change or
  by an authority that still offers an exportable key. Use it as is.
- **Azure Trusted Signing**, Microsoft's cloud signing service. No PFX
  exists; `signtool` signs through a plug-in with an Azure identity. This is
  the cheapest and least fragile path for a new project, but it needs an
  Azure subscription, an identity validation that takes a few days, and a
  change to the Windows job of the workflow, which is a small piece of work
  I can do once you have the account.
- **A certificate on a hardware token** cannot be used by a GitHub-hosted
  runner at all; it needs a self-hosted Windows runner with the token
  attached.

Whichever you choose, the publisher name in the certificate's subject
becomes the MSIX `Publisher`: the build script stamps it from the
certificate, and Windows refuses an MSIX whose manifest and certificate
disagree. The manifest currently says `CN=Sgian Development`, a placeholder
that the script overwrites.

If you have a PFX:

```powershell
[Convert]::ToBase64String([IO.File]::ReadAllBytes('sgian.pfx')) | Set-Content cert.pfx.b64
gh secret set WINDOWS_CERTIFICATE --env native-release < cert.pfx.b64
gh secret set WINDOWS_CERTIFICATE_PASSWORD --env native-release
Remove-Item cert.pfx.b64
```

## 6. Check, then rehearse, then run

```bash
gh secret list --env native-release
gh variable list --env native-release
```

Eleven secrets and one variable. Then, before dispatching the workflow, run
the macOS packaging rehearsal from `native-release.md` once more with your
real Sparkle public key in the build and a throwaway private key in the
rehearsal. It costs nothing and proves the embedded public key is the one
you expect.

Then follow "Create a candidate" in `native-release.md`: dispatch the
workflow from `main`, let it build and sign on all three platforms, and
install the resulting artifacts on clean machines before anything is
published.

## Rotation and loss

Rotate one identity at a time and prove a signed update across the change
before retiring the old key; the policy is in `native-release.md`. A lost
Sparkle or Tauri key strands every installed app on the old key, which is
why both are backed up at the moment of creation. A lost Apple or Windows
certificate is only an inconvenience: revoke, reissue, and re-sign.
