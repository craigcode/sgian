#!/usr/bin/env bash
# M1 demo (docs/design/keyboard-lease-and-ledger.md §6): two holders contend
# for one pane over `ctl`; the ledger verifies; one flipped byte is named.
# Usage: demos/m1-keyboard-lease.sh [sgian-exe] [workspace-dir]
#   Defaults: src-tauri/target/debug/sgian and a fresh mktemp workspace.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exe="${1:-$root/src-tauri/target/debug/sgian}"
ws="${2:-$(mktemp -d "${TMPDIR:-/tmp}/sgian-m1.XXXXXX")}"
[ -x "$exe" ] || { echo "build first: (cd src-tauri && cargo build)"; exit 2; }
mkdir -p "$ws"
trap '"$exe" ctl --workspace "$ws" shutdown >/dev/null 2>&1 || true' EXIT

ctl() { "$exe" ctl --workspace "$ws" "$@"; }
must_fail() { if "$@" 2>/tmp/sgian-m1-err; then echo "expected refusal: $*"; exit 1; else echo "  refused: $(cat /tmp/sgian-m1-err)"; fi; }

echo "== pane"
ctl new --name demo
echo "== alice takes the keyboard"
ctl lease take demo --as alice
echo "== unattributed input and bob are refused; alice types"
must_fail ctl send demo echo unattributed
must_fail ctl send demo --as bob echo bob
ctl send demo --as alice echo hello-from-alice
echo "== bob needs --force --why"
must_fail ctl lease take demo --as bob
ctl lease take demo --as bob --force --why "alice stepped away"
echo "== release needs a note"
must_fail ctl lease release demo --as bob
ctl lease release demo --as bob -m "answered the prompt; agent can continue"
echo "== ledger"
ctl ledger demo --verify
ctl ledger demo

echo "== tamper one byte, verify names the line"
token_path="$(ctl --json ipc-endpoint | python3 -c 'import sys,json; print(json.load(sys.stdin)["token_path"])')"
pane_id="$(ctl --json panes | python3 -c 'import sys,json; print([p["pane"]["id"] for p in json.load(sys.stdin)["panes"] if p["pane"]["title"]=="demo"][0])')"
ledger="$(dirname "$token_path")/ledger/$pane_id.jsonl"
python3 - "$ledger" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
t = s.replace('"why":"alice stepped away"', '"why":"alice stepped awaY"', 1)
assert t != s
open(p, "w").write(t)
PY
must_fail ctl ledger demo --verify
echo "== demo ok"
