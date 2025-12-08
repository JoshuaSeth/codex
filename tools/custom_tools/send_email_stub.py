#!/usr/bin/env python3
"""Developer-friendly email stub that appends JSONL records to a log file."""

from __future__ import annotations

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict

LOG_ENV = "CODEX_SEND_EMAIL_LOG"


def _load_args() -> Dict[str, Any]:
    raw = os.environ.get("CODEX_TOOL_ARGS_JSON", "{}")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:  # pragma: no cover - defensive
        return {"error": f"invalid arguments: {exc}"}


def main() -> None:
    args = _load_args()
    record = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "subject": args.get("subject", ""),
        "body": args.get("body", ""),
        "recipients": args.get("recipients", ["boss@example.com"]),
        "metadata": {k: v for k, v in args.items() if k not in {"subject", "body", "recipients"}},
    }

    log_path = Path(os.environ.get(LOG_ENV) or Path.home() / ".codex" / "dev_emails.jsonl")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    with log_path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(record) + "\n")

    print(json.dumps({"status": "queued", "path": str(log_path)}))


if __name__ == "__main__":
    main()
