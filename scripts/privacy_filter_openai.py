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
CHUNK_SIZE = 512
CHUNK_OVERLAP = 64


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


def chunk_offsets(text: str) -> list[tuple[int, str]]:
    if len(text) <= CHUNK_SIZE:
        return [(0, text)]

    chunks: list[tuple[int, str]] = []
    start = 0
    while start < len(text):
        end = min(len(text), start + CHUNK_SIZE)
        if end < len(text):
            split = max(text.rfind("\n", start, end), text.rfind(" ", start, end))
            if split > start + (CHUNK_SIZE // 2):
                end = split + 1
        chunks.append((start, text[start:end]))
        if end == len(text):
            break
        start = max(end - CHUNK_OVERLAP, start + 1)
    return chunks


def detect(text: str) -> list[dict[str, int | str]]:
    entities: list[dict[str, Any]] = []
    model = classifier()
    for offset, chunk in chunk_offsets(text):
        for entity in model(chunk):
            shifted = dict(entity)
            shifted["start"] = int(shifted["start"]) + offset
            shifted["end"] = int(shifted["end"]) + offset
            entities.append(shifted)
    return merge_adjacent_spans(text, entities)


def main() -> int:
    payload = json.load(sys.stdin)
    text = payload["text"]
    print(json.dumps({"spans": detect(text)}, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
