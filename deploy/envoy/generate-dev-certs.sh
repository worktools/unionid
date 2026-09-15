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

openssl req -newkey rsa:3072 -nodes -sha256 \
  -subj "/CN=unionid-unauthorized-client" \
  -addext "subjectAltName=URI:spiffe://unionid.dev/other" \
  -keyout "$client_dir/other-client.key" \
  -out "$authority_dir/other-client.csr" >/dev/null 2>&1
cat >"$authority_dir/other-client.ext" <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=clientAuth
subjectAltName=URI:spiffe://unionid.dev/other
EOF
openssl x509 -req -sha256 -days 2 \
  -in "$authority_dir/other-client.csr" \
  -CA "$authority_dir/ca.crt" \
  -CAkey "$authority_dir/ca.key" \
  -CAcreateserial \
  -extfile "$authority_dir/other-client.ext" \
  -out "$client_dir/other-client.crt" >/dev/null 2>&1

cp "$authority_dir/ca.crt" "$gateway_dir/ca.crt"
cp "$authority_dir/ca.crt" "$client_dir/ca.crt"
rm "$authority_dir/server.csr" "$authority_dir/server.ext" \
  "$authority_dir/client.csr" "$authority_dir/client.ext" \
  "$authority_dir/other-client.csr" "$authority_dir/other-client.ext" \
  "$authority_dir/ca.srl"
openssl verify -CAfile "$authority_dir/ca.crt" \
  "$gateway_dir/server.crt" "$client_dir/client.crt" \
  "$client_dir/other-client.crt" >/dev/null

touch "$authority_dir/index.txt"
printf '1000\n' >"$authority_dir/serial"
printf '1000\n' >"$authority_dir/crlnumber"
mkdir "$authority_dir/newcerts"
cat >"$authority_dir/ca.cnf" <<'EOF'
[ca]
default_ca = unionid_ca

[unionid_ca]
dir = .
database = $dir/index.txt
new_certs_dir = $dir/newcerts
certificate = $dir/ca.crt
private_key = $dir/ca.key
serial = $dir/serial
crlnumber = $dir/crlnumber
default_md = sha256
default_crl_days = 2
policy = unionid_policy

[unionid_policy]
commonName = supplied
EOF
(
  cd "$authority_dir"
  openssl ca -batch -gencrl -config ca.cnf -out "$gateway_dir/ca.crl" \
    >/dev/null 2>&1
)
openssl crl -in "$gateway_dir/ca.crl" -noout -verify \
  -CAfile "$authority_dir/ca.crt" >/dev/null 2>&1

# The generated material is short-lived and deleted by verify.sh. Compose runs
# Envoy with this host owner's UID/GID so the bind mount needs no broad access.
chmod 0700 "$gateway_dir"
chmod 0444 "$gateway_dir/ca.crt" "$gateway_dir/ca.crl" "$gateway_dir/server.crt" \
  "$client_dir/ca.crt" "$client_dir/client.crt" "$client_dir/other-client.crt"
chmod 0400 "$gateway_dir/server.key" "$authority_dir/ca.key" \
  "$client_dir/client.key" "$client_dir/other-client.key"
echo "generated two-day development certificates in $certificate_dir"
