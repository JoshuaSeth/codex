# PitchAI Elise — Ad‑Hoc Telegram Dispatch (Plan + Implementation Notes)

Goal: allow Seth to send a Telegram message that triggers an Azure run of Elise (Codex agent). The Telegram message becomes the **prompt** for the next turn, and the final agent response is posted back to the **same Telegram chat**.

Constraints / decisions (per Seth):

- **Reply in the same Telegram chat** where the command was sent.
- Elise should **eternally continue the same Codex session** (same `thread_id`) across:
  - scheduled polling runs
  - ad-hoc Telegram-triggered runs
- Seth mailbox should be available for Elise to query/search, but with **separate state** (separate notes/ledger).
- Start with a **local** end-to-end validation path, then deploy as **Azure** webhook-driven.

## Why we cannot rely on ACA `job start --env-vars` for prompts

Azure Container Apps Job `start` is a `POST` with **no request body** (at least as currently used by the CLI for our environment). In practice, `--env-vars` is accepted by the CLI, but it does not affect the job execution template. Therefore, we cannot pass `PITCHAI_PROMPT_OVERRIDE` through the start API.

Implication: ad-hoc prompts must be passed via **shared storage** that both the dispatcher and the job can access.

## Architecture

### Components

1. **Elise Agent Job** (already exists)
   - Azure Container Apps **Scheduled Job** (cron)
   - Runs `/opt/pitchai/run_elise_job.py`
   - Uses Graph app-only mail tools (`mail_search`, `mail_read`, `send_email`)
   - Persists `thread_id` and state in Azure Files (`/mnt/elise`)
   - Sends a final message via Telegram stop-hook.

2. **Telegram Dispatcher** (new)
   - Azure Container App (always-on HTTP service with ingress)
   - Receives Telegram webhook updates on `POST /telegram/webhook`
   - Validates:
     - Telegram secret token header
     - Allowed `chat.id` (same chat we want replies in)
     - Allowed sender (optional, recommended)
   - Writes the incoming message into a **prompt queue file** in the shared Azure Files mount.
   - Starts a new Elise job execution (best-effort) so the message is handled immediately.

### Data flow

1. Seth sends Telegram message → Telegram bot.
2. Telegram calls dispatcher webhook with the update payload.
3. Dispatcher:
   - writes `/mnt/elise/prompts/telegram/<timestamp>_<update_id>.md`
   - starts Elise job execution
4. Elise job runner:
   - takes a lock (prevents concurrent runs from corrupting `state.json`)
   - consumes one prompt file (oldest)
   - runs/resumes the same Codex `thread_id`
5. Codex stop-hook posts final agent message back to the same chat.

## Prompt queue design

Directory layout on the shared volume (`/mnt/elise`):

- `/mnt/elise/prompts/telegram/` — pending prompt files written by dispatcher
- `/mnt/elise/prompts/telegram/_processed/` — archived prompt files after consumption
- `/mnt/elise/state.json` — persisted `thread_id` for Elise
- `/mnt/elise/elise/ledger.md` — Elise’s main ledger
- `/mnt/elise/seth_mailbox/ledger.md` — separate state/ledger for Seth mailbox activity

Prompt file format: markdown, includes metadata + the user’s message. Example:

```md
## Telegram command
- ts_utc: 2025-12-25T01:23:45Z
- update_id: 123456789
- chat_id: 5246077032
- from_user_id: 11111111
- from_username: sethvanderbijl

### Instruction
<raw telegram text>
```

Runner behavior:

- Consume at most **one** pending prompt per run (keeps each job bounded).
- If no prompts pending, fall back to the normal polling prompt file.
- After reading a prompt file, move it to `_processed/` (or rename with a `.done` suffix).

## Concurrency & “eternal session”

To preserve a single ongoing Codex session, the job must not run two Codex turns concurrently against the same `/mnt/elise/state.json` and `CODEX_HOME`.

Mitigations:

1. **Job-side lock** (mandatory, defense in depth)
   - Use an atomic directory creation lock on the Azure Files mount (works on CIFS).
   - If lock exists:
     - either wait briefly (bounded) or exit cleanly.
2. **Dispatcher-side guard** (optional)
   - Check the job execution list for an active `Running` execution.
   - If running: still queue the prompt, but avoid starting another execution immediately.

## Seth mailbox “separate state”

Two approaches:

1. **Separate tools** in Elise config (recommended)
   - Add `seth_mail_search` / `seth_mail_read` tools that set `PITCHAI_GRAPH_MAILBOX_UPN` via per-tool `env`.
   - Instruct Elise to log any Seth mailbox activity into `/mnt/elise/seth_mailbox/ledger.md`.
2. **Separate colleague session**
   - Elise calls `ask_colleague` to query Seth mailbox using a separate Codex conversation id.
   - Higher complexity; only do if needed.

## Local validation plan (before Azure webhook)

1. Run the dispatcher in “dev mode” (no Telegram webhook):
   - Accept a local POST with a Telegram-like JSON payload.
   - Write a prompt file into a local mount directory.
2. Run the Elise image locally with the same mount:
   - Confirm the runner consumes the prompt file and produces a Codex response.
   - Confirm Telegram stop-hook posts the response to the configured chat.

## Azure validation plan

1. Deploy dispatcher container app (ingress enabled, single replica).
2. Mount the same Azure Files share at `/mnt/elise`.
3. Assign managed identity with permission to start the job.
4. Set Telegram webhook to the dispatcher URL + secret token.
5. Send a Telegram message:
   - confirm prompt file appears in `/mnt/elise/prompts/telegram/`
   - confirm job run is triggered
   - confirm Telegram receives the final response

## Implementation checklist

- [ ] Add this plan doc (done by committing it)
- [ ] Update `codex-cli/scripts/pitchai_run_elise_job.py`:
  - [ ] prompt queue support
  - [ ] job lock to prevent concurrent runs
- [ ] Update `codex-cli/scripts/pitchai_elise_config.toml`:
  - [ ] add `seth_mail_search` / `seth_mail_read` with per-tool env override
  - [ ] add guidance for `/mnt/elise/seth_mailbox/ledger.md`
- [ ] Add dispatcher service code + Dockerfile
- [ ] Local smoke test (prompt queue + stop-hook)
- [ ] Azure deploy:
  - [ ] Container app w/ ingress
  - [ ] Mount `/mnt/elise`
  - [ ] Managed identity RBAC
  - [ ] Telegram webhook set
- [ ] End-to-end test: Telegram → ACA Job → Telegram

