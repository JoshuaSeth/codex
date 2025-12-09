#!/usr/bin/env python3
"""Dispatch a background Codex web agent run for Elise."""
from __future__ import annotations

import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict

def _resolve_elise_core_root() -> Path:
    env_root = os.environ.get("CODEX_ELISE_CORE_ROOT") or os.environ.get("ELISE_CORE_ROOT")
    if env_root:
        return Path(env_root).expanduser().resolve()

    script_path = Path(__file__).resolve()
    for parent in script_path.parents:
        candidate = parent / "Elise" / "core"
        if candidate.exists():
            return candidate.resolve()

    raise SystemExit(
        "Elise core repo not found. Set CODEX_ELISE_CORE_ROOT (or ELISE_CORE_ROOT) "
        "to the absolute path of the Elise/core checkout."
    )


ELISE_CORE_ROOT = _resolve_elise_core_root()
LOG_DIR = ELISE_CORE_ROOT / "logs"
TASK_INDEX = LOG_DIR / "elise_web_tasks.json"
TASK_LOG = LOG_DIR / "elise_web_tasks_log.jsonl"
RUNNER_SCRIPT = ELISE_CORE_ROOT / "infrastructure" / "web_tasks" / "run_web_agent_task.py"
DEFAULT_WEB_AGENT_CONFIG = Path.home() / ".codex" / "web-agent.toml"


def _load_json(path: Path) -> Dict[str, Any]:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text())
    except json.JSONDecodeError:
        return {}


def _write_index(index: Dict[str, Any]) -> None:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    TASK_INDEX.write_text(json.dumps(index, indent=2), encoding="utf-8")


def _append_history(entry: Dict[str, Any]) -> None:
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    with TASK_LOG.open("a", encoding="utf-8") as handle:
        json.dump(entry, handle)
        handle.write("\n")


def _build_prompt(instructions: str, context: str | None) -> str:
    base = (
        "You are the Codex Web Agent acting as Elise's web-form colleague. "
        "Strictly focus on carrying out the requested visual web actions via the Chrome "
        "DevTools MCP (navigate, click, type, fill forms, submit buttons, etc.). "
        "Do not perform open-ended research or unrelated shell commands. If a step cannot "
        "be completed, clearly describe what blocked you and what is needed."
    )
    prompt = f"{base}\n\nPrimary task:\n{instructions.strip()}\n"
    if context:
        prompt += f"\nAdditional context from Elise:\n{context.strip()}\n"
    prompt += "\nFinish with a concise summary of exactly what actions you completed, any data entered, and follow-up steps."
    return prompt


def main() -> int:
    args_json = os.environ.get("CODEX_TOOL_ARGS_JSON")
    if not args_json:
        print("CODEX_TOOL_ARGS_JSON missing", file=sys.stderr)
        return 1
    try:
        payload = json.loads(args_json)
    except json.JSONDecodeError as exc:  # noqa: BLE001
        print(f"Invalid CODEX_TOOL_ARGS_JSON: {exc}", file=sys.stderr)
        return 1

    instructions = (payload.get("instructions") or "").strip()
    if not instructions:
        print("'instructions' is required", file=sys.stderr)
        return 1
    context = (payload.get("context") or "").strip() or None

    call_id = os.environ.get("CODEX_TOOL_CALL_ID") or f"task-{datetime.now(timezone.utc).timestamp()}"
    conversation_id = os.environ.get("CODEX_CONVERSATION_ID") or ""
    turn_id = os.environ.get("CODEX_TURN_ID") or ""
    turn_cwd = os.environ.get("CODEX_TURN_CWD") or ""
    config_file = os.environ.get("CODEX_CONFIG_FILE") or str(Path.home() / ".codex" / "elise.toml")

    web_agent_config = payload.get("web_agent_config") or os.environ.get("ELISE_WEB_AGENT_CONFIG")
    if not web_agent_config:
        web_agent_config = str(DEFAULT_WEB_AGENT_CONFIG)

    web_prompt = _build_prompt(instructions, context)

    timestamp = datetime.now(timezone.utc).isoformat()
    index = _load_json(TASK_INDEX)
    index[call_id] = {
        "task_id": call_id,
        "timestamp": timestamp,
        "conversation_id": conversation_id,
        "turn_id": turn_id,
        "turn_cwd": turn_cwd,
        "config_file": config_file,
        "instructions": instructions,
        "context": context,
        "web_agent_prompt": web_prompt,
        "web_agent_config": web_agent_config,
        "status": "pending",
    }
    _write_index(index)
    _append_history({
        "event": "queued",
        "task_id": call_id,
        "timestamp": timestamp,
        "conversation_id": conversation_id,
        "instructions": instructions,
    })

    if not RUNNER_SCRIPT.exists():
        print(f"Runner script not found: {RUNNER_SCRIPT}", file=sys.stderr)
        return 1

    cmd = [
        sys.executable,
        str(RUNNER_SCRIPT),
        "--task-id",
        call_id,
    ]
    env = os.environ.copy()
    env.setdefault("ELISE_WEB_TASK_INDEX", str(TASK_INDEX))
    env.setdefault("ELISE_WEB_TASK_LOG", str(TASK_LOG))
    env.setdefault("ELISE_WEB_AGENT_CONFIG", web_agent_config)

    subprocess.Popen(  # noqa: S603, S607
        cmd,
        cwd=str(ELISE_CORE_ROOT),
        env=env,
        start_new_session=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    summary = instructions if len(instructions) <= 120 else f"{instructions[:117]}..."
    print(
        f"Dispatched the web-form colleague (task {call_id}) for: {summary}. "
        "I'll resume once their actions are complete."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
