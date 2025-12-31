#!/usr/bin/env python3
from __future__ import annotations

import base64
import json
import os
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional


@dataclass
class CodexRunConfig:
    volume_root: Path
    codex_home: Path
    workdir: Path
    state_path: Path
    prompt_path: Path
    config_path: Path
    prompt_queue_dir: Path


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
    config_home.mkdir(parents=True, exist_ok=True)
    auth_path = config_home / "auth.json"
    b64 = os.getenv("CODEX_AUTH_JSON_B64", "").strip()
    if not b64:
        if auth_path.exists():
            return
        raise RuntimeError("Missing required environment variable: CODEX_AUTH_JSON_B64")

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


def _safe_remove_dir(path: Path) -> None:
    if not path.exists():
        return
    for child in path.iterdir():
        try:
            if child.is_dir():
                _safe_remove_dir(child)
            else:
                child.unlink(missing_ok=True)
        except Exception:
            pass
    try:
        path.rmdir()
    except Exception:
        pass


def _acquire_lock(volume_root: Path) -> Optional[Path]:
    lock_dir = Path(os.getenv("PITCHAI_ELISE_LOCK_DIR", str(volume_root / "locks" / "elise_agent.lock")))
    wait_s = int(os.getenv("PITCHAI_ELISE_LOCK_WAIT_S", "60"))
    stale_after_s = int(os.getenv("PITCHAI_ELISE_LOCK_STALE_AFTER_S", "3600"))

    lock_dir.parent.mkdir(parents=True, exist_ok=True)
    deadline = time.time() + max(0, wait_s)

    while True:
        try:
            lock_dir.mkdir(parents=False, exist_ok=False)
            meta = {"ts_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "pid": os.getpid(), "host": os.uname().nodename}
            try:
                (lock_dir / "meta.json").write_text(json.dumps(meta, indent=2) + "\n", encoding="utf-8")
            except Exception:
                pass
            return lock_dir
        except FileExistsError:
            try:
                age_s = time.time() - lock_dir.stat().st_mtime
            except Exception:
                age_s = 0
            if age_s > stale_after_s:
                print(f"[lock] Removing stale lock at {lock_dir} (age_s={int(age_s)})", file=sys.stderr)
                _safe_remove_dir(lock_dir)
                continue

            if time.time() >= deadline:
                print(f"[lock] Could not acquire lock within {wait_s}s; exiting (lock={lock_dir})", file=sys.stderr)
                return None
            time.sleep(2)


def _release_lock(lock_dir: Optional[Path]) -> None:
    if lock_dir is None:
        return
    _safe_remove_dir(lock_dir)


def _pick_prompt_from_queue(cfg: CodexRunConfig) -> tuple[Path, Optional[Path]]:
    override = os.getenv("PITCHAI_PROMPT_OVERRIDE", "").strip()
    if override:
        tmp = cfg.volume_root / "prompt_override.md"
        tmp.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(override + "\n", encoding="utf-8")
        return (tmp, None)

    prompt_dir = cfg.prompt_queue_dir
    processing_dir = prompt_dir / "_processing"
    processed_dir = prompt_dir / "_processed"
    failed_dir = prompt_dir / "_failed"
    for d in (prompt_dir, processing_dir, processed_dir, failed_dir):
        d.mkdir(parents=True, exist_ok=True)

    candidates = sorted([p for p in prompt_dir.iterdir() if p.is_file() and p.suffix.lower() in (".md", ".txt")])
    if not candidates:
        return (cfg.prompt_path, None)

    selected = candidates[0]
    processing_path = processing_dir / selected.name
    try:
        selected.rename(processing_path)
        selected = processing_path
    except Exception as exc:
        print(f"[prompt] Failed moving {selected} -> {processing_path}: {exc}", file=sys.stderr)
        return (cfg.prompt_path, None)

    try:
        print(f"[prompt] Using queued prompt: {selected}", file=sys.stderr)
        selected.read_bytes()
        return (selected, selected)
    except Exception as exc:
        print(f"[prompt] Failed reading queued prompt {selected}: {exc}", file=sys.stderr)
        try:
            selected.rename(failed_dir / selected.name)
        except Exception:
            pass
        return (cfg.prompt_path, None)


def _finalize_prompt(prompt_in_processing: Optional[Path], *, rc: int, prompt_queue_dir: Path) -> None:
    if prompt_in_processing is None:
        return
    processing_dir = prompt_queue_dir / "_processing"
    processed_dir = prompt_queue_dir / "_processed"
    failed_dir = prompt_queue_dir / "_failed"
    target_dir = processed_dir if rc == 0 else failed_dir
    target_dir.mkdir(parents=True, exist_ok=True)
    try:
        prompt_in_processing.rename(target_dir / prompt_in_processing.name)
    except Exception:
        pass


def _resolve_config() -> CodexRunConfig:
    volume_root = Path(os.getenv("PITCHAI_ELISE_VOLUME", "/mnt/elise"))
    workdir = Path(os.getenv("PITCHAI_ELISE_WORKDIR", str(volume_root / "elise")))
    codex_home = Path(os.getenv("CODEX_HOME", str(volume_root / "codex_home")))
    state_path = Path(os.getenv("PITCHAI_ELISE_STATE_PATH", str(volume_root / "state.json")))

    prompt_path = Path(os.getenv("PITCHAI_PROMPT_PATH", "/opt/pitchai/elise_prompt.md"))
    prompt_queue_dir = Path(os.getenv("PITCHAI_PROMPT_QUEUE_DIR", str(volume_root / "prompts" / "telegram")))

    config_path = Path(os.getenv("PITCHAI_CODEX_CONFIG_PATH", "/opt/pitchai/elise_config.toml"))
    return CodexRunConfig(
        volume_root=volume_root,
        codex_home=codex_home,
        workdir=workdir,
        state_path=state_path,
        prompt_path=prompt_path,
        config_path=config_path,
        prompt_queue_dir=prompt_queue_dir,
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
    lock_dir = _acquire_lock(cfg.volume_root)
    if lock_dir is None:
        return 0

    prompt_in_processing: Optional[Path] = None
    try:
        selected_prompt, prompt_in_processing = _pick_prompt_from_queue(cfg)
        cfg = CodexRunConfig(
            volume_root=cfg.volume_root,
            codex_home=cfg.codex_home,
            workdir=cfg.workdir,
            state_path=cfg.state_path,
            prompt_path=selected_prompt,
            config_path=cfg.config_path,
            prompt_queue_dir=cfg.prompt_queue_dir,
        )

        state = _read_state(cfg.state_path)
        thread_id = state.get("thread_id") if isinstance(state, dict) else None
        if not isinstance(thread_id, str) or not thread_id.strip():
            thread_id = None

        rc = _spawn_codex(cfg, thread_id=thread_id)
        _finalize_prompt(prompt_in_processing, rc=rc, prompt_queue_dir=cfg.prompt_queue_dir)
        return rc
    finally:
        _release_lock(lock_dir)


if __name__ == "__main__":
    raise SystemExit(main())
