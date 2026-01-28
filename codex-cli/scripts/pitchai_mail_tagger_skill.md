# PitchAI Mailbox Tagger — Skill / Credentials Reference

This document is a quick reference for the scheduled “Mailbox Tagger” Codex agent.

## Mailbox access (Microsoft Graph, app-only, certificate)

This agent uses Microsoft Graph with app-only (client credentials) auth.

Required env vars (non-secret unless noted):

- `PITCHAI_GRAPH_TENANT_ID` (tenant id or domain, e.g. `pitchai1.onmicrosoft.com`)
- `PITCHAI_GRAPH_CLIENT_ID` (Entra app registration client id)
- `PITCHAI_GRAPH_MAILBOX_UPN` (mailbox to operate on, e.g. `seth.vanderbijl@pitchai.net`)

Secrets (inject via ACA Job secrets; never hardcode):

- `PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64` (base64 of PEM private key)
- `PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64` (base64 of PEM public cert)

Required Graph Application permissions (admin consent):

- `Mail.ReadWrite`

Notes:
- This job modifies message categories and read/unread state.
- Categories are plain strings; Outlook may show them without a color unless they exist in the master category list.

## Project management DB (PitchAI PM DB)

This agent writes tasks/features into the shared Postgres database.

Secrets/env vars (inject via ACA Job secrets):

- `PITCHAI_PM_DB_URL` (recommended) — full Postgres DSN, e.g. `postgresql://user:pass@host:5432/dbname`

OR, if you prefer split env vars:

- `PITCHAI_PM_DB_HOST`
- `PITCHAI_PM_DB_PORT`
- `PITCHAI_PM_DB_NAME`
- `PITCHAI_PM_DB_USER`
- `PITCHAI_PM_DB_PASS`

The agent uses custom tools (`pm_search_projects`, `pm_create_task`, `pm_create_feature`) and should always attach source email metadata to support idempotency.

## Operational notes

- This job processes only **unread + untagged** messages, up to 15 per run.
- Invoices + newsletters are marked **Read** after tagging.
- Client/project/partner/personal messages stay **Unread**.

