# PitchAI `codex-dev` Fork Preservation Guide

## Purpose

This document is the merge checklist for keeping our fork behavior intact while syncing upstream.
Use it before and after every upstream merge. Treat it as the canonical behavior inventory for the
local fork, not as a commit-by-commit changelog.

## How to use this document

1. Read this file before starting an upstream merge.
2. Diff the listed touchpoints against upstream if those files changed.
3. Re-run the targeted tests and smokes from the checklist at the end of this file.
4. Update this document immediately whenever we add new fork-only behavior.

## Companion docs

- Full local commit inventory: `docs/pitchai-fork-commit-inventory.txt`
- CLI/runtime behavior reference: `docs/exec.md`
- Completion-gate architecture/spec: `docs/completion-gate-spec.md`
- Continuous voice mode architecture/spec: `docs/voice-mode-spec.md`

The full local commit inventory is generated from:

```bash
git log --no-merges --oneline upstream/main..main
```

## Non-negotiable merge rules

- Do not introduce fallback-heavy behavior into core business logic. Fail loudly in core and only
  use fallbacks at clear I/O or UI edges.
- Preserve the merge-resistant explanatory comments around:
  - background-terminal continuation after empty `write_stdin` waits
  - `/deep` bounded continuation budgets
  - completion-gate stop denial / fail-closed behavior
  - auth/broker precedence and explicit API-key warnings
- If config or protocol shapes change, update generated artifacts in the same change:
  - `just write-config-schema`
  - `just write-app-server-schema`
- If upstream touches turn-completion, queueing, protocol event types, session snapshot/restore, or
  auth selection, assume manual merge work is required even if the code compiles.

## Fork feature inventory

### 1) Turn continuation, queueing, and stop control

This is the most merge-sensitive area in the fork.

Required outcomes:

- New user messages sent during an active turn stay normal queued follow-ups by default. We do not
  want upstream's “turn later messages into steering” behavior as the default.
- Editing an older queued message must never destroy newer queued follow-ups.
- Delivered tool results must be fully observed before deciding that a turn is done.
- Websocket streams that omit `response.output_item.done` must still behave like SSE and continue
  correctly.
- Empty `write_stdin` waits on still-running background terminals must not let the next
  assistant-only status message prematurely end the turn.
- Deliberate sleep-only background terminal waits must not be capped by the normal background
  terminal timeout ceiling.
- Short assistant-only status replies immediately after tool output must not be mistaken for final
  answers. Use backend `phase=commentary` first and only use short-message length as a narrow
  fallback when `phase` is absent.
- `--persistent` must block turn completion while any session terminal is still alive.
- `--non-stop` must suppress normal turn completion entirely.
- `--non-stop-for <duration>` and timed `/non-stop` overrides must auto-continue only until the
  timeout expires, then allow the next ordinary final answer to stop cleanly.
- `/non-stop on|off|status|<duration>|on <duration>` must be a live runtime override, not just a
  startup flag.
- `/non-stop on|<duration>` entered during a running turn must queue behind that turn and apply
  before the next queued message runs.
- `/non-stop off` entered during an actively running non-stop turn must apply immediately so the
  current turn can stop at its next normal completion boundary.
- `/deep <count>` must arm the next `<count>` new turns with a 4-step forced-follow-up budget.
  Immediate steers into an already-running turn must not consume that budget.
- In `--non-stop` mode, submitting while a turn is running must open the submit-mode picker:
  - `Steer now`
  - `After next normal stop`
  - `Timed release`
- The explicit queue shortcut must reuse that same picker and default to `After next normal stop`.
- `TurnCompleteDeferredByNonStop` is the fork-critical boundary signal for the `After next normal
  stop` path.
- `/enqueue-in <delay> <message>` must keep working in non-stop mode and release messages later
  into the active non-stop thread.
- Turn-ending errors must pause queued follow-up autosend instead of draining the entire queue into
  repeated failures.
- Queued model changes must apply before the next queued message runs.

Why this is integral to our fork:

- PitchAI operators frequently park work in background terminals and monitor it across multiple
  sampling rounds.
- The model often responds to those waits with natural-language progress text before the next tool
  call. Upstream-style “assistant-only response means stop” is wrong for this workflow.
- Some workflows need a hard guarantee that Codex keeps going while a terminal is alive
  (`--persistent`) or regardless of normal completion heuristics (`--non-stop`).
- Some workflows need a bounded “go deeper a few more rounds, then stop normally” control. That is
  exactly what `/deep` does.
- A blanket “short message means not final” rule is unsafe. The fork intentionally uses a narrower
  post-tool-output guard instead.

Current local behaviors to preserve:

- Background-terminal continuation guard
- Post-tool-output short commentary guard
- Classic follow-up queue semantics
- Queued edit preservation
- Error-paused queue autosend
- Runtime `/non-stop` override
- Timed non-stop mode
- `/deep` bounded continuation budget
- Non-stop submit picker
- Non-stop boundary queue
- Non-stop delayed release queue
- Queued model changes before next queued message

Primary touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/stream_events_utils.rs`
- `codex-rs/core/src/unified_exec/process_manager.rs`
- `codex-rs/core/src/non_stop.rs`
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/chatwidget/tests.rs`
- `codex-rs/tui/src/slash_command.rs`
- `codex-rs/tui/src/status/card.rs`
- `codex-rs/core/tests/suite/unified_exec.rs`

Representative local commits:

- `4292aa2de` (`tui: queue submissions while a turn is running`)
- `d93cabc3f` (`tui: preserve newer queued drafts when editing`)
- `9dc9b9dda` (`core: wait for delivered pending tool results`)
- `7d03ea839` (`Fix queued input handling on stream disconnect`)
- `1e0738a13` (`core: retry stream disconnects up to 30x`)
- `ae40e8149` (`core: raise default stream retry budget`)

### 2) Steering vs follow-up semantics

We intentionally preserve classic follow-up queueing and only use steering where we explicitly
opted into it.

Required outcomes:

- Default “send while running” stays queued follow-up behavior unless non-stop submit UI chooses
  `Steer now`.
- Non-stop immediate-send behavior is explicit and UI-driven, not an accidental global default.
- Queued drafts, pending steers, and follow-up chains remain stable across edits, reconnects,
  interruptions, and thread restore.

Primary touchpoints:

- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/chatwidget/tests.rs`
- `codex-rs/core/src/codex.rs`

### 3) Conversation repair and resume ergonomics

Required outcomes:

- `/fix` must remain available from the TUI.
- `/fix` reloads the full rollout history from disk, rehydrates the conversation, and then runs a
  local repair compaction.
- `/fix` must fail loudly when session persistence is unavailable instead of silently pretending it
  worked.
- Resume continues to preserve warning context, including reroute warnings and repaired history
  context where applicable.

Why this matters:

- Some broken `.jsonl` rollouts still have enough data for local recovery if we explicitly reload
  the full rollout and compact again.
- Without `/fix`, operators can get stuck on compact/resume errors from damaged rollout history.

Primary touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/rollout/recorder.rs`
- `codex-rs/tui/src/slash_command.rs`
- `codex-rs/tui/src/chatwidget.rs`

### 4) Stream/network resilience and retries

Required outcomes:

- Stream disconnects remain aggressively retried with backoff.
- Selected transport / decoding / bad-request classes continue to use the elevated retry policy
  that the fork added.
- Recovery warnings stay visible to operators but remain non-fatal when retry succeeds.
- SSE and websocket parity must stay intact when the backend emits incomplete item-finish events.

Why this matters:

- Long-running PitchAI jobs routinely hit transient transport failures. Upstream defaults are too
  eager to fail the task.
- Losing the elevated retry policy regresses jobs back to “one flaky stream disconnect kills the
  run.”

Primary touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/codex-api/src/sse/responses.rs`
- `codex-rs/codex-api/src/endpoint/responses_websocket.rs`
- `codex-rs/core/tests/suite/unified_exec.rs`

### 5) Silent reroutes (model mismatch handling)

Required outcomes:

- Detect true backend model reroutes and surface them explicitly.
- Emit both `ModelReroute` and `Warning` style signals.
- Persist reroute context in history so resumes keep the warning context.
- Avoid warning spam inside a single turn or session.
- Only re-emit warnings on real reroutes. Do not warn when the operator already selected the
  served model and there is no actual mismatch.

Why this matters:

- Reroutes can materially change Codex behavior and performance.
- We explicitly want loud detection of silent downgrades, but we do not want bogus noise when the
  requested and served models already match.

Primary touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/codex-api/src/sse/responses.rs`
- `codex-rs/codex-api/src/endpoint/responses_websocket.rs`

Representative local commits:

- `97ae4c13c`
- `11ce84689`
- `cc7beea2d`

### 6) Hooks, custom tools, and custom config behavior

Required outcomes:

- Top-level `tool_hook_command` and `stop_hook_command` config loading must stay enabled.
- Profile overrides for hook commands must continue to work.
- Tool hooks must emit before/after tool execution as expected.
- Stop hooks must run on turn completion and be able to influence timeout behavior where supported.
- Config-defined CLI/custom tools must remain supported.
- The restored hook tests must stay in the suite.

Why this matters:

- Upstream merge work already dropped this once. The break was silent until AFASAsk workflows
  noticed hooks no longer fired.

Primary touchpoints:

- `codex-rs/core/src/config/mod.rs`
- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/tools/mod.rs`
- `codex-rs/core/src/tools/router.rs`
- `codex-rs/core/tests/suite/mod.rs`
- `codex-rs/core/tests/suite/hooks.rs`

Representative local commits:

- `fb1bbdbd5`
- `eeca20185`
- `9093784a8`
- `5ab5fb818`
- `997f70922`
- `7ba7eeba9`
- `1695e53a7`
- `98e0fb33b`
- `005bc0b30`

### 7) Auth-token-server policy and usage-limit continuation

Required outcomes:

- Default auth path is managed shared auth in `$CODEX_HOME/auth.json`.
- In PitchAI automation, that auth is expected to come from the auth-token broker/server.
- `CODEX_API_KEY` must never be treated as implicit fallback.
- API-key mode is explicit-only and clearly warned (`CODEX_FORCE_API_KEY_AUTH=1`).
- CLI help/docs must keep explaining that broker/shared auth is the normal path and API-key mode is
  break-glass only.
- Wrapper runners must classify usage/rate-limit outcomes, report them to the broker, fetch a new
  auth lease, and auto-continue the same thread with backoff.
- Broker outcome detection must recognize both backend machine-readable rate-limit errors and the
  human-facing `"You've hit your usage limit"` output emitted by `codex exec --json`.
- Wrapper runners must also detect the high-risk cyber reroute warning (`chatgpt.com/cyber`) and
  replay the original prompt with a large retry budget instead of silently accepting the downgraded
  fallback model result.

Why this matters:

- Using implicit API-key fallback causes the fork to silently switch onto the wrong auth mode and
  lose the broker recovery path.
- PitchAI automation expects auth-token rotation on usage limits, not permanent task failure.

Primary touchpoints:

- `codex-rs/core/src/auth.rs`
- `codex-rs/exec/src/cli.rs`
- `codex-rs/exec/src/lib.rs`
- `codex-rs/cli/src/main.rs`
- `codex-cli/scripts/pitchai_run_codex_job.py`

### 8) Strict filesystem scoping

Required outcomes:

- `--strict-dir <DIR>` must remain available on `exec` and TUI startup.
- It must stay repeatable and restrict reads and writes to the explicit roots.
- It must imply `workspace-write`.
- It must disable default writable temp roots such as `/tmp` and `$TMPDIR`.
- It must not silently combine with `--dangerously-bypass-approvals-and-sandbox`.
- Commands still run normally inside allowed roots; the feature is a filesystem scope control, not
  a command disablement layer.

Why this matters:

- PitchAI needs sessions that are strictly fenced to selected directories for file access while
  still allowing normal command execution inside those roots.

Primary touchpoints:

- `codex-rs/exec/src/cli.rs`
- `codex-rs/tui/src/cli.rs`
- `codex-rs/exec/src/lib.rs`
- `codex-rs/core/src/config/mod.rs`
- `docs/exec.md`

### 9) LLM completion gate

Required outcomes:

- `--completion-criteria` and `/completion-criteria` enable a session-scoped completion gate.
- Candidate stop boundaries call a second structured-output judge using real in-memory session
  history.
- The judge request includes the original user request plus a bounded XML transcript window, not
  rollout reparsing.
- Judge denials inject contextual continuation prompts and keep the same turn alive.
- Judge failures fail closed and must not silently allow stop.
- Session status, TUI state, and app-server thread start/resume/fork payloads must keep exposing the
  active gate configuration.
- Completion-gate protocol events must remain wired through the app-server:
  - `CompletionGateStarted`
  - `CompletionGateDecision`
  - `CompletionGateBlockedStop`
  - `CompletionGateError`

Why this is integral to our fork:

- `--persistent` and `--non-stop` solve structural “don’t stop yet” problems.
- The completion gate solves “do not stop unless the task-specific completion criterion is
  satisfied.”
- It is easy to lose in an upstream merge because the agent still appears to complete normally
  unless you notice that the extra judge call disappeared.

Primary touchpoints:

- `codex-rs/core/src/completion_gate.rs`
- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/state/session.rs`
- `codex-rs/core/src/contextual_user_message.rs`
- `codex-rs/core/schemas/completion_gate_decision.schema.json`
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/exec/src/cli.rs`
- `codex-rs/exec/src/lib.rs`
- `codex-rs/tui/src/cli.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/status/card.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server/src/codex_message_processor.rs`

### 10) Continuous voice mode (`--voice`)

Required outcomes:

- `--voice` must remain a first-class Codex session flag in the CLI/TUI, not a wrapper-only hook.
- `codex-dev --voice` must bootstrap a local docker voice web container and point assistant speech
  there instead of depending on the shared dispatcher deployment.
- `codex-dev --voice` must print a remotely usable public cockpit URL, not just a `127.0.0.1`
  localhost URL.
- That public cockpit URL must be HTTPS, otherwise remote browsers regress to `mic unsupported`
  because microphone capture is blocked on insecure origins.
- `codex-dev --voice` must keep working when `PITCHAI_DISPATCH_BASE_URL=https://example.invalid`;
  the runtime cannot depend on the shared dispatcher service.
- The public cockpit URL must carry the instance dispatch token so the browser route and websocket
  auth work without a shared dispatcher login cookie.
- `codex-dev --voice` must force yolo semantics by default so live voice sessions do not block on
  in-app approval prompts.
- `codex-dev --voice` must keep the normal `gpt-5.4` default unless the caller explicitly passes
  `--model`.
- Once the local voice image already exists, `codex-dev --voice` must also keep working even if
  the local `pitchai_dispatch` repo checkout is absent; runtime depends on the local image,
  container, tmux context, and speech/STT secrets, not on the shared dispatcher or a live repo
  checkout.
- `codex-dev --voice` must reuse the latest local voice image by default instead of rebuilding on
  every source-tree change; explicit rebuild env vars may opt back into forced or source-change
  rebuilds.
- Inline slash commands, especially `/voice-input ...`, must clear the visible composer text after
  dispatch. If an upstream merge drops that, the raw slash command stays in the composer and later
  Enter presses or lifecycle events can replay the same voice transcript repeatedly, which in turn
  floods the speech queue and starves playback.
- `codex-dev --voice` must create or reuse a tmux session so live transcripts can be pasted
  straight into the active running turn.
- Runtime `/voice on|off|status` overrides must remain available in the TUI.
- Voice mode must be usable with or without `--non-stop`; enabling `/voice on` or `--voice`
  must not silently force non-stop behavior anymore.
- While a turn is active, voice transcripts must still inject directly into that running turn.
- After a normal stop, the next qualifying voice transcript must start a fresh new turn instead of
  requiring non-stop mode.
- `/voice on|off` entered during a running turn must queue behind that turn and apply before the
  next queued message runs.
- Thread snapshot/restore and app-server start/resume/fork payloads must preserve `voice_mode`.
- Voice mode must start speaking streamed assistant updates before turn completion when enough
  visible text is available, and the final completed message must still be pushed if it changed.
- Dispatch tmux voice cockpit must boot with direct speech mode, latest-wins playback, and
  AssemblyAI live transcript auto-send.
- When exactly one newer assistant clip arrives during playback, Dispatch audio must prefer a
  softer sentence-boundary handoff instead of always hard-cutting mid-sentence.
- If backlog grows beyond one pending newer clip, Dispatch audio must immediately drop older
  pending speech and jump to the latest block; stale intermediate clips must never keep speaking.
- Finalized voice transcripts that meet the configured minimum word count must inject directly into
  the running turn instead of queueing behind normal completion.
- Dispatcher transcript auto-send for real Codex panes must route through `/voice-input ...`
  so Codex can mark the resulting history row as `[voice]` instead of losing the transcript origin.

Why this is integral to our fork:

- This is the operating mode for continuous spoken Codex sessions in Dispatch.
- It depends on both repos staying aligned: Codex must expose/persist the mode and push assistant
  speech, while Dispatch must run the real-time browser playback and transcript auto-send path.
- It is easy to lose in an upstream merge because the CLI can still compile while reverting to the
  old final-only notify behavior or dropping runtime overrides/state propagation.

Primary touchpoints:

- `codex-rs/core/src/voice_mode.rs`
- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/codex_thread.rs`
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/tui/src/slash_command.rs`
- `codex-rs/tui/src/cli.rs`
- `codex-rs/exec/src/cli.rs`
- `codex-rs/exec/src/lib.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server/src/codex_message_processor.rs`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/web/app.py`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/templates/tmux.html`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/static/audio_push.js`
- `scripts/codex-dev-wrapper.sh`
- `scripts/voice/codex-dev-voice-web.sh`
- `pitchai_dispatch/playwright/tests/tmux_voice_audio_push.spec.cjs`
- `pitchai_dispatch/playwright/tests/tmux_voice_auto_send.spec.cjs`
- `pitchai_dispatch/playwright/tests/tmux_voice_real_codex.spec.cjs`
- `pitchai_dispatch/playwright/tests/tmux_voice_codex_dev_local_container.spec.cjs`
- `scripts/smoke/run_voice_mode_real_smoke.sh`
- `scripts/smoke/run_codex_dev_voice_local_container_smoke.sh`

### 10.1) WhatsApp bridge autodiscovery / runtime stability

Required outcomes:

- The dispatcher must prefer a WhatsApp bridge directory that includes installed `node_modules`
  over a source checkout that only contains `index.cjs`.
- Containerized dispatcher installs must keep working even when the repo is bind-mounted into the
  container for editable/live debugging.
- Live WhatsApp status must continue reporting the real connected account once the bridge is ready.

Why this is integral to our fork:

- We operate the live dispatcher with source overlays often enough that a naive “first index.cjs
  wins” lookup breaks the bridge and silently drops the operator’s connected WhatsApp account.
- This is easy to regress in future merges because the UI still loads while the bridge repeatedly
  crashes on missing Node deps.

Primary touchpoints:

- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/web/app.py`
- `pitchai_dispatch/dispatcher/whatsapp_bridge/index.cjs`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/web/routers/whatsapp.py`

### 11) Realtime voice / audio forwarding

Required outcomes:

- Realtime voice mode must remain startable from the TUI.
- Mic/input audio must continue to flow from the TUI into the realtime backend.
- Output audio deltas must continue to flow through app-server notifications to frontend clients.
- The fork must preserve the end-to-end path from core realtime events to app-server
  `ThreadRealtimeOutputAudioDelta`.

Why this matters:

- PitchAI frontend and dispatch-adjacent workflows depend on realtime audio output continuing to
  reach the UI layer.

Primary touchpoints:

- `codex-rs/tui/src/chatwidget/realtime.rs`
- `codex-rs/core/src/realtime_conversation.rs`
- `codex-rs/app-server/src/bespoke_event_handling.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`

### 12) Protocol, session snapshot, and app-server propagation

Required outcomes:

- Thread snapshot/restore must preserve all live fork state needed to resume correctly:
  - non-stop runtime override
  - non-stop timeout
  - voice mode state
  - pending queued `/voice` override
  - pending queued `/non-stop` override
  - pending delayed non-stop messages
  - pending `/deep` request count
  - completion-gate settings
  - queued model change
- App-server thread start/resume/fork responses must keep exposing new fork state where relevant.
- New fork-critical protocol events must remain wired through JSON schema + generated TS:
  - `TurnCompleteDeferredByNonStop`
  - `NonStopModeUpdated`
  - completion-gate events

Why this matters:

- If session snapshot/restore drops this state, the fork behaves correctly only in the first live
  thread and regresses on reconnect, thread switch, or frontend resume.

Primary touchpoints:

- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/core/src/codex_thread.rs`
- `codex-rs/core/src/state/session.rs`
- `codex-rs/tui/src/chatwidget.rs`
- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server/src/codex_message_processor.rs`

### 13) Resume/session ergonomics

Required outcomes:

- Resume keeps or refreshes working context such as cwd and environment view where the fork expects
  it.
- Resume warnings about model mismatches/reroutes remain visible.
- Session selectors and resume branch behavior remain intact.

Representative local commits:

- `ddbc65e57`
- `b6901f990`
- `cd05ee828`
- `6f627a4ce`

### 14) Dispatcher / automation integrations

Keep PitchAI operational integrations used by agent jobs and surrounding tooling.

Scope includes:

- dispatcher polling/status/run orchestration
- Telegram job and stop-hook flows
- SharePoint inbox automation and move robustness
- mailbox tagger tools and related job runners

Representative local commits:

- `ce9da3c4f`
- `62a512b50`
- `e7f21ab18`
- `8238ff6fe`
- `b1436b559`
- `d40ea5e52`
- `58b80d505`
- `f0efdd306`
- `5e1509923`
- `19bb1c121`
- `3b2478f8a`

## Verification checklist after every upstream merge

Run at minimum:

```bash
# Verify the fork delta still exists.
git log --no-merges --oneline upstream/main..main

cd codex-rs

# Event / websocket parity / continuation guardrails.
cargo test -p codex-api websocket_event_queue_synthesizes_missing_output_item_done -- --nocapture
cargo test -p codex-core websocket_v2_synthesizes_tool_done_when_missing -- --nocapture
cargo test -p codex-core --test all suite::unified_exec::assistant_only_response_after_background_wait_triggers_follow_up -- --exact --nocapture
cargo test -p codex-core --test all suite::unified_exec::commentary_message_after_tool_output_triggers_follow_up -- --exact --nocapture
cargo test -p codex-core --test all suite::unified_exec::short_final_answer_after_tool_output_still_stops -- --exact --nocapture

# Persistent / non-stop / deep controls.
cargo test -p codex-core --test all suite::unified_exec::persistent_mode_keeps_turn_alive_while_terminal_is_running -- --exact --nocapture
cargo test -p codex-core --test all suite::unified_exec::non_stop_mode_forces_follow_up_after_final_answer -- --exact --nocapture
cargo test -p codex-core --test all suite::unified_exec::timed_non_stop_mode_allows_stop_after_timeout -- --exact --nocapture
cargo test -p codex-core --test all suite::unified_exec::deep_budget_forces_four_extra_candidate_stop_rounds -- --exact --nocapture

# TUI queueing / runtime override behavior.
cargo test -p codex-tui deep -- --nocapture
cargo test -p codex-tui slash_non_stop -- --nocapture
cargo test -p codex-tui queued_model_selection_is_applied_before_next_queued_message -- --nocapture
cargo test -p codex-tui slash_fix_submits_repair_conversation_op -- --nocapture

# Hooks.
cargo test -p codex-core --test all hooks -- --nocapture

# Strict-dir and completion gate.
cargo test -p codex-exec add_dir
cargo test -p codex-core completion_gate -- --nocapture

# App-server / protocol surfaces.
cargo test -p codex-protocol
cargo test -p codex-app-server-protocol
```

Recommended real smokes after a large merge:

```bash
# Strict-dir smoke.
codex exec --strict-dir /tmp --strict-dir "$PWD" "print pwd and list files"

# Persistent / non-stop smoke.
codex exec --persistent "start a background terminal and keep watching it"
codex exec --non-stop-for 2m "keep going until timeout, then stop normally"

# Completion-gate smoke.
scripts/smoke/run_completion_gate_exec_smoke.sh
```

## Merge policy notes

- If upstream changes stream/event plumbing, re-verify websocket and SSE parity immediately.
- If upstream changes TUI submit/queue logic, explicitly re-check:
  - follow-up queueing
  - queued edit preservation
  - non-stop submit picker
  - `/deep`
  - queued `/non-stop`
  - queued model changes
- If upstream changes auth selection, explicitly re-check broker/shared-auth precedence before
  trusting automation again.
- If upstream changes realtime/audio plumbing, explicitly re-check the app-server audio delta path.
- Keep this guide updated whenever the fork gains or removes behavior.
