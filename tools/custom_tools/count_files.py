#!/usr/bin/env python3
"""Count files beneath a directory and emit a JSON summary."""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Dict


def _load_args() -> Dict[str, Any]:
    raw = os.environ.get("CODEX_TOOL_ARGS_JSON", "{}")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:  # pragma: no cover - defensive
        return {"error": f"invalid arguments: {exc}"}


def main() -> None:
    args = _load_args()
    target = Path(args.get("path") or ".")
    follow_symlinks = bool(args.get("follow_symlinks", False))
    include_hidden = bool(args.get("include_hidden", True))

    if not target.exists():
        print(json.dumps({"error": f"path does not exist: {target}"}))
        return

    count = 0
    for entry in target.rglob("*") if follow_symlinks else target.glob("**/*"):
        try:
            is_file = entry.is_file()
        except OSError:
            continue
        if not is_file:
            continue
        if not include_hidden and any(part.startswith(".") for part in entry.parts):
            continue
        count += 1

    print(
        json.dumps(
            {
                "path": str(target.resolve()),
                "count": count,
                "follow_symlinks": follow_symlinks,
                "include_hidden": include_hidden,
            }
        )
    )


if __name__ == "__main__":
    main()
