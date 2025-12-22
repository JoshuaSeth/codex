#!/usr/bin/env python3
from __future__ import annotations

import base64
import hashlib
import io
import json
import os
import re
import ssl
import sys
import time
import zipfile
from dataclasses import dataclass
from datetime import datetime, timezone
from email import policy
from email.parser import BytesParser
from html import unescape
from typing import Any, Callable, Dict, List, Optional
from urllib.parse import urlparse

import msal
import requests
from pypdf import PdfReader

try:
    import PIL.Image  # type: ignore
except Exception:  # pragma: no cover
    PIL = None

try:
    import pytesseract  # type: ignore
except Exception:  # pragma: no cover
    pytesseract = None


def _now_utc_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _odata_quote(value: str) -> str:
    return value.replace("'", "''")


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


def _acquire_token(app: msal.ConfidentialClientApplication, scope: str) -> str:
    result = app.acquire_token_for_client(scopes=[scope])
    if "access_token" not in result:
        error = result.get("error")
        desc = result.get("error_description")
        raise RuntimeError(f"Token acquisition failed for {scope}: {error}: {desc}")
    return str(result["access_token"])


def _request_with_retries(
    method: str,
    url: str,
    *,
    headers: Dict[str, str],
    params: Optional[Dict[str, str]] = None,
    data: Optional[bytes] = None,
    json_body: Optional[Dict[str, Any]] = None,
    timeout_s: int = 120,
    max_attempts: int = 6,
) -> requests.Response:
    for attempt in range(1, max_attempts + 1):
        try:
            resp = requests.request(
                method,
                url,
                headers=headers,
                params=params,
                data=data,
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


class SharePointRestClient:
    def __init__(self, site_url: str, token: str, *, token_provider: Callable[[], str] | None = None) -> None:
        self._site_url = site_url.rstrip("/")
        self._token = token
        self._token_provider = token_provider
        self._digest: Optional[str] = None
        self._digest_acquired_at: float = 0.0

    @property
    def site_url(self) -> str:
        return self._site_url

    def _headers(self, extra: Optional[Dict[str, str]] = None) -> Dict[str, str]:
        base = {
            "Authorization": f"Bearer {self._token}",
            "Accept": "application/json;odata=nometadata",
        }
        if extra:
            base.update(extra)
        return base

    def _refresh_token(self) -> None:
        if not self._token_provider:
            return
        token = self._token_provider()
        if not token:
            raise RuntimeError("Token provider returned empty token")
        self._token = token
        self._digest = None
        self._digest_acquired_at = 0.0

    def _ensure_digest(self) -> str:
        if self._digest and (time.time() - self._digest_acquired_at) < (25 * 60):
            return self._digest
        url = f"{self._site_url}/_api/contextinfo"
        resp = _request_with_retries("POST", url, headers=self._headers({"Content-Length": "0"}))
        data = resp.json()
        digest = None
        if isinstance(data, dict):
            digest = data.get("FormDigestValue")
            if not digest and "d" in data:
                d = data.get("d", {})
                info = d.get("GetContextWebInformation", {})
                digest = info.get("FormDigestValue")
        if not digest:
            raise RuntimeError("Could not get FormDigestValue from _api/contextinfo")
        self._digest = str(digest)
        self._digest_acquired_at = time.time()
        return self._digest

    def _request(
        self,
        method: str,
        url: str,
        *,
        extra_headers: Optional[Dict[str, str]] = None,
        params: Optional[Dict[str, str]] = None,
        data: Optional[bytes] = None,
        json_body: Optional[Dict[str, Any]] = None,
        timeout_s: int = 120,
        max_attempts: int = 6,
    ) -> requests.Response:
        refreshed_token = False
        retried_digest = False
        while True:
            headers = self._headers(extra_headers)
            try:
                return _request_with_retries(
                    method,
                    url,
                    headers=headers,
                    params=params,
                    data=data,
                    json_body=json_body,
                    timeout_s=timeout_s,
                    max_attempts=max_attempts,
                )
            except requests.HTTPError as exc:
                status = exc.response.status_code if exc.response is not None else None
                if status == 401 and self._token_provider and not refreshed_token:
                    refreshed_token = True
                    self._refresh_token()
                    continue
                if status == 403 and "X-RequestDigest" in headers and not retried_digest:
                    retried_digest = True
                    self._digest = None
                    self._digest_acquired_at = 0.0
                    continue
                raise

    def get_library_root_folder(self, library_title: str) -> str:
        url = (
            f"{self._site_url}/_api/web/lists/GetByTitle('{_odata_quote(library_title)}')"
            "?$select=RootFolder/ServerRelativeUrl&$expand=RootFolder"
        )
        resp = self._request("GET", url)
        data = resp.json()
        root = data.get("RootFolder", {}).get("ServerRelativeUrl")
        if not root:
            raise RuntimeError(f"Could not resolve RootFolder.ServerRelativeUrl for library '{library_title}'")
        return str(root).rstrip("/")

    def ensure_folder_tree(self, server_relative_url: str) -> None:
        folder = server_relative_url.rstrip("/")
        if not folder.startswith("/"):
            folder = "/" + folder
        parts = [p for p in folder.split("/") if p]
        prefix = ""
        for part in parts:
            prefix += "/" + part
            try:
                self._ensure_folder(prefix)
            except requests.HTTPError as exc:
                status = exc.response.status_code if exc.response is not None else None
                if status == 400:
                    continue
                raise

    def _ensure_folder(self, server_relative_url: str) -> None:
        folder = server_relative_url.rstrip("/")
        get_url = f"{self._site_url}/_api/web/GetFolderByServerRelativeUrl('{_odata_quote(folder)}')?$select=Exists"
        try:
            resp = self._request("GET", get_url, max_attempts=2)
            data = resp.json()
            if isinstance(data, dict) and data.get("Exists") is True:
                return
        except requests.HTTPError as exc:
            status = exc.response.status_code if exc.response is not None else None
            if status != 404:
                raise

        create_url = f"{self._site_url}/_api/web/folders"
        body = {"ServerRelativeUrl": folder}
        self._request(
            "POST",
            create_url,
            extra_headers={
                "Content-Type": "application/json;odata=nometadata",
                "X-RequestDigest": self._ensure_digest(),
            },
            json_body=body,
        )

    def list_files_in_folder(self, folder_ref: str, *, top: int, orderby: str) -> List[Dict[str, Any]]:
        folder = folder_ref.rstrip("/")
        url = f"{self._site_url}/_api/web/GetFolderByServerRelativeUrl('{_odata_quote(folder)}')/Files"
        params = {
            "$select": "Name,ServerRelativeUrl,TimeLastModified,Length",
            "$orderby": orderby,
            "$top": str(top),
        }
        resp = self._request("GET", url, params=params, timeout_s=300)
        data = resp.json()
        value = data.get("value")
        if not isinstance(value, list):
            return []
        return [v for v in value if isinstance(v, dict)]

    def download_file_bytes(self, file_ref: str) -> bytes:
        ref = file_ref.rstrip("/")
        url = f"{self._site_url}/_api/web/GetFileByServerRelativeUrl('{_odata_quote(ref)}')/$value"
        resp = self._request("GET", url, timeout_s=300)
        return resp.content

    def file_exists(self, file_ref: str) -> bool:
        ref = file_ref.rstrip("/")
        url = f"{self._site_url}/_api/web/GetFileByServerRelativeUrl('{_odata_quote(ref)}')?$select=Exists"
        try:
            resp = self._request("GET", url, timeout_s=120, max_attempts=2)
        except requests.HTTPError as exc:
            status = exc.response.status_code if exc.response is not None else None
            if status == 404:
                return False
            raise
        data = resp.json()
        return bool(isinstance(data, dict) and data.get("Exists") is True)

    def move_file(self, source_file_ref: str, dest_file_ref: str, *, keep_both: bool) -> None:
        src = source_file_ref.rstrip("/")
        dst = dest_file_ref.rstrip("/")
        if not src or not dst:
            raise ValueError("source/dest file ref is empty")

        host = urlparse(self._site_url).netloc
        src_abs = f"https://{host}{src}" if src.startswith("/") else src
        dst_abs = f"https://{host}{dst}" if dst.startswith("/") else dst

        url = f"{self._site_url}/_api/SP.MoveCopyUtil.MoveFileByPath()"
        body = {
            "srcPath": {"DecodedUrl": src_abs},
            "destPath": {"DecodedUrl": dst_abs},
            "options": {
                "KeepBoth": bool(keep_both),
                "ShouldBypassSharedLocks": True,
                "ResetAuthorAndCreatedOnCopy": False,
                "RetainEditorAndModifiedOnMove": True,
            },
        }
        self._request(
            "POST",
            url,
            extra_headers={
                "Content-Type": "application/json;odata=nometadata",
                "X-RequestDigest": self._ensure_digest(),
            },
            json_body=body,
            timeout_s=300,
        )

    def delete_file(self, file_ref: str) -> None:
        ref = file_ref.rstrip("/")
        url = f"{self._site_url}/_api/web/GetFileByServerRelativeUrl('{_odata_quote(ref)}')"
        self._request(
            "POST",
            url,
            extra_headers={
                "X-RequestDigest": self._ensure_digest(),
                "IF-MATCH": "*",
                "X-HTTP-Method": "DELETE",
            },
            timeout_s=120,
        )

    def get_file_list_item_id(self, file_ref: str) -> int:
        ref = file_ref.rstrip("/")
        url = f"{self._site_url}/_api/web/GetFileByServerRelativeUrl('{_odata_quote(ref)}')/ListItemAllFields?$select=Id"
        resp = self._request("GET", url, timeout_s=120)
        data = resp.json()
        item_id = data.get("Id")
        if not isinstance(item_id, int):
            raise RuntimeError(f"Could not resolve list item Id for file: {ref}")
        return item_id

    def update_list_item_fields(self, library_title: str, item_id: int, fields: Dict[str, Any]) -> None:
        url = f"{self._site_url}/_api/web/lists/GetByTitle('{_odata_quote(library_title)}')/items({item_id})"
        self._request(
            "POST",
            url,
            extra_headers={
                "Content-Type": "application/json;odata=nometadata",
                "X-RequestDigest": self._ensure_digest(),
                "IF-MATCH": "*",
                "X-HTTP-Method": "MERGE",
            },
            json_body=fields,
            timeout_s=120,
        )


def _strip_html_to_text(html: str) -> str:
    h = re.sub(r"(?is)<(script|style).*?>.*?</\\1>", " ", html)
    h = re.sub(r"(?is)<br\\s*/?>", "\n", h)
    h = re.sub(r"(?is)</p\\s*>", "\n", h)
    h = re.sub(r"(?is)<[^>]+>", " ", h)
    h = re.sub(r"[ \\t\\r\\f\\v]+", " ", h)
    h = re.sub(r"\\n\\s+", "\n", h)
    return h.strip()


def _email_body_text(msg: Any) -> str:
    if msg.is_multipart():
        html_candidate: Optional[str] = None
        for part in msg.walk():
            if part.get_content_maintype() != "text":
                continue
            if part.get_content_disposition() == "attachment":
                continue
            ctype = part.get_content_type()
            try:
                content = part.get_content()
            except Exception:
                continue
            if ctype == "text/plain" and isinstance(content, str) and content.strip():
                return content
            if ctype == "text/html" and isinstance(content, str) and content.strip():
                html_candidate = content
        if html_candidate:
            return _strip_html_to_text(html_candidate)
        return ""

    ctype = msg.get_content_type()
    if ctype == "text/plain":
        try:
            content = msg.get_content()
            return content if isinstance(content, str) else ""
        except Exception:
            return ""
    if ctype == "text/html":
        try:
            content = msg.get_content()
            return _strip_html_to_text(content) if isinstance(content, str) else ""
        except Exception:
            return ""
    return ""


def _safe_text(s: str, *, max_chars: int) -> str:
    if len(s) <= max_chars:
        return s
    return s[: max_chars - 3] + "..."

def _safe_filename(name: str) -> str:
    # Prevent path traversal and keep filenames predictable when writing locally.
    base = name.rsplit("/", 1)[-1].rsplit("\\", 1)[-1]
    base = base.strip() or "file"
    base = re.sub(r"[^A-Za-z0-9._-]+", "_", base)
    return base[:180] or "file"


def _extract_ascii_strings(data: bytes, *, min_len: int = 6, max_total: int = 12000) -> str:
    out: List[str] = []
    buf: List[int] = []

    def flush() -> None:
        nonlocal buf
        if len(buf) >= min_len:
            out.append(bytes(buf).decode("ascii", errors="ignore"))
        buf = []

    for b in data:
        if 32 <= b <= 126:
            buf.append(b)
            if len(buf) >= 2048:
                flush()
        else:
            flush()
        if sum(len(s) for s in out) > max_total:
            break
    flush()
    return "\n".join(out)[:max_total].strip()


def _zip_read_text(zip_bytes: bytes, member: str) -> str:
    try:
        with zipfile.ZipFile(io.BytesIO(zip_bytes)) as zf:
            with zf.open(member) as fh:
                return fh.read().decode("utf-8", errors="replace")
    except Exception:
        return ""


def _zip_list_members(zip_bytes: bytes, *, prefix: str) -> List[str]:
    try:
        with zipfile.ZipFile(io.BytesIO(zip_bytes)) as zf:
            return [name for name in zf.namelist() if name.startswith(prefix)]
    except Exception:
        return []


def _extract_office_xml_text(xml_text: str) -> str:
    # Extract text nodes from OOXML (docx/pptx/xlsx) without full XML parsing.
    parts = []
    for match in re.finditer(r"<(?:a:t|w:t|t)(?:\\s[^>]*)?>(.*?)</(?:a:t|w:t|t)>", xml_text, flags=re.DOTALL):
        parts.append(unescape(re.sub(r"<[^>]+>", "", match.group(1))))
    text = "\n".join(p.strip() for p in parts if p and p.strip())
    return text.strip()


def _extract_text_from_docx(data: bytes) -> str:
    xml = _zip_read_text(data, "word/document.xml")
    if not xml:
        return ""
    return _extract_office_xml_text(xml)


def _extract_text_from_pptx(data: bytes) -> str:
    members = _zip_list_members(data, prefix="ppt/slides/slide")
    if not members:
        return ""
    out: List[str] = []
    for member in sorted(members)[:20]:
        xml = _zip_read_text(data, member)
        if xml:
            out.append(_extract_office_xml_text(xml))
    return "\n".join([t for t in out if t]).strip()


def _extract_text_from_xlsx(data: bytes) -> str:
    shared = _zip_read_text(data, "xl/sharedStrings.xml")
    if shared:
        return _extract_office_xml_text(shared)
    return ""


def _extract_text_from_pdf(data: bytes) -> str:
    try:
        reader = PdfReader(io.BytesIO(data))
        out: List[str] = []
        for page in reader.pages[:20]:
            try:
                out.append(page.extract_text() or "")
            except Exception:
                continue
        return "\n".join(out).strip()
    except Exception:
        return ""


def _extract_text_from_image(data: bytes) -> str:
    if PIL is None or pytesseract is None:
        return ""
    try:
        img = PIL.Image.open(io.BytesIO(data))
        return str(pytesseract.image_to_string(img)).strip()
    except Exception:
        return ""


@dataclass(frozen=True)
class ExtractedText:
    file_type: str
    text: str
    source: str


def _extract_text(filename: str, data: bytes, *, depth: int = 0) -> ExtractedText:
    name_l = filename.lower()
    if depth >= 3:
        return ExtractedText(file_type="binary", text=_extract_ascii_strings(data), source="max_depth_ascii_strings")

    if name_l.endswith(".eml"):
        try:
            msg = BytesParser(policy=policy.default).parsebytes(data)
        except Exception:
            return ExtractedText(file_type="eml", text="", source="eml_parse_failed")
        subject = str(msg.get("subject", "") or "").strip()
        from_ = str(msg.get("from", "") or "").strip()
        to_ = str(msg.get("to", "") or "").strip()
        cc = str(msg.get("cc", "") or "").strip()
        body = _email_body_text(msg)
        attachment_names: List[str] = []
        attachment_texts: List[str] = []
        for part in msg.walk():
            if part.get_content_disposition() != "attachment":
                continue
            att_name = part.get_filename() or "attachment"
            raw = part.get_payload(decode=True) or b""
            attachment_names.append(f"{att_name} ({len(raw)} bytes)")
            if raw and len(attachment_texts) < 5 and len(raw) <= 20_000_000:
                extracted = _extract_text(att_name, raw, depth=depth + 1)
                att_text = extracted.text.strip()
                if att_text:
                    attachment_texts.append(
                        "\n".join(
                            [
                                f"attachment: {att_name}",
                                f"attachment_file_type: {extracted.file_type}",
                                f"attachment_extract_source: {extracted.source}",
                                "",
                                _safe_text(att_text, max_chars=4000),
                            ]
                        ).strip()
                    )

        parts: List[str] = [
            f"subject: {subject}",
            f"from: {from_}",
            f"to: {to_}",
            f"cc: {cc}",
            "",
            body,
        ]
        if attachment_names:
            parts.extend(["", "attachments:"])
            parts.extend([f"- {name}" for name in attachment_names])
        if attachment_texts:
            parts.extend(["", "attachment_extracted_text:"])
            parts.extend(attachment_texts)
        text = "\n".join(parts).strip()
        return ExtractedText(file_type="eml", text=text, source="email_parser")

    if name_l.endswith(".docx"):
        text = _extract_text_from_docx(data)
        return ExtractedText(file_type="docx", text=text, source="ooxml_word")

    if name_l.endswith(".pptx"):
        text = _extract_text_from_pptx(data)
        return ExtractedText(file_type="pptx", text=text, source="ooxml_powerpoint")

    if name_l.endswith(".xlsx") or name_l.endswith(".xlsm"):
        text = _extract_text_from_xlsx(data)
        return ExtractedText(file_type="xlsx", text=text, source="ooxml_excel")

    if name_l.endswith(".pdf"):
        text = _extract_text_from_pdf(data)
        return ExtractedText(file_type="pdf", text=text, source="pypdf")

    if any(name_l.endswith(ext) for ext in [".png", ".jpg", ".jpeg", ".tiff", ".bmp"]):
        text = _extract_text_from_image(data)
        return ExtractedText(file_type="image", text=text, source="tesseract" if text else "image_no_text")

    if any(name_l.endswith(ext) for ext in [".txt", ".md", ".csv", ".json", ".xml", ".html", ".htm"]):
        try:
            decoded = data.decode("utf-8", errors="replace")
        except Exception:
            decoded = ""
        if name_l.endswith(".html") or name_l.endswith(".htm"):
            decoded = _strip_html_to_text(decoded)
        return ExtractedText(file_type="text", text=decoded.strip(), source="utf8_decode")

    try:
        decoded = data.decode("utf-8", errors="replace").strip()
    except Exception:
        decoded = ""
    if decoded and sum(1 for ch in decoded[:2000] if ch.isprintable()) / max(1, len(decoded[:2000])) > 0.8:
        return ExtractedText(file_type="text", text=decoded, source="utf8_guess")

    return ExtractedText(file_type="binary", text=_extract_ascii_strings(data), source="ascii_strings")


def _load_tool_args() -> Dict[str, Any]:
    raw = os.getenv("CODEX_TOOL_ARGS_JSON", "{}")
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid CODEX_TOOL_ARGS_JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise RuntimeError("CODEX_TOOL_ARGS_JSON must be a JSON object")
    return data


def _new_sharepoint_client() -> SharePointRestClient:
    tenant_id = _require_env("PITCHAI_SP_TENANT_ID")
    client_id = _require_env("PITCHAI_SP_CLIENT_ID")
    site_url = _require_env("PITCHAI_SP_SITE_URL").rstrip("/")
    private_key_pem = _b64decode_text(_require_env("PITCHAI_CERT_PRIVATE_KEY_B64"))
    public_cert_pem = _b64decode_text(_require_env("PITCHAI_CERT_PUBLIC_CERT_B64"))

    thumbprint = _sha1_thumbprint_from_pem(public_cert_pem)
    authority = f"https://login.microsoftonline.com/{tenant_id}"
    app = msal.ConfidentialClientApplication(
        client_id=client_id,
        authority=authority,
        client_credential={"private_key": private_key_pem, "thumbprint": thumbprint},
    )

    host = urlparse(site_url).netloc
    scope = f"https://{host}/.default"
    token = _acquire_token(app, scope)
    return SharePointRestClient(site_url, token, token_provider=lambda: _acquire_token(app, scope))


def _op_list_inbox() -> Dict[str, Any]:
    args = _load_tool_args()
    library = str(args.get("library") or _optional_env("PITCHAI_SP_LIBRARY") or "Documenten")
    override_limit = _optional_env("PITCHAI_MAX_FILES")
    if override_limit and override_limit.isdigit() and int(override_limit) > 0:
        limit = int(override_limit)
    else:
        limit = int(args.get("limit") or 50)
    orderby = str(args.get("orderby") or "TimeLastModified desc")

    sp = _new_sharepoint_client()
    root = sp.get_library_root_folder(library)
    inbox = f"{root}/INBOX".rstrip("/")
    files = sp.list_files_in_folder(inbox, top=limit, orderby=orderby)

    out = []
    for f in files:
        name = str(f.get("Name") or "")
        ref = str(f.get("ServerRelativeUrl") or "")
        modified = str(f.get("TimeLastModified") or "")
        length = f.get("Length")
        try:
            size = int(length) if length is not None else 0
        except Exception:
            size = 0
        ext = ""
        if "." in name:
            ext = name.rsplit(".", 1)[-1].lower()
        out.append(
            {
                "name": name,
                "file_ref": ref,
                "modified_utc": modified,
                "size_bytes": size,
                "ext": ext,
            }
        )

    return {"library": library, "library_root": root, "inbox": inbox, "count": len(out), "files": out, "ts": _now_utc_iso()}


def _op_read_file() -> Dict[str, Any]:
    args = _load_tool_args()
    file_ref = args.get("file_ref")
    if not isinstance(file_ref, str) or not file_ref.strip():
        raise RuntimeError("Missing required parameter: file_ref")

    max_chars = int(args.get("max_chars") or 15000)
    sp = _new_sharepoint_client()
    raw = sp.download_file_bytes(file_ref)
    filename = file_ref.rstrip("/").rsplit("/", 1)[-1]
    extracted = _extract_text(filename, raw)
    text = extracted.text.strip()
    truncated = False
    if max_chars > 0 and len(text) > max_chars:
        text = _safe_text(text, max_chars=max_chars)
        truncated = True
    sha256 = hashlib.sha256(raw).hexdigest()
    out_dir = os.getenv("PITCHAI_EXTRACTED_TEXT_DIR") or "/tmp/pitchai_sharepoint_extracted"
    os.makedirs(out_dir, exist_ok=True)
    call_id = os.getenv("CODEX_TOOL_CALL_ID") or f"call_{sha256[:12]}"
    safe_call_id = re.sub(r"[^A-Za-z0-9._-]+", "_", call_id)[:80] or f"call_{sha256[:12]}"
    safe_name = _safe_filename(filename)
    out_path = os.path.join(out_dir, f"{safe_call_id}_{safe_name}.txt")
    with open(out_path, "w", encoding="utf-8", errors="replace") as fh:
        fh.write(text)
    return {
        "file_ref": file_ref,
        "filename": filename,
        "file_type": extracted.file_type,
        "extract_source": extracted.source,
        "sha256": sha256,
        "size_bytes": len(raw),
        "extracted_text_path": out_path,
        "text_char_count": len(text),
        "truncated": truncated,
        "ts": _now_utc_iso(),
    }


def _op_ensure_folder() -> Dict[str, Any]:
    args = _load_tool_args()
    folder_ref = args.get("folder_ref")
    if not isinstance(folder_ref, str) or not folder_ref.strip():
        raise RuntimeError("Missing required parameter: folder_ref")
    sp = _new_sharepoint_client()
    sp.ensure_folder_tree(folder_ref)
    return {"folder_ref": folder_ref, "ok": True, "ts": _now_utc_iso()}


def _op_move_file() -> Dict[str, Any]:
    args = _load_tool_args()
    src = args.get("src_ref")
    dst = args.get("dest_ref")
    if not isinstance(src, str) or not src.strip():
        raise RuntimeError("Missing required parameter: src_ref")
    if not isinstance(dst, str) or not dst.strip():
        raise RuntimeError("Missing required parameter: dest_ref")
    keep_both = bool(args.get("keep_both") or False)
    sp = _new_sharepoint_client()
    sp.move_file(src, dst, keep_both=keep_both)
    return {"src_ref": src, "dest_ref": dst, "keep_both": keep_both, "ok": True, "ts": _now_utc_iso()}


def _op_update_fields() -> Dict[str, Any]:
    args = _load_tool_args()
    file_ref = args.get("file_ref")
    fields = args.get("fields")
    library = str(args.get("library") or _optional_env("PITCHAI_SP_LIBRARY") or "Documenten")
    if not isinstance(file_ref, str) or not file_ref.strip():
        raise RuntimeError("Missing required parameter: file_ref")
    if not isinstance(fields, dict) or not fields:
        raise RuntimeError("Missing required parameter: fields (object)")

    sp = _new_sharepoint_client()
    item_id = sp.get_file_list_item_id(file_ref)
    remaining = dict(fields)
    dropped: List[str] = []

    def extract_error_message(resp: requests.Response) -> str:
        try:
            data = resp.json()
        except Exception:
            return (resp.text or "").strip()
        if not isinstance(data, dict):
            return (resp.text or "").strip()
        error = data.get("error")
        if isinstance(error, dict):
            msg = error.get("message")
            if isinstance(msg, dict):
                value = msg.get("value")
                if isinstance(value, str):
                    return value.strip()
            if isinstance(msg, str):
                return msg.strip()
        return (resp.text or "").strip()

    def parse_unknown_field_name(message: str) -> Optional[str]:
        patterns = [
            r"The property '([^']+)' does not exist on type",
            r"Property '([^']+)' does not exist on type",
            r"Field or property '([^']+)' does not exist",
            r"Cannot find field '([^']+)'",
        ]
        for pat in patterns:
            m = re.search(pat, message)
            if m:
                return str(m.group(1))
        return None

    while remaining:
        try:
            sp.update_list_item_fields(library, item_id, remaining)
            return {
                "library": library,
                "file_ref": file_ref,
                "item_id": item_id,
                "updated_keys": sorted(remaining.keys()),
                "dropped_keys": sorted(set(dropped)),
                "ok": True,
                "ts": _now_utc_iso(),
            }
        except requests.HTTPError as exc:
            resp = exc.response
            if resp is None:
                raise
            message = extract_error_message(resp)
            unknown = parse_unknown_field_name(message)
            if unknown and unknown in remaining:
                dropped.append(unknown)
                remaining.pop(unknown, None)
                continue
            raise

    return {
        "library": library,
        "file_ref": file_ref,
        "item_id": item_id,
        "updated_keys": [],
        "dropped_keys": sorted(set(dropped)),
        "ok": False,
        "ts": _now_utc_iso(),
    }


def main() -> int:
    if len(sys.argv) < 2:
        raise RuntimeError("Usage: sharepoint_tool.py <op>")

    op = sys.argv[1].strip()
    if op == "list_inbox":
        out = _op_list_inbox()
    elif op == "read_file":
        out = _op_read_file()
    elif op == "ensure_folder":
        out = _op_ensure_folder()
    elif op == "move_file":
        out = _op_move_file()
    elif op == "update_fields":
        out = _op_update_fields()
    else:
        raise RuntimeError(f"Unknown op: {op}")

    sys.stdout.write(json.dumps(out, ensure_ascii=False))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
