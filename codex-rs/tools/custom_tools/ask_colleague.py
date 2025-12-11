#!/usr/bin/env python3
"""Ask a Codex colleague by recursively spawning codex-dev."""

from __future__ import annotations

import json
import os
import random
import shlex
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Tuple


ADJECTIVES = [
    "adaptable",
    "brilliant",
    "clever",
    "diligent",
    "eager",
    "fearless",
    "gentle",
    "helpful",
    "insightful",
    "jovial",
    "kind",
    "lively",
    "mindful",
    "noble",
    "optimistic",
    "patient",
    "quick",
    "resilient",
    "steady",
    "thoughtful",
    "upbeat",
    "vivid",
    "warm",
    "youthful",
    "zealous",
]

ANIMALS = [
    "alpaca",
    "badger",
    "cougar",
    "dolphin",
    "eagle",
    "falcon",
    "gazelle",
    "heron",
    "ibis",
    "jaguar",
    "koala",
    "lemur",
    "magpie",
    "narwhal",
    "otter",
    "panther",
    "quail",
    "raccoon",
    "sparrow",
    "tiger",
    "urchin",
    "viper",
    "whale",
    "xerus",
    "yak",
    "zebra",
]


def load_args() -> Dict[str, Any]:
    raw = os.environ.get("CODEX_TOOL_ARGS_JSON")
    if not raw:
        raise ValueError("CODEX_TOOL_ARGS_JSON is missing")
    try:
        payload = json.loads(raw)
    except json.JSONDecodeError as exc:  # pragma: no cover - env issues
        raise ValueError(f"Invalid CODEX_TOOL_ARGS_JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise ValueError("CODEX_TOOL_ARGS_JSON must encode an object")
    return payload


def slugify_candidate(adjective: str, animal: str) -> str:
    return f"{adjective.lower()}-{animal.lower()}"


def random_slug(existing: Dict[str, Any]) -> str:
    while True:
        slug = slugify_candidate(random.choice(ADJECTIVES), random.choice(ANIMALS))
        if slug not in existing:
            return slug


def resolve_codex_home() -> Path:
    colleague_home = os.environ.get("CODEX_COLLEAGUE_HOME")
    if colleague_home:
        path = Path(colleague_home).expanduser()
        path.mkdir(parents=True, exist_ok=True)
        os.environ["CODEX_HOME"] = str(path)
        bootstrap_codex_home_if_needed(path)
        return path

    env_home = os.environ.get("CODEX_HOME")
    if env_home:
        path = Path(env_home).expanduser()
        path.mkdir(parents=True, exist_ok=True)
        return path
    turn_cwd = os.environ.get("CODEX_TURN_CWD")
    if turn_cwd:
        fallback = Path(turn_cwd).expanduser() / ".codex_home"
        os.environ["CODEX_HOME"] = str(fallback)
        fallback.mkdir(parents=True, exist_ok=True)
        bootstrap_codex_home_if_needed(fallback)
        return fallback
    default_path = Path.home() / ".codex"
    os.environ["CODEX_HOME"] = str(default_path)
    default_path.mkdir(parents=True, exist_ok=True)
    return default_path


def bootstrap_codex_home_if_needed(codex_home: Path) -> None:
    source = Path.home() / ".codex"
    if not source.exists():
        return
    # Avoid needless copies when the fallback already mirrors the source.
    marker = codex_home / ".bootstrapped"
    if marker.exists():
        return
    try:
        shutil.copytree(
            source,
            codex_home,
            dirs_exist_ok=True,
            ignore=shutil.ignore_patterns("sessions", "live"),
        )
        marker.touch()
    except OSError as exc:  # pragma: no cover - best-effort copy
        print(
            f"ask_colleague (warning): failed to copy ~/.codex into workspace: {exc}",
            file=sys.stderr,
        )


def index_paths() -> Tuple[Path, Path]:
    codex_home = resolve_codex_home()
    base_dir = codex_home / "colleagues"
    return base_dir, base_dir / "ask_colleague_index.json"


def load_index() -> Dict[str, Any]:
    _, index_path = index_paths()
    if not index_path.exists():
        return {}
    try:
        with index_path.open("r", encoding="utf-8") as handle:
            return json.load(handle)
    except json.JSONDecodeError:
        return {}


def save_index(data: Dict[str, Any]) -> None:
    base_dir, index_path = index_paths()
    base_dir.mkdir(parents=True, exist_ok=True)
    tmp_path = index_path.with_suffix(".tmp")
    with tmp_path.open("w", encoding="utf-8") as handle:
        json.dump(data, handle, indent=2)
    tmp_path.replace(index_path)


def build_command(new_thread: bool, conversation_id: str | None, prompt: str) -> List[str]:
    binary = os.environ.get("CODEX_COLLEAGUE_BIN", "codex-dev")
    extra_args = shlex.split(os.environ.get("CODEX_COLLEAGUE_ARGS", ""))
    cmd: List[str] = [binary] + extra_args
    cmd.extend(["--config", "stop_hook_command=[]", "--config", "tool_hook_command=[]"])
    config_file = os.environ.get("CODEX_CONFIG_FILE")
    if config_file:
        cmd.extend(["--config-file", config_file])
    if new_thread:
        cmd.append("exec")
        cmd.append("--json")
        if os.environ.get("CODEX_COLLEAGUE_SKIP_GIT_CHECK") == "1":
            cmd.append("--skip-git-repo-check")
        cmd.append(prompt)
    else:
        if not conversation_id:
            raise ValueError("conversation_id required for resume")
        cmd.extend(["exec", "--json"])
        if os.environ.get("CODEX_COLLEAGUE_SKIP_GIT_CHECK") == "1":
            cmd.append("--skip-git-repo-check")
        cmd.extend(["resume", conversation_id, prompt])
    return cmd


def run_codex(cmd: List[str]) -> Tuple[str, str]:
    proc = subprocess.Popen(
        cmd,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=os.environ.copy(),
    )
    thread_id: str | None = None
    final_message: str | None = None
    assert proc.stdout is not None  # for type checkers
    for line in proc.stdout:
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "thread.started" and not thread_id:
            thread_id = event.get("thread_id")
        if event.get("type") == "item.completed":
            item = event.get("item", {})
            if item.get("type") == "agent_message":
                final_message = item.get("text")
    stdout_data, stderr_data = proc.communicate()
    if proc.returncode != 0:
        raise RuntimeError(
            f"codex-dev exited with {proc.returncode}: {stderr_data.strip() or stdout_data.strip()}"
        )
    if not thread_id:
        raise RuntimeError("Failed to capture conversation id from codex output")
    if final_message is None:
        final_message = ""
    return thread_id, final_message


def main() -> int:
    try:
        payload = load_args()
        prompt_raw = payload.get("prompt")
        if not isinstance(prompt_raw, str) or not prompt_raw.strip():
            raise ValueError("`prompt` must be a non-empty string")
        prompt = prompt_raw.strip()
        requested_id = payload.get("id")
        if requested_id is not None and not isinstance(requested_id, str):
            raise ValueError("`id` must be a string if provided")
        index = load_index()
        friendly_id: str | None = None
        conversation_id: str | None = None
        is_new_thread = True
        if requested_id:
            lookup = requested_id.strip().lower()
            lower_map = {k.lower(): k for k in index.keys()}
            canonical = lower_map.get(lookup)
            if not canonical:
                raise ValueError(f"No colleague found with id '{requested_id}'")
            friendly_id = canonical
            conversation_id = index[canonical]["conversation_id"]
            is_new_thread = False
        cmd = build_command(is_new_thread, conversation_id, prompt)
        conversation_uuid, message = run_codex(cmd)
        if is_new_thread:
            if friendly_id is None:
                friendly_id = random_slug(index)
            index[friendly_id] = {
                "conversation_id": conversation_uuid,
                "created_at": datetime.now(timezone.utc).isoformat(),
            }
            save_index(index)
        result = {
            "status": "ok",
            "mode": "resume" if not is_new_thread else "new",
            "friendly_id": friendly_id,
            "conversation_id": conversation_uuid,
            "message": message,
        }
        print(json.dumps(result))
        return 0
    except Exception as exc:  # noqa: BLE001
        print(f"ask_colleague: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
