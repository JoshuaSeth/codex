from __future__ import annotations

import os
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from typing import Optional


@dataclass(frozen=True)
class SessionViewerConfig:
    codex_home: Path
    sessions_dir: Path
    archived_sessions_dir: Optional[Path]

    @property
    def exists(self) -> bool:
        return self.sessions_dir.exists()


@lru_cache(maxsize=1)
def get_config() -> SessionViewerConfig:
    base = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")).expanduser()
    sessions = base / "sessions"
    archived = base / "archived_sessions"
    return SessionViewerConfig(codex_home=base, sessions_dir=sessions, archived_sessions_dir=archived if archived.exists() else None)
