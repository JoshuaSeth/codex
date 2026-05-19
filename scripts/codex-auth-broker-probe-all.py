#!/usr/bin/env python3
"""Probe every auth-token-server account and print a sanitized quota summary."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import sys
import urllib.error
import urllib.parse
import urllib.request


DEFAULT_ENV_FILE = "/etc/auth-token-server/auth-token-server.env"
DEFAULT_BROKER_URL = "http://127.0.0.1:38188"


def _read_env_file(path: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    if not path.exists():
        return values
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        values[key.strip()] = value.strip().strip('"').strip("'")
    return values


def _request(
    base_url: str,
    token: str,
    path: str,
    *,
    method: str = "GET",
    timeout: float = 30.0,
) -> dict:
    data = b"{}" if method != "GET" else None
    request = urllib.request.Request(
        base_url.rstrip("/") + path,
        data=data,
        method=method,
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        payload = json.loads(response.read().decode("utf-8"))
    if not isinstance(payload, dict):
        raise RuntimeError(f"Broker returned non-object JSON for {path}")
    return payload


def _epoch_to_iso(value: object) -> str | None:
    if not isinstance(value, int):
        return None
    return dt.datetime.fromtimestamp(value, tz=dt.timezone.utc).isoformat()


def _window_summary(window: object) -> dict[str, object | None]:
    if not isinstance(window, dict):
        return {"used_percent": None, "reset_at": None, "reset_after_seconds": None}
    return {
        "used_percent": window.get("used_percent"),
        "reset_at": _epoch_to_iso(window.get("reset_at")),
        "reset_after_seconds": window.get("reset_after_seconds"),
    }


def _safe_account_summary(account: dict) -> dict[str, object]:
    metadata = account.get("metadata") if isinstance(account.get("metadata"), dict) else {}
    state = account.get("state") if isinstance(account.get("state"), dict) else {}
    usage = state.get("usage") if isinstance(state.get("usage"), dict) else {}
    rate_limit = usage.get("rate_limit") if isinstance(usage.get("rate_limit"), dict) else {}
    return {
        "account_id": metadata.get("account_id"),
        "label": metadata.get("label"),
        "email": usage.get("email") or metadata.get("label"),
        "enabled": metadata.get("enabled"),
        "availability": state.get("availability"),
        "plan_type": usage.get("plan_type"),
        "active_lease": bool(state.get("active_lease")),
        "lease_expires_at": state.get("lease_expires_at"),
        "last_probe_at": state.get("last_probe_at"),
        "last_error": state.get("last_error"),
        "rate_limit_allowed": rate_limit.get("allowed") if isinstance(rate_limit, dict) else None,
        "rate_limit_reached": rate_limit.get("limit_reached") if isinstance(rate_limit, dict) else None,
        "five_hour": _window_summary(rate_limit.get("primary_window")),
        "weekly": _window_summary(rate_limit.get("secondary_window")),
    }


def _format_percent(value: object) -> str:
    if value is None:
        return "-"
    return f"{value}%"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env-file", default=os.environ.get("CODEX_DEV_AUTH_BROKER_ENV_FILE", DEFAULT_ENV_FILE))
    parser.add_argument("--json", action="store_true", help="print sanitized JSON instead of a table")
    args = parser.parse_args()

    env_values = _read_env_file(Path(args.env_file))
    base_url = (
        os.environ.get("CODEX_AUTH_BROKER_ADMIN_URL")
        or os.environ.get("CODEX_AUTH_BROKER_URL")
        or env_values.get("CODEX_AUTH_BROKER_ADMIN_URL")
        or env_values.get("CODEX_AUTH_BROKER_URL")
        or DEFAULT_BROKER_URL
    )
    token = (
        os.environ.get("CODEX_AUTH_BROKER_ADMIN_TOKEN")
        or os.environ.get("AUTH_TOKEN_SERVER_ADMIN_TOKEN")
        or env_values.get("CODEX_AUTH_BROKER_ADMIN_TOKEN")
        or env_values.get("AUTH_TOKEN_SERVER_ADMIN_TOKEN")
    )
    if not token:
        raise SystemExit("missing broker admin token")

    accounts = _request(base_url, token, "/v1/admin/accounts").get("accounts", [])
    if not isinstance(accounts, list):
        raise SystemExit("broker /v1/admin/accounts returned invalid accounts payload")

    probe_errors: dict[str, str] = {}
    for account in accounts:
        metadata = account.get("metadata") if isinstance(account.get("metadata"), dict) else {}
        account_id = metadata.get("account_id")
        if not isinstance(account_id, str) or not account_id:
            continue
        try:
            _request(
                base_url,
                token,
                f"/v1/admin/accounts/{urllib.parse.quote(account_id, safe='')}/probe",
                method="POST",
            )
        except Exception as exc:
            probe_errors[account_id] = str(exc)

    refreshed = _request(base_url, token, "/v1/admin/accounts").get("accounts", [])
    summaries = [_safe_account_summary(account) for account in refreshed if isinstance(account, dict)]
    for summary in summaries:
        account_id = summary.get("account_id")
        if isinstance(account_id, str) and account_id in probe_errors:
            summary["probe_error"] = probe_errors[account_id]

    summaries.sort(key=lambda item: str(item.get("email") or item.get("label") or ""))

    if args.json:
        print(json.dumps({"accounts": summaries}, indent=2, sort_keys=True))
        return 0 if not probe_errors else 1

    print("email\tavailability\t5h_used\t5h_reset_utc\tweekly_used\tweekly_reset_utc\tactive_lease")
    for summary in summaries:
        five_hour = summary["five_hour"]
        weekly = summary["weekly"]
        print(
            "\t".join(
                [
                    str(summary.get("email") or ""),
                    str(summary.get("availability") or ""),
                    _format_percent(five_hour["used_percent"]),
                    str(five_hour["reset_at"] or ""),
                    _format_percent(weekly["used_percent"]),
                    str(weekly["reset_at"] or ""),
                    "yes" if summary.get("active_lease") else "no",
                ]
            )
        )
    return 0 if not probe_errors else 1


if __name__ == "__main__":
    sys.exit(main())
