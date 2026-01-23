# Non-interactive mode

For information about non-interactive mode, see [this documentation](https://developers.openai.com/codex/noninteractive).

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
