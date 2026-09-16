#!/usr/bin/env python3
"""Verify the published v0.5 native package against a v0.6 candidate binary."""

import argparse
import hashlib
import json
import pathlib
import socket
import subprocess
import tarfile
import tempfile
import time


SOURCE = """type State =
  Pending
  | Running {
    worker text,
    attempt int,
  }

type Task = {
  id int,
  title text,
  state State,
}

table tasks Task
  key id

insert tasks {id = 1, title = "from v0.5", state = Running {worker = "worker-a", attempt = 2}}
insert tasks {id = 2, title = "pending", state = Pending}

from tasks
sort id
select {id, title, state}
"""

QUERY = "from tasks\nsort id\nselect {id, title, state}"


def run(command, cwd=None):
    result = subprocess.run(command, cwd=cwd, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(
            f"command failed ({' '.join(map(str, command))}):\n{result.stderr}"
        )
    return json.loads(result.stdout)


def require_version(binary, expected):
    report = run([binary, "version", "--format", "json"])
    if report.get("software_version") != expected:
        raise RuntimeError(f"expected unionid {expected}, got {report}")
    return report


def verify_checksum(archive, checksum):
    fields = checksum.read_text().strip().split()
    if len(fields) != 2 or pathlib.Path(fields[1]).name != archive.name:
        raise RuntimeError("v0.5 checksum file does not name the supplied archive")
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if fields[0].lower() != actual:
        raise RuntimeError("v0.5 archive SHA-256 does not match its published checksum")
    return actual


def extract_previous_binary(archive, destination):
    with tarfile.open(archive, "r:gz") as source:
        roots = {pathlib.PurePosixPath(member.name).parts[0] for member in source.getmembers()}
        members = source.getmembers()
        if len(roots) != 1:
            raise RuntimeError("v0.5 archive must contain exactly one package root")
        root = roots.pop()
        expected = f"{root}/bin/unionid"
        matches = [member for member in members if member.name == expected]
        if len(matches) != 1 or not matches[0].isfile():
            raise RuntimeError("v0.5 archive does not contain one native unionid binary")
        data = source.extractfile(matches[0])
        if data is None:
            raise RuntimeError("cannot read the v0.5 unionid binary")
        binary = destination / "unionid-v0.5.0"
        binary.write_bytes(data.read())
        binary.chmod(0o755)
        return binary


def unused_local_address():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{listener.getsockname()[1]}"


def wait_for_server(process, address):
    host, port = address.rsplit(":", 1)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            raise RuntimeError(f"v0.6 server exited early:\n{stdout}\n{stderr}")
        try:
            with socket.create_connection((host, int(port)), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("v0.6 server did not become ready")


def stop_server(process):
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def require_query(response, label):
    if not response.get("ok") or len(response.get("rows", [])) != 2:
        raise RuntimeError(f"{label} did not return both typed rows")
    return {key: response[key] for key in ["columns", "rows", "schema"]}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True, type=pathlib.Path)
    parser.add_argument("--checksum", required=True, type=pathlib.Path)
    parser.add_argument("--current-binary", required=True, type=pathlib.Path)
    parser.add_argument("--work-dir", type=pathlib.Path)
    args = parser.parse_args()

    archive = args.archive.resolve()
    checksum = args.checksum.resolve()
    current = args.current_binary.resolve()
    for path in [archive, checksum, current]:
        if not path.is_file():
            raise RuntimeError(f"required file does not exist: {path}")

    archive_digest = verify_checksum(archive, checksum)
    temporary = None
    if args.work_dir:
        work = args.work_dir.resolve()
        if work.exists() and (not work.is_dir() or any(work.iterdir())):
            raise RuntimeError(f"work directory must be empty: {work}")
        work.mkdir(parents=True, exist_ok=True)
    else:
        temporary = tempfile.TemporaryDirectory(prefix="unionid-v050-compat-")
        work = pathlib.Path(temporary.name)

    previous = extract_previous_binary(archive, work)
    previous_version = require_version(previous, "0.5.0")
    current_version = require_version(current, "0.6.0")
    for key in [
        "protocol_versions",
        "stream_protocol_versions",
        "readable_storage_formats",
        "readable_backup_formats",
        "current_storage",
    ]:
        if current_version.get(key) != previous_version.get(key):
            raise RuntimeError(f"v0.6 changed the frozen {key} capability")

    database = work / "v050.redb"
    source = work / "setup.unid"
    source.write_text(SOURCE)
    previous_rows = require_query(
        run([previous, "run", "--db", database, "--file", source, "--format", "json"]),
        "v0.5 setup",
    )
    run([previous, "check", "--db", database, "--format", "json"])
    backup = work / "v050.backup.json"
    run(
        [
            previous,
            "backup",
            "--db",
            database,
            "--output",
            backup,
            "--format",
            "json",
        ]
    )

    run([current, "doctor", "--db", database, "--format", "json"])
    integrity = run([current, "check", "--db", database, "--format", "json"])
    if integrity.get("backend") != "redb" or integrity.get("backend_clean") is not True:
        raise RuntimeError("v0.6 did not cleanly check the v0.5 database")
    current_rows = require_query(
        run([current, "run", "--db", database, "--query", QUERY, "--format", "json"]),
        "v0.6 query",
    )
    if current_rows != previous_rows:
        raise RuntimeError("v0.6 changed the v0.5 typed query result or schema identity")

    restored = work / "restored.redb"
    run(
        [
            current,
            "restore",
            "--backup",
            backup,
            "--db",
            restored,
            "--format",
            "json",
        ]
    )
    restored_rows = require_query(
        run([current, "run", "--db", restored, "--query", QUERY, "--format", "json"]),
        "v0.6 restored query",
    )
    if restored_rows != previous_rows:
        raise RuntimeError("v0.6 did not preserve the v0.5 logical backup")

    address = unused_local_address()
    server = subprocess.Popen(
        [current, "server", "--db", database, "--addr", address, "--read-only"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        wait_for_server(server, address)
        client_rows = require_query(
            run(
                [
                    previous,
                    "cli",
                    "--addr",
                    address,
                    "--query",
                    QUERY,
                    "--format",
                    "json",
                    "--no-history",
                ]
            ),
            "v0.5 client against v0.6 server",
        )
    finally:
        stop_server(server)
    if client_rows != previous_rows:
        raise RuntimeError("v0.5 client observed a changed v0.6 protocol result")

    print(
        json.dumps(
            {
                "schema_version": 1,
                "ok": True,
                "archive_sha256": archive_digest,
                "previous_version": previous_version["software_version"],
                "current_version": current_version["software_version"],
                "rows": len(current_rows["rows"]),
                "data_compatible": True,
                "backup_compatible": True,
                "client_compatible": True,
            },
            sort_keys=True,
        )
    )
    if temporary:
        temporary.cleanup()


if __name__ == "__main__":
    main()
