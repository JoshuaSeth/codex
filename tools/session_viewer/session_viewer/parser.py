from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path
from typing import Iterator, Optional

from .models import RawEvent, SessionMeta, SessionView
from . import summary


def _parse_timestamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def iter_raw_events(path: Path) -> Iterator[RawEvent]:
    with path.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                continue
            timestamp = _parse_timestamp(data.get("timestamp")) if data.get("timestamp") else datetime.fromtimestamp(path.stat().st_mtime)
            yield RawEvent(timestamp=timestamp, type=data.get("type", "unknown"), payload=data.get("payload") or {}, raw=data)


def parse_session(path: Path) -> SessionView:
    meta: Optional[SessionMeta] = None
    cards = []
    prev_ts: Optional[datetime] = None
    for event in iter_raw_events(path):
        if event.type == "session_meta" and meta is None:
            payload = event.payload
            meta = SessionMeta(
                conversation_id=payload.get("id", path.stem),
                cwd=payload.get("cwd"),
                originator=payload.get("originator"),
                provider=payload.get("model_provider"),
                cli_version=payload.get("cli_version"),
                instructions=payload.get("instructions"),
                created_at=event.timestamp,
                file_path=path,
            )
            continue
        delta = (event.timestamp - prev_ts).total_seconds() if prev_ts else None
        prev_ts = event.timestamp
        card = summary.to_card(event, delta)
        if card:
            cards.append(card)
    if meta is None:
        meta = SessionMeta(
            conversation_id=path.stem,
            cwd=None,
            originator=None,
            provider=None,
            cli_version=None,
            instructions=None,
            created_at=datetime.fromtimestamp(path.stat().st_mtime),
            file_path=path,
        )
    return SessionView(meta=meta, cards=cards)
