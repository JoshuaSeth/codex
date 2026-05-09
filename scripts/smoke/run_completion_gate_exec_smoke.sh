#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CODEX_BIN="$ROOT/codex-rs/target/debug/codex"

echo "[smoke] building Codex CLI (codex-cli)..." >&2
pushd "$ROOT/codex-rs" >/dev/null
cargo build -p codex-cli >/dev/null
popd >/dev/null

if [[ ! -f "$HOME/.codex/auth.json" ]]; then
  echo "[smoke] missing $HOME/.codex/auth.json; cannot run real-model completion-gate smoke." >&2
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
export CODEX_LIVE_DEVICE_ID="smoke-completion-gate"

CRITERIA="The assistant may stop only when the candidate final response is exactly ALPHA_BETA_DONE."

run_case() {
  local name="$1"
  local prompt="$2"
  local expect_blocked="$3"
  local last_message_file="$TMP/${name}.last.txt"
  local log_file="$TMP/${name}.log"

  echo "[smoke] running case: $name" >&2
  "$CODEX_BIN" exec \
    --skip-git-repo-check \
    --dangerously-bypass-approvals-and-sandbox \
    --completion-criteria "$CRITERIA" \
    --output-last-message "$last_message_file" \
    "$prompt" \
    >"$log_file" 2>&1

  python3 - "$log_file" "$last_message_file" "$expect_blocked" <<'PY'
import pathlib
import sys

log_path = pathlib.Path(sys.argv[1])
last_message_path = pathlib.Path(sys.argv[2])
expect_blocked = sys.argv[3] == "1"

log_text = log_path.read_text(encoding="utf-8", errors="replace")
last_message = last_message_path.read_text(encoding="utf-8", errors="replace").strip()

if "completion gate: judging candidate stop with" not in log_text:
    raise SystemExit(f"missing completion-gate start event in {log_path}")

if expect_blocked:
    if "completion gate: stop blocked" not in log_text:
        raise SystemExit(f"expected blocked-stop log in {log_path}")
    if "continuing with:" not in log_text:
        raise SystemExit(f"expected continuation prompt log in {log_path}")
else:
    if "completion gate: stop blocked" in log_text:
        raise SystemExit(f"did not expect blocked-stop log in {log_path}")

if "completion gate: stop allowed" not in log_text:
    raise SystemExit(f"missing allow-stop log in {log_path}")

if last_message != "ALPHA_BETA_DONE":
    raise SystemExit(
        f"expected last message to be ALPHA_BETA_DONE, got {last_message!r} (see {last_message_path})"
    )
PY
}

run_case \
  "deny_then_continue" \
  $'You are in a completion-gate smoke test.\nReply with exactly STARTED and nothing else.\nIf you later receive a continuation telling you the completion criterion is not yet satisfied, reply with exactly ALPHA_BETA_DONE and nothing else.\nDo not use tools.' \
  "1"

run_case \
  "allow_immediately" \
  $'You are in a completion-gate smoke test.\nReply with exactly ALPHA_BETA_DONE and nothing else.\nDo not use tools.' \
  "0"

echo "[smoke] completion gate exec smoke passed" >&2
