#!/usr/bin/env python3
"""Check current release metadata without rewriting historical version references."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import tomllib


ROOT = Path(__file__).resolve().parent.parent
PACKAGES = {"unionid", "unionid-query", "unionid-derive"}


def tracked_lockfiles(root):
    # Local consumers and build directories are not release metadata.
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z"], cwd=root
    ).decode().split("\0")
    return sorted(path for path in tracked if Path(path).name == "Cargo.lock")


def replace_version(block, version):
    updated, count = re.subn(
        r'(?m)^(version\s*=\s*)"[^"\n]*"',
        lambda match: match[1] + json.dumps(version), block,
    )
    if count != 1:
        raise ValueError("expected exactly one quoted version in metadata block")
    return updated


def synchronized_files(root):
    """Plan software-version edits; leave capabilities and historical text alone."""
    version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    planned = {}
    for relative in ("derive/Cargo.toml", "query-macro/Cargo.toml"):
        original = (root / relative).read_text()
        # Split only at top-level table headers, preserving comments/layout.
        blocks = re.split(r'(?m)(?=^\[)', original)
        for index, block in enumerate(blocks):
            if block.startswith("[package]\n"):
                blocks[index] = replace_version(block, version)
            elif relative == "query-macro/Cargo.toml" and block.startswith("[dependencies]\n"):
                blocks[index], count = re.subn(
                    r'(?m)^(unionid\s*=\s*\{\s*version\s*=\s*)"[^"\n]*"',
                    lambda match: match[1] + json.dumps(f"={version}"), block,
                )
                if count != 1:
                    raise ValueError("expected one inline unionid dependency in query macro")
        updated = "".join(blocks)
        parsed = tomllib.loads(updated)
        if parsed["package"]["version"] != version:
            raise ValueError(f"could not synchronize {relative}")
        if relative.startswith("query-macro/") and parsed["dependencies"]["unionid"]["version"] != f"={version}":
            raise ValueError("could not synchronize query macro dependency")
        if updated != original:
            planned[relative] = updated

    for relative in tracked_lockfiles(root):
        original = (root / relative).read_text()
        blocks = re.split(r'(?m)(?=^\[\[package\]\])', original)
        for index, block in enumerate(blocks):
            if block.startswith("[[package]]"):
                package = tomllib.loads(block)["package"][0]
                if package["name"] in PACKAGES and "source" not in package:
                    blocks[index] = replace_version(block, version)
        updated = "".join(blocks)
        tomllib.loads(updated)
        if updated != original:
            planned[relative] = updated

    relative = "release/contract.json"
    original = (root / relative).read_text()
    contract = json.loads(original)
    if contract["software_version"] != version:
        # Preserve all capability values and the established JSON layout.
        updated, count = re.subn(
            r'(?m)^(\s*"software_version"\s*:\s*)"[^"\n]*"',
            lambda match: match[1] + json.dumps(version), original,
        )
        if count != 1:
            raise ValueError("expected exactly one software_version in release contract")
        expected = dict(contract, software_version=version)
        if json.loads(updated) != expected:
            raise ValueError("synchronization would change other contract fields")
        planned[relative] = updated
    return planned


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

    for relative in tracked_lockfiles(root):
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
    parser.add_argument(
        "--sync-version", action="store_true",
        help="sync software versions from root Cargo.toml; keep capabilities and history unchanged",
    )
    args = parser.parse_args()
    if args.sync_version:
        # Parse and validate every edit before writing any file. These are
        # reviewable working-tree edits, not a database or release transaction.
        for relative, content in synchronized_files(ROOT).items():
            (ROOT / relative).write_text(content)
            print(f"Synchronized {relative}")
    version, errors = check(ROOT)
    if errors:
        raise SystemExit("Release metadata differs from Cargo.toml:\n" + "\n".join(errors))
    print(f"Release metadata is consistent with unionid {version}")


if __name__ == "__main__":
    main()
