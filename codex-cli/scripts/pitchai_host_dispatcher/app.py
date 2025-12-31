#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import re
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional
from uuid import uuid4

import docker
from fastapi import FastAPI, Header, HTTPException, Request
from fastapi.responses import HTMLResponse, PlainTextResponse


@dataclass(frozen=True)
class Settings:
    dispatch_token: str
    queue_dir: Path
    runs_dir: Path
    docker_host_volume_root_dir: Path
    runner_image: str
    runner_max_items_per_run: int
    runner_env: dict[str, str]
    runner_name_prefix: str


def _require_env(name: str) -> str:
    value = (os.getenv(name) or "").strip()
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def _optional_env(name: str) -> Optional[str]:
    value = (os.getenv(name) or "").strip()
    return value or None


def _now_utc_compact() -> str:
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


def _sanitize_filename(value: str) -> str:
    value = value.strip()
    value = re.sub(r"[^a-zA-Z0-9_.-]+", "_", value)
    return value[:120] if value else "job"


def _load_settings() -> Settings:
    queue_dir = Path(os.getenv("PITCHAI_HOST_QUEUE_DIR", "/data/queue"))
    runs_dir = Path(os.getenv("PITCHAI_HOST_RUNS_DIR", "/data/runs"))

    docker_host_volume_root_dir = Path(_require_env("PITCHAI_DOCKER_HOST_VOLUME_ROOT_DIR"))

    runner_image = os.getenv("PITCHAI_RUNNER_IMAGE", "registry.pitchai.net:5000/pitchai/codex-runner:latest").strip()
    runner_max_items_per_run = int(os.getenv("PITCHAI_MAX_ITEMS_PER_RUN", "10") or "10")
    runner_max_items_per_run = max(1, min(runner_max_items_per_run, 50))

    runner_env_json = _optional_env("PITCHAI_RUNNER_ENV_JSON")
    runner_env: dict[str, str] = {}
    if runner_env_json:
        try:
            parsed = json.loads(runner_env_json)
            if isinstance(parsed, dict):
                runner_env = {str(k): str(v) for k, v in parsed.items() if str(k)}
        except Exception:
            runner_env = {}

    # Default runner env: point it at the host-mounted queue + volume root.
    runner_env.setdefault("PITCHAI_VOLUME_ROOT", "/mnt/elise")
    runner_env.setdefault("PITCHAI_PROMPT_QUEUE_DIR", "/mnt/elise/prompts/http")
    runner_env.setdefault("PITCHAI_MAX_ITEMS_PER_RUN", str(runner_max_items_per_run))
    runner_env.setdefault("CODEX_HOME", "/mnt/elise/codex_home")
    runner_env.setdefault("PITCHAI_WORKDIR", "/mnt/elise/workdir")
    runner_env.setdefault("PYTHONUNBUFFERED", "1")

    return Settings(
        dispatch_token=_require_env("PITCHAI_DISPATCH_TOKEN"),
        queue_dir=queue_dir,
        runs_dir=runs_dir,
        docker_host_volume_root_dir=docker_host_volume_root_dir,
        runner_image=runner_image,
        runner_max_items_per_run=runner_max_items_per_run,
        runner_env=runner_env,
        runner_name_prefix=os.getenv("PITCHAI_RUNNER_NAME_PREFIX", "pitchai-codex-runner").strip() or "pitchai-codex-runner",
    )


def _parse_dispatch_request(payload: Any) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise HTTPException(status_code=400, detail="invalid json body")

    prompt = payload.get("prompt")
    config_toml = payload.get("config_toml")
    if not isinstance(prompt, str) or not prompt.strip():
        raise HTTPException(status_code=400, detail="missing prompt")
    if not isinstance(config_toml, str) or not config_toml.strip():
        raise HTTPException(status_code=400, detail="missing config_toml")

    state_key = payload.get("state_key")
    workdir_rel = payload.get("workdir_rel")
    model = payload.get("model")
    conversation_id = payload.get("conversation_id")
    fork = payload.get("fork", False)
    pre_commands = payload.get("pre_commands", [])
    post_commands = payload.get("post_commands", [])
    git_repo = payload.get("git_repo")
    git_branch = payload.get("git_branch")
    git_base = payload.get("git_base")
    git_clone_dir_rel = payload.get("git_clone_dir_rel")

    if state_key is not None and not isinstance(state_key, str):
        raise HTTPException(status_code=400, detail="state_key must be string")
    if workdir_rel is not None and not isinstance(workdir_rel, str):
        raise HTTPException(status_code=400, detail="workdir_rel must be string")
    if model is not None and not isinstance(model, str):
        raise HTTPException(status_code=400, detail="model must be string")
    if conversation_id is not None and not isinstance(conversation_id, str):
        raise HTTPException(status_code=400, detail="conversation_id must be string")
    if fork is not None and not isinstance(fork, bool):
        raise HTTPException(status_code=400, detail="fork must be boolean")
    if pre_commands is not None and not isinstance(pre_commands, list):
        raise HTTPException(status_code=400, detail="pre_commands must be list of strings")
    if post_commands is not None and not isinstance(post_commands, list):
        raise HTTPException(status_code=400, detail="post_commands must be list of strings")
    if isinstance(pre_commands, list) and any((not isinstance(c, str)) for c in pre_commands):
        raise HTTPException(status_code=400, detail="pre_commands must be list of strings")
    if isinstance(post_commands, list) and any((not isinstance(c, str)) for c in post_commands):
        raise HTTPException(status_code=400, detail="post_commands must be list of strings")
    if git_repo is not None and not isinstance(git_repo, str):
        raise HTTPException(status_code=400, detail="git_repo must be string")
    if git_branch is not None and not isinstance(git_branch, str):
        raise HTTPException(status_code=400, detail="git_branch must be string")
    if git_base is not None and not isinstance(git_base, str):
        raise HTTPException(status_code=400, detail="git_base must be string")
    if git_clone_dir_rel is not None and not isinstance(git_clone_dir_rel, str):
        raise HTTPException(status_code=400, detail="git_clone_dir_rel must be string")

    return {
        "prompt": prompt.strip(),
        "config_toml": config_toml.strip(),
        "state_key": state_key.strip() if isinstance(state_key, str) and state_key.strip() else None,
        "workdir_rel": workdir_rel.strip() if isinstance(workdir_rel, str) and workdir_rel.strip() else None,
        "model": model.strip() if isinstance(model, str) and model.strip() else None,
        "conversation_id": conversation_id.strip() if isinstance(conversation_id, str) and conversation_id.strip() else None,
        "fork": bool(fork) if isinstance(fork, bool) else False,
        "pre_commands": [c.strip() for c in pre_commands if isinstance(c, str) and c.strip()]
        if isinstance(pre_commands, list)
        else [],
        "post_commands": [c.strip() for c in post_commands if isinstance(c, str) and c.strip()]
        if isinstance(post_commands, list)
        else [],
        "git_repo": git_repo.strip() if isinstance(git_repo, str) and git_repo.strip() else None,
        "git_branch": git_branch.strip() if isinstance(git_branch, str) and git_branch.strip() else None,
        "git_base": git_base.strip() if isinstance(git_base, str) and git_base.strip() else None,
        "git_clone_dir_rel": git_clone_dir_rel.strip()
        if isinstance(git_clone_dir_rel, str) and git_clone_dir_rel.strip()
        else None,
    }


def _write_bundle(settings: Settings, req: dict[str, Any]) -> Path:
    settings.queue_dir.mkdir(parents=True, exist_ok=True)
    (settings.queue_dir / "_processing").mkdir(parents=True, exist_ok=True)
    (settings.queue_dir / "_processed").mkdir(parents=True, exist_ok=True)
    (settings.queue_dir / "_failed").mkdir(parents=True, exist_ok=True)

    ts = _now_utc_compact()
    rid = uuid4().hex[:12]
    name = _sanitize_filename(f"{ts}_{rid}")
    bundle = settings.queue_dir / name
    bundle.mkdir(parents=False, exist_ok=False)

    (bundle / "prompt.md").write_text(req["prompt"].rstrip() + "\n", encoding="utf-8")
    (bundle / "config.toml").write_text(req["config_toml"].rstrip() + "\n", encoding="utf-8")

    meta = {
        "ts_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "state_key": req.get("state_key"),
        "workdir_rel": req.get("workdir_rel"),
        "model": req.get("model"),
        "conversation_id": req.get("conversation_id"),
        "fork": bool(req.get("fork") or False),
        "pre_commands": req.get("pre_commands") or [],
        "post_commands": req.get("post_commands") or [],
        "git_repo": req.get("git_repo"),
        "git_branch": req.get("git_branch"),
        "git_base": req.get("git_base"),
        "git_clone_dir_rel": req.get("git_clone_dir_rel"),
    }
    (bundle / "meta.json").write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    return bundle


def _spawn_runner(settings: Settings, *, bundle_name: str) -> str:
    client = docker.from_env()
    container_name = f"{settings.runner_name_prefix}-{_now_utc_compact()}-{uuid4().hex[:6]}"

    env = dict(settings.runner_env)

    # Best-effort: ensure we run the latest runner image.
    try:
        client.images.pull(settings.runner_image)
    except Exception:
        pass

    # Mount the host queue dir and codex home/workdir volumes.
    # - Queue dir: /data/queue on host -> /mnt/elise/prompts/http in runner
    # - Codex home: /data/codex_home on host -> /mnt/elise/codex_home in runner
    # - Workdir: /data/workdir on host -> /mnt/elise/workdir in runner
    host_root = settings.docker_host_volume_root_dir.resolve()
    host_root.mkdir(parents=True, exist_ok=True)
    (host_root / "prompts" / "http").mkdir(parents=True, exist_ok=True)
    (host_root / "codex_home").mkdir(parents=True, exist_ok=True)
    (host_root / "workdir").mkdir(parents=True, exist_ok=True)

    volumes = {str(host_root): {"bind": "/mnt/elise", "mode": "rw"}}

    # Run the generic runner script from the codex runner image and persist logs
    # into the mounted volume so we can debug runs even after the container is removed.
    log_path = f"/mnt/elise/runs/{bundle_name}.log"
    cmd = ["sh", "-c", f"python3 /opt/pitchai/run_codex_job.py > {log_path} 2>&1"]

    container = client.containers.run(
        image=settings.runner_image,
        name=container_name,
        command=cmd,
        environment=env,
        volumes=volumes,
        user="0:0",
        detach=True,
        remove=True,
        labels={"pitchai.kind": "codex-runner", "pitchai.bundle": bundle_name},
    )

    settings.runs_dir.mkdir(parents=True, exist_ok=True)
    record = {
        "ts_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "bundle": bundle_name,
        "container": container.name,
        "image": settings.runner_image,
        "log_path": log_path,
    }
    (settings.runs_dir / f"{bundle_name}.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    return container.name


def _has_running_runner(settings: Settings) -> bool:
    client = docker.from_env()
    containers = client.containers.list(
        all=False, filters={"label": "pitchai.kind=codex-runner"}
    )
    return bool(containers)


def _list_bundles(queue_dir: Path) -> dict[str, list[str]]:
    def _names(path: Path) -> list[str]:
        if not path.exists():
            return []
        out: list[str] = []
        for p in sorted(path.iterdir(), reverse=True):
            if p.is_dir() and not p.name.startswith("_"):
                out.append(p.name)
        return out[:50]

    return {
        "queued": _names(queue_dir),
        "processing": _names(queue_dir / "_processing"),
        "processed": _names(queue_dir / "_processed"),
        "failed": _names(queue_dir / "_failed"),
    }

def _list_runs(runs_dir: Path) -> list[dict[str, Any]]:
    if not runs_dir.exists():
        return []
    runs: list[dict[str, Any]] = []
    for p in sorted(runs_dir.glob("*.json"), reverse=True)[:50]:
        try:
            data = json.loads(p.read_text(encoding="utf-8"))
            if isinstance(data, dict):
                runs.append(data)
        except Exception:
            continue
    return runs


SETTINGS = _load_settings()
app = FastAPI()


@app.get("/healthz", response_class=PlainTextResponse)
def healthz() -> str:
    return "ok"


@app.get("/ui", response_class=HTMLResponse)
def ui() -> str:
    bundles = _list_bundles(SETTINGS.queue_dir)
    runs = _list_runs(SETTINGS.runs_dir)
    rows = []
    for key in ("queued", "processing", "processed", "failed"):
        items = bundles.get(key, [])
        rows.append(f"<h3>{key} ({len(items)})</h3><pre>{json.dumps(items, indent=2)}</pre>")
    rows.append(f"<h3>runs ({len(runs)})</h3><pre>{json.dumps(runs, indent=2)}</pre>")
    return (
        "<html><head><title>PitchAI Codex Dispatcher</title></head><body>"
        "<h2>PitchAI Codex Dispatcher</h2>"
        + "".join(rows)
        + "</body></html>"
    )


@app.post("/dispatch", response_class=PlainTextResponse)
async def dispatch(
    request: Request,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
) -> str:
    got = (x_pitchai_dispatch_token or "").strip()
    if got != SETTINGS.dispatch_token:
        raise HTTPException(status_code=401, detail="invalid dispatch token")

    try:
        payload = await request.json()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=400, detail=f"invalid json: {exc}") from exc

    req = _parse_dispatch_request(payload)
    bundle = _write_bundle(SETTINGS, req)

    # Avoid spawning multiple concurrent runner containers. One runner can drain
    # multiple queued items per run, and concurrent runs contend on the same
    # Codex home/state volume and lock.
    try:
        if _has_running_runner(SETTINGS):
            return f"queued:{bundle.name}:runner:already_running"
    except Exception:
        # Best-effort only; if we can't check, we still try to start.
        pass

    try:
        container_name = _spawn_runner(SETTINGS, bundle_name=bundle.name)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"failed to start runner: {exc}") from exc

    return f"queued:{bundle.name}:runner:{container_name}"
