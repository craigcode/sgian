#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
app="$repo/apps/macos/build/Sgian.app"
version="$(node -p "require('$repo/package.json').version")"
# SGIAN_PACKAGE_REHEARSAL=1 exercises everything here that does not need
# Apple credentials: the bundle checks, the DMG, the Sparkle appcast and its
# independent verification. Signing is ad hoc, notarization is skipped, and
# the output lands in build/rehearsal, a directory the release workflow never
# uploads from, so a rehearsal cannot become a release by accident.
rehearsal="${SGIAN_PACKAGE_REHEARSAL:-0}"
if [[ "$rehearsal" == 1 ]]; then
  echo 'REHEARSAL: ad-hoc signing, no notarization; nothing under build/rehearsal may be published' >&2
  SGIAN_CODESIGN_IDENTITY='-'
  : "${SGIAN_SPARKLE_PRIVATE_KEY_FILE:?Sparkle signing key file required}"
else
  : "${SGIAN_CODESIGN_IDENTITY:?Developer ID identity required}"
  : "${SGIAN_NOTARY_KEY_PATH:?App Store Connect key file required}"
  : "${APPLE_API_KEY:?App Store Connect key ID required}"
  : "${APPLE_API_ISSUER:?App Store Connect issuer required}"
  : "${SGIAN_SPARKLE_PRIVATE_KEY_FILE:?Sparkle signing key file required}"
  [[ "$SGIAN_CODESIGN_IDENTITY" != '-' ]] || { echo 'Refusing ad-hoc release' >&2; exit 1; }
fi
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app/Contents/Info.plist")" == "$version" ]] || exit 1
/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$app/Contents/Info.plist" >/dev/null
lipo "$app/Contents/MacOS/Sgian" -verify_arch arm64 x86_64
lipo "$app/Contents/Helpers/sgian" -verify_arch arm64 x86_64
codesign --verify --deep --strict "$app"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
out="$repo/apps/macos/build/release"
if [[ "$rehearsal" == 1 ]]; then out="$repo/apps/macos/build/rehearsal"; fi
rm -rf "$out"
mkdir -p "$out" "$work/dmg"
notarize() {
  if [[ "$rehearsal" == 1 ]]; then echo "REHEARSAL: skipping notarization of $1" >&2; return 0; fi
  xcrun notarytool submit "$1" --key "$SGIAN_NOTARY_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER" --wait --output-format json > "$work/notary.json"
  python3 - "$work/notary.json" <<'PY'
import json, sys
result=json.load(open(sys.argv[1]))
if result.get('status') != 'Accepted': raise SystemExit('Notarization was not accepted; inspect the submission in App Store Connect')
PY
}
ditto -c -k --keepParent "$app" "$work/Sgian.zip"
notarize "$work/Sgian.zip"
if [[ "$rehearsal" != 1 ]]; then
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
  spctl --assess --type execute --verbose "$app"
fi
ditto "$app" "$work/dmg/Sgian.app"
ln -s /Applications "$work/dmg/Applications"
dmg="$out/Sgian_${version}_macos_universal.dmg"
hdiutil create -volname Sgian -srcfolder "$work/dmg" -ov -format UDZO "$dmg"
if [[ "$rehearsal" == 1 ]]; then
  codesign --force --sign - "$dmg"
else
  codesign --force --timestamp --sign "$SGIAN_CODESIGN_IDENTITY" "$dmg"
fi
notarize "$dmg"
if [[ "$rehearsal" != 1 ]]; then
  xcrun stapler staple "$dmg"
  xcrun stapler validate "$dmg"
fi
sparkle="$repo/apps/macos/.build/native-x86_64/artifacts/sparkle/Sparkle/bin"
"$sparkle/generate_appcast" --ed-key-file "$SGIAN_SPARKLE_PRIVATE_KEY_FILE" --download-url-prefix "https://github.com/craigcode/sgian/releases/download/v${version}/" "$out"
python3 - "$out" "$app/Contents/Info.plist" "$version" "$repo" <<'PY'
import hashlib,json,plistlib,sys,subprocess,xml.etree.ElementTree as ET
from pathlib import Path
out=Path(sys.argv[1]); info=plistlib.loads(Path(sys.argv[2]).read_bytes()); version=sys.argv[3]
root=ET.parse(out/'appcast.xml').getroot()
items=root.findall('./channel/item')
if len(items)!=1: raise SystemExit('Expected one signed appcast entry')
enclosure=items[0].find('enclosure')
if enclosure is None or not enclosure.get('{http://www.andymatuschak.org/xml-namespaces/sparkle}edSignature'):
    raise SystemExit('Missing Sparkle signature')
asset=out/f'Sgian_{version}_macos_universal.dmg'
subprocess.run(['swift',str(Path(sys.argv[4])/'scripts/verify-sparkle.swift'),info['SUPublicEDKey'],enclosure.get('{http://www.andymatuschak.org/xml-namespaces/sparkle}edSignature'),str(asset)],check=True)
(out/'macos-package.json').write_text(json.dumps(dict(version=version,asset=asset.name,
    public_key=info['SUPublicEDKey'],sha256=hashlib.sha256(asset.read_bytes()).hexdigest()))+'\n')
PY
