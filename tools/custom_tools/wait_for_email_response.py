#!/usr/bin/env python3
"""Register pending email replies and hibernate until replies arrive."""

from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List

def _resolve_elise_core_root() -> Path:
    env_root = os.environ.get("CODEX_ELISE_CORE_ROOT") or os.environ.get("ELISE_CORE_ROOT")
    if env_root:
        return Path(env_root).expanduser().resolve()

    script_path = Path(__file__).resolve()
    for parent in script_path.parents:
        candidate = parent / "Elise" / "core"
        if candidate.exists():
            return candidate.resolve()

    raise SystemExit(
        "Elise core repo not found. Set CODEX_ELISE_CORE_ROOT (or ELISE_CORE_ROOT) "
        "to the absolute path of the Elise/core checkout."
    )


ELISE_CORE_ROOT = _resolve_elise_core_root()
OUTBOX_DIR = ELISE_CORE_ROOT / "logs"
OUTBOX_INDEX = OUTBOX_DIR / "elise_outbox_index.json"


def load_index() -> Dict[str, dict]:
    if not OUTBOX_INDEX.exists():
        return {}
    try:
        return json.loads(OUTBOX_INDEX.read_text())
    except json.JSONDecodeError:
        return {}


def save_index(index: Dict[str, dict]) -> None:
    OUTBOX_DIR.mkdir(parents=True, exist_ok=True)
    OUTBOX_INDEX.write_text(json.dumps(index, indent=2), encoding="utf-8")


def normalize_ids(payload: dict) -> List[str]:
    ids: List[str] = []
    if "message_id" in payload and payload["message_id"]:
        ids.append(str(payload["message_id"]).strip())
    for value in payload.get("message_ids", []) or []:
        if value:
            ids.append(str(value).strip())
    return [mid for mid in ids if mid]


def main() -> int:
    args_json = os.environ.get("CODEX_TOOL_ARGS_JSON")
    if not args_json:
        print("CODEX_TOOL_ARGS_JSON missing", file=sys.stderr)
        return 1
    try:
        payload = json.loads(args_json)
    except json.JSONDecodeError as exc:
        print(f"Invalid JSON: {exc}", file=sys.stderr)
        return 1

    ids = normalize_ids(payload)
    if not ids:
        print("Provide at least one message_id or message_ids entry", file=sys.stderr)
        return 1

    index = load_index()
    timestamp = datetime.now(timezone.utc).isoformat()
    convo_id = os.environ.get("CODEX_CONVERSATION_ID")
    turn_id = os.environ.get("CODEX_TURN_ID")
    cwd = os.environ.get("CODEX_TURN_CWD")
    config_file = os.environ.get("CODEX_CONFIG_FILE")
    tool_call_id = os.environ.get("CODEX_TOOL_CALL_ID")

    registered: List[str] = []
    missing: List[str] = []
    for internet_id in ids:
        entry = index.get(internet_id)
        if not entry:
            missing.append(internet_id)
            continue
        entry.setdefault("conversation_id", convo_id)
        entry["wait_call_id"] = tool_call_id
        entry["wait_registered_at"] = timestamp
        entry["wait_turn_id"] = turn_id
        entry["wait_cwd"] = cwd
        entry["wait_config_file"] = config_file
        entry["status"] = "waiting_reply"
        registered.append(internet_id)

    if registered:
        save_index(index)

    parts = []
    if registered:
        parts.append(
            "Waiting for replies to the following internet ids: "
            + ", ".join(registered)
        )
    if missing:
        parts.append(
            "No record found for: " + ", ".join(missing)
        )
    print("\n".join(parts))
    return 0 if registered else 1


if __name__ == "__main__":
    sys.exit(main())
