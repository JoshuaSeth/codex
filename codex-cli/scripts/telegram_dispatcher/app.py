#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

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

    prompt_queue_dir: Path
    prompt_wrapper_template: str

    dispatch_mode: str
    aca_subscription_id: Optional[str]
    aca_resource_group: Optional[str]
    aca_job_name: Optional[str]
    aca_api_version: str

    local_dispatch_command: Optional[str]


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

    return Settings(
        telegram_bot_token=_require_env("TELEGRAM_BOT_TOKEN"),
        telegram_webhook_secret=_require_env("TELEGRAM_WEBHOOK_SECRET"),
        allowed_chat_id=_optional_env("TELEGRAM_ALLOWED_CHAT_ID"),
        allowed_user_id=_optional_env("TELEGRAM_ALLOWED_USER_ID"),
        prompt_queue_dir=prompt_queue_dir,
        prompt_wrapper_template=wrapper,
        dispatch_mode=dispatch_mode,
        aca_subscription_id=_optional_env("ACA_SUBSCRIPTION_ID"),
        aca_resource_group=_optional_env("ACA_RESOURCE_GROUP"),
        aca_job_name=_optional_env("ACA_JOB_NAME"),
        aca_api_version=os.getenv("ACA_API_VERSION", "2025-01-01").strip() or "2025-01-01",
        local_dispatch_command=_optional_env("PITCHAI_LOCAL_DISPATCH_COMMAND"),
    )


def _telegram_send_message(bot_token: str, chat_id: str, text: str) -> None:
    url = f"https://api.telegram.org/bot{bot_token}/sendMessage"
    resp = requests.post(url, json={"chat_id": chat_id, "text": text}, timeout=20)
    resp.raise_for_status()


def _write_prompt_file(settings: Settings, *, update_id: int, chat_id: str, from_user_id: str, from_username: str, text: str) -> Path:
    ts_utc = _now_utc()
    ts_id = _now_utc_compact()
    settings.prompt_queue_dir.mkdir(parents=True, exist_ok=True)
    filename = f"{ts_id}_{update_id}_{_sanitize_filename(from_username)}.md"
    path = settings.prompt_queue_dir / filename

    payload = settings.prompt_wrapper_template.format(
        ts_utc=ts_utc,
        update_id=update_id,
        chat_id=chat_id,
        from_user_id=from_user_id,
        from_username=from_username or "unknown",
        text=text.rstrip(),
    )
    path.write_text(payload + "\n", encoding="utf-8")
    return path


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


@app.post("/telegram/webhook", response_class=PlainTextResponse)
async def telegram_webhook(
    request: Request,
    x_telegram_bot_api_secret_token: Optional[str] = Header(default=None),
) -> str:
    if SETTINGS.telegram_webhook_secret and x_telegram_bot_api_secret_token != SETTINGS.telegram_webhook_secret:
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
    if sender.get("is_bot") is True:
        return "ignored"

    from_user_id = str(sender.get("id", ""))
    if SETTINGS.allowed_user_id and from_user_id != SETTINGS.allowed_user_id:
        return "ignored"

    from_username = str(sender.get("username") or sender.get("first_name") or "user")
    text = message.get("text")
    if not isinstance(text, str) or not text.strip():
        return "ignored"

    prompt_path = _write_prompt_file(
        SETTINGS,
        update_id=update_id,
        chat_id=chat_id,
        from_user_id=from_user_id,
        from_username=from_username,
        text=text,
    )

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

    try:
        if _aca_has_running_execution(SETTINGS):
            return "ok"
    except Exception:
        # If we can't check, still attempt a start (prompt stays queued regardless).
        pass

    try:
        _aca_start_job(SETTINGS)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"failed to start job: {exc}") from exc

    return "ok"
