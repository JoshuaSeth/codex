#!/usr/bin/env python3
"""Send email via Graph and capture metadata for Codex custom tools."""

from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable, List

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
if str(ELISE_CORE_ROOT) not in sys.path:
    sys.path.insert(0, str(ELISE_CORE_ROOT))

from standalone_mail_sender.graph_email import GraphApiError, GraphEmailClient

OUTBOX_DIR = ELISE_CORE_ROOT / "logs"
OUTBOX_INDEX = OUTBOX_DIR / "elise_outbox_index.json"
OUTBOX_LOG = OUTBOX_DIR / "elise_outbox_log.jsonl"


def _ensure_list(value: object, field: str) -> List[str]:
    if value is None:
        return []
    if isinstance(value, str):
        value = value.strip()
        return [value] if value else []
    if isinstance(value, Iterable):
        result: List[str] = []
        for item in value:
            if not isinstance(item, str):
                raise ValueError(f"{field} entries must be strings; got {type(item)!r}")
            item = item.strip()
            if item:
                result.append(item)
        return result
    raise ValueError(f"{field} must be a string or list of strings")


def _load_json(path: Path) -> dict:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text())
    except json.JSONDecodeError:
        return {}


def _write_index(index: dict) -> None:
    OUTBOX_DIR.mkdir(parents=True, exist_ok=True)
    OUTBOX_INDEX.write_text(json.dumps(index, indent=2), encoding="utf-8")


def _append_log(entry: dict) -> None:
    OUTBOX_DIR.mkdir(parents=True, exist_ok=True)
    with OUTBOX_LOG.open("a", encoding="utf-8") as handle:
        json.dump(entry, handle)
        handle.write("\n")


def main() -> int:
    args_json = os.environ.get("CODEX_TOOL_ARGS_JSON")
    if not args_json:
        print("CODEX_TOOL_ARGS_JSON is missing", file=sys.stderr)
        return 1
    try:
        request = json.loads(args_json)
    except json.JSONDecodeError as exc:
        print(f"Invalid JSON in CODEX_TOOL_ARGS_JSON: {exc}", file=sys.stderr)
        return 1

    try:
        to_values = _ensure_list(request.get("to"), "to")
        if not to_values:
            raise ValueError("'to' is required and must contain at least one email address")
        subject = request.get("subject")
        if not subject:
            raise ValueError("'subject' is required")
        body = request.get("body")
        if not body:
            raise ValueError("'body' is required")
        cc_values = _ensure_list(request.get("cc"), "cc")
        reply_to_values = _ensure_list(request.get("reply_to"), "reply_to")
        raw_from = request.get("from") or os.environ.get("CODEX_DEFAULT_FROM_EMAIL") or "elise@pitchai.net"
        from_address = raw_from.strip()
    except ValueError as exc:
        print(f"Input error: {exc}", file=sys.stderr)
        return 1

    client = GraphEmailClient(save_to_sent=True)

    try:
        result = client.send_email_with_metadata(
            subject=subject,
            body=body,
            to_recipients=to_values,
            cc_recipients=cc_values,
            reply_to=reply_to_values,
            from_address=from_address,
            body_type="HTML",
            signature_name="elise",
        )
    except GraphApiError as exc:  # noqa: BLE001
        print(f"Graph API error: {exc}", file=sys.stderr)
        return 1

    timestamp = datetime.now(timezone.utc).isoformat()
    entry = {
        "timestamp": timestamp,
        "conversation_id": os.environ.get("CODEX_CONVERSATION_ID"),
        "turn_id": os.environ.get("CODEX_TURN_ID"),
        "turn_cwd": os.environ.get("CODEX_TURN_CWD"),
        "tool_call_id": os.environ.get("CODEX_TOOL_CALL_ID"),
        "tool_name": os.environ.get("CODEX_TOOL_NAME"),
        "config_file": os.environ.get("CODEX_CONFIG_FILE"),
        "subject": subject,
        "to": to_values,
        "cc": cc_values,
        "reply_to": reply_to_values,
        "from": from_address,
        "graph_message_id": result.message_id,
        "internet_message_id": result.internet_message_id,
        "graph_conversation_id": result.conversation_id,
        "web_link": result.web_link,
        "status": "sent",
        "wait_call_id": None,
        "wait_registered_at": None,
    }

    index = _load_json(OUTBOX_INDEX)
    if result.internet_message_id:
        index[result.internet_message_id] = entry
        _write_index(index)
    _append_log(entry)

    internet_id = result.internet_message_id or "unknown"
    summary = (
        f"Sent email to {', '.join(to_values)} "
        f"(Graph id: {result.message_id}; internet id: {internet_id}).\n"
        "Use wait_for_email_response with that internet id if you need to wait for the reply."
    )
    print(summary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
