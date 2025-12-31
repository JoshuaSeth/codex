#!/usr/bin/env bash
set -euo pipefail

: "${PITCHAI_DISPATCH_TOKEN:?Missing PITCHAI_DISPATCH_TOKEN}"

export PITCHAI_HOST_QUEUE_DIR="${PITCHAI_HOST_QUEUE_DIR:-./_local_queue}"
export PITCHAI_RUNNER_IMAGE="${PITCHAI_RUNNER_IMAGE:-registry.pitchai.net:5000/pitchai/codex-runner:latest}"
export PITCHAI_RUNNER_ENV_JSON="${PITCHAI_RUNNER_ENV_JSON:-{}}"
export PITCHAI_DOCKER_HOST_VOLUME_ROOT_DIR="${PITCHAI_DOCKER_HOST_VOLUME_ROOT_DIR:-$(python3 - <<'PY'\nfrom pathlib import Path\nprint(str((Path.cwd() / '_local_volume_root').resolve()))\nPY\n)}"

python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt

uvicorn app:app --host 127.0.0.1 --port 8129 --reload
