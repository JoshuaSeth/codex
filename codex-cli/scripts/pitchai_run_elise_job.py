#!/usr/bin/env python3
from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional


@dataclass
class CodexRunConfig:
    codex_home: Path
    workdir: Path
    state_path: Path
    prompt_path: Path
    config_path: Path


def _require_env(name: str) -> str:
    value = os.getenv(name)
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def _read_state(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return {}


def _write_state(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def _decode_auth_json(config_home: Path) -> None:
    b64 = _require_env("CODEX_AUTH_JSON_B64")
    config_home.mkdir(parents=True, exist_ok=True)
    auth_path = config_home / "auth.json"
    auth_path.write_bytes(base64.b64decode(b64.encode("utf-8")))
    try:
        os.chmod(auth_path, 0o600)
    except PermissionError:
        # Azure Files mounts do not always support chmod (CIFS), but Codex can
        # still read the credentials file.
        pass


def _model_args() -> tuple[list[str], list[str]]:
    model = os.getenv("PITCHAI_CODEX_MODEL", "").strip()
    if not model:
        return ([], [])
    if model == "gpt-5.2-medium":
        return (["-m", "gpt-5.2-codex"], ["-c", "model_reasoning_effort=medium"])
    if model == "gpt-5.2-high":
        return (["-m", "gpt-5.2-codex"], ["-c", "model_reasoning_effort=high"])
    return (["-m", model], [])


def _resolve_config() -> CodexRunConfig:
    volume_root = Path(os.getenv("PITCHAI_ELISE_VOLUME", "/mnt/elise"))
    workdir = Path(os.getenv("PITCHAI_ELISE_WORKDIR", str(volume_root / "elise")))
    codex_home = Path(os.getenv("CODEX_HOME", str(volume_root / "codex_home")))
    state_path = Path(os.getenv("PITCHAI_ELISE_STATE_PATH", str(volume_root / "state.json")))

    prompt_path = Path(os.getenv("PITCHAI_PROMPT_PATH", "/opt/pitchai/elise_prompt.md"))
    override = os.getenv("PITCHAI_PROMPT_OVERRIDE", "").strip()
    if override:
        tmp = volume_root / "prompt_override.md"
        tmp.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(override + "\n", encoding="utf-8")
        prompt_path = tmp

    config_path = Path(os.getenv("PITCHAI_CODEX_CONFIG_PATH", "/opt/pitchai/elise_config.toml"))
    return CodexRunConfig(
        codex_home=codex_home,
        workdir=workdir,
        state_path=state_path,
        prompt_path=prompt_path,
        config_path=config_path,
    )


def _spawn_codex(cfg: CodexRunConfig, *, thread_id: Optional[str]) -> int:
    cfg.codex_home.mkdir(parents=True, exist_ok=True)
    cfg.workdir.mkdir(parents=True, exist_ok=True)
    _decode_auth_json(cfg.codex_home)

    model_args, config_overrides = _model_args()

    base_cmd = [
        "codex",
        "exec",
        "--config-home",
        str(cfg.codex_home),
        "--config-file",
        str(cfg.config_path),
        "--skip-git-repo-check",
        "--json",
        "--cd",
        str(cfg.workdir),
        *model_args,
        *config_overrides,
    ]
    if thread_id:
        base_cmd.extend(["resume", thread_id])

    with cfg.prompt_path.open("rb") as prompt_fh:
        proc = subprocess.Popen(
            base_cmd + ["-"],
            stdin=prompt_fh,
            stdout=subprocess.PIPE,
            stderr=sys.stderr,
            text=True,
            bufsize=1,
        )

        captured_thread_id: Optional[str] = None
        assert proc.stdout is not None
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            try:
                evt = json.loads(line)
            except Exception:
                continue
            if isinstance(evt, dict) and evt.get("type") == "thread.started":
                tid = evt.get("thread_id")
                if isinstance(tid, str) and tid.strip():
                    captured_thread_id = tid.strip()

        rc = proc.wait()

    if captured_thread_id:
        state = _read_state(cfg.state_path)
        if state.get("thread_id") != captured_thread_id:
            state["thread_id"] = captured_thread_id
            _write_state(cfg.state_path, state)

    return int(rc)


def main() -> int:
    cfg = _resolve_config()
    state = _read_state(cfg.state_path)
    thread_id = state.get("thread_id") if isinstance(state, dict) else None
    if not isinstance(thread_id, str) or not thread_id.strip():
        thread_id = None
    return _spawn_codex(cfg, thread_id=thread_id)


if __name__ == "__main__":
    raise SystemExit(main())
