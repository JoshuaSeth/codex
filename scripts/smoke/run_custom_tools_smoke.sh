#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

echo "[smoke] validating PitchAI extensions (custom tools/hooks/pending tools)" >&2
echo "[smoke] note: end-to-end custom-tool *model* calls require API-key auth; ChatGPT login may not expose custom tools to the model." >&2

pushd "$ROOT/codex-rs" >/dev/null

just fmt >/dev/null

# These tests execute real tool commands (python scripts) and exercise:
# - config-defined custom tools
# - hibernate_after_call pending-tool lifecycle + deliver-pending
# - tool hooks and stop hooks (including hook directives like timeout overrides)
cargo test -p codex-core suite::tools::config_defined_custom_tool_runs_command
cargo test -p codex-core suite::tools::custom_tool_hibernate_after_call_triggers_pending_flow
cargo test -p codex-core suite::hooks::tool_and_stop_hooks_run_and_tool_hook_can_override_timeout

popd >/dev/null

echo "[smoke] ok" >&2
