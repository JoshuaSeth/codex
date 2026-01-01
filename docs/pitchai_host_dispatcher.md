# PitchAI Codex Host Dispatcher (prod server)

This document describes how `dispatch.pitchai.net` works and how to deploy/operate it on the production host (`root@37.27.67.52`).

## What it is

- A small **FastAPI** service (the “dispatcher”) that accepts an authenticated **POST** request.
- It writes a **bundle directory** containing:
  - `prompt.md`
  - `config.toml`
  - `meta.json` (optional `conversation_id`, `fork`, `state_key`, `workdir_rel`, `model`)
- It then starts a **Codex runner container** that drains queued bundles and executes `codex exec ...`.
- The runner persists logs to `/mnt/elise/runs/<bundle>.log` inside the shared volume (host: `/root/pitchai-codex-dispatcher/data/runs/`).

## HTTP API

### `POST /dispatch`

Headers:
- `X-PitchAI-Dispatch-Token: <token>`

JSON body:
```json
{
  "prompt": "string (required)",
  "config_toml": "string (required)",
  "state_key": "string (optional)",
  "workdir_rel": "string (optional)",
  "model": "string (optional, e.g. gpt-5.2-medium or gpt-5.2-high)",
  "conversation_id": "string (optional UUID; resume this conversation)",
  "fork": true,
  "pre_commands": ["string shell command", "…"],
  "post_commands": ["string shell command", "…"],

  "git_repo": "https://github.com/org/repo.git",
  "git_branch": "pitchai-work-<id>",
  "git_base": "main",
  "git_clone_dir_rel": "repo"
}
```

Response:
- `queued:<bundle>:runner:<container_or_already_running>`

Notes:
- If a Codex runner is already running, dispatcher returns `runner:already_running` and does **not** spawn another container.

### `GET /healthz`
- Returns `ok`

### `GET /ui`
- Simple HTML view of queued/processed/failed bundles + recent runs.

## Polling / live status API

All endpoints below require the same header as `/dispatch`:
- `X-PitchAI-Dispatch-Token: <token>`

### `GET /runs`
- Returns recent run records (JSON).

### `GET /runs/<bundle>/record`
- Returns the stored run record for that bundle.

### `GET /runs/<bundle>/log?offset=0&max_bytes=20000`
- Offset-based tail of the growing runner log at `data/runs/<bundle>.log`.
- Response includes:
  - `offset`, `next_offset`, `size`, `eof`, `content`

### `GET /runs/<bundle>/events?offset=0&max_bytes=20000`
- Same as `/log`, but also includes `events` parsed from JSON lines in the log (Codex JSONL events).

### `GET /runs/<bundle>/rollout?offset=0&max_bytes=20000`
- Offset-based tail of the Codex rollout JSONL (once the run has emitted `thread.started`).
- Response includes `thread_id` and `rollout_path`.

## Codex runner behavior

The runner is `registry.pitchai.net:5000/pitchai/codex-runner:latest` and runs:
- `python3 /opt/pitchai/run_codex_job.py`

Key behaviors:
- Drains up to `PITCHAI_MAX_ITEMS_PER_RUN` bundles per run.
- Uses `meta.json` fields:
  - `model` to override model
  - `state_key` to isolate state per workflow
  - `conversation_id` to resume a specific session id
  - `fork=true` to call `codex exec resume <id> --fork` (preserves original rollout)
  - `git_repo` + `git_branch` (+ optional `git_base`) to clone + branch before running Codex
  - `pre_commands` and `post_commands` to run arbitrary shell commands in the final working directory

### Git clone + branch

If `git_repo` and `git_branch` are set, the runner will:
- Clone the repo into `<workdir>/<git_clone_dir_rel>` (default `repo`)
- Create/reset local branch `git_branch` from `origin/<git_base>` (default `main`)
- Run Codex + hooks inside the cloned repo directory

Credentials:
- For private HTTPS repos, set `PITCHAI_GIT_TOKEN` in runner env.
- The token is used via `GIT_ASKPASS` (not embedded in the git command line).

## `--fork` semantics

`codex exec resume <conversation_id> --fork`:
- Creates a **new rollout file** with a new conversation id.
- Copies the original history, then appends the new prompt/turn to the fork.
- Leaves the original `.jsonl` rollout unchanged.

This is implemented in:
- `codex/codex-rs/exec/src/cli.rs` (`--fork`)
- `codex/codex-rs/exec/src/lib.rs` (rollout forking)

## Production deployment (Hetzner host)

### 1) Files/dirs on host

Installed to:
- `/root/pitchai-codex-dispatcher/`

Contains:
- `/root/pitchai-codex-dispatcher/docker-compose.yml`
- `/root/pitchai-codex-dispatcher/.env`
- `/root/pitchai-codex-dispatcher/data/` (persistent volume root)
  - `prompts/http/` (queue)
  - `runs/` (runner logs + run records)
  - `codex_home/` (Codex sessions + `auth.json`)
  - `workdir/` (workspace)
  - `locks/` (runner lock directory)

### 2) `.env` variables

`/root/pitchai-codex-dispatcher/.env` must contain:
- `PITCHAI_DISPATCH_TOKEN=...` (used by `X-PitchAI-Dispatch-Token`)
- `PITCHAI_DOCKER_HOST_VOLUME_ROOT_DIR=/root/pitchai-codex-dispatcher/data`
- `PITCHAI_RUNNER_IMAGE=registry.pitchai.net:5000/pitchai/codex-runner:latest`
- `PITCHAI_MAX_ITEMS_PER_RUN=10`
- `PITCHAI_RUNNER_ENV_JSON={}` (can inject extra env into runner)

Credentials:
- Codex auth is stored at `/root/.codex/auth.json`
- The deployment copies it to `/root/pitchai-codex-dispatcher/data/codex_home/auth.json`

### 3) Start / update

```bash
cd /root/pitchai-codex-dispatcher
docker compose pull
docker compose up -d
docker compose ps
```

### 4) Nginx + HTTPS

Nginx site:
- `/etc/nginx/sites-available/dispatch.pitchai.net`
- symlinked in `/etc/nginx/sites-enabled/dispatch.pitchai.net`

HTTPS certificate is managed by certbot:
```bash
certbot --nginx -d dispatch.pitchai.net --redirect
```

## Observability / debugging

- Dispatcher logs:
  - `docker logs -f pitchai-codex-dispatcher`
- Runner logs (persisted even after container removal):
  - `/root/pitchai-codex-dispatcher/data/runs/<bundle>.log`
- Bundles:
  - queued: `/root/pitchai-codex-dispatcher/data/prompts/http/`
  - processed: `/root/pitchai-codex-dispatcher/data/prompts/http/_processed/`
  - failed: `/root/pitchai-codex-dispatcher/data/prompts/http/_failed/`

## Local dev

Folder:
- `codex/codex-cli/scripts/pitchai_host_dispatcher/`

Quick start:
```bash
cd codex/codex-cli/scripts/pitchai_host_dispatcher
export PITCHAI_DISPATCH_TOKEN=devtoken
./start_server.sh
```

Or build/run via Docker:
```bash
docker build -t pitchai/codex-dispatcher:dev .
```
