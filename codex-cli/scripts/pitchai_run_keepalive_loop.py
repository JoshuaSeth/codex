#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional


def _optional_env(name: str) -> Optional[str]:
    value = os.getenv(name)
    return value if value else None


def _read_state(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except Exception:
        return {}


def _write_state(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def _read_bootstrap_prompt(path: Optional[Path]) -> Optional[str]:
    if path is None:
        return None
    try:
        text = path.read_text(encoding="utf-8").strip()
        return text if text else None
    except Exception:
        return None


@dataclass(frozen=True)
class KeepaliveConfig:
    codex_bin: str
    config_home: Path
    config_file: Path
    workdir: Path
    state_path: Path
    prompt: str
    bootstrap_prompt: Optional[str]
    max_iterations: Optional[int]
    error_sleep_s: int
    stop_hook_sleep_s_override: Optional[int]


def _resolve_config() -> KeepaliveConfig:
    volume_root = Path(os.getenv("PITCHAI_KEEPALIVE_VOLUME", "/mnt/codex_keepalive"))
    config_home = Path(os.getenv("PITCHAI_KEEPALIVE_CODEX_HOME", str(volume_root / "codex_home")))
    workdir = Path(os.getenv("PITCHAI_KEEPALIVE_WORKDIR", str(volume_root / "workdir")))
    state_path = Path(os.getenv("PITCHAI_KEEPALIVE_STATE_PATH", str(volume_root / "state.json")))

    config_path_env = _optional_env("PITCHAI_KEEPALIVE_CONFIG_PATH")
    config_file = (
        Path(config_path_env)
        if config_path_env
        else Path(__file__).with_name("pitchai_keepalive_loop_config.toml")
    )

    codex_bin = os.getenv("PITCHAI_CODEX_BIN") or os.getenv("CODEX_BIN") or "codex"

    prompt = os.getenv("PITCHAI_KEEPALIVE_PROMPT", "continue").strip()
    if not prompt:
        prompt = "continue"

    bootstrap_prompt = _optional_env("PITCHAI_KEEPALIVE_BOOTSTRAP_PROMPT")
    bootstrap_prompt_path_env = _optional_env("PITCHAI_KEEPALIVE_BOOTSTRAP_PROMPT_PATH")
    bootstrap_prompt = bootstrap_prompt or _read_bootstrap_prompt(
        Path(bootstrap_prompt_path_env) if bootstrap_prompt_path_env else None
    )

    max_iterations_env = _optional_env("PITCHAI_KEEPALIVE_MAX_ITERATIONS")
    max_iterations = None
    if max_iterations_env:
        try:
            max_iterations = max(1, int(max_iterations_env))
        except ValueError:
            max_iterations = None

    error_sleep_s = int(os.getenv("PITCHAI_KEEPALIVE_ERROR_SLEEP_S", "60"))
    error_sleep_s = max(0, error_sleep_s)

    stop_hook_sleep_s_override_env = _optional_env("PITCHAI_KEEPALIVE_STOP_HOOK_SLEEP_S")
    stop_hook_sleep_s_override = None
    if stop_hook_sleep_s_override_env:
        try:
            stop_hook_sleep_s_override = max(0, int(stop_hook_sleep_s_override_env))
        except ValueError:
            stop_hook_sleep_s_override = None

    return KeepaliveConfig(
        codex_bin=codex_bin,
        config_home=config_home,
        config_file=config_file,
        workdir=workdir,
        state_path=state_path,
        prompt=prompt,
        bootstrap_prompt=bootstrap_prompt,
        max_iterations=max_iterations,
        error_sleep_s=error_sleep_s,
        stop_hook_sleep_s_override=stop_hook_sleep_s_override,
    )


def _spawn_codex(cfg: KeepaliveConfig, *, thread_id: Optional[str], prompt: str) -> tuple[int, Optional[str]]:
    cfg.config_home.mkdir(parents=True, exist_ok=True)
    cfg.workdir.mkdir(parents=True, exist_ok=True)

    cmd = [
        cfg.codex_bin,
        "exec",
        "--config-home",
        str(cfg.config_home),
        "--config-file",
        str(cfg.config_file),
        "--skip-git-repo-check",
        "--json",
        "--cd",
        str(cfg.workdir),
    ]

    if cfg.stop_hook_sleep_s_override is not None:
        cmd.extend(
            [
                "--config",
                f'stop_hook_command=["bash","-lc","sleep {cfg.stop_hook_sleep_s_override}"]',
            ]
        )

    if thread_id:
        cmd.extend(["resume", thread_id, prompt])
    else:
        cmd.append(prompt)

    proc = subprocess.Popen(
        cmd,
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
    return int(rc), captured_thread_id


def main() -> int:
    cfg = _resolve_config()
    fixed_thread_id = (os.getenv("PITCHAI_KEEPALIVE_THREAD_ID") or "").strip() or None

    iteration = 0
    while True:
        iteration += 1
        thread_id = fixed_thread_id
        if thread_id is None:
            state = _read_state(cfg.state_path)
            thread_id_raw = state.get("thread_id") if isinstance(state, dict) else None
            thread_id = thread_id_raw.strip() if isinstance(thread_id_raw, str) else None

        prompt = cfg.prompt
        if thread_id is None:
            if cfg.bootstrap_prompt is None:
                print(
                    "[keepalive] No thread_id found. Set PITCHAI_KEEPALIVE_THREAD_ID, or set "
                    "PITCHAI_KEEPALIVE_BOOTSTRAP_PROMPT / PITCHAI_KEEPALIVE_BOOTSTRAP_PROMPT_PATH "
                    "to create the initial thread.",
                    file=sys.stderr,
                )
                return 2
            prompt = cfg.bootstrap_prompt

        print(
            f"[keepalive] iteration={iteration} thread_id={thread_id or '(new)'}",
            file=sys.stderr,
        )

        rc, captured_thread_id = _spawn_codex(cfg, thread_id=thread_id, prompt=prompt)

        if captured_thread_id and captured_thread_id != thread_id:
            state = _read_state(cfg.state_path)
            if state.get("thread_id") != captured_thread_id:
                state["thread_id"] = captured_thread_id
                _write_state(cfg.state_path, state)
            thread_id = captured_thread_id

        if rc != 0:
            print(
                f"[keepalive] codex exited rc={rc}; sleeping {cfg.error_sleep_s}s before retry",
                file=sys.stderr,
            )
            if cfg.error_sleep_s:
                time.sleep(cfg.error_sleep_s)

        if cfg.max_iterations is not None and iteration >= cfg.max_iterations:
            print(f"[keepalive] reached max iterations ({cfg.max_iterations}); exiting", file=sys.stderr)
            return rc


if __name__ == "__main__":
    raise SystemExit(main())

