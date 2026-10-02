#!/usr/bin/env python3
"""Check current release metadata without rewriting historical version references."""

import argparse
import json
from pathlib import Path
import subprocess
import tomllib


ROOT = Path(__file__).resolve().parent.parent
PACKAGES = {"unionid", "unionid-query", "unionid-derive"}


def check(root):
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    version = manifest["package"]["version"]
    errors = []

    def expect(location, actual, expected):
        if actual != expected:
            errors.append(f"{location}: expected {expected!r}, found {actual!r}")

    for relative in ("derive/Cargo.toml", "query-macro/Cargo.toml"):
        other = tomllib.loads((root / relative).read_text())
        expect(f"{relative} package.version", other["package"]["version"], version)
    macro = tomllib.loads((root / "query-macro/Cargo.toml").read_text())
    expect("query-macro/Cargo.toml unionid requirement",
           macro["dependencies"]["unionid"]["version"], f"={version}")

    # Check tracked lockfiles only; local consumers and build directories are
    # not repository release metadata. Git supplies the authoritative file set.
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z"], cwd=root
    ).decode().split("\0")
    for relative in sorted(path for path in tracked if Path(path).name == "Cargo.lock"):
        lock = tomllib.loads((root / relative).read_text())
        for package in lock["package"]:
            if package["name"] in PACKAGES and "source" not in package:
                expect(f"{relative} {package['name']}", package["version"], version)

    contract = json.loads((root / "release/contract.json").read_text())
    expect("release/contract.json software_version", contract["software_version"], version)
    expect("release/contract.json minimum_rust_version",
           contract["minimum_rust_version"], manifest["package"]["rust-version"])
    redb = manifest["dependencies"]["redb"]
    if not isinstance(redb, str) or not redb.startswith("="):
        errors.append("Cargo.toml redb must pin an exact dependency version")
    else:
        expect("release/contract.json redb_version", contract["redb_version"], redb[1:])
    return version, errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    version, errors = check(ROOT)
    if errors:
        raise SystemExit("Release metadata differs from Cargo.toml:\n" + "\n".join(errors))
    print(f"Release metadata is consistent with unionid {version}")


if __name__ == "__main__":
    main()
