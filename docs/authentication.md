# Authentication

For information about Codex CLI authentication, see [this documentation](https://developers.openai.com/codex/auth).

## PitchAI Auth Policy

- Default path: shared managed auth in `$CODEX_HOME/auth.json` / keyring.
- In automation, that managed auth is expected to be broker-issued from the auth-token server.
- `CODEX_API_KEY` is not an implicit runtime fallback.
- API-key auth is explicit-only and should be treated as break-glass behavior.

## Broker Auth (Automation)

PitchAI automation runners (for example `run_codex_job.py`) use:

- `CODEX_AUTH_BROKER_URL`
- `CODEX_AUTH_BROKER_TOKEN`

to acquire a lease-scoped `auth.json`, run Codex, and report lease outcomes back to the broker.

## Usage-Limit Recovery

When broker mode is enabled and a usage/rate-limit outcome is detected, the runner:

1. Reports the outcome (`usage_limit_reached`) for the current lease.
2. Acquires a fresh lease (new auth payload).
3. Rewrites `$CODEX_HOME/auth.json`.
4. Auto-continues the same conversation (bounded retries with backoff).
