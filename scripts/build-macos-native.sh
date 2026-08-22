#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
package_root="$repo_root/apps/macos"
output_root="$package_root/build"
app="$output_root/Sgian.app"
module_cache="${TMPDIR:-/tmp}/sgian-swift-module-cache"
cargo_target_dir="$repo_root/src-tauri/target"
signing_identity="${SGIAN_CODESIGN_IDENTITY:--}"

export CLANG_MODULE_CACHE_PATH="$module_cache"
export SWIFTPM_MODULECACHE_OVERRIDE="$module_cache"

cargo build \
  --release \
  --manifest-path "$repo_root/src-tauri/Cargo.toml" \
  --target-dir "$cargo_target_dir"
swift build --disable-sandbox --package-path "$package_root" -c release
swift_bin_dir="$(swift build --disable-sandbox --package-path "$package_root" -c release --show-bin-path)"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Helpers" "$app/Contents/Resources"
cp "$package_root/Info.plist" "$app/Contents/Info.plist"
cp "$swift_bin_dir/SgianMac" "$app/Contents/MacOS/Sgian"
cp "$cargo_target_dir/release/sgian" "$app/Contents/Helpers/sgian"
cp "$repo_root/src-tauri/icons/icon.icns" "$app/Contents/Resources/Sgian.icns"
cp "$repo_root/LICENSE" "$app/Contents/Resources/Sgian-LICENSE.txt"
cp "$package_root/.build/checkouts/SwiftTerm/LICENSE" "$app/Contents/Resources/SwiftTerm-LICENSE.txt"

# SwiftTerm explicitly probes the standard app Resources location for its
# SwiftPM shader bundle (and falls back cleanly when Metal is disabled).
for resource_bundle in "$swift_bin_dir"/*.bundle; do
  if [[ -d "$resource_bundle" ]]; then
    cp -R "$resource_bundle" "$app/Contents/Resources/"
  fi
done

chmod 755 "$app/Contents/MacOS/Sgian" "$app/Contents/Helpers/sgian"

# Sign nested code first, then seal the outer bundle. Local builds use an
# ad-hoc identity; release automation can supply a Developer ID identity.
sign_args=(--force --sign "$signing_identity")
if [[ "$signing_identity" != "-" ]]; then
  sign_args+=(--options runtime --timestamp)
fi
codesign "${sign_args[@]}" "$app/Contents/Helpers/sgian"
codesign "${sign_args[@]}" "$app"

test -x "$app/Contents/MacOS/Sgian"
test -x "$app/Contents/Helpers/sgian"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --verify --deep --strict --verbose=2 "$app"

echo "$app"
