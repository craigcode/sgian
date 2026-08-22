#!/usr/bin/env bash
# ENHANCEMENTS §5: multi-round Unix transport soak for packaged/release binaries.
# Usage: transport-soak.sh <sgian-exe> <workspace-dir>
set -euo pipefail

exe="${1:?sgian executable required}"
ws="${2:?workspace dir required}"
rounds="${SGIAN_SOAK_ROUNDS:-3}"
burst="${SGIAN_SOAK_BURST:-32}"

mkdir -p "$ws"

sample_threads() {
  # Best-effort: count threads for a live sgian process.
  # Shared runners are noisy; callers treat growth as a soft warning.
  local pid
  pid="$(pgrep -f '[s]gian' | head -n1 || true)"
  if [ -z "${pid:-}" ]; then
    echo 0
    return
  fi
  ps -o nlwp= -p "$pid" 2>/dev/null | tr -d ' ' || echo 0
}

"$exe" ctl --workspace "$ws" new --name soak-main
baseline_threads="$(sample_threads)"
echo "TRANSPORT SOAK baseline threads=${baseline_threads:-0}"

for round in $(seq 1 "$rounds"); do
  echo "TRANSPORT SOAK round $round/$rounds"
  pids=()
  for _ in $(seq 1 "$burst"); do
    "$exe" ctl --workspace "$ws" panes --json >/dev/null &
    pids+=("$!")
  done
  for pid in "${pids[@]}"; do
    wait "$pid"
  done

  "$exe" ctl --workspace "$ws" new --name "soak-r${round}"
  "$exe" ctl --workspace "$ws" restart "soak-r${round}"
  "$exe" ctl --workspace "$ws" send "soak-r${round}" 'echo soak-ok\n'

  log="$(find "${XDG_DATA_HOME:-$HOME/.local/share}/Sgian" -name daemon.log -type f 2>/dev/null | head -n1 || true)"
  if [ -n "${log:-}" ] && [ -f "$log" ]; then
    printf 'soak-round-%s\n' "$round" >>"$log" || true
  fi

  "$exe" ctl --workspace "$ws" panes --json >/dev/null
done

after_threads="$(sample_threads)"
echo "TRANSPORT SOAK after threads=${after_threads:-0}"
if [ -n "${baseline_threads:-}" ] && [ -n "${after_threads:-}" ]; then
  if [ "$after_threads" -gt $((baseline_threads + 64)) ]; then
    echo "TRANSPORT SOAK WARN: thread count grew from $baseline_threads to $after_threads" >&2
  fi
fi

"$exe" ctl --workspace "$ws" panes --json >/dev/null
echo "TRANSPORT SOAK OK"
