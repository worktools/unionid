#!/usr/bin/env python3
import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import tarfile
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
CONTRACT_PATH = ROOT / "release" / "contract.json"


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
    parser = argparse.ArgumentParser(description="Verify a unionid release archive and tutorial")
    parser.add_argument("--dist", required=True, type=pathlib.Path)
    parser.add_argument(
        "--expected-sha256",
        required=True,
        help="archive SHA-256 supplied by the trusted build or release environment",
    )
    args = parser.parse_args()
    trusted_digest = args.expected_sha256.lower()
    if not re.fullmatch(r"[0-9a-f]{64}", trusted_digest):
        raise RuntimeError("--expected-sha256 must be exactly 64 hexadecimal characters")
    archives = sorted(args.dist.glob("unionid-v*.tar.gz"))
    if len(archives) != 1:
        raise RuntimeError(f"expected one release archive in {args.dist}, found {len(archives)}")
    archive_path = archives[0]
    checksum_path = archive_path.with_suffix(archive_path.suffix + ".sha256")
    expected, filename = checksum_path.read_text().strip().split()
    if filename != archive_path.name:
        raise RuntimeError("checksum names a different archive")
    actual = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    if actual != trusted_digest:
        raise RuntimeError("release archive does not match the trusted SHA-256")
    if expected != trusted_digest:
        raise RuntimeError("release checksum mismatch")

    contract = json.loads(CONTRACT_PATH.read_text())
    expected_contract = {
        "contract_schema",
        "software_version",
        "minimum_rust_version",
        "redb_version",
        "capabilities",
    }
    if set(contract) != expected_contract or contract["contract_schema"] != 1:
        raise RuntimeError("unsupported or malformed trusted release contract")
    capabilities = contract["capabilities"]

    with tempfile.TemporaryDirectory(prefix="unionid-release-") as temporary:
        temporary = pathlib.Path(temporary)
        with tarfile.open(archive_path, "r:gz") as archive:
            members = archive.getmembers()
            roots = {pathlib.PurePosixPath(member.name).parts[0] for member in members}
            if len(roots) != 1:
                raise RuntimeError("archive must contain exactly one package root")
            for member in members:
                path = pathlib.PurePosixPath(member.name)
                if path.is_absolute() or ".." in path.parts or not member.isfile():
                    raise RuntimeError(f"unsafe or unexpected archive member: {member.name}")
            archive.extractall(temporary, filter="data")
        root = temporary / roots.pop()
        release = json.loads((root / "RELEASE.json").read_text())
        if release["name"] != "unionid":
            raise RuntimeError("unexpected release name")
        expected_root = f"unionid-v{release['version']}-{release['target']}"
        if root.name != expected_root:
            raise RuntimeError("archive root does not match release version and target")
        packaged_contract = json.loads((root / "release" / "contract.json").read_text())
        if packaged_contract != contract:
            raise RuntimeError("packaged release contract does not match the trusted source contract")
        if release["version"] != contract["software_version"]:
            raise RuntimeError("release version does not match the release contract")
        if release["release_contract_schema"] != contract["contract_schema"]:
            raise RuntimeError("RELEASE.json contract schema does not match the release contract")
        if release["minimum_rust_version"] != contract["minimum_rust_version"]:
            raise RuntimeError("RELEASE.json Rust version does not match the release contract")
        if release["redb"] != contract["redb_version"]:
            raise RuntimeError("RELEASE.json redb version does not match the release contract")
        if not re.fullmatch(r"[0-9a-f]{40}", release["source_commit"]):
            raise RuntimeError("RELEASE.json source commit is not canonical")
        if release["source_dirty"] is not False:
            raise RuntimeError("release archive was built from a dirty worktree")
        expected_release_notes = f"docs/RELEASE-v{release['version']}.md"
        if release["release_notes"] != expected_release_notes:
            raise RuntimeError("RELEASE.json does not select version-specific release notes")
        release_capabilities = {key: release.get(key) for key in capabilities}
        if release_capabilities != capabilities:
            raise RuntimeError(
                f"release manifest does not match {CONTRACT_PATH.relative_to(ROOT)}: "
                f"expected {capabilities}, got {release_capabilities}"
            )
        executable = "unionid.exe" if "windows" in release["target"] else "unionid"
        binary = root / "bin" / executable
        binary_report = json.loads(
            subprocess.check_output(
                [binary, "version", "--format", "json"], text=True
            )
        )
        binary_capabilities = reported_capabilities(binary_report)
        if binary_capabilities != capabilities:
            raise RuntimeError(
                f"release binary capability mismatch: expected {capabilities}, "
                f"got {binary_capabilities}"
            )
        if binary_report["software_version"] != release["version"]:
            raise RuntimeError("release binary version does not match RELEASE.json")
        if binary_report["target"] != release["target"]:
            raise RuntimeError("release binary target does not match RELEASE.json")
        required = [
            binary,
            root / "README.md",
            root / "docs" / "GETTING_STARTED.md",
            root / "docs" / "HTTP.md",
            root / "docs" / "MIGRATIONS.md",
            root / "docs" / "PROTOCOL.md",
            root / "docs" / "UPGRADING.md",
            root / release["release_notes"],
            root / "release" / "contract.json",
            root / "examples" / "tasks.uid",
            root / "examples" / "todolist.rs",
            root / "tutorial" / "validate.py",
            root / "tutorial" / "01_setup.uid",
            root / "tutorial" / "04_reopen.uid",
        ]
        if not all(path.is_file() for path in required):
            raise RuntimeError("release layout is incomplete")
        version = subprocess.check_output([binary, "--version"], text=True).strip()
        if version != f"unionid {release['version']}":
            raise RuntimeError(f"unexpected version output: {version}")
        help_text = subprocess.check_output([binary, "cli", "--help"], text=True)
        for expected_help in ["--history <PATH>", ".schema", ".storage"]:
            if expected_help not in help_text:
                raise RuntimeError(f"CLI help is missing {expected_help}")
        work = temporary / "fresh-tutorial"
        result = subprocess.run(
            [
                "python3",
                root / "tutorial" / "validate.py",
                "--binary",
                binary,
                "--work-dir",
                work,
                "--tutorial-dir",
                root / "tutorial",
            ],
            text=True,
            capture_output=True,
        )
        if result.returncode:
            raise RuntimeError(result.stderr)
        print(json.dumps({"ok": True, "archive": archive_path.name, "sha256": actual, "tutorial": json.loads(result.stdout)}))


if __name__ == "__main__":
    main()
