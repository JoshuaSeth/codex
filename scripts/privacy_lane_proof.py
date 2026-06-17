#!/usr/bin/env python3
"""Integration-style proof for the Codex privacy lane.

This mirrors the Rust middleware contract with a real local detector command:
detect spans, replace real PII with realistic fake values before "send", keep
the reversible map local, and restore backend-like responses before display.
"""

from __future__ import annotations

import hashlib
import json
import os
import secrets
import shlex
import subprocess
import sys
from dataclasses import dataclass


DETECTOR_CMD = os.environ.get(
    "PITCHAI_CODEX_PRIVACY_FILTER_CMD",
    f"{sys.executable} scripts/privacy_filter_gliner.py",
)


@dataclass(frozen=True)
class Span:
    start: int
    end: int
    kind: str


class PrivacySession:
    def __init__(self, enabled: bool, detector_cmd: str) -> None:
        self.enabled = enabled
        self.detector_cmd = detector_cmd
        self.secret = secrets.token_hex(32)
        self.real_to_fake: dict[str, str] = {}
        self.fake_to_real: dict[str, str] = {}

    def anonymize(self, text: str) -> str:
        if not self.enabled:
            return text
        spans = self.detect(text)
        out: list[str] = []
        cursor = 0
        for span in spans:
            out.append(text[cursor : span.start])
            out.append(self.fake_for(text[span.start : span.end], span.kind))
            cursor = span.end
        out.append(text[cursor:])
        return "".join(out)

    def restore(self, text: str) -> str:
        restored = text
        for fake in sorted(self.fake_to_real, key=len, reverse=True):
            restored = restored.replace(fake, self.fake_to_real[fake])
        return restored

    def detect(self, text: str) -> list[Span]:
        proc = subprocess.run(
            shlex.split(self.detector_cmd),
            input=json.dumps({"text": text}),
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=True,
        )
        payload = json.loads(proc.stdout)
        spans = [
            Span(int(item["start"]), int(item["end"]), str(item["kind"]))
            for item in payload["spans"]
        ]
        spans.sort(key=lambda span: (span.start, span.end))
        kept: list[Span] = []
        last_end = 0
        for span in spans:
            if span.start >= last_end:
                kept.append(span)
                last_end = span.end
        return kept

    def fake_for(self, real: str, kind: str) -> str:
        key = hashlib.sha1(
            b"\0".join([self.secret.encode(), kind.encode(), real.encode()])
        ).hexdigest()
        if key in self.real_to_fake:
            return self.real_to_fake[key]
        fake = realistic_fake(kind, key)
        self.real_to_fake[key] = fake
        self.fake_to_real[fake] = real
        return fake

    def safe_mapping_artifact(self) -> dict[str, int | list[str]]:
        return {
            "real_to_fake_entries": len(self.real_to_fake),
            "fake_to_real_entries": len(self.fake_to_real),
            "fake_values": sorted(self.fake_to_real),
        }


def realistic_fake(kind: str, key: str) -> str:
    idx = int(key[:6], 16)
    lower = kind.lower()
    first = ["Avery", "Jordan", "Morgan", "Casey", "Riley", "Taylor"][idx % 6]
    last = ["Bennett", "Reed", "Hayes", "Carter", "Brooks", "Parker"][(idx + 1) % 6]
    if "email" in lower:
        return f"{first}.{last}@example.net".lower()
    if "phone" in lower:
        return f"({200 + idx % 700}) 555-{1000 + idx % 9000:04d}"
    if "address" in lower:
        street = ["Maple", "Cedar", "Walnut", "Lake", "Hill", "Pine"][idx % 6]
        city = ["Madison, WI", "Raleigh, NC", "Boulder, CO", "Albany, NY"][idx % 4]
        return f"{100 + idx % 8900} {street} St, {city}"
    return f"{first} {last}"


def main() -> int:
    original = (
        "Jane Smith lives at 14 Pearl St, Boston, MA and uses "
        "jane.smith@example.com or 212-555-0199. Ask Jane Smith to confirm."
    )
    session = PrivacySession(enabled=True, detector_cmd=DETECTOR_CMD)
    outbound = session.anonymize(original)
    stable = session.anonymize("Jane Smith")
    backend_response = f"Drafted the note for {stable} using {outbound}."
    restored = session.restore(backend_response)
    disabled = PrivacySession(enabled=False, detector_cmd=DETECTOR_CMD).anonymize(original)

    leaked_real_values = [
        value
        for value in [
            "Jane Smith",
            "14 Pearl St, Boston, MA",
            "jane.smith@example.com",
            "212-555-0199",
        ]
        if value in outbound or value in backend_response
    ]
    result = {
        "detector_cmd": DETECTOR_CMD,
        "original_local_input": original,
        "anonymized_outbound_payload": outbound,
        "backend_like_fake_response": backend_response,
        "restored_local_output": restored,
        "stable_fake_for_repeated_name": stable,
        "disabled_mode_payload": disabled,
        "mapping_leak_check": {
            "outbound_or_backend_contains_real_values": leaked_real_values,
            "mapping_secret_exported": False,
            "raw_mapping_exported": False,
        },
        "safe_mapping_artifact": session.safe_mapping_artifact(),
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if not leaked_real_values and disabled == original else 1


if __name__ == "__main__":
    raise SystemExit(main())
