#!/usr/bin/env python3
"""OpenAI Privacy Filter detector adapter for Codex privacy middleware.

Protocol:
  stdin:  {"text": "..."}
  stdout: {"spans": [{"start": 0, "end": 10, "kind": "private_person"}]}

This runs the official `openai/privacy-filter` token-classification model
locally through Hugging Face Transformers. Codex owns fake-value replacement
and the local reversible map; this script only returns detected spans.
"""

from __future__ import annotations

import json
import sys
from functools import cache
from typing import Any

from transformers import pipeline


MODEL_ID = "openai/privacy-filter"


@cache
def classifier() -> Any:
    return pipeline(
        task="token-classification",
        model=MODEL_ID,
        aggregation_strategy="simple",
    )


def trim_span(text: str, start: int, end: int) -> tuple[int, int]:
    while start < end and text[start].isspace():
        start += 1
    while end > start and text[end - 1].isspace():
        end -= 1
    return start, end


def merge_adjacent_spans(
    text: str, entities: list[dict[str, Any]]
) -> list[dict[str, int | str]]:
    spans: list[dict[str, int | str]] = []
    for entity in sorted(entities, key=lambda item: (int(item["start"]), int(item["end"]))):
        kind = str(entity.get("entity_group") or entity.get("entity") or "private_unknown")
        start, end = trim_span(text, int(entity["start"]), int(entity["end"]))
        if start >= end:
            continue
        if (
            spans
            and spans[-1]["kind"] == kind
            and text[int(spans[-1]["end"]) : start].strip() == ""
        ):
            spans[-1]["end"] = max(int(spans[-1]["end"]), end)
            continue
        spans.append({"start": start, "end": end, "kind": kind})
    return spans


def main() -> int:
    payload = json.load(sys.stdin)
    text = payload["text"]
    entities = classifier()(text)
    print(json.dumps({"spans": merge_adjacent_spans(text, entities)}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
