#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <empty-output-directory>" >&2
  exit 2
fi

certificate_dir="$1"
if [[ -e "$certificate_dir" ]] && [[ -n "$(find "$certificate_dir" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
  echo "certificate output directory must be empty: $certificate_dir" >&2
  exit 2
fi
mkdir -p "$certificate_dir"
certificate_dir="$(cd "$certificate_dir" && pwd)"
authority_dir="$certificate_dir/authority"
gateway_dir="$certificate_dir/gateway"
client_dir="$certificate_dir/client"
mkdir -p "$authority_dir" "$gateway_dir" "$client_dir"

umask 077
openssl req -x509 -newkey rsa:3072 -nodes -sha256 -days 2 \
  -subj "/CN=unionid-development-ca" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign" \
  -keyout "$authority_dir/ca.key" \
  -out "$authority_dir/ca.crt" >/dev/null 2>&1

openssl req -newkey rsa:3072 -nodes -sha256 \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
  -keyout "$gateway_dir/server.key" \
  -out "$authority_dir/server.csr" >/dev/null 2>&1
cat >"$authority_dir/server.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1
EOF
openssl x509 -req -sha256 -days 2 \
  -in "$authority_dir/server.csr" \
  -CA "$authority_dir/ca.crt" \
  -CAkey "$authority_dir/ca.key" \
  -CAcreateserial \
  -extfile "$authority_dir/server.ext" \
  -out "$gateway_dir/server.crt" >/dev/null 2>&1

openssl req -newkey rsa:3072 -nodes -sha256 \
  -subj "/CN=unionid-development-client" \
  -addext "subjectAltName=URI:spiffe://unionid.dev/client" \
  -keyout "$client_dir/client.key" \
  -out "$authority_dir/client.csr" >/dev/null 2>&1
cat >"$authority_dir/client.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
subjectAltName=URI:spiffe://unionid.dev/client
EOF
openssl x509 -req -sha256 -days 2 \
  -in "$authority_dir/client.csr" \
  -CA "$authority_dir/ca.crt" \
  -CAkey "$authority_dir/ca.key" \
  -CAcreateserial \
  -extfile "$authority_dir/client.ext" \
  -out "$client_dir/client.crt" >/dev/null 2>&1

cp "$authority_dir/ca.crt" "$gateway_dir/ca.crt"
cp "$authority_dir/ca.crt" "$client_dir/ca.crt"
rm "$authority_dir/server.csr" "$authority_dir/server.ext" \
  "$authority_dir/client.csr" "$authority_dir/client.ext" \
  "$authority_dir/ca.srl"
openssl verify -CAfile "$authority_dir/ca.crt" \
  "$gateway_dir/server.crt" "$client_dir/client.crt" >/dev/null

# The generated material is short-lived and deleted by verify.sh. Compose runs
# Envoy with this host owner's UID/GID so the bind mount needs no broad access.
chmod 0700 "$gateway_dir"
chmod 0444 "$gateway_dir/ca.crt" "$gateway_dir/server.crt" \
  "$client_dir/ca.crt" "$client_dir/client.crt"
chmod 0400 "$gateway_dir/server.key" "$authority_dir/ca.key" \
  "$client_dir/client.key"
echo "generated two-day development certificates in $certificate_dir"
