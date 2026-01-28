#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import sys
import uuid
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional

import psycopg2
import psycopg2.extras


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


def _load_tool_args() -> Dict[str, Any]:
    raw = os.getenv("CODEX_TOOL_ARGS_JSON", "{}")
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid CODEX_TOOL_ARGS_JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise RuntimeError("CODEX_TOOL_ARGS_JSON must be a JSON object")
    return data


def _db_dsn() -> str:
    url = _optional_env("PITCHAI_PM_DB_URL")
    if url and url.strip():
        return url.strip()

    host = _require_env("PITCHAI_PM_DB_HOST")
    port = _require_env("PITCHAI_PM_DB_PORT")
    name = _require_env("PITCHAI_PM_DB_NAME")
    user = _require_env("PITCHAI_PM_DB_USER")
    password = _require_env("PITCHAI_PM_DB_PASS")
    return f"postgresql://{user}:{password}@{host}:{port}/{name}"


def _connect():
    return psycopg2.connect(_db_dsn(), connect_timeout=10)


def _uuid_param(value: Any, *, field: str) -> uuid.UUID:
    if not isinstance(value, str) or not value.strip():
        raise RuntimeError(f"Missing required parameter: {field}")
    try:
        return uuid.UUID(value.strip())
    except ValueError as exc:
        raise RuntimeError(f"Invalid UUID for {field}: {value}") from exc


def _text_param(value: Any, *, field: str, required: bool = True) -> Optional[str]:
    if value is None:
        if required:
            raise RuntimeError(f"Missing required parameter: {field}")
        return None
    if not isinstance(value, str):
        raise RuntimeError(f"Invalid parameter type for {field}: expected string")
    s = value.strip()
    if not s and required:
        raise RuntimeError(f"Missing required parameter: {field}")
    return s if s else None


def _find_existing_by_source(cur, *, table: str, project_id: uuid.UUID, source_message_id: Optional[str], source_internet_message_id: Optional[str]) -> Optional[str]:
    if source_message_id:
        cur.execute(
            f"""
            select id::text
            from public.{table}
            where project_id = %s
              and description->>'source_message_id' = %s
            limit 1
            """,
            (str(project_id), source_message_id),
        )
        row = cur.fetchone()
        if row and row[0]:
            return str(row[0])

    if source_internet_message_id:
        cur.execute(
            f"""
            select id::text
            from public.{table}
            where project_id = %s
              and description->>'source_internet_message_id' = %s
            limit 1
            """,
            (str(project_id), source_internet_message_id),
        )
        row = cur.fetchone()
        if row and row[0]:
            return str(row[0])

    return None


def _op_search_projects() -> Dict[str, Any]:
    args = _load_tool_args()
    query = _text_param(args.get("query"), field="query", required=True) or ""
    limit = int(args.get("limit") or 10)
    limit = max(1, min(limit, 25))

    like = f"%{query}%"
    with _connect() as conn:
        with conn.cursor(cursor_factory=psycopg2.extras.RealDictCursor) as cur:
            cur.execute(
                """
                select
                  p.id::text as id,
                  p.name as name,
                  gr.repo_full_name as repo_full_name,
                  gr.repo_html_url as repo_html_url
                from public.projects p
                left join pitchai_dispatch.project_git_repos gr on gr.project_id = p.id
                where p.name ilike %s
                order by p.rank nulls last, p.updated_at desc nulls last, p.created_at desc nulls last
                limit %s
                """,
                (like, limit),
            )
            rows = cur.fetchall() or []

    return {"ok": True, "query": query, "count": len(rows), "projects": rows, "ts": _now_utc_iso()}


def _op_create_feature() -> Dict[str, Any]:
    args = _load_tool_args()
    project_id = _uuid_param(args.get("project_id"), field="project_id")
    name = _text_param(args.get("name"), field="name", required=True) or ""
    description_text = _text_param(args.get("description_text"), field="description_text", required=False)

    source_message_id = _text_param(args.get("source_message_id"), field="source_message_id", required=False)
    source_internet_message_id = _text_param(args.get("source_internet_message_id"), field="source_internet_message_id", required=False)
    source_subject = _text_param(args.get("source_subject"), field="source_subject", required=False)
    source_from = _text_param(args.get("source_from"), field="source_from", required=False)
    received_utc = _text_param(args.get("received_utc"), field="received_utc", required=False)
    mailbox_upn = _text_param(args.get("mailbox_upn"), field="mailbox_upn", required=False)

    meta: Dict[str, Any] = {
        "source": "mailbox_tagger",
        "source_message_id": source_message_id,
        "source_internet_message_id": source_internet_message_id,
        "source_subject": source_subject,
        "source_from": source_from,
        "received_utc": received_utc,
        "mailbox_upn": mailbox_upn,
        "description_text": description_text,
        "created_at_utc": _now_utc_iso(),
    }
    meta = {k: v for k, v in meta.items() if v is not None}

    with _connect() as conn:
        with conn.cursor() as cur:
            existing = _find_existing_by_source(
                cur,
                table="features",
                project_id=project_id,
                source_message_id=source_message_id,
                source_internet_message_id=source_internet_message_id,
            )
            if existing:
                return {"ok": True, "did_create": False, "feature_id": existing, "ts": _now_utc_iso()}

            fid = str(uuid.uuid4())
            cur.execute(
                """
                insert into public.features (id, name, description, project_id, state_name, created_at, updated_at)
                values (%s, %s, %s::jsonb, %s, %s, now(), now())
                """,
                (fid, name, json.dumps(meta), str(project_id), "Todo"),
            )
            conn.commit()
            return {"ok": True, "did_create": True, "feature_id": fid, "ts": _now_utc_iso()}


def _op_create_task() -> Dict[str, Any]:
    args = _load_tool_args()
    project_id = _uuid_param(args.get("project_id"), field="project_id")
    feature_id_raw = _text_param(args.get("feature_id"), field="feature_id", required=False)
    feature_id = str(uuid.UUID(feature_id_raw)) if feature_id_raw else None

    name = _text_param(args.get("name"), field="name", required=True) or ""
    description_text = _text_param(args.get("description_text"), field="description_text", required=False)

    value_name = _text_param(args.get("value_name"), field="value_name", required=False) or "High"
    state_name = _text_param(args.get("state_name"), field="state_name", required=False) or "Todo"
    is_bug = bool(args.get("is_bug")) if "is_bug" in args else False

    source_message_id = _text_param(args.get("source_message_id"), field="source_message_id", required=False)
    source_internet_message_id = _text_param(args.get("source_internet_message_id"), field="source_internet_message_id", required=False)
    source_subject = _text_param(args.get("source_subject"), field="source_subject", required=False)
    source_from = _text_param(args.get("source_from"), field="source_from", required=False)
    received_utc = _text_param(args.get("received_utc"), field="received_utc", required=False)
    mailbox_upn = _text_param(args.get("mailbox_upn"), field="mailbox_upn", required=False)

    meta: Dict[str, Any] = {
        "source": "mailbox_tagger",
        "source_message_id": source_message_id,
        "source_internet_message_id": source_internet_message_id,
        "source_subject": source_subject,
        "source_from": source_from,
        "received_utc": received_utc,
        "mailbox_upn": mailbox_upn,
        "description_text": description_text,
        "created_at_utc": _now_utc_iso(),
    }
    meta = {k: v for k, v in meta.items() if v is not None}

    with _connect() as conn:
        with conn.cursor() as cur:
            existing = _find_existing_by_source(
                cur,
                table="tasks",
                project_id=project_id,
                source_message_id=source_message_id,
                source_internet_message_id=source_internet_message_id,
            )
            if existing:
                return {"ok": True, "did_create": False, "task_id": existing, "ts": _now_utc_iso()}

            tid = str(uuid.uuid4())
            cur.execute(
                """
                insert into public.tasks (
                  id, name, description, project_id, feature_id, state_name, value_name, is_bug, created_at, updated_at
                )
                values (%s, %s, %s::jsonb, %s, %s, %s, %s, %s, now(), now())
                """,
                (tid, name, json.dumps(meta), str(project_id), feature_id, state_name, value_name, is_bug),
            )
            conn.commit()
            return {"ok": True, "did_create": True, "task_id": tid, "ts": _now_utc_iso()}


def main(argv: List[str] | None = None) -> int:
    argv = argv or sys.argv[1:]
    if len(argv) < 1:
        print("Usage: pm_db_tool.py <search_projects|create_feature|create_task>", file=sys.stderr)
        return 2

    op = argv[0].strip()
    ops = {
        "search_projects": _op_search_projects,
        "create_feature": _op_create_feature,
        "create_task": _op_create_task,
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
