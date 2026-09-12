#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import re
import subprocess
import tarfile
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
CONSUMER_SOURCES = ROOT / "tests" / "current-consumer"


def main():
    """Package unionid and exercise its public API from an isolated Rust crate."""
    parser = argparse.ArgumentParser(
        description="Build and run an independent consumer against the packaged unionid crate"
    )
    parser.add_argument(
        "--allow-dirty",
        action="store_true",
        help="allow a development package from a dirty worktree",
    )
    args = parser.parse_args()

    cargo = (ROOT / "Cargo.toml").read_text()
    version = re.search(r'^version = "([^"]+)"$', cargo, re.MULTILINE).group(1)
    command = ["cargo", "package", "--locked", "--offline", "--no-verify"]
    if args.allow_dirty:
        command.append("--allow-dirty")
    subprocess.run(command, cwd=ROOT, check=True)
    package = ROOT / "target" / "package" / f"unionid-{version}.crate"
    if not package.is_file():
        raise RuntimeError(f"packaged crate not found: {package}")

    # The temporary consumer resolves only packages already pinned by the root
    # lockfile. Prime a fresh runner once, then keep the independent build
    # offline so it cannot silently substitute a registry copy of unionid.
    cached = subprocess.run(
        ["cargo", "fetch", "--locked", "--offline"],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if cached.returncode != 0:
        subprocess.run(["cargo", "fetch", "--locked"], cwd=ROOT, check=True)

    with tempfile.TemporaryDirectory(prefix="unionid-current-consumer-") as temporary:
        temporary = pathlib.Path(temporary)
        source = temporary / "package"
        source.mkdir()
        with tarfile.open(package, "r:gz") as archive:
            members = archive.getmembers()
            for member in members:
                path = pathlib.PurePosixPath(member.name)
                if path.is_absolute() or ".." in path.parts:
                    raise RuntimeError(f"unsafe crate member: {member.name}")
            archive.extractall(source, filter="data")
        packaged_root = source / f"unionid-{version}"
        if not (packaged_root / "Cargo.toml").is_file():
            raise RuntimeError("packaged crate has no Cargo.toml")

        consumer = temporary / "consumer"
        (consumer / "src").mkdir(parents=True)
        for source_file in CONSUMER_SOURCES.glob("*.rs"):
            (consumer / "src" / source_file.name).write_bytes(source_file.read_bytes())
        dependency = json.dumps(str(packaged_root))
        (consumer / "Cargo.toml").write_text(
            f"""[package]
name = "unionid-current-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.94"

[dependencies]
axum = "0.8"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
tokio = {{ version = "1", features = ["macros", "rt-multi-thread", "net", "time", "sync"] }}
unionid = {{ path = {dependency}, features = ["http", "http-client"] }}
"""
        )
        work = temporary / "work"
        environment = os.environ.copy()
        # Keep the consumer source isolated while reusing dependency artifacts.
        # A fresh target directory duplicates several GiB of the HTTP stack and
        # makes this release check unnecessarily expensive on developer hosts.
        environment["CARGO_TARGET_DIR"] = str(ROOT / "target")
        subprocess.run(
            [
                "cargo",
                "run",
                "--offline",
                "--manifest-path",
                str(consumer / "Cargo.toml"),
                "--",
                str(work),
            ],
            cwd=consumer,
            env=environment,
            check=True,
        )
        report = {
            "ok": True,
            "package": package.name,
            "consumer": "independent",
            "sdk_paths": ["engine", "tcp", "async_tcp", "http"],
            "storage_format": 6,
            "protocol": 2,
            "legacy_inputs": False,
            "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        }
        print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
