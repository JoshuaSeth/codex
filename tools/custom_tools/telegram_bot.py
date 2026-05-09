"""Telegram notification system + Codex hook bridge."""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import html
import json
import os
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib import error as urllib_error
from urllib import request as urllib_request

try:
    import structlog
except ModuleNotFoundError:  # pragma: no cover - fallback for hook environments
    import logging

    logging.basicConfig(level=logging.INFO)

    class _FallbackLogger:  # pylint: disable=too-few-public-methods
        def __init__(self, name: str):
            self._logger = logging.getLogger(name)

        def info(self, msg: str, **kwargs: Any) -> None:
            self._logger.info("%s %s", msg, kwargs if kwargs else "")

        def warning(self, msg: str, **kwargs: Any) -> None:
            self._logger.warning("%s %s", msg, kwargs if kwargs else "")

        def error(self, msg: str, **kwargs: Any) -> None:
            self._logger.error("%s %s", msg, kwargs if kwargs else "")

    class _StructlogShim:
        @staticmethod
        def get_logger(name: str) -> _FallbackLogger:
            return _FallbackLogger(name)

    structlog = _StructlogShim()

try:
    import psycopg2
    import psycopg2.extras
except ModuleNotFoundError:  # pragma: no cover - optional in local hook environments
    psycopg2 = None  # type: ignore[assignment]

logger = structlog.get_logger(__name__)

DEBUG_LOG = os.getenv("CODEX_STOP_HOOK_LOG")

_ENV_CACHE: dict[str, str] | None = None


def _find_env_file() -> Path | None:
    """Locate the nearest .env file walking up toward the project root."""
    current = Path(__file__).resolve().parent
    for directory in (current, *current.parents):
        env_path = directory / ".env"
        if env_path.exists():
            return env_path
        # Stop at repository root (heuristic: contains .git)
        if (directory / ".git").exists():
            break
    return None


def _load_env_file() -> dict[str, str]:
    """Read Telegram-related variables from the repo's .env file if present."""
    global _ENV_CACHE
    if _ENV_CACHE is not None:
        return _ENV_CACHE

    env_values: dict[str, str] = {}
    env_path = _find_env_file()
    if env_path and env_path.exists():
        for raw_line in env_path.read_text().splitlines():
            line = raw_line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            key = key.strip()
            value = value.strip().strip('"\'')
            env_values[key] = value

    _ENV_CACHE = env_values
    return env_values


def _get_env_value(key: str) -> str | None:
    """Return env var preferring .env file, falling back to OS environment."""
    env_data = _load_env_file()
    value = env_data.get(key)
    if value:
        return value
    return os.getenv(key)


def _first_env_value(*keys: str) -> str | None:
    for key in keys:
        value = _get_env_value(key)
        if value and value.strip():
            return value.strip()
    return None


def _pm_db_dsn() -> str | None:
    url = _first_env_value("PITCHAI_PM_DB_URL")
    if url:
        return url

    host = _first_env_value("PITCHAI_PM_DB_HOST", "PITCHAI_DB_HOST")
    port = _first_env_value("PITCHAI_PM_DB_PORT", "PITCHAI_DB_PORT")
    name = _first_env_value("PITCHAI_PM_DB_NAME", "PITCHAI_DB_NAME")
    user = _first_env_value("PITCHAI_PM_DB_USER", "PITCHAI_DB_USER")
    password = _first_env_value("PITCHAI_PM_DB_PASS", "PITCHAI_DB_PASS")
    if not all((host, port, name, user, password)):
        return None
    return f"postgresql://{user}:{password}@{host}:{port}/{name}"


def _pm_db_connect():
    if psycopg2 is None:
        return None
    dsn = _pm_db_dsn()
    if not dsn:
        return None
    return psycopg2.connect(dsn, connect_timeout=10)


def _coerce_int(value: Any) -> int | None:
    if value is None:
        return None
    try:
        return int(str(value).strip())
    except (TypeError, ValueError):
        return None


def _project_name_from_cwd(cwd: Any) -> str:
    text = str(cwd or "(unknown cwd)")
    try:
        return Path(text).resolve().name or str(Path(text).resolve())
    except Exception:  # noqa: BLE001
        return Path(text).name or text


def _workspace_default_socket(cur, workspace_id: str | None) -> str | None:
    if not workspace_id:
        return None
    cur.execute(
        """
        select socket_path
        from pitchai_dispatch.workspace_tmux_servers
        where workspace_id = %s
        order by case when label = 'default' then 0 else 1 end, created_at asc
        limit 1
        """,
        (workspace_id,),
    )
    row = cur.fetchone()
    if not row:
        return None
    socket_path = row[0] if not isinstance(row, dict) else row.get("socket_path")
    return str(socket_path).strip() if socket_path else None


def _resolve_tmux_socket_path(cur, *, socket_hint: str | None, workspace_id: str | None) -> str | None:
    if socket_hint:
        hint = socket_hint.strip()
        if hint.startswith("/"):
            return hint
        if hint.startswith("tmux-"):
            return f"/host_tmp/{hint}"
    return _workspace_default_socket(cur, workspace_id)


def _route_from_tmux_key(
    cur,
    *,
    tmux_key: str,
    workspace_id: str | None,
    ui_title: str | None,
) -> dict[str, Any] | None:
    key = tmux_key.strip()
    if not key:
        return None

    socket_hint = None
    tmux_session = key
    if "|" in key:
        socket_hint, tmux_session = key.split("|", 1)

    socket_path = _resolve_tmux_socket_path(cur, socket_hint=socket_hint, workspace_id=workspace_id)
    route = {
        "workspace_id": workspace_id,
        "tmux_socket_path": socket_path,
        "tmux_session": tmux_session.strip() or None,
        "tmux_window_index": 0,
        "ui_title": ui_title,
    }
    return route if route.get("tmux_session") else None


def _lookup_session_route(conversation_id: str | None) -> dict[str, Any] | None:
    env_tmux_session = _first_env_value("PITCHAI_CODEX_TMUX_SESSION")
    if env_tmux_session:
        env_route = {
            "workspace_id": _first_env_value("PITCHAI_WORKSPACE_ID"),
            "tmux_socket_path": _first_env_value("PITCHAI_CODEX_TMUX_SOCKET"),
            "tmux_session": env_tmux_session,
            "tmux_window_index": _coerce_int(_first_env_value("PITCHAI_CODEX_TMUX_WINDOW")) or 0,
            "ui_title": _first_env_value("PITCHAI_CODEX_UI_TITLE"),
            "route_source": "env",
        }
        return env_route

    if not conversation_id:
        return None

    conn = _pm_db_connect()
    if conn is None:
        return None

    try:
        with conn:
            with conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
                cur.execute(
                    """
                    select
                      workspace_id::text as workspace_id,
                      cwd,
                      repo_full_name,
                      meta_json
                    from pitchai_dispatch.project_codex_sessions
                    where conversation_id = %s
                    order by updated_at desc nulls last, created_at desc nulls last
                    limit 1
                    """,
                    (conversation_id,),
                )
                row = cur.fetchone()
                if not row:
                    return None

                workspace_id = row.get("workspace_id")
                meta_json = row.get("meta_json") if isinstance(row.get("meta_json"), dict) else {}
                ui_title_map = meta_json.get("ui_title_by_tmux_session")
                if isinstance(ui_title_map, dict) and ui_title_map:
                    tmux_key, ui_title = list(ui_title_map.items())[-1]
                    route = _route_from_tmux_key(
                        cur,
                        tmux_key=str(tmux_key),
                        workspace_id=workspace_id,
                        ui_title=str(ui_title).strip() if ui_title else None,
                    )
                    if route:
                        route["route_source"] = "project_codex_sessions.ui_title_by_tmux_session"
                        route["cwd"] = row.get("cwd")
                        route["repo_full_name"] = row.get("repo_full_name")
                        return route

                ui_remove_events = meta_json.get("ui_remove_events")
                if isinstance(ui_remove_events, list) and ui_remove_events:
                    last_event = ui_remove_events[-1]
                    if isinstance(last_event, dict):
                        socket_path = last_event.get("tmux_socket_path")
                        route = {
                            "workspace_id": workspace_id,
                            "tmux_socket_path": str(socket_path).strip() if socket_path else None,
                            "tmux_session": str(last_event.get("tmux_session") or "").strip() or None,
                            "tmux_window_index": _coerce_int(last_event.get("tmux_window_index")) or 0,
                            "ui_title": meta_json.get("ui_title"),
                            "route_source": "project_codex_sessions.ui_remove_events",
                            "cwd": row.get("cwd"),
                            "repo_full_name": row.get("repo_full_name"),
                        }
                        if route.get("tmux_session"):
                            return route
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "Failed to look up Codex session route",
            conversation_id=conversation_id,
            error=str(exc),
        )
        return None
    finally:
        conn.close()

    return None


def _synthetic_outbound_update_id(chat_id: int, message_id: int) -> int:
    return -(((abs(chat_id) % 10_000_000_000) * 1_000_000) + abs(message_id))


def _persist_sent_final_update(
    *,
    payload: dict[str, Any],
    telegram_result: dict[str, Any],
    route: dict[str, Any] | None,
) -> None:
    conn = _pm_db_connect()
    if conn is None:
        return

    result = telegram_result.get("result")
    if not isinstance(result, dict):
        return

    chat_id = _coerce_int((result.get("chat") or {}).get("id") if isinstance(result.get("chat"), dict) else None)
    message_id = _coerce_int(result.get("message_id"))
    if chat_id is None or message_id is None:
        return

    conversation_id = str(payload.get("conversation_id") or "").strip() or None
    bundle = _first_env_value("PITCHAI_DISPATCH_BUNDLE", "PITCHAI_CODEX_BUNDLE")
    final_message = payload.get("final_message") or _extract_last_assistant_message(payload.get("response_items"))
    preview = _truncate(str(final_message).strip(), 240) if final_message else ""
    workspace_id = None
    if route and isinstance(route.get("workspace_id"), str) and route.get("workspace_id"):
        workspace_id = route["workspace_id"]

    raw_json = {
        "kind": "sent_final_update",
        "payload": {
            "conversation_id": conversation_id,
            "turn_id": payload.get("turn_id"),
            "cwd": payload.get("cwd"),
            "bundle": bundle,
        },
        "route": route or {},
        "telegram_result": telegram_result,
    }

    columns = [
        "update_id",
        "status",
        "chat_id",
        "message_id",
        "conversation_id",
        "bundle",
        "prompt_preview",
        "raw_json",
    ]
    values: list[Any] = [
        _synthetic_outbound_update_id(chat_id, message_id),
        "sent_final_update",
        chat_id,
        message_id,
        conversation_id,
        bundle,
        preview,
        json.dumps(raw_json),
    ]
    placeholders = ["%s", "%s", "%s", "%s", "%s", "%s", "%s", "%s::jsonb"]
    updates = [
        "updated_at = now()",
        "status = excluded.status",
        "chat_id = excluded.chat_id",
        "message_id = excluded.message_id",
        "conversation_id = excluded.conversation_id",
        "bundle = excluded.bundle",
        "prompt_preview = excluded.prompt_preview",
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
    except Exception as exc:  # noqa: BLE001
        logger.warning(
            "Failed to persist sent Telegram update route",
            conversation_id=conversation_id,
            message_id=message_id,
            error=str(exc),
        )
    finally:
        conn.close()


class TelegramNotifier:
    """Handles Telegram notifications for monitoring alerts."""

    def __init__(self, bot_token: str | None = None, chat_id: str | None = None):
        """Initialize Telegram notifier.

        Args:
            bot_token: Telegram bot token (or from TELEGRAM_BOT_TOKEN env)
            chat_id: Telegram chat ID (or from TELEGRAM_CHAT_ID env)
        """
        env_bot_token = _first_env_value(
            "PITCHAI_UPDATES_TELEGRAM_BOT_TOKEN",
            "TELEGRAM_UPDATES_BOT_TOKEN",
            "TELEGRAM_BOT_TOKEN",
        )
        env_chat_id = _first_env_value(
            "PITCHAI_UPDATES_TELEGRAM_CHAT_ID",
            "TELEGRAM_UPDATES_CHAT_ID",
            "TELEGRAM_CHAT_ID",
        )

        self.bot_token = bot_token or env_bot_token
        self.chat_id = chat_id or env_chat_id
        self.api_url = (
            f"https://api.telegram.org/bot{self.bot_token}/sendMessage"
            if self.bot_token
            else None
        )
        if not self.bot_token:
            logger.warning("Telegram bot token not configured")
        else:
            logger.info("Telegram notifier ready")

    async def send_daily_report(self, report: dict[str, Any]) -> bool:
        """Send daily monitoring report via Telegram.

        Args:
            report: Daily monitoring report data

        Returns:
            True if sent successfully
        """
        if not self._is_configured():
            return False

        message = self._format_daily_report(report)
        return await self.send_markdown(message)

    async def send_critical_alert(self, alert: dict[str, Any]) -> bool:
        """Send critical alert immediately.

        Args:
            alert: Critical alert data

        Returns:
            True if sent successfully
        """
        if not self._is_configured():
            return False

        message = self._format_critical_alert(alert)
        return await self.send_markdown(message, priority="high")

    async def send_test_failure_summary(self, failures: list[dict[str, Any]]) -> bool:
        """Send UI test failure summary.

        Args:
            failures: List of test failures

        Returns:
            True if sent successfully
        """
        if not self._is_configured():
            return False

        message = self._format_test_failures(failures)
        return await self.send_markdown(message)

    def _format_daily_report(self, report: dict[str, Any]) -> str:
        """Format daily report for Telegram."""
        timestamp = report.get("timestamp", datetime.now(timezone.utc).isoformat())

        # Overall status emoji
        status_emoji = "✅" if report.get("all_healthy", True) else "⚠️"

        lines = [
            f"{status_emoji} *PitchAI Daily Monitoring Report*",
            f"_Generated: {timestamp}_",
            "",
            "📊 *Summary*",
            f"• UI Tests: {report.get('ui_tests_passed', 0)}/{report.get('ui_tests_total', 0)} passed",
            f"• Containers: {report.get('containers_monitored', 0)} monitored",
            f"• Errors: {report.get('total_errors', 0)} detected",
            ""
        ]

        # Add critical issues if any
        if report.get("critical_issues"):
            lines.append("🚨 *Critical Issues*")
            for issue in report["critical_issues"][:5]:  # Limit to 5
                lines.append(f"• {issue.get('container', 'Unknown')}: {issue.get('message', '')[:100]}")
            lines.append("")

        # Add failed tests if any
        if report.get("failed_tests"):
            lines.append("❌ *Failed UI Tests*")
            for test in report["failed_tests"][:5]:  # Limit to 5
                lines.append(f"• {test.get('name', 'Unknown')}: {test.get('error', '')[:100]}")
            lines.append("")

        # Add recommendations
        if report.get("recommendations"):
            lines.append("💡 *Recommendations*")
            for rec in report["recommendations"][:3]:  # Limit to 3
                lines.append(f"• {rec}")
            lines.append("")

        # Health status
        lines.append("🏥 *System Health*")
        lines.append(f"• Overall: {'Healthy' if report.get('all_healthy', True) else 'Issues Detected'}")
        lines.append(f"• Uptime: {report.get('uptime_percentage', 100):.1f}%")

        return "\n".join(lines)

    def _format_critical_alert(self, alert: dict[str, Any]) -> str:
        """Format critical alert for immediate notification."""
        lines = [
            "🚨🚨🚨 *CRITICAL ALERT* 🚨🚨🚨",
            "",
            f"*Service:* {alert.get('service', 'Unknown')}",
            f"*Issue:* {alert.get('issue', 'Unknown error')}",
            f"*Time:* {alert.get('timestamp', datetime.now(timezone.utc).isoformat())}",
            "",
            "*Details:*",
            f"{alert.get('details', 'No additional details available')[:500]}",
            "",
            f"*Action Required:* {alert.get('action', 'Please investigate immediately')}",
            "",
            "_This is an automated alert from PitchAI Monitoring_"
        ]

        return "\n".join(lines)

    def _format_test_failures(self, failures: list[dict[str, Any]]) -> str:
        """Format test failures for notification."""
        if not failures:
            return "✅ All UI tests passed successfully!"

        lines = [
            "⚠️ *UI Test Failures Detected*",
            f"_Failed: {len(failures)} test(s)_",
            ""
        ]

        for failure in failures[:10]:  # Limit to 10
            lines.append(f"❌ *{failure.get('test_name', 'Unknown Test')}*")
            lines.append(f"   Error: {failure.get('error', 'Unknown error')[:200]}")
            lines.append(f"   Duration: {failure.get('duration', 0):.2f}s")
            lines.append("")

        if len(failures) > 10:
            lines.append(f"_... and {len(failures) - 10} more failures_")

        return "\n".join(lines)

    async def _send_message(
        self,
        message: str,
        *,
        priority: str = "normal",
        parse_mode: str = "Markdown",
    ) -> dict[str, Any] | None:
        """Send message via Telegram.

        Args:
            message: Formatted message to send
            priority: Message priority (normal/high)
            parse_mode: Telegram parse mode

        Returns:
            Telegram API response payload, or None on failure
        """
        if not self._is_configured():
            logger.warning("Telegram not configured, skipping notification")
            return None

        try:
            # Add priority indicator for high priority
            if priority == "high":
                message = "‼️ " + message

            payload = json.dumps(
                {
                    "chat_id": self.chat_id,
                    "text": message,
                    "parse_mode": parse_mode,
                    "disable_web_page_preview": True,
                }
            ).encode("utf-8")

            headers = {"Content-Type": "application/json"}
            request = urllib_request.Request(self.api_url, data=payload, headers=headers)

            loop = asyncio.get_running_loop()
            raw = await loop.run_in_executor(None, lambda: urllib_request.urlopen(request, timeout=15).read())
            decoded = json.loads(raw.decode("utf-8") or "{}")

            logger.info("Telegram notification sent", priority=priority, parse_mode=parse_mode)
            return decoded if isinstance(decoded, dict) else {"ok": True}

        except urllib_error.URLError as exc:
            logger.error("Failed to send Telegram notification", error=str(exc))
            return None
        except Exception as exc:  # noqa: BLE001
            logger.error("Unexpected error sending Telegram notification", error=str(exc))
            return None

    def _is_configured(self) -> bool:
        """Check if Telegram is properly configured."""
        return bool(self.api_url and self.chat_id)

    async def test_connection(self) -> bool:
        """Test Telegram connection with a test message.

        Returns:
            True if test message sent successfully
        """
        test_message = (
            "🔔 *PitchAI Monitoring Test*\n"
            f"_Connection test at {datetime.now(timezone.utc).isoformat()}_\n"
            "\n"
            "✅ Telegram notifications are working!"
        )

        return await self.send_markdown(test_message)

    async def send_markdown(self, message: str, priority: str = "normal") -> bool:
        """Send an arbitrary Markdown message."""

        return await self.send_markdown_with_metadata(message, priority=priority) is not None

    async def send_markdown_with_metadata(
        self,
        message: str,
        priority: str = "normal",
    ) -> dict[str, Any] | None:
        return await self._send_message(message, priority=priority, parse_mode="Markdown")

    async def send_html(self, message: str, priority: str = "normal") -> bool:
        return await self.send_html_with_metadata(message, priority=priority) is not None

    async def send_html_with_metadata(
        self,
        message: str,
        priority: str = "normal",
    ) -> dict[str, Any] | None:
        return await self._send_message(message, priority=priority, parse_mode="HTML")

    async def send_plain_text(self, message: str, priority: str = "normal") -> bool:
        return await self.send_markdown(message, priority=priority)


def _truncate(text: str, limit: int = 1500) -> str:
    if len(text) <= limit:
        return text
    return text[: limit - 1].rstrip() + "…"


def _extract_status(text: str | None) -> str | None:
    if not text:
        return None
    match = re.search(r"<status>(.*?)</status>", text, flags=re.IGNORECASE | re.DOTALL)
    if match:
        return match.group(1).strip().upper()
    return None


def _extract_last_assistant_message(response_items: list[dict[str, Any]] | None) -> str | None:
    if not response_items:
        return None
    for item in reversed(response_items):
        if item.get("type") == "message" and item.get("role") == "assistant":
            for content in item.get("content", []):
                if content.get("type") in {"output_text", "text"} and content.get("text"):
                    return content["text"]
    return None


def _escape_markdown(text: str) -> str:
    # Telegram "Markdown" parse_mode is sensitive to unbalanced entities, so
    # escape common special characters in arbitrary model output.
    return (
        text.replace("\\", "\\\\")
        .replace("_", "\\_")
        .replace("*", "\\*")
        .replace("`", "\\`")
        .replace("[", "\\[")
    )


def _sha256_hex(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _read_dedup_state(path: Path) -> dict[str, Any]:
    try:
        if not path.exists():
            return {}
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:  # noqa: BLE001
        return {}


def _write_dedup_state(path: Path, data: dict[str, Any]) -> None:
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    except Exception:  # noqa: BLE001
        return


def _stop_hook_should_dedup_skip(
    payload: dict[str, Any], formatted_message: str
) -> tuple[bool, str | None, Path | None]:
    state_path_raw = os.getenv("TELEGRAM_DEDUP_STATE_PATH", "").strip()
    if not state_path_raw:
        return (False, None, None)

    ttl_s_raw = os.getenv("TELEGRAM_DEDUP_TTL_S", "").strip()
    ttl_s = int(ttl_s_raw) if ttl_s_raw else 86400
    if ttl_s <= 0:
        return (False, None, Path(state_path_raw))

    state_path = Path(state_path_raw).expanduser()
    state = _read_dedup_state(state_path)
    now_epoch = int(datetime.now(timezone.utc).timestamp())

    basis = payload.get("final_message") or _extract_last_assistant_message(payload.get("response_items"))
    if not isinstance(basis, str) or not basis.strip():
        basis = formatted_message
    basis_hash = _sha256_hex(basis.strip())

    last_hash = state.get("last_basis_sha256")
    last_sent_epoch = state.get("last_sent_epoch")
    if isinstance(last_hash, str) and isinstance(last_sent_epoch, int):
        if last_hash == basis_hash and (now_epoch - last_sent_epoch) < ttl_s:
            return (True, basis_hash, state_path)

    return (False, basis_hash, state_path)

def _build_stop_hook_context(payload: dict[str, Any]) -> dict[str, Any]:
    cwd = str(payload.get("cwd") or "(unknown cwd)")
    final_message = payload.get("final_message") or _extract_last_assistant_message(payload.get("response_items"))
    final_message = _truncate(str(final_message or "(No final assistant message recorded.)").strip(), 3500)
    conversation_id = str(payload.get("conversation_id") or "").strip() or None
    dispatch_bundle = _first_env_value("PITCHAI_DISPATCH_BUNDLE", "PITCHAI_CODEX_BUNDLE")
    execution_name = _first_env_value("CONTAINER_APP_JOB_EXECUTION_NAME", "PITCHAI_JOB_EXECUTION_NAME")
    route = _lookup_session_route(conversation_id)
    return {
        "cwd": cwd,
        "project_name": _project_name_from_cwd(cwd),
        "final_message": final_message,
        "conversation_id": conversation_id,
        "dispatch_bundle": dispatch_bundle,
        "execution_name": execution_name,
        "sent_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "route": route,
    }


def _format_stop_hook_message(context: dict[str, Any]) -> str:
    route = context.get("route") if isinstance(context.get("route"), dict) else None
    conversation_id = context.get("conversation_id")
    bundle = context.get("dispatch_bundle")
    execution_name = context.get("execution_name")

    header_lines = [f"<b>{html.escape(str(context['project_name']))}</b>"]

    meta_bits = [context["sent_at_utc"]]
    if execution_name:
        meta_bits.append(str(execution_name))
    if conversation_id:
        meta_bits.append(f"conversation={conversation_id}")
    if bundle:
        meta_bits.append(f"bundle={bundle}")
    header_lines.append(f"<i>{html.escape(' | '.join(meta_bits))}</i>")

    if route:
        route_bits = []
        if route.get("ui_title"):
            route_bits.append(f"title={route['ui_title']}")
        if route.get("tmux_session"):
            route_bits.append(f"tmux={route['tmux_session']}")
        if route.get("tmux_window_index") is not None:
            route_bits.append(f"window={route['tmux_window_index']}")
        if route.get("tmux_socket_path"):
            route_bits.append(f"socket={route['tmux_socket_path']}")
        if route.get("workspace_id"):
            route_bits.append(f"workspace={route['workspace_id']}")
        if route_bits:
            header_lines.append(f"<code>{html.escape(' | '.join(route_bits))}</code>")

    body = html.escape(str(context["final_message"]))
    return "\n".join(header_lines + ["", body])


def _append_debug_log(payload: dict[str, Any], message: str) -> None:
    if not DEBUG_LOG:
        return
    try:
        path = Path(DEBUG_LOG).expanduser()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(
                f"{datetime.now(timezone.utc).isoformat()} | convo={payload.get('conversation_id')} | cwd={payload.get('cwd')}\n"
            )
            fh.write(message + "\n\n")
    except Exception as exc:  # noqa: BLE001
        logger.warning("Failed to append debug log", error=str(exc))


async def handle_stop_hook_event(payload: dict[str, Any], *, dry_run: bool = False) -> bool:
    context = _build_stop_hook_context(payload)
    message = _format_stop_hook_message(context)
    _append_debug_log(payload, message)
    if dry_run:
        print(message)
        return True

    notifier = TelegramNotifier()
    if not notifier._is_configured():  # noqa: SLF001
        logger.warning("Telegram credentials missing; skipping stop-hook notification")
        return False

    should_skip, basis_hash, state_path = _stop_hook_should_dedup_skip(payload, message)
    if should_skip:
        logger.info(
            "Skipping duplicate stop-hook telegram notification",
            conversation_id=payload.get("conversation_id"),
            cwd=payload.get("cwd"),
        )
        return True

    logger.info(
        "Sending stop-hook telegram notification",
        conversation_id=payload.get("conversation_id"),
        cwd=payload.get("cwd"),
    )
    telegram_result = await notifier.send_html_with_metadata(message)
    if telegram_result and state_path and basis_hash:
        _write_dedup_state(
            state_path,
            {"last_basis_sha256": basis_hash, "last_sent_epoch": int(datetime.now(timezone.utc).timestamp())},
        )
    if telegram_result:
        _persist_sent_final_update(payload=payload, telegram_result=telegram_result, route=context.get("route"))
        return True
    return False


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Telegram helpers for Codex hooks")
    parser.add_argument(
        "--stop-hook",
        action="store_true",
        help="Read a stop-hook payload from stdin and forward it to Telegram",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the formatted message instead of sending it",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(argv or sys.argv[1:])

    if not args.stop_hook:
        logger.error("No mode selected; pass --stop-hook when used as a Codex hook")
        return 1

    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError as exc:  # noqa: BLE001
        logger.error("Failed to parse stop-hook payload", error=str(exc))
        return 1

    try:
        asyncio.run(handle_stop_hook_event(payload, dry_run=args.dry_run))
    except Exception as exc:  # noqa: BLE001
        logger.error("Unexpected error while handling stop hook", error=str(exc))
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
