# Non-interactive mode

For information about non-interactive mode, see [this documentation](https://developers.openai.com/codex/noninteractive).

## Auth precedence (PitchAI fork)

- `codex exec` prefers managed shared auth from `$CODEX_HOME/auth.json` (or configured keyring store).
- In PitchAI environments this managed auth should come from the auth-token broker/server.
- `CODEX_API_KEY` is not used as implicit fallback.
- API-key mode is enabled only when explicitly forced with `CODEX_FORCE_API_KEY_AUTH=1`.
- PitchAI default workflow is shared/broker auth; API-key mode is intentionally exceptional.

## Broker + Auto-Continue (PitchAI runners)

PitchAI wrapper runners around `codex exec` (for example `run_codex_job.py`) add broker-aware recovery:

- Acquire broker lease auth (`CODEX_AUTH_BROKER_URL`, `CODEX_AUTH_BROKER_TOKEN`).
- Run `codex exec --json` against lease-backed `auth.json`.
- On usage/rate-limit outcomes, report lease status and fetch a fresh lease.
- Resume the same thread automatically with a bounded backoff/retry budget.

Runner controls:

- `PITCHAI_BROKER_USAGE_LIMIT_AUTO_CONTINUE_MAX`
- `PITCHAI_BROKER_USAGE_LIMIT_BACKOFF_INITIAL_S`
- `PITCHAI_BROKER_USAGE_LIMIT_BACKOFF_MAX_S`
- `PITCHAI_BROKER_AUTO_CONTINUE_PROMPT`

## Strict folder scope (`--strict-dir`)

Use repeatable `--strict-dir` flags when you need a session that is file-scoped:

- Reads and writes are restricted to the listed roots.
- `workspace-write` sandbox mode is implied automatically.
- Default writable temp roots (`/tmp`, `$TMPDIR`) are disabled.
- Tool command behavior is unchanged; approval policy still governs escalation.

Example:

```bash
codex exec --strict-dir /repo --strict-dir /shared "run tests and patch issues"
```

## JSON event stream

`codex exec --json` emits a JSONL stream to stdout describing thread/turn/item lifecycle events.

Capture to a file:

```bash
codex exec --json "summarize this repo" > /tmp/codex.events.jsonl
```

Attach a read-only viewer (can be run later, at any time):

```bash
codex view /tmp/codex.events.jsonl
```

Run exec detached and immediately attach the viewer:

```bash
codex exec-view "summarize this repo"
```

By default `exec-view` writes events under `$CODEX_HOME/live/exec-view/` and writes stderr to a sibling
`.stderr.log` file.

## Async helpers

Start `codex exec --json` detached and print the thread id immediately:

```bash
THREAD_ID=$(codex exec-async "summarize this repo")
```

`exec-async` also writes a pointer file so other commands can find the right events stream later:

- `$CODEX_HOME/live/<thread_id>.events.jsonl.path`

Summarize what the agent did (status + last turn result):

```bash
codex get-result "$THREAD_ID"
```

Block until any of the given threads finishes (defaults to timing out after 2 hours):

```bash
codex await-any "$THREAD_ID" 019bd2b2-09f5-7dc0-a7d1-1d8e74b0d104
```
