#!/usr/bin/env bash

set -euo pipefail

if [[ -z "${CODEX_AUTH_JSON_B64:-}" ]]; then
  echo "Missing CODEX_AUTH_JSON_B64" >&2
  exit 2
fi

mkdir -p "$HOME/.codex"
echo "$CODEX_AUTH_JSON_B64" | base64 -d > "$HOME/.codex/auth.json"
chmod 600 "$HOME/.codex/auth.json"

MODEL_ARGS=()
CONFIG_OVERRIDES=()

# We run under a ChatGPT account in production, which does not support the full
# OpenAI model catalog. Interpret "gpt-5.2-medium/high" as reasoning-effort
# tiers on top of the supported Codex model.
case "${PITCHAI_CODEX_MODEL:-}" in
  "" )
    ;;
  gpt-5.2-medium )
    MODEL_ARGS=(-m gpt-5.2-codex)
    CONFIG_OVERRIDES=(-c model_reasoning_effort=medium)
    ;;
  gpt-5.2-high )
    MODEL_ARGS=(-m gpt-5.2-codex)
    CONFIG_OVERRIDES=(-c model_reasoning_effort=high)
    ;;
  * )
    MODEL_ARGS=(-m "$PITCHAI_CODEX_MODEL")
    ;;
esac

PROMPT_PATH="/opt/pitchai/sharepoint_inbox_prompt.md"
if [[ -n "${PITCHAI_MAX_FILES:-}" ]]; then
  if ! [[ "$PITCHAI_MAX_FILES" =~ ^[0-9]+$ ]] || [[ "$PITCHAI_MAX_FILES" -le 0 ]]; then
    echo "Invalid PITCHAI_MAX_FILES (expected positive integer): $PITCHAI_MAX_FILES" >&2
    exit 2
  fi

  TMP_PROMPT="$(mktemp /tmp/pitchai_sharepoint_prompt.XXXXXX.md)"
  cat > "$TMP_PROMPT" <<EOF
For this run ONLY: process at most $PITCHAI_MAX_FILES files.
- Call sp_list_inbox with limit=$PITCHAI_MAX_FILES.
- Process only those files, then stop and output the summary.

EOF
  cat "$PROMPT_PATH" >> "$TMP_PROMPT"
  PROMPT_PATH="$TMP_PROMPT"
fi

exec codex exec \
  --config-file /opt/pitchai/config.toml \
  --skip-git-repo-check \
  --json \
  "${MODEL_ARGS[@]}" \
  "${CONFIG_OVERRIDES[@]}" \
  - < "$PROMPT_PATH"
