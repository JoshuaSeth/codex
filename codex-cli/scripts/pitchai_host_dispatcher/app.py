#!/usr/bin/env python3
from __future__ import annotations

import base64
import html
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
from fastapi.responses import JSONResponse
from fastapi.responses import HTMLResponse, PlainTextResponse


@dataclass(frozen=True)
class Settings:
    dispatch_token: str
    queue_dir: Path
    runs_dir: Path
    data_root: Path
    docker_host_volume_root_dir: Path
    runner_image: str
    runner_max_items_per_run: int
    runner_env: dict[str, str]
    runner_name_prefix: str
    ui_basic_user: Optional[str]
    ui_basic_pass: Optional[str]


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
    data_root = runs_dir.parent

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
        data_root=data_root,
        docker_host_volume_root_dir=docker_host_volume_root_dir,
        runner_image=runner_image,
        runner_max_items_per_run=runner_max_items_per_run,
        runner_env=runner_env,
        runner_name_prefix=os.getenv("PITCHAI_RUNNER_NAME_PREFIX", "pitchai-codex-runner").strip() or "pitchai-codex-runner",
        ui_basic_user=_optional_env("PITCHAI_UI_BASIC_USER"),
        ui_basic_pass=_optional_env("PITCHAI_UI_BASIC_PASS"),
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

def _auth_or_401(x_pitchai_dispatch_token: Optional[str]) -> None:
    got = (x_pitchai_dispatch_token or "").strip()
    if got != SETTINGS.dispatch_token:
        raise HTTPException(status_code=401, detail="invalid dispatch token")

def _parse_basic_auth(authorization: Optional[str]) -> Optional[tuple[str, str]]:
    if not authorization:
        return None
    s = authorization.strip()
    if not s.lower().startswith("basic "):
        return None
    token = s.split(" ", 1)[1].strip()
    if not token:
        return None
    try:
        raw = base64.b64decode(token).decode("utf-8", errors="replace")
    except Exception:
        return None
    if ":" not in raw:
        return None
    user, pwd = raw.split(":", 1)
    return user, pwd


def _auth_any_or_401(x_pitchai_dispatch_token: Optional[str], authorization: Optional[str]) -> None:
    got = (x_pitchai_dispatch_token or "").strip()
    if got and got == SETTINGS.dispatch_token:
        return

    if SETTINGS.ui_basic_user and SETTINGS.ui_basic_pass:
        parsed = _parse_basic_auth(authorization)
        if parsed and parsed[0] == SETTINGS.ui_basic_user and parsed[1] == SETTINGS.ui_basic_pass:
            return

        raise HTTPException(
            status_code=401,
            detail="basic auth required",
            headers={"WWW-Authenticate": 'Basic realm="PitchAI Codex Dispatcher"'},
        )

    raise HTTPException(status_code=401, detail="invalid dispatch token")


def _auth_ui_if_enabled(authorization: Optional[str]) -> None:
    if SETTINGS.ui_basic_user and SETTINGS.ui_basic_pass:
        _auth_any_or_401(None, authorization)


def _read_tail(path: Path, *, offset: int, max_bytes: int) -> dict[str, Any]:
    if offset < 0:
        offset = 0
    max_bytes = max(1, min(int(max_bytes), 200_000))

    if not path.exists():
        return {"exists": False, "offset": offset, "next_offset": offset, "size": 0, "eof": True, "content": ""}

    data = path.read_bytes()
    size = len(data)
    if offset > size:
        offset = size
    chunk = data[offset : offset + max_bytes]
    next_offset = offset + len(chunk)
    try:
        text = chunk.decode("utf-8", errors="replace")
    except Exception:
        text = ""
    return {
        "exists": True,
        "offset": offset,
        "next_offset": next_offset,
        "size": size,
        "eof": next_offset >= size,
        "content": text,
    }

def _read_last_bytes(path: Path, *, max_bytes: int) -> bytes:
    max_bytes = max(1, min(int(max_bytes), 200_000))
    if not path.exists():
        return b""
    data = path.read_bytes()
    if len(data) <= max_bytes:
        return data
    return data[-max_bytes:]


def _extract_last_json_line(text: str) -> Optional[dict[str, Any]]:
    for line in reversed(text.splitlines()):
        s = line.strip()
        if not s.startswith("{"):
            continue
        try:
            obj = json.loads(s)
        except Exception:
            continue
        if isinstance(obj, dict):
            return obj
    return None


def _run_record_path(bundle: str) -> Path:
    safe = _sanitize_filename(bundle)
    return SETTINGS.runs_dir / f"{safe}.json"


def _run_log_path(bundle: str) -> Path:
    safe = _sanitize_filename(bundle)
    return SETTINGS.runs_dir / f"{safe}.log"


def _extract_thread_id_from_log_text(text: str) -> Optional[str]:
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            evt = json.loads(line)
        except Exception:
            continue
        if isinstance(evt, dict) and evt.get("type") == "thread.started":
            tid = evt.get("thread_id")
            if isinstance(tid, str) and tid.strip():
                return tid.strip()
    return None


def _load_run_record(bundle: str) -> dict[str, Any]:
    record_path = _run_record_path(bundle)
    if not record_path.exists():
        raise HTTPException(status_code=404, detail="unknown bundle")
    try:
        data = json.loads(record_path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            raise ValueError("record must be dict")
    except Exception as exc:
        raise HTTPException(status_code=500, detail=f"invalid record: {exc}") from exc
    return data


def _get_thread_id_for_bundle(bundle: str) -> Optional[str]:
    record = _load_run_record(bundle)
    tid = record.get("thread_id")
    if isinstance(tid, str) and tid.strip():
        return tid.strip()

    # Best-effort: parse from log tail if available and persist back to record.
    log_path = _run_log_path(bundle)
    if not log_path.exists():
        return None
    text = log_path.read_text(encoding="utf-8", errors="replace")
    tid2 = _extract_thread_id_from_log_text(text)
    if not tid2:
        return None
    try:
        record["thread_id"] = tid2
        record_path = _run_record_path(bundle)
        record_path.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    except Exception:
        pass
    return tid2


def _find_rollout_for_thread_id(thread_id: str) -> Optional[Path]:
    # Read from the dispatcher container's mounted data volume (e.g. /data).
    sessions_dir = SETTINGS.data_root / "codex_home" / "sessions"
    if not sessions_dir.exists():
        return None
    # Filename convention includes the conversation id at the end.
    matches = list(sessions_dir.rglob(f"*{thread_id}.jsonl"))
    if not matches:
        return None
    # Prefer newest by mtime.
    return max(matches, key=lambda p: p.stat().st_mtime)


@app.get("/healthz", response_class=PlainTextResponse)
def healthz() -> str:
    return "ok"


@app.get("/ui", response_class=HTMLResponse)
def ui(authorization: Optional[str] = Header(default=None)) -> str:
    _auth_ui_if_enabled(authorization)
    bundles = _list_bundles(SETTINGS.queue_dir)
    runs = _list_runs(SETTINGS.runs_dir)

    running_containers: list[dict[str, Any]] = []
    try:
        client = docker.from_env()
        containers = client.containers.list(all=False, filters={"label": "pitchai.kind=codex-runner"})
        for c in containers:
            try:
                running_containers.append(
                    {
                        "name": c.name,
                        "status": c.status,
                        "labels": dict(getattr(c, "labels", {}) or {}),
                        "id": c.short_id,
                    }
                )
            except Exception:
                continue
    except Exception:
        pass

    def _run_link(run: dict[str, Any]) -> str:
        bundle = str(run.get("bundle") or "").strip()
        if not bundle:
            return html.escape(json.dumps(run))
        safe_bundle = html.escape(bundle)
        return f'<a href="/ui/runs/{safe_bundle}">{safe_bundle}</a>'

    rows = []
    rows.append(f"<h3>running ({len(running_containers)})</h3><pre>{html.escape(json.dumps(running_containers, indent=2))}</pre>")
    for key in ("queued", "processing", "processed", "failed"):
        items = bundles.get(key, [])
        links = [f'<a href="/ui/runs/{html.escape(name)}">{html.escape(name)}</a>' for name in items]
        rows.append(f"<h3>{key} ({len(items)})</h3><div>{' '.join(links) if links else '<em>none</em>'}</div>")
    rows.append(
        "<h3>runs</h3>"
        + "<ol>"
        + "".join([f"<li>{_run_link(r)}</li>" for r in runs])
        + "</ol>"
    )

    return (
        "<html><head><title>PitchAI Codex Dispatcher</title></head><body>"
        "<h2>PitchAI Codex Dispatcher</h2>"
        "<p>Polling API: <code>GET /runs</code>, <code>GET /runs/&lt;bundle&gt;/events</code>, <code>GET /runs/&lt;bundle&gt;/rollout</code></p>"
        "<p>This UI can use either browser Basic Auth (recommended) or a dispatch token saved in localStorage.</p>"
        "<label>Dispatch token: <input id='token' type='password' size='64' /></label> "
        "<button onclick='window.__saveToken()'>Save</button> "
        "<button onclick='window.__clearToken()'>Clear</button>"
        "<script>\n"
        "window.__saveToken = () => { localStorage.setItem('pitchai_dispatch_token', document.getElementById('token').value || ''); };\n"
        "window.__clearToken = () => { localStorage.removeItem('pitchai_dispatch_token'); document.getElementById('token').value=''; };\n"
        "window.addEventListener('load', () => { document.getElementById('token').value = localStorage.getItem('pitchai_dispatch_token') || ''; });\n"
        "</script>"
        + "".join(rows)
        + "</body></html>"
    )

@app.get("/ui/runs/{bundle}", response_class=HTMLResponse)
def ui_run(bundle: str, authorization: Optional[str] = Header(default=None)) -> str:
    _auth_ui_if_enabled(authorization)
    safe_bundle = html.escape(bundle)
    return (
        "<html><head><title>PitchAI Codex Run</title>"
        "<style>body{font-family:ui-sans-serif,system-ui,Segoe UI,Roboto,Arial;max-width:1100px;margin:16px auto;padding:0 12px} "
        "pre{background:#111;color:#eee;padding:12px;overflow:auto;border-radius:8px;white-space:pre-wrap} "
        ".grid{display:grid;grid-template-columns:1fr;gap:12px} "
        ".row{display:flex;gap:8px;align-items:center;flex-wrap:wrap} "
        "code{background:#f2f2f2;padding:2px 6px;border-radius:6px}</style>"
        "</head><body>"
        f"<h2>Run: <code>{safe_bundle}</code></h2>"
        "<div class='row'>"
        "<a href='/ui'>← back</a>"
        "</div>"
        "<div class='row'>"
        "<label>Dispatch token (optional if using Basic Auth): <input id='token' type='password' size='64'/></label>"
        "<button onclick='window.__saveToken()'>Save</button>"
        "<button onclick='window.__clearToken()'>Clear</button>"
        "</div>"
        "<div class='grid'>"
        "<div><h3>Record</h3><pre id='record'>(loading)</pre></div>"
        "<div><h3>Events (parsed JSON)</h3><pre id='events'>(loading)</pre></div>"
        "<div><h3>Log (raw)</h3><pre id='log'>(loading)</pre></div>"
        "<div><h3>Rollout (raw)</h3><pre id='rollout'>(loading)</pre></div>"
        "</div>"
        "<script>\n"
        "const BUNDLE = " + json.dumps(bundle) + ";\n"
        "window.__saveToken = () => { localStorage.setItem('pitchai_dispatch_token', document.getElementById('token').value || ''); };\n"
        "window.__clearToken = () => { localStorage.removeItem('pitchai_dispatch_token'); document.getElementById('token').value=''; };\n"
        "function hdrs(){ const t = localStorage.getItem('pitchai_dispatch_token') || ''; return t ? {'X-PitchAI-Dispatch-Token': t} : {}; }\n"
        "async function jget(url){ const res = await fetch(url, {headers: hdrs(), credentials: 'same-origin'}); if(!res.ok){ throw new Error(res.status + ' ' + await res.text()); } return await res.json(); }\n"
        "async function loadRecord(){ try{ const r = await jget(`/runs/${encodeURIComponent(BUNDLE)}/record`); document.getElementById('record').textContent = JSON.stringify(r, null, 2); }catch(e){ document.getElementById('record').textContent = String(e); } }\n"
        "let logOff=0, evtOff=0, rolOff=0;\n"
        "async function pollLog(){ try{ const r = await jget(`/runs/${encodeURIComponent(BUNDLE)}/log?offset=${logOff}&max_bytes=50000`); if(r && r.content){ document.getElementById('log').textContent += r.content; } logOff = r.next_offset || logOff; }catch(e){ document.getElementById('log').textContent = String(e); } }\n"
        "async function pollEvents(){ try{ const r = await jget(`/runs/${encodeURIComponent(BUNDLE)}/events?offset=${evtOff}&max_bytes=50000`); const pre = document.getElementById('events'); for(const ev of (r.events||[])){ pre.textContent += JSON.stringify(ev) + '\\n'; } evtOff = r.next_offset || evtOff; }catch(e){ document.getElementById('events').textContent = String(e); } }\n"
        "async function pollRollout(){ try{ const r = await jget(`/runs/${encodeURIComponent(BUNDLE)}/rollout?offset=${rolOff}&max_bytes=50000`); if(r && r.content){ document.getElementById('rollout').textContent += r.content; } rolOff = r.next_offset || rolOff; }catch(e){ const msg = String(e); if(msg.includes('404')){ document.getElementById('rollout').textContent = '(rollout not available yet)'; return; } document.getElementById('rollout').textContent = msg; } }\n"
        "window.addEventListener('load', () => { document.getElementById('token').value = localStorage.getItem('pitchai_dispatch_token') || ''; loadRecord(); pollEvents(); pollLog(); pollRollout(); setInterval(pollEvents, 1500); setInterval(pollLog, 1500); setInterval(pollRollout, 3000); });\n"
        "</script>"
        "</body></html>"
    )

@app.get("/runs", response_class=JSONResponse)
def list_runs(
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    return _list_runs(SETTINGS.runs_dir)


@app.get("/runs/{bundle}/record", response_class=JSONResponse)
def run_record(
    bundle: str,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    return _load_run_record(bundle)


@app.get("/runs/{bundle}/log", response_class=JSONResponse)
def run_log(
    bundle: str,
    offset: int = 0,
    max_bytes: int = 20000,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    return _read_tail(_run_log_path(bundle), offset=offset, max_bytes=max_bytes)


@app.get("/runs/{bundle}/events", response_class=JSONResponse)
def run_events(
    bundle: str,
    offset: int = 0,
    max_bytes: int = 20000,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    tail = _read_tail(_run_log_path(bundle), offset=offset, max_bytes=max_bytes)
    events: list[dict[str, Any]] = []
    for line in str(tail.get("content") or "").splitlines():
        if not line.strip().startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except Exception:
            continue
        if isinstance(obj, dict):
            events.append(obj)
    tail["events"] = events
    return tail


@app.get("/runs/{bundle}/events/latest", response_class=JSONResponse)
def run_latest_event(
    bundle: str,
    max_bytes: int = 50000,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    raw = _read_last_bytes(_run_log_path(bundle), max_bytes=max_bytes)
    text = raw.decode("utf-8", errors="replace")
    event = _extract_last_json_line(text)
    return {
        "exists": bool(raw),
        "event": event,
    }


@app.get("/runs/{bundle}/rollout", response_class=JSONResponse)
def run_rollout(
    bundle: str,
    offset: int = 0,
    max_bytes: int = 20000,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    tid = _get_thread_id_for_bundle(bundle)
    if not tid:
        raise HTTPException(status_code=404, detail="thread_id not known yet")
    rollout = _find_rollout_for_thread_id(tid)
    if not rollout:
        raise HTTPException(status_code=404, detail="rollout not found")
    out = _read_tail(rollout, offset=offset, max_bytes=max_bytes)
    out["thread_id"] = tid
    out["rollout_path"] = str(rollout)
    return out


@app.get("/runs/{bundle}/rollout/latest", response_class=JSONResponse)
def run_latest_rollout_event(
    bundle: str,
    max_bytes: int = 50000,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> Any:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)
    tid = _get_thread_id_for_bundle(bundle)
    if not tid:
        raise HTTPException(status_code=404, detail="thread_id not known yet")
    rollout = _find_rollout_for_thread_id(tid)
    if not rollout:
        raise HTTPException(status_code=404, detail="rollout not found")
    raw = _read_last_bytes(rollout, max_bytes=max_bytes)
    text = raw.decode("utf-8", errors="replace")
    event = _extract_last_json_line(text)
    return {
        "thread_id": tid,
        "rollout_path": str(rollout),
        "exists": bool(raw),
        "event": event,
    }


@app.post("/dispatch", response_class=PlainTextResponse)
async def dispatch(
    request: Request,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
    authorization: Optional[str] = Header(default=None),
) -> str:
    _auth_any_or_401(x_pitchai_dispatch_token, authorization)

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
