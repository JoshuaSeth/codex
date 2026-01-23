#!/usr/bin/env python3

from __future__ import annotations

import glob
import json
import os
import pty
import fcntl
import struct
import termios
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


def _repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _copy_if_exists(src: Path, dst: Path) -> None:
    if not src.exists():
        return
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(src, dst)


def _read_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def _wait_for_live_file(codex_home: Path, timeout_s: float) -> Path | None:
    live_dir = codex_home / "live"
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        matches = glob.glob(str(live_dir / "*.json"))
        if matches:
            return Path(sorted(matches)[0])
        time.sleep(0.05)
    return None


def _wait_for_status(live_file: Path, expected: str, timeout_s: float) -> dict:
    deadline = time.time() + timeout_s
    last: dict | None = None
    while time.time() < deadline:
        try:
            last = _read_json(live_file)
        except Exception:
            time.sleep(0.05)
            continue
        if last.get("status") == expected:
            return last
        time.sleep(0.1)
    raise RuntimeError(f"timed out waiting for status={expected!r}; last={last!r}")


def _send_line(master_fd: int, line: str) -> None:
    os.write(master_fd, line.encode("utf-8") + b"\r")

def _type_text(master_fd: int, lock: threading.Lock, text: str, delay_s: float = 0.02) -> None:
    for ch in text:
        with lock:
            os.write(master_fd, ch.encode("utf-8"))
        time.sleep(delay_s)


def _press_enter(master_fd: int, lock: threading.Lock) -> None:
    with lock:
        os.write(master_fd, b"\r")


def _kill_process_group(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except Exception:
        proc.kill()


def _run_one(frontend: str, feature_flags: list[str]) -> None:
    root = _repo_root()
    codex_bin = root / "codex-rs" / "target" / "debug" / "codex"
    if not codex_bin.exists():
        raise RuntimeError(f"missing Codex binary at {codex_bin}")

    tmp = Path(tempfile.mkdtemp(prefix=f"codex_live_status_{frontend}_"))
    codex_home = tmp / "codex_home"
    codex_home.mkdir(parents=True, exist_ok=True)
    (tmp / "work").mkdir(parents=True, exist_ok=True)

    _copy_if_exists(Path.home() / ".codex" / "auth.json", codex_home / "auth.json")
    _copy_if_exists(Path.home() / ".codex" / "config.toml", codex_home / "config.toml")

    if not (codex_home / "auth.json").exists():
        raise RuntimeError("missing ~/.codex/auth.json; cannot run real-model TUI smoke")

    env = os.environ.copy()
    env["CODEX_HOME"] = str(codex_home)
    env["CODEX_UNSAFE_ALLOW_NO_SANDBOX"] = "1"
    env["CODEX_LIVE_DEVICE_ID"] = f"smoke-{frontend}"

    master_fd, slave_fd = pty.openpty()
    write_lock = threading.Lock()
    log_path = tmp / f"{frontend}.pty.log"

    fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))

    proc = subprocess.Popen(
        [
            str(codex_bin),
            "--no-alt-screen",
            "--dangerously-bypass-approvals-and-sandbox",
            "--cd",
            str(root),
            *feature_flags,
        ],
        cwd=str(tmp / "work"),
        env=env,
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=slave_fd,
        start_new_session=True,
        close_fds=True,
        text=False,
    )
    os.close(slave_fd)

    stop_reader = threading.Event()

    def reader() -> None:
        with log_path.open("wb") as log:
            while not stop_reader.is_set():
                r, _, _ = select.select([master_fd], [], [], 0.2)
                if master_fd not in r:
                    if proc.poll() is not None:
                        break
                    continue
                try:
                    data = os.read(master_fd, 4096)
                except OSError:
                    break
                if not data:
                    break
                if b"\x1b[6n" in data:
                    try:
                        with write_lock:
                            os.write(master_fd, b"\x1b[1;1R")
                    except OSError:
                        pass
                log.write(data)
                log.flush()

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()

    success = False
    try:
        live_file = _wait_for_live_file(codex_home, timeout_s=8.0)
        if live_file is None:
            if proc.poll() is not None:
                raise RuntimeError(f"{frontend} exited early (see {log_path})")
            try:
                with write_lock:
                    _send_line(master_fd, "hello")
            except OSError as exc:
                raise RuntimeError(f"failed to write to {frontend} pty (see {log_path}): {exc}") from exc
            live_file = _wait_for_live_file(codex_home, timeout_s=30.0)
        if live_file is None:
            raise RuntimeError(
                f"live status file not created under {codex_home / 'live'} (see {log_path})"
            )

        record = _read_json(live_file)
        if record.get("frontend") != frontend:
            raise RuntimeError(f"expected frontend={frontend!r}, got {record.get('frontend')!r}")
        if not record.get("tty"):
            raise RuntimeError(f"expected tty to be populated for {frontend} session")

        hb1 = record.get("last_heartbeat_at")
        time.sleep(3)
        hb2 = _read_json(live_file).get("last_heartbeat_at")
        if hb1 == hb2:
            raise RuntimeError("heartbeat did not update (last_heartbeat_at unchanged)")

        _wait_for_status(live_file, "waiting_user_input", timeout_s=60.0)

        _type_text(master_fd, write_lock, "Respond with the single word done.")
        _press_enter(master_fd, write_lock)
        _wait_for_status(live_file, "running", timeout_s=60.0)
        _wait_for_status(live_file, "waiting_user_input", timeout_s=180.0)

        _type_text(master_fd, write_lock, "/quit")
        _press_enter(master_fd, write_lock)

        try:
            proc.wait(timeout=30.0)
        except subprocess.TimeoutExpired:
            _kill_process_group(proc)
            raise RuntimeError(f"{frontend} did not exit after /quit (see {log_path})")

        final = _read_json(live_file)
        if final.get("status") != "completed" or final.get("alive") is not False:
            raise RuntimeError(f"expected completed+alive=false; got {final!r}")
        if not final.get("ended_at"):
            raise RuntimeError("expected ended_at set on clean exit")
        success = True
    finally:
        stop_reader.set()
        try:
            _kill_process_group(proc)
        except Exception:
            pass
        try:
            os.close(master_fd)
        except Exception:
            pass
        thread.join(timeout=1.0)
        if success:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"[smoke] kept debug dir: {tmp} (log: {log_path})", file=sys.stderr)


def main() -> int:
    _run_one("tui", ["--disable", "tui2"])
    _run_one("tui2", ["--enable", "tui2"])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
