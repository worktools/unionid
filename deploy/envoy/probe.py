#!/usr/bin/env python3
import argparse
import json
import socket
import ssl
import sys


def request(args, query, with_client_certificate=True):
    """Send one bounded JSON Lines request through the TLS gateway."""
    context = ssl.create_default_context(ssl.Purpose.SERVER_AUTH, cafile=args.ca)
    context.minimum_version = ssl.TLSVersion.TLSv1_3
    if with_client_certificate:
        context.load_cert_chain(args.cert, args.key)
    payload = {
        "version": 1,
        "request_id": args.mode,
        "query": query,
        "params": {},
    }
    with socket.create_connection((args.host, args.port), timeout=3) as connection:
        with context.wrap_socket(connection, server_hostname="localhost") as secure:
            secure.settimeout(3)
            secure.sendall(json.dumps(payload, separators=(",", ":")).encode() + b"\n")
            response = secure.makefile("rb").readline(16 * 1024 * 1024 + 1)
    if not response or len(response) > 16 * 1024 * 1024:
        raise RuntimeError("gateway returned no bounded response")
    return json.loads(response)


def validate_response_identity(response, request_id):
    """Require the protocol version and request identity used by the probe."""
    if response.get("version") != 1 or response.get("request_id") != request_id:
        raise RuntimeError(f"response identity mismatch: {response!r}")


def main():
    """Run one authenticated or expected-rejection gateway probe."""
    parser = argparse.ArgumentParser(description="Probe the Unionid Envoy mTLS reference")
    parser.add_argument(
        "mode", choices=("read", "reject", "reject-identity", "read-only")
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8443)
    parser.add_argument("--ca", required=True)
    parser.add_argument("--cert")
    parser.add_argument("--key")
    args = parser.parse_args()
    if args.mode != "reject" and (not args.cert or not args.key):
        parser.error("--cert and --key are required for authenticated probes")

    if args.mode in {"reject", "reject-identity"}:
        try:
            response = request(
                args,
                "from tasks",
                with_client_certificate=args.mode == "reject-identity",
            )
        except (ConnectionError, OSError, ssl.SSLError, TimeoutError):
            print(json.dumps({"ok": True, f"{args.mode}_rejected": True}))
            return
        rejected_identity = (
            "without a certificate"
            if args.mode == "reject"
            else "with an unauthorized identity"
        )
        raise RuntimeError(f"gateway accepted a client {rejected_identity}: {response!r}")

    if args.mode == "read":
        response = request(args, "from tasks\nsort id")
        validate_response_identity(response, args.mode)
        if not response.get("ok") or len(response.get("rows", [])) != 1:
            raise RuntimeError(f"unexpected authenticated read response: {response!r}")
        print(json.dumps({"ok": True, "authenticated_read": True}))
        return

    response = request(args, 'insert tasks {id = 2, title = "blocked"}')
    validate_response_identity(response, args.mode)
    if response.get("ok") or response.get("error", {}).get("code") != "E_READ_ONLY":
        raise RuntimeError(f"read-only mutation was not rejected: {response!r}")
    print(json.dumps({"ok": True, "read_only_rejected": True}))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"gateway probe failed: {error}", file=sys.stderr)
        raise SystemExit(1)
