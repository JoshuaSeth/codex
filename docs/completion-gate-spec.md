# Spec — LLM Completion Gate for Final Assistant Messages

Project: **PitchAI — Codex fork**
Repo: `codex` / `codex-rs`
Primary PM task: `cae381bb-c37f-432e-9ab0-6c1a4e8f0e1f` (**Design LLM completion gate for final assistant messages**)

## 0) Request + approach

### 0.1 Goal/request (verbatim-style)

```text
We want one version where on every final assistant message we run some GPT call.
That GPT call gets input: the latest assistant messages and user messages, clearly XMLed,
up to 3 user messages back or up until the last judging moment back.
It should also get the original user request / usual message and the latest Codex response.

The judge model evaluates whether the stop criteria are actually satisfied.
If the criteria are satisfied, Codex is allowed to stop.
If the criteria are not satisfied, Codex is not allowed to stop.
The judge returns structured data via schema.json.
If it is not fine, the returned follow-up prompt is inserted as a user message and Codex continues.

We already have `--persistent` and `--non-stop`.
This is a different mode: stop is allowed only if the external judge says it is allowed.
This must be enableable from the CLI and the criteria text must also be settable via a slash command.

The first step is a simple Rust equivalent of `lib_llm` for making a cheap structured OpenAI call.
```

### 0.2 Proposed approach (selected architecture)

```text
- Add an opt-in completion gate at the exact point where Codex is otherwise about to emit TurnComplete.
- Do not trigger the judge on arbitrary assistant stream chunks; trigger it only on a real candidate stop boundary.
- Implement a small Rust LLM client modeled after Python `lib_llm`, with strict JSON-schema output support.
- Build the judge input from live session history in core, not from reparsing rollout files.
- Track a per-session "last judged boundary" so the judge sees only new conversational context plus the original request.
- If the judge says stop is not allowed, inject the returned follow-up as a synthetic user continuation and keep going.
- If the judge says stop is allowed, let normal completion continue.
- If the gate is enabled and the judge cannot produce a valid schema result, fail closed: do not silently allow stop.
```

## 1) Why this feature is needed

Today Codex decides whether to continue or stop in `codex-rs/core/src/codex.rs` at the end of a model response, after `response.completed` arrives. Existing continuation guards already exist for:

- queued user input,
- background-terminal follow-up,
- short intermediary assistant status messages after tool output,
- `--persistent`,
- `--non-stop`.

Those guards are all structural. They are good for obvious cases, but they still answer a narrower question:

> “Does the turn structure imply more work?”

They do **not** answer the higher-level question the user wants here:

> “Has the agent actually satisfied the task-specific completion criterion?”

That higher-level question needs a separate evaluator.

## 2) Existing systems and constraints

### 2.1 Existing stop/continue system

Relevant current behavior:

- `codex-rs/core/src/codex.rs`
  - candidate completion happens after `response.completed`
  - `needs_follow_up` is the main stop/continue switch
  - `--non-stop` and `--persistent` are applied there
- `codex-rs/protocol/src/models.rs`
  - backend may provide `phase=commentary|final_answer`
  - backend may provide `end_turn`
  - neither is reliable enough to be the sole completion signal
- `codex-rs/core/src/state/session.rs`
  - session state already owns normalized history
- `codex-rs/core/src/context_manager/history.rs`
  - `ContextManager::raw_items()` and `for_prompt()` already provide stable in-memory history

This means the completion gate must hook into the **candidate stop boundary**, not into raw streaming text.

### 2.2 Existing structured-output support

Codex already supports strict structured output for normal turns via `final_output_json_schema`:

- `codex-rs/protocol/src/protocol.rs`
- `codex-rs/core/src/client_common.rs`

This is useful precedent, but the completion judge should be a **separate call path**. We do not want to overload the main agent turn with evaluation logic.

### 2.3 Existing `lib_llm` reference

The Python reference already exists outside this repo:

- `cookiecutter-pitchai-project-v2/.../libs/lib_llm/src/lib_llm/_client.py`
- `cookiecutter-pitchai-project-v2/.../libs/lib_llm/src/lib_llm/_request_builder.py`
- `cookiecutter-pitchai-project-v2/.../libs/lib_llm/src/lib_llm/_transport.py`

Important properties worth preserving in Rust:

- thin client,
- explicit backend config,
- strict JSON-schema generation,
- clean separation of request building vs transport,
- usable outside this one feature later.

## 3) Architecture decision

### 3.1 Chosen architecture

The best architecture is:

1. **A small dedicated Rust LLM client**, modeled after `lib_llm`, used first by the completion gate.
2. **A core completion-gate subsystem** that runs only at candidate stop boundaries.
3. **A bounded transcript builder** that reads normalized session history and emits XML-formatted judge context.
4. **A strict JSON-schema judge response** that deterministically controls stop vs continue.
5. **A synthetic user continuation message** when the judge denies stopping.

### 3.2 Alternatives considered

#### A. Heuristics only in `codex.rs`

Rejected.

Examples: “assistant messages under 5 lines are not final”, “final messages are longer”, “commentary means continue”. Those heuristics are already useful in narrow places, but they are not a real task-completion evaluator. They are also fragile across models.

#### B. Trigger the judge on every assistant message item

Rejected.

Codex stop/continue is decided at `response.completed`, not per item. Triggering the judge earlier would cause redundant calls, race conditions with tool calls later in the same response, and false positives.

#### C. Reuse the main Codex model session for the judge

Rejected.

That would couple the judge to agent-turn state, risk polluting the conversation, and make budgets, prompts, schema parsing, and observability harder to isolate.

### 3.3 Why the chosen architecture is best

It matches how Codex already works:

- stop/continue is decided at one clear place,
- history already exists in normalized in-memory form,
- structured schema support already exists conceptually,
- `persistent` and `non-stop` can remain orthogonal,
- future reuse of the judge client becomes straightforward.

## 4) Scope and non-goals

### 4.1 In scope

- opt-in completion gate mode,
- CLI enablement,
- slash-command session updates,
- minimal Rust `lib_llm`-style client,
- strict judge schema,
- synthetic continuation injection,
- TUI/app-server/frontend visibility,
- real-data smoke/integration/E2E validation.

### 4.2 Out of scope

- replacing the main Codex Responses API client,
- changing existing `persistent` / `non-stop` semantics,
- allowing silent fallback to heuristic-only completion when the gate is enabled,
- mock-based tests for the gate,
- synthetic transcript fixtures as the primary validation path.

## 5) User-facing behavior

### 5.1 New CLI surface

Session-scoped flags:

- `--completion-criteria <TEXT>`
  - enables the gate for this session and sets the criterion text
- `--completion-criteria-file <FILE>`
  - optional convenience for long criteria; mutually exclusive with inline text
- `--completion-judge-model <MODEL>`
  - optional override for the judge model
- `--completion-judge-base-url <URL>`
  - optional provider override
- `--completion-judge-api-key-env <ENV_VAR>`
  - optional explicit env-var name containing the key

Operational knobs:

- `--completion-judge-timeout-ms <N>`
- `--completion-judge-max-retries <N>`
- `--completion-judge-max-assistant-messages <N>` default `10`
- `--completion-judge-max-user-messages <N>` default `3`

### 5.2 New slash commands

Add a dedicated slash command, available during a running task:

- `/completion-criteria <TEXT>`
  - set or replace the active criterion for this session
- `/completion-criteria clear`
  - disable the gate for subsequent candidate-stop boundaries
- `/completion-criteria status`
  - show current criterion, judge model, last decision, and whether the next stop would be judged

Important rule:

- changing the criterion while a task is running takes effect at the **next candidate stop boundary**; it does not restart the current model response.

### 5.3 Status visibility

The UI must clearly show:

- completion gate enabled/disabled,
- current criterion text (possibly truncated in compact UI, full text in details view),
- last judge decision,
- when a stop was blocked by the judge,
- the continue prompt inserted by the judge.

This must surface in:

- TUI status/details,
- app-server protocol events,
- the real dispatch/browser UI.

## 6) Internal architecture

### 6.1 New Rust LLM client (`lib_llm` equivalent)

Create a small reusable Rust client with an interface intentionally similar to Python `lib_llm`.

Recommended workspace shape:

- new crate: `codex-rs/llm/`
- crate name: `codex-llm`

Initial public surface:

```rust
pub enum LlmBackend {
    OpenAi,
    AzureOpenAi,
    OpenAiCompatible,
}

pub struct LlmClientConfig {
    pub backend: LlmBackend,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub reasoning_effort: Option<String>,
}

pub struct StructuredJsonRequest {
    pub system_prompt: String,
    pub user_prompt: String,
    pub schema_name: String,
    pub schema: serde_json::Value,
    pub temperature: f32,
    pub max_tokens: u32,
}

pub struct StructuredJsonResponse {
    pub raw_text: String,
    pub parsed_json: serde_json::Value,
    pub request_id: Option<String>,
    pub model: String,
}

pub struct LlmClient { ... }

impl LlmClient {
    pub async fn generate_json(&self, request: StructuredJsonRequest) -> Result<StructuredJsonResponse, LlmError>;
}
```

### 6.2 Why a separate crate is preferred

This is the cleanest long-term shape because:

- the judge call is not part of the main Codex tool-using turn loop,
- it needs a much smaller surface,
- it can later power other evaluator/reviewer workflows,
- it keeps HTTP retries, request IDs, and schema parsing isolated.

### 6.3 MVP backend support

Architecture should support three backends from day one:

- OpenAI,
- Azure OpenAI,
- OpenAI-compatible.

Implementation may start with OpenAI first, but the API shape must not preclude the other two. The point is future-proof parity with `lib_llm`, not a one-off hardcoded judge transport.

## 7) Completion-gate subsystem in core

### 7.1 New config/state objects

Add session-scoped state, conceptually:

```rust
pub struct CompletionGateConfig {
    pub enabled: bool,
    pub criteria_text: String,
    pub judge_model: Option<String>,
    pub judge_base_url: Option<String>,
    pub judge_api_key_env: Option<String>,
    pub timeout_ms: u64,
    pub max_retries: u32,
    pub max_user_messages: usize,
    pub max_assistant_messages: usize,
}

pub struct CompletionGateState {
    pub last_judged_history_len: usize,
    pub last_decision: Option<CompletionJudgeDecision>,
    pub consecutive_denials: u32,
    pub consecutive_failures: u32,
}
```

`CompletionGateState` belongs in session state, not per-turn temporary state.

### 7.2 Candidate-stop hook point

The gate must run only when all of these are true:

1. the current model response is complete,
2. Codex would otherwise emit normal `TurnComplete`,
3. `needs_follow_up == false` after ordinary structural checks,
4. `--non-stop` is not preventing stop,
5. `--persistent` is not preventing stop,
6. there is no queued user input that already forces continuation.

This means the gate sits **after** existing hard continuation guards and **before** `TurnComplete` emission.

### 7.3 Order of operations

1. main turn reaches candidate stop
2. build completion-judge request from session history
3. call judge model
4. validate strict JSON-schema response
5. if `allow_stop=true`:
   - persist decision
   - advance judged boundary
   - allow normal `TurnComplete`
6. if `allow_stop=false`:
   - persist decision
   - advance judged boundary
   - inject synthetic user continuation from `continue_prompt`
   - continue the same session
7. if judge call errors or schema validation fails:
   - emit explicit gate error
   - do **not** silently allow stop
   - continue in fail-closed mode

## 8) Judge context construction

### 8.1 Source of truth

The judge transcript must come from live normalized session history:

- `SessionState.history`
- `ContextManager::raw_items()`

It must **not** be reconstructed from `.jsonl` rollout parsing during live operation.

Rollout files remain valuable for debugging and for standalone smoke tooling, but not as the online source of truth.

### 8.2 Bounded history rule

The judge must receive:

- the original user request for the session,
- the latest candidate final assistant message,
- all assistant and user messages since the last judge boundary,
- if there has never been a judge boundary yet:
  - up to the latest `3` user messages,
  - and up to the latest `10` assistant messages.

This exactly captures the requested behavior:

- conversational,
- bounded,
- incremental,
- avoids resending the entire session forever.

### 8.3 XML prompt envelope

The judge input should be rendered as explicit XML-like sections because that is readable, auditable, and robust for prompt construction.

Recommended shape:

```xml
<completion_gate_request>
  <criteria>
    ... user-provided completion criterion ...
  </criteria>
  <original_user_request>
    ... first user request of the session ...
  </original_user_request>
  <judge_window mode="since_last_judge_or_bounded_recent">
    <user_message index="1">...</user_message>
    <assistant_message index="2" phase="commentary">...</assistant_message>
    <assistant_message index="3" phase="final_answer">...</assistant_message>
  </judge_window>
  <candidate_final_response>
    ... the latest assistant response Codex is about to treat as final ...
  </candidate_final_response>
</completion_gate_request>
```

### 8.4 System prompt for the judge

The system prompt should instruct the judge to do exactly one thing:

- decide whether the candidate stop is allowed under the criterion,
- explain briefly,
- if not allowed, provide a concise continuation prompt addressed to Codex.

It must also explicitly instruct:

- no tools,
- no web search,
- no speculation about hidden state,
- evaluate based only on the provided conversation context.

## 9) Judge output schema

The judge must return strict JSON Schema output, not free text.

Recommended file:

- `codex-rs/core/schemas/completion_gate_decision.schema.json`

Recommended shape:

```json
{
  "type": "object",
  "additionalProperties": false,
  "required": [
    "allow_stop",
    "reason",
    "missing_requirements",
    "continue_prompt",
    "evidence"
  ],
  "properties": {
    "allow_stop": { "type": "boolean" },
    "reason": { "type": "string", "minLength": 1 },
    "missing_requirements": {
      "type": "array",
      "items": { "type": "string" }
    },
    "continue_prompt": { "type": "string" },
    "evidence": {
      "type": "array",
      "items": { "type": "string" }
    }
  }
}
```

Interpretation:

- `allow_stop=true`
  - `continue_prompt` may be empty
- `allow_stop=false`
  - `continue_prompt` must be non-empty at runtime validation even if the JSON schema itself cannot encode that dependency simply

## 10) Continuation injection behavior

### 10.1 When stop is denied

If the judge denies stop, Codex must inject a synthetic continuation message into the same session.

That message must:

- be recorded in history,
- be visible in debugging/rollout,
- clearly indicate it came from the completion gate,
- contain the judge-provided `continue_prompt`.

Recommended shape in model-visible history:

```xml
<completion_gate_feedback>
  <decision>continue</decision>
  <reason>...</reason>
  <continue_prompt>...</continue_prompt>
</completion_gate_feedback>
```

### 10.2 Why synthetic user message is preferred

A synthetic user continuation is the right default because:

- it behaves like a normal follow-up request,
- it is easy to audit,
- it keeps the continuation explicit instead of magical,
- it avoids silently mutating developer instructions mid-session.

## 11) Failure semantics

### 11.1 Fail closed

If the completion gate is enabled, these cases must **not** silently allow stop:

- judge API error,
- timeout after configured retries,
- schema mismatch,
- empty / unparsable judge response,
- missing `continue_prompt` on a deny decision.

This repo explicitly disallows fallback-heavy business logic. The gate is business logic. Therefore:

- no “judge failed, so just stop anyway”,
- no “judge failed, fall back to heuristics only”,
- no silent bypass.

### 11.2 Explicit edge retries are allowed

The LLM client is an API edge. Retries with backoff are acceptable there.

Recommended behavior:

- retry transient 429/5xx/network failures,
- respect timeout budget,
- record request ID and final error,
- after retry exhaustion, fail closed and continue rather than silently stopping.

## 12) Interaction with existing modes

### 12.1 `--non-stop`

`--non-stop` remains stronger.

If `--non-stop` is enabled:

- normal turn completion is already forbidden,
- the completion judge does not decide stop,
- the judge may optionally be skipped entirely to avoid wasted calls.

### 12.2 `--persistent`

`--persistent` remains a hard structural gate.

If a session terminal is still alive and `--persistent` is enabled:

- do not run the judge yet,
- do not allow stop yet,
- wait until the persistent condition is satisfied.

Only once `--persistent` would allow stopping should the completion gate judge the result.

### 12.3 Queued user messages

If queued user input already forces continuation, do not spend a judge call on that boundary. The judge is only for real candidate-stop moments.

## 13) App-server and frontend requirements

### 13.1 New protocol events

Expose explicit events for observability:

- `completion_gate_started`
- `completion_gate_decision`
- `completion_gate_blocked_stop`
- `completion_gate_error`

Each event should include:

- session/thread id,
- criterion version/hash,
- judge model,
- allow/deny/error,
- reason,
- request id if available,
- latency.

### 13.2 Real browser UI behavior

The real frontend must show:

- gate enabled badge,
- active criterion,
- “Stop blocked by completion gate” banner when applicable,
- judge reason,
- follow-up injected by the gate,
- latest allow/deny state.

This is required because the user explicitly wants a real UI that can prove the behavior, not just terminal logs.

Because `codex-rs` provides the backend/app-server but not the browser app itself, the real Playwright E2E is expected to run against the actual dispatch/frontend repo while starting the real Codex app-server from this repo.

## 14) Observability and reporting

For every judged stop boundary, record:

- session id,
- turn id,
- criterion hash,
- judge input window sizes,
- judge model,
- latency,
- request id,
- allow/deny/error,
- continuation injected or not.

Recommended outputs:

- normal logs,
- rollout/session record,
- app-server protocol event,
- optional machine-readable artifact:
  - `artifacts/completion-gate/<timestamp>/decision.json`

## 15) Definition of done

The feature is done only when all of the following are true:

1. `codex-llm` (or equivalent dedicated module) exists and can make a real structured judge call.
2. A session-scoped completion-gate configuration exists in CLI/runtime state.
3. A slash command can set, clear, and inspect the active criterion.
4. Candidate-stop boundaries call the judge only when Codex would otherwise stop.
5. A deny decision injects a synthetic continuation and the session continues.
6. An allow decision permits normal stop.
7. Judge failures are fail-closed and visible.
8. TUI/app-server/frontend all expose the active criterion and last decision.
9. Real-data smoke, integration, and browser E2E tests all pass.
10. Those tests fail meaningfully when the feature is broken or misconfigured.

## 16) Real-data validation plan (6 required methods)

No mocks, no stubs, no synthetic transcript fixtures as the primary validation path.

### 16.1 Validation 1 — direct judge-client smoke on a real session file

Goal:

- run the actual judge client binary/module against a real recorded Codex session file and a real API call.

Method:

- use a real `.jsonl` from `~/.codex/sessions/...` or a freshly recorded real session,
- build the XML judge payload from that real session context,
- call the actual judge model,
- assert strict JSON-schema parsing succeeds.

Example shape:

- `cargo run -p codex-llm --bin completion-judge-smoke -- --session ~/.codex/sessions/...jsonl --criteria "..."`

Why this can fail properly:

- wrong auth,
- bad schema wiring,
- broken transcript extraction,
- broken JSON parsing.

### 16.2 Validation 2 — real CLI smoke where judge denies stop

Goal:

- prove that a would-be final assistant response is blocked and a continuation is inserted.

Method:

- run the real CLI on a real git repo with `--completion-criteria`,
- use a real prompt that commonly yields an early “I’m done / current status” response before the work is actually complete,

Example shape:

- `cargo run -p codex-cli -- exec --completion-criteria "Only stop once the requested file has been modified and verified" "..."`

- assert from the actual rollout/logs/UI that:
  - Codex reached a candidate stop,
  - the judge denied it,
  - a synthetic continuation was inserted,
  - the session kept running.

Why this can fail properly:

- judge never called,
- deny path not wired,
- continuation not injected,
- history boundary wrong.

### 16.3 Validation 3 — real CLI smoke where judge allows stop

Goal:

- prove that the happy path still stops cleanly.

Method:

- run the real CLI with a criterion that is clearly satisfied,
- use a real prompt whose final output is obviously complete,
- assert:
  - the judge was called,
  - it returned `allow_stop=true`,
  - Codex emitted normal completion.

Why this can fail properly:

- judge deny loop bug,
- incorrect criteria packaging,
- stop never released.

### 16.4 Validation 4 — real session with `--persistent` + completion gate

Goal:

- prove the interaction order is correct.

Method:

- start a real background terminal task,
- run a session with both `--persistent` and `--completion-criteria`,
- verify:
  - while the terminal is alive, `persistent` prevents stop,
  - the judge is not wasted on those boundaries,
  - once the terminal exits and the agent reaches a real candidate stop, the judge runs.

Why this can fail properly:

- wrong ordering between `persistent` and judge,
- judge spam while terminal still alive,
- accidental stop before terminal exit.

### 16.5 Validation 5 — direct real app-server integration test

Goal:

- prove the actual server protocol emits the right state.

Method:

- start the real Codex app-server process,
- create a real session with completion criteria enabled,
- drive a real run to a judge-denied and judge-allowed boundary,
- assert the actual emitted protocol stream contains:
  - gate started,
  - decision event,
  - blocked-stop event when applicable,
  - final stop when allowed.

Why this can fail properly:

- missing protocol event,
- UI-visible state not wired,
- event ordering bug.

### 16.6 Validation 6 — real Playwright browser E2E against the real frontend

Goal:

- prove the actual browser UI shows the behavior the user cares about.

Method:

- start the real Codex app-server,
- start the real frontend/dispatch UI for real,
- use Playwright against the real browser UI,
- create a session with completion criteria,
- send a real prompt,
- assert the screen shows:
  - completion gate enabled,
  - stop blocked banner when denied,
  - continuation message visible,
  - final completion only after the judge allows it.

Why this can fail properly:

- UI not wired,
- banner never appears,
- continuation not rendered,
- app-server/frontend protocol mismatch.

## 17) How test results must be reported

Every full validation run must produce:

1. a short markdown summary,
2. a machine-readable JSON report,
3. failure artifacts for the UI run.

Recommended artifact set:

- `artifacts/completion-gate/<timestamp>/summary.md`
- `artifacts/completion-gate/<timestamp>/results.json`
- `artifacts/completion-gate/<timestamp>/playwright/` screenshots, trace, video, HTML dump

Minimum fields in `results.json`:

- git SHA,
- runtime command lines,
- session ids,
- judge model,
- criteria hash,
- pass/fail per validation method,
- request IDs,
- artifact paths,
- first failing assertion if any.

## 18) Runtime criteria before stopping is allowed

When completion gate is enabled, stop is allowed only if **all** of the following are true:

1. Codex has reached a real candidate stop boundary.
2. `--non-stop` is not enabled.
3. `--persistent` is either disabled or already satisfied.
4. No queued user input already forces continuation.
5. The judge request succeeded.
6. The judge response passed strict JSON-schema validation.
7. The judge returned `allow_stop=true`.

If any one of these is false, Codex may not stop.

## 19) Delivery criteria before implementation work may stop

Implementation work may not be considered complete until:

- the feature exists end-to-end,
- the browser UI proves it,
- the CLI proves it,
- app-server proves it,
- the failure path proves it fails closed,
- the validation artifacts are produced from real runs,
- the real-data tests fail when the system is intentionally misconfigured.

## 20) Recommended implementation sequence

1. add `codex-llm` thin client with strict JSON-schema call support
2. add completion-gate config/state types
3. add transcript builder from session history
4. add judge prompt + schema
5. hook judge into candidate-stop boundary in core
6. add synthetic continuation injection
7. add TUI/app-server/frontend visibility
8. add real smoke scripts
9. add real app-server integration test
10. add real Playwright browser E2E

## 21) Explicit must-preserve rules

These rules are integral to the fork and must not be lost in future upstream merges:

- the judge runs only at real candidate-stop boundaries,
- enabling the gate never silently falls back to “just stop anyway”,
- the gate reads normalized live session history, not online `.jsonl` reparsing,
- deny decisions become explicit synthetic continuations,
- the browser UI exposes the decision and continuation,
- real-data validation is mandatory for this feature.
