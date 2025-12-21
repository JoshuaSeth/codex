#!/usr/bin/env bash

set -euo pipefail

if [[ -z "${CODEX_AUTH_JSON_B64:-}" ]]; then
  echo "Missing CODEX_AUTH_JSON_B64" >&2
  exit 2
fi

mkdir -p "$HOME/.codex"
echo "$CODEX_AUTH_JSON_B64" | base64 -d > "$HOME/.codex/auth.json"
chmod 600 "$HOME/.codex/auth.json"

exec codex exec \
  --config-file /opt/pitchai/config.toml \
  --skip-git-repo-check \
  --json \
  - < /opt/pitchai/sharepoint_inbox_prompt.md

