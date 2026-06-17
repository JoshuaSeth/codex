#!/usr/bin/env python3
"""Local model-backed detector for Codex privacy middleware.

Protocol:
  stdin:  {"text": "..."}
  stdout: {"spans": [{"start": 0, "end": 10, "kind": "PERSON_NAME"}]}

The model is downloaded by Hugging Face/GLiNER into the local cache and runs
locally. This script intentionally performs no replacement itself; Codex owns
the local reversible fake-value map.
"""

from __future__ import annotations

import json
import sys

from gliner import GLiNER


MODEL_ID = "urchade/gliner_multi_pii-v1"
LABELS = ["person name", "address", "email", "phone number"]
KIND_BY_LABEL = {
    "person name": "PERSON_NAME",
    "address": "ADDRESS",
    "email": "EMAIL",
    "phone number": "PHONE",
}


def main() -> int:
    payload = json.load(sys.stdin)
    text = payload["text"]
    model = GLiNER.from_pretrained(MODEL_ID)
    entities = model.predict_entities(text, LABELS, threshold=0.3)
    spans = [
        {
            "start": int(entity["start"]),
            "end": int(entity["end"]),
            "kind": KIND_BY_LABEL.get(entity["label"], entity["label"].upper()),
        }
        for entity in entities
    ]
    print(json.dumps({"spans": spans}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
