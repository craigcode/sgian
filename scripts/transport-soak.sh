#!/usr/bin/env bash
# ENHANCEMENTS §5: multi-round Unix transport soak for packaged/release binaries.
# Usage: transport-soak.sh <sgian-exe> <workspace-dir>
set -euo pipefail

exe="${1:?sgian executable required}"
ws="${2:?workspace dir required}"
rounds="${SGIAN_SOAK_ROUNDS:-3}"
burst="${SGIAN_SOAK_BURST:-32}"

mkdir -p "$ws"
trap '"$exe" ctl --workspace "$ws" shutdown >/dev/null 2>&1 || true' EXIT

"$exe" ctl --workspace "$ws" new --name soak-main

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

  "$exe" ctl --workspace "$ws" panes --json >/dev/null
done

"$exe" ctl --workspace "$ws" panes --json >/dev/null
echo "TRANSPORT SOAK OK"
