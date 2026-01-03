#!/usr/bin/env python3
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
    if resume_id:
        base_cmd.extend(["resume", resume_id])
        if fork:
            base_cmd.append("--fork")

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

    if captured_thread_id and persist_thread_id:
        state = _read_state(cfg.state_path)
        if state.get("thread_id") != captured_thread_id:
            state["thread_id"] = captured_thread_id
            _write_state(cfg.state_path, state)

    return int(rc)

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

            if work_item is not None:
                state_dir = Path(os.getenv("PITCHAI_STATE_DIR", str(cfg.volume_root)))
                state_key = work_item.state_key or _state_key_for_config(work_item.config_path)
                state_path = Path(os.getenv("PITCHAI_STATE_PATH", str(state_dir / f"state_{state_key}.json")))

                # Default per-item workdir: isolate runs by queue bundle name.
                if work_item.workdir == cfg.workdir:
                    bundle_name = work_item.queue_processing_path.name
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

        return int(last_rc)
    finally:
        _release_lock(lock_dir)


if __name__ == "__main__":
    raise SystemExit(main())
