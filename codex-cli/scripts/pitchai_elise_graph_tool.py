#!/usr/bin/env python3
from __future__ import annotations

import base64
import hashlib
import json
import os
import ssl
import sys
import time
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional
from urllib.parse import quote

import msal
import requests


GRAPH_ROOT = "https://graph.microsoft.com/v1.0"
GRAPH_SCOPE = "https://graph.microsoft.com/.default"


def _now_utc_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _require_env(name: str) -> str:
    value = os.getenv(name)
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def _optional_env(name: str) -> Optional[str]:
    value = os.getenv(name)
    return value if value else None


def _b64decode_text(value: str) -> str:
    return base64.b64decode(value.encode("utf-8")).decode("utf-8")


def _sha1_thumbprint_from_pem(pem: str) -> str:
    der = ssl.PEM_cert_to_DER_cert(pem)
    return hashlib.sha1(der).hexdigest()


def _acquire_token(app: msal.ConfidentialClientApplication) -> str:
    result = app.acquire_token_for_client(scopes=[GRAPH_SCOPE])
    if "access_token" not in result:
        error = result.get("error")
        desc = result.get("error_description")
        raise RuntimeError(f"Token acquisition failed for Graph: {error}: {desc}")
    return str(result["access_token"])


def _request_with_retries(
    method: str,
    url: str,
    *,
    headers: Dict[str, str],
    params: Optional[Dict[str, str]] = None,
    json_body: Optional[Dict[str, Any]] = None,
    timeout_s: int = 60,
    max_attempts: int = 6,
) -> requests.Response:
    for attempt in range(1, max_attempts + 1):
        try:
            resp = requests.request(
                method,
                url,
                headers=headers,
                params=params,
                json=json_body,
                timeout=timeout_s,
            )
        except requests.RequestException as exc:
            if attempt >= max_attempts:
                raise
            wait_s = min(60, 2**attempt)
            print(f"[net] {method} {url} failed ({exc}); retrying in {wait_s}s", file=sys.stderr)
            time.sleep(wait_s)
            continue

        if resp.status_code in (429, 500, 502, 503, 504):
            if attempt >= max_attempts:
                resp.raise_for_status()
            retry_after = resp.headers.get("Retry-After")
            wait_s = int(retry_after) if retry_after and retry_after.isdigit() else min(60, 2**attempt)
            print(f"[net] {method} {url} -> {resp.status_code}; retrying in {wait_s}s", file=sys.stderr)
            time.sleep(wait_s)
            continue

        resp.raise_for_status()
        return resp

    raise RuntimeError("unreachable")


def _load_tool_args() -> Dict[str, Any]:
    raw = os.getenv("CODEX_TOOL_ARGS_JSON", "{}")
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid CODEX_TOOL_ARGS_JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise RuntimeError("CODEX_TOOL_ARGS_JSON must be a JSON object")
    return data


def _odata_quote(value: str) -> str:
    return value.replace("'", "''")


def _quote_path(value: str) -> str:
    return quote(value, safe="")


class GraphMailClient:
    def __init__(self) -> None:
        tenant_id = _require_env("PITCHAI_GRAPH_TENANT_ID")
        client_id = _require_env("PITCHAI_GRAPH_CLIENT_ID")
        private_key_pem = _b64decode_text(_require_env("PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64"))
        public_cert_pem = _b64decode_text(_require_env("PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64"))
        thumbprint = _sha1_thumbprint_from_pem(public_cert_pem)

        mailbox_upn = _require_env("PITCHAI_GRAPH_MAILBOX_UPN")
        sender_upn = _optional_env("PITCHAI_GRAPH_SENDER_UPN") or mailbox_upn

        self.mailbox_upn = mailbox_upn
        self.sender_upn = sender_upn
        self.mailbox_id = _quote_path(mailbox_upn)
        self.sender_id = _quote_path(sender_upn)

        authority = f"https://login.microsoftonline.com/{tenant_id}"
        self._app = msal.ConfidentialClientApplication(
            client_id=client_id,
            authority=authority,
            client_credential={"private_key": private_key_pem, "thumbprint": thumbprint},
        )

    def _headers(self, *, extra: Optional[Dict[str, str]] = None) -> Dict[str, str]:
        token = _acquire_token(self._app)
        base = {"Authorization": f"Bearer {token}", "Content-Type": "application/json"}
        if extra:
            base.update(extra)
        return base

    def search_messages(
        self,
        *,
        folder: str = "Inbox",
        top: int = 15,
        unread_only: bool = True,
        from_address: Optional[str] = None,
        subject_contains: Optional[str] = None,
        received_since_utc: Optional[str] = None,
    ) -> Dict[str, Any]:
        folder_id = _quote_path(folder.strip() or "Inbox")
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/mailFolders/{folder_id}/messages"

        filters: List[str] = []
        if unread_only:
            filters.append("isRead eq false")
        if from_address:
            addr = _odata_quote(from_address.strip())
            filters.append(f"from/emailAddress/address eq '{addr}'")
        if subject_contains:
            sub = _odata_quote(subject_contains.strip())
            filters.append(f"contains(subject,'{sub}')")
        if received_since_utc:
            filters.append(f"receivedDateTime ge {received_since_utc.strip()}")

        params: Dict[str, str] = {
            "$top": str(max(1, min(int(top), 50))),
            "$orderby": "receivedDateTime desc",
            "$select": "id,subject,from,receivedDateTime,isRead,hasAttachments,webLink,conversationId,internetMessageId",
        }
        if filters:
            params["$filter"] = " and ".join(filters)

        resp = _request_with_retries("GET", url, headers=self._headers(), params=params)
        data = resp.json()
        value = data.get("value", [])
        if not isinstance(value, list):
            value = []
        return {"ok": True, "folder": folder, "count": len(value), "messages": value, "ts": _now_utc_iso()}

    def read_message(self, message_id: str, *, max_chars: int = 15000, mark_as_read: bool = False) -> Dict[str, Any]:
        mid = _quote_path(message_id.strip())
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/messages/{mid}"
        params = {
            "$select": "id,subject,from,toRecipients,ccRecipients,bccRecipients,replyTo,receivedDateTime,sentDateTime,isRead,conversationId,internetMessageId,bodyPreview,body,webLink",
        }
        headers = self._headers(extra={"Prefer": 'outlook.body-content-type="text"'})
        resp = _request_with_retries("GET", url, headers=headers, params=params)
        msg = resp.json()

        body = msg.get("body") if isinstance(msg, dict) else None
        if isinstance(body, dict):
            content = body.get("content")
            if isinstance(content, str) and max_chars > 0 and len(content) > max_chars:
                body = dict(body)
                body["content"] = content[:max_chars]
                msg["body"] = body
                msg["truncated"] = True
            else:
                msg["truncated"] = False

        if mark_as_read:
            try:
                _request_with_retries(
                    "PATCH",
                    url,
                    headers=self._headers(),
                    json_body={"isRead": True},
                    timeout_s=30,
                    max_attempts=3,
                )
                msg["marked_as_read"] = True
            except Exception as exc:  # noqa: BLE001
                msg["marked_as_read"] = False
                msg["mark_as_read_error"] = str(exc)
        else:
            msg["marked_as_read"] = False

        msg["ok"] = True
        msg["ts"] = _now_utc_iso()
        return msg

    def send_email(
        self,
        *,
        to: List[str],
        subject: str,
        body: str,
        cc: Optional[List[str]] = None,
        bcc: Optional[List[str]] = None,
        reply_to: Optional[List[str]] = None,
        body_type: str = "HTML",
        save_to_sent: bool = True,
    ) -> Dict[str, Any]:
        def recipients(addrs: Optional[List[str]]) -> List[Dict[str, Any]]:
            if not addrs:
                return []
            out: List[Dict[str, Any]] = []
            for addr in addrs:
                if isinstance(addr, str) and addr.strip():
                    out.append({"emailAddress": {"address": addr.strip()}})
            return out

        message: Dict[str, Any] = {
            "subject": subject,
            "body": {"contentType": body_type, "content": body},
            "toRecipients": recipients(to),
        }
        if cc:
            message["ccRecipients"] = recipients(cc)
        if bcc:
            message["bccRecipients"] = recipients(bcc)
        if reply_to:
            message["replyTo"] = recipients(reply_to)

        # Create a draft so we can return metadata (message id, conversation id, etc.).
        draft_url = f"{GRAPH_ROOT}/users/{self.sender_id}/messages"
        draft = _request_with_retries("POST", draft_url, headers=self._headers(), json_body=message).json()
        draft_id = draft.get("id")
        if not isinstance(draft_id, str) or not draft_id:
            raise RuntimeError("Graph did not return a draft id")

        send_url = f"{GRAPH_ROOT}/users/{self.sender_id}/messages/{_quote_path(draft_id)}/send"
        _request_with_retries("POST", send_url, headers=self._headers(), timeout_s=60, max_attempts=3)

        return {
            "ok": True,
            "draft_id": draft_id,
            "internet_message_id": draft.get("internetMessageId"),
            "conversation_id": draft.get("conversationId"),
            "web_link": draft.get("webLink"),
            "sender_upn": self.sender_upn,
            "save_to_sent_requested": save_to_sent,
            "ts": _now_utc_iso(),
        }


def _op_mail_search() -> Dict[str, Any]:
    args = _load_tool_args()
    folder = str(args.get("folder") or "Inbox")
    top = int(args.get("top") or 15)

    if "unread_only" in args:
        unread_only = bool(args.get("unread_only"))
    else:
        unread_only = True

    from_address = args.get("from_address")
    subject_contains = args.get("subject_contains")
    received_since_utc = args.get("received_since_utc")

    client = GraphMailClient()
    return client.search_messages(
        folder=folder,
        top=top,
        unread_only=unread_only,
        from_address=from_address if isinstance(from_address, str) and from_address.strip() else None,
        subject_contains=subject_contains if isinstance(subject_contains, str) and subject_contains.strip() else None,
        received_since_utc=received_since_utc if isinstance(received_since_utc, str) and received_since_utc.strip() else None,
    )


def _op_mail_read() -> Dict[str, Any]:
    args = _load_tool_args()
    message_id = args.get("message_id")
    if not isinstance(message_id, str) or not message_id.strip():
        raise RuntimeError("Missing required parameter: message_id")

    max_chars = int(args.get("max_chars") or 15000)
    mark_as_read = bool(args.get("mark_as_read") or False)

    client = GraphMailClient()
    return client.read_message(message_id, max_chars=max_chars, mark_as_read=mark_as_read)


def _op_send_email() -> Dict[str, Any]:
    args = _load_tool_args()
    to = args.get("to")
    subject = args.get("subject")
    body = args.get("body")

    if not isinstance(to, list) or not to:
        raise RuntimeError("Missing required parameter: to (array)")
    to_addrs = [str(x).strip() for x in to if isinstance(x, str) and x.strip()]
    if not to_addrs:
        raise RuntimeError("Parameter to[] must contain at least one email address")

    if not isinstance(subject, str) or not subject.strip():
        raise RuntimeError("Missing required parameter: subject")
    if not isinstance(body, str) or not body.strip():
        raise RuntimeError("Missing required parameter: body")

    cc = args.get("cc")
    bcc = args.get("bcc")
    reply_to = args.get("reply_to")
    body_type = str(args.get("body_type") or "HTML")

    if "save_to_sent" in args:
        save_to_sent = bool(args.get("save_to_sent"))
    else:
        save_to_sent = True

    client = GraphMailClient()
    return client.send_email(
        to=to_addrs,
        cc=[str(x).strip() for x in cc if isinstance(cc, list) and isinstance(x, str) and x.strip()] if isinstance(cc, list) else None,
        bcc=[str(x).strip() for x in bcc if isinstance(bcc, list) and isinstance(x, str) and x.strip()] if isinstance(bcc, list) else None,
        reply_to=[str(x).strip() for x in reply_to if isinstance(reply_to, list) and isinstance(x, str) and x.strip()]
        if isinstance(reply_to, list)
        else None,
        subject=subject.strip(),
        body=body,
        body_type=body_type,
        save_to_sent=save_to_sent,
    )


def main(argv: List[str] | None = None) -> int:
    argv = argv or sys.argv[1:]
    if len(argv) < 1:
        print("Usage: elise_graph_tool.py <mail_search|mail_read|send_email>", file=sys.stderr)
        return 2

    op = argv[0].strip()
    ops = {
        "mail_search": _op_mail_search,
        "mail_read": _op_mail_read,
        "send_email": _op_send_email,
    }
    fn = ops.get(op)
    if fn is None:
        print(f"Unknown operation: {op}", file=sys.stderr)
        return 2

    try:
        result = fn()
    except Exception as exc:  # noqa: BLE001
        print(f"[error] {exc}", file=sys.stderr)
        return 1

    print(json.dumps(result, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

