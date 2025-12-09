#!/bin/zsh
set -euo pipefail

: "${CODEX_BIN:=$HOME/.local/bin/codex-dev}"
: "${CODEX_WORKDIR:=$HOME}"
PROMPT_FILE=${PROMPT_FILE:-"$PWD/scripts/prompts/morning_healthcheck_prompt.txt"}
LOG_DIR=${LOG_DIR:-"$HOME/Library/Logs/codex-automation"}
mkdir -p "$LOG_DIR"

if [[ ! -x "$CODEX_BIN" ]]; then
  echo "codex binary not found at $CODEX_BIN" >&2
  exit 1
fi

if [[ ! -f "$PROMPT_FILE" ]]; then
  echo "prompt file $PROMPT_FILE not found" >&2
  exit 1
fi
PROMPT=$(<"$PROMPT_FILE")
if [[ -z "$PROMPT" ]]; then
  echo "prompt file $PROMPT_FILE is empty" >&2
  exit 1
fi

cd "$CODEX_WORKDIR"
RUN_TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
echo "[$RUN_TS] Starting codex-dev automation" >> "$LOG_DIR/morning_run.log"
"$CODEX_BIN" exec --yolo --skip-git-repo-check "$PROMPT" >> "$LOG_DIR/morning_run.log" 2>&1
