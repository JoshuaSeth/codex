#!/usr/bin/env python3
from __future__ import annotations

import base64
import hashlib
import ipaddress
import json
import os
import re
import socket
import ssl
import sys
import time
import warnings
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Tuple
from urllib.parse import quote, urlparse

warnings.filterwarnings("ignore", message="urllib3 v2 only supports OpenSSL*", category=Warning)

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


def _normalize_categories(raw: Any) -> List[str]:
    if not isinstance(raw, list):
        raise RuntimeError("categories must be an array of strings")
    out: List[str] = []
    for item in raw:
        if not isinstance(item, str):
            continue
        val = item.strip()
        if not val:
            continue
        if val not in out:
            out.append(val)
        if len(out) >= 30:
            break
    if not out:
        raise RuntimeError("categories must contain at least one non-empty string")
    return out


class GraphMailClient:
    def __init__(self) -> None:
        tenant_id = _require_env("PITCHAI_GRAPH_TENANT_ID")
        client_id = _require_env("PITCHAI_GRAPH_CLIENT_ID")
        private_key_pem = _b64decode_text(_require_env("PITCHAI_GRAPH_CERT_PRIVATE_KEY_B64"))
        public_cert_pem = _b64decode_text(_require_env("PITCHAI_GRAPH_CERT_PUBLIC_CERT_B64"))
        thumbprint = _sha1_thumbprint_from_pem(public_cert_pem)

        mailbox_upn = _require_env("PITCHAI_GRAPH_MAILBOX_UPN")

        self.mailbox_upn = mailbox_upn
        self.mailbox_id = _quote_path(mailbox_upn)

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
        untagged_only: bool = False,
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
        if received_since_utc:
            filters.append(f"receivedDateTime ge {received_since_utc.strip()}")

        requested_top = max(1, min(int(top), 50))
        fetch_top = 50 if untagged_only and requested_top < 50 else requested_top

        params: Dict[str, str] = {
            "$top": str(fetch_top),
            "$orderby": "receivedDateTime desc",
            "$select": "id,subject,from,receivedDateTime,isRead,hasAttachments,webLink,conversationId,internetMessageId,categories",
        }
        if filters:
            params["$filter"] = " and ".join(filters)

        resp = _request_with_retries("GET", url, headers=self._headers(), params=params)
        data = resp.json()
        value = data.get("value", [])
        if not isinstance(value, list):
            value = []

        if subject_contains and subject_contains.strip():
            needle = subject_contains.strip().lower()
            value = [
                msg
                for msg in value
                if isinstance(msg, dict) and isinstance(msg.get("subject"), str) and needle in msg["subject"].lower()
            ]

        if untagged_only:
            def is_untagged(msg: Any) -> bool:
                if not isinstance(msg, dict):
                    return False
                cats = msg.get("categories")
                return not cats or (isinstance(cats, list) and len(cats) == 0)

            value = [msg for msg in value if is_untagged(msg)]

        value = value[:requested_top]

        return {
            "ok": True,
            "folder": folder,
            "count": len(value),
            "messages": value,
            "mailbox_upn": self.mailbox_upn,
            "ts": _now_utc_iso(),
        }

    def read_message(self, message_id: str, *, max_chars: int = 15000) -> Dict[str, Any]:
        mid = _quote_path(message_id.strip())
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/messages/{mid}"
        params = {
            "$select": "id,subject,from,toRecipients,ccRecipients,replyTo,receivedDateTime,sentDateTime,isRead,categories,conversationId,internetMessageId,webLink,bodyPreview,body,internetMessageHeaders",
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

        msg["ok"] = True
        msg["ts"] = _now_utc_iso()
        msg["mailbox_upn"] = self.mailbox_upn
        return msg

    def get_categories(self, message_id: str) -> List[str]:
        mid = _quote_path(message_id.strip())
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/messages/{mid}"
        resp = _request_with_retries("GET", url, headers=self._headers(), params={"$select": "categories"})
        data = resp.json()
        cats = data.get("categories") if isinstance(data, dict) else None
        if not cats:
            return []
        if isinstance(cats, list):
            return [str(x) for x in cats if isinstance(x, str) and x.strip()]
        return []

    def set_categories(self, message_id: str, categories: List[str]) -> Dict[str, Any]:
        mid = _quote_path(message_id.strip())
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/messages/{mid}"
        _request_with_retries(
            "PATCH",
            url,
            headers=self._headers(),
            json_body={"categories": categories},
            timeout_s=30,
            max_attempts=3,
        )
        return {"ok": True, "message_id": message_id, "categories": categories, "ts": _now_utc_iso()}

    def set_read_state(self, message_id: str, *, is_read: bool) -> Dict[str, Any]:
        mid = _quote_path(message_id.strip())
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/messages/{mid}"
        _request_with_retries(
            "PATCH",
            url,
            headers=self._headers(),
            json_body={"isRead": bool(is_read)},
            timeout_s=30,
            max_attempts=3,
        )
        return {"ok": True, "message_id": message_id, "is_read": bool(is_read), "ts": _now_utc_iso()}

    def read_headers(self, message_id: str) -> Dict[str, str]:
        mid = _quote_path(message_id.strip())
        url = f"{GRAPH_ROOT}/users/{self.mailbox_id}/messages/{mid}"
        resp = _request_with_retries(
            "GET",
            url,
            headers=self._headers(),
            params={"$select": "internetMessageHeaders"},
        )
        data = resp.json()
        raw = data.get("internetMessageHeaders") if isinstance(data, dict) else None
        if not isinstance(raw, list):
            return {}
        out: Dict[str, str] = {}
        for item in raw:
            if not isinstance(item, dict):
                continue
            name = item.get("name")
            value = item.get("value")
            if isinstance(name, str) and isinstance(value, str) and name.strip():
                out[name.strip().lower()] = value
        return out


def _resolve_public_ips(hostname: str) -> List[str]:
    try:
        infos = socket.getaddrinfo(hostname, None)
    except Exception:
        return []
    ips: List[str] = []
    for family, _, _, _, sockaddr in infos:
        if family == socket.AF_INET:
            ip = sockaddr[0]
        elif family == socket.AF_INET6:
            ip = sockaddr[0]
        else:
            continue
        if ip not in ips:
            ips.append(ip)
    return ips


def _is_private_or_reserved_ip(ip: str) -> bool:
    try:
        addr = ipaddress.ip_address(ip)
    except ValueError:
        return True
    return bool(
        addr.is_private
        or addr.is_loopback
        or addr.is_link_local
        or addr.is_reserved
        or addr.is_multicast
        or addr.is_unspecified
    )


def _validate_unsubscribe_url(url: str) -> Tuple[bool, str]:
    parsed = urlparse(url)
    if parsed.scheme not in ("http", "https"):
        return (False, f"unsupported scheme: {parsed.scheme}")
    if not parsed.netloc:
        return (False, "missing host")

    host = parsed.hostname or ""
    if not host:
        return (False, "missing host")

    try:
        ipaddress.ip_address(host)
        ip_literal = True
    except ValueError:
        ip_literal = False

    if ip_literal and _is_private_or_reserved_ip(host):
        return (False, "blocked ip literal (private/reserved)")

    if not ip_literal:
        resolved = _resolve_public_ips(host)
        if not resolved:
            return (False, "dns resolution failed")
        for ip in resolved:
            if _is_private_or_reserved_ip(ip):
                return (False, f"blocked dns ip (private/reserved): {ip}")

    port = parsed.port
    if port is not None and port not in (80, 443):
        return (False, f"blocked port: {port}")

    return (True, "ok")


def _extract_list_unsubscribe_urls(value: str) -> List[str]:
    candidates = re.findall(r"<([^>]+)>", value)
    if not candidates:
        candidates = [v.strip() for v in value.split(",")]
    out: List[str] = []
    for c in candidates:
        u = c.strip().strip('\"').strip()
        if not u:
            continue
        if u not in out:
            out.append(u)
    return out


def _extract_body_unsubscribe_urls(body_text: str) -> List[str]:
    if not body_text or not isinstance(body_text, str):
        return []
    # Heuristic: find URLs that clearly indicate unsubscribe/opt-out.
    urls = re.findall(r"https?://[^\s<>\"]+", body_text, flags=re.IGNORECASE)
    keywords = ("unsubscribe", "optout", "opt-out", "email-preferences", "subscription", "unsub")
    out: List[str] = []
    for url in urls:
        cleaned = url.strip().rstrip(").,;]>\"'")
        lowered = cleaned.lower()
        if not cleaned:
            continue
        if any(k in lowered for k in keywords):
            if cleaned not in out:
                out.append(cleaned)
        if len(out) >= 10:
            break
    return out


def _pick_http_unsubscribe_url(urls: List[str]) -> Optional[str]:
    https = [u for u in urls if u.lower().startswith("https://")]
    if https:
        return https[0]
    http = [u for u in urls if u.lower().startswith("http://")]
    if http:
        return http[0]
    return None


def _safe_unsubscribe_request(url: str, *, one_click: bool, dry_run: bool) -> Dict[str, Any]:
    ok, reason = _validate_unsubscribe_url(url)
    if not ok:
        return {"ok": False, "url": url, "reason": reason}
    if dry_run:
        return {"ok": True, "url": url, "dry_run": True, "one_click": one_click}

    headers: Dict[str, str] = {"User-Agent": "PitchAI-Mailbox-Tagger/1.0"}
    if one_click:
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    method = "POST" if one_click else "GET"
    data = "List-Unsubscribe=One-Click" if one_click else None

    resp = requests.request(
        method,
        url,
        headers=headers,
        data=data,
        timeout=30,
        allow_redirects=True,
        stream=True,
    )
    max_bytes = 100_000
    read = 0
    try:
        for chunk in resp.iter_content(chunk_size=8192):
            if not chunk:
                break
            read += len(chunk)
            if read >= max_bytes:
                break
    finally:
        try:
            resp.close()
        except Exception:
            pass

    ok_status = int(resp.status_code) < 400
    return {
        "ok": ok_status,
        "url": url,
        "one_click": one_click,
        "status_code": int(resp.status_code),
        "final_url": str(resp.url),
        "bytes_read": int(read),
    }


def _op_mail_search() -> Dict[str, Any]:
    args = _load_tool_args()
    folder = str(args.get("folder") or "Inbox")
    top = int(args.get("top") or 15)

    unread_only = bool(args.get("unread_only")) if "unread_only" in args else True
    untagged_only = bool(args.get("untagged_only")) if "untagged_only" in args else False

    from_address = args.get("from_address")
    subject_contains = args.get("subject_contains")
    received_since_utc = args.get("received_since_utc")

    client = GraphMailClient()
    return client.search_messages(
        folder=folder,
        top=top,
        unread_only=unread_only,
        untagged_only=untagged_only,
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
    client = GraphMailClient()
    return client.read_message(message_id, max_chars=max_chars)


def _op_mail_update_categories() -> Dict[str, Any]:
    args = _load_tool_args()
    message_id = args.get("message_id")
    if not isinstance(message_id, str) or not message_id.strip():
        raise RuntimeError("Missing required parameter: message_id")

    categories = _normalize_categories(args.get("categories"))
    mode = str(args.get("mode") or "set").strip().lower()
    if mode not in ("set", "add"):
        raise RuntimeError("mode must be 'set' or 'add'")

    client = GraphMailClient()
    if mode == "add":
        existing = client.get_categories(message_id)
        merged = existing[:]
        for cat in categories:
            if cat not in merged:
                merged.append(cat)
        categories = merged
    return client.set_categories(message_id, categories)


def _op_mail_set_read_state() -> Dict[str, Any]:
    args = _load_tool_args()
    message_id = args.get("message_id")
    if not isinstance(message_id, str) or not message_id.strip():
        raise RuntimeError("Missing required parameter: message_id")
    if "is_read" not in args:
        raise RuntimeError("Missing required parameter: is_read")
    is_read = bool(args.get("is_read"))

    client = GraphMailClient()
    return client.set_read_state(message_id, is_read=is_read)


def _op_mail_unsubscribe() -> Dict[str, Any]:
    args = _load_tool_args()
    message_id = args.get("message_id")
    if not isinstance(message_id, str) or not message_id.strip():
        raise RuntimeError("Missing required parameter: message_id")
    dry_run = bool(args.get("dry_run")) if "dry_run" in args else False

    client = GraphMailClient()
    headers = client.read_headers(message_id)
    list_unsub = headers.get("list-unsubscribe") or ""
    one_click = False
    lup = headers.get("list-unsubscribe-post") or ""
    if "one-click" in lup.lower() or "list-unsubscribe=one-click" in lup.lower():
        one_click = True

    candidates: List[str] = []
    source = "body"
    if list_unsub.strip():
        candidates = _extract_list_unsubscribe_urls(list_unsub)
        source = "list-unsubscribe"
    else:
        # Fallback: scan the body for obvious unsubscribe links.
        try:
            msg = client.read_message(message_id, max_chars=100000)
        except Exception:
            msg = {}
        body = msg.get("body") if isinstance(msg, dict) else None
        body_text = body.get("content") if isinstance(body, dict) else ""
        candidates = _extract_body_unsubscribe_urls(body_text if isinstance(body_text, str) else "")

    http_url = _pick_http_unsubscribe_url(candidates)
    if not http_url:
        return {
            "ok": True,
            "message_id": message_id,
            "result": "no_unsubscribe_url_found",
            "source": source,
            "candidates": candidates[:5],
            "ts": _now_utc_iso(),
        }

    res = _safe_unsubscribe_request(http_url, one_click=(one_click and source == "list-unsubscribe"), dry_run=dry_run)
    res.update({"message_id": message_id, "ts": _now_utc_iso(), "source": source})
    return res


def main(argv: List[str] | None = None) -> int:
    argv = argv or sys.argv[1:]
    if len(argv) < 1:
        print(
            "Usage: mail_tagger_graph_tool.py <mail_search|mail_read|mail_update_categories|mail_set_read_state|mail_unsubscribe>",
            file=sys.stderr,
        )
        return 2

    op = argv[0].strip()
    ops = {
        "mail_search": _op_mail_search,
        "mail_read": _op_mail_read,
        "mail_update_categories": _op_mail_update_categories,
        "mail_set_read_state": _op_mail_set_read_state,
        "mail_unsubscribe": _op_mail_unsubscribe,
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
