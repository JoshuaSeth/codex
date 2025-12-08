# Custom Tool Helpers

These Python scripts are packaged with the repo so you can quickly wire config-defined tools without copying files outside the workspace.

## Available scripts

| Script | Description | Sample command |
| --- | --- | --- |
| `count_files.py` | Counts files under a directory (defaults to the current working directory). Supports optional `{"path": "./docs", "follow_symlinks": false, "include_hidden": false}` arguments. | `command = ["python3", "./tools/custom_tools/count_files.py"]` |
| `send_email_stub.py` | Development-only email stub that appends subject/body/metadata to a JSON Lines log file. Respects `CODEX_SEND_EMAIL_LOG` and defaults to `~/.codex/dev_emails.jsonl`. | `command = ["python3", "./tools/custom_tools/send_email_stub.py"]` |
| `echo_tool.py` | Simple tool that echoes the provided text. Useful for demos and smoke tests. | `command = ["python3", "./tools/custom_tools/echo_tool.py"]` |
| `telegram_bot.py` | Telegram notifier utilities plus a `--stop-hook` mode that reads Codex stop-hook payloads from stdin and posts the final message (and working directory) to your configured channel. Reads `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` (or a repo-level `.env`); set `CODEX_STOP_HOOK_LOG` to capture local debug entries. | `stop_hook_command = ["python3", "./tools/custom_tools/telegram_bot.py", "--stop-hook"]` |

Each script reads tool arguments from the `CODEX_TOOL_ARGS_JSON` environment variable (set automatically when you register a `[custom_tools.*]` entry) and writes structured JSON to stdout for Codex to capture.

If your helper only queues work (for example, kicking off a CI pipeline or waiting for a webhook), set `shutdown_after_call = true` in the corresponding config stanza. Codex will log the tool output, emit a background note, and shut down so you can `codex resume` once the external system finishes.

Additions are welcome—drop new scripts in this directory when you find reusable patterns.
