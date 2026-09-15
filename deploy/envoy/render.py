#!/usr/bin/env python3
import argparse
import ipaddress
import pathlib


ROOT = pathlib.Path(__file__).resolve().parent
TEMPLATE = ROOT / "envoy.yaml.template"


def port(value):
    parsed = int(value)
    if not 1 <= parsed <= 65535:
        raise argparse.ArgumentTypeError("port must be between 1 and 65535")
    return parsed


def main():
    parser = argparse.ArgumentParser(description="Render the Unionid Envoy reference")
    parser.add_argument("--listen-address", default="127.0.0.1")
    parser.add_argument("--listen-port", type=port, default=8443)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    address = ipaddress.ip_address(args.listen_address)
    if address.is_unspecified:
        parser.error("choose an explicit loopback or controlled-network address, not 0.0.0.0/::")
    rendered = (
        TEMPLATE.read_text()
        .replace("__LISTEN_ADDRESS__", str(address))
        .replace("__LISTEN_PORT__", str(args.listen_port))
    )
    if "__" in rendered:
        raise RuntimeError("unresolved Envoy configuration placeholder")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(rendered)


if __name__ == "__main__":
    main()
