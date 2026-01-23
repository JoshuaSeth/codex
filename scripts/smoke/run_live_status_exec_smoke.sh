#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CODEX_BIN="$ROOT/codex-rs/target/debug/codex"

echo "[smoke] building Codex CLI (codex-cli)..." >&2
pushd "$ROOT/codex-rs" >/dev/null
cargo build -p codex-cli >/dev/null
popd >/dev/null

if [[ ! -f "$HOME/.codex/auth.json" ]]; then
  echo "[smoke] missing $HOME/.codex/auth.json; cannot run real-model live-status smoke." >&2
  exit 1
fi

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

CODEX_HOME="$TMP/codex_home"
mkdir -p "$CODEX_HOME"
cp "$HOME/.codex/auth.json" "$CODEX_HOME/auth.json"
if [[ -f "$HOME/.codex/config.toml" ]]; then
  cp "$HOME/.codex/config.toml" "$CODEX_HOME/config.toml"
fi

export CODEX_HOME
export CODEX_UNSAFE_ALLOW_NO_SANDBOX=1
export CODEX_LIVE_DEVICE_ID="smoke-exec"

OUT_JSONL="$TMP/codex_exec.jsonl"

echo "[smoke] starting real codex exec..." >&2
"$CODEX_BIN" exec \
  --json \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  'Run a shell command `sleep 4` and then respond with the single word done.' \
  >"$OUT_JSONL" 2>&1 &
PID=$!

LIVE_DIR="$CODEX_HOME/live"
for _ in $(seq 1 200); do
  if ls "$LIVE_DIR"/*.json >/dev/null 2>&1; then
    break
  fi
  sleep 0.05
done

LIVE_FILE="$(ls -1 "$LIVE_DIR"/*.json 2>/dev/null | head -n 1 || true)"
if [[ -z "${LIVE_FILE:-}" || ! -f "$LIVE_FILE" ]]; then
  echo "[smoke] live status file was not created under $LIVE_DIR" >&2
  kill -9 "$PID" >/dev/null 2>&1 || true
  exit 1
fi

python3 - "$LIVE_FILE" "$PID" <<'PY'
import json
import sys

path = sys.argv[1]
pid = int(sys.argv[2])
with open(path, "r", encoding="utf-8") as f:
    record = json.load(f)

required = [
    "schema_version",
    "thread_id",
    "instance_id",
    "frontend",
    "status",
    "alive",
    "pid",
    "started_at",
    "last_heartbeat_at",
]
missing = [k for k in required if k not in record]
assert not missing, f"missing keys: {missing}"

assert record["schema_version"] == 1
assert record["frontend"] == "exec"
assert record["alive"] is True
assert record["pid"] == pid
assert record.get("device_id") == "smoke-exec"
print("[smoke] initial record ok")
PY

hb1="$(python3 - "$LIVE_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    print(json.load(f)["last_heartbeat_at"])
PY
)"
sleep 3
hb2="$(python3 - "$LIVE_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    print(json.load(f)["last_heartbeat_at"])
PY
)"
if [[ "$hb1" == "$hb2" ]]; then
  echo "[smoke] heartbeat did not update (last_heartbeat_at unchanged)" >&2
  kill -9 "$PID" >/dev/null 2>&1 || true
  exit 1
fi

set +e
wait "$PID"
rc=$?
set -e

if [[ $rc -ne 0 ]]; then
  echo "[smoke] codex exec exited non-zero ($rc); output tail:" >&2
  tail -n 200 "$OUT_JSONL" >&2 || true
  exit 1
fi

python3 - "$LIVE_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    record = json.load(f)

assert record["status"] == "completed", f"expected completed, got {record.get('status')!r}"
assert record["alive"] is False, f"expected alive=false, got {record.get('alive')!r}"
assert record.get("ended_at"), "expected ended_at set"
print("[smoke] final record ok")
PY

echo "[smoke] ok" >&2
