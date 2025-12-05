from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List, Optional

from pydantic import BaseModel


class SessionMeta(BaseModel):
    conversation_id: str
    cwd: Optional[str]
    originator: Optional[str]
    provider: Optional[str]
    cli_version: Optional[str]
    instructions: Optional[str]
    created_at: datetime
    file_path: Path


class TimelineCard(BaseModel):
    timestamp: datetime
    delta_seconds: Optional[float]
    kind: str
    title: str
    subtitle: Optional[str] = None
    body: Optional[str] = None
    body_html: Optional[str] = None
    code: Optional[str] = None
    level: str = "info"
    icon: str = "📝"
    icon_class: str = "icon icon-file-text"
    presentation: str = "summary-collapsed"  # full, summary-collapsed, summary-open, hidden
    raw: Dict[str, Any]

    @property
    def delta_human(self) -> str:
        if self.delta_seconds is None:
            return "—"
        seconds = self.delta_seconds
        if seconds < 1:
            return f"{seconds * 1000:.0f} ms"
        if seconds < 60:
            return f"{seconds:.1f} s"
        minutes = seconds / 60
        if minutes < 60:
            return f"{minutes:.1f} min"
        hours = minutes / 60
        return f"{hours:.1f} h"


class SessionView(BaseModel):
    meta: SessionMeta
    cards: List[TimelineCard]


@dataclass
class RawEvent:
    timestamp: datetime
    type: str
    payload: Dict[str, Any]
    raw: Dict[str, Any]
