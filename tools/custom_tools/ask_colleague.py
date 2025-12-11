#!/usr/bin/env python3
"""
Thin shim to run the canonical ask_colleague helper that lives under codex-rs/.

This keeps the configuration snippets stable (`./tools/custom_tools/ask_colleague.py`)
while letting the actual implementation ship from the Rust workspace. If the
target script ever moves, this shim will fail loudly so we notice quickly.
"""

from __future__ import annotations

import runpy
from pathlib import Path
import sys


def main() -> None:
    repo_root = Path(__file__).resolve().parents[2]
    target = repo_root / "codex-rs" / "tools" / "custom_tools" / "ask_colleague.py"
    if not target.exists():
        sys.stderr.write(f"ask_colleague shim: missing target script {target}\n")
        sys.exit(2)
    runpy.run_path(str(target), run_name="__main__")


if __name__ == "__main__":
    main()
