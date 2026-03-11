# PitchAI `codex-dev` Fork Preservation Guide

## Purpose

This document is the maintenance checklist for keeping our fork behavior intact while syncing upstream.
Use it before/after every upstream merge.

Canonical full local commit inventory is in:

- `docs/pitchai-fork-commit-inventory.txt`

That file is generated from:

```bash
git log --no-merges --oneline upstream/main..main
```

## Must-preserve fork behaviors

### 1) Turn continuation and follow-up queueing (no accidental early stop)

Keep all behavior that ensures a turn continues when more model work is required.

Required outcomes:

- In-flight user input is queued as follow-ups while a turn is running.
- Editing one queued message never destroys later queued follow-ups.
- Tool output delivery is awaited correctly before deciding a turn is done.
- Websocket streams that omit `response.output_item.done` still continue correctly.

Key related fork commits:

- `4292aa2de` (`tui: queue submissions while a turn is running`)
- `d93cabc3f` (`tui: preserve newer queued drafts when editing`)
- `9dc9b9dda` (`core: wait for delivered pending tool results`)
- `7d03ea839` (`Fix queued input handling on stream disconnect`)

Current local fix (post-upstream merge):

- Websocket path now synthesizes `OutputItemDone` on `response.completed` parity with SSE, preventing premature turn completion when the server only emits `response.output_item.added`.

### 2) Steering vs follow-up semantics

We preserve classic follow-up queue behavior (not steering replacement behavior).

Required outcomes:

- New user messages sent during active turn are queued as normal follow-ups.
- The queued draft and follow-up chain stay stable across edits and reconnects.

Primary touchpoints:

- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/core/src/codex.rs`

### 3) Stream/network resilience and retries

Keep the higher retry behavior introduced in fork work.

Required outcomes:

- Stream disconnects are retried aggressively with backoff.
- Global stream retry budget remains higher than upstream defaults.
- Retry warnings remain visible but non-fatal when recovery is possible.

Key related fork commits:

- `1e0738a13` (`core: retry stream disconnects up to 30x`)
- `ae40e8149` (`core: raise default stream retry budget`)

### 4) Silent reroutes (model reroute handling)

Keep reroute detection and non-spam warning behavior.

Required outcomes:

- Detect backend model reroutes and surface explicit notice.
- Loudly call out reroutes with both `ModelReroute` and `Warning` events.
- Persist the warning in history so resumed sessions keep the context.
- Avoid repeated warning spam in a single turn/session.
- Keep fallback behavior that protects codex performance when rerouted.

Primary touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/codex-api/src/sse/responses.rs`
- `codex-rs/codex-api/src/endpoint/responses_websocket.rs`

Key related fork commits:

- `97ae4c13c`
- `11ce84689`
- `cc7beea2d`

### 5) Hooks, custom tools, and custom config behaviors

Keep the fork extensions around hooks, orchestration profiles, and custom tools.

Required outcomes:

- Tool hooks and stop hooks remain enabled and stable.
- Hook telemetry and stop-hook integrations remain working.
- Config-defined CLI/custom tools and profile overrides remain supported.

Key related fork commits:

- `fb1bbdbd5`, `eeca20185`, `9093784a8`
- `5ab5fb818`, `997f70922`, `7ba7eeba9`
- `1695e53a7`, `98e0fb33b`, `005bc0b30`

### 6) Resume/session ergonomics

Keep fork resume improvements.

Required outcomes:

- Resume preserves/refreshes working context (`cwd`, branch/environment paths).
- Session selectors and resume branch behavior remain available.

Key related fork commits:

- `ddbc65e57`, `b6901f990`, `cd05ee828`, `6f627a4ce`

### 7) Dispatcher/automation integrations

Keep PitchAI operational integrations used by agents/jobs.

Scope includes:

- Dispatcher polling/status/run orchestration
- Telegram job/stop-hook flows
- SharePoint inbox automation and move robustness
- Mailbox tagger tools and related job runners

Representative fork commits:

- `ce9da3c4f`, `62a512b50`, `e7f21ab18`, `8238ff6fe`
- `b1436b559`, `d40ea5e52`, `58b80d505`
- `f0efdd306`, `5e1509923`, `19bb1c121`, `3b2478f8a`

### 8) Auth-token-server policy and usage-limit continuation

Keep PitchAI auth policy explicit and visible in CLI/help/docs.

Required outcomes:

- Default auth path is managed shared auth in `$CODEX_HOME/auth.json` (broker-issued for automation).
- `CODEX_API_KEY` is never treated as implicit fallback.
- API-key mode is explicit-only (`CODEX_FORCE_API_KEY_AUTH=1`) and clearly warned.
- Runner reports usage-limit outcomes to broker, refreshes lease auth, and auto-continues bounded retries.

Primary touchpoints:

- `codex-rs/core/src/auth.rs`
- `codex-rs/exec/src/cli.rs`
- `codex-rs/exec/src/lib.rs`
- `codex-rs/cli/src/main.rs`
- `codex-rs/cli/src/login.rs`
- `codex-cli/scripts/pitchai_run_codex_job.py`
- `docs/authentication.md`
- `docs/exec.md`
- `docs/pitchai_generic_codex_job.md`

## Verification checklist after every upstream merge

Run at minimum:

```bash
# Verify local delta still exists.
git log --no-merges --oneline upstream/main..main

# Turn continuation + websocket parity regression coverage.
cd codex-rs
cargo test -p codex-api websocket_event_queue_synthesizes_missing_output_item_done -- --nocapture
cargo test -p codex-api synthesizes_output_item_done_when_missing -- --nocapture
cargo test -p codex-core websocket_v2_synthesizes_tool_done_when_missing -- --nocapture
cargo test -p codex-core websocket_v2_test_codex_shell_chain -- --nocapture
cargo test -p codex-core openai_model_header_mismatch_emits_warning_event_and_warning_item -- --nocapture
cargo test -p codex-core response_model_field_mismatch_emits_warning_when_header_matches_requested -- --nocapture
```

## Merge policy notes

- Prefer explicit failure in core/business logic instead of silent fallbacks.
- If upstream changes stream/event plumbing, re-verify websocket and SSE parity immediately.
- Keep this guide updated whenever fork-only behavior changes.
