#!/usr/bin/env python3
"""Deterministic detector for local network-capture proof only."""

from __future__ import annotations

import json
import sys


KINDS = {
    "Jane Smith": "private_person",
    "14 Pearl St": "private_address",
    "jane.smith@example.com": "private_email",
    "(415) 555-1212": "private_phone_number",
}


def main() -> int:
    text = json.load(sys.stdin)["text"]
    spans = []
    for value, kind in KINDS.items():
        start = 0
        while True:
            index = text.find(value, start)
            if index == -1:
                break
            spans.append({"start": index, "end": index + len(value), "kind": kind})
            start = index + len(value)
    print(
        json.dumps(
            {"spans": sorted(spans, key=lambda span: span["start"])},
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
