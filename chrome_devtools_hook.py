#!/usr/bin/env python3
"""Hook that records chrome-devtools MCP tool calls to JSON Lines."""
import json
import pathlib
import sys
from datetime import datetime, timezone

OUTPUT_PATH = pathlib.Path(__file__).with_name("chrome_devtools_actions.jsonl")


def log_error(message: str) -> None:
    sys.stderr.write(f"chrome_devtools_hook: {message}\n")


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:  # noqa: BLE001
        log_error(f"failed to read JSON payload: {exc}")
        return 1

    call = payload.get("call") or {}
    tool_name = call.get("tool_name", "")
    if tool_name.startswith("mcp__chrome-devtools"):
        payload.setdefault("logged_at", datetime.now(tz=timezone.utc).isoformat())
        try:
            with OUTPUT_PATH.open("a", encoding="utf-8") as fh:
                json.dump(payload, fh)
                fh.write("\n")
        except Exception as exc:  # noqa: BLE001
            log_error(f"failed to append payload: {exc}")
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
