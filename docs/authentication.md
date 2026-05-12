# Authentication

For information about Codex CLI authentication, see [this documentation](https://developers.openai.com/codex/auth).

## PitchAI Auth Policy

- Default path: shared managed auth in `$CODEX_HOME/auth.json` / keyring.
- In automation, that managed auth is expected to be broker-issued from the auth-token server.
- `CODEX_API_KEY` is not an implicit runtime fallback.
- API-key auth is explicit-only and should be treated as break-glass behavior.

## Broker Auth (Automation)

PitchAI automation runners (for example `run_codex_job.py`) and `codex-dev` use:

- `CODEX_AUTH_BROKER_URL`
- `CODEX_AUTH_BROKER_TOKEN`
- `CODEX_AUTH_BROKER_ROTATION_MAX_ATTEMPTS` (optional; `codex-dev` default is `64` from the wrapper)

to acquire a lease-scoped `auth.json`, run Codex, and report lease outcomes back to the broker.

## Usage-Limit Recovery

When broker mode is enabled and a usage/rate-limit, quota, unauthorized, or refresh-token outcome is detected, Codex:

1. Reports the outcome (`usage_limit_reached`) for the current lease.
2. Acquires a fresh lease (new auth payload).
3. Rewrites `$CODEX_HOME/auth.json`.
4. Auto-continues the same conversation within the current turn.

`codex-dev login` imports the newly saved ChatGPT `auth.json` into the broker when the wrapper can read the local auth-token-server admin token. The Rust login path performs the import during token persistence, and the `codex-dev` wrapper performs a second post-login import from `$CODEX_HOME/auth.json` so browser/device login exits cannot silently leave the broker stale.
