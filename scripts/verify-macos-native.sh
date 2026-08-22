#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
module_cache="${TMPDIR:-/tmp}/sgian-swift-module-cache"
cargo_target_dir="$repo_root/src-tauri/target"

export CLANG_MODULE_CACHE_PATH="$module_cache"
export SWIFTPM_MODULECACHE_OVERRIDE="$module_cache"

cargo build \
  --manifest-path "$repo_root/src-tauri/Cargo.toml" \
  --target-dir "$cargo_target_dir"
SGIAN_NATIVE_INTEGRATION=1 \
SGIAN_BACKEND_BINARY="$cargo_target_dir/debug/sgian" \
  swift test --disable-sandbox --package-path "$repo_root/apps/macos"
