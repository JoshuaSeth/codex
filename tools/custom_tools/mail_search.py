#!/usr/bin/env python3
"""Search Elise's mailbox via Microsoft Graph and list messages."""

from __future__ import annotations

import json
import os
import sys
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List, Optional

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

from standalone_mail_sender.graph_email import GraphEmailClient, GraphApiError  # noqa: E402


class MailClient(GraphEmailClient):
    def list_messages(
        self,
        *,
        folder: Optional[str] = None,
        search: Optional[str] = None,
        unread_only: bool = False,
        top: int = 10,
    ) -> List[Dict[str, Any]]:
        params: Dict[str, Any] = {
            "$top": min(max(top, 1), 50),
            "$select": "id,subject,from,receivedDateTime,internetMessageId,isRead,bodyPreview",
        }
        filters: List[str] = []
        headers: Dict[str, str] = {}

        if unread_only:
            filters.append("isRead eq false")
        if filters:
            params["$filter"] = " and ".join(filters)
        if search:
            headers["ConsistencyLevel"] = "eventual"
            params["$search"] = search
        else:
            params["$orderby"] = "receivedDateTime desc"

        endpoint = "/me/messages"
        if folder:
            endpoint = f"/me/mailFolders/{folder}/messages"

        response = self._request("GET", endpoint, params=params, headers=headers)
        payload = response.json()
        return payload.get("value", [])


def normalize_query(raw: Optional[str]) -> Optional[str]:
    if raw is None:
        return None
    raw = raw.strip()
    if not raw or raw == "*":
        return None
    return f'"{raw}"'


def main() -> int:
    args_json = os.environ.get("CODEX_TOOL_ARGS_JSON")
    if not args_json:
        print("CODEX_TOOL_ARGS_JSON missing", file=sys.stderr)
        return 1

    try:
        request = json.loads(args_json)
    except json.JSONDecodeError as exc:  # pragma: no cover - defensive
        print(f"Invalid JSON in CODEX_TOOL_ARGS_JSON: {exc}", file=sys.stderr)
        return 1

    query = request.get("query")
    folder = request.get("folder")
    unread_only = request.get("unread_only", True)
    top = request.get("limit", 10)

    client = MailClient(save_to_sent=False)
    try:
        messages = client.list_messages(
            folder=folder,
            search=normalize_query(query),
            unread_only=unread_only,
            top=top,
        )
    except GraphApiError as exc:
        print(f"Graph API error: {exc}", file=sys.stderr)
        return 1

    results: List[Dict[str, Any]] = []
    for msg in messages:
        sender = (msg.get("from") or {}).get("emailAddress") or {}
        results.append(
            {
                "message_id": msg.get("id"),
                "internet_message_id": msg.get("internetMessageId"),
                "subject": msg.get("subject"),
                "from": sender.get("address"),
                "from_name": sender.get("name"),
                "received": msg.get("receivedDateTime"),
                "is_read": msg.get("isRead"),
                "preview": (msg.get("bodyPreview") or "").strip(),
            }
        )

    if not results:
        print("No messages found matching the criteria.")
        return 0

    print("Found messages:\n")
    for item in results:
        received = item.get("received")
        try:
            if received:
                received = datetime.fromisoformat(received.replace("Z", "+00:00")).strftime("%Y-%m-%d %H:%M UTC")
        except ValueError:
            pass
        print(
            f"- Subject: {item.get('subject') or '(no subject)'}\n"
            f"  From: {item.get('from_name') or ''} <{item.get('from') or ''}>\n"
            f"  Received: {received}\n"
            f"  Internet ID: {item.get('internet_message_id')}\n"
            f"  Message ID: {item.get('message_id')}\n"
            f"  Preview: {(item.get('preview') or '')[:240]}"
        )
        print()

    print(json.dumps({"results": results}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
