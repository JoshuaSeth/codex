# Generic Codex Job Image (PitchAI) — Run Any Config/Prompt

This repo ships a Docker image that contains the `codex` CLI plus PitchAI helper scripts. The key idea is: **the image is generic**, and you choose what agent runs by setting environment variables to point at a specific Codex config file and prompt file.

## The generic runner

The image includes `/opt/pitchai/run_codex_job.py` (source: `codex-cli/scripts/pitchai_run_codex_job.py`).

It:

- Decodes `CODEX_AUTH_JSON_B64` into `$CODEX_HOME/auth.json`
- Runs `codex exec` with:
  - `--config-file $PITCHAI_CODEX_CONFIG_PATH`
  - prompt read from `$PITCHAI_PROMPT_PATH`
  - persistent working directory `$PITCHAI_WORKDIR`
- Persists a `thread_id` in a state file so the next run resumes the same session.
- Optionally consumes a prompt from a shared “prompt queue” directory (useful for webhook dispatch).
- Uses a CIFS-safe directory lock on the mounted volume to prevent concurrent runs from corrupting state.

## Environment variables (what you configure)

Required:

- `CODEX_AUTH_JSON_B64` — base64 of Codex `auth.json` for the container.

Common:

- `PITCHAI_CODEX_CONFIG_PATH` — path to the Codex config TOML to use.
  - Example: `/opt/pitchai/elise_config.toml`
  - You can also mount a config on a volume and point to it, e.g. `/mnt/elise/configs/my_agent.toml`
- `PITCHAI_PROMPT_PATH` — path to the prompt file to feed into `codex exec`.
  - Example: `/opt/pitchai/elise_prompt.md`
- `PITCHAI_WORKDIR` — working directory passed to `codex exec --cd` (where files/ledger live).
- `PITCHAI_VOLUME_ROOT` — root of the persistent mount (defaults to `/mnt/elise`).
- `PITCHAI_STATE_DIR` / `PITCHAI_STATE_PATH` — where to persist the `thread_id`.
  - Default is `/mnt/elise/state_<hash>.json` (hash derived from the config path).

Prompt queue (optional):

- `PITCHAI_PROMPT_QUEUE_DIR` — directory of queued `.md`/`.txt` prompts to consume (one per run).
- `PITCHAI_PROMPT_OVERRIDE` — if set, overrides both queue + file prompt for this run.

Model mapping (optional):

- `PITCHAI_CODEX_MODEL`
  - `gpt-5.2-high` → `-m gpt-5.2-codex -c model_reasoning_effort=high`
  - `gpt-5.2-medium` → `-m gpt-5.2-codex -c model_reasoning_effort=medium`

## Azure Container Apps Job pattern

You typically create **one ACA Job per agent**, but they can all share the same image. The only differences are:

- schedule (cron)
- env vars (`PITCHAI_CODEX_CONFIG_PATH`, `PITCHAI_PROMPT_PATH`, `PITCHAI_WORKDIR`, tool creds)
- volume mount(s)

In an ACA Job, set the container command to:

- `command: ["/opt/pitchai/run_codex_job.py"]`

And mount an Azure Files share to persist:

- `$CODEX_HOME`
- `$PITCHAI_WORKDIR`
- state file(s)
- optional prompt queue directory

## Example: run Elise with the generic runner

Set:

- `PITCHAI_CODEX_CONFIG_PATH=/opt/pitchai/elise_config.toml`
- `PITCHAI_PROMPT_PATH=/opt/pitchai/elise_prompt.md`
- `PITCHAI_WORKDIR=/mnt/elise/elise`
- `PITCHAI_PROMPT_QUEUE_DIR=/mnt/elise/prompts/telegram`

And provide Graph + Telegram env vars as described in `docs/pitchai_elise_agent.md`.

