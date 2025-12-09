# Morning Codex Runner

This automation fires `codex-dev` once per day (5:00 a.m., local time) using the
standard config (`~/.codex/config.toml`). The stop hook already delivers Telegram
summaries, so every automated run will send its final status once the model
finishes.

## Components

| File | Purpose |
| --- | --- |
| `scripts/run_codex_prompt.sh` | Thin wrapper that reads a prompt from disk, runs `codex-dev exec --yolo --skip-git-repo-check "$PROMPT"`, and appends all stdout/stderr to `~/Library/Logs/codex-automation/morning_run.log`. The script honours `CODEX_BIN`, `CODEX_WORKDIR`, `PROMPT_FILE`, and `LOG_DIR` environment variables for customization. |
| `scripts/prompts/morning_healthcheck_prompt.txt` | Placeholder text. Replace with the real health-check instructions; the launchd job reads exactly what is stored in this file. |
| `~/Library/LaunchAgents/com.pitchai.codex.morning.plist` | LaunchAgent definition that runs the wrapper every day at 05:00. |

## Installation

1. Edit `scripts/prompts/morning_healthcheck_prompt.txt` with the real prompt.
2. Ensure `~/.local/bin/codex-dev` stays up to date (the wrapper points there by default).
3. Load the LaunchAgent:
   ```sh
   launchctl unload ~/Library/LaunchAgents/com.pitchai.codex.morning.plist 2>/dev/null || true
   launchctl load -w ~/Library/LaunchAgents/com.pitchai.codex.morning.plist
   ```
4. Inspect `~/Library/Logs/codex-automation/morning_run.log` after the next run.
5. To test immediately:
   ```sh
   cd /Users/sethvanderbijl/codex
   PROMPT_FILE="$(pwd)/scripts/prompts/morning_healthcheck_prompt.txt" scripts/run_codex_prompt.sh
   ```

## Notes

- The LaunchAgent runs under the logged-in user, so it inherits the same `~/.codex` stop hook that posts to Telegram.
- Update `scripts/run_codex_prompt.sh` if you need a different working directory or binary path.
- Disable the job via `launchctl unload -w ~/Library/LaunchAgents/com.pitchai.codex.morning.plist` whenever you need to pause the automation.
