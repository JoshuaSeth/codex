#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"

usage() {
  cat <<'EOF' >&2
Usage:
  codex-dev-voice-web.sh --shell --tmux-session <name> [--tmux-window <idx>] [--tmux-socket <path>]
  codex-dev-voice-web.sh --json  --tmux-session <name> [--tmux-window <idx>] [--tmux-socket <path>]

Bootstraps the local codex voice web container and prints either shell exports or JSON.
EOF
  exit 2
}

env_file_value() {
  local name="$1"
  local env_file="$2"
  if [[ ! -r "$env_file" ]]; then
    return 1
  fi

  python3 - "$name" "$env_file" <<'PY'
import pathlib
import sys

name = sys.argv[1]
env_file = pathlib.Path(sys.argv[2])
prefix = f"{name}="

for raw_line in env_file.read_text(encoding="utf-8", errors="replace").splitlines():
    line = raw_line.strip()
    if not line or line.startswith("#") or not line.startswith(prefix):
        continue
    print(line[len(prefix):])
    raise SystemExit(0)

raise SystemExit(1)
PY
}

resolve_secret() {
  local name="$1"
  local env_file="$2"
  local value="${!name:-}"
  if [[ -n "$value" ]]; then
    printf '%s\n' "$value"
    return 0
  fi
  env_file_value "$name" "$env_file"
}

pick_port() {
  local preferred="${1:-}"
  python3 - "$preferred" <<'PY'
import socket
import sys

preferred = sys.argv[1].strip()

def is_free(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True

if preferred:
    port = int(preferred)
    if not is_free(port):
        raise SystemExit(f"requested voice web port {port} is not available")
    print(port)
    raise SystemExit(0)

for port in range(8140, 8160):
    if is_free(port):
        print(port)
        raise SystemExit(0)

raise SystemExit("no free voice web port found in 8140-8159")
PY
}

urlencode() {
  python3 - "$1" <<'PY'
import sys
import urllib.parse

print(urllib.parse.quote(sys.argv[1], safe=""))
PY
}

json_string() {
  python3 - "$1" <<'PY'
import json
import sys

print(json.dumps(sys.argv[1]))
PY
}

resolve_dockerfile_path() {
  local build_context_root="$1"
  local explicit_path="${PITCHAI_CODEX_VOICE_WEB_DOCKERFILE:-}"
  if [[ -n "$explicit_path" ]]; then
    printf '%s\n' "$explicit_path"
    return 0
  fi
  if [[ -f "$build_context_root/Dockerfile.voice" ]]; then
    printf '%s\n' "$build_context_root/Dockerfile.voice"
    return 0
  fi
  printf '%s\n' "$build_context_root/Dockerfile"
}

dockerfile_requires_dft_lib_llm_context() {
  local dockerfile="$1"
  [[ -f "$dockerfile" ]] || return 1
  rg -q 'COPY --from=dft_lib_llm ' "$dockerfile"
}

first_non_loopback_ipv4() {
  python3 - <<'PY'
import ipaddress
import socket

try:
    infos = socket.getaddrinfo(socket.gethostname(), None, family=socket.AF_INET, type=socket.SOCK_STREAM)
except Exception:
    infos = []

seen = set()
for info in infos:
    addr = info[4][0]
    if addr in seen:
        continue
    seen.add(addr)
    try:
        ip = ipaddress.ip_address(addr)
    except ValueError:
        continue
    if ip.is_loopback:
        continue
    if ip.is_private and not ip.is_global:
        print(addr)
        raise SystemExit(0)
    print(addr)
    raise SystemExit(0)

raise SystemExit(1)
PY
}

detect_public_ipv4() {
  local explicit_host="${PITCHAI_CODEX_VOICE_PUBLIC_HOST:-}"
  if [[ -n "$explicit_host" ]]; then
    printf '%s\n' "$explicit_host"
    return 0
  fi

  if curl -4fsS --max-time 3 https://api.ipify.org 2>/dev/null; then
    printf '\n'
    return 0
  fi

  first_non_loopback_ipv4
}

hash_config() {
  local payload="$1"
  printf '%s' "$payload" | sha256sum | awk '{print $1}'
}

hash_build_context() {
  local build_context_root="$1"
  local dockerfile_path="$2"
  python3 - "$build_context_root" "$dockerfile_path" <<'PY'
import hashlib
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
dockerfile = pathlib.Path(sys.argv[2])
if not root.is_dir():
    raise SystemExit("missing build context")

try:
    dockerfile_relative = dockerfile.relative_to(root)
except ValueError:
    dockerfile_relative = None

include_paths = [root / "tailwind.config.js", root / "tailwind.input.css", root / "src"]

if dockerfile.is_file():
    include_paths.insert(0, dockerfile)

if dockerfile.name == "Dockerfile.voice":
    include_paths.insert(1, root / "pyproject.voice.toml")
else:
    include_paths.insert(1, root / "pyproject.toml")

digest = hashlib.sha256()

for base in include_paths:
    if not base.exists():
        continue
    if base.is_file():
        if dockerfile_relative is not None and base == dockerfile:
            relative = dockerfile_relative
        elif base == dockerfile:
            relative = pathlib.Path(f"__external__/{dockerfile.name}")
        else:
            relative = base.relative_to(root)
        digest.update(str(relative).encode("utf-8"))
        digest.update(b"\0")
        digest.update(base.read_bytes())
        digest.update(b"\0")
        continue
    for path in sorted(p for p in base.rglob("*") if p.is_file()):
        relative = path.relative_to(root)
        parts = relative.parts
        if any(part in {".git", ".venv", "node_modules", "__pycache__"} for part in parts):
            continue
        digest.update(str(relative).encode("utf-8"))
        digest.update(b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")

print(digest.hexdigest())
PY
}

wait_for_health() {
  local base_url="$1"
  local container_name="$2"
  local attempts=60

  for ((i=0; i<attempts; i++)); do
    if curl -fsS "${base_url}/healthz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done

  echo "codex-dev: local voice web failed health check at ${base_url}/healthz" >&2
  docker logs --tail 120 "$container_name" >&2 || true
  return 1
}

output_mode=""
tmux_session=""
tmux_window=""
tmux_socket=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --shell)
      output_mode="shell"
      shift
      ;;
    --json)
      output_mode="json"
      shift
      ;;
    --tmux-session)
      tmux_session="${2:-}"
      shift 2
      ;;
    --tmux-window)
      tmux_window="${2:-}"
      shift 2
      ;;
    --tmux-socket)
      tmux_socket="${2:-}"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

if [[ -z "$output_mode" || -z "$tmux_session" ]]; then
  usage
fi

dispatch_repo="${PITCHAI_DISPATCH_REPO:-/root/code/pitchai_dispatch}"
dispatch_app_root="${dispatch_repo}/dispatcher"
build_context="${PITCHAI_CODEX_VOICE_WEB_BUILD_CONTEXT:-$dispatch_app_root}"
dockerfile_path="$(resolve_dockerfile_path "$build_context")"
dispatch_env_file="${PITCHAI_DISPATCH_ENV_FILE:-/root/pitchai-codex-dispatcher/.env}"
dft_lib_llm_dir="${PITCHAI_DFT_LIB_LLM_DIR:-/root/code/dft/libs/lib_llm}"
if [[ -n "${PITCHAI_CODEX_VOICE_WEB_CONTAINER:-}" ]]; then
  container_name="${PITCHAI_CODEX_VOICE_WEB_CONTAINER}"
else
  session_slug="$(printf '%s' "$tmux_session" | tr '[:upper:]' '[:lower:]' | tr -cs 'a-z0-9' '-')"
  session_slug="${session_slug#-}"
  session_slug="${session_slug%-}"
  if [[ -z "$session_slug" ]]; then
    session_slug="session"
  fi
  session_slug="${session_slug:0:48}"
  container_name="pitchai-codex-dev-voice-web-${session_slug}"
fi
image_name="${PITCHAI_CODEX_VOICE_WEB_IMAGE:-pitchai/codex-dev-voice-web:local}"
public_proxy_script="${PITCHAI_CODEX_VOICE_PUBLIC_PROXY_SCRIPT:-$SCRIPT_DIR/codex-dev-voice-public-proxy.sh}"
codex_home="${CODEX_HOME:-$HOME/.codex}"
voice_state_root="${PITCHAI_CODEX_VOICE_WEB_STATE_ROOT:-${codex_home}/voice-web}"
state_file="${voice_state_root}/local-web.json"
token_file="${voice_state_root}/dispatch-token"
code_root="${CODEX_DEV_CODE_ROOT:-$HOME/code}"

for required_cmd in curl docker python3; do
  if ! command -v "$required_cmd" >/dev/null 2>&1; then
    echo "codex-dev: missing required command for --voice: $required_cmd" >&2
    exit 1
  fi
done

assemblyai_api_key="$(resolve_secret PITCHAI_ASSEMBLYAI_API_KEY "$dispatch_env_file" || true)"
if [[ -z "$assemblyai_api_key" ]]; then
  echo "codex-dev: --voice requires PITCHAI_ASSEMBLYAI_API_KEY (env or $dispatch_env_file)" >&2
  exit 1
fi

tts_base_url="$(resolve_secret PITCHAI_TTS_BASE_URL "$dispatch_env_file" || true)"
tts_api_key="$(resolve_secret PITCHAI_TTS_API_KEY "$dispatch_env_file" || true)"
tts_model="$(resolve_secret PITCHAI_TTS_MODEL "$dispatch_env_file" || true)"
tts_voice="$(resolve_secret PITCHAI_TTS_VOICE "$dispatch_env_file" || true)"

if [[ -z "$tts_base_url" ]]; then
  tts_base_url="http://host.docker.internal:8891"
fi
audio_push_stream_force_chunks="${PITCHAI_CODEX_VOICE_AUDIO_FORCE_CHUNKS:-0}"
audio_push_stream_chunk_chars="${PITCHAI_CODEX_VOICE_AUDIO_CHUNK_CHARS:-320}"

mkdir -p "$voice_state_root" "$voice_state_root/queue" "$voice_state_root/runs" "$codex_home"

dispatch_token="${PITCHAI_CODEX_VOICE_WEB_TOKEN:-}"
if [[ -z "$dispatch_token" && -r "$token_file" ]]; then
  dispatch_token="$(tr -d '\r\n' <"$token_file")"
fi
if [[ -z "$dispatch_token" ]]; then
  dispatch_token="$(
    python3 - <<'PY'
import secrets
print(secrets.token_urlsafe(24))
PY
  )"
  printf '%s\n' "$dispatch_token" >"$token_file"
fi

requested_port="${PITCHAI_CODEX_VOICE_WEB_PORT:-}"
bind_addr="${PITCHAI_CODEX_VOICE_WEB_BIND_ADDR:-127.0.0.1}"
port=""
if docker container inspect "$container_name" >/dev/null 2>&1; then
  running_state="$(docker inspect -f '{{.State.Running}}' "$container_name" 2>/dev/null || true)"
  existing_port="$(docker inspect -f '{{range $p, $v := .NetworkSettings.Ports}}{{if eq $p "8129/tcp"}}{{(index $v 0).HostPort}}{{end}}{{end}}' "$container_name" 2>/dev/null || true)"
  if [[ "$running_state" == "true" && -n "$existing_port" ]]; then
    port="${requested_port:-$existing_port}"
  fi
fi
if [[ -z "$port" ]]; then
  port="$(pick_port "$requested_port")"
fi
base_url="http://127.0.0.1:${port}"

public_base_url="${PITCHAI_CODEX_VOICE_PUBLIC_BASE_URL:-}"
if [[ -z "$public_base_url" ]]; then
  if [[ -x "$public_proxy_script" ]]; then
    public_proxy_err_file="$(mktemp)"
    if public_base_url="$("$public_proxy_script" --target-port "$port" 2>"$public_proxy_err_file" | tr -d '\r\n')"; then
      :
    else
      if [[ -s "$public_proxy_err_file" ]]; then
        echo "codex-dev: failed to configure public HTTPS voice URL:" >&2
        sed 's/^/codex-dev:   /' "$public_proxy_err_file" >&2
      fi
      public_base_url=""
    fi
    rm -f "$public_proxy_err_file"
  fi
fi
public_base_url="${public_base_url%/}"

build_source_hash=""
force_image_build=0
case "$(printf '%s' "${PITCHAI_CODEX_VOICE_WEB_FORCE_BUILD:-0}" | tr '[:upper:]' '[:lower:]')" in
  1|true|yes|on)
    force_image_build=1
    ;;
esac

rebuild_on_change=0
case "$(printf '%s' "${PITCHAI_CODEX_VOICE_WEB_REBUILD_ON_CHANGE:-1}" | tr '[:upper:]' '[:lower:]')" in
  1|true|yes|on)
    rebuild_on_change=1
    ;;
esac

socket_dir=""
if [[ -n "$tmux_socket" ]]; then
  socket_dir="$(dirname "$tmux_socket")"
  if [[ ! -d "$socket_dir" ]]; then
    echo "codex-dev: tmux socket directory does not exist: $socket_dir" >&2
    exit 1
  fi
fi

current_image_build_hash=""
current_image_id=""
image_exists=0
if docker image inspect "$image_name" >/dev/null 2>&1; then
  image_exists=1
  current_image_build_hash="$(docker image inspect -f '{{ index .Config.Labels "org.opencontainers.image.revision" }}' "$image_name" 2>/dev/null || true)"
  current_image_id="$(docker image inspect -f '{{.Id}}' "$image_name" 2>/dev/null || true)"
fi

need_image_build=0
build_reason=""
if [[ "$image_exists" != "1" ]]; then
  need_image_build=1
  build_reason="image missing"
elif [[ "$force_image_build" == "1" ]]; then
  need_image_build=1
  build_reason="PITCHAI_CODEX_VOICE_WEB_FORCE_BUILD=1"
elif [[ "$rebuild_on_change" == "1" ]]; then
  if [[ -d "$build_context" ]]; then
    build_source_hash="$(hash_build_context "$build_context" "$dockerfile_path")"
  fi
  if [[ -n "$build_source_hash" && "$current_image_build_hash" != "$build_source_hash" ]]; then
    need_image_build=1
    build_reason="source changed"
  fi
fi

if [[ "$need_image_build" == "1" ]]; then
  if [[ -z "$build_source_hash" && -d "$build_context" ]]; then
    build_source_hash="$(hash_build_context "$build_context" "$dockerfile_path")"
  fi
  if [[ ! -d "$build_context" ]]; then
    echo "codex-dev: voice web image $image_name needs rebuild but build context does not exist: $build_context" >&2
    exit 1
  fi
  if [[ ! -f "$dockerfile_path" ]]; then
    echo "codex-dev: voice web image $image_name needs rebuild but dockerfile does not exist: $dockerfile_path" >&2
    exit 1
  fi
  if [[ -n "$build_reason" ]]; then
    echo "codex-dev: building local voice web image $image_name ($build_reason) ..." >&2
  else
    echo "codex-dev: building local voice web image $image_name ..." >&2
  fi
  docker_build_args=(
    build
    -f "$dockerfile_path" \
    --build-arg "PITCHAI_BUILD_SHA=${build_source_hash}" \
    -t "$image_name" \
  )
  if dockerfile_requires_dft_lib_llm_context "$dockerfile_path"; then
    if [[ ! -f "$dft_lib_llm_dir/pyproject.toml" ]]; then
      echo "codex-dev: voice web build requires dft_lib_llm context but it is missing at $dft_lib_llm_dir" >&2
      exit 1
    fi
    docker_build_args+=(--build-context "dft_lib_llm=${dft_lib_llm_dir}")
  fi
  docker_build_args+=("$build_context")
  docker "${docker_build_args[@]}" >&2
  current_image_build_hash="$(docker image inspect -f '{{ index .Config.Labels "org.opencontainers.image.revision" }}' "$image_name" 2>/dev/null || true)"
  current_image_id="$(docker image inspect -f '{{.Id}}' "$image_name" 2>/dev/null || true)"
fi

image_identity="${current_image_id:-$image_name}"

config_hash="$(
  hash_config "$(
    cat <<EOF
port=${port}
bind_addr=${bind_addr}
code_root=${code_root}
codex_home=${codex_home}
tmux_socket=${tmux_socket}
voice_state_root=${voice_state_root}
tts_base_url=${tts_base_url}
tts_api_key=${tts_api_key}
tts_model=${tts_model}
tts_voice=${tts_voice}
audio_push_stream_force_chunks=${audio_push_stream_force_chunks}
audio_push_stream_chunk_chars=${audio_push_stream_chunk_chars}
assemblyai_api_key=${assemblyai_api_key}
dispatch_token=${dispatch_token}
image_identity=${image_identity}
EOF
  )"
)"

needs_start=1
if docker container inspect "$container_name" >/dev/null 2>&1; then
  current_hash="$(docker inspect -f '{{ index .Config.Labels "pitchai.codex.voice.config-sha" }}' "$container_name" 2>/dev/null || true)"
  current_running="$(docker inspect -f '{{.State.Running}}' "$container_name" 2>/dev/null || true)"
  if [[ "$current_hash" == "$config_hash" && "$current_running" == "true" ]]; then
    needs_start=0
  else
    docker rm -f "$container_name" >/dev/null 2>&1 || true
  fi
fi

if [[ "$needs_start" == "1" ]]; then
  docker_args=(
    run
    -d
    --name "$container_name"
    --restart unless-stopped
    --label "pitchai.codex.voice.config-sha=${config_hash}"
    --add-host "host.docker.internal:host-gateway"
    -p "${bind_addr}:${port}:8129"
    -e "PITCHAI_DISPATCH_OFFLINE=1"
    -e "PITCHAI_DB_MIGRATE_ON_START=0"
    -e "PITCHAI_PM_SCHEMA_CHECK_ON_START=0"
    -e "PITCHAI_DB_AUTOCREATE=1"
    -e "PITCHAI_UI_TERMINAL_ENABLED=1"
    -e "PITCHAI_UI_DISABLE_AUTH_LOCALHOST=1"
    -e "PITCHAI_LOCAL_AUTH_ENABLED=0"
    -e "PITCHAI_WHATSAPP_ENABLED=0"
    -e "PITCHAI_DISPATCH_TOKEN=${dispatch_token}"
    -e "PITCHAI_HOST_QUEUE_DIR=${voice_state_root}/queue"
    -e "PITCHAI_HOST_RUNS_DIR=${voice_state_root}/runs"
    -e "PITCHAI_DOCKER_HOST_VOLUME_ROOT_DIR=${voice_state_root}"
    -e "PITCHAI_DOCKER_HOST_CODEX_HOME_DIR=${codex_home}"
    -e "PITCHAI_HOST_PROC_DIR=/host_proc"
    -e "PITCHAI_TMUX_STT_PROVIDER=assemblyai"
    -e "PITCHAI_TMUX_STT_ENABLED=1"
    -e "PITCHAI_ASSEMBLYAI_API_KEY=${assemblyai_api_key}"
    -e "PITCHAI_TTS_BASE_URL=${tts_base_url}"
    -e "PITCHAI_AUDIO_PUSH_STREAM_FORCE_CHUNKS=${audio_push_stream_force_chunks}"
    -e "PITCHAI_AUDIO_PUSH_STREAM_CHUNK_CHARS=${audio_push_stream_chunk_chars}"
    -v "${voice_state_root}:${voice_state_root}"
    -v "${codex_home}:${codex_home}"
    -v "/proc:/host_proc:ro"
  )

  if [[ -n "$tts_api_key" ]]; then
    docker_args+=(-e "PITCHAI_TTS_API_KEY=${tts_api_key}")
  fi
  if [[ -n "$tts_model" ]]; then
    docker_args+=(-e "PITCHAI_TTS_MODEL=${tts_model}")
  fi
  if [[ -n "$tts_voice" ]]; then
    docker_args+=(-e "PITCHAI_TTS_VOICE=${tts_voice}")
  fi
  if [[ -d "$code_root" ]]; then
    docker_args+=(-v "${code_root}:${code_root}:ro")
  fi
  if [[ -n "$socket_dir" ]]; then
    docker_args+=(-v "${socket_dir}:${socket_dir}")
  fi

  docker_args+=("$image_name")
  docker "${docker_args[@]}" >/dev/null
fi

wait_for_health "$base_url" "$container_name"

voice_url="${base_url}/ui/tmux_voice?session=$(urlencode "$tmux_session")"
public_voice_url=""
if [[ -n "$tmux_window" ]]; then
  voice_url="${voice_url}&window=$(urlencode "$tmux_window")"
fi
if [[ -n "$tmux_socket" ]]; then
  voice_url="${voice_url}&socket=$(urlencode "$tmux_socket")"
fi
voice_url="${voice_url}&dispatch_token=$(urlencode "$dispatch_token")"
preferred_base_url="$base_url"
preferred_voice_url="$voice_url"
if [[ -n "$public_base_url" ]]; then
  public_voice_url="${public_base_url}/ui/tmux_voice?session=$(urlencode "$tmux_session")"
  if [[ -n "$tmux_window" ]]; then
    public_voice_url="${public_voice_url}&window=$(urlencode "$tmux_window")"
  fi
  if [[ -n "$tmux_socket" ]]; then
    public_voice_url="${public_voice_url}&socket=$(urlencode "$tmux_socket")"
  fi
  public_voice_url="${public_voice_url}&dispatch_token=$(urlencode "$dispatch_token")"
  preferred_base_url="$public_base_url"
  preferred_voice_url="$public_voice_url"
fi

state_tmp_file="${state_file}.tmp.$$"
cat >"$state_tmp_file" <<EOF
{
  "baseUrl": $(json_string "$base_url"),
  "voiceUrl": $(json_string "$voice_url"),
  "preferredBaseUrl": $(json_string "$preferred_base_url"),
  "preferredVoiceUrl": $(json_string "$preferred_voice_url"),
  "publicBaseUrl": $(json_string "$public_base_url"),
  "publicVoiceUrl": $(json_string "$public_voice_url"),
  "speechEndpoint": $(json_string "${base_url}/ui/api/agent/voice_push"),
  "speechToken": $(json_string "$dispatch_token"),
  "containerName": $(json_string "$container_name"),
  "imageName": $(json_string "$image_name"),
  "port": ${port},
  "tmuxSession": $(json_string "$tmux_session"),
  "tmuxWindow": $(json_string "$tmux_window"),
  "tmuxSocket": $(json_string "$tmux_socket"),
  "stateFile": $(json_string "$state_file")
}
EOF
mv "$state_tmp_file" "$state_file"

if [[ "$output_mode" == "json" ]]; then
  cat "$state_file"
  exit 0
fi

printf 'export %s=%q\n' "PITCHAI_CODEX_SPEECH_ENDPOINT" "${base_url}/ui/api/agent/voice_push"
printf 'export %s=%q\n' "PITCHAI_CODEX_SPEECH_TOKEN" "$dispatch_token"
printf 'export %s=%q\n' "PITCHAI_CODEX_SPEECH_SOURCE" "codex"
printf 'export %s=%q\n' "PITCHAI_CODEX_TMUX_SESSION" "$tmux_session"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_URL" "$preferred_voice_url"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_BASE_URL" "$preferred_base_url"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_LOCAL_URL" "$voice_url"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_LOCAL_BASE_URL" "$base_url"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_PUBLIC_URL" "$public_voice_url"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_PUBLIC_BASE_URL" "$public_base_url"
printf 'export %s=%q\n' "PITCHAI_CODEX_VOICE_WEB_STATE_FILE" "$state_file"
