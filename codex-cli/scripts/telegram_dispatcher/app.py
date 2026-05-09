#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import hashlib
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional
from uuid import uuid4

import requests
from fastapi import FastAPI, Header, HTTPException, Request
from fastapi.responses import PlainTextResponse

try:
    from azure.identity import DefaultAzureCredential  # type: ignore
except Exception:  # noqa: BLE001
    DefaultAzureCredential = None  # type: ignore[misc,assignment]

try:
    import psycopg2
    import psycopg2.extras
except Exception:  # noqa: BLE001
    psycopg2 = None  # type: ignore[assignment]


@dataclass(frozen=True)
class Settings:
    telegram_bot_token: str
    telegram_webhook_secret: str
    allowed_chat_id: Optional[str]
    allowed_user_id: Optional[str]
    telegram_bot_user_id: Optional[str]

    prompt_queue_dir: Path
    prompt_wrapper_template: str

    dispatch_mode: str
    aca_subscription_id: Optional[str]
    aca_resource_group: Optional[str]
    aca_job_name: Optional[str]
    aca_api_version: str

    local_dispatch_command: Optional[str]

    dispatch_api_token: Optional[str]
    http_queue_dir: Path
    http_default_job_name: Optional[str]
    http_allowed_job_names: set[str]


@dataclass(frozen=True)
class DispatchRequest:
    prompt: str
    config_toml: str
    state_key: Optional[str]
    workdir_rel: Optional[str]
    model: Optional[str]
    conversation_id: Optional[str]
    fork: bool
    pre_commands: list[str]
    post_commands: list[str]
    git_repo: Optional[str]
    git_branch: Optional[str]
    git_base: Optional[str]
    git_clone_dir_rel: Optional[str]
    job_name: Optional[str]


def _require_env(name: str) -> str:
    value = os.getenv(name, "").strip()
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def _optional_env(name: str) -> Optional[str]:
    value = os.getenv(name, "").strip()
    return value or None


def _db_dsn() -> Optional[str]:
    url = _optional_env("PITCHAI_PM_DB_URL")
    if url:
        return url

    host = _optional_env("PITCHAI_PM_DB_HOST") or _optional_env("PITCHAI_DB_HOST")
    port = _optional_env("PITCHAI_PM_DB_PORT") or _optional_env("PITCHAI_DB_PORT")
    name = _optional_env("PITCHAI_PM_DB_NAME") or _optional_env("PITCHAI_DB_NAME")
    user = _optional_env("PITCHAI_PM_DB_USER") or _optional_env("PITCHAI_DB_USER")
    password = _optional_env("PITCHAI_PM_DB_PASS") or _optional_env("PITCHAI_DB_PASS")
    if not all((host, port, name, user, password)):
        return None
    return f"postgresql://{user}:{password}@{host}:{port}/{name}"


def _db_connect():
    if psycopg2 is None:
        return None
    dsn = _db_dsn()
    if not dsn:
        return None
    return psycopg2.connect(dsn, connect_timeout=10)


def _now_utc() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _now_utc_compact() -> str:
    # Azure Files share names and paths must be compatible with SMB/Windows rules
    # (e.g., ':' is not allowed). Use a compact ISO-like timestamp for filenames.
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _sanitize_filename(value: str) -> str:
    value = value.strip()
    value = re.sub(r"[^a-zA-Z0-9_.-]+", "_", value)
    return value[:120] if value else "telegram"


def load_settings() -> Settings:
    prompt_queue_dir = Path(os.getenv("PITCHAI_PROMPT_QUEUE_DIR", "/mnt/elise/prompts/telegram"))
    http_queue_dir = Path(os.getenv("PITCHAI_HTTP_QUEUE_DIR", "/mnt/elise/prompts/http"))

    wrapper = os.getenv(
        "PITCHAI_PROMPT_WRAPPER",
        (
            "## Telegram command\n"
            "- ts_utc: {ts_utc}\n"
            "- update_id: {update_id}\n"
            "- chat_id: {chat_id}\n"
            "- from_user_id: {from_user_id}\n"
            "- from_username: {from_username}\n"
            "\n"
            "### Instruction\n"
            "{text}\n"
        ),
    )

    dispatch_mode = os.getenv("PITCHAI_DISPATCH_MODE", "azure").strip().lower()
    if dispatch_mode not in ("azure", "local", "noop"):
        raise RuntimeError("PITCHAI_DISPATCH_MODE must be one of: azure, local, noop")

    allowed_jobs_raw = os.getenv("PITCHAI_HTTP_ALLOWED_JOB_NAMES", "").strip()
    allowed_jobs = {j.strip() for j in allowed_jobs_raw.split(",") if j.strip()} if allowed_jobs_raw else set()

    return Settings(
        telegram_bot_token=_require_env("TELEGRAM_BOT_TOKEN"),
        telegram_webhook_secret=_require_env("TELEGRAM_WEBHOOK_SECRET"),
        allowed_chat_id=_optional_env("TELEGRAM_ALLOWED_CHAT_ID"),
        allowed_user_id=_optional_env("TELEGRAM_ALLOWED_USER_ID"),
        telegram_bot_user_id=_optional_env("TELEGRAM_BOT_USER_ID"),
        prompt_queue_dir=prompt_queue_dir,
        prompt_wrapper_template=wrapper,
        dispatch_mode=dispatch_mode,
        aca_subscription_id=_optional_env("ACA_SUBSCRIPTION_ID"),
        aca_resource_group=_optional_env("ACA_RESOURCE_GROUP"),
        aca_job_name=_optional_env("ACA_JOB_NAME"),
        aca_api_version=os.getenv("ACA_API_VERSION", "2025-01-01").strip() or "2025-01-01",
        local_dispatch_command=_optional_env("PITCHAI_LOCAL_DISPATCH_COMMAND"),
        dispatch_api_token=_optional_env("PITCHAI_DISPATCH_API_TOKEN"),
        http_queue_dir=http_queue_dir,
        http_default_job_name=_optional_env("PITCHAI_HTTP_DEFAULT_JOB_NAME"),
        http_allowed_job_names=allowed_jobs,
    )


def _telegram_send_message(bot_token: str, chat_id: str, text: str) -> None:
    url = f"https://api.telegram.org/bot{bot_token}/sendMessage"
    resp = requests.post(url, json={"chat_id": chat_id, "text": text}, timeout=20)
    resp.raise_for_status()


def _write_prompt_file(
    settings: Settings,
    *,
    update_id: int,
    chat_id: str,
    from_user_id: str,
    from_username: str,
    text: str,
) -> tuple[Path, bool]:
    ts_utc = _now_utc()
    settings.prompt_queue_dir.mkdir(parents=True, exist_ok=True)
    # Use a deterministic filename per update_id to avoid accidental duplicate processing
    # when Telegram retries the same update (timeouts, transient network errors, etc).
    filename = f"{update_id:012d}_{_sanitize_filename(from_username)}.md"
    path = settings.prompt_queue_dir / filename

    payload = settings.prompt_wrapper_template.format(
        ts_utc=ts_utc,
        update_id=update_id,
        chat_id=chat_id,
        from_user_id=from_user_id,
        from_username=from_username or "unknown",
        text=text.rstrip(),
    )
    try:
        with path.open("x", encoding="utf-8") as file_handle:
            file_handle.write(payload + "\n")
    except FileExistsError:
        return (path, False)
    return (path, True)


def _write_reply_bundle(
    settings: Settings,
    *,
    update_id: int,
    chat_id: str,
    from_user_id: str,
    from_username: str,
    text: str,
    conversation_id: str,
    reply_to_message_id: int,
    bundle: Optional[str],
    workspace_id: Optional[str],
    route: Optional[dict[str, Any]],
) -> Path:
    ts = _now_utc_compact()
    rid = uuid4().hex[:12]
    settings.prompt_queue_dir.mkdir(parents=True, exist_ok=True)
    bundle_dir = settings.prompt_queue_dir / f"{ts}_reply_{rid}"
    bundle_dir.mkdir(parents=False, exist_ok=False)

    (bundle_dir / "prompt.md").write_text(text.rstrip() + "\n", encoding="utf-8")
    meta = {
        "ts_utc": _now_utc(),
        "source": "telegram_reply",
        "update_id": update_id,
        "chat_id": chat_id,
        "from_user_id": from_user_id,
        "from_username": from_username,
        "conversation_id": conversation_id,
        "reply_to_message_id": reply_to_message_id,
        "bundle": bundle,
        "workspace_id": workspace_id,
        "tmux_route": route or {},
    }
    (bundle_dir / "meta.json").write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    return bundle_dir


def _tmux_args(route: dict[str, Any]) -> list[str]:
    socket_path = route.get("tmux_socket_path")
    if isinstance(socket_path, str) and socket_path.strip():
        return ["tmux", "-S", socket_path.strip()]
    return ["tmux"]


def _submit_reply_to_tmux(route: dict[str, Any], text: str) -> None:
    tmux_session = route.get("tmux_session")
    if not isinstance(tmux_session, str) or not tmux_session.strip():
        raise RuntimeError("tmux_session missing from route")

    window_index = route.get("tmux_window_index")
    try:
        window_part = int(window_index) if window_index is not None else 0
    except (TypeError, ValueError):
        window_part = 0

    target = f"{tmux_session.strip()}:{window_part}"
    base_cmd = _tmux_args(route)

    subprocess.run(
        [*base_cmd, "has-session", "-t", tmux_session.strip()],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    subprocess.run(
        [*base_cmd, "load-buffer", "-"],
        input=text,
        check=True,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )
    subprocess.run(
        [*base_cmd, "paste-buffer", "-d", "-t", target],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    subprocess.run(
        [*base_cmd, "send-keys", "-t", target, "Enter"],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )


def _lookup_reply_mapping(chat_id: str, reply_to_message_id: int) -> Optional[dict[str, Any]]:
    conn = _db_connect()
    if conn is None:
        return None

    try:
        with conn:
            with conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
                cur.execute(
                    """
                    select
                      conversation_id,
                      bundle,
                      workspace_id::text as workspace_id,
                      raw_json
                    from pitchai_dispatch.telegram_inbound_updates
                    where chat_id = %s
                      and message_id = %s
                      and status = 'sent_final_update'
                    order by created_at desc
                    limit 1
                    """,
                    (int(chat_id), reply_to_message_id),
                )
                row = cur.fetchone()
                if not row:
                    return None

                raw_json = row.get("raw_json") if isinstance(row.get("raw_json"), dict) else {}
                route = raw_json.get("route") if isinstance(raw_json.get("route"), dict) else {}
                return {
                    "conversation_id": row.get("conversation_id"),
                    "bundle": row.get("bundle"),
                    "workspace_id": row.get("workspace_id"),
                    "route": route,
                }
    finally:
        conn.close()


def _record_telegram_update(
    *,
    update_id: int,
    status: str,
    chat_id: Optional[str],
    from_user_id: Optional[str],
    message_id: Optional[int],
    reply_to_message_id: Optional[int],
    conversation_id: Optional[str],
    bundle: Optional[str],
    prompt_preview: str,
    error: Optional[str],
    raw_json: dict[str, Any],
    workspace_id: Optional[str],
) -> None:
    conn = _db_connect()
    if conn is None:
        return

    columns = [
        "update_id",
        "status",
        "chat_id",
        "from_user_id",
        "message_id",
        "reply_to_message_id",
        "conversation_id",
        "bundle",
        "prompt_preview",
        "error",
        "raw_json",
    ]
    values: list[Any] = [
        update_id,
        status,
        int(chat_id) if chat_id else None,
        int(from_user_id) if from_user_id else None,
        message_id,
        reply_to_message_id,
        conversation_id,
        bundle,
        prompt_preview,
        error,
        json.dumps(raw_json),
    ]
    placeholders = ["%s", "%s", "%s", "%s", "%s", "%s", "%s", "%s", "%s", "%s", "%s::jsonb"]
    updates = [
        "updated_at = now()",
        "status = excluded.status",
        "chat_id = excluded.chat_id",
        "from_user_id = excluded.from_user_id",
        "message_id = excluded.message_id",
        "reply_to_message_id = excluded.reply_to_message_id",
        "conversation_id = excluded.conversation_id",
        "bundle = excluded.bundle",
        "prompt_preview = excluded.prompt_preview",
        "error = excluded.error",
        "raw_json = excluded.raw_json",
    ]

    if workspace_id:
        columns.append("workspace_id")
        values.append(workspace_id)
        placeholders.append("%s::uuid")
        updates.append("workspace_id = excluded.workspace_id")

    sql = (
        "insert into pitchai_dispatch.telegram_inbound_updates ("
        + ", ".join(columns)
        + ") values ("
        + ", ".join(placeholders)
        + ") on conflict (update_id) do update set "
        + ", ".join(updates)
    )

    try:
        with conn:
            with conn.cursor() as cur:
                cur.execute(sql, tuple(values))
    finally:
        conn.close()


def _parse_dispatch_request(payload: Any) -> DispatchRequest:
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
    job_name = payload.get("job_name")

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
    if job_name is not None and not isinstance(job_name, str):
        raise HTTPException(status_code=400, detail="job_name must be string")

    return DispatchRequest(
        prompt=prompt.strip(),
        config_toml=config_toml.strip(),
        state_key=state_key.strip() if isinstance(state_key, str) and state_key.strip() else None,
        workdir_rel=workdir_rel.strip() if isinstance(workdir_rel, str) and workdir_rel.strip() else None,
        model=model.strip() if isinstance(model, str) and model.strip() else None,
        conversation_id=conversation_id.strip()
        if isinstance(conversation_id, str) and conversation_id.strip()
        else None,
        fork=bool(fork) if isinstance(fork, bool) else False,
        pre_commands=[c.strip() for c in pre_commands if isinstance(c, str) and c.strip()] if isinstance(pre_commands, list) else [],
        post_commands=[c.strip() for c in post_commands if isinstance(c, str) and c.strip()] if isinstance(post_commands, list) else [],
        git_repo=git_repo.strip() if isinstance(git_repo, str) and git_repo.strip() else None,
        git_branch=git_branch.strip() if isinstance(git_branch, str) and git_branch.strip() else None,
        git_base=git_base.strip() if isinstance(git_base, str) and git_base.strip() else None,
        git_clone_dir_rel=git_clone_dir_rel.strip()
        if isinstance(git_clone_dir_rel, str) and git_clone_dir_rel.strip()
        else None,
        job_name=job_name.strip() if isinstance(job_name, str) and job_name.strip() else None,
    )


def _write_http_dispatch_bundle(settings: Settings, req: DispatchRequest) -> Path:
    settings.http_queue_dir.mkdir(parents=True, exist_ok=True)

    ts = _now_utc_compact()
    rid = uuid4().hex[:12]
    bundle = settings.http_queue_dir / f"{ts}_{rid}"
    bundle.mkdir(parents=False, exist_ok=False)

    (bundle / "prompt.md").write_text(req.prompt.rstrip() + "\n", encoding="utf-8")
    (bundle / "config.toml").write_text(req.config_toml.rstrip() + "\n", encoding="utf-8")
    meta = {
        "ts_utc": _now_utc(),
        "state_key": req.state_key,
        "workdir_rel": req.workdir_rel,
        "model": req.model,
        "conversation_id": req.conversation_id,
        "fork": req.fork,
        "pre_commands": req.pre_commands,
        "post_commands": req.post_commands,
        "git_repo": req.git_repo,
        "git_branch": req.git_branch,
        "git_base": req.git_base,
        "git_clone_dir_rel": req.git_clone_dir_rel,
    }
    (bundle / "meta.json").write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
    return bundle


def _aca_start_job_named(settings: Settings, job_name: str) -> str:
    assert settings.aca_subscription_id and settings.aca_resource_group
    url = (
        "https://management.azure.com/subscriptions/"
        f"{settings.aca_subscription_id}/resourceGroups/{settings.aca_resource_group}"
        f"/providers/Microsoft.App/jobs/{job_name}/start"
        f"?api-version={settings.aca_api_version}"
    )
    token = _aca_token()
    resp = requests.post(url, headers={"Authorization": f"Bearer {token}"}, timeout=30)
    resp.raise_for_status()
    data = resp.json()
    name = data.get("name")
    return str(name) if isinstance(name, str) else "unknown"


def _aca_token() -> str:
    if DefaultAzureCredential is None:
        raise RuntimeError("azure-identity is not available in this environment")
    cred = DefaultAzureCredential()
    return cred.get_token("https://management.azure.com/.default").token


def _aca_list_executions(settings: Settings) -> list[dict[str, Any]]:
    assert settings.aca_subscription_id and settings.aca_resource_group and settings.aca_job_name
    url = (
        "https://management.azure.com/subscriptions/"
        f"{settings.aca_subscription_id}/resourceGroups/{settings.aca_resource_group}"
        f"/providers/Microsoft.App/jobs/{settings.aca_job_name}/executions"
        f"?api-version={settings.aca_api_version}"
    )
    token = _aca_token()
    resp = requests.get(url, headers={"Authorization": f"Bearer {token}"}, timeout=30)
    resp.raise_for_status()
    data = resp.json()
    value = data.get("value", [])
    return value if isinstance(value, list) else []


def _aca_has_running_execution(settings: Settings) -> bool:
    for item in _aca_list_executions(settings):
        props = item.get("properties")
        if isinstance(props, dict) and props.get("status") == "Running":
            return True
    return False


def _aca_start_job(settings: Settings) -> str:
    assert settings.aca_subscription_id and settings.aca_resource_group and settings.aca_job_name
    url = (
        "https://management.azure.com/subscriptions/"
        f"{settings.aca_subscription_id}/resourceGroups/{settings.aca_resource_group}"
        f"/providers/Microsoft.App/jobs/{settings.aca_job_name}/start"
        f"?api-version={settings.aca_api_version}"
    )
    token = _aca_token()
    resp = requests.post(url, headers={"Authorization": f"Bearer {token}"}, timeout=30)
    resp.raise_for_status()
    data = resp.json()
    name = data.get("name")
    return str(name) if isinstance(name, str) else "unknown"


def _local_dispatch(settings: Settings) -> None:
    cmd = settings.local_dispatch_command
    if not cmd:
        raise RuntimeError("PITCHAI_LOCAL_DISPATCH_COMMAND is required when PITCHAI_DISPATCH_MODE=local")
    subprocess.Popen(cmd, shell=True, stdout=sys.stderr, stderr=sys.stderr)


def _maybe_start_default_dispatch(settings: Settings) -> None:
    if settings.dispatch_mode == "noop":
        return
    if settings.dispatch_mode == "local":
        _local_dispatch(settings)
        return

    if not (settings.aca_subscription_id and settings.aca_resource_group and settings.aca_job_name):
        raise RuntimeError("ACA_* env vars not configured")

    try:
        if _aca_has_running_execution(settings):
            return
    except Exception as exc:  # noqa: BLE001
        print(f"[dispatch] failed checking running executions: {exc}", file=sys.stderr, flush=True)
        return

    _aca_start_job(settings)


app = FastAPI()
SETTINGS = load_settings()


@app.get("/healthz", response_class=PlainTextResponse)
def healthz() -> str:
    return "ok"


@app.post("/dispatch", response_class=PlainTextResponse)
async def dispatch(
    request: Request,
    x_pitchai_dispatch_token: Optional[str] = Header(default=None),
) -> str:
    expected = SETTINGS.dispatch_api_token
    if expected:
        got = (x_pitchai_dispatch_token or "").strip()
        if got != expected:
            raise HTTPException(status_code=401, detail="invalid dispatch token")
    else:
        raise HTTPException(status_code=500, detail="dispatch not configured")

    try:
        payload = await request.json()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=400, detail=f"invalid json: {exc}") from exc

    req = _parse_dispatch_request(payload)
    bundle = _write_http_dispatch_bundle(SETTINGS, req)

    if SETTINGS.dispatch_mode == "noop":
        return "queued"
    if SETTINGS.dispatch_mode == "local":
        _local_dispatch(SETTINGS)
        return "queued"

    if not (SETTINGS.aca_subscription_id and SETTINGS.aca_resource_group):
        raise HTTPException(status_code=500, detail="ACA_* env vars not configured")

    job_name = req.job_name or SETTINGS.http_default_job_name or SETTINGS.aca_job_name
    if not job_name:
        raise HTTPException(status_code=500, detail="no job configured to start")

    if SETTINGS.http_allowed_job_names and job_name not in SETTINGS.http_allowed_job_names:
        raise HTTPException(status_code=403, detail="job_name not allowed")

    # Best-effort: avoid duplicate running executions if we can.
    try:
        SETTINGS_JOB = Settings(**{**SETTINGS.__dict__, "aca_job_name": job_name})  # type: ignore[arg-type]
        if _aca_has_running_execution(SETTINGS_JOB):
            return f"queued:{bundle.name}"
    except Exception as exc:  # noqa: BLE001
        print(f"[dispatch] failed checking running executions: {exc}", file=sys.stderr, flush=True)
        return f"queued:{bundle.name}"

    try:
        _aca_start_job_named(SETTINGS, job_name)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"failed to start job: {exc}") from exc

    return f"queued:{bundle.name}"


@app.post("/telegram/webhook", response_class=PlainTextResponse)
async def telegram_webhook(
    request: Request,
    x_telegram_bot_api_secret_token: Optional[str] = Header(default=None),
) -> str:
    if SETTINGS.telegram_webhook_secret and x_telegram_bot_api_secret_token != SETTINGS.telegram_webhook_secret:
        got = x_telegram_bot_api_secret_token or ""
        expected = SETTINGS.telegram_webhook_secret
        got_sha = hashlib.sha256(got.encode("utf-8")).hexdigest()
        expected_sha = hashlib.sha256(expected.encode("utf-8")).hexdigest()
        print(
            f"[auth] webhook secret mismatch got_sha={got_sha} expected_sha={expected_sha} got_len={len(got)} expected_len={len(expected)}",
            file=sys.stderr,
            flush=True,
        )
        raise HTTPException(status_code=401, detail="invalid telegram secret token")

    try:
        update = await request.json()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=400, detail=f"invalid json: {exc}") from exc

    if not isinstance(update, dict):
        raise HTTPException(status_code=400, detail="invalid update payload")

    update_id = update.get("update_id")
    message = update.get("message") or update.get("edited_message")
    if not isinstance(update_id, int) or not isinstance(message, dict):
        return "ignored"

    chat = message.get("chat") if isinstance(message.get("chat"), dict) else {}
    chat_id = str(chat.get("id", ""))
    if SETTINGS.allowed_chat_id and chat_id != SETTINGS.allowed_chat_id:
        return "ignored"

    sender = message.get("from") if isinstance(message.get("from"), dict) else {}
    from_user_id = str(sender.get("id", ""))
    sender_is_bot = sender.get("is_bot")
    if sender_is_bot is True:
        return "ignored"
    if SETTINGS.telegram_bot_user_id and from_user_id == SETTINGS.telegram_bot_user_id:
        return "ignored"
    if SETTINGS.allowed_user_id and from_user_id != SETTINGS.allowed_user_id:
        return "ignored"

    from_username = str(sender.get("username") or sender.get("first_name") or "user")
    text = message.get("text")
    if not isinstance(text, str) or not text.strip():
        return "ignored"
    prompt_preview = text.strip().replace("\n", " ")[:240]
    incoming_message_id = message.get("message_id") if isinstance(message.get("message_id"), int) else None
    reply_to = message.get("reply_to_message") if isinstance(message.get("reply_to_message"), dict) else {}
    reply_to_message_id = reply_to.get("message_id") if isinstance(reply_to.get("message_id"), int) else None

    if reply_to_message_id is not None:
        mapping = _lookup_reply_mapping(chat_id, reply_to_message_id)
        if mapping is None:
            _record_telegram_update(
                update_id=update_id,
                status="reply_mapping_missing",
                chat_id=chat_id,
                from_user_id=from_user_id,
                message_id=incoming_message_id,
                reply_to_message_id=reply_to_message_id,
                conversation_id=None,
                bundle=None,
                prompt_preview=prompt_preview,
                error="No sent_final_update mapping found for reply target",
                raw_json={"update": update},
                workspace_id=None,
            )
            try:
                _telegram_send_message(
                    SETTINGS.telegram_bot_token,
                    chat_id,
                    "Reply received, but I could not resolve the originating Codex session for that message.",
                )
            except Exception:
                pass
            return "ok"

        mapped_conversation_id = str(mapping.get("conversation_id") or "").strip() or None
        mapped_bundle = str(mapping.get("bundle") or "").strip() or None
        mapped_workspace_id = str(mapping.get("workspace_id") or "").strip() or None
        route = mapping.get("route") if isinstance(mapping.get("route"), dict) else {}

        status = "reply_unroutable"
        error = None
        bundle_name = mapped_bundle
        raw_json: dict[str, Any] = {
            "update": update,
            "reply_mapping": mapping,
        }
        try:
            if route.get("tmux_session"):
                _submit_reply_to_tmux(route, text)
                status = "submitted_to_tmux"
                raw_json["delivery"] = {"mode": "tmux"}
            elif mapped_conversation_id:
                queued_bundle = _write_reply_bundle(
                    SETTINGS,
                    update_id=update_id,
                    chat_id=chat_id,
                    from_user_id=from_user_id,
                    from_username=from_username,
                    text=text,
                    conversation_id=mapped_conversation_id,
                    reply_to_message_id=reply_to_message_id,
                    bundle=mapped_bundle,
                    workspace_id=mapped_workspace_id,
                    route=route,
                )
                bundle_name = queued_bundle.name
                status = "queued_reply_resume"
                raw_json["delivery"] = {"mode": "queued_resume", "bundle_dir": queued_bundle.name}
                _maybe_start_default_dispatch(SETTINGS)
            else:
                error = "Resolved reply target has neither tmux route nor conversation_id"
        except Exception as exc:  # noqa: BLE001
            if mapped_conversation_id:
                try:
                    queued_bundle = _write_reply_bundle(
                        SETTINGS,
                        update_id=update_id,
                        chat_id=chat_id,
                        from_user_id=from_user_id,
                        from_username=from_username,
                        text=text,
                        conversation_id=mapped_conversation_id,
                        reply_to_message_id=reply_to_message_id,
                        bundle=mapped_bundle,
                        workspace_id=mapped_workspace_id,
                        route=route,
                    )
                    bundle_name = queued_bundle.name
                    status = "queued_reply_resume"
                    error = f"tmux submit failed: {exc}"
                    raw_json["delivery"] = {
                        "mode": "queued_resume_after_tmux_failure",
                        "bundle_dir": queued_bundle.name,
                        "tmux_error": str(exc),
                    }
                    _maybe_start_default_dispatch(SETTINGS)
                except Exception as queued_exc:  # noqa: BLE001
                    error = f"tmux submit failed: {exc}; queueing failed: {queued_exc}"
            else:
                error = str(exc)

        _record_telegram_update(
            update_id=update_id,
            status=status,
            chat_id=chat_id,
            from_user_id=from_user_id,
            message_id=incoming_message_id,
            reply_to_message_id=reply_to_message_id,
            conversation_id=mapped_conversation_id,
            bundle=bundle_name,
            prompt_preview=prompt_preview,
            error=error,
            raw_json=raw_json,
            workspace_id=mapped_workspace_id,
        )
        if status == "reply_unroutable":
            try:
                _telegram_send_message(
                    SETTINGS.telegram_bot_token,
                    chat_id,
                    "Reply received, but the originating Codex session could not be routed automatically.",
                )
            except Exception:
                pass
        return "ok"

    prompt_path, created = _write_prompt_file(
        SETTINGS,
        update_id=update_id,
        chat_id=chat_id,
        from_user_id=from_user_id,
        from_username=from_username,
        text=text,
    )

    _record_telegram_update(
        update_id=update_id,
        status="queued_new_prompt" if created else "duplicate_update",
        chat_id=chat_id,
        from_user_id=from_user_id,
        message_id=incoming_message_id,
        reply_to_message_id=None,
        conversation_id=None,
        bundle=prompt_path.name,
        prompt_preview=prompt_preview,
        error=None,
        raw_json={"update": update, "delivery": {"mode": "prompt_file", "created": created}},
        workspace_id=None,
    )

    if created:
        try:
            _telegram_send_message(SETTINGS.telegram_bot_token, chat_id, f"Queued for Elise: {prompt_path.name}")
        except Exception:
            pass

    if created:
        try:
            _maybe_start_default_dispatch(SETTINGS)
        except RuntimeError as exc:
            raise HTTPException(status_code=500, detail=str(exc)) from exc
        except Exception as exc:  # noqa: BLE001
            raise HTTPException(status_code=500, detail=f"failed to start job: {exc}") from exc

    return "ok"
