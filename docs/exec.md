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
- `PITCHAI_CYBER_RETRY_MAX`
- `PITCHAI_CYBER_RETRY_BACKOFF_INITIAL_S`
- `PITCHAI_CYBER_RETRY_BACKOFF_MAX_S`

PitchAI runners also treat the high-risk cyber reroute warning (`chatgpt.com/cyber`) as an
incomplete attempt and replay the original prompt instead of accepting the downgraded result.

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

## Persistent terminal mode (`--persistent`)

Use `--persistent` for sessions that must not conclude while a session terminal is still alive:

- Any still-live TTY-backed unified-exec terminal blocks turn completion.
- Assistant-only status updates are treated as in-between progress, not completion.
- This is opt-in because intentionally leaving a shell open keeps the turn running.

Examples:

```bash
codex exec --persistent "start the long-running job and stay on it until the terminal exits"
codex exec resume --persistent 019c... "keep watching the background terminal"
```

## Non-stop mode (`--non-stop`)

Use `--non-stop` when you want Codex to keep running regardless of what it thinks the answer is:

- Normal turn completion is disabled entirely.
- `--non-stop-for <duration>` enables the same behavior only until the timeout expires, then lets
  the next normal final answer stop the turn cleanly.
- This is stronger than `--persistent`; live terminals are not required.
- While a non-stop turn is running, submitted messages open a mode picker:
  - `Steer now`
  - `After next normal stop`
  - `Timed release`
- The explicit queue shortcut opens the same picker, defaulting to `After next normal stop`.
- In TUI sessions, `/non-stop on`, `/non-stop off`, `/non-stop status`, `/non-stop <duration>`,
  and `/non-stop on <duration>` override the live session setting without restarting the thread.
- `/deep <count>` arms the next `<count>` new turns with 4 extra candidate-stop follow-ups before
  normal stopping resumes. Immediate steers into a currently running turn do not consume it.
- If `/non-stop on|<duration>` is entered while a normal turn is already running, the change
  queues behind that turn, applies after it finishes, and affects the next queued message.
- If `/non-stop off` is entered while a running turn is currently held open only by non-stop mode,
  it applies immediately so the current turn may stop at its next normal completion boundary.
- `/enqueue-in <delay> <message>` still works for custom timed releases.
- The session keeps sampling until it is externally interrupted, aborted, or otherwise forced to stop.

Example:

```bash
codex exec --non-stop "keep going indefinitely until I interrupt you"
codex exec --non-stop-for 2h "keep going until timeout, then stop on the next normal final answer"
```

## Continuous voice mode (`--voice`)

Use `--voice` when a Codex session should stay live in the Dispatch voice cockpit:

- `--voice` is a first-class session mode, not the old `codex-dev --use-speech` notify hook.
- `--voice` does not force non-stop behavior by itself; add `--non-stop` if you want automatic follow-up sampling to continue after normal completion.
- `codex-dev --voice` now auto-boots a local dockerized voice web container on `127.0.0.1`,
  exposes an HTTPS public host URL for remote browsers, points Codex at that local speech endpoint, and does not depend on the shared
  `dispatch.pitchai.net` app deployment.
- The wrapper prints both a local cockpit URL and a public cockpit URL. The public URL carries the
  per-instance dispatch token in the query string so it can be opened from another machine.
- When a browser session is available, `codex-dev --voice` also auto-opens the preferred voice
  cockpit URL, which now prefers the public HTTPS voice page over the private local container URL.
  Set `PITCHAI_CODEX_VOICE_WEB_AUTO_OPEN=0` to disable that behavior.
- The public URL is intentionally HTTPS because browsers do not allow microphone capture from an
  insecure remote HTTP origin. `codex-dev --voice` reuses the host nginx/TLS edge for this while
  keeping the actual voice app self-contained in the local container.
- `codex-dev --voice` also forces `--yolo` unless you already passed
  `--yolo` / `--dangerously-bypass-approvals-and-sandbox`, so live voice sessions do not stall on
  in-app approval prompts.
- When `codex-dev --voice` chooses a default model on your behalf, it keeps the normal
  `gpt-5.4` default unless you explicitly override it.
- By default, `codex-dev --voice` reuses the latest already-built local voice image and only
  builds when that image tag is missing. Set `PITCHAI_CODEX_VOICE_WEB_FORCE_BUILD=1` to force a
  rebuild, or `PITCHAI_CODEX_VOICE_WEB_REBUILD_ON_CHANGE=1` to rebuild when the local voice build
  context changes.
- Once the local voice web image exists, `codex-dev --voice` also no longer requires the
  `pitchai_dispatch` repo checkout to stay present; only the local container/image and the
  speech/STT secrets are required at runtime.
- If `codex-dev --voice` is launched outside tmux, the wrapper creates a dedicated tmux session
  first so live transcripts can paste straight into the running turn.
- Live voice transcripts steer directly into the running turn while work is active, or start a
  fresh new turn after Codex has stopped normally.
- Assistant speech starts streaming as soon as enough visible text is available, and the final
  completed message is still pushed if it changed.
- Dispatch voice cockpit preemption remains latest-wins: a newer assistant update fades out and
  replaces older playback.
- Live AssemblyAI transcripts from the voice cockpit submit directly into the running turn instead
  of queueing behind normal completion.
- Voice-originated user turns are tagged internally as voice transcripts and render back in thread
  history with a visible `[voice]` marker.
- In TUI sessions, `/voice on`, `/voice off`, and `/voice status` override the live session
  setting without restarting the thread. If `/voice on|off` is entered while a turn is already
  running, the change queues behind that turn and applies before the next queued message runs.

Example:

```bash
codex --voice
codex-dev --voice "stay in a continuous spoken conversation on the local voice web container"
codex exec --voice "stay in a continuous spoken conversation until I interrupt you"
```

## Completion gate (`--completion-criteria`)

Use the completion gate when Codex should only be allowed to stop after a second LLM judge signs off on the latest candidate final answer.

- `--completion-criteria <TEXT>` enables the gate for the session.
- `--completion-criteria-file <FILE>` loads longer criteria from disk.
- `--completion-judge-model`, `--completion-judge-base-url`, and `--completion-judge-api-key-env` override the judge transport.
- `--completion-judge-timeout-ms`, `--completion-judge-max-retries`, `--completion-judge-max-assistant-messages`, and `--completion-judge-max-user-messages` tune the bounded judge request.
- The judge sees the original user request plus a bounded XML transcript window from live in-memory history and returns strict JSON-schema output.
- If the judge denies stop, Codex injects the returned continuation prompt as contextual user input and keeps going.
- If the judge returns invalid JSON or errors out, the gate fails closed and keeps the turn alive.

TUI sessions can update the active criterion in-place with:

- `/completion-criteria <TEXT>`
- `/completion-criteria status`
- `/completion-criteria clear`

Examples:

```bash
codex exec --completion-criteria "Only stop after you run the requested verification and report the result." "finish the task"
codex exec --completion-criteria-file /tmp/criterion.txt --completion-judge-model gpt-5.1-mini "work until the criterion is satisfied"
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
