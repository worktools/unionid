#!/usr/bin/env python3
import ast
import pathlib
import re
import subprocess
import sys
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent.parent


def require(path, fragments):
    """Read a repository file and require every expected invariant marker."""
    text = (ROOT / path).read_text()
    for fragment in fragments:
        if fragment not in text:
            raise RuntimeError(f"{path} is missing {fragment!r}")
    return text


def main():
    """Verify the static security and packaging invariants of the deployment."""
    config = require(
        "deploy/envoy/envoy.yaml",
        [
            "require_client_certificate: true",
            "tls_minimum_protocol_version: TLSv1_3",
            "filename: /certs/ca.crl",
            "match_typed_subject_alt_names:",
            'exact: "spiffe://unionid.dev/client"',
            "transport_socket_connect_timeout: 5s",
            "envoy.filters.network.connection_limit",
            "max_connections: 64",
            "per_connection_buffer_limit_bytes: 1048576",
            "idle_timeout: 35s",
            'client_subject: "%DOWNSTREAM_PEER_SUBJECT%"',
            'client_certificate_sha256: "%DOWNSTREAM_PEER_FINGERPRINT_256%"',
            "address: 127.0.0.1",
            "port_value: 7878",
        ],
    )
    forbidden_log_fields = {
        "query",
        "params",
        "row",
        "idempotency_key",
        "operation_id",
    }
    configured_fields = {
        line.strip().split(":", 1)[0]
        for line in config.splitlines()
        if ":" in line
    }
    exposed = sorted(forbidden_log_fields & configured_fields)
    if exposed:
        raise RuntimeError(f"Envoy access logs expose sensitive fields: {exposed}")
    compose = require(
        "deploy/envoy/compose.yaml",
        [
            "envoyproxy/envoy:v1.39.1",
            "read_only: true",
            "no-new-privileges:true",
            "network_mode: host",
            "UNIONID_ENVOY_UID",
            "UNIONID_ENVOY_GID",
        ],
    )
    if not re.search(r"envoyproxy/envoy:v\d+\.\d+\.\d+", compose):
        raise RuntimeError("Envoy image must use an exact release tag")
    require(
        "deploy/envoy/verify.sh",
        [
            "probe.py\" reject",
            "probe.py\" reject-identity",
            "--read-only",
            "kill -TERM",
            "check --db",
            "unionid-development-client",
            'UNIONID_CERT_DIR/ca.key',
            'UNIONID_ENVOY_UID="$(id -u)"',
        ],
    )
    verify = (ROOT / "deploy/envoy/verify.sh").read_text()
    if verify.count('probe.py" read "${probe_args[@]}" >/dev/null') != 2:
        raise RuntimeError("each rejected identity must be followed by a healthy authorized probe")
    probe_path = ROOT / "deploy/envoy/probe.py"
    ast.parse(probe_path.read_text(), filename=str(probe_path))
    render_path = ROOT / "deploy/envoy/render.py"
    ast.parse(render_path.read_text(), filename=str(render_path))
    template = (ROOT / "deploy/envoy/envoy.yaml.template").read_text()
    expected = (
        template.replace("__LISTEN_ADDRESS__", "127.0.0.1")
        .replace("__LISTEN_PORT__", "8443")
        .replace("__CLIENT_URI_SAN_JSON__", '"spiffe://unionid.dev/client"')
    )
    if config != expected:
        raise RuntimeError("checked-in Envoy config does not match the safe rendered default")
    with tempfile.TemporaryDirectory(prefix="unionid-envoy-render-") as temporary:
        output = pathlib.Path(temporary) / "envoy.yaml"
        subprocess.run(
            [sys.executable, render_path, "--output", output],
            cwd=ROOT,
            check=True,
        )
        if output.read_text() != config:
            raise RuntimeError("renderer does not reproduce the safe checked-in config")
        rejected = subprocess.run(
            [
                sys.executable,
                render_path,
                "--listen-address",
                "0.0.0.0",
                "--output",
                output,
            ],
            cwd=ROOT,
            text=True,
            capture_output=True,
        )
        if rejected.returncode == 0:
            raise RuntimeError("renderer accepts an unspecified public listen address")
        rejected_san = subprocess.run(
            [
                sys.executable,
                render_path,
                "--client-uri-san",
                "spiffe://unionid.dev/client\ninjected: true",
                "--output",
                output,
            ],
            cwd=ROOT,
            text=True,
            capture_output=True,
        )
        if rejected_san.returncode == 0:
            raise RuntimeError("renderer accepts an unsafe client URI SAN")
    docs = require(
        "docs/DEPLOYMENT.md",
        [
            "## 中文说明",
            "## English Description",
            "E_READ_ONLY",
            "receipt",
            "SIGTERM",
            "25 秒",
            "25 seconds",
            "127.0.0.1:7878",
            "forward_client_cert_details: SANITIZE_SET",
            "--client-uri-san",
            "ca.crl",
            "错误 SAN",
            "wrong-SAN",
        ],
    )
    if docs.count("UNIONID_CERT_DIR") < 2:
        raise RuntimeError("deployment guide must include copy-pasteable certificate setup")
    print("Envoy deployment reference is internally consistent")


if __name__ == "__main__":
    main()
