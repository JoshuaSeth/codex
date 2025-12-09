#!/usr/bin/env python3
"""Elise-specific tool hook: log DevTools calls + extend sleep timeouts."""
from __future__ import annotations

import json
import pathlib
import sys
from datetime import datetime, timezone
from typing import Any, Dict, List, Sequence, Union

DEVTOOLS_LOG = pathlib.Path(__file__).with_name("chrome_devtools_actions.jsonl")
SLEEP_DIRECTIVE = {"local_shell": {"timeout_ms": "infinite"}}
DEBUG_LOG = pathlib.Path.home() / ".codex" / "elise_sleep_hook_debug.jsonl"


def log_error(message: str) -> None:
    sys.stderr.write(f"elise_tool_hook: {message}\n")


def log_debug(label: str, info: Dict[str, Any]) -> None:
    record = {
        "label": label,
        "info": info,
        "logged_at": datetime.now(tz=timezone.utc).isoformat(),
    }
    try:
        DEBUG_LOG.parent.mkdir(parents=True, exist_ok=True)
        with DEBUG_LOG.open("a", encoding="utf-8") as fh:
            json.dump(record, fh)
            fh.write("\n")
    except OSError:
        pass


def load_event() -> Dict[str, Any]:
    try:
        return json.load(sys.stdin)
    except Exception as exc:  # noqa: BLE001
        raise SystemExit(f"failed to read JSON payload: {exc}")


def log_devtools(event: Dict[str, Any]) -> None:
    call = event.get("call") or {}
    tool_name = call.get("tool_name", "")
    if not tool_name.startswith("mcp__chrome-devtools"):
        return
    entry = dict(event)
    entry.setdefault("logged_at", datetime.now(tz=timezone.utc).isoformat())
    try:
        with DEVTOOLS_LOG.open("a", encoding="utf-8") as fh:
            json.dump(entry, fh)
            fh.write("\n")
    except OSError as exc:
        log_error(f"failed to append payload: {exc}")


def command_has_sleep(command: Union[str, Sequence[str], None]) -> bool:
    if command is None:
        return False
    if isinstance(command, str):
        text = command.strip()
        if not text:
            return False
        first = text.split()[0]
        name = pathlib.Path(first).name.lower()
        lowered = text.lower()
        return name == "sleep" or lowered.startswith("sleep ")
    try:
        for token in command:
            if not token:
                continue
            stripped = str(token).strip()
            if not stripped:
                continue
            # token could already include arguments ("sleep 12")
            first = stripped.split()[0]
            name = pathlib.Path(first).name.lower()
            if name == "sleep":
                return True
    except TypeError:
        return False
    return False


def maybe_emit_sleep_directive(event: Dict[str, Any]) -> None:
    if event.get("phase") != "before_execution":
        return
    call = event.get("call") or {}
    payload = call.get("payload") or {}
    tool_name = (call.get("tool_name") or "").lower()

    if payload.get("kind") == "local_shell":
        command = payload.get("command") or []
        text = " ".join(map(str, command)).lower()
        if "sleep" in text:
            log_debug("local_shell", {"command": command, "tool_name": tool_name})
        if command_has_sleep(command):
            sys.stdout.write(json.dumps(SLEEP_DIRECTIVE))
            sys.stdout.write("\n")
            sys.stdout.flush()
        return

    if payload.get("kind") == "function" and tool_name in {"shell_command", "shell"}:
        parsed = payload.get("parsed_arguments")
        if parsed is None:
            try:
                parsed = json.loads(payload.get("arguments") or "{}")
            except json.JSONDecodeError:
                parsed = None
        command_value = parsed.get("command") if isinstance(parsed, dict) else None
        if command_value is not None and "sleep" in str(command_value).lower():
            log_debug(
                "shell_command",
                {"command": command_value, "payload_kind": payload.get("kind"), "tool_name": tool_name},
            )
        if isinstance(parsed, dict) and command_has_sleep(command_value):
            sys.stdout.write(json.dumps(SLEEP_DIRECTIVE))
            sys.stdout.write("\n")
            sys.stdout.flush()


def main() -> None:
    event = load_event()
    log_devtools(event)
    maybe_emit_sleep_directive(event)


if __name__ == "__main__":
    main()
