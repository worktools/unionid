#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "$0")/../.." && pwd)"
unionid_binary="${1:-$repository_root/target/debug/unionid}"
gateway_port="${UNIONID_GATEWAY_PORT:-8443}"
temporary="$(mktemp -d "${TMPDIR:-/tmp}/unionid-envoy.XXXXXX")"
certificate_dir="$temporary/certs"
database="$temporary/gateway.redb"
server_log="$temporary/unionid.log"
compose_log="$temporary/envoy.log"
envoy_config="$temporary/envoy.yaml"
server_pid=""
export COMPOSE_PROJECT_NAME="unionid-gateway-$$"
export UNIONID_CERT_DIR="$certificate_dir/gateway"
export UNIONID_GATEWAY_PORT="$gateway_port"
export UNIONID_ENVOY_CONFIG="$envoy_config"
export UNIONID_ENVOY_UID="$(id -u)"
export UNIONID_ENVOY_GID="$(id -g)"

compose() {
  docker compose --project-directory "$repository_root/deploy/envoy" \
    -f "$repository_root/deploy/envoy/compose.yaml" "$@"
}

stop_server() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill -TERM "$server_pid"
    wait "$server_pid"
  fi
  server_pid=""
}

cleanup() {
  stop_server || true
  compose down --remove-orphans >"$compose_log" 2>&1 || true
  rm -rf "$temporary"
}
trap cleanup EXIT

for command in docker openssl python3; do
  if ! command -v "$command" >/dev/null; then
    echo "required command is unavailable: $command" >&2
    exit 2
  fi
done
if ! docker compose version >/dev/null 2>&1; then
  echo "docker compose v2 is required" >&2
  exit 2
fi
if [[ ! -x "$unionid_binary" ]]; then
  cargo build --locked --bin unionid --manifest-path "$repository_root/Cargo.toml"
fi

python3 - "$gateway_port" <<'PY'
import socket
import sys

for port in (7878, int(sys.argv[1])):
    with socket.socket() as probe:
        probe.settimeout(0.1)
        if probe.connect_ex(("127.0.0.1", port)) == 0:
            raise SystemExit(f"verification port is already in use: {port}")
PY

"$repository_root/deploy/envoy/generate-dev-certs.sh" "$certificate_dir"
if [[ -e "$UNIONID_CERT_DIR/ca.key" || -e "$UNIONID_CERT_DIR/client.key" ]]; then
  echo "gateway certificate directory contains a CA or client private key" >&2
  exit 1
fi
python3 "$repository_root/deploy/envoy/render.py" \
  --listen-port "$gateway_port" --output "$envoy_config"
setup='create table tasks (id int, title text)
insert tasks {id = 1, title = "gateway"}'
"$unionid_binary" run --db "$database" --query "$setup" >/dev/null

"$unionid_binary" server --db "$database" --addr 127.0.0.1:7878 \
  >"$server_log" 2>&1 &
server_pid=$!
compose up --detach --pull always

probe_args=(
  --port "$gateway_port"
  --ca "$certificate_dir/client/ca.crt"
  --cert "$certificate_dir/client/client.crt"
  --key "$certificate_dir/client/client.key"
)
for _ in $(seq 1 30); do
  if python3 "$repository_root/deploy/envoy/probe.py" read "${probe_args[@]}"; then
    ready=true
    break
  fi
  sleep 1
done
if [[ "${ready:-false}" != true ]]; then
  compose logs gateway >&2 || true
  cat "$server_log" >&2
  exit 1
fi
python3 "$repository_root/deploy/envoy/probe.py" reject \
  --port "$gateway_port" --ca "$certificate_dir/client/ca.crt"

stop_server
"$unionid_binary" check --db "$database" >/dev/null
"$unionid_binary" server --db "$database" --read-only --addr 127.0.0.1:7878 \
  >>"$server_log" 2>&1 &
server_pid=$!
for _ in $(seq 1 30); do
  if python3 "$repository_root/deploy/envoy/probe.py" read "${probe_args[@]}"; then
    read_only_ready=true
    break
  fi
  sleep 1
done
if [[ "${read_only_ready:-false}" != true ]]; then
  compose logs gateway >&2 || true
  cat "$server_log" >&2
  exit 1
fi
python3 "$repository_root/deploy/envoy/probe.py" read-only "${probe_args[@]}"
stop_server
"$unionid_binary" check --db "$database" >/dev/null
compose logs gateway >"$compose_log"
if ! grep -q 'unionid-development-client' "$compose_log"; then
  echo "Envoy audit log did not contain the authenticated client subject" >&2
  cat "$compose_log" >&2
  exit 1
fi

echo '{"ok":true,"mtls":true,"anonymous_rejected":true,"read_only":true,"graceful_shutdown":true,"audit_identity":true}'
