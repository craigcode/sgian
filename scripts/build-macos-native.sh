#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
package_root="$repo_root/apps/macos"
output_root="$package_root/build"
app="$output_root/Sgian.app"
configuration="${SGIAN_BUILD_CONFIGURATION:-release}"
architecture="${SGIAN_MAC_ARCH:-$(uname -m)}"
signing_identity="${SGIAN_CODESIGN_IDENTITY:--}"
module_cache="${TMPDIR:-/tmp}/sgian-swift-module-cache"
export CLANG_MODULE_CACHE_PATH="$module_cache"
export SWIFTPM_MODULECACHE_OVERRIDE="$module_cache"

case "$configuration" in debug|release) ;; *) echo 'Expected debug or release configuration' >&2; exit 1;; esac
case "$architecture" in arm64|x86_64|universal) ;; *) echo 'Expected arm64, x86_64, or universal architecture' >&2; exit 1;; esac
if [[ "${SGIAN_RELEASE:-0}" == 1 ]]; then
  [[ "$signing_identity" != '-' && "$configuration" == release ]] || { echo 'Release requires Developer ID signing and a release build' >&2; exit 1; }
  [[ -n "${SGIAN_SPARKLE_PUBLIC_KEY:-}" ]] || { echo 'Release requires SGIAN_SPARKLE_PUBLIC_KEY' >&2; exit 1; }
fi
version="$(node -p "require('$repo_root/package.json').version")"
mkdir -p "$output_root"
staging="$(mktemp -d "$output_root/.native-build.XXXXXX")"
trap 'rm -rf "$staging"' EXIT
architectures=("$architecture")
if [[ "$architecture" == universal ]]; then architectures=(arm64 x86_64); fi

for arch in "${architectures[@]}"; do
  target="aarch64-apple-darwin"
  if [[ "$arch" == x86_64 ]]; then target=x86_64-apple-darwin; fi
  rustup target add "$target"
  cargo_args=(build --locked --manifest-path "$repo_root/src-tauri/Cargo.toml" --target "$target")
  if [[ "$configuration" == release ]]; then cargo_args+=(--release); fi
  cargo "${cargo_args[@]}"
  swift build --disable-sandbox --package-path "$package_root" -c "$configuration" --arch "$arch"
  swift_bin_dir="$(swift build --disable-sandbox --package-path "$package_root" -c "$configuration" --arch "$arch" --show-bin-path)"
  cp "$swift_bin_dir/SgianMac" "$staging/Sgian-$arch"
  cp "$repo_root/src-tauri/target/$target/$configuration/sgian" "$staging/sgian-$arch"
done

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers" "$app/Contents/Resources" "$app/Contents/Frameworks"
cp "$package_root/Info.plist" "$app/Contents/Info.plist"
if [[ "$architecture" == universal ]]; then
  lipo -create "$staging/Sgian-arm64" "$staging/Sgian-x86_64" -output "$app/Contents/MacOS/Sgian"
  lipo -create "$staging/sgian-arm64" "$staging/sgian-x86_64" -output "$app/Contents/Helpers/sgian"
else
  cp "$staging/Sgian-$architecture" "$app/Contents/MacOS/Sgian"
  cp "$staging/sgian-$architecture" "$app/Contents/Helpers/sgian"
fi
cp "$repo_root/src-tauri/icons/icon.icns" "$app/Contents/Resources/Sgian.icns"
cp "$repo_root/LICENSE" "$app/Contents/Resources/Sgian-LICENSE.txt"
cp "$package_root/.build/checkouts/SwiftTerm/LICENSE" "$app/Contents/Resources/SwiftTerm-LICENSE.txt"
sparkle="$package_root/.build/artifacts/sparkle/Sparkle"
cp "$sparkle/LICENSE" "$app/Contents/Resources/Sparkle-LICENSE.txt"
ditto "$sparkle/Sparkle.xcframework/macos-arm64_x86_64/Sparkle.framework" "$app/Contents/Frameworks/Sparkle.framework"
for resource_bundle in "$swift_bin_dir"/*.bundle; do
  if [[ -d "$resource_bundle" ]]; then cp -R "$resource_bundle" "$app/Contents/Resources/"; fi
done

/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app/Contents/Info.plist"
if [[ -n "${SGIAN_SPARKLE_PUBLIC_KEY:-}" ]]; then
  python3 - "$app/Contents/Info.plist" <<'PY'
import base64, os, plistlib, sys
from pathlib import Path
key = os.environ['SGIAN_SPARKLE_PUBLIC_KEY']
if len(base64.b64decode(key, validate=True)) != 32:
    raise SystemExit('Sparkle public key must encode exactly 32 bytes')
p = Path(sys.argv[1]); info = plistlib.loads(p.read_bytes())
info['SUPublicEDKey'] = key
p.write_bytes(plistlib.dumps(info))
PY
fi
chmod 755 "$app/Contents/MacOS/Sgian" "$app/Contents/Helpers/sgian"
sign_args=(--force --sign "$signing_identity")
if [[ "$signing_identity" != '-' ]]; then sign_args+=(--options runtime --timestamp); fi
framework="$app/Contents/Frameworks/Sparkle.framework/Versions/B"
for component in "$framework/XPCServices/Downloader.xpc" "$framework/XPCServices/Installer.xpc" "$framework/Autoupdate" "$framework/Updater.app"; do
  codesign "${sign_args[@]}" --preserve-metadata=entitlements "$component"
done
codesign "${sign_args[@]}" "$app/Contents/Frameworks/Sparkle.framework"
codesign "${sign_args[@]}" "$app/Contents/Helpers/sgian"
codesign "${sign_args[@]}" "$app"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --verify --deep --strict --verbose=2 "$app"
lipo -info "$app/Contents/MacOS/Sgian"
lipo -info "$app/Contents/Helpers/sgian"
echo "$app"
