#!/usr/bin/env python3
"""Read a specific email from Elise's mailbox via Microsoft Graph."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any, Dict, Optional

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
    def get_message(self, message_id: str, select: Optional[str] = None) -> Dict[str, Any]:
        params: Dict[str, Any] = {}
        if select:
            params["$select"] = select
        response = self._request("GET", f"/me/messages/{message_id}", params=params)
        return response.json()

    def find_by_internet_id(self, internet_id: str) -> Optional[Dict[str, Any]]:
        params = {
            "$top": 1,
            "$filter": f"internetMessageId eq '{internet_id}'",
            "$select": "id,subject,from,receivedDateTime,internetMessageId,isRead",
        }
        response = self._request("GET", "/me/messages", params=params)
        value = response.json().get("value", [])
        return value[0] if value else None

    def mark_as_read(self, message_id: str) -> None:
        self._request("PATCH", f"/me/messages/{message_id}", json={"isRead": True})


def main() -> int:
    args_json = os.environ.get("CODEX_TOOL_ARGS_JSON")
    if not args_json:
        print("CODEX_TOOL_ARGS_JSON missing", file=sys.stderr)
        return 1
    try:
        request = json.loads(args_json)
    except json.JSONDecodeError as exc:
        print(f"Invalid JSON in CODEX_TOOL_ARGS_JSON: {exc}", file=sys.stderr)
        return 1

    message_id = request.get("message_id")
    internet_id = request.get("internet_message_id")
    mark_as_read = request.get("mark_as_read", False)
    prefer_html = request.get("prefer_html", True)

    if not message_id and not internet_id:
        print("Provide either message_id or internet_message_id", file=sys.stderr)
        return 1

    client = MailClient(save_to_sent=False)

    if not message_id:
        lookup = client.find_by_internet_id(internet_id)
        if not lookup:
            print("No message found for the supplied internet_message_id", file=sys.stderr)
            return 1
        message_id = lookup.get("id")

    try:
        message = client.get_message(message_id, select=None)
    except GraphApiError as exc:
        print(f"Graph API error: {exc}", file=sys.stderr)
        return 1

    if mark_as_read and not message.get("isRead"):
        try:
            client.mark_as_read(message_id)
            message["isRead"] = True
        except GraphApiError as exc:
            print(f"Failed to mark as read: {exc}", file=sys.stderr)

    sender = (message.get("from") or {}).get("emailAddress") or {}
    body = message.get("body") or {}
    content_type = body.get("contentType")
    content = body.get("content") or ""
    if not prefer_html and content_type == "html":
        # Basic HTML -> text fallback without additional dependencies.
        import re
        from html import unescape

        text = re.sub(r"<\s*br\s*/?>", "\n", content, flags=re.IGNORECASE)
        text = re.sub(r"<[^>]+>", "", text)
        content_text = unescape(text)
    else:
        content_text = content

    output = {
        "message_id": message.get("id"),
        "internet_message_id": message.get("internetMessageId"),
        "subject": message.get("subject"),
        "from": sender.get("address"),
        "from_name": sender.get("name"),
        "received": message.get("receivedDateTime"),
        "is_read": message.get("isRead"),
        "body_type": content_type,
        "body": content_text.strip(),
    }

    print(json.dumps(output, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
