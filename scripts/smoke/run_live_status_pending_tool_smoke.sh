#!/usr/bin/env bash
set -euo pipefail

echo "[smoke] deprecated: pending-tool flow was removed; this script is kept for reference only." >&2
echo "[smoke] use resume-based flows (e.g. --replace-last-toolresult + --no-prompt) instead." >&2
exit 1

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CODEX_BIN="$ROOT/codex-rs/target/debug/codex"

echo "[smoke] building Codex CLI (codex-cli)..." >&2
pushd "$ROOT/codex-rs" >/dev/null
cargo build -p codex-cli >/dev/null
popd >/dev/null

if [[ ! -f "$HOME/.codex/auth.json" ]]; then
  echo "[smoke] missing $HOME/.codex/auth.json; cannot run real-model pending-tool smoke." >&2
  exit 1
fi

TMP="$(mktemp -d)"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

CODEX_HOME="$TMP/codex_home"
WORKDIR="$TMP/work"
mkdir -p "$CODEX_HOME" "$WORKDIR"
cp "$HOME/.codex/auth.json" "$CODEX_HOME/auth.json"
if [[ -f "$HOME/.codex/config.toml" ]]; then
  cp "$HOME/.codex/config.toml" "$CODEX_HOME/config.toml"
fi

TOOL_SCRIPT="$WORKDIR/pending_tool_smoke.py"
cat >"$TOOL_SCRIPT" <<'PY'
import json
import os

payload = json.loads(os.environ.get("CODEX_TOOL_ARGS_JSON", "{}"))
ticket = payload.get("ticket", "none")
print(json.dumps({"status": "pending", "ticket": ticket}))
PY

export CODEX_HOME
export CODEX_UNSAFE_ALLOW_NO_SANDBOX=1
export CODEX_LIVE_DEVICE_ID="smoke-pending"

OUT_JSONL="$TMP/codex_exec_pending.jsonl"

echo "[smoke] starting real codex exec (pending-tool flow)..." >&2
"$CODEX_BIN" exec \
  --json \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  -c "custom_tools.pending_tool_smoke.command=[\"python3\",\"$TOOL_SCRIPT\"]" \
  -c "custom_tools.pending_tool_smoke.description=\"Queue async job (live-status smoke)\"" \
  -c 'custom_tools.pending_tool_smoke.parameters={type="object",additionalProperties=false,required=["ticket"],properties={ticket={type="string"}}}' \
  -c "custom_tools.pending_tool_smoke.timeout_ms=2000" \
  -c "custom_tools.pending_tool_smoke.hibernate_after_call=true" \
  'Call the tool pending_tool_smoke with {"ticket":"sync-42"} and then wait. Once the pending result is delivered, respond with the single word done.' \
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

wait_for_pending() {
  python3 - "$LIVE_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    record = json.load(f)

if record.get("status") != "waiting_pending_tool":
    print("")
    sys.exit(0)

detail = record.get("detail") or {}
call_id = detail.get("call_id") or ""
tool_name = detail.get("tool_name") or ""

if tool_name != "pending_tool_smoke":
    print("")
    sys.exit(0)

ipc = record.get("ipc") or {}
host = ipc.get("host") or record.get("host")
port = ipc.get("port") or record.get("port")
if not host or not port:
    print("")
    sys.exit(0)

print(call_id)
PY
}

CALL_ID=""
for _ in $(seq 1 600); do
  if ! kill -0 "$PID" >/dev/null 2>&1; then
    echo "[smoke] codex exec exited before entering waiting_pending_tool; output tail:" >&2
    tail -n 200 "$OUT_JSONL" >&2 || true
    exit 1
  fi
  CALL_ID="$(wait_for_pending || true)"
  if [[ -n "${CALL_ID:-}" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -z "${CALL_ID:-}" ]]; then
  echo "[smoke] did not observe waiting_pending_tool with call_id in live status file" >&2
  tail -n 200 "$OUT_JSONL" >&2 || true
  kill -9 "$PID" >/dev/null 2>&1 || true
  exit 1
fi

THREAD_ID="$(python3 - "$LIVE_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    record = json.load(f)

print(record.get("thread_id", ""))
PY
)"

if [[ -z "${THREAD_ID:-}" ]]; then
  echo "[smoke] could not read thread_id from live status record" >&2
  kill -9 "$PID" >/dev/null 2>&1 || true
  exit 1
fi

echo "[smoke] pending tool call_id=$CALL_ID thread_id=$THREAD_ID" >&2

"$CODEX_BIN" exec deliver-pending \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  --call-id "$CALL_ID" \
  --output "resolved ticket=sync-42" \
  "$THREAD_ID" \
  >/dev/null

for _ in $(seq 1 120); do
  status="$(python3 - "$LIVE_FILE" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    record = json.load(f)

print(record.get("status", ""))
PY
)"
  if [[ "$status" != "waiting_pending_tool" ]]; then
    break
  fi
  sleep 0.25
done

for _ in $(seq 1 300); do
  if ! kill -0 "$PID" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if kill -0 "$PID" >/dev/null 2>&1; then
  echo "[smoke] codex exec did not exit after deliver-pending; output tail:" >&2
  tail -n 200 "$OUT_JSONL" >&2 || true
  echo "[smoke] live record:" >&2
  cat "$LIVE_FILE" >&2 || true
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
