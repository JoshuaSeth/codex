# Spec — Continuous Voice Mode for Codex (`--voice`)

Project: **PitchAI — Codex fork**
Repo: `codex` / `codex-rs` plus `pitchai_dispatch` integration
Primary PM task: `574e19a0-c94d-40c4-93ff-b9d1e57e7a50` (**Write continuous voice mode architecture spec**)

## 0) Request + selected approach

### 0.1 Goal/request (verbatim-style)

```text
We want a new special mode --voice.

When this mode is active, not only do we use the speech engine to generate speech,
like we do already with the --use-speech flag, we want that to happen for every
system/assistant message, not only for the final message.

If a previous message is still playing and we have a new one to play, we fade out
the previous message and say the new message.

This mode is also --non-stop meaning it does not stop, but if it hears a user text
that consists of at least 10 words, it immediately inserts that in the current
conversation. It does not wait for it to finish or enqueue it for later. It adds it
to the running conversation instantly.

For this we want to study the dispatch app because we want to use AssemblyAI live
transcription. Speed is key. Audio should be streamed in chunks so we do not lose
time. Any new assistant message cancels any previous queue of chunks. For user
messages we use continuous AssemblyAI transcription, and when AssemblyAI detects one
coherent message it is turned into a user message and inserted into the running
non-stop conversation.

This should be continuous output and continuous input.
```

### 0.2 Selected architecture

```text
Implement --voice as a browser/Dispatch-driven continuous voice layer on top of a
normal Codex thread, not as an extension of the legacy notify hook and not
as a reuse of the current OpenAI realtime voice transport.

- Codex remains the normal conversation engine.
- --voice enables a dedicated voice session mode; `--non-stop` remains optional.
- Codex emits every assistant-visible message/update into a thread-scoped voice
  output stream.
- Dispatch owns playback, chunked TTS, interruption, fade-out, AssemblyAI live STT,
  and browser-level device/runtime concerns.
- Continuous user speech is transcribed by AssemblyAI in Dispatch.
- When Dispatch decides a coherent utterance is ready and it contains at least
  10 words, it is inserted immediately into the active Codex thread as live voice
  input instead of queueing behind turn completion.
```

## 1) Why this feature exists

The current speech behavior is structurally insufficient:

- `/usr/local/bin/codex-dev` exposes `--use-speech`, but that only installs a legacy
  `notify` hook.
- `notify` only fires after a completed turn.
- That means today we can speak the final message, but we cannot do all of the
  following at once:
  - speak intermediary assistant updates,
  - interrupt/fade older playback when newer assistant text arrives,
  - keep a conversation running continuously,
  - inject live user speech into a still-running turn.

The current Codex realtime mode is also not the right product base:

- it is a separate transport path,
- it is audio-oriented and not aligned with the normal Codex turn engine,
- it carries different auth and session assumptions,
- and it is not the same thing as “continuous browser voice over a normal Codex
  thread”.

Dispatch already contains the stronger building blocks:

- streaming/interruptible browser audio push,
- fade-out and latest-wins playback,
- AssemblyAI realtime token minting,
- continuous STT orchestration,
- browser audio leadership and unlock handling,
- and real browser test coverage.

The right design is therefore to make voice mode a first-class operating mode for a
normal Codex thread, while reusing Dispatch as the I/O/control surface.

## 2) Existing systems and constraints

### 2.1 Existing speech path

Current wrapper path:

- `/usr/local/bin/codex-dev`
- `pitchai_dispatch/dispatcher/tools/codex_notify_voice_push.py`
- `codex-rs/core/src/config/mod.rs`
- `codex-rs/hooks/src/user_notification.rs`

Properties:

- edge-triggered only after agent turn completion,
- no per-message streaming,
- no per-fragment interruption,
- no live inbound speech insertion.

Conclusion:

- keep this path for legacy “speak after final answer” compatibility,
- do not build `--voice` on top of it.

### 2.2 Existing Codex realtime voice path

Relevant files:

- `codex-rs/tui/src/chatwidget/realtime.rs`
- `codex-rs/core/src/realtime_conversation.rs`
- `codex-rs/core/src/codex.rs`
- `codex-rs/app-server/tests/suite/v2/realtime_conversation.rs`

Properties:

- separate realtime websocket transport,
- separate session start/close lifecycle,
- not the same as normal threaded Codex task execution,
- currently framed as audio-only in the TUI path.

Conclusion:

- do not use this as the implementation substrate for `--voice`,
- but borrow any useful protocol/event ideas from it.

### 2.3 Existing Dispatch voice system

Relevant files:

- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/static/audio_push.js`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/web/app.py`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/templates/tmux.html`
- `pitchai_dispatch/playwright/tests/tmux_voice_audio_push.spec.cjs`
- `pitchai_dispatch/playwright/tests/tmux_voice_live_smoke.spec.cjs`
- `pitchai_dispatch/playwright/tests/voice_mode.spec.cjs`
- `pitchai_dispatch/playwright/tests/voice_benchmarks.spec.cjs`

Capabilities already present:

- latest-wins audio playback,
- fade-out preemption,
- streaming TTS generation,
- browser websocket audio delivery,
- AssemblyAI realtime token generation,
- continuous STT orchestration,
- pause speech while user is speaking,
- real browser test surface.

Conclusion:

- Dispatch is already the correct place for playback and transcription concerns.

### 2.4 Architectural constraints

- The core conversation engine must remain Codex normal-turn execution.
- `--voice` must not be a wrapper hack hidden in `codex-dev`.
- No business-logic fallbacks: if the core voice session contract is broken, fail
  loudly instead of silently degrading inside core logic.
- UI and transport edges may retry/reconnect, but core state must stay explicit.
- The system must support real low-latency operation, which means chunked output and
  immediate preemption. It does not mean pretending networked STT/TTS can literally
  be zero-latency.

## 3) Architecture decision

### 3.1 Decision

The chosen architecture is:

1. add `--voice` as a first-class Codex session mode;
2. implement it on top of the normal non-stop Codex thread engine;
3. add a dedicated thread-scoped voice event stream between Codex/app-server and
   Dispatch;
4. let Dispatch own browser playback, streaming TTS, cancellation, fade-out, and
   AssemblyAI STT;
5. let Dispatch inject finalized voice utterances immediately into the running
   thread as live voice input.

### 3.2 Why this is the best architecture

This is the best balance of correctness, speed, and long-term maintainability:

- it preserves one normal conversation engine,
- it reuses the strongest existing browser voice code,
- it avoids abusing post-turn hooks,
- it avoids coupling product voice mode to the separate realtime transport,
- it makes the UI behavior visible and testable through the real app-server and
  browser path.

### 3.3 Alternatives rejected

#### A. Extend the legacy `notify` speech hook

Rejected because it is post-turn by design and is the wrong layer for continuous
voice I/O.

#### B. Rebase the feature on current Codex realtime transport

Rejected because it is a different product mode with different auth/session
assumptions and would fight the normal Codex threaded execution model.

#### C. Make it TUI/local-device only

Rejected as the primary architecture because Dispatch already contains the browser
voice stack and the desired experience is browser-driven, not terminal-device-driven.

## 4) User-facing behavior

### 4.1 CLI surface

Add:

- `--voice`

Behavior:

- enables continuous voice mode for the session,
- may be combined with `--non-stop`, but does not require it,
- requires a compatible app-server + Dispatch voice frontend to get the full
  browser voice experience.

Optional future knobs that are in scope for this architecture, even if implemented in
later slices:

- `--voice-min-words <N>` default `10`
- `--voice-tts-voice <VOICE>`
- `--voice-tts-model <MODEL>`
- `--voice-preempt-fade-ms <N>`
- `--voice-inbound-language <LANG>`

### 4.2 Slash commands

Required:

- `/voice on`
- `/voice off`
- `/voice status`

Optional but recommended:

- `/voice min-words <N>`
- `/voice voice <VOICE>`
- `/voice pause`
- `/voice resume`

Rules:

- `/voice on` while a turn is running queues the mode change and applies it before
  the next queued/new turn, following the same runtime-override model we already use
  for `/non-stop`.
- `/voice off` must stop further automatic voice playback and stop accepting
  continuous voice injection for subsequent events, but it must not corrupt the
  underlying thread state.

### 4.3 Spoken content rules

Speak:

- assistant commentary intended for the operator,
- assistant final answers,
- assistant progress updates that are surfaced in the thread UI.

Do not speak:

- hidden system prompts,
- raw tool-call JSON,
- internal protocol/debug events,
- opaque machine-only warnings unless explicitly promoted to operator-visible status.

### 4.4 Inbound speech rules

- Dispatch listens continuously using AssemblyAI realtime transcription.
- Partial transcripts are visible to the browser UI but are not injected into Codex.
- When AssemblyAI produces a coherent finalized utterance and the utterance has at
  least `10` words, Dispatch turns that into a live user input event for the active
  thread immediately.
- That input is not treated as a normal queued follow-up. It is treated as immediate
  live voice input, equivalent to an intentional steer into the running conversation.
- The voice utterance must appear in thread history immediately so operators can see
  exactly what got injected.

## 5) Stop/continue semantics

### 5.1 Base rule

`--voice` does not by itself change stop semantics.

That means:

- if `--non-stop` is also enabled, a normal assistant “final” message does not stop the session;
- if `--non-stop` is not enabled, Codex may stop normally after a final answer;
- after such a normal stop, the next qualifying live voice transcript starts a fresh new turn.

### 5.2 When stopping is allowed

While `--voice` is active, stopping follows the underlying turn policy:

1. with `--non-stop`, stopping is allowed only on explicit abort/shutdown/fatal-error or another
   explicit stop policy;
2. without `--non-stop`, normal final answers may stop the current turn;
3. after a normal stop, live voice input remains armed and may start the next turn immediately.

Stopping is **not** allowed merely because, under explicit `--non-stop`:

- the assistant says it is done,
- the assistant emits a final answer,
- no tool call appears in the latest response,
- the last spoken message sounds terminal.

### 5.3 What “immediate insert” means

Immediate insert does **not** mean mutating a model response mid-token on the wire.
It means:

- Dispatch emits a live voice input event as soon as the utterance is finalized;
- Codex core records that input immediately in thread state;
- the current running turn is interrupted/steered using the same kind of intentional
  live-input mechanism we already use for running-turn interaction;
- the session does not wait for normal completion/queue-drain first.

## 6) Detailed architecture

### 6.1 Control plane

#### Codex CLI / session layer

Touchpoints:

- `codex-rs/exec/src/cli.rs`
- `codex-rs/tui/src/cli.rs`
- `codex-rs/cli/src/main.rs`
- `codex-rs/core/src/config/mod.rs`

Responsibilities:

- parse and store `--voice`,
- persist it in session/thread state,
- surface it in status/details views,
- wire runtime slash overrides.

#### Core session state

Touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/core/src/codex_thread.rs`
- `codex-rs/protocol/src/protocol.rs`

Responsibilities:

- represent whether voice mode is active,
- represent whether a thread currently has a live voice frontend attached,
- accept live voice input events,
- emit assistant-visible text into a voice-output stream,
- coordinate interruption/steering semantics for injected voice utterances.

### 6.2 Output plane: assistant text to spoken audio

#### Core emission

Core must emit voice output events whenever an operator-visible assistant message is
produced, not only at turn completion.

Recommended event shape:

- thread id
- conversation/session id
- turn id
- message id
- sequence number
- content text chunk
- chunk type (`start`, `delta`, `final`, `replace`, `cancel`)
- assistant phase if available (`commentary`, `final_answer`)
- timestamp

Important rule:

- a newer assistant message starts a new output generation and cancels the previous
  generation for playback purposes.

#### App-server forwarding

Touchpoints:

- `codex-rs/app-server-protocol/src/protocol/v2.rs`
- `codex-rs/app-server/src/codex_message_processor.rs`
- `codex-rs/app-server/src/bespoke_event_handling.rs`

Responsibilities:

- forward thread voice events to subscribed clients,
- preserve ordering,
- expose explicit `cancel` / `replace` semantics,
- avoid replaying stale audio chunks after a newer assistant message is active.

#### Dispatch backend

Touchpoints:

- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/web/app.py`

Responsibilities:

- receive voice output events,
- call the speech API in chunked mode,
- stream resulting WAV/audio chunks to the browser,
- stop generating or publishing stale chunks once a newer assistant message wins.

Important rule:

- if an assistant message is superseded, TTS generation for the old message should
  be canceled best-effort at the backend and must be ignored at the frontend even if
  some stale chunks arrive.

#### Dispatch frontend

Touchpoints:

- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/static/audio_push.js`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/templates/tmux.html`

Responsibilities:

- play arriving audio chunks,
- fade out and preempt current playback when newer assistant output arrives,
- ensure only one active browser leader/tab speaks,
- show current speaking status and interruption state in the UI,
- clear stale queued chunks from superseded assistant messages.

### 6.3 Input plane: microphone to live Codex input

#### Browser capture and STT

Touchpoints:

- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/templates/tmux.html`
- `pitchai_dispatch/dispatcher/src/pitchai_dispatcher/web/app.py`

Responsibilities:

- capture mic input continuously,
- acquire AssemblyAI realtime token,
- stream mic frames to AssemblyAI,
- show partial/final transcripts in the UI,
- pause or duck outgoing speech while the operator is speaking.

#### Utterance finalization

Dispatch decides when a user utterance is ready for Codex injection.

Required rule set:

- require a finalized/coherent transcript unit from AssemblyAI,
- require `>= 10` words by default,
- ignore very short chatter/noise by default,
- expose the final injected text in the UI before/while it is sent,
- assign a monotonic utterance id for observability.

#### Codex live input injection

Touchpoints:

- `codex-rs/core/src/codex.rs`
- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/tui/src/chatwidget.rs` for local UI parity/state

Responsibilities:

- accept the finalized voice utterance as immediate live input,
- record it in thread history,
- steer/interrupt current execution rather than queue behind completion,
- preserve a clear audit trail that the source was voice.

Recommended new protocol concept:

- `VoiceUserInputReceived`
- `VoiceUserInputInjected`
- `VoiceUserInputRejected`

### 6.4 Cancellation and preemption model

Output preemption:

- newest assistant-visible message wins,
- previous playback fades out,
- stale backend chunks are ignored,
- stale frontend queue is dropped.

Input preemption:

- new finalized user utterance may interrupt/steer the current run,
- voice input is not blocked on the current response ending,
- voice input should not be duplicated if AssemblyAI reconnects or replays a final.

### 6.5 Observability

Every voice session must produce inspectable logs and UI state for:

- voice mode enabled/disabled,
- active browser subscriber count,
- last assistant message spoken,
- current playback generation id,
- current/last AssemblyAI utterance id,
- last injected transcript text,
- last voice-input rejection reason,
- last TTS latency metrics,
- last STT-finalization latency metrics.

## 7) Failure handling

### 7.1 Allowed retries and reconnection

Allowed at the edges:

- websocket reconnect between browser and Dispatch,
- AssemblyAI realtime reconnect,
- speech API request retry for transient I/O errors,
- app-server client resubscription.

### 7.2 Not allowed as silent core fallbacks

Not allowed in core logic:

- silently downgrading `--voice` to legacy `notify`,
- silently turning immediate voice input into normal queueing,
- silently dropping failed voice inputs without UI/operator visibility,
- silently pretending speech was spoken when playback pipeline failed.

### 7.3 Failure visibility

Required operator-visible messages:

- no browser voice client attached,
- AssemblyAI token acquisition failed,
- speech API failed,
- live voice input rejected because utterance too short,
- voice input rejected because session is not voice-enabled,
- voice event stream disconnected and reconnected.

## 8) Security and auth

- Reuse current managed Codex auth for the underlying Codex thread.
- Reuse Dispatch’s existing browser auth/session model.
- AssemblyAI tokens must remain short-lived and minted by Dispatch backend.
- No raw long-lived AssemblyAI secret may be exposed to the browser.
- Voice session routing must stay thread-scoped and user/session-scoped.

## 9) Performance targets

“Millisecond latency” is not a literal engineering requirement for a networked STT +
LLM + TTS system. The correct target is aggressive streaming and preemption.

Initial targets:

- assistant text availability to first audio chunk queued in browser:
  - p50 `< 900ms`
  - p95 `< 2000ms`
- newer assistant message to previous playback preempt start:
  - p50 `< 150ms`
  - p95 `< 400ms`
- AssemblyAI finalized utterance to Codex voice input injection:
  - p50 `< 500ms`
  - p95 `< 1200ms`
- finalized utterance to visible thread insertion in UI:
  - p50 `< 300ms`
  - p95 `< 800ms`

These numbers must be measured in the real browser/Dispatch/Codex path, not guessed.

## 10) Implementation plan

### Phase 1 — Core mode and protocol plumbing

- add `--voice` CLI/config/session support,
- add runtime `/voice` overrides,
- add protocol/session state for voice mode,
- add thread-scoped voice output events,
- add voice input injection op/event types,
- surface basic status in TUI/app-server UI.

### Phase 2 — Dispatch output path

- bind thread voice events to Dispatch,
- chunk TTS from assistant text,
- enforce latest-wins cancellation,
- reuse `audio_push.js` fade-out/preemption behavior for thread voice mode,
- show current speaking status in browser UI.

### Phase 3 — Dispatch input path

- continuous AssemblyAI STT session,
- coherent-finalized utterance detection,
- 10-word threshold,
- immediate injection into Codex thread,
- visible transcript preview + injected transcript history row.

### Phase 4 — Robustness and observability

- edge reconnect handling,
- dedupe repeated finals,
- latency metrics,
- operator error surfaces,
- long-run smoke tests and interruption tests.

## 11) Definition of done

The feature is done only when all of the following are true:

1. `--voice` exists as a first-class mode and survives session/thread restore.
2. Voice mode speaks every operator-visible assistant update, not just final answers.
3. A newer assistant message cancels/fades the older playback and becomes the only
   active spoken output.
4. Voice mode behaves as non-stop while active; ordinary final answers do not end the
   session.
5. A finalized AssemblyAI utterance with at least 10 words is inserted immediately
   into the active conversation without waiting for ordinary turn completion.
6. The injected transcript is visible in thread history and clearly marked as voice.
7. The full system works through the actual Codex app-server + Dispatch + browser
   stack using real provider calls.
8. Real tests demonstrate both passing behavior and at least one intentional failure
   path for each major validation class.
9. Operator-facing status/errors are visible and precise.
10. No part of the core behavior silently falls back to legacy notify-only speech or
    ordinary queued user input.

## 12) Required validation and testing

All validation here must use real running services and real provider calls. No mocks,
no stubs, no patching out the external path, and no synthetic-only acceptance.

### Test 1 — Direct Codex file/path smoke

Goal:

- run the actual Codex entry path with `--voice` against a real conversation thread
  and real app-server/Dispatch services.

Method:

- launch the real app-server,
- launch the real Dispatch server,
- start a real `codex`/`codex-dev` voice session in a real repo,
- attach a real browser client,
- confirm that a commentary assistant message is emitted and spoken before the final
  answer.

Must fail if:

- only final messages speak,
- no voice events are emitted,
- session silently downgrades to legacy `notify`.

Artifacts:

- terminal logs,
- app-server logs,
- dispatch logs,
- browser console logs,
- thread transcript excerpt,
- voice event dump.

### Test 2 — Real Playwright end-to-end browser test

Goal:

- prove the real browser UI shows and behaves correctly with the actual running
  servers.

Method:

- start real app-server and real Dispatch,
- run a Playwright test against the real browser UI,
- trigger at least two assistant updates in one running thread,
- assert the first spoken segment is interrupted by the second,
- assert the UI shows the current speaking message and no stale queue remains.

Must fail if:

- the older message keeps speaking after a newer one arrived,
- the UI does not reflect the current message,
- stale queued chunks still play.

Artifacts:

- Playwright video,
- screenshots,
- browser console/network logs,
- captured websocket/event trace.

### Test 3 — Real AssemblyAI live-input smoke with generated WAV

Goal:

- prove live inbound voice transcription really injects into the active conversation.

Method:

- generate real WAV input using the speech API,
- feed that WAV into the real browser mic/input path or equivalent real audio source,
- use one clip with fewer than 10 words and one with at least 10 words,
- verify the short utterance is not injected and the long utterance is injected
  immediately into the running conversation.

Must fail if:

- both clips inject,
- neither injects,
- long utterance waits for turn completion instead of steering immediately.

Artifacts:

- input WAV files,
- transcript logs,
- AssemblyAI session logs,
- thread history with injected voice message,
- latency measurements.

### Test 4 — Real interruption/preemption latency benchmark

Goal:

- quantify whether latest-wins interruption is actually fast enough.

Method:

- run a real thread that emits one longer assistant message,
- cause a second assistant update quickly after it,
- measure:
  - time from second message visibility to previous audio fade start,
  - time until new audio begins.

Must fail if:

- previous audio continues past the allowed threshold,
- new audio start exceeds the configured latency budget.

Artifacts:

- benchmark output JSON,
- timestamps from server and browser,
- optional waveform/clip timing traces.

### Test 5 — Real long-running non-stop voice session smoke

Goal:

- prove the system survives a realistic continuous session rather than a single happy
  path.

Method:

- run a real voice session for a defined window such as 10–15 minutes,
- exchange multiple spoken user utterances and multiple assistant updates,
- include at least one reconnect/transient audio interruption event,
- verify session state, voice state, and transcript continuity remain correct.

Must fail if:

- voice mode drops back to ordinary stop behavior,
- repeated voice inputs duplicate,
- reconnect causes stale audio replay or transcript duplication.

Artifacts:

- full server logs,
- browser logs,
- thread transcript,
- session timeline,
- latency summary.

### Test 6 — Real negative-path browser E2E

Goal:

- prove the UI and backend fail loudly and correctly when a real dependency is not
  available.

Method:

- run the real browser/server path with one intentionally broken real edge, for
  example revoked AssemblyAI token minting or disabled TTS credential,
- verify the UI surfaces the exact failure,
- verify the session does not silently pretend audio or live input still works.

Must fail if:

- UI stays green while the feature is non-functional,
- voice input is silently dropped,
- speech output silently degrades without operator visibility.

Artifacts:

- UI screenshot/video,
- exact error messages,
- server logs,
- browser logs.

## 13) Test result reporting

Each validation run must report:

- git commit SHA,
- app-server version/commit,
- Dispatch version/commit,
- provider environment used,
- exact command lines used,
- start/end timestamps,
- pass/fail outcome,
- measured latencies,
- links/paths to logs, screenshots, videos, and audio artifacts,
- explicit statement of which definition-of-done items were covered.

Recommended reporting format:

- one markdown report per run under a dedicated `artifacts/voice-mode-validation/`
  directory in the appropriate repo or CI artifact bundle,
- plus a concise summary appended to the PM task changelog.

## 14) Acceptance criteria before implementation can be called complete

Implementation cannot be called complete until:

- all definition-of-done items are satisfied,
- all 6 validation classes have been executed against real running services,
- the required Playwright browser E2E has passed on the real UI,
- the generated-WAV live-input smoke has passed,
- at least one negative-path test has failed for the correct reason,
- measured latency is within agreed initial targets or there is an explicit accepted
  exception documented in the PM task and spec follow-up.

## 15) Future extensions

Not required for initial delivery, but this architecture should leave room for:

- bounded voice sessions (`--voice-for <duration>`),
- completion-gate integration so voice mode can decide when it is truly safe to stop,
- multi-client voice observers,
- richer utterance segmentation policy than simple minimum-word threshold,
- per-thread voice preferences,
- browser-side transcript correction UI before injection.

## 16) Summary

The correct implementation is:

- voice as a first-class Codex session mode,
- normal Codex thread as the conversation engine,
- Dispatch/browser as the continuous voice I/O runtime,
- AssemblyAI for live transcription,
- chunked latest-wins TTS with cancellation,
- immediate finalized voice utterance injection into the running thread,
- no silent fallback to legacy notify-only behavior.
