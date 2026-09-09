#!/usr/bin/env python3
import argparse
import gzip
import hashlib
import io
import json
import pathlib
import re
import subprocess
import tarfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
CONTRACT_PATH = ROOT / "release" / "contract.json"


def command(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def add_bytes(archive, name, data, mode=0o644):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    archive.addfile(info, io.BytesIO(data))


def reported_capabilities(report):
    storage = report["current_storage"]
    return {
        "version_report_schema": report["schema_version"],
        "storage_format": storage["format"],
        "storage_formats_readable": report["readable_storage_formats"],
        "catalog_codec": storage["catalog_codec"],
        "value_codec": storage["value_codec"],
        "index_key_codec": storage["index_key_codec"],
        "migration_codec": storage["migration_codec"],
        "receipt_codec": storage["receipt_codec"],
        "maintenance_codec": storage["maintenance_codec"],
        "backup_format": storage["backup_codec"],
        "backup_formats_readable": report["readable_backup_formats"],
        "protocol": max(report["protocol_versions"]),
        "protocol_versions": report["protocol_versions"],
        "stream_protocol_versions": report["stream_protocol_versions"],
    }


def main():
    parser = argparse.ArgumentParser(description="Build a deterministic unionid release archive")
    parser.add_argument("--output", type=pathlib.Path, default=ROOT / "dist")
    parser.add_argument("--target")
    parser.add_argument("--expect-version")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()

    cargo = (ROOT / "Cargo.toml").read_text()
    version = re.search(r'^version = "([^"]+)"$', cargo, re.MULTILINE).group(1)
    redb = re.search(r'^redb = "=([^"]+)"$', cargo, re.MULTILINE).group(1)
    if args.expect_version and version != args.expect_version:
        raise RuntimeError(
            f"Cargo version {version} does not match requested release {args.expect_version}"
        )
    host = next(
        line.split(":", 1)[1].strip()
        for line in command("rustc", "-vV").splitlines()
        if line.startswith("host:")
    )
    target = args.target or host
    if not args.skip_build:
        subprocess.run(
            ["cargo", "build", "--release", "--locked", "--target", target],
            cwd=ROOT,
            check=True,
        )
    executable = "unionid.exe" if "windows" in target else "unionid"
    binary = ROOT / "target" / target / "release" / executable
    if not binary.is_file():
        raise RuntimeError(f"release binary not found: {binary}")
    binary_version = json.loads(
        subprocess.check_output(
            [binary, "version", "--format", "json"], cwd=ROOT, text=True
        )
    )
    if binary_version["software_version"] != version:
        raise RuntimeError("release binary version does not match Cargo.toml")
    if binary_version["target"] != target:
        raise RuntimeError("release binary target does not match the requested target")
    capabilities = reported_capabilities(binary_version)
    contract = json.loads(CONTRACT_PATH.read_text())
    if capabilities != contract:
        raise RuntimeError(
            f"release binary does not match {CONTRACT_PATH.relative_to(ROOT)}: "
            f"expected {contract}, got {capabilities}"
        )

    package = f"unionid-v{version}-{target}"
    files = {
        f"{package}/bin/{executable}": (binary.read_bytes(), 0o755),
        f"{package}/README.md": ((ROOT / "README.md").read_bytes(), 0o644),
        f"{package}/tutorial/validate.py": ((ROOT / "scripts/validate-tutorial.py").read_bytes(), 0o755),
    }
    for source in sorted((ROOT / "docs").rglob("*")):
        if source.is_file():
            relative = source.relative_to(ROOT).as_posix()
            files[f"{package}/{relative}"] = (source.read_bytes(), 0o644)
    for source in sorted((ROOT / "examples").rglob("*")):
        if source.is_file():
            relative = source.relative_to(ROOT).as_posix()
            files[f"{package}/{relative}"] = (source.read_bytes(), 0o644)
    for source in sorted((ROOT / "examples/getting-started").glob("*.uid")):
        files[f"{package}/tutorial/{source.name}"] = (source.read_bytes(), 0o644)
    release = {
        "name": "unionid",
        "version": version,
        "target": target,
        "rust_toolchain": command("rustc", "--version"),
        "redb": redb,
        **capabilities,
    }
    files[f"{package}/RELEASE.json"] = (
        (json.dumps(release, indent=2, sort_keys=True) + "\n").encode(),
        0o644,
    )

    args.output.mkdir(parents=True, exist_ok=True)
    archive_path = args.output / f"{package}.tar.gz"
    with archive_path.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as archive:
                for name, (data, mode) in sorted(files.items()):
                    add_bytes(archive, name, data, mode)
    digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    checksum = archive_path.with_suffix(archive_path.suffix + ".sha256")
    checksum.write_text(f"{digest}  {archive_path.name}\n")
    print(json.dumps({"archive": str(archive_path), "checksum": str(checksum), **release}))


if __name__ == "__main__":
    main()
