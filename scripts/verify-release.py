#!/usr/bin/env python3
import argparse
import hashlib
import json
import pathlib
import subprocess
import tarfile
import tempfile


def main():
    parser = argparse.ArgumentParser(description="Verify a unionid release archive and tutorial")
    parser.add_argument("--dist", required=True, type=pathlib.Path)
    args = parser.parse_args()
    archives = sorted(args.dist.glob("unionid-v*.tar.gz"))
    if len(archives) != 1:
        raise RuntimeError(f"expected one release archive in {args.dist}, found {len(archives)}")
    archive_path = archives[0]
    checksum_path = archive_path.with_suffix(archive_path.suffix + ".sha256")
    expected, filename = checksum_path.read_text().strip().split()
    if filename != archive_path.name:
        raise RuntimeError("checksum names a different archive")
    actual = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    if actual != expected:
        raise RuntimeError("release checksum mismatch")

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
        executable = "unionid.exe" if "windows" in release["target"] else "unionid"
        binary = root / "bin" / executable
        required = [
            binary,
            root / "README.md",
            root / "docs" / "GETTING_STARTED.md",
            root / "docs" / "UPGRADING.md",
            root / "docs" / "RELEASE-v0.1.0.md",
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
