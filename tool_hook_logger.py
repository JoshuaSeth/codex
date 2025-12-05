#!/usr/bin/env python3
"""Append Codex tool hook payloads to tool_calls.jsonl."""
import json
import pathlib
import sys
from datetime import datetime, timezone

OUTPUT_PATH = pathlib.Path(__file__).with_name("tool_calls.jsonl")

def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:  # noqa: BLE001
        # best-effort logging; swallow errors so hooks never break Codex.
        sys.stderr.write(f"tool_hook_logger: failed to read payload: {exc}\n")
        return 0

    payload.setdefault("timestamp", datetime.now(tz=timezone.utc).isoformat())

    try:
        with OUTPUT_PATH.open("a", encoding="utf-8") as fh:
            json.dump(payload, fh)
            fh.write("\n")
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"tool_hook_logger: failed to append: {exc}\n")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
