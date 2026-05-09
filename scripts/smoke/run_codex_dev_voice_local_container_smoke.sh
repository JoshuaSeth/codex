#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DISPATCH_ROOT="/root/code/pitchai_dispatch"
CODEX_DEV_BIN="${PITCHAI_REAL_CODEX_DEV_BIN:-/usr/local/bin/codex-dev}"
AUTH_JSON="${PITCHAI_REAL_CODEX_AUTH:-$HOME/.codex/auth.json}"

if [[ ! -x "$CODEX_DEV_BIN" ]]; then
  echo "[codex-dev-voice-smoke] missing codex-dev wrapper at $CODEX_DEV_BIN" >&2
  exit 1
fi

if [[ ! -f "$AUTH_JSON" ]]; then
  echo "[codex-dev-voice-smoke] missing Codex auth at $AUTH_JSON" >&2
  exit 1
fi

IMAGE_NAME="${PITCHAI_CODEX_VOICE_WEB_IMAGE:-pitchai/codex-dev-voice-web:local}"
if [[ -n "${PITCHAI_CODEX_VOICE_WEB_DOCKERFILE:-}" ]]; then
  VOICE_DOCKERFILE="${PITCHAI_CODEX_VOICE_WEB_DOCKERFILE}"
elif [[ -f "$DISPATCH_ROOT/dispatcher/Dockerfile.voice" ]]; then
  VOICE_DOCKERFILE="$DISPATCH_ROOT/dispatcher/Dockerfile.voice"
else
  VOICE_DOCKERFILE="$DISPATCH_ROOT/dispatcher/Dockerfile"
fi

DOCKER_BUILD_ARGS=(-f "$VOICE_DOCKERFILE" -t "$IMAGE_NAME")
if rg -q 'COPY --from=dft_lib_llm ' "$VOICE_DOCKERFILE"; then
  : "${PITCHAI_DFT_LIB_LLM_DIR:=/root/code/dft/libs/lib_llm}"
  if [[ ! -f "$PITCHAI_DFT_LIB_LLM_DIR/pyproject.toml" ]]; then
    echo "[codex-dev-voice-smoke] missing dft_lib_llm build context at $PITCHAI_DFT_LIB_LLM_DIR" >&2
    exit 1
  fi
  DOCKER_BUILD_ARGS+=(--build-context "dft_lib_llm=$PITCHAI_DFT_LIB_LLM_DIR")
fi
docker build "${DOCKER_BUILD_ARGS[@]}" "$DISPATCH_ROOT/dispatcher" >/dev/null

pushd "$DISPATCH_ROOT" >/dev/null
PITCHAI_REAL_CODEX_DEV_BIN="$CODEX_DEV_BIN" \
PITCHAI_REAL_CODEX_AUTH="$AUTH_JSON" \
PITCHAI_CODEX_VOICE_WEB_IMAGE="$IMAGE_NAME" \
PITCHAI_CODEX_VOICE_WEB_DOCKERFILE="$VOICE_DOCKERFILE" \
PITCHAI_CODEX_VOICE_WEB_AUTO_OPEN=0 \
npx playwright test playwright/tests/tmux_voice_codex_dev_local_container.spec.cjs --config=playwright.config.cjs
PITCHAI_REAL_CODEX_DEV_BIN="$CODEX_DEV_BIN" \
PITCHAI_REAL_CODEX_AUTH="$AUTH_JSON" \
PITCHAI_CODEX_VOICE_WEB_IMAGE="$IMAGE_NAME" \
PITCHAI_CODEX_VOICE_WEB_DOCKERFILE="$VOICE_DOCKERFILE" \
PITCHAI_CODEX_VOICE_WEB_AUTO_OPEN=0 \
node playwright/scripts/codex_dev_voice_local_playback_smoke.cjs
popd >/dev/null
