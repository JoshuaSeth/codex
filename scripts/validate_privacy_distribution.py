#!/usr/bin/env python3
"""Validate a privacy distribution archive in an isolated temp prefix."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tarfile
import tempfile
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("archive", type=Path)
    parser.add_argument("--detector-cmd", required=True)
    args = parser.parse_args()

    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        with tarfile.open(args.archive, "r:gz") as tar:
            tar.extractall(root, filter="data")
        package_dir = next(path for path in root.iterdir() if path.is_dir())
        prefix = root / "prefix"
        subprocess.run([str(package_dir / "install.sh"), str(prefix)], check=True)
        codex_privacy = prefix / "bin" / "codex-privacy"
        if not codex_privacy.exists():
            raise RuntimeError(f"install did not create {codex_privacy}")
        probe = Path(__file__).resolve().with_name("privacy_network_probe.py")
        proof = root / "distribution_probe.json"
        subprocess.run(
            [
                "python3",
                str(probe),
                "--codex",
                str(prefix / "lib/pitchai-codex-privacy/bin/codex"),
                "--detector-cmd",
                args.detector_cmd,
                "--out",
                str(proof),
                "--timeout",
                "180",
            ],
            check=True,
        )
        data = json.loads(proof.read_text())
        if data["captured_request_contains_real_values"]:
            raise RuntimeError("real PII leaked in distribution probe")
        print(
            json.dumps(
                {
                    "archive": str(args.archive),
                    "installed": str(codex_privacy),
                    "probe": str(proof),
                    "restored": data["stdout_restored_real_values"],
                    "leaks": data["captured_request_contains_real_values"],
                },
                indent=2,
            )
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
