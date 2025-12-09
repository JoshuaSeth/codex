# Pending Tool Wait Improvements

## Background

Custom tools can request `hibernate_after_call = true`, which currently makes Codex exit immediately after logging a placeholder tool result. The operator (or a webhook) later edits the rollout via `codex-dev exec resume --replace-last-toolresult …` and sends a follow-up turn. This halts the CLI, forces humans to reopen the conversation, and prevents any live visual feedback while the automation waits for an email/webhook.

We want Plan #1: keep the original `codex-dev exec` session alive, show a "waiting for reply" indicator, and automatically continue once the real tool output arrives—no extra prompt.

## Explored Approaches

1. **In-session wait (keep turn alive)** – When a tool returns `hibernate_after_call`, the session records a pending entry, emits a background event, and `process_items` pauses until an external signal delivers the final output. Once the webhook injects the output (via a new op), the same turn resumes seamlessly. *Pros:* preserves transcript ordering, CLI never exits, minimal surface area. *Cons:* needs new IPC so background automation can deliver the output to the live session; must handle cancellation/ctrl-C.

2. **External resume helper (status quo++)** – Continue exiting the CLI, but auto-trigger `codex-dev exec resume …` under the hood (a supervisor process respawns the CLI inside the same terminal when the webhook fires). *Pros:* leverages existing resume path. *Cons:* user loses scrollback; spinner would require re-attaching to terminal multiplexer; brittle if terminal closed.

3. **Dual-channel logging** – Session keeps running but immediately writes the pending placeholder to the rollout and tells the CLI to display a spinner. Another Codex process tails the rollout; when webhook writes the final result entry, it injects it into the running session by replaying diff events. *Pros:* uses existing rollout files, no new op needed. *Cons:* tricky synchronization between two codex instances writing to the same session; risk of race conditions.

4. **Dedicated pending-task service** – When a tool requests `hibernate_after_call`, Codex persists a record in a SQLite/queue service. The CLI polls this queue; when the webhook posts a completion payload into the queue, the CLI rehydrates the turn by requesting an updated transcript from the service. *Pros:* future multi-machine potential. *Cons:* introduces new service dependency and persistence for what should remain a local workflow.

Given the requirements (single-machine CLI UX, deterministic local logs), **Approach 1** is the best fit.

## Implementation Plan (Approach 1)

1. **Session state & config hooks**
   - Extend `SessionState` with a `PendingToolManager` that tracks `call_id`, `tool_name`, `turn_id`, cwd, note, and a `tokio::sync::watch` channel used to deliver the final output.
   - Add helper APIs on `Session`: `register_pending_tool`, `await_pending_tool`, `resolve_pending_tool`, `cancel_pending_tool`.
   - Emit a new `EventMsg::PendingToolState` (variants `Waiting` and `Resolved`) so CLIs can show/hide the spinner. Also reuse existing `BackgroundEvent` for textual guidance.

2. **Tool execution plumbing**
   - In `ToolRegistry::dispatch`, when the handler returns `ToolOutput::Pending { shutdown: true, .. }`, call `session.register_pending_tool(...)`. Instead of firing `shutdown_session`, capture the call metadata and return the same `ToolOutput` so we can keep telemetry intact.
   - Update `process_items` to call `session.await_pending_tool(call_id, payload).await` for any `ResponseInputItem::FunctionCallOutput` whose `call_id` is flagged as pending. The helper should:
     - Immediately emit `PendingToolState::Waiting` (only once).
     - Suspend until `resolve_pending_tool` supplies a `FunctionCallOutputPayload` or cancellation occurs.
     - Replace the placeholder payload with the final output, mark the entry resolved, and emit `PendingToolState::Resolved`.
   - Ensure cancellation/ctrl-C rejects the pending future and surfaces a polite error to the model.

3. **New op for delivering results**
   - Introduce `Op::DeliverPendingToolResult { conversation_id?, call_id, output_text, success_flag, content_items? }` handled inside `codex.rs`. The handler should locate the matching `PendingToolEntry` and call `resolve_pending_tool` with the supplied payload.
   - Expose a CLI surface: `codex-dev exec resume --deliver-pending <call_id> --payload-file …` OR extend the existing `--replace-last-toolresult` helper to detect live sessions by conversation id and push results through the new op when possible (falling back to rollout editing if the session already ended). For webhooks, add a tiny helper script that calls the new CLI subcommand instead of spawning a fresh `codex-dev exec resume`.

4. **CLI UX (spinner & persistence)**
   - Extend `EventProcessorWithHumanOutput` to handle `EventMsg::PendingToolState` by printing a sticky status line and optionally rendering a spinner (e.g., `/-\|`). Keep a timer tick via `tokio::spawn` inside exec main so the spinner updates until resolved.
   - Ensure `CodexStatus` remains `Running` during waiting; no `Op::Shutdown` is sent until the actual `TaskComplete` arrives.
   - Update JSONL output mode to emit matching events so scripts can detect waiting/resolved transitions.

5. **Fallback & resume compatibility**
   - If the CLI truly dies (user closes terminal), pending entries still exist in memory; add persistence by writing pending metadata into the rollout (e.g., an auxiliary JSONL entry). On restart, `codex exec resume --replace-last-toolresult` should continue to work exactly as before by editing the rollout.
   - Document behavior in `README_extensive.md` + `docs/config.md`, clarifying that `hibernate_after_call = true` now denotes "await external completion" instead of immediate shutdown.

6. **Testing & validation**
   - Unit-test the new `PendingToolManager` (register/resolve/cancel).
   - Integration test: run a fake tool with `hibernate_after_call`, simulate webhook by calling the new op after a short delay, assert that the CLI stays connected and the model receives the final output.
   - Smoke-test existing resume helper to ensure fallback path works when no live session is found.

