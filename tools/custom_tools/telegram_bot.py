"""Telegram notification system + Codex hook bridge."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import re
import sys
from datetime import datetime
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


class TelegramNotifier:
    """Handles Telegram notifications for monitoring alerts."""

    def __init__(self, bot_token: str | None = None, chat_id: str | None = None):
        """Initialize Telegram notifier.

        Args:
            bot_token: Telegram bot token (or from TELEGRAM_BOT_TOKEN env)
            chat_id: Telegram chat ID (or from TELEGRAM_CHAT_ID env)
        """
        env_bot_token = _get_env_value("TELEGRAM_BOT_TOKEN")
        env_chat_id = _get_env_value("TELEGRAM_CHAT_ID")

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
        return await self._send_message(message)

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
        return await self._send_message(message, priority="high")

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
        return await self._send_message(message)

    def _format_daily_report(self, report: dict[str, Any]) -> str:
        """Format daily report for Telegram."""
        timestamp = report.get("timestamp", datetime.utcnow().isoformat())

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
            f"*Time:* {alert.get('timestamp', datetime.utcnow().isoformat())}",
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

    async def _send_message(self, message: str, priority: str = "normal") -> bool:
        """Send message via Telegram.

        Args:
            message: Formatted message to send
            priority: Message priority (normal/high)

        Returns:
            True if sent successfully
        """
        if not self._is_configured():
            logger.warning("Telegram not configured, skipping notification")
            return False

        try:
            # Add priority indicator for high priority
            if priority == "high":
                message = "‼️ " + message

            payload = json.dumps(
                {
                    "chat_id": self.chat_id,
                    "text": message,
                    "parse_mode": "Markdown",
                    "disable_web_page_preview": True,
                }
            ).encode("utf-8")

            headers = {"Content-Type": "application/json"}
            request = urllib_request.Request(self.api_url, data=payload, headers=headers)

            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, lambda: urllib_request.urlopen(request, timeout=15).read())

            logger.info("Telegram notification sent", priority=priority)
            return True

        except urllib_error.URLError as exc:
            logger.error("Failed to send Telegram notification", error=str(exc))
            return False
        except Exception as exc:  # noqa: BLE001
            logger.error("Unexpected error sending Telegram notification", error=str(exc))
            return False

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
            f"_Connection test at {datetime.utcnow().isoformat()}_\n"
            "\n"
            "✅ Telegram notifications are working!"
        )

        return await self._send_message(test_message)

    async def send_plain_text(self, message: str, priority: str = "normal") -> bool:
        """Send an arbitrary Markdown message."""

        return await self._send_message(message, priority=priority)


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


def _format_stop_hook_message(payload: dict[str, Any]) -> str:
    conversation_id = payload.get("conversation_id", "unknown")
    turn_id = payload.get("turn_id", "unknown")
    cwd = payload.get("cwd", "(unknown cwd)")
    final_message = payload.get("final_message") or _extract_last_assistant_message(
        payload.get("response_items")
    )
    final_message = final_message or "(No final assistant message recorded.)"
    final_message = final_message.strip()
    final_message = _truncate(final_message, 1500)

    token_usage = payload.get("token_usage") or {}
    total_tokens = token_usage.get("total_tokens")
    input_tokens = token_usage.get("input_tokens")
    output_tokens = token_usage.get("output_tokens")
    reasoning_tokens = token_usage.get("reasoning_output_tokens")

    status = _extract_status(final_message)

    project_name = cwd
    try:
        project_name = str(Path(cwd).resolve())
    except Exception:  # noqa: BLE001
        project_name = cwd

    lines: list[str] = [f"*Project:* {project_name}"]

    if status:
        lines.append(f"*Status:* {status}")

    # if total_tokens is not None:
    #     lines.append(
    #         "📊 Tokens — total {total} (in {inp}, out {out}, reasoning {reason})".format(
    #             total=total_tokens,
    #             inp=input_tokens if input_tokens is not None else "?",
    #             out=output_tokens if output_tokens is not None else "?",
    #             reason=reasoning_tokens if reasoning_tokens is not None else "?",
    #         )
    #     )

    if lines:
        lines.append("")
    lines.append(final_message)
    return "\n".join(lines)


def _append_debug_log(payload: dict[str, Any], message: str) -> None:
    if not DEBUG_LOG:
        return
    try:
        path = Path(DEBUG_LOG).expanduser()
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as fh:
            fh.write(
                f"{datetime.utcnow().isoformat()} | convo={payload.get('conversation_id')} | cwd={payload.get('cwd')}\n"
            )
            fh.write(message + "\n\n")
    except Exception as exc:  # noqa: BLE001
        logger.warning("Failed to append debug log", error=str(exc))


async def handle_stop_hook_event(payload: dict[str, Any], *, dry_run: bool = False) -> bool:
    message = _format_stop_hook_message(payload)
    _append_debug_log(payload, message)
    if dry_run:
        print(message)
        return True

    notifier = TelegramNotifier()
    if not notifier._is_configured():  # noqa: SLF001
        logger.warning("Telegram credentials missing; skipping stop-hook notification")
        return False

    logger.info(
        "Sending stop-hook telegram notification",
        conversation_id=payload.get("conversation_id"),
        cwd=payload.get("cwd"),
    )
    return await notifier.send_plain_text(message)


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
