from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path
from typing import Iterable, List, Optional

from .config import get_config

ROLL_OUT_PREFIX = "rollout-"
ROLL_OUT_SUFFIX = ".jsonl"


def iter_rollout_files(include_archived: bool = True) -> Iterable[Path]:
    cfg = get_config()
    if cfg.sessions_dir.exists():
        yield from cfg.sessions_dir.rglob(f"{ROLL_OUT_PREFIX}*{ROLL_OUT_SUFFIX}")
    if include_archived and cfg.archived_sessions_dir and cfg.archived_sessions_dir.exists():
        yield from cfg.archived_sessions_dir.rglob(f"{ROLL_OUT_PREFIX}*{ROLL_OUT_SUFFIX}")


def _conversation_id_from_file(path: Path) -> Optional[str]:
    try:
        with path.open("r", encoding="utf-8") as fh:
            first_line = fh.readline().strip()
            if not first_line:
                return None
            data = json.loads(first_line)
            if data.get("type") == "session_meta":
                payload = data.get("payload") or {}
                return payload.get("id")
    except Exception:
        return None
    return None


def find_rollout_by_conversation_id(conversation_id: str) -> Optional[Path]:
    needle = conversation_id.lower()
    # Fast path: filename substring search
    candidates: List[Path] = []
    for path in iter_rollout_files():
        name = path.name.lower()
        if needle in name:
            candidates.append(path)
    if not candidates:
        # fallback to full scan: maybe user provided short prefix; attempt to match by reading files
        candidates = list(iter_rollout_files())
    for path in candidates:
        cid = _conversation_id_from_file(path)
        if cid and cid.lower().startswith(needle):
            return path
    return None


def list_recent_sessions(limit: int = 25) -> List[dict]:
    files = []
    for path in iter_rollout_files():
        try:
            stat = path.stat()
        except FileNotFoundError:
            continue
        files.append((stat.st_mtime, path))
    files.sort(reverse=True)
    rows: List[dict] = []
    cfg = get_config()
    for mtime, path in files[:limit]:
        cid = _conversation_id_from_file(path)
        rows.append(
            {
                "conversation_id": cid,
                "filename": path.name,
                "path": path,
                "timestamp": datetime.fromtimestamp(mtime),
                "relative_dir": str(path.parent.relative_to(cfg.codex_home)),
            }
        )
    return rows
