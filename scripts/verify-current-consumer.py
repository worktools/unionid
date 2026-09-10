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
CONSUMER_SOURCE = ROOT / "tests" / "current-consumer" / "main.rs"


def main():
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
        (consumer / "src" / "main.rs").write_bytes(CONSUMER_SOURCE.read_bytes())
        dependency = json.dumps(str(packaged_root))
        (consumer / "Cargo.toml").write_text(
            f"""[package]
name = "unionid-current-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.94"

[dependencies]
serde = {{ version = "1", features = ["derive"] }}
unionid = {{ path = {dependency} }}
"""
        )
        work = temporary / "work"
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(temporary / "target")
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
            "storage_format": 6,
            "protocol": 2,
            "legacy_inputs": False,
            "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        }
        print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
