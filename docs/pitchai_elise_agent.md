# PitchAI Elise Mail Agent (Codex) — Deployment & Ops Guide

This document explains how to deploy and operate the “Polling Elise” Codex agent as an **Azure Container Apps (ACA) Scheduled Job**. It is designed to be fully reproducible and also contains a quick reference so the agent (“Elise”) can understand how to read and act on email.

## Elise quick reference (mail tools + workflow)

Elise runs as a Codex agent driven by a natural-language prompt. The prompt + config register these **custom tools**:

- `mail_search` — list newest messages (defaults to **unread** in Inbox).
- `mail_read` — read a specific message body + headers.
- `send_email` — send an email (Graph app-only).

When operating:

1. Call `mail_search` (usually `top=15`, `unread_only=true`) to see what’s new.
2. Call `mail_read` for any message you need to act on.
3. Maintain persistent state:
   - Write decisions + next actions to `/mnt/elise/elise/ledger.md`.
   - Store drafts under `/mnt/elise/elise/drafts/`.
4. Respect the boss-approval policy:
   - You may always email Seth (`seth.vanderbijl@pitchai.net`) for advice/approval.
   - Do **not** email anyone else unless Seth explicitly approves.
   - Approval arrives by email containing `APPROVED: <draft_filename>`.
5. When finished for now, write a short status update in `ledger.md` and end your final response with a concise summary + what you’re waiting on.

Tool parameters (as implemented today):

- `mail_search` params:
  - `folder` (string, default `Inbox`)
  - `top` (int, default `15`, max `50`)
  - `unread_only` (bool, default `true`)
  - `from_address` (string, optional)
  - `subject_contains` (string, optional)
  - `received_since_utc` (string, optional; ISO-8601)
- `mail_read` params:
  - `message_id` (string, required)
  - `max_chars` (int, default `15000`)
  - `mark_as_read` (bool, default `false`)
- `send_email` params:
  - `to` (string[], required)
  - `subject` (string, required)
  - `body` (string, required)
  - `cc`, `bcc`, `reply_to` (string[], optional)
  - `body_type` (string, default `HTML`)
  - `save_to_sent` (bool, default `true`)

Implementation references:

- Tool config: `codex-cli/scripts/pitchai_elise_config.toml`
- Tool implementation: `codex-cli/scripts/pitchai_elise_graph_tool.py`

## Architecture overview

High-level:

1. An ACA **Scheduled Job** runs every 6 hours (cron, UTC).
2. The container entrypoint runs `/opt/pitchai/run_elise_job.py`.
3. The runner:
   - Decodes `CODEX_AUTH_JSON_B64` into `$CODEX_HOME/auth.json`
   - Persists a `thread_id` in `/mnt/elise/state.json`
   - Executes the agent via `codex exec` (and uses `resume <thread_id>` on subsequent runs)
4. Codex config registers Graph mail custom tools and a Telegram `stop_hook_command` to send the final message after each run.
5. An Azure Files mount at `/mnt/elise` persists:
   - Codex home (`/mnt/elise/codex_home`)
   - Agent working directory + ledger (`/mnt/elise/elise/`)
   - Thread state (`/mnt/elise/state.json`)

Related: for **ad-hoc** “Telegram message → run Elise immediately”, see `docs/pitchai_elise_telegram_dispatch.md`.

## Repo file map (what to copy/build)

- `codex-cli/scripts/pitchai_elise_prompt.md` — the natural-language prompt for each scheduled run.
- `codex-cli/scripts/pitchai_elise_config.toml` — Codex config:
  - disables sandbox and approvals inside the already-sandboxed container
  - registers `mail_search`, `mail_read`, `send_email`
  - registers Telegram stop hook
  - sets persistence + boss-approval rules in the base prompt
- `codex-cli/scripts/pitchai_elise_graph_tool.py` — Python Graph app-only tool implementation.
- `codex-cli/scripts/pitchai_run_elise_job.py` — runner that persists `thread_id` and starts/resumes the agent.
- `tools/custom_tools/telegram_bot.py` — Telegram notifier + `--stop-hook` mode.
- `codex-cli/Dockerfile` — container image that bundles everything into `/opt/pitchai/` and installs Python deps.

## Credentials & secrets

This deployment uses **two independent auth systems**:

1. **Codex auth** (OpenAI / ChatGPT plan / OpenAI API key), stored as `auth.json`.
2. **Microsoft Graph app-only** (client credentials with certificate), used to read/send mail.

Secrets must be injected at runtime (ACA Job secrets / Key Vault references). Do **not** bake credentials into git or into the image.

### A) Codex: generate `auth.json` and store it as `CODEX_AUTH_JSON_B64`

Supported shortcut for headless environments: authenticate locally and copy the resulting `$CODEX_HOME/auth.json` into the container (Codex docs: `docs/authentication.md`).

Usage-based billing (API key example):

```bash
printenv OPENAI_API_KEY | codex login --with-api-key
```

Then base64-encode `auth.json` (strip newlines):

```bash
# macOS
CODEX_AUTH_JSON_B64="$(base64 ~/.codex/auth.json | tr -d '\n')"

# Linux
CODEX_AUTH_JSON_B64="$(base64 -w0 ~/.codex/auth.json)"
```

In Azure, set a job secret:

- Secret name: `codex-auth-json-b64`
- Env var: `CODEX_AUTH_JSON_B64=secretref:codex-auth-json-b64`

The runner decodes this into `$CODEX_HOME/auth.json` on startup (`codex-cli/scripts/pitchai_run_elise_job.py`).

### B) Graph: create an app-only “mail agent” app + certificate

Create the app registration **in the tenant that contains the mailbox** (your Azure subscription tenant can be different).

1. Create an Entra ID app registration (e.g. `pitchai-elise-graph`)
2. Add a certificate credential (upload the public cert)
3. Add Microsoft Graph **Application** permissions:
   - `Mail.ReadWrite`
   - `Mail.Send`
4. Grant **admin consent**

Generate a certificate (example: 10 years):

```bash
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout elise_graph.key.pem \
  -out elise_graph.cert.pem \
  -subj "/CN=pitchai-elise-graph"
```

Base64 encode the PEM files (strip newlines):

```bash
# macOS
PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64="$(base64 elise_graph.key.pem | tr -d '\n')"
PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64="$(base64 elise_graph.cert.pem | tr -d '\n')"
```

In Azure, configure:

- `PITCHAI_GRAPH_TENANT_ID` (non-secret)
- `PITCHAI_GRAPH_CLIENT_ID` (non-secret)
- `PITCHAI_GRAPH_MAILBOX_UPN` (non-secret, e.g. `elise@pitchai.net`)
- Secret: `pitchai-graph-private-key-b64` → env `PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64`
- Secret: `pitchai-graph-public-cert-b64` → env `PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64`

Optional hardening (recommended later): restrict app-only mailbox access using an Exchange Application Access Policy so the app can only read Elise’s mailbox.

### C) Telegram: stop-hook notifications

The Codex `stop_hook_command` posts the agent’s final message to Telegram.

In Azure:

- Secret: `telegram-bot-token` → env `TELEGRAM_BOT_TOKEN`
- Secret: `telegram-chat-id` → env `TELEGRAM_CHAT_ID`

Stop-hook implementation: `tools/custom_tools/telegram_bot.py` (copied into the image as `/opt/pitchai/telegram_bot.py`).

## Build and push the container image

This repo’s image builds:

- The native `codex` binary from `codex-rs` (Rust).
- Installs the published Node wrapper from `codex-cli/dist/codex.tgz`.
- Replaces the bundled native binary with the freshly built one.
- Copies PitchAI scripts into `/opt/pitchai/` and installs the Python deps (MSAL, requests, etc.).

Example (push to ACR, amd64 for Azure):

```bash
ACR_NAME="<your_acr_name>"
TAG="$(git rev-parse --short HEAD)-amd64"

az acr login -n "$ACR_NAME"

docker buildx build --platform linux/amd64 \
  -f codex-cli/Dockerfile \
  -t "$ACR_NAME.azurecr.io/pitchai/codex-agent:$TAG" \
  --push .
```

## Azure: storage + Container Apps Environment mount

This job requires persistence across runs. We use Azure Files and mount it at `/mnt/elise`.

1. Create a storage account + file share (example):

```bash
RG="<resource-group>"
LOCATION="westeurope"
STORAGE="<storage-account-name>"
SHARE="elise"

az storage account create -g "$RG" -n "$STORAGE" -l "$LOCATION" --sku Standard_LRS
KEY="$(az storage account keys list -g "$RG" -n "$STORAGE" --query '[0].value' -o tsv)"
az storage share create --account-name "$STORAGE" --account-key "$KEY" --name "$SHARE"
```

2. Attach the file share to your Container Apps Environment:

```bash
ENV_NAME="<containerapps-env-name>"

az containerapp env storage set -g "$RG" -n "$ENV_NAME" \
  --storage-name elise \
  --access-mode ReadWrite \
  --azure-file-account-name "$STORAGE" \
  --azure-file-account-key "$KEY" \
  --azure-file-share-name "$SHARE"
```

## Azure: create the Scheduled Job

The job should:

- Use the pushed image
- Mount environment storage `elise` to `/mnt/elise`
- Run `/opt/pitchai/run_elise_job.py`
- Run on cron `0 */6 * * *` (UTC)
- Inject secrets as environment variables

Tip: the easiest reproducible flow is a YAML-driven create:

```bash
az containerapp job create -g "$RG" -n elise-agent-job --yaml elise-agent-job.yaml
```

Minimal `elise-agent-job.yaml` shape (fill placeholders):

```yaml
location: <region>
name: elise-agent-job
properties:
  environmentId: /subscriptions/<sub>/resourceGroups/<rg>/providers/Microsoft.App/managedEnvironments/<env>
  configuration:
    triggerType: Schedule
    replicaTimeout: 1800
    replicaRetryLimit: 1
    scheduleTriggerConfig:
      cronExpression: "0 */6 * * *"
      parallelism: 1
      replicaCompletionCount: 1
    registries:
    - server: <acr>.azurecr.io
      username: <acr_username>
      passwordSecretRef: acr-password
    secrets:
    - name: acr-password
    - name: codex-auth-json-b64
    - name: telegram-bot-token
    - name: telegram-chat-id
    - name: pitchai-graph-private-key-b64
    - name: pitchai-graph-public-cert-b64
  template:
    containers:
    - name: elise-agent
      image: <acr>.azurecr.io/pitchai/codex-agent:<tag>
      command: ["/opt/pitchai/run_elise_job.py"]
      resources: { cpu: 1.0, memory: 2Gi }
      env:
      - name: CODEX_AUTH_JSON_B64
        secretRef: codex-auth-json-b64
      - name: TELEGRAM_BOT_TOKEN
        secretRef: telegram-bot-token
      - name: TELEGRAM_CHAT_ID
        secretRef: telegram-chat-id
      - name: PITCHAI_CODEX_MODEL
        value: gpt-5.2-high
      - name: PITCHAI_GRAPH_TENANT_ID
        value: <mailbox_tenant_guid>
      - name: PITCHAI_GRAPH_CLIENT_ID
        value: <app_client_id_guid>
      - name: PITCHAI_GRAPH_MAILBOX_UPN
        value: elise@yourdomain.com
      - name: PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64
        secretRef: pitchai-graph-private-key-b64
      - name: PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64
        secretRef: pitchai-graph-public-cert-b64
      volumeMounts:
      - mountPath: /mnt/elise
        volumeName: elise
    volumes:
    - name: elise
      storageName: elise
      storageType: AzureFile
```

Then set secrets:

```bash
az containerapp job secret set -g "$RG" -n elise-agent-job --secrets \
  codex-auth-json-b64="$CODEX_AUTH_JSON_B64" \
  telegram-bot-token="$TELEGRAM_BOT_TOKEN" \
  telegram-chat-id="$TELEGRAM_CHAT_ID" \
  pitchai-graph-private-key-b64="$PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64" \
  pitchai-graph-public-cert-b64="$PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64" \
  acr-password="$ACR_PASSWORD"
```

Notes:

- Cron is interpreted in **UTC**.
- For testing, set `PITCHAI_CODEX_MODEL=gpt-5.2-medium`.
- In this repo, `PITCHAI_CODEX_MODEL=gpt-5.2-high` maps to `-m gpt-5.2-codex -c model_reasoning_effort=high` in `codex-cli/scripts/pitchai_run_elise_job.py`.

## Runtime paths (what should exist)

After a run, the Azure Files share should contain:

- `/mnt/elise/state.json` (stores `thread_id`)
- `/mnt/elise/codex_home/auth.json` (decoded from secret at runtime)
- `/mnt/elise/elise/ledger.md`
- `/mnt/elise/elise/drafts/` (if any drafts were created)

## Validate end-to-end (local + Azure)

### Local smoke test

Mount a local directory to simulate `/mnt/elise` and run the image:

```bash
mkdir -p /tmp/elise_mount

docker run --rm -it \
  -v /tmp/elise_mount:/mnt/elise \
  -e CODEX_AUTH_JSON_B64="$CODEX_AUTH_JSON_B64" \
  -e PITCHAI_GRAPH_TENANT_ID="<...>" \
  -e PITCHAI_GRAPH_CLIENT_ID="<...>" \
  -e PITCHAI_GRAPH_MAILBOX_UPN="elise@..." \
  -e PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64="$PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64" \
  -e PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64="$PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64" \
  -e TELEGRAM_BOT_TOKEN="$TELEGRAM_BOT_TOKEN" \
  -e TELEGRAM_CHAT_ID="$TELEGRAM_CHAT_ID" \
  -e PITCHAI_CODEX_MODEL="gpt-5.2-medium" \
  pitchai/codex-agent:local \
  /opt/pitchai/run_elise_job.py
```

Confirm:

- You see `{"type":"thread.started","thread_id":"..."}` in stdout.
- `/tmp/elise_mount/state.json` contains that `thread_id`.
- `/tmp/elise_mount/elise/ledger.md` is created/updated.

### Azure: trigger + follow logs

Start a manual execution:

```bash
EXEC=$(az containerapp job start -g "$RG" -n elise-agent-job --query name -o tsv)
REPL=$(az containerapp job replica list -g "$RG" -n elise-agent-job --execution "$EXEC" --query '[0].name' -o tsv)
az containerapp job logs show -g "$RG" -n elise-agent-job --execution "$EXEC" --replica "$REPL" --container elise-agent --follow true --format text
```

Verify the execution succeeded:

```bash
az containerapp job execution list -g "$RG" -n elise-agent-job -o table
```

Verify resume works:

- Trigger the job twice; confirm `thread_id` stays the same across runs and `state.json` remains stable.

## Troubleshooting

### Graph auth failures

- `invalid_client` / `AADSTS...`: wrong tenant, wrong client id, cert not uploaded, or public cert doesn’t match the private key.
- `403 Forbidden`: missing `Mail.ReadWrite`/`Mail.Send` (Application), or admin consent not granted.
- Mailbox not found: wrong mailbox tenant or wrong `PITCHAI_GRAPH_MAILBOX_UPN`.

### ACA log retrieval: “No replicas found”

The `az containerapp job logs show` command needs an execution + replica if the default execution’s pods are already cleaned up:

```bash
az containerapp job replica list -g "$RG" -n elise-agent-job --execution "$EXEC"
```

### Azure Files + chmod problems

Azure Files mounts (CIFS/SMB) don’t always support unix permissions. This repo includes a fix to **ignore `chmod` PermissionDenied** when writing certain files so `$CODEX_HOME` can live on Azure Files.

If you rebase onto upstream or port to another environment, re-verify this behavior.

## Security notes (recommended follow-ups)

- Restrict the Graph app-only principal to Elise’s mailbox (Exchange Application Access Policy).
- Reduce sensitive content in logs/telegrams: avoid echoing raw message bodies in final responses.
- Consider using Key Vault references for secrets instead of raw ACA secrets.
