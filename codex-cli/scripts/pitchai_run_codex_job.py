#!/usr/bin/env python3
"""
PitchAI generic Codex runner.

Authentication policy:
- Preferred/default: auth-token-server broker mode (`CODEX_AUTH_BROKER_URL` +
  `CODEX_AUTH_BROKER_TOKEN`) which leases an `auth.json` and writes it to
  `$CODEX_HOME/auth.json`.
- Legacy/non-broker mode: `CODEX_AUTH_JSON_B64` can still seed `auth.json`.
- This runner never injects `CODEX_API_KEY` into the child process.

Usage-limit recovery in broker mode:
- On rate/usage limit outcomes (`usage_limit_reached`, `insufficient_quota`,
  `429`), the runner reports the lease outcome to the broker, acquires a fresh
  lease (new auth payload), rewrites `auth.json`, and auto-continues the same
  conversation in-process.
- Auto-continue knobs:
  - `PITCHAI_BROKER_USAGE_LIMIT_AUTO_CONTINUE_MAX` (default: 8)
  - `PITCHAI_BROKER_USAGE_LIMIT_BACKOFF_INITIAL_S` (default: 5)
  - `PITCHAI_BROKER_USAGE_LIMIT_BACKOFF_MAX_S` (default: 120)
  - `PITCHAI_BROKER_AUTO_CONTINUE_PROMPT` (default: "continue")

Cyber-safety reroute recovery:
- When `codex exec --json` reports the high-risk cyber reroute warning
  (`chatgpt.com/cyber` / "high-risk cyber activity"), the runner treats that
  turn as incomplete and replays the original prompt instead of accepting the
  downgraded result.
- Retry knobs:
  - `PITCHAI_CYBER_RETRY_MAX` (default: 200)
  - `PITCHAI_CYBER_RETRY_BACKOFF_INITIAL_S` (default: 2)
  - `PITCHAI_CYBER_RETRY_BACKOFF_MAX_S` (default: 30)
"""
from __future__ import annotations

import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional
from urllib import error as urllib_error
from urllib import request as urllib_request


@dataclass
class CodexRunConfig:
    volume_root: Path
    codex_home: Path
    workdir: Path
    state_path: Path
    prompt_path: Path
    config_path: Path
    prompt_queue_dir: Path


@dataclass(frozen=True)
class QueuedWorkItem:
    prompt_path: Path
    config_path: Path
    workdir: Path
    state_key: Optional[str]
    model: Optional[str]
    conversation_id: Optional[str]
    fork: bool
    pre_commands: list[str]
    post_commands: list[str]
    git_repo: Optional[str]
    git_branch: Optional[str]
    git_base: Optional[str]
    git_clone_dir_rel: Optional[str]
    git_prepared: bool
    queue_processing_path: Path


@dataclass(frozen=True)
class BrokerLease:
    lease_id: str
    account_id: str


def _require_env(name: str) -> str:
    value = os.getenv(name)
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def _optional_env(name: str) -> Optional[str]:
    value = os.getenv(name)
    return value if value else None


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


def _write_auth_file(auth_path: Path, auth_bytes: bytes) -> None:
    auth_path.write_bytes(auth_bytes)
    try:
        os.chmod(auth_path, 0o600)
    except PermissionError:
        # Azure Files mounts do not always support chmod (CIFS), but Codex can
        # still read the credentials file.
        pass


def _decode_auth_json(config_home: Path) -> BrokerLease | None:
    config_home.mkdir(parents=True, exist_ok=True)
    auth_path = config_home / "auth.json"
    broker_cfg = _broker_config()
    if broker_cfg is not None:
        lease = _acquire_broker_lease(broker_cfg)
        _write_auth_file(auth_path, lease["auth_bytes"])
        return BrokerLease(lease_id=lease["lease_id"], account_id=lease["account_id"])

    b64 = os.getenv("CODEX_AUTH_JSON_B64", "").strip()
    if not b64:
        if auth_path.exists():
            return None
        raise RuntimeError("Missing required environment variable: CODEX_AUTH_JSON_B64")

    _write_auth_file(auth_path, base64.b64decode(b64.encode("utf-8")))
    return None


def _model_args() -> tuple[list[str], list[str]]:
    model = (os.getenv("PITCHAI_CODEX_MODEL_OVERRIDE") or os.getenv("PITCHAI_CODEX_MODEL", "")).strip()
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


def _acquire_lock(volume_root: Path, *, key: str) -> Optional[Path]:
    lock_dir = Path(os.getenv("PITCHAI_CODEX_LOCK_DIR", str(volume_root / "locks" / f"{key}.lock")))
    wait_s = int(os.getenv("PITCHAI_CODEX_LOCK_WAIT_S", "60"))
    stale_after_s = int(os.getenv("PITCHAI_CODEX_LOCK_STALE_AFTER_S", "3600"))

    lock_dir.parent.mkdir(parents=True, exist_ok=True)
    deadline = time.time() + max(0, wait_s)

    while True:
        try:
            lock_dir.mkdir(parents=False, exist_ok=False)
            meta = {
                "ts_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "pid": os.getpid(),
                "host": os.uname().nodename,
                "key": key,
            }
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


def _sanitize_key(value: str) -> str:
    value = value.strip()
    value = "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in value)
    return value[:80] if value else "default"


def _broker_config() -> dict[str, str] | None:
    url = (os.getenv("CODEX_AUTH_BROKER_URL") or "").strip()
    token = (os.getenv("CODEX_AUTH_BROKER_TOKEN") or "").strip()
    if not url and not token:
        return None
    if not url or not token:
        raise RuntimeError("Both CODEX_AUTH_BROKER_URL and CODEX_AUTH_BROKER_TOKEN are required")

    timeout_raw = (os.getenv("CODEX_AUTH_BROKER_TIMEOUT_S") or "15").strip()
    timeout_s = str(max(1.0, min(float(timeout_raw or "15"), 120.0)))
    client_name = (os.getenv("CODEX_AUTH_BROKER_CLIENT_NAME") or "").strip() or "pitchai-dispatch-runner"
    affinity_key = (os.getenv("PITCHAI_STATE_KEY") or "").strip() or "default"
    return {
        "url": url.rstrip("/"),
        "token": token,
        "timeout_s": timeout_s,
        "client_name": client_name,
        "affinity_key": affinity_key,
    }


def _http_json(method: str, url: str, *, token: str, payload: dict[str, Any] | None, timeout_s: float) -> dict[str, Any]:
    data = None
    headers = {"Authorization": f"Bearer {token}"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib_request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib_request.urlopen(req, timeout=timeout_s) as resp:  # noqa: S310 - explicit operator config
            raw = resp.read().decode("utf-8")
    except urllib_error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Broker request failed {method} {url}: {exc.code}: {body}") from exc
    except urllib_error.URLError as exc:
        raise RuntimeError(f"Broker request failed {method} {url}: {exc}") from exc
    obj = json.loads(raw or "{}")
    if not isinstance(obj, dict):
        raise RuntimeError(f"Broker response from {url} is not a JSON object")
    return obj


def _acquire_broker_lease(cfg: dict[str, str]) -> dict[str, Any]:
    payload = _http_json(
        "POST",
        f"{cfg['url']}/v1/leases",
        token=cfg["token"],
        timeout_s=float(cfg["timeout_s"]),
        payload={
            "client_name": cfg["client_name"],
            "affinity_key": cfg["affinity_key"],
            "lease_reason": "pitchai-dispatch-runner",
        },
    )
    auth_json_b64 = str(payload.get("auth_json_b64") or "").strip()
    if not auth_json_b64:
        raise RuntimeError("Broker lease response missing auth_json_b64")
    return {
        "lease_id": str(payload["lease_id"]),
        "account_id": str(payload["account_id"]),
        "auth_bytes": base64.b64decode(auth_json_b64.encode("utf-8")),
    }


def _report_broker_lease(
    cfg: dict[str, str],
    lease: BrokerLease,
    *,
    outcome: str,
    updated_auth_bytes: bytes | None,
    detail: str | None,
) -> None:
    payload: dict[str, Any] = {"outcome": outcome, "detail": detail}
    if updated_auth_bytes is not None:
        payload["updated_auth_json_b64"] = base64.b64encode(updated_auth_bytes).decode("ascii")
    _http_json(
        "POST",
        f"{cfg['url']}/v1/leases/{lease.lease_id}/report",
        token=cfg["token"],
        timeout_s=float(cfg["timeout_s"]),
        payload=payload,
    )


def _materialized_auth_bytes(config_home: Path) -> bytes | None:
    auth_path = config_home / "auth.json"
    if auth_path.exists():
        return auth_path.read_bytes()
    return None


def _refresh_broker_lease(cfg: CodexRunConfig, broker_cfg: dict[str, str]) -> BrokerLease:
    lease = _acquire_broker_lease(broker_cfg)
    auth_path = cfg.codex_home / "auth.json"
    _write_auth_file(auth_path, lease["auth_bytes"])
    return BrokerLease(lease_id=lease["lease_id"], account_id=lease["account_id"])


def _run_codex_json_stream(
    cmd: list[str],
    *,
    child_env: dict[str, str],
    prompt_path: Optional[Path],
    inline_prompt: Optional[str],
) -> tuple[int, list[str], Optional[str]]:
    output_lines: list[str] = []
    captured_thread_id: Optional[str] = None
    proc: Optional[subprocess.Popen[str]] = None
    prompt_fh = None
    try:
        if prompt_path is not None:
            prompt_fh = prompt_path.open("rb")
            proc = subprocess.Popen(
                cmd + ["-"],
                stdin=prompt_fh,
                stdout=subprocess.PIPE,
                stderr=sys.stderr,
                env=child_env,
                text=True,
                bufsize=1,
            )
        else:
            proc = subprocess.Popen(
                cmd + ["-"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=sys.stderr,
                env=child_env,
                text=True,
                bufsize=1,
            )
            assert proc.stdin is not None
            proc.stdin.write((inline_prompt or "").rstrip("\n") + "\n")
            proc.stdin.close()

        assert proc.stdout is not None
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            if len(output_lines) < 400:
                output_lines.append(line.rstrip("\n"))
            try:
                evt = json.loads(line)
            except Exception:
                continue
            if isinstance(evt, dict) and evt.get("type") == "thread.started":
                tid = evt.get("thread_id")
                if isinstance(tid, str) and tid.strip():
                    captured_thread_id = tid.strip()
        rc = int(proc.wait())
    finally:
        if prompt_fh is not None:
            prompt_fh.close()
    return rc, output_lines, captured_thread_id


def _classify_broker_outcome(lines: list[str], rc: int) -> tuple[str, str | None]:
    combined = "\n".join(lines).strip()
    combined_lower = combined.lower()
    if (
        "usage_limit_reached" in combined
        or "insufficient_quota" in combined
        or "429 too many requests" in combined_lower
        or "you've hit your usage limit" in combined_lower
        or "the usage limit has been reached" in combined_lower
        or "chatgpt.com/codex/settings/usage" in combined_lower
    ):
        return ("usage_limit_reached", combined[:4000] or None)
    if (
        "chatgpt.com/cyber" in combined_lower
        or "high-risk cyber activity" in combined_lower
        or "high_risk_cyber_activity" in combined_lower
        or "highriskcyberactivity" in combined_lower
        or "codex/concepts/cyber-safety" in combined_lower
    ):
        return ("cyber_safety_reroute", combined[:4000] or None)
    if (
        "refresh_token_expired" in combined
        or "refresh_token_reused" in combined
        or "refresh_token_invalidated" in combined
        or "Please log out and sign in again" in combined
    ):
        return ("auth_invalid", combined[:4000] or None)
    if rc == 0:
        return ("success", None)
    return ("runner_error", combined[:4000] or None)


def _load_meta(path: Path) -> dict[str, Any]:
    try:
        if not path.exists():
            return {}
        data = json.loads(path.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else {}
    except Exception:
        return {}

def _sanitize_commands(value: Any) -> list[str]:
    if not isinstance(value, list):
        return []
    out: list[str] = []
    for item in value:
        if not isinstance(item, str):
            continue
        cmd = item.strip()
        if not cmd:
            continue
        # prevent huge payloads / accidental blobs
        if len(cmd) > 4000:
            cmd = cmd[:4000]
        out.append(cmd)
        if len(out) >= 25:
            break
    return out


def _resolve_workdir(cfg: CodexRunConfig, *, meta: dict[str, Any]) -> Path:
    workdir_rel = meta.get("workdir_rel")
    if not isinstance(workdir_rel, str) or not workdir_rel.strip():
        return cfg.workdir
    rel = Path(workdir_rel.strip())
    if rel.is_absolute() or ".." in rel.parts:
        return cfg.workdir
    return cfg.volume_root / rel


def _pick_prompt_from_queue(cfg: CodexRunConfig) -> tuple[Path, Optional[Path], Optional[QueuedWorkItem]]:
    override = os.getenv("PITCHAI_PROMPT_OVERRIDE", "").strip()
    if override:
        tmp = cfg.volume_root / "prompt_override.md"
        tmp.parent.mkdir(parents=True, exist_ok=True)
        tmp.write_text(override + "\n", encoding="utf-8")
        return (tmp, None, None)

    prompt_dir = cfg.prompt_queue_dir
    processing_dir = prompt_dir / "_processing"
    processed_dir = prompt_dir / "_processed"
    failed_dir = prompt_dir / "_failed"
    for d in (prompt_dir, processing_dir, processed_dir, failed_dir):
        d.mkdir(parents=True, exist_ok=True)

    file_candidates = sorted([p for p in prompt_dir.iterdir() if p.is_file() and p.suffix.lower() in (".md", ".txt")])
    dir_candidates = sorted(
        [p for p in prompt_dir.iterdir() if p.is_dir() and not p.name.startswith("_") and (p / "prompt.md").is_file()]
    )
    if not file_candidates and not dir_candidates:
        return (cfg.prompt_path, None, None)

    # Prefer directory bundles (prompt+config+meta) over plain prompt files.
    if dir_candidates:
        selected_dir = dir_candidates[0]
        processing_path = processing_dir / selected_dir.name
        try:
            selected_dir.rename(processing_path)
        except Exception as exc:
            print(f"[prompt] Failed moving {selected_dir} -> {processing_path}: {exc}", file=sys.stderr)
            return (cfg.prompt_path, None, None)

        prompt_path = processing_path / "prompt.md"
        config_path = (processing_path / "config.toml") if (processing_path / "config.toml").exists() else cfg.config_path
        meta = _load_meta(processing_path / "meta.json")
        workdir = _resolve_workdir(cfg, meta=meta)
        state_key = meta.get("state_key")
        model = meta.get("model")
        conversation_id = meta.get("conversation_id")
        fork = meta.get("fork", False)
        pre_commands = _sanitize_commands(meta.get("pre_commands"))
        post_commands = _sanitize_commands(meta.get("post_commands"))
        git_repo = meta.get("git_repo")
        git_branch = meta.get("git_branch")
        git_base = meta.get("git_base")
        git_clone_dir_rel = meta.get("git_clone_dir_rel")
        git_prepared = meta.get("git_prepared")
        item = QueuedWorkItem(
            prompt_path=prompt_path,
            config_path=config_path,
            workdir=workdir,
            state_key=_sanitize_key(state_key) if isinstance(state_key, str) and state_key.strip() else None,
            model=str(model).strip() if isinstance(model, str) and model.strip() else None,
            conversation_id=str(conversation_id).strip()
            if isinstance(conversation_id, str) and conversation_id.strip()
            else None,
            fork=bool(fork) if isinstance(fork, bool) else False,
            pre_commands=pre_commands,
            post_commands=post_commands,
            git_repo=str(git_repo).strip() if isinstance(git_repo, str) and git_repo.strip() else None,
            git_branch=str(git_branch).strip() if isinstance(git_branch, str) and git_branch.strip() else None,
            git_base=str(git_base).strip() if isinstance(git_base, str) and git_base.strip() else None,
            git_clone_dir_rel=str(git_clone_dir_rel).strip()
            if isinstance(git_clone_dir_rel, str) and git_clone_dir_rel.strip()
            else None,
            git_prepared=bool(git_prepared) if isinstance(git_prepared, bool) else False,
            queue_processing_path=processing_path,
        )
        print(f"[prompt] Using queued bundle: {processing_path}", file=sys.stderr)
        return (prompt_path, None, item)

    selected = file_candidates[0]
    processing_path = processing_dir / selected.name
    try:
        selected.rename(processing_path)
        selected = processing_path
    except Exception as exc:
        print(f"[prompt] Failed moving {selected} -> {processing_path}: {exc}", file=sys.stderr)
        return (cfg.prompt_path, None, None)

    try:
        print(f"[prompt] Using queued prompt: {selected}", file=sys.stderr)
        selected.read_bytes()
        item = QueuedWorkItem(
            prompt_path=selected,
            config_path=cfg.config_path,
            workdir=cfg.workdir,
            state_key=None,
            model=None,
            conversation_id=None,
            fork=False,
            pre_commands=[],
            post_commands=[],
            git_repo=None,
            git_branch=None,
            git_base=None,
            git_clone_dir_rel=None,
            git_prepared=False,
            queue_processing_path=selected,
        )
        return (selected, selected, item)
    except Exception as exc:
        print(f"[prompt] Failed reading queued prompt {selected}: {exc}", file=sys.stderr)
        try:
            selected.rename(failed_dir / selected.name)
        except Exception:
            pass
        return (cfg.prompt_path, None, None)


def _finalize_work_item(work_item: Optional[QueuedWorkItem], *, rc: int, prompt_queue_dir: Path) -> None:
    if work_item is None:
        return
    processed_dir = prompt_queue_dir / "_processed"
    failed_dir = prompt_queue_dir / "_failed"
    target_dir = processed_dir if rc == 0 else failed_dir
    target_dir.mkdir(parents=True, exist_ok=True)
    try:
        src = work_item.queue_processing_path
        src.rename(target_dir / src.name)
    except Exception:
        return


def _state_key_for_config(config_path: Path) -> str:
    explicit = os.getenv("PITCHAI_STATE_KEY", "").strip()
    if explicit:
        return explicit
    raw = f"config:{config_path}"
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()[:12]


def _resolve_config() -> CodexRunConfig:
    volume_root = Path(os.getenv("PITCHAI_VOLUME_ROOT", "/mnt/elise"))
    workdir = Path(os.getenv("PITCHAI_WORKDIR", str(volume_root / "workdir")))
    codex_home = Path(os.getenv("CODEX_HOME", str(volume_root / "codex_home")))

    config_path = Path(os.getenv("PITCHAI_CODEX_CONFIG_PATH", "/opt/pitchai/config.toml"))
    prompt_path = Path(os.getenv("PITCHAI_PROMPT_PATH", "/opt/pitchai/prompt.md"))
    prompt_queue_dir = Path(os.getenv("PITCHAI_PROMPT_QUEUE_DIR", str(volume_root / "prompts" / "queue")))

    state_dir = Path(os.getenv("PITCHAI_STATE_DIR", str(volume_root)))
    state_key = _state_key_for_config(config_path)
    state_path = Path(os.getenv("PITCHAI_STATE_PATH", str(state_dir / f"state_{state_key}.json")))

    return CodexRunConfig(
        volume_root=volume_root,
        codex_home=codex_home,
        workdir=workdir,
        state_path=state_path,
        prompt_path=prompt_path,
        config_path=config_path,
        prompt_queue_dir=prompt_queue_dir,
    )


def _spawn_codex(
    cfg: CodexRunConfig,
    *,
    resume_id: Optional[str],
    fork: bool,
    persist_thread_id: bool,
) -> int:
    """
    Run `codex exec` and, in broker mode, auto-continue after usage-limit errors.

    Behavior:
    - First attempt uses the queued/file prompt.
    - If usage/rate limit is detected and broker auth is configured, the runner:
      1) reports outcome for the active lease,
      2) acquires a new lease (`/v1/leases`),
      3) rewrites `$CODEX_HOME/auth.json`,
      4) retries by resuming the same thread with a short "continue" prompt.
    - If the backend reroutes the request because of the high-risk cyber safety
      gate, the runner retries the original prompt instead of accepting the
      downgraded result.
    - Non-usage failures are returned immediately to preserve fail-fast behavior.
    """
    cfg.codex_home.mkdir(parents=True, exist_ok=True)
    cfg.workdir.mkdir(parents=True, exist_ok=True)
    broker_cfg = _broker_config()
    broker_lease = _decode_auth_json(cfg.codex_home)

    try:
        max_usage_auto_continue = int(os.getenv("PITCHAI_BROKER_USAGE_LIMIT_AUTO_CONTINUE_MAX", "8") or "8")
    except ValueError:
        max_usage_auto_continue = 8
    max_usage_auto_continue = max(0, min(max_usage_auto_continue, 64))

    try:
        usage_backoff_initial_s = float(os.getenv("PITCHAI_BROKER_USAGE_LIMIT_BACKOFF_INITIAL_S", "5") or "5")
    except ValueError:
        usage_backoff_initial_s = 5.0
    usage_backoff_initial_s = max(0.0, min(usage_backoff_initial_s, 600.0))

    try:
        usage_backoff_max_s = float(os.getenv("PITCHAI_BROKER_USAGE_LIMIT_BACKOFF_MAX_S", "120") or "120")
    except ValueError:
        usage_backoff_max_s = 120.0
    usage_backoff_max_s = max(usage_backoff_initial_s, min(usage_backoff_max_s, 7200.0))

    try:
        max_cyber_retries = int(os.getenv("PITCHAI_CYBER_RETRY_MAX", "200") or "200")
    except ValueError:
        max_cyber_retries = 200
    max_cyber_retries = max(0, min(max_cyber_retries, 1000))

    try:
        cyber_backoff_initial_s = float(os.getenv("PITCHAI_CYBER_RETRY_BACKOFF_INITIAL_S", "2") or "2")
    except ValueError:
        cyber_backoff_initial_s = 2.0
    cyber_backoff_initial_s = max(0.0, min(cyber_backoff_initial_s, 300.0))

    try:
        cyber_backoff_max_s = float(os.getenv("PITCHAI_CYBER_RETRY_BACKOFF_MAX_S", "30") or "30")
    except ValueError:
        cyber_backoff_max_s = 30.0
    cyber_backoff_max_s = max(cyber_backoff_initial_s, min(cyber_backoff_max_s, 1800.0))

    continue_prompt = (os.getenv("PITCHAI_BROKER_AUTO_CONTINUE_PROMPT") or "").strip() or "continue"

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
    child_env = dict(os.environ)
    child_env.pop("CODEX_API_KEY", None)

    captured_thread_id: Optional[str] = None
    resume_for_attempt = resume_id
    usage_continue_attempts = 0
    cyber_retry_attempts = 0
    final_rc = 1
    fork_applied = False
    attempt_number = 0
    replay_original_prompt = False
    while True:
        attempt_number += 1
        cmd = list(base_cmd)
        if resume_for_attempt:
            cmd.extend(["resume", resume_for_attempt])
            if fork and not fork_applied:
                cmd.append("--fork")
                fork_applied = True

        prompt_path = (
            cfg.prompt_path
            if (attempt_number == 1 or replay_original_prompt or not resume_for_attempt)
            else None
        )
        inline_prompt = None if prompt_path is not None else continue_prompt
        replay_original_prompt = False
        rc, output_lines, attempt_thread_id = _run_codex_json_stream(
            cmd,
            child_env=child_env,
            prompt_path=prompt_path,
            inline_prompt=inline_prompt,
        )
        final_rc = rc

        if attempt_thread_id:
            captured_thread_id = attempt_thread_id
            resume_for_attempt = attempt_thread_id

        outcome, detail = _classify_broker_outcome(output_lines, rc)
        if broker_cfg is not None and broker_lease is not None:
            try:
                _report_broker_lease(
                    broker_cfg,
                    broker_lease,
                    outcome="runner_error" if outcome == "cyber_safety_reroute" else outcome,
                    updated_auth_bytes=_materialized_auth_bytes(cfg.codex_home),
                    detail=detail,
                )
            except Exception as exc:
                print(
                    f"[broker] failed to report lease {broker_lease.lease_id}: {exc}",
                    file=sys.stderr,
                    flush=True,
                )

        if outcome == "cyber_safety_reroute":
            if cyber_retry_attempts >= max_cyber_retries:
                print(
                    f"[runner] high-risk cyber reroute repeated; retry budget exhausted ({cyber_retry_attempts}/{max_cyber_retries})",
                    file=sys.stderr,
                    flush=True,
                )
                final_rc = rc if rc != 0 else 1
                break

            cyber_retry_attempts += 1
            replay_original_prompt = True
            if resume_for_attempt:
                print(
                    f"[runner] high-risk cyber reroute detected; replaying original prompt on {resume_for_attempt} ({cyber_retry_attempts}/{max_cyber_retries})",
                    file=sys.stderr,
                    flush=True,
                )
            else:
                print(
                    f"[runner] high-risk cyber reroute detected before thread id was observed; retrying original prompt ({cyber_retry_attempts}/{max_cyber_retries})",
                    file=sys.stderr,
                    flush=True,
                )

            delay_s = min(cyber_backoff_max_s, cyber_backoff_initial_s * (2 ** (cyber_retry_attempts - 1)))
            if delay_s > 0:
                print(
                    f"[runner] sleeping {delay_s:.1f}s before cyber reroute retry",
                    file=sys.stderr,
                    flush=True,
                )
                time.sleep(delay_s)
            continue

        if rc == 0:
            break

        if outcome != "usage_limit_reached" or broker_cfg is None:
            break

        if usage_continue_attempts >= max_usage_auto_continue:
            print(
                f"[broker] usage limit reached again; auto-continue budget exhausted ({usage_continue_attempts}/{max_usage_auto_continue})",
                file=sys.stderr,
                flush=True,
            )
            break

        try:
            broker_lease = _refresh_broker_lease(cfg, broker_cfg)
        except Exception as exc:
            print(
                f"[broker] failed to refresh lease after usage limit: {exc}",
                file=sys.stderr,
                flush=True,
            )
            break

        usage_continue_attempts += 1
        if not resume_for_attempt and resume_id:
            resume_for_attempt = resume_id

        if resume_for_attempt:
            print(
                f"[broker] usage limit reached; swapped auth lease and auto-continuing on {resume_for_attempt} ({usage_continue_attempts}/{max_usage_auto_continue})",
                file=sys.stderr,
                flush=True,
            )
        else:
            print(
                f"[broker] usage limit reached before thread id was observed; swapped auth lease and retrying original prompt ({usage_continue_attempts}/{max_usage_auto_continue})",
                file=sys.stderr,
                flush=True,
            )

        delay_s = min(usage_backoff_max_s, usage_backoff_initial_s * (2 ** (usage_continue_attempts - 1)))
        if delay_s > 0:
            print(
                f"[broker] sleeping {delay_s:.1f}s before auto-continue",
                file=sys.stderr,
                flush=True,
            )
            time.sleep(delay_s)

    if captured_thread_id and persist_thread_id:
        state = _read_state(cfg.state_path)
        if state.get("thread_id") != captured_thread_id:
            state["thread_id"] = captured_thread_id
            _write_state(cfg.state_path, state)

    return int(final_rc)

def _run_hook_commands(
    commands: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    label: str,
) -> int:
    if not commands:
        return 0
    rc = 0
    for i, cmd in enumerate(commands, start=1):
        print(f"[hook {label}] ({i}/{len(commands)}) {cmd}", file=sys.stderr, flush=True)
        try:
            proc = subprocess.run(
                ["sh", "-lc", cmd],
                cwd=str(cwd),
                env=env,
                text=True,
                stdout=sys.stderr,
                stderr=sys.stderr,
            )
            rc = int(proc.returncode)
        except Exception as exc:
            print(f"[hook {label}] failed: {exc}", file=sys.stderr, flush=True)
            return 1
        if rc != 0:
            print(f"[hook {label}] command failed rc={rc}", file=sys.stderr, flush=True)
            return rc
    return 0


def _sanitize_relpath(value: Optional[str]) -> Optional[Path]:
    if not value:
        return None
    s = value.strip()
    if not s:
        return None
    rel = Path(s)
    if rel.is_absolute() or ".." in rel.parts:
        return None
    return rel


def _run_git(
    args: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    label: str,
) -> None:
    print(f"[git {label}] {' '.join(args)}", file=sys.stderr, flush=True)
    subprocess.run(args, cwd=str(cwd), env=env, check=True, stdout=sys.stderr, stderr=sys.stderr, text=True)


def _prepare_git_repo(
    workdir: Path,
    *,
    repo_url: str,
    branch: str,
    base: str,
    clone_dir_rel: Optional[str],
) -> Path:
    """
    Clone/fetch `repo_url` into a subdir of `workdir`, then create/force-reset
    local branch `branch` from `origin/<base>`.

    Credentials:
    - If `PITCHAI_GIT_TOKEN` is set, use GIT_ASKPASS so the token is never placed
      into the command line arguments.
    """
    clone_rel = _sanitize_relpath(clone_dir_rel) or Path("repo")
    repo_dir = workdir / clone_rel
    repo_dir.parent.mkdir(parents=True, exist_ok=True)

    env = dict(os.environ)
    env["GIT_TERMINAL_PROMPT"] = "0"

    token = (os.getenv("PITCHAI_GIT_TOKEN") or "").strip()
    askpass_path = workdir / ".git-askpass.sh"
    if token:
        askpass_path.write_text(
            "#!/usr/bin/env sh\n"
            "case \"$1\" in\n"
            "  *Username*) echo \"x-access-token\" ;;\n"
            "  *Password*) echo \"$PITCHAI_GIT_TOKEN\" ;;\n"
            "  *) echo \"\" ;;\n"
            "esac\n",
            encoding="utf-8",
        )
        askpass_path.chmod(0o700)
        env["GIT_ASKPASS"] = str(askpass_path)
        env["PITCHAI_GIT_TOKEN"] = token

    if not (repo_dir / ".git").exists():
        if repo_dir.exists():
            # Clean non-git directory.
            _safe_remove_dir(repo_dir)
        repo_dir.parent.mkdir(parents=True, exist_ok=True)
        _run_git(["git", "clone", "--no-tags", repo_url, str(repo_dir)], cwd=repo_dir.parent, env=env, label="clone")
    else:
        # Verify origin matches, otherwise reclone.
        try:
            out = subprocess.check_output(["git", "remote", "get-url", "origin"], cwd=str(repo_dir), env=env, text=True)
            origin = out.strip()
        except Exception:
            origin = ""
        if origin and origin != repo_url:
            print(f"[git] origin mismatch; recloning into {repo_dir}", file=sys.stderr, flush=True)
            _safe_remove_dir(repo_dir)
            _run_git(["git", "clone", "--no-tags", repo_url, str(repo_dir)], cwd=repo_dir.parent, env=env, label="reclone")
        else:
            _run_git(["git", "fetch", "--prune", "origin"], cwd=repo_dir, env=env, label="fetch")

    # Determine base ref.
    base_ref = f"origin/{base}"
    try:
        subprocess.run(["git", "rev-parse", "--verify", base_ref], cwd=str(repo_dir), env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except Exception:
        base_ref = "origin/main"

    # Reset local branch from base.
    _run_git(["git", "checkout", "-B", branch, base_ref], cwd=repo_dir, env=env, label="checkout")
    return repo_dir


def main() -> int:
    cfg = _resolve_config()
    key = _state_key_for_config(cfg.config_path)
    lock_dir = _acquire_lock(cfg.volume_root, key=key)
    if lock_dir is None:
        return 0

    try:
        max_items = int(os.getenv("PITCHAI_MAX_ITEMS_PER_RUN", "1") or "1")
        max_items = max(1, min(max_items, 50))

        last_rc = 0
        for _ in range(max_items):
            work_item: Optional[QueuedWorkItem] = None
            selected_prompt, _, work_item = _pick_prompt_from_queue(cfg)
            if work_item is None:
                # No queued work; optionally run the default prompt once.
                if selected_prompt == cfg.prompt_path:
                    break

            try:
                if work_item is not None:
                    state_dir = Path(os.getenv("PITCHAI_STATE_DIR", str(cfg.volume_root)))
                    state_key = work_item.state_key or _state_key_for_config(work_item.config_path)
                    state_path = Path(os.getenv("PITCHAI_STATE_PATH", str(state_dir / f"state_{state_key}.json")))
                    bundle_name = work_item.queue_processing_path.name
                    os.environ["PITCHAI_DISPATCH_BUNDLE"] = bundle_name

                    # Default per-item workdir: isolate runs by queue bundle name.
                    if work_item.workdir == cfg.workdir:
                        run_root = cfg.volume_root / "workdir" / _sanitize_key(state_key) / _sanitize_key(bundle_name)
                        run_root.mkdir(parents=True, exist_ok=True)
                        workdir = run_root
                    else:
                        workdir = work_item.workdir

                    run_cfg = CodexRunConfig(
                        volume_root=cfg.volume_root,
                        codex_home=cfg.codex_home,
                        workdir=workdir,
                        state_path=state_path,
                        prompt_path=work_item.prompt_path,
                        config_path=work_item.config_path,
                        prompt_queue_dir=cfg.prompt_queue_dir,
                    )
                    if work_item.model:
                        os.environ["PITCHAI_CODEX_MODEL_OVERRIDE"] = work_item.model
                    else:
                        os.environ.pop("PITCHAI_CODEX_MODEL_OVERRIDE", None)
                else:
                    os.environ.pop("PITCHAI_DISPATCH_BUNDLE", None)
                    run_cfg = CodexRunConfig(
                        volume_root=cfg.volume_root,
                        codex_home=cfg.codex_home,
                        workdir=cfg.workdir,
                        state_path=cfg.state_path,
                        prompt_path=selected_prompt,
                        config_path=cfg.config_path,
                        prompt_queue_dir=cfg.prompt_queue_dir,
                    )
                    os.environ.pop("PITCHAI_CODEX_MODEL_OVERRIDE", None)

                state = _read_state(run_cfg.state_path)
                resume_id = state.get("thread_id") if isinstance(state, dict) else None
                if not isinstance(resume_id, str) or not resume_id.strip():
                    resume_id = None

                fork = False
                persist_thread_id = True
                pre_commands: list[str] = []
                post_commands: list[str] = []
                git_repo: Optional[str] = None
                git_branch: Optional[str] = None
                git_base: Optional[str] = None
                git_clone_dir_rel: Optional[str] = None
                git_prepared = False
                if work_item is not None and work_item.conversation_id:
                    resume_id = work_item.conversation_id
                if work_item is not None and work_item.fork:
                    fork = True
                    # Preserve the original `conversation_id` for future "fork again"
                    # runs by not persisting the newly created fork session id.
                    persist_thread_id = False
                if work_item is not None:
                    pre_commands = list(work_item.pre_commands or [])
                    post_commands = list(work_item.post_commands or [])
                    git_repo = work_item.git_repo
                    git_branch = work_item.git_branch
                    git_base = work_item.git_base
                    git_clone_dir_rel = work_item.git_clone_dir_rel
                    git_prepared = bool(work_item.git_prepared)

                hook_env = dict(os.environ)
                hook_env["PITCHAI_CODEX_PHASE"] = "pre"
                hook_env["PITCHAI_CODEX_WORKDIR"] = str(run_cfg.workdir)

                # Optional: clone repo + create branch before running Codex/prompt.
                if git_repo and git_branch and not git_prepared:
                    base = git_base or "main"
                    repo_dir = _prepare_git_repo(
                        run_cfg.workdir,
                        repo_url=git_repo,
                        branch=git_branch,
                        base=base,
                        clone_dir_rel=git_clone_dir_rel,
                    )
                    run_cfg = CodexRunConfig(
                        volume_root=run_cfg.volume_root,
                        codex_home=run_cfg.codex_home,
                        workdir=repo_dir,
                        state_path=run_cfg.state_path,
                        prompt_path=run_cfg.prompt_path,
                        config_path=run_cfg.config_path,
                        prompt_queue_dir=run_cfg.prompt_queue_dir,
                    )
                    hook_env["PITCHAI_CODEX_WORKDIR"] = str(run_cfg.workdir)
                elif git_prepared and not (run_cfg.workdir / ".git").exists():
                    print("[git] git_prepared=true but no .git found in workdir", file=sys.stderr, flush=True)
                    last_rc = 1
                    _finalize_work_item(work_item, rc=last_rc, prompt_queue_dir=cfg.prompt_queue_dir)
                    break

                pre_rc = _run_hook_commands(pre_commands, cwd=run_cfg.workdir, env=hook_env, label="pre")
                if pre_rc != 0:
                    last_rc = int(pre_rc)
                    _finalize_work_item(work_item, rc=last_rc, prompt_queue_dir=cfg.prompt_queue_dir)
                    # Best effort: run post even if pre failed.
                    hook_env["PITCHAI_CODEX_PHASE"] = "post"
                    hook_env["PITCHAI_CODEX_LAST_RC"] = str(last_rc)
                    _run_hook_commands(post_commands, cwd=run_cfg.workdir, env=hook_env, label="post")
                    break

                rc = _spawn_codex(run_cfg, resume_id=resume_id, fork=fork, persist_thread_id=persist_thread_id)
                last_rc = rc
                _finalize_work_item(work_item, rc=rc, prompt_queue_dir=cfg.prompt_queue_dir)

                hook_env["PITCHAI_CODEX_PHASE"] = "post"
                hook_env["PITCHAI_CODEX_LAST_RC"] = str(rc)
                _run_hook_commands(post_commands, cwd=run_cfg.workdir, env=hook_env, label="post")
                if rc != 0:
                    break
            except Exception as exc:
                # Ensure the queue does not get stuck in _processing on unexpected failures.
                print(f"[error] runner failed: {type(exc).__name__}: {exc}", file=sys.stderr, flush=True)
                last_rc = 1
                _finalize_work_item(work_item, rc=last_rc, prompt_queue_dir=cfg.prompt_queue_dir)
                break

        return int(last_rc)
    finally:
        _release_lock(lock_dir)


if __name__ == "__main__":
    raise SystemExit(main())
