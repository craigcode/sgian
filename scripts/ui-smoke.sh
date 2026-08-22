#!/usr/bin/env bash
# ENHANCEMENTS §5: launch a packaged Sgian GUI under SGIAN_UI_SMOKE and wait
# for the success marker. Usage:
#   ui-smoke.sh <executable-or-.app> <workspace-dir> [marker-path]
set -euo pipefail

app="${1:?app path required}"
ws="${2:?workspace dir required}"
marker="${3:-$ws/.sgian-ui-smoke-ok}"
timeout_secs="${SGIAN_UI_SMOKE_TIMEOUT_SECS:-90}"

mkdir -p "$ws"
err_path="${marker}.err"
rm -f "$marker" "$err_path" || true

export SGIAN_UI_SMOKE=1
export SGIAN_WORKSPACE="$ws"
export SGIAN_UI_SMOKE_MARKER="$marker"

bin="$app"
if [[ "$app" == *.app ]]; then
  # Launch the Mach-O directly so SGIAN_* env vars are inherited (open(1) does not).
  bin="$(ls -d "$app"/Contents/MacOS/* | head -n1)"
fi

"$bin" &
pid=$!

deadline=$((SECONDS + timeout_secs))
while [ "$SECONDS" -lt "$deadline" ]; do
  if [ -f "$marker" ]; then
    echo "UI SMOKE OK ($marker)"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    exit 0
  fi
  if [ -f "$err_path" ]; then
    echo "UI SMOKE FAILED: $(cat "$err_path")" >&2
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    exit 1
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    # complete_ui_smoke exits 0 after writing the marker — race the filesystem.
    if [ -f "$marker" ]; then
      echo "UI SMOKE OK ($marker)"
      exit 0
    fi
    echo "UI SMOKE FAILED: app exited before writing marker" >&2
    if [ -f "$err_path" ]; then
      cat "$err_path" >&2 || true
    fi
    exit 1
  fi
  sleep 1
done

echo "UI SMOKE TIMED OUT after ${timeout_secs}s" >&2
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
exit 1
