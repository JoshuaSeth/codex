#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DISPATCH_ROOT="/root/code/pitchai_dispatch"
CODEX_BIN="${PITCHAI_REAL_CODEX_BIN:-$ROOT/codex-rs/target/release/codex}"
AUTH_JSON="${PITCHAI_REAL_CODEX_AUTH:-$HOME/.codex/auth.json}"
TTS_BASE_URL="${PITCHAI_TTS_BASE_URL:-http://127.0.0.1:8891}"

if [[ ! -x "$CODEX_BIN" ]]; then
  echo "[voice-smoke] missing Codex binary at $CODEX_BIN" >&2
  exit 1
fi

if [[ ! -f "$AUTH_JSON" ]]; then
  echo "[voice-smoke] missing Codex auth at $AUTH_JSON" >&2
  exit 1
fi

if ! curl -sS "$TTS_BASE_URL/" >/dev/null 2>&1; then
  echo "[voice-smoke] TTS base URL is unreachable: $TTS_BASE_URL" >&2
  exit 1
fi

pushd "$DISPATCH_ROOT" >/dev/null
PITCHAI_TTS_BASE_URL="$TTS_BASE_URL" \
PITCHAI_REAL_CODEX_BIN="$CODEX_BIN" \
PITCHAI_REAL_CODEX_AUTH="$AUTH_JSON" \
npx playwright test playwright/tests/tmux_voice_real_codex.spec.cjs --config=playwright.config.cjs
popd >/dev/null
