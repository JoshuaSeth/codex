#!/usr/bin/env python3
"""Append Codex tool hook payloads to JSONL (path configurable)."""
from __future__ import annotations

import json
import os
import pathlib
import sys
from datetime import datetime, timezone
from typing import Iterable

DEFAULT_OUTPUT = pathlib.Path(__file__).with_name("tool_calls.jsonl")
LOG_ENV = "CODEX_TOOL_HOOK_LOG"


def resolve_output_path(argv: Iterable[str]) -> pathlib.Path:
    """Determine where to write events.

    Priority:
    1. First CLI argument (allows a custom path via tool_hook_command).
    2. Environment variable CODEX_TOOL_HOOK_LOG.
    3. Default `tool_calls.jsonl` alongside this script.
    """

    arg_iter = list(argv)
    if arg_iter:
        return pathlib.Path(arg_iter[0]).expanduser()

    env_path = os.environ.get(LOG_ENV)
    if env_path:
        return pathlib.Path(env_path).expanduser()

    return DEFAULT_OUTPUT


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except Exception as exc:  # noqa: BLE001
        # best-effort logging; swallow errors so hooks never break Codex.
        sys.stderr.write(f"tool_hook_logger: failed to read payload: {exc}\n")
        return 0

    payload.setdefault("timestamp", datetime.now(tz=timezone.utc).isoformat())

    output_path = resolve_output_path(sys.argv[1:])
    output_path.parent.mkdir(parents=True, exist_ok=True)

    try:
        with output_path.open("a", encoding="utf-8") as fh:
            json.dump(payload, fh)
            fh.write("\n")
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"tool_hook_logger: failed to append to {output_path}: {exc}\n")
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
