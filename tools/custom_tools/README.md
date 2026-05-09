# Custom Tool Helpers

These Python scripts are packaged with the repo so you can quickly wire config-defined tools without copying files outside the workspace.

## Available scripts

| Script | Description | Sample command |
| --- | --- | --- |
| `count_files.py` | Counts files under a directory (defaults to the current working directory). Supports optional `{"path": "./docs", "follow_symlinks": false, "include_hidden": false}` arguments. | `command = ["python3", "./tools/custom_tools/count_files.py"]` |
| `send_email_stub.py` | Development-only email stub that appends subject/body/metadata to a JSON Lines log file. Respects `CODEX_SEND_EMAIL_LOG` and defaults to `~/.codex/dev_emails.jsonl`. | `command = ["python3", "./tools/custom_tools/send_email_stub.py"]` |
| `wait_for_email_response.py` | Registers Graph `internet_message_id` values in Elise's outbox index so an external automation can resume a conversation when a reply arrives. | `command = ["python3", "./tools/custom_tools/wait_for_email_response.py"]` |
| `web_form_colleague.py` | Queues a Chrome DevTools-based web agent run that Elise uses for web forms (writes task metadata and launches a background runner). | `command = ["python3", "./tools/custom_tools/web_form_colleague.py"]` |
| `mail_search.py` | Lists Outlook mail (defaults to newest unread Inbox messages) with subject, sender, timestamps, previews, and Graph ids. Supports optional `{"query": "invoice", "limit": 5, "unread_only": false}` arguments. | `command = ["python3", "./tools/custom_tools/mail_search.py"]` |
| `mail_read.py` | Reads a single Outlook message given a `message_id` or `internet_message_id`, returning the HTML body (or a text fallback) plus metadata. Optional `{"mark_as_read": true}` will flag the note as read. | `command = ["python3", "./tools/custom_tools/mail_read.py"]` |
| `echo_tool.py` | Simple tool that echoes the provided text. Useful for demos and smoke tests. | `command = ["python3", "./tools/custom_tools/echo_tool.py"]` |
| `telegram_bot.py` | Telegram notifier utilities plus a `--stop-hook` mode that reads Codex stop-hook payloads from stdin and posts the final message to the updates channel. It prefers `PITCHAI_UPDATES_TELEGRAM_CHAT_ID` / `TELEGRAM_UPDATES_CHAT_ID` over `TELEGRAM_CHAT_ID`, persists the sent Telegram `message_id` plus Codex/tmux route metadata when the PM DB is configured, and enables reply-to-message routing back to the originating Codex session. Set `CODEX_STOP_HOOK_LOG` to capture local debug entries. | `stop_hook_command = ["python3", "./tools/custom_tools/telegram_bot.py", "--stop-hook"]` |
| `ask_colleague.py` | Recursively invokes `codex-dev` so the agent can ask or resume a named “colleague” conversation using friendly ids. | `command = ["python3", "./tools/custom_tools/ask_colleague.py"]` |

Each script reads tool arguments from the `CODEX_TOOL_ARGS_JSON` environment variable (set automatically when you register a `[custom_tools.*]` entry) and writes structured JSON to stdout for Codex to capture.

Additions are welcome—drop new scripts in this directory when you find reusable patterns.

## Path resolution note

Codex resolves **path-typed** fields in `config.toml` (for example MCP server `cwd` entries) relative to the folder containing that config file (for each config layer). In practice:

- If you define tools in `.codex/config.toml` (recommended for repo-local tooling), `./tools/custom_tools/...` works because `.codex/` lives under your repo.

For `custom_tools.*.command`, Codex does **not** rewrite command argv values: relative command paths are resolved by the OS relative to the tool’s working directory (the tool’s configured `cwd`, otherwise the session `cwd`).
