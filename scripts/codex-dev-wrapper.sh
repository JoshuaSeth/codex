#!/usr/bin/env bash
set -euo pipefail

# Ensure cargo is available even in non-login shells.
if ! command -v cargo >/dev/null 2>&1; then
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
  fi
fi

# Wrapper-only options:
#   --use-speech
#   --voice
#
# `--use-speech` enables the legacy notify hook that only speaks completed turns.
# `--voice` is the modern path: it bootstraps a local dockerized voice web cockpit,
# points Codex at that local endpoint, and keeps the session self-contained instead
# of depending on the shared dispatcher deployment. `--voice` no longer forces
# `--non-stop`; combine the two flags explicitly when you want continuous
# auto-follow-up sampling.
use_speech=0
voice_mode=0
forward_args=()
while [[ $# -gt 0 ]]; do
  arg="$1"
  shift
  case "$arg" in
    --use-speech|--use-speech=1|--use-speech=true|--use-speech=yes|--use-speech=on)
      use_speech=1
      ;;
    --use-speech=0|--use-speech=false|--use-speech=no|--use-speech=off)
      ;;
    --voice)
      voice_mode=1
      forward_args+=("$arg")
      ;;
    --)
      forward_args+=("--" "$@")
      break
      ;;
    *)
      forward_args+=("$arg")
      ;;
  esac
done
set -- "${forward_args[@]}"

toml_escape() {
  local s="${1:-}"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '%s' "$s"
}

has_config_override() {
  local needle="${1:-}"
  shift || true
  local expect_cfg_value=0
  local arg
  for arg in "$@"; do
    if [[ "$expect_cfg_value" == "1" ]]; then
      if [[ "$arg" == "$needle="* ]]; then
        return 0
      fi
      expect_cfg_value=0
      continue
    fi
    if [[ "$arg" == "-c" || "$arg" == "--config" ]]; then
      expect_cfg_value=1
      continue
    fi
    if [[ "$arg" == "$needle="* ]]; then
      return 0
    fi
  done
  return 1
}

pick_dispatch_state_python() {
  local candidate
  if [[ -n "${PITCHAI_CODEX_STATE_PYTHON:-}" ]]; then
    printf '%s\n' "${PITCHAI_CODEX_STATE_PYTHON}"
    return 0
  fi
  for candidate in \
    "/root/code/pitchai_dispatch/_audiofix_worktree/.venv/bin/python" \
    "$HOME/code/pitchai_dispatch/.venv/bin/python" \
    "/root/pitchai-codex-dispatcher/build-src/dispatcher/.venv/bin/python" \
    "python3.14" \
    "python3.13" \
    "python3.12" \
    "python3.11" \
    "python3"; do
    if [[ "$candidate" == */* ]]; then
      if [[ -x "$candidate" ]]; then
        printf '%s\n' "$candidate"
        return 0
      fi
      continue
    fi
    if command -v "$candidate" >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

pick_dispatch_state_hook_script() {
  local candidate
  if [[ -n "${PITCHAI_CODEX_STATE_HOOK_SCRIPT:-}" ]]; then
    printf '%s\n' "${PITCHAI_CODEX_STATE_HOOK_SCRIPT}"
    return 0
  fi
  for candidate in \
    "/root/pitchai-codex-dispatcher/build-src/dispatcher/tools/codex_notify_dispatch_state.py" \
    "/root/code/pitchai_dispatch/dispatcher/tools/codex_notify_dispatch_state.py" \
    "/root/code/pitchai_dispatch/_audiofix_worktree/dispatcher/tools/codex_notify_dispatch_state.py"; do
    if [[ -f "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

pick_dispatch_state_sidecar_script() {
  local candidate
  if [[ -n "${PITCHAI_CODEX_STATE_SIDECAR_SCRIPT:-}" ]]; then
    printf '%s\n' "${PITCHAI_CODEX_STATE_SIDECAR_SCRIPT}"
    return 0
  fi
  for candidate in \
    "/root/pitchai-codex-dispatcher/build-src/dispatcher/tools/codex_session_state_sidecar.py" \
    "/root/code/pitchai_dispatch/dispatcher/tools/codex_session_state_sidecar.py" \
    "/root/code/pitchai_dispatch/_audiofix_worktree/dispatcher/tools/codex_session_state_sidecar.py"; do
    if [[ -f "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

read_auth_token_server_env_value() {
  local key="${1:-}"
  local file="${2:-}"
  local line value
  if [[ -z "$key" || -z "$file" || ! -r "$file" ]]; then
    return 1
  fi
  line="$(grep -m1 "^${key}=" "$file" 2>/dev/null || true)"
  if [[ -z "$line" ]]; then
    return 1
  fi
  value="${line#*=}"
  value="${value%$'\r'}"
  if [[ "$value" == \"*\" && "$value" == *\" ]]; then
    value="${value#\"}"
    value="${value%\"}"
  elif [[ "$value" == \'*\' && "$value" == *\' ]]; then
    value="${value#\'}"
    value="${value%\'}"
  fi
  printf '%s\n' "$value"
}

codex_dev_is_login_command() {
  local saw_login=0
  local arg
  for arg in "$@"; do
    case "$arg" in
      login)
        saw_login=1
        ;;
      status|-h|--help)
        return 1
        ;;
    esac
  done
  [[ "$saw_login" == "1" ]]
}

configure_codex_auth_broker() {
  if [[ "${CODEX_DEV_AUTH_BROKER_DISABLED:-0}" == "1" ]]; then
    return 0
  fi

  local env_file="${CODEX_DEV_AUTH_BROKER_ENV_FILE:-/etc/auth-token-server/auth-token-server.env}"
  local default_url="${CODEX_DEV_AUTH_BROKER_URL:-http://127.0.0.1:38188}"
  local client_token="${AUTH_TOKEN_SERVER_CLIENT_TOKEN:-}"
  local admin_token="${AUTH_TOKEN_SERVER_ADMIN_TOKEN:-}"

  if [[ -z "$client_token" ]]; then
    client_token="$(read_auth_token_server_env_value AUTH_TOKEN_SERVER_CLIENT_TOKEN "$env_file" || true)"
  fi
  if [[ -z "$admin_token" ]]; then
    admin_token="$(read_auth_token_server_env_value AUTH_TOKEN_SERVER_ADMIN_TOKEN "$env_file" || true)"
  fi

  if [[ -z "${CODEX_AUTH_BROKER_TOKEN:-}" && -n "$client_token" ]]; then
    export CODEX_AUTH_BROKER_TOKEN="$client_token"
  fi
  if [[ -z "${CODEX_AUTH_BROKER_URL:-}" && -n "${CODEX_AUTH_BROKER_TOKEN:-}" ]]; then
    export CODEX_AUTH_BROKER_URL="$default_url"
  fi
  if [[ -n "${CODEX_AUTH_BROKER_URL:-}" && -n "${CODEX_AUTH_BROKER_TOKEN:-}" ]]; then
    export CODEX_AUTH_BROKER_CLIENT_NAME="${CODEX_AUTH_BROKER_CLIENT_NAME:-codex-dev}"
    export CODEX_AUTH_BROKER_LEASE_REASON="${CODEX_AUTH_BROKER_LEASE_REASON:-codex-dev}"
    export CODEX_AUTH_BROKER_ROTATION_MAX_ATTEMPTS="${CODEX_AUTH_BROKER_ROTATION_MAX_ATTEMPTS:-64}"
  fi

  if codex_dev_is_login_command "$@"; then
    if [[ -z "${CODEX_AUTH_BROKER_ADMIN_TOKEN:-}" && -n "$admin_token" ]]; then
      export CODEX_AUTH_BROKER_ADMIN_TOKEN="$admin_token"
    fi
    if [[ -z "${CODEX_AUTH_BROKER_ADMIN_URL:-}" && -n "${CODEX_AUTH_BROKER_ADMIN_TOKEN:-}" ]]; then
      export CODEX_AUTH_BROKER_ADMIN_URL="${CODEX_AUTH_BROKER_URL:-$default_url}"
    fi
    export CODEX_AUTH_BROKER_IMPORT_ON_LOGIN="${CODEX_AUTH_BROKER_IMPORT_ON_LOGIN:-1}"
  fi
}

codex_dev_import_login_auth_to_broker() {
  if [[ "${CODEX_DEV_AUTH_BROKER_DISABLED:-0}" == "1" ]]; then
    return 0
  fi
  if [[ "${CODEX_AUTH_BROKER_IMPORT_ON_LOGIN:-0}" != "1" ]]; then
    return 0
  fi
  if [[ -z "${CODEX_AUTH_BROKER_ADMIN_URL:-}" || -z "${CODEX_AUTH_BROKER_ADMIN_TOKEN:-}" ]]; then
    echo "codex-dev: auth broker login import skipped; admin broker env is not configured." >&2
    return 1
  fi

  local codex_home="${CODEX_HOME:-$HOME/.codex}"
  local auth_path="$codex_home/auth.json"
  if [[ ! -r "$auth_path" ]]; then
    echo "codex-dev: auth broker login import failed; auth.json not readable at $auth_path" >&2
    return 1
  fi

  python3 - "$auth_path" <<'PY'
import base64
import json
import os
import pathlib
import sys
import urllib.error
import urllib.request


def _decode_jwt_payload(token: str) -> dict:
    try:
        payload = token.split(".")[1]
        payload += "=" * ((4 - len(payload) % 4) % 4)
        decoded = base64.urlsafe_b64decode(payload.encode("ascii"))
        parsed = json.loads(decoded)
        return parsed if isinstance(parsed, dict) else {}
    except Exception:
        return {}


auth_path = pathlib.Path(sys.argv[1])
auth_json = json.loads(auth_path.read_text(encoding="utf-8"))
tokens = auth_json.get("tokens") if isinstance(auth_json, dict) else None
if not isinstance(tokens, dict) or not tokens.get("refresh_token"):
    raise SystemExit(f"codex-dev: {auth_path} does not contain ChatGPT refresh tokens")

claims = _decode_jwt_payload(str(tokens.get("id_token") or ""))
profile_claims = claims.get("https://api.openai.com/profile")
auth_claims = claims.get("https://api.openai.com/auth")
email = claims.get("email")
if not email and isinstance(profile_claims, dict):
    email = profile_claims.get("email")
account_id = tokens.get("account_id")
if isinstance(auth_claims, dict) and auth_claims.get("chatgpt_account_id"):
    account_id = auth_claims["chatgpt_account_id"]

label = os.environ.get("CODEX_AUTH_BROKER_IMPORT_LABEL") or email or "codex-dev-login"
priority = int(os.environ.get("CODEX_AUTH_BROKER_IMPORT_PRIORITY") or "100")
url = os.environ["CODEX_AUTH_BROKER_ADMIN_URL"].rstrip("/") + "/v1/admin/accounts/import"
body = json.dumps(
    {
        "auth_json": auth_json,
        "label": label,
        "priority": priority,
        "enabled": True,
    }
).encode("utf-8")
request = urllib.request.Request(
    url,
    data=body,
    method="POST",
    headers={
        "Authorization": f"Bearer {os.environ['CODEX_AUTH_BROKER_ADMIN_TOKEN']}",
        "Content-Type": "application/json",
    },
)
try:
    with urllib.request.urlopen(request, timeout=20) as response:
        payload = json.loads(response.read().decode("utf-8"))
except urllib.error.HTTPError as exc:
    detail = exc.read().decode("utf-8", errors="replace")
    raise SystemExit(f"codex-dev: auth broker login import failed: HTTP {exc.code}: {detail}") from exc

imported_account_id = (
    payload.get("metadata", {}).get("account_id") if isinstance(payload, dict) else None
)
print(
    "codex-dev: imported login auth into auth broker"
    f" (account_id={imported_account_id or account_id or 'unknown'}).",
    file=sys.stderr,
)
PY
}

codex_dev_auth_file_fingerprint() {
  local auth_path="${1:-}"
  if [[ -z "$auth_path" || ! -r "$auth_path" ]]; then
    return 1
  fi
  python3 - "$auth_path" <<'PY'
import hashlib
import pathlib
import sys

print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
}

codex_dev_auth_has_chatgpt_refresh_token() {
  local auth_path="${1:-}"
  if [[ -z "$auth_path" || ! -r "$auth_path" ]]; then
    return 1
  fi
  python3 - "$auth_path" <<'PY'
import json
import pathlib
import sys

try:
    auth_json = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
except Exception:
    raise SystemExit(1)
tokens = auth_json.get("tokens") if isinstance(auth_json, dict) else None
if not isinstance(tokens, dict) or not tokens.get("refresh_token"):
    raise SystemExit(1)
PY
}

codex_dev_wait_for_login_auth() {
  local auth_path="${1:-}"
  local before_fingerprint="${2:-}"
  local allow_unchanged="${3:-0}"
  local wait_seconds="${CODEX_DEV_LOGIN_IMPORT_WAIT_SECONDS:-30}"
  local deadline
  local after_fingerprint
  deadline=$((SECONDS + wait_seconds))
  while (( SECONDS <= deadline )); do
    if codex_dev_auth_has_chatgpt_refresh_token "$auth_path"; then
      after_fingerprint="$(codex_dev_auth_file_fingerprint "$auth_path" || true)"
      if [[ "$allow_unchanged" == "1" || -z "$before_fingerprint" || "$after_fingerprint" != "$before_fingerprint" ]]; then
        return 0
      fi
    fi
    sleep 0.5
  done
  return 1
}

codex_dev_run_login_command() {
  local codex_home="${CODEX_HOME:-$HOME/.codex}"
  local auth_path="$codex_home/auth.json"
  local before_fingerprint
  local login_status
  local allow_unchanged=0

  before_fingerprint="$(codex_dev_auth_file_fingerprint "$auth_path" || true)"

  set +e
  "$bin" "$@"
  login_status=$?
  set -e

  if [[ "$login_status" == "0" ]]; then
    allow_unchanged=1
  fi

  if codex_dev_wait_for_login_auth "$auth_path" "$before_fingerprint" "$allow_unchanged"; then
    if codex_dev_import_login_auth_to_broker; then
      exit 0
    fi
    if [[ "$login_status" == "0" ]]; then
      exit 1
    fi
    exit "$login_status"
  fi

  if [[ "$login_status" == "0" && "${CODEX_AUTH_BROKER_IMPORT_ON_LOGIN:-0}" == "1" ]]; then
    echo "codex-dev: login completed but no ChatGPT refresh token was available for auth broker import at $auth_path" >&2
    exit 1
  fi
  exit "$login_status"
}

if [[ "$use_speech" == "1" && "$voice_mode" == "1" ]]; then
  echo "codex-dev: --use-speech ignored because --voice uses the streaming local voice web path." >&2
  use_speech=0
fi

configure_codex_auth_broker "$@"

if [[ "$use_speech" == "1" ]]; then
  if has_config_override notify "$@"; then
    echo "codex-dev: --use-speech ignored (notify override already provided)." >&2
  else
    hook_script="${PITCHAI_CODEX_SPEECH_NOTIFY_SCRIPT:-/root/code/pitchai_dispatch/dispatcher/tools/codex_notify_voice_push.py}"
    py_bin="${PITCHAI_CODEX_SPEECH_PYTHON:-python3}"
    if [[ ! -f "$hook_script" ]]; then
      echo "codex-dev: --use-speech disabled; notify hook script not found: $hook_script" >&2
    elif ! command -v "$py_bin" >/dev/null 2>&1; then
      echo "codex-dev: --use-speech disabled; python runtime not found: $py_bin" >&2
    else
	      if [[ -z "${PITCHAI_CODEX_SPEECH_ENDPOINT:-}" ]]; then
	        base_url="${PITCHAI_DISPATCH_BASE_URL:-https://dispatch.pitchai.net}"
	        base_url="${base_url%/}"
	        export PITCHAI_CODEX_SPEECH_ENDPOINT="$base_url/ui/api/agent/voice_push"
	      fi
	      if [[ -z "${PITCHAI_CODEX_SPEECH_TOKEN:-}" && -z "${PITCHAI_DISPATCH_TOKEN:-}" ]]; then
	        env_file="${PITCHAI_DISPATCH_ENV_FILE:-/root/pitchai-codex-dispatcher/.env}"
	        if [[ -r "$env_file" ]]; then
	          token_line="$(grep -m1 '^PITCHAI_DISPATCH_TOKEN=' "$env_file" 2>/dev/null || true)"
	          if [[ -n "$token_line" ]]; then
	            export PITCHAI_DISPATCH_TOKEN="${token_line#PITCHAI_DISPATCH_TOKEN=}"
	          fi
	        fi
	      fi
	      if [[ -z "${PITCHAI_CODEX_SPEECH_TOKEN:-}" && -n "${PITCHAI_DISPATCH_TOKEN:-}" ]]; then
	        export PITCHAI_CODEX_SPEECH_TOKEN="$PITCHAI_DISPATCH_TOKEN"
	      fi
	      if [[ -z "${PITCHAI_CODEX_SPEECH_SOURCE:-}" ]]; then
	        export PITCHAI_CODEX_SPEECH_SOURCE="codex_notify"
      fi
      notify_override="notify=[\"$(toml_escape "$py_bin")\",\"$(toml_escape "$hook_script")\"]"
      set -- -c "$notify_override" "$@"
      echo "codex-dev: speech hook enabled (--use-speech)." >&2
    fi
  fi
fi

if has_config_override stop_hook_command "$@"; then
  :
else
  hook_script="$(pick_dispatch_state_hook_script || true)"
  py_bin="$(pick_dispatch_state_python || true)"
  if [[ -z "$hook_script" ]]; then
    echo "codex-dev: state stop hook disabled; script not found: $hook_script" >&2
  elif [[ -z "$py_bin" ]]; then
    echo "codex-dev: state stop hook disabled; python runtime not found: $py_bin" >&2
  else
    if [[ -z "${PITCHAI_CODEX_STATE_ENDPOINT:-}" ]]; then
      base_url="${PITCHAI_DISPATCH_BASE_URL:-https://dispatch.pitchai.net}"
      base_url="${base_url%/}"
      export PITCHAI_CODEX_STATE_ENDPOINT="$base_url/ui/api/codex_session_completion"
    fi
    if [[ -z "${PITCHAI_CODEX_STATE_TOKEN:-}" && -z "${PITCHAI_DISPATCH_TOKEN:-}" ]]; then
      env_file="${PITCHAI_DISPATCH_ENV_FILE:-/root/pitchai-codex-dispatcher/.env}"
      if [[ -r "$env_file" ]]; then
        token_line="$(grep -m1 '^PITCHAI_DISPATCH_TOKEN=' "$env_file" 2>/dev/null || true)"
        if [[ -n "$token_line" ]]; then
          export PITCHAI_DISPATCH_TOKEN="${token_line#PITCHAI_DISPATCH_TOKEN=}"
        fi
      fi
    fi
    if [[ -z "${PITCHAI_CODEX_STATE_TOKEN:-}" && -n "${PITCHAI_DISPATCH_TOKEN:-}" ]]; then
      export PITCHAI_CODEX_STATE_TOKEN="$PITCHAI_DISPATCH_TOKEN"
    fi
    stop_hook_override="stop_hook_command=[\"$(toml_escape "$py_bin")\",\"$(toml_escape "$hook_script")\"]"
    set -- -c "$stop_hook_override" "$@"
  fi
fi

main_repo_root="${CODEX_DEV_REPO:-$HOME/code/codex}"

# Source selection:
# - live (default): run from the currently checked out branch in CODEX_DEV_REPO
# - pinned: run a known-good Codex build from a dedicated worktree
# - live: run from the currently checked out branch in CODEX_DEV_REPO
source_mode="${CODEX_DEV_MODE:-live}"

# The pinned build is tracked by a stable tag pushed to our fork.
pin_ref="${CODEX_DEV_PIN_REF:-pitchai-codex-dev-stable-20260123-2}"
pin_worktree="${CODEX_DEV_PIN_WORKTREE:-$HOME/code/worktrees/codex/codex-dev-stable}"

repo_root="$main_repo_root"
if [[ "$source_mode" == "pinned" ]]; then
  if [[ ! -d "$main_repo_root" ]]; then
    echo "codex-dev: Codex repo not found at: $main_repo_root" >&2
    echo "codex-dev: set CODEX_DEV_REPO to your repo root (the folder containing codex-rs/)" >&2
    exit 1
  fi

  # If the directory exists but is a broken/stale worktree (common after moving repos),
  # delete it so we can recreate it cleanly.
  if [[ -d "$pin_worktree" ]]; then
    main_git="$(realpath "$main_repo_root/.git")"
    if ! common_git="$(git -C "$pin_worktree" rev-parse --git-common-dir 2>/dev/null)"; then
      echo "codex-dev: pinned worktree at $pin_worktree is broken; recreating ..." >&2
      rm -rf "$pin_worktree"
    else
      common_git="$(realpath "$common_git")"
      if [[ "$common_git" != "$main_git" ]]; then
        echo "codex-dev: pinned worktree at $pin_worktree points at $common_git (expected $main_git); recreating ..." >&2
        rm -rf "$pin_worktree"
      fi
    fi
  fi

  if [[ ! -d "$pin_worktree" ]]; then
    mkdir -p "$(dirname "$pin_worktree")"
    echo "codex-dev: creating pinned worktree at $pin_worktree ($pin_ref) ..." >&2
    git -C "$main_repo_root" worktree prune >/dev/null 2>&1 || true
    if ! git -C "$main_repo_root" worktree add "$pin_worktree" "$pin_ref" >/dev/null; then
      echo "codex-dev: worktree add failed; pruning stale worktrees and retrying with -f ..." >&2
      git -C "$main_repo_root" worktree prune >/dev/null 2>&1 || true
      git -C "$main_repo_root" worktree add -f "$pin_worktree" "$pin_ref" >/dev/null
    fi
  fi

  desired_commit="$(git -C "$main_repo_root" rev-parse "${pin_ref}^{commit}")"
  actual_commit="$(git -C "$pin_worktree" rev-parse HEAD 2>/dev/null || true)"
  if [[ "$actual_commit" != "$desired_commit" ]]; then
    echo "codex-dev: syncing pinned worktree to $pin_ref (${desired_commit:0:12}) ..." >&2
    git -C "$pin_worktree" checkout --detach "$desired_commit" >/dev/null
  fi

  repo_root="$pin_worktree"
elif [[ "$source_mode" == "live" ]]; then
  repo_root="$main_repo_root"
else
  echo "codex-dev: invalid CODEX_DEV_MODE: $source_mode (use pinned or live)" >&2
  exit 2
fi

workspace_root="$repo_root/codex-rs"
voice_bootstrap_script="$repo_root/scripts/voice/codex-dev-voice-web.sh"

profile="${CODEX_DEV_PROFILE:-release}"
case "$profile" in
  release|debug) ;;
  *)
    echo "codex-dev: invalid CODEX_DEV_PROFILE: $profile (use release or debug)" >&2
    exit 2
    ;;
esac

if [[ ! -d "$workspace_root" ]]; then
  echo "codex-dev: Codex workspace not found at: $workspace_root" >&2
  echo "codex-dev: set CODEX_DEV_REPO or CODEX_DEV_PIN_WORKTREE to a repo root containing codex-rs/" >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "codex-dev: cargo not found in PATH" >&2
  exit 127
fi

has_yolo_arg() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      --yolo|--dangerously-bypass-approvals-and-sandbox)
        return 0
        ;;
    esac
  done
  return 1
}

has_model_arg() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      -m|--model|--model=*)
        return 0
        ;;
    esac
  done
  return 1
}

voice_web_auto_open_enabled() {
  case "$(printf '%s' "${PITCHAI_CODEX_VOICE_WEB_AUTO_OPEN:-1}" | tr '[:upper:]' '[:lower:]')" in
    0|false|no|off)
      return 1
      ;;
  esac
  return 0
}

voice_web_has_browser_session() {
  if [[ -n "${WSL_DISTRO_NAME:-}" || -n "${WSL_INTEROP:-}" ]]; then
    return 0
  fi
  if [[ "$OSTYPE" == darwin* ]]; then
    return 0
  fi
  if [[ -n "${DISPLAY:-}" || -n "${WAYLAND_DISPLAY:-}" || -n "${MIR_SOCKET:-}" ]]; then
    return 0
  fi
  return 1
}

first_browser_candidate() {
  local raw="${1:-}"
  raw="${raw%%:*}"
  raw="${raw//%s/}"
  raw="${raw%% *}"
  printf '%s\n' "$raw"
}

open_url_in_browser() {
  local url="${1:-}"
  local browser_name=""
  local opener_pid=""
  local -a opener=()

  if [[ -z "$url" ]]; then
    return 0
  fi
  if ! voice_web_auto_open_enabled; then
    return 0
  fi
  if ! voice_web_has_browser_session; then
    return 0
  fi

  if [[ -n "${PITCHAI_CODEX_VOICE_WEB_BROWSER:-}" ]]; then
    browser_name="${PITCHAI_CODEX_VOICE_WEB_BROWSER}"
    if command -v "$browser_name" >/dev/null 2>&1; then
      opener=("$browser_name")
    else
      echo "codex-dev: configured voice browser command not found: $browser_name" >&2
      return 0
    fi
  else
    browser_name="$(first_browser_candidate "${BROWSER:-}")"
    if [[ -n "$browser_name" ]] && command -v "$browser_name" >/dev/null 2>&1; then
      opener=("$browser_name")
    elif [[ -n "${WSL_DISTRO_NAME:-}" || -n "${WSL_INTEROP:-}" ]] && command -v wslview >/dev/null 2>&1; then
      opener=(wslview)
    elif [[ "$OSTYPE" == darwin* ]] && command -v open >/dev/null 2>&1; then
      opener=(open)
    elif command -v xdg-open >/dev/null 2>&1; then
      opener=(xdg-open)
    elif command -v gio >/dev/null 2>&1; then
      opener=(gio open)
    fi
  fi

  if [[ "${#opener[@]}" -eq 0 ]]; then
    return 0
  fi

  echo "codex-dev: opening voice cockpit in browser at ${url}" >&2
  ("${opener[@]}" "$url" >/dev/null 2>&1) &
  opener_pid="$!"
  disown "$opener_pid" 2>/dev/null || true
}

voice_default_model() {
  if [[ -n "${CODEX_DEV_VOICE_DEFAULT_MODEL:-}" ]]; then
    printf '%s\n' "${CODEX_DEV_VOICE_DEFAULT_MODEL}"
    return 0
  fi
  printf '%s\n' "${CODEX_DEV_VOICE_DEFAULT_MODEL_DEFAULT:-gpt-5.4}"
}

tmux_context() {
  if [[ -n "${TMUX:-}" ]]; then
    local session_name
    local window_index
    local socket_path
    session_name="$(tmux display-message -p -F '#{session_name}')"
    window_index="$(tmux display-message -p -F '#{window_index}')"
    socket_path="$(tmux display-message -p -F '#{socket_path}')"
    printf '%s\n%s\n%s\n' "$session_name" "$window_index" "$socket_path"
    return 0
  fi
  if [[ -n "${CODEX_DEV_VOICE_TMUX_SESSION:-}" ]]; then
    local session_name="${CODEX_DEV_VOICE_TMUX_SESSION:-}"
    local window_index="${CODEX_DEV_VOICE_TMUX_WINDOW:-0}"
    local socket_path="${CODEX_DEV_VOICE_TMUX_SOCKET:-$(tmux display-message -p -F '#{socket_path}' 2>/dev/null || true)}"
    printf '%s\n%s\n%s\n' "$session_name" "$window_index" "$socket_path"
    return 0
  fi
  return 1
}

# Build output directories.
#
# `codex-dev` prefers running an existing binary over rebuilding, but when it does
# build we use a dedicated target dir to avoid getting stuck behind other cargo
# invocations holding a lock on the default `target/` directory:
#   "Blocking waiting for file lock on artifact directory"
default_target_dir="$workspace_root/target"
dev_target_dir="${CODEX_DEV_TARGET_DIR:-$default_target_dir/codex-dev}"

default_bin="$default_target_dir/$profile/codex"
dev_bin="$dev_target_dir/$profile/codex"

if [[ -x "$default_bin" && -x "$dev_bin" ]]; then
  if [[ "$dev_bin" -nt "$default_bin" ]]; then
    bin="$dev_bin"
  else
    bin="$default_bin"
  fi
elif [[ -x "$dev_bin" ]]; then
  bin="$dev_bin"
elif [[ -x "$default_bin" ]]; then
  bin="$default_bin"
else
  # Default build output when no binary exists.
  bin="$dev_bin"
fi

# Default behavior: do not rebuild automatically.
# Set CODEX_DEV_AUTO_BUILD=1 to rebuild when sources changed.
auto_build="${CODEX_DEV_AUTO_BUILD:-0}"

needs_build=0
build_reason=""

if [[ ! -x "$bin" ]]; then
  needs_build=1
  build_reason="missing binary"
elif [[ "${CODEX_DEV_ALWAYS_BUILD:-}" == "1" ]]; then
  needs_build=1
  build_reason="CODEX_DEV_ALWAYS_BUILD=1"
elif [[ "$auto_build" != "0" ]]; then
  if IFS= read -r _ < <(
    find "$workspace_root" \
      -path "$workspace_root/target" -prune -o \
      -type f \( -name "*.rs" -o -name "Cargo.toml" -o -name "Cargo.lock" \) \
      -newer "$bin" -print -quit
  ); then
    needs_build=1
    build_reason="sources changed"
  fi
fi

if [[ "$needs_build" == "1" ]]; then
  build_args=("-p" "codex-cli")
  if [[ "$profile" == "release" ]]; then
    build_args+=("--release")
  fi

  if [[ -n "$build_reason" ]]; then
    echo "codex-dev: building Codex ($profile) in $workspace_root ($build_reason) ..." >&2
  else
    echo "codex-dev: building Codex ($profile) in $workspace_root ..." >&2
  fi

  (cd "$workspace_root" && CARGO_TARGET_DIR="$dev_target_dir" cargo build "${build_args[@]}")
  bin="$dev_bin"
fi

if [[ "$voice_mode" == "1" ]]; then
  if [[ ! -x "$voice_bootstrap_script" ]]; then
    echo "codex-dev: missing voice bootstrap helper: $voice_bootstrap_script" >&2
    exit 1
  fi

  if ! has_yolo_arg "$@"; then
    set -- --yolo "$@"
  fi
  if ! has_model_arg "$@"; then
    set -- --model "$(voice_default_model)" "$@"
  fi

  if ! tmux_lines="$(tmux_context 2>/dev/null)"; then
    if ! command -v tmux >/dev/null 2>&1; then
      echo "codex-dev: --voice requires tmux so live transcripts can be inserted into the running session." >&2
      exit 1
    fi

    generated_session="codex-voice-$(date +%Y%m%d-%H%M%S)"
    tmux_cmd=(env "CODEX_DEV_VOICE_TMUX_SESSION=$generated_session" "CODEX_DEV_VOICE_TMUX_WINDOW=0" "$0")
    while [[ $# -gt 0 ]]; do
      tmux_cmd+=("$1")
      shift
    done
    printf -v tmux_cmd_string '%q ' "${tmux_cmd[@]}"
    echo "codex-dev: --voice needs tmux; launching a dedicated session ${generated_session} ..." >&2
    exec tmux new-session \
      -s "$generated_session" \
      -c "${PWD}" \
      "$tmux_cmd_string"
  fi

  session_name="$(printf '%s\n' "$tmux_lines" | sed -n '1p')"
  window_index="$(printf '%s\n' "$tmux_lines" | sed -n '2p')"
  socket_path="$(printf '%s\n' "$tmux_lines" | sed -n '3p')"

  if [[ -z "$session_name" ]]; then
    echo "codex-dev: failed to resolve tmux session for --voice" >&2
    exit 1
  fi

  export CODEX_DEV_CODE_ROOT="$(dirname "$repo_root")"
  if ! voice_bootstrap_exports="$(
    CODEX_HOME="${CODEX_HOME:-$HOME/.codex}" \
      "$voice_bootstrap_script" \
        --shell \
        --tmux-session "$session_name" \
        --tmux-window "${window_index:-0}" \
        --tmux-socket "${socket_path:-}"
  )"; then
    echo "codex-dev: failed to bootstrap local voice web helper" >&2
    exit 1
  fi
  eval "$voice_bootstrap_exports"
  if [[ -z "${PITCHAI_CODEX_VOICE_WEB_URL:-}" ]]; then
    echo "codex-dev: voice web bootstrap did not return PITCHAI_CODEX_VOICE_WEB_URL" >&2
    exit 1
  fi
  export PITCHAI_CODEX_SPEECH_TIMEOUT_S="${PITCHAI_CODEX_SPEECH_TIMEOUT_S:-8}"
  export PITCHAI_CODEX_SPEECH_INITIAL_CHARS="${PITCHAI_CODEX_SPEECH_INITIAL_CHARS:-8}"
  export PITCHAI_CODEX_SPEECH_UPDATE_CHARS="${PITCHAI_CODEX_SPEECH_UPDATE_CHARS:-24}"

  echo "codex-dev: voice cockpit ready at ${PITCHAI_CODEX_VOICE_WEB_URL}" >&2
  if [[ -n "${PITCHAI_CODEX_VOICE_WEB_PUBLIC_URL:-}" && "${PITCHAI_CODEX_VOICE_WEB_PUBLIC_URL}" == "${PITCHAI_CODEX_VOICE_WEB_URL}" ]]; then
    echo "codex-dev: local fallback voice cockpit at ${PITCHAI_CODEX_VOICE_WEB_LOCAL_URL}" >&2
  elif [[ -n "${PITCHAI_CODEX_VOICE_WEB_PUBLIC_URL:-}" ]]; then
    echo "codex-dev: public voice cockpit ready at ${PITCHAI_CODEX_VOICE_WEB_PUBLIC_URL}" >&2
  fi
  open_url_in_browser "${PITCHAI_CODEX_VOICE_WEB_URL}"
fi

if tmux_lines_state="$(tmux_context 2>/dev/null)"; then
  state_tmux_session="$(printf '%s\n' "$tmux_lines_state" | sed -n '1p')"
  state_tmux_window="$(printf '%s\n' "$tmux_lines_state" | sed -n '2p')"
  state_tmux_socket="$(printf '%s\n' "$tmux_lines_state" | sed -n '3p')"
  if [[ -n "$state_tmux_session" ]]; then
    export PITCHAI_CODEX_STATE_TMUX_SESSION="$state_tmux_session"
    export PITCHAI_CODEX_STATE_TMUX_WINDOW="${state_tmux_window:-0}"
    export PITCHAI_CODEX_STATE_TMUX_SOCKET="${state_tmux_socket:-}"
  fi
fi

if [[ -n "${PITCHAI_CODEX_STATE_TMUX_SESSION:-}" ]] && [[ "${PITCHAI_CODEX_STATE_SIDECAR_DISABLED:-0}" != "1" ]]; then
  sidecar_script="$(pick_dispatch_state_sidecar_script || true)"
  sidecar_python="${PITCHAI_CODEX_STATE_SIDECAR_PYTHON:-$(pick_dispatch_state_python || true)}"
  if [[ -n "$sidecar_script" ]] && [[ -n "$sidecar_python" ]]; then
    (
      export PITCHAI_CODEX_SESSION_STATE_REGISTRY_PATH="${PITCHAI_CODEX_SESSION_STATE_REGISTRY_PATH:-${PITCHAI_CODEX_STATE_REGISTRY_PATH:-/data/codex_session_state_registry.yaml}}"
      "$sidecar_python" "$sidecar_script" --pid "$$" >/dev/null 2>&1
    ) &
    disown "$!" 2>/dev/null || true
  fi
fi

if codex_dev_is_login_command "$@"; then
  codex_dev_run_login_command "$@"
fi

exec "$bin" "$@"
