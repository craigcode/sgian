#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
app="$repo/apps/macos/build/Sgian.app"
version="$(node -p "require('$repo/package.json').version")"
: "${SGIAN_CODESIGN_IDENTITY:?Developer ID identity required}"
: "${SGIAN_NOTARY_KEY_PATH:?App Store Connect key file required}"
: "${APPLE_API_KEY:?App Store Connect key ID required}"
: "${APPLE_API_ISSUER:?App Store Connect issuer required}"
: "${SGIAN_SPARKLE_PRIVATE_KEY_FILE:?Sparkle signing key file required}"
[[ "$SGIAN_CODESIGN_IDENTITY" != '-' ]] || { echo 'Refusing ad-hoc release' >&2; exit 1; }
[[ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$app/Contents/Info.plist")" == "$version" ]] || exit 1
/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$app/Contents/Info.plist" >/dev/null
lipo -verify_arch arm64 x86_64 "$app/Contents/MacOS/Sgian"
lipo -verify_arch arm64 x86_64 "$app/Contents/Helpers/sgian"
codesign --verify --deep --strict "$app"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
out="$repo/apps/macos/build/release"
rm -rf "$out"
mkdir -p "$out" "$work/dmg"
notarize() {
  xcrun notarytool submit "$1" --key "$SGIAN_NOTARY_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER" --wait --output-format json > "$work/notary.json"
  python3 - "$work/notary.json" <<'PY'
import json, sys
result=json.load(open(sys.argv[1]))
if result.get('status') != 'Accepted': raise SystemExit('Notarization was not accepted; inspect the submission in App Store Connect')
PY
}
ditto -c -k --keepParent "$app" "$work/Sgian.zip"
notarize "$work/Sgian.zip"
xcrun stapler staple "$app"
xcrun stapler validate "$app"
spctl --assess --type execute --verbose "$app"
ditto "$app" "$work/dmg/Sgian.app"
ln -s /Applications "$work/dmg/Applications"
dmg="$out/Sgian_${version}_macos_universal.dmg"
hdiutil create -volname Sgian -srcfolder "$work/dmg" -ov -format UDZO "$dmg"
codesign --force --timestamp --sign "$SGIAN_CODESIGN_IDENTITY" "$dmg"
notarize "$dmg"
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"
sparkle="$repo/apps/macos/.build/artifacts/sparkle/Sparkle/bin"
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
