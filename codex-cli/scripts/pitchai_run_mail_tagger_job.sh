#!/usr/bin/env bash

set -euo pipefail

if [[ -z "${CODEX_AUTH_JSON_B64:-}" ]]; then
  echo "Missing CODEX_AUTH_JSON_B64" >&2
  exit 2
fi

mkdir -p "$HOME/.codex"
echo "$CODEX_AUTH_JSON_B64" | base64 -d > "$HOME/.codex/auth.json"
chmod 600 "$HOME/.codex/auth.json" || true

MODEL_ARGS=()
CONFIG_OVERRIDES=()

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

PROMPT_PATH="/opt/pitchai/mail_tagger_prompt.md"
CONFIG_PATH="/opt/pitchai/mail_tagger_config.toml"

if [[ ! -r "$CONFIG_PATH" ]]; then
  echo "Missing or unreadable Codex config: $CONFIG_PATH" >&2
  ls -la /opt/pitchai >&2 || true
  exit 2
fi
if [[ ! -r "$PROMPT_PATH" ]]; then
  echo "Missing or unreadable prompt: $PROMPT_PATH" >&2
  ls -la /opt/pitchai >&2 || true
  exit 2
fi

# Ensure the config path is applied even if --config-file parsing changes between Codex versions.
export CODEX_CONFIG_FILE="$CONFIG_PATH"

cp "$CONFIG_PATH" "$HOME/.codex/config.toml"
chmod 600 "$HOME/.codex/config.toml" || true

exec codex --config-file "$CONFIG_PATH" exec \
  --skip-git-repo-check \
  --json \
  "${MODEL_ARGS[@]}" \
  "${CONFIG_OVERRIDES[@]}" \
  - < "$PROMPT_PATH"
