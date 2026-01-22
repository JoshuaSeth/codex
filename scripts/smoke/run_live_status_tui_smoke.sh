#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CODEX_BIN="$ROOT/codex-rs/target/debug/codex"

echo "[smoke] building Codex CLI (codex-cli)..." >&2
pushd "$ROOT/codex-rs" >/dev/null
cargo build -p codex-cli >/dev/null
popd >/dev/null

if [[ ! -f "$HOME/.codex/auth.json" ]]; then
  echo "[smoke] missing $HOME/.codex/auth.json; cannot run real-model TUI smoke." >&2
  exit 1
fi

python3 "$ROOT/scripts/smoke/live_status_tui_smoke.py"

echo "[smoke] ok" >&2
