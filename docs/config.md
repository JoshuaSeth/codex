# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## CLI config overrides

You can override where Codex reads configuration/state without changing global environment variables:

| Flag | What it overrides | Example |
| ---- | ----------------- | ------- |
| `--config-home DIR` | Entire Codex home (auth, sessions, hooks, `config.toml`, logs). Mirrors `$CODEX_HOME`. | `codex-dev --config-home ~/.codex-dev exec "status"` |
| `--config-file FILE` | Only the TOML config file, while keeping the rest of `$CODEX_HOME` unchanged. | `codex exec --config-file ./ci/replay.toml -- sandbox ls` |

Both options accept relative or absolute paths; Codex canonicalizes them before use so downstream helpers (config editing, session logging, etc.) all use the same resolved location.

## Prompt sequences

Sometimes you want Codex to run through a fixed series of prompts without babysitting the terminal. Supply `--prompt-sequence FILE` and Codex will:

1. Load the `[[steps]]` table from the provided TOML file.
2. Send the first step's `prompt` (plus any `attachments`) as if you typed it.
3. After each `TaskComplete` event, automatically submit the next step until the sequence finishes.

Example sequence (`docs/prompt_sequences/two_step_demo.toml`):

```toml
[[steps]]
name = "Greeting"
prompt = "Respond with a concise hello and nothing else."

[[steps]]
name = "Status tag"
prompt = "Acknowledge that the greeting was sent and output only: <status>SEQUENCE_COMPLETE</status>"
```

Launch it with:

```bash
codex-dev exec --config-file ~/.codex/default.toml --prompt-sequence docs/prompt_sequences/two_step_demo.toml --yolo
```

Notes:

- `--prompt-sequence` cannot be combined with an explicit PROMPT argument, `--image`, or exec subcommands like `codex exec review` / `codex exec resume`.
- Attachments listed under `attachments = ["relative/path.png"]` are resolved relative to the sequence file on disk.

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

When Codex knows which client started the turn, the legacy notify JSON payload also includes a top-level `client` field. The TUI reports `codex-tui`, and the app server reports the `clientInfo.name` value from `initialize`.

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## SQLite State DB

Codex stores the SQLite-backed state DB under `sqlite_home` (config key) or the
`CODEX_SQLITE_HOME` environment variable. When unset, WorkspaceWrite sandbox
sessions default to a temp directory; other modes default to `CODEX_HOME`.

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

## Plan mode defaults

`plan_mode_reasoning_effort` lets you set a Plan-mode-specific default reasoning
effort override. When unset, Plan mode uses the built-in Plan preset default
(currently `medium`). When explicitly set (including `none`), it overrides the
Plan preset. The string value `none` means "no reasoning" (an explicit Plan
override), not "inherit the global default". There is currently no separate
config value for "follow the global default in Plan mode".

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
