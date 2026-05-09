#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF' >&2
Usage:
  codex-dev-voice-public-proxy.sh --target-port <port> [--public-port <port>] [--json]

Ensures an HTTPS nginx proxy for the local codex voice web container and prints the
public base URL that remote browsers should use.
EOF
  exit 2
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
            sock.bind(("0.0.0.0", port))
        except OSError:
            return False
    return True

if preferred:
    port = int(preferred)
    if not is_free(port):
        raise SystemExit(f"requested public https port {port} is not available")
    print(port)
    raise SystemExit(0)

for port in range(18440, 18460):
    if is_free(port):
        print(port)
        raise SystemExit(0)

raise SystemExit("no free public https port found in 18440-18459")
PY
}

json_string() {
  python3 - "$1" <<'PY'
import json
import sys

print(json.dumps(sys.argv[1]))
PY
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
    print(addr)
    raise SystemExit(0)

raise SystemExit(1)
PY
}

detect_public_ipv4() {
  local explicit_ip="${PITCHAI_CODEX_VOICE_PUBLIC_LISTEN_IP:-}"
  if [[ -n "$explicit_ip" ]]; then
    printf '%s\n' "$explicit_ip"
    return 0
  fi

  if curl -4fsS --max-time 3 https://api.ipify.org 2>/dev/null; then
    printf '\n'
    return 0
  fi

  first_non_loopback_ipv4
}

output_mode="plain"
target_port=""
requested_public_port=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)
      output_mode="json"
      shift
      ;;
    --target-port)
      target_port="${2:-}"
      shift 2
      ;;
    --public-port)
      requested_public_port="${2:-}"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

if [[ -z "$target_port" ]]; then
  usage
fi

public_host="${PITCHAI_CODEX_VOICE_PUBLIC_HOSTNAME:-dispatch.pitchai.net}"
cert_dir="${PITCHAI_CODEX_VOICE_PUBLIC_CERT_DIR:-/etc/letsencrypt/live/${public_host}}"
fullchain_path="${cert_dir}/fullchain.pem"
privkey_path="${cert_dir}/privkey.pem"
conf_path="${PITCHAI_CODEX_VOICE_PUBLIC_PROXY_CONF:-/etc/nginx/conf.d/pitchai-codex-voice-public.conf}"
shared_map_conf="${PITCHAI_CODEX_VOICE_PUBLIC_PROXY_MAP_CONF:-/etc/nginx/conf.d/pitchai-codex-voice-public-map.conf}"
public_ip="$(detect_public_ipv4 | tr -d '\r\n')"

if [[ -z "$public_ip" ]]; then
  echo "codex-dev: failed to resolve a public IPv4 for the voice HTTPS proxy" >&2
  exit 1
fi

if [[ ! -r "$fullchain_path" || ! -r "$privkey_path" ]]; then
  echo "codex-dev: missing HTTPS certificate for ${public_host} at ${cert_dir}" >&2
  exit 1
fi

existing_public_port=""
existing_target_port=""
if [[ -r "$conf_path" ]]; then
  existing_public_port="$(python3 - "$conf_path" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
match = re.search(r"listen\s+\d+\.\d+\.\d+\.\d+:(\d+)\s+ssl", text)
if match:
    print(match.group(1))
PY
)"
  existing_target_port="$(python3 - "$conf_path" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
match = re.search(r"# target=https-proxy->http://127\.0\.0\.1:(\d+)", text)
if match:
    print(match.group(1))
PY
)"
fi

if [[ -z "$requested_public_port" && -n "$existing_public_port" ]]; then
  requested_public_port="$existing_public_port"
fi

if [[ -n "$existing_public_port" && "$requested_public_port" == "$existing_public_port" ]]; then
  public_port="$existing_public_port"
else
  public_port="$(pick_port "$requested_public_port")"
fi
public_base_url="https://${public_host}:${public_port}"

if [[ -n "$existing_public_port" && "$existing_public_port" == "$public_port" && "$existing_target_port" == "$target_port" ]]; then
  if [[ "$output_mode" == "json" ]]; then
    cat <<EOF
{"publicBaseUrl": $(json_string "$public_base_url")}
EOF
  else
    printf '%s\n' "$public_base_url"
  fi
  exit 0
fi

if [[ "$(id -u)" -ne 0 ]]; then
  echo "codex-dev: root is required to configure the public HTTPS voice proxy at ${conf_path}" >&2
  exit 1
fi

cat > "$shared_map_conf" <<'EOF'
# Managed by codex-dev voice public proxy (DO NOT EDIT BY HAND)
map $http_upgrade $pitchai_codex_voice_connection_upgrade {
    default upgrade;
    ''      close;
}
EOF

tmp_conf="$(mktemp)"
cat > "$tmp_conf" <<EOF
# Managed by codex-dev voice public proxy (DO NOT EDIT BY HAND)
# public_base_url=${public_base_url}
# target=https-proxy->http://127.0.0.1:${target_port}

server {
    listen 127.0.0.1:${public_port} ssl;
    listen ${public_ip}:${public_port} ssl;
    server_name ${public_host};

    access_log /var/log/nginx/pitchai-codex-voice-public.access.log;
    error_log  /var/log/nginx/pitchai-codex-voice-public.error.log;

    client_max_body_size 200m;

    ssl_certificate ${fullchain_path};
    ssl_certificate_key ${privkey_path};

    location / {
        proxy_pass http://127.0.0.1:${target_port};
        proxy_http_version 1.1;

        proxy_set_header Host              \$host;
        proxy_set_header X-Real-IP         \$remote_addr;
        proxy_set_header X-Forwarded-For   \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Forwarded-Port  \$server_port;

        proxy_set_header Upgrade    \$http_upgrade;
        proxy_set_header Connection \$pitchai_codex_voice_connection_upgrade;

        proxy_read_timeout 3600;
        proxy_send_timeout 3600;
    }
}
EOF

backup_conf=""
if [[ -f "$conf_path" ]]; then
  backup_conf="${conf_path}.bak.$(date -u +%Y%m%dT%H%M%SZ)"
  cp -p "$conf_path" "$backup_conf"
fi

install -m 0644 "$tmp_conf" "$conf_path"
rm -f "$tmp_conf"

if ! nginx -t >/dev/null 2>&1; then
  if [[ -n "$backup_conf" && -f "$backup_conf" ]]; then
    cp -p "$backup_conf" "$conf_path"
  else
    rm -f "$conf_path"
  fi
  nginx -t || true
  echo "codex-dev: nginx config test failed for the voice public proxy" >&2
  exit 1
fi

systemctl reload nginx

if [[ "$output_mode" == "json" ]]; then
  cat <<EOF
{"publicBaseUrl": $(json_string "$public_base_url")}
EOF
else
  printf '%s\n' "$public_base_url"
fi
