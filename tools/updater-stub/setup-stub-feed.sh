#!/usr/bin/env bash
#
# setup-stub-feed.sh — Build and serve the localhost updater stub feed.
#
# Produces a latest.json (version + per-platform darwin-aarch64 {url, signature})
# in a fresh private temporary directory (mktemp -d; the path is printed), copies
# the updater artifacts (Sgian.app.tar.gz + .sig) from the release build output,
# and starts an HTTP server on port 8787.
#
# The committed default tauri.conf.json ships a production-safe updater endpoint
# (https://, no dangerousInsecureTransportProtocol). The localhost stub feed
# requires http://localhost with dangerousInsecureTransportProtocol enabled, which
# lives in a dev/stub config overlay: src-tauri/tauri.stub.conf.json. Pass --build
# (or run the overlay build manually) so the built app's updater points at the
# localhost stub feed. file:// endpoints are NOT supported by tauri-plugin-updater.
#
# Usage:
#   ./tools/updater-stub/setup-stub-feed.sh [version]    # set up + serve (foreground)
#   ./tools/updater-stub/setup-stub-feed.sh [version] --no-serve  # set up only
#   ./tools/updater-stub/setup-stub-feed.sh [version] --build     # build with stub overlay, set up + serve
#   ./tools/updater-stub/setup-stub-feed.sh [version] --build --no-serve  # build, set up only
#
# Arguments:
#   version  Feed SemVer to advertise (default: matches tauri.conf.json "version").
#             Set higher than the installed build (e.g. 0.2.0) to test "update available".
#             Set equal (e.g. 0.1.0) to test "up to date".
#
# Environment:
#   SGIAN_FEED_DIR  Override the feed directory (default: a fresh mktemp -d dir;
#                   --feed-dir PATH takes precedence over this).
#   SGIAN_FEED_PORT Override the HTTP port (default: 8787).
#   BUNDLE_DIR      Override the build bundle directory
#                   (default: src-tauri/target/release/bundle/macos).
#   TAURI_SIGNING_PRIVATE_KEY      Required for --build (updater artifact signing).
#   TAURI_SIGNING_PRIVATE_KEY_PASSWORD  Optional, for a password-protected key.
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

FEED_PORT="${SGIAN_FEED_PORT:-8787}"
BUNDLE_DIR="${BUNDLE_DIR:-$REPO_ROOT/src-tauri/target/release/bundle/macos}"

# Validate the port before it is interpolated into the JSON manifest, the lsof
# probe, and the http.server command line: digits only, in the range 1-65535.
if [[ ! "$FEED_PORT" =~ ^[0-9]+$ ]] || [ "$FEED_PORT" -lt 1 ] || [ "$FEED_PORT" -gt 65535 ]; then
  echo "ERROR: invalid feed port '$FEED_PORT' (expected an integer in 1-65535)" >&2
  exit 2
fi

# Print the usage/documentation block (kept in sync with the file header above).
# A heredoc is used instead of sed-extracting the header comments so the help
# output never leaks script-body lines (e.g. `set -euo pipefail`).
print_usage() {
  cat <<'USAGE'
setup-stub-feed.sh — Build and serve the localhost updater stub feed.

Produces a latest.json (version + per-platform darwin-aarch64 {url, signature})
in a fresh private temporary directory (mktemp -d; the path is printed), copies
the updater artifacts (Sgian.app.tar.gz + .sig) from the release build output,
and starts an HTTP server on port 8787.

The committed default tauri.conf.json ships a production-safe updater endpoint
(https://, no dangerousInsecureTransportProtocol). The localhost stub feed
requires http://localhost with dangerousInsecureTransportProtocol enabled, which
lives in a dev/stub config overlay: src-tauri/tauri.stub.conf.json. Pass --build
(or run the overlay build manually) so the built app's updater points at the
localhost stub feed. file:// endpoints are NOT supported by tauri-plugin-updater.

Usage:
  ./tools/updater-stub/setup-stub-feed.sh [version]    # set up + serve (foreground)
  ./tools/updater-stub/setup-stub-feed.sh [version] --no-serve  # set up only
  ./tools/updater-stub/setup-stub-feed.sh [version] --build     # build with stub overlay, set up + serve
  ./tools/updater-stub/setup-stub-feed.sh [version] --build --no-serve  # build, set up only

Arguments:
  version  Feed SemVer to advertise (default: matches tauri.conf.json "version").
            Set higher than the installed build (e.g. 0.2.0) to test "update available".
            Set equal (e.g. 0.1.0) to test "up to date".

Options:
  --no-serve       Set up the feed without starting the HTTP server.
  --build          Build with the dev/stub config overlay before setting up the feed.
  --feed-dir PATH  Use PATH as the feed directory instead of a fresh mktemp -d dir.
  -h, --help       Print this help and exit.

Environment:
  SGIAN_FEED_DIR  Override the feed directory (default: a fresh mktemp -d dir;
                  --feed-dir PATH takes precedence over this).
  SGIAN_FEED_PORT Override the HTTP port (default: 8787).
  BUNDLE_DIR      Override the build bundle directory
                  (default: src-tauri/target/release/bundle/macos).
  TAURI_SIGNING_PRIVATE_KEY      Required for --build (updater artifact signing).
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD  Optional, for a password-protected key.
USAGE
}

# Parse args: first non-flag arg is version, --no-serve flag suppresses server,
# --build triggers an overlay build, --feed-dir overrides the feed directory,
# and unknown --flags are rejected.
FEED_VERSION=""
NO_SERVE=false
BUILD=false
FEED_DIR_OVERRIDE="${SGIAN_FEED_DIR:-}"
while [ $# -gt 0 ]; do
  arg="$1"
  case "$arg" in
    --no-serve) NO_SERVE=true ;;
    --build) BUILD=true ;;
    --feed-dir)
      if [ $# -lt 2 ]; then
        echo "ERROR: --feed-dir requires a path argument" >&2
        exit 2
      fi
      FEED_DIR_OVERRIDE="$2"
      shift
      ;;
    --feed-dir=*) FEED_DIR_OVERRIDE="${arg#--feed-dir=}" ;;
    -h|--help)
      print_usage
      exit 0
      ;;
    --*)
      echo "ERROR: unknown option: $arg" >&2
      echo "       Run '$0 --help' for usage." >&2
      exit 2
      ;;
    -*)
      echo "ERROR: unknown option: $arg" >&2
      echo "       Run '$0 --help' for usage." >&2
      exit 2
      ;;
    *)
      if [ -n "$FEED_VERSION" ]; then
        echo "ERROR: multiple version arguments: '$FEED_VERSION' and '$arg'" >&2
        echo "       Only one positional version may be given." >&2
        exit 2
      fi
      FEED_VERSION="$arg"
      ;;
  esac
  shift
done

# Feed directory: an explicit override (--feed-dir / SGIAN_FEED_DIR) is used
# as-is; otherwise create a fresh private directory via mktemp -d. A fixed
# world-writable path like /tmp/sgian-feed is deliberately NOT used — a
# pre-existing attacker-owned dir or symlink there would let another local
# user redirect the artifact copies (symlink clobber).
if [ -n "$FEED_DIR_OVERRIDE" ]; then
  FEED_DIR="$FEED_DIR_OVERRIDE"
  mkdir -p "$FEED_DIR"
else
  FEED_DIR="$(mktemp -d "${TMPDIR:-/tmp}/sgian-feed.XXXXXX")"
fi

# Default version: read from tauri.conf.json. The repo path is passed as an
# argument (not interpolated into the JS source) so a checkout path containing
# a single quote cannot break the string literal or inject code.
if [ -z "$FEED_VERSION" ]; then
  FEED_VERSION=$(node -e '
    const c = require(process.argv[1] + "/src-tauri/tauri.conf.json");
    console.log(c.version);
  ' "$REPO_ROOT")
fi

# Validate the version before it is interpolated into the JSON manifest:
# accept only SemVer-shaped strings (X.Y.Z with an optional -prerelease/+build
# suffix). Anything else (quotes, braces, shell metacharacters) is rejected.
if [[ ! "$FEED_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]]; then
  echo "ERROR: invalid feed version '$FEED_VERSION' (expected SemVer, e.g. 0.2.0)" >&2
  exit 2
fi

TARBALL="$BUNDLE_DIR/Sgian.app.tar.gz"
SIG_FILE="$BUNDLE_DIR/Sgian.app.tar.gz.sig"

# --build: build with the dev/stub config overlay so the produced app's updater
# points at http://localhost:8787 with dangerousInsecureTransportProtocol enabled.
# The committed default tauri.conf.json ships an https production endpoint; the
# localhost stub overlay (src-tauri/tauri.stub.conf.json) is merged via --config.
if [ "$BUILD" = true ]; then
  if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
    echo "ERROR: --build requires TAURI_SIGNING_PRIVATE_KEY to be set." >&2
    echo "       Generate a keypair with 'npx tauri signer generate' and export" >&2
    echo "       TAURI_SIGNING_PRIVATE_KEY before re-running." >&2
    exit 1
  fi
  # The local updater key was generated with --ci (no/empty password). Omitting
  # the password env var makes updater signing fail with "incorrect updater
  # private key password". Export an empty password so the .sig is produced.
  # (tauri build IGNORES .env files — secrets must be real exported shell vars.)
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD-}"

  echo "==> Building with the dev/stub updater config overlay"
  echo "    overlay: src-tauri/tauri.stub.conf.json (merged via tauri --config)"
  echo "    endpoint -> http://localhost:$FEED_PORT/latest.json"
  # NOTE: --config is a Tauri-CLI option (it merges the overlay conf). Do NOT
  # use a `--` separator before it — that forwards --config (and --bundles) to
  # cargo, producing `unexpected argument '--config' found` and the overlay
  # build never runs. --bundles app skips the .dmg (faster; the updater only
  # needs the .app.tar.gz + .sig).
  (cd "$REPO_ROOT" && npx tauri build --config src-tauri/tauri.stub.conf.json --bundles app)

  # Mark the artifact as a stub-overlay build. The stub overlay points the
  # updater at http://localhost with dangerousInsecureTransportProtocol
  # enabled, so this artifact must never be shipped. The marker lives next to
  # the artifact because stub and production builds share the same output path.
  {
    echo "This build was produced with the localhost updater stub overlay"
    echo "(src-tauri/tauri.stub.conf.json): updater endpoint http://localhost:$FEED_PORT"
    echo "with dangerousInsecureTransportProtocol enabled."
    echo "DO NOT RELEASE this artifact. Rebuild from a clean checkout without"
    echo "the overlay for a releasable build."
    echo "Built: $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  } > "$BUNDLE_DIR/STUB-BUILD"
  echo "" >&2
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
  echo "!!  STUB BUILD — DO NOT RELEASE                                   !!" >&2
  echo "!!  This artifact's updater points at http://localhost with       !!" >&2
  echo "!!  insecure transport enabled. Marker written to:                !!" >&2
  echo "!!    $BUNDLE_DIR/STUB-BUILD" >&2
  echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!" >&2
  echo "" >&2
fi

if [ ! -f "$TARBALL" ]; then
  echo "ERROR: updater tarball not found at $TARBALL" >&2
  echo "       Run with --build, or run 'npx tauri build --config src-tauri/tauri.stub.conf.json --bundles app'" >&2
  echo "       with TAURI_SIGNING_PRIVATE_KEY set first." >&2
  exit 1
fi
if [ ! -f "$SIG_FILE" ]; then
  echo "ERROR: updater signature not found at $SIG_FILE" >&2
  echo "       Run with --build, or run 'npx tauri build --config src-tauri/tauri.stub.conf.json --bundles app'" >&2
  echo "       with TAURI_SIGNING_PRIVATE_KEY set first." >&2
  exit 1
fi
if [ ! -s "$SIG_FILE" ]; then
  echo "ERROR: updater signature is empty at $SIG_FILE" >&2
  exit 1
fi

echo "==> Setting up updater stub feed in $FEED_DIR"

# Copy the updater artifacts so the url is reachable from the same server.
cp -f "$TARBALL" "$FEED_DIR/Sgian.app.tar.gz"
cp -f "$SIG_FILE" "$FEED_DIR/Sgian.app.tar.gz.sig"

# Read the signature text content (base64-encoded minisign signature).
SIGNATURE=$(cat "$SIG_FILE")

# Validate the signature before it is interpolated into the JSON manifest:
# a Tauri updater .sig is a single line of base64, so reject anything with
# characters outside the base64 alphabet (this catches embedded quotes,
# backslashes, newlines, and control characters).
if [[ -z "$SIGNATURE" || "$SIGNATURE" =~ [^A-Za-z0-9+/=] ]]; then
  echo "ERROR: $SIG_FILE is not single-line base64; refusing to embed it in latest.json" >&2
  exit 1
fi

# Generate latest.json with the signature embedded.
# The signature field = text content of the .sig file (not a path).
# The url points to the tarball served from the same HTTP server.
PUB_DATE=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

cat > "$FEED_DIR/latest.json" <<JSON
{
  "version": "$FEED_VERSION",
  "notes": "Sgian updater stub feed (localhost dev/test only)",
  "pub_date": "$PUB_DATE",
  "platforms": {
    "darwin-aarch64": {
      "signature": "$SIGNATURE",
      "url": "http://localhost:$FEED_PORT/Sgian.app.tar.gz"
    }
  }
}
JSON

echo "==> Feed manifest written to $FEED_DIR/latest.json"
echo "    version:    $FEED_VERSION"
echo "    signature:  $(echo "$SIGNATURE" | head -c 40)...(truncated)"
echo "    tarball:    $FEED_DIR/Sgian.app.tar.gz"

if [ "$NO_SERVE" = true ]; then
  echo "==> --no-serve specified; skipping HTTP server."
  exit 0
fi

# Fail if the port is already in use: the listener could be anything (an old
# feed serving a stale directory, or an unrelated process), so do NOT assume
# the feed is being served.
if lsof -ti :"$FEED_PORT" >/dev/null 2>&1; then
  echo "ERROR: port $FEED_PORT is already in use by another process; the feed in" >&2
  echo "       $FEED_DIR is NOT being served." >&2
  echo "       Stop the listener (lsof -ti :$FEED_PORT | xargs kill) or choose a" >&2
  echo "       different port via SGIAN_FEED_PORT, then re-run this script." >&2
  exit 1
fi

echo "==> Starting HTTP server on http://localhost:$FEED_PORT (directory: $FEED_DIR)"
echo "    Press Ctrl+C to stop."
echo ""
echo "    Verify: curl -fsS http://localhost:$FEED_PORT/latest.json"
echo ""

# Bind to 127.0.0.1 explicitly — python3 -m http.server binds IPv6-only (::)
# by default on macOS, which causes curl (IPv4-first) to fail.
exec python3 -m http.server "$FEED_PORT" --directory "$FEED_DIR" --bind 127.0.0.1
