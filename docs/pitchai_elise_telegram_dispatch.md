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
- After reading a prompt file, move it to `_processed/` on success (or `_failed/` on error).

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

- [x] Add this plan doc (`docs/pitchai_elise_telegram_dispatch.md`)
- [x] Update `codex-cli/scripts/pitchai_run_elise_job.py` (prompt queue + CIFS lock)
- [x] Update `codex-cli/scripts/pitchai_elise_config.toml` (Seth mailbox tools + separate ledger guidance)
- [x] Add dispatcher service code + Dockerfile (`codex-cli/scripts/telegram_dispatcher/`)
- [x] Azure deploy:
  - [x] Container app w/ ingress
  - [x] Mount `/mnt/elise`
  - [x] Managed identity RBAC (start ACA job)
  - [x] Telegram webhook set (secret token header)
- [x] End-to-end test: Telegram → prompt queue → ACA job → Telegram (verified via ACA logs)

## Reproducible deployment (Azure)

This section is a practical “copy/paste” guide to reproduce the setup in a new resource group / environment.

### 1) Build + push the dispatcher image (ACR)

From the `codex/` repo root:

```bash
ACR_NAME="<your_acr_name>"
TAG="$(git rev-parse --short HEAD)-amd64"

az acr login -n "$ACR_NAME"

docker buildx build --platform linux/amd64 \
  -f codex-cli/scripts/telegram_dispatcher/Dockerfile \
  -t "$ACR_NAME.azurecr.io/pitchai/elise-telegram-dispatcher:$TAG" \
  --push .
```

### 2) Create/update the dispatcher Container App

Prereqs:

- A Container Apps Environment with an Azure Files mount named `elise` (see `docs/pitchai_elise_agent.md`).
- The Elise agent job already exists (example job name: `elise-agent-job`).

Create the app (first time) or update it (subsequent):

```bash
RG="<resource-group>"
ENV_NAME="<containerapps-env-name>"
APP_NAME="elise-tg-dispatcher"
JOB_NAME="elise-agent-job"
SUB_ID="$(az account show --query id -o tsv)"
IMAGE="$ACR_NAME.azurecr.io/pitchai/elise-telegram-dispatcher:$TAG"

# Secrets (values omitted here on purpose)
az containerapp secret set -g "$RG" -n "$APP_NAME" \
  --secrets \
    telegram-bot-token="<BOT_TOKEN>" \
    tg-webhook-secret="<WEBHOOK_SECRET>" \
  -o none

az containerapp update -g "$RG" -n "$APP_NAME" \
  --image "$IMAGE" \
  --set-env-vars \
    TELEGRAM_BOT_TOKEN=secretref:telegram-bot-token \
    TELEGRAM_WEBHOOK_SECRET=secretref:tg-webhook-secret \
    TELEGRAM_ALLOWED_CHAT_ID="<CHAT_ID>" \
    ACA_SUBSCRIPTION_ID="$SUB_ID" \
    ACA_RESOURCE_GROUP="$RG" \
    ACA_JOB_NAME="$JOB_NAME" \
    PITCHAI_DISPATCH_MODE="azure" \
  -o none

# Ensure the shared volume is mounted at /mnt/elise
az containerapp update -g "$RG" -n "$APP_NAME" \
  --set-env-vars PITCHAI_PROMPT_QUEUE_DIR="/mnt/elise/prompts/telegram" \
  -o none
```

Mounting Azure Files (if creating from scratch) is usually easiest via:

- `az containerapp create ... --storage-mounts ...` (or)
- set it in the Portal once, then keep it stable.

### 3) Give the dispatcher permission to start the job

Assign managed identity to the dispatcher app and grant it access to the job resource scope.

Role guidance:

- simplest: `Contributor` on the Job resource (works)
- tighter: a custom role that allows `Microsoft.App/jobs/start/action` and `Microsoft.App/jobs/executions/read`

### 4) Set the Telegram webhook (secret token)

Telegram uses the `X-Telegram-Bot-Api-Secret-Token` header to sign webhook calls. The dispatcher validates it.

```bash
FQDN="$(az containerapp show -g "$RG" -n "$APP_NAME" --query properties.configuration.ingress.fqdn -o tsv)"

curl -X POST "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/setWebhook" \
  -d "url=https://${FQDN}/telegram/webhook" \
  -d "secret_token=<WEBHOOK_SECRET>" \
  -d 'allowed_updates=["message"]'
```

### 5) End-to-end test (without Telegram)

You can simulate Telegram by POSTing a payload directly (use the same secret header Telegram would send):

```bash
curl -X POST "https://${FQDN}/telegram/webhook" \
  -H "Content-Type: application/json" \
  -H "X-Telegram-Bot-Api-Secret-Token: <WEBHOOK_SECRET>" \
  --data '{"update_id":900000123,"message":{"message_id":1,"date":1735080000,"chat":{"id":<CHAT_ID>,"type":"private"},"from":{"id":<CHAT_ID>,"is_bot":false,"first_name":"Seth"},"text":"10+43"}}'
```

Then confirm:

- Dispatcher returns HTTP 200.
- An `elise-agent-job-*` execution starts.
- Job logs include `[prompt] Using queued prompt: ...` and the agent’s response.

### 6) Common gotcha: secrets + revisions

If you update the dispatcher’s webhook secret in ACA, ensure the running revision uses the updated secret:

- easiest: `az containerapp update` (creates a new revision in multiple-revisions mode; in single mode it updates the active template)
- verify by sending a bad secret and confirming logs print `[auth] webhook secret mismatch ...`
