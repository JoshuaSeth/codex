# Codex live session status (Route 4)

## Verbatim goal + architecture (from request)

### 4) Codex-level live status + unify exec and TUI (requires Codex change): “one truth for all frontends”

Idea
Make Codex itself expose a durable, queryable session status that works for:

- codex exec (runner automation)
- codex interactive TUI/TUI2
- potentially app-server

Concretely: standardize $CODEX_HOME/live/<thread_id>.json to include:

- pid, started_at, last_heartbeat_at
- frontend: exec / tui / tui2 / app-server
- status: running / waiting_pending_tool / waiting_user_input / completed / errored
- detail: pending tool call_id/tool_name, current turn id, etc.

Where in Codex

- Exec already writes live/<id>.json (../codex/codex-rs/exec/src/pending_tool_ipc.rs:40), but only host/port.
  - extend it to include pid + timestamps and periodically refresh (or refresh on event emission).
- For TUI, add a similar “live marker writer” in codex-rs/tui and codex-rs/tui2 during session lifetime.
  - this directly solves the original “is this rollout open in some terminal somewhere?”: the TUI process itself
    writes the marker.
- You can also add an explicit “waiting for input” transition when the TUI returns to the prompt.

Pros

- Solves the hard version of the problem (TUI “open / waiting for input”) in a principled way.
- Gives dispatcher a single, stable contract for session liveness across exec + interactive.
- Avoids filesystem heuristics and log parsing: state is first-class.

Cons / risks

- Requires modifying Codex core/frontends and rolling out a new Codex binary + runner image.
- Needs careful cleanup semantics:
  - handle crashes (stale live marker)
  - handle multiple processes resuming same thread (should that be allowed?)
- If you want dispatcher to contact the exec process for deliver-pending from outside the runner container, you’ll
  also need to address the current container-network mismatch (server binds 127.0.0.1 inside container). Options
  include:
  - bind to 0.0.0.0 and restrict via firewall + random token
  - run runner container with host networking (Linux only; security tradeoffs)
  - or keep deliver-pending “in-container only” and have dispatcher trigger docker exec codex exec deliver-
    pending ...

Best use
If you want a long-term unified architecture where “session status” is always available (including TUI sessions), this
is the most correct design. It’s just more invasive.

## Product spec

### Problem statement

We need a reliable way (local dev + remote prod host) to answer:

1. Is a given Codex thread/session currently running anywhere?
2. If yes, where (which host/device) is it running?
3. If yes, what state is it in?
   - running (actively executing a turn)
   - waiting for external pending tool result
   - waiting for user input in an interactive TUI
   - completed/errored (terminal states for non-interactive sessions; interactive sessions only when user exits)

This must work even when the “frontend” is a terminal UI (TUI/TUI2) where we cannot introspect terminal state from the
outside.

### High-level approach

Codex itself becomes the source of truth by writing a “live status record” per thread at:

`$CODEX_HOME/live/<thread_id>.json`

The record is updated periodically (heartbeat) and on meaningful state transitions.

PitchAI dispatcher reads this record (by thread_id) and renders it in the run UI and via a JSON API.

### Live status JSON contract (v1)

#### Path

- `$CODEX_HOME/live/<thread_id>.json`
  - same directory already used by exec’s pending-tool IPC metadata

#### Required fields

- `schema_version` (number): `1`
- `thread_id` (string): Codex thread id / conversation id
- `instance_id` (string): unique id for this process instance (uuid)
- `frontend` (string enum): `exec | tui | tui2 | app-server`
- `status` (string enum):
  - `running`
  - `waiting_pending_tool`
  - `waiting_user_input`
  - `completed`
  - `errored`
- `pid` (number)
- `started_at` (RFC3339 UTC string)
- `last_heartbeat_at` (RFC3339 UTC string)
- `alive` (boolean)

#### Optional fields

- `ended_at` (RFC3339 UTC string)
- `detail` (object):
  - for pending tool: `call_id`, `tool_name`, `turn_id`, `note`
  - for errors/completions: `note` or `message`
- `hostname` (string)
- `device_id` (string) – from env (see below)
- `cwd` (string) – session working root
- `tty` (string) – best-effort (interactive only)
- `ppid` (number) – best-effort
- `cli_version` (string) – best-effort
- `ipc` (object) – only for exec (pending-tool delivery):
  - `host` (string)
  - `port` (number)

#### Backward compatibility fields (temporary)

To avoid breaking existing `codex exec deliver-pending` implementations, the record should continue to be readable as
the old “pending tool IPC metadata” shape. Two acceptable strategies:

1. Keep top-level `host`/`port` in addition to `ipc.host/ipc.port`, OR
2. Update deliver-pending to read `ipc` and keep a legacy parser that accepts `{host,port}`.

We will implement (2) and keep (1) only if needed for legacy binaries.

#### Heartbeat + staleness

- Codex updates `last_heartbeat_at` at a fixed interval (default: 2s) while the process is alive.
- Consumers treat the record as stale when `now - last_heartbeat_at > STALE_THRESHOLD` (default: 30s).
- “Stale” means: likely crashed or ended without graceful shutdown, even if `alive=true` in the last written record.

#### Device identity

Codex fills `device_id` from (first hit wins):

- `CODEX_LIVE_DEVICE_ID`
- `PITCHAI_DEVICE_ID`

This allows both local dev and the prod host to explicitly label machines (e.g. `mbp-seth`, `hetzner-prod-1`).

### State transitions (normative)

#### Exec

- When thread/session is created or resumed: write record with `status=running`.
- When `PendingToolState(status=Waiting)` is observed: `status=waiting_pending_tool` and fill `detail`.
- When `PendingToolState(status=Resolved|Cancelled)` is observed: `status=running` (turn resumes).
- When exec finishes successfully: `status=completed`, `alive=false`, `ended_at` set.
- When exec ends due to a fatal error: `status=errored`, `alive=false`, `ended_at` set.

#### TUI / TUI2

- When session configured (thread id known): write record with `status=waiting_user_input`.
- When a turn starts (`TurnStarted`): `status=running`.
- When a turn completes (`TurnComplete`) and user is back at prompt: `status=waiting_user_input`.
- When pending tool waiting/resolved/cancelled events occur: mirror exec semantics but return to
  `running` (resume) and then eventually `waiting_user_input`.
- When user exits the app: `status=completed`, `alive=false`, `ended_at` set.

### Dispatcher integration

Dispatcher must be able to show this status on the run page and via API.

#### Contract

- Dispatcher derives `thread_id` from:
  - run record (`runs/<bundle>.json`) when available, else
  - parsing Codex JSONL log for `thread.started`.

- Dispatcher looks up live status at:
  - `DATA_ROOT/codex_home/live/<thread_id>.json`
    - where `DATA_ROOT` is the host-mounted persistent volume root (e.g. `/data`)
    - the dispatcher already mounts `codex_home` as `/data/codex_home`

#### UI changes

Run page “Status” panel must show:

- queue state (existing)
- runner container status (existing)
- codex live status (new), including:
  - frontend
  - state
  - pid
  - hostname/device_id
  - heartbeat age + stale warning

#### API changes

Add an authenticated endpoint returning a JSON snapshot:

- `GET /runs/<bundle>/status`
  - includes `queue_state`, `runner_status`, `thread_id`, and `live_status` (raw record + derived fields)

### Definition of done

1. `codex exec` writes `$CODEX_HOME/live/<thread_id>.json` with the v1 schema, updates heartbeats, and sets
   `completed/errored` on exit.
2. `codex` TUI and `codex` TUI2 write the same file for interactive sessions and correctly reflect
   `waiting_user_input` vs `running`.
3. `codex exec deliver-pending` still works (legacy shape accepted).
4. PitchAI dispatcher run page shows codex frontend/state/pid/device/heartbeat for real runs.
5. Stale detection works for crash/kill scenarios (no heartbeat ⇒ stale warning).
6. Playwright E2E test runs the real dispatcher server and verifies live status appears and transitions.

## Validation & testing (real data, no mocks)

All tests below are required before stopping.

1. **Smoke: real exec writes + completes**
   - Build Codex (`cargo build -p codex-cli`) and run a real `codex exec` with a real `CODEX_HOME` directory.
   - Verify the live file exists, updates `last_heartbeat_at`, and ends with `status=completed` and `alive=false`.

2. **Smoke: real TUI writes waiting/running**
   - Launch `codex` interactively, start a session, confirm `status=waiting_user_input`.
   - Send a prompt, confirm it switches to `running`, then back to `waiting_user_input`.

3. **Smoke: pending tool waiting/resume**
   - Configure a real custom tool with `hibernate_after_call=true` (so Codex emits `PendingToolState`).
   - Trigger it in a real session and verify the live status switches to `waiting_pending_tool` with `detail.call_id`.
   - Use `codex exec deliver-pending ...` to deliver output and verify the session resumes and status transitions.

4. **Integration: dispatcher reads live status**
   - Start dispatcher locally pointing `PITCHAI_HOST_RUNS_DIR` + `PITCHAI_HOST_QUEUE_DIR` at a real data dir.
   - Run a real Codex exec job that writes into the same `codex_home`.
   - Verify the `/ui/runs/<bundle>` status panel displays live status fields.

5. **E2E UI (Playwright): live status appears + transitions**
   - Playwright test:
     - starts the dispatcher server for real (uvicorn)
     - spawns a real Codex process that creates a real thread and writes live status
     - opens `/ui/runs/<bundle>` and asserts:
       - thread_id becomes non-pending
       - codex frontend is shown (e.g. `exec`)
       - state transitions `running → completed`
       - stale warning is not present
   - The test must fail if:
     - the live file is never created, OR
     - heartbeat fields are missing, OR
     - the UI does not render the expected badges.

6. **Crash-resilience: stale record detection**
   - Start a Codex process, wait for live file, then force-kill the process.
   - Verify dispatcher marks the live record stale (heartbeat age exceeds threshold) and shows a warning.

### Reporting + stop criteria

- Rust: `just fmt` and `just fix -p ...` must run clean for modified crates; relevant `cargo test -p ...` must pass.
- Dispatcher: Python import/syntax smoke must pass; server must boot in the E2E configuration.
- Playwright: test run must complete with exit code 0; failures must be visible via console output and HTML report.
- Stopping is allowed only when all definition-of-done items are met and all tests above have been run successfully.

