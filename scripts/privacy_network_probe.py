#!/usr/bin/env python3
"""Run a built Codex binary against a local Responses mock and capture privacy proof."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


REAL_VALUES = [
    "Jane Smith",
    "14 Pearl St",
    "jane.smith@example.com",
    "(415) 555-1212",
]


PROMPT = (
    "Please summarize this contact twice: Jane Smith lives at 14 Pearl St, "
    "email jane.smith@example.com, phone (415) 555-1212. Jane Smith needs a follow-up."
)


def _sse_event(event: dict[str, Any]) -> str:
    kind = event["type"]
    return f"event: {kind}\ndata: {json.dumps(event, separators=(',', ':'))}\n\n"


def _sse_response(text: str) -> bytes:
    events = [
        {"type": "response.created", "response": {"id": "resp-privacy-probe"}},
        {
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": "msg-privacy-probe",
                "content": [{"type": "output_text", "text": text}],
            },
        },
        {
            "type": "response.completed",
            "response": {
                "id": "resp-privacy-probe",
                "usage": {
                    "input_tokens": 0,
                    "input_tokens_details": None,
                    "output_tokens": 0,
                    "output_tokens_details": None,
                    "total_tokens": 0,
                },
            },
        },
    ]
    return "".join(_sse_event(event) for event in events).encode()


def _text_values(value: Any) -> list[str]:
    if isinstance(value, dict):
        out: list[str] = []
        for child in value.values():
            out.extend(_text_values(child))
        return out
    if isinstance(value, list):
        out: list[str] = []
        for child in value:
            out.extend(_text_values(child))
        return out
    if isinstance(value, str):
        return [value]
    return []


class ProbeState:
    request_json: dict[str, Any] | None = None


def make_handler(state: ProbeState) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self) -> None:  # noqa: N802
            length = int(self.headers.get("content-length", "0"))
            body = self.rfile.read(length)
            state.request_json = json.loads(body)

            request_text = "\n".join(_text_values(state.request_json))
            backend_text = f"Backend saw this anonymized prompt: {request_text}"
            response = _sse_response(backend_text)

            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)

        def log_message(self, _format: str, *_args: Any) -> None:
            return

    return Handler


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--codex", default="codex-rs/target/release/codex")
    parser.add_argument("--out", default="docs/privacy_network_probe_20260618.json")
    parser.add_argument("--detector-cmd")
    parser.add_argument("--timeout", type=int, default=600)
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    codex = (
        (root / args.codex).resolve()
        if not Path(args.codex).is_absolute()
        else Path(args.codex)
    )
    out = (
        (root / args.out).resolve()
        if not Path(args.out).is_absolute()
        else Path(args.out)
    )
    detector_cmd = args.detector_cmd or (
        "uv run --python 3.12 --with 'transformers>=4.53.0' --with torch --with accelerate "
        f"python {root / 'scripts/privacy_filter_openai.py'}"
    )

    state = ProbeState()
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    env = os.environ.copy()
    with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
        env.update(
            {
                "CODEX_HOME": home,
                "CODEX_SQLITE_HOME": home,
                "CODEX_API_KEY": "dummy",
                "PITCHAI_CODEX_PRIVACY_MIDDLEWARE": "1",
                "PITCHAI_CODEX_PRIVACY_FILTER_CMD": detector_cmd,
            }
        )
        base_url = f"http://127.0.0.1:{server.server_port}/v1"
        cmd = [
            str(codex),
            "-c",
            f"openai_base_url={json.dumps(base_url)}",
            "exec",
            "--skip-git-repo-check",
            PROMPT,
        ]
        try:
            proc = subprocess.run(
                cmd,
                cwd=cwd,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=args.timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            stdout = (
                exc.stdout.decode(errors="replace")
                if isinstance(exc.stdout, bytes)
                else exc.stdout
            )
            stderr = (
                exc.stderr.decode(errors="replace")
                if isinstance(exc.stderr, bytes)
                else exc.stderr
            )
            proc = subprocess.CompletedProcess(
                cmd,
                124,
                stdout=stdout or "",
                stderr=(stderr or "") + f"\nTimed out after {args.timeout}s",
            )

    server.shutdown()
    thread.join(timeout=5)

    if state.request_json is None:
        raise RuntimeError(f"mock backend captured no request; stderr={proc.stderr}")

    request_blob = json.dumps(state.request_json, sort_keys=True)
    stdout = proc.stdout
    stderr = proc.stderr
    real_in_request = [value for value in REAL_VALUES if value in request_blob]
    real_restored_stdout = [value for value in REAL_VALUES if value in stdout]
    fake_request_values = [
        text
        for text in _text_values(state.request_json)
        if any(marker in text for marker in ["@example.", " St", "555"])
    ]

    proof = {
        "detector": "openai/privacy-filter via scripts/privacy_filter_openai.py",
        "binary": str(codex),
        "prompt": PROMPT,
        "exit_code": proc.returncode,
        "captured_request_contains_real_values": real_in_request,
        "stdout_restored_real_values": real_restored_stdout,
        "captured_request_candidate_fake_texts": fake_request_values,
        "captured_request": state.request_json,
        "stdout": stdout,
        "stderr": stderr,
    }
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(proof, indent=2, sort_keys=True) + "\n")

    if proc.returncode != 0:
        raise RuntimeError(f"codex exited {proc.returncode}; see {out}")
    if real_in_request:
        raise RuntimeError(
            f"real values leaked into outbound request: {real_in_request}; see {out}"
        )
    if len(real_restored_stdout) < len(REAL_VALUES):
        raise RuntimeError(f"stdout did not restore all real values; see {out}")

    print(out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
