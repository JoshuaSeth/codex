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
    job_name: Optional[str]


def _require_env(name: str) -> str:
    value = os.getenv(name, "").strip()
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def _optional_env(name: str) -> Optional[str]:
    value = os.getenv(name, "").strip()
    return value or None


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
    job_name = payload.get("job_name")

    if state_key is not None and not isinstance(state_key, str):
        raise HTTPException(status_code=400, detail="state_key must be string")
    if workdir_rel is not None and not isinstance(workdir_rel, str):
        raise HTTPException(status_code=400, detail="workdir_rel must be string")
    if model is not None and not isinstance(model, str):
        raise HTTPException(status_code=400, detail="model must be string")
    if job_name is not None and not isinstance(job_name, str):
        raise HTTPException(status_code=400, detail="job_name must be string")

    return DispatchRequest(
        prompt=prompt.strip(),
        config_toml=config_toml.strip(),
        state_key=state_key.strip() if isinstance(state_key, str) and state_key.strip() else None,
        workdir_rel=workdir_rel.strip() if isinstance(workdir_rel, str) and workdir_rel.strip() else None,
        model=model.strip() if isinstance(model, str) and model.strip() else None,
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

    prompt_path, created = _write_prompt_file(
        SETTINGS,
        update_id=update_id,
        chat_id=chat_id,
        from_user_id=from_user_id,
        from_username=from_username,
        text=text,
    )

    if created:
        try:
            _telegram_send_message(SETTINGS.telegram_bot_token, chat_id, f"Queued for Elise: {prompt_path.name}")
        except Exception:
            pass

    if SETTINGS.dispatch_mode == "noop":
        return "ok"

    if SETTINGS.dispatch_mode == "local":
        _local_dispatch(SETTINGS)
        return "ok"

    # Azure: best-effort start, but avoid starting a duplicate execution if one is already running.
    if not (SETTINGS.aca_subscription_id and SETTINGS.aca_resource_group and SETTINGS.aca_job_name):
        raise HTTPException(status_code=500, detail="ACA_* env vars not configured")

    # If this is a duplicate webhook delivery, don't start a new execution. The prompt is already queued.
    if not created:
        return "ok"

    try:
        if _aca_has_running_execution(SETTINGS):
            return "ok"
    except Exception as exc:  # noqa: BLE001
        # Safety: if we can't check running state, do not attempt to start a new execution.
        # The prompt remains queued and will be picked up by the scheduled run.
        print(f"[dispatch] failed checking running executions: {exc}", file=sys.stderr, flush=True)
        return "ok"

    try:
        _aca_start_job(SETTINGS)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"failed to start job: {exc}") from exc

    return "ok"
