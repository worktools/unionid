#!/usr/bin/env python3
"""Verify a published native package against the next release candidate."""

import argparse
import hashlib
import json
import pathlib
import socket
import subprocess
import tarfile
import tempfile
import time


SOURCE = """enum State {
  Pending
  Running {worker: text, attempt: int}
}

struct Task {
  id: int
  title: text
  state: State
}

table tasks: Task {key id}

insert tasks {id: 1, title: "from previous release", state: Running {worker: "worker-a", attempt: 2}}
insert tasks {id: 2, title: "pending", state: Pending}

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


def verify_checksum(archive, checksum, previous_label):
    fields = checksum.read_text().strip().split()
    if len(fields) != 2 or pathlib.Path(fields[1]).name != archive.name:
        raise RuntimeError(
            f"{previous_label} checksum file does not name the supplied archive"
        )
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    if fields[0].lower() != actual:
        raise RuntimeError(
            f"{previous_label} archive SHA-256 does not match its published checksum"
        )
    return actual


def extract_previous_binary(archive, destination, previous_version):
    with tarfile.open(archive, "r:gz") as source:
        roots = {pathlib.PurePosixPath(member.name).parts[0] for member in source.getmembers()}
        members = source.getmembers()
        if len(roots) != 1:
            raise RuntimeError("previous archive must contain exactly one package root")
        root = roots.pop()
        expected = f"{root}/bin/unionid"
        matches = [member for member in members if member.name == expected]
        if len(matches) != 1 or not matches[0].isfile():
            raise RuntimeError(
                "previous archive does not contain one native unionid binary"
            )
        data = source.extractfile(matches[0])
        if data is None:
            raise RuntimeError("cannot read the previous unionid binary")
        binary = destination / f"unionid-v{previous_version}"
        binary.write_bytes(data.read())
        binary.chmod(0o755)
        return binary


def unused_local_address():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{listener.getsockname()[1]}"


def wait_for_server(process, address, current_label):
    host, port = address.rsplit(":", 1)
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stdout, stderr = process.communicate()
            raise RuntimeError(
                f"{current_label} server exited early:\n{stdout}\n{stderr}"
            )
        try:
            with socket.create_connection((host, int(port)), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError(f"{current_label} server did not become ready")


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


def receipt_request(binary, database, envelope, label):
    address = unused_local_address()
    server = subprocess.Popen(
        [binary, "server", "--db", database, "--addr", address],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        wait_for_server(server, address, label)
        host, port = address.rsplit(":", 1)
        with socket.create_connection((host, int(port)), timeout=10) as connection:
            with connection.makefile("rwb") as stream:
                stream.write(json.dumps(envelope).encode() + b"\n")
                stream.flush()
                encoded = stream.readline(16 * 1024 * 1024 + 1)
                if not encoded.endswith(b"\n") or len(encoded) > 16 * 1024 * 1024:
                    raise RuntimeError(f"{label} receipt response exceeded the frame budget")
                response = json.loads(encoded)
        if not response.get("ok") or "idempotency" not in response:
            raise RuntimeError(f"{label} receipt request failed")
        return response
    finally:
        stop_server(server)


def verify_database_case(previous, current, work, previous_label, current_label, case, expected_format, journal):
    database = work / f"previous-{case}.redb"
    source = work / f"setup-{case}.unid"
    source.write_text(SOURCE)
    previous_rows = require_query(
        run([previous, "run", "--db", database, "--file", source, "--format", "json"]),
        f"{previous_label} {case} setup",
    )
    if journal:
        run(
            [
                previous,
                "backup",
                "incremental",
                "init",
                "--db",
                database,
                "--repo",
                work / f"incremental-{case}",
                "--format",
                "json",
            ]
        )
    envelope = {
        "version": 2, "request_id": "legacy-receipt",
        "query": 'update tasks | filter id == 2 | set title = "pending" | returning',
        "params": {}, "idempotency_key": f"previous-{case}",
    }
    original_receipt = receipt_request(previous, database, envelope, previous_label)
    run([previous, "check", "--db", database, "--format", "json"])
    backup = work / f"previous-{case}.backup.json"
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

    diagnosis = run([current, "doctor", "--db", database, "--format", "json"])
    storage_format = (
        diagnosis.get("database", {}).get("storage_versions", {}).get("format")
    )
    if storage_format != expected_format:
        raise RuntimeError(f"{current_label} did not diagnose {case} as expected")
    integrity = run([current, "check", "--db", database, "--format", "json"])
    first_backend_clean = integrity.get("backend_clean")
    if integrity.get("backend") != "redb" or not isinstance(first_backend_clean, bool):
        raise RuntimeError(
            f"{current_label} did not cleanly check the {previous_label} {case} database"
        )
    if not first_backend_clean:
        reopened = run([current, "check", "--db", database, "--format", "json"])
        if reopened.get("backend") != "redb" or reopened.get("backend_clean") is not True:
            raise RuntimeError(
                f"{current_label} did not converge the {previous_label} {case} "
                "database to a clean reopen"
            )
    current_rows = require_query(
        run([current, "run", "--db", database, "--query", QUERY, "--format", "json"]),
        f"{current_label} {case} query",
    )
    if current_rows != previous_rows:
        raise RuntimeError(
            f"{current_label} changed the {previous_label} {case} typed query result "
            "or schema identity"
        )

    restored = work / f"restored-{case}.redb"
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
        f"{current_label} restored {case} query",
    )
    if restored_rows != previous_rows:
        raise RuntimeError(
            f"{current_label} did not preserve the {previous_label} {case} logical backup"
        )

    for path in [database, restored]:
        replay = receipt_request(current, path, {**envelope, "request_id": "retry"}, current_label)
        if replay["idempotency"]["replayed"] is not True or replay.get("statements", []) != original_receipt.get("statements", []):
            raise RuntimeError(f"{current_label} did not replay the legacy receipt unchanged")
        for field in ["columns", "rows", "schema", "affected_rows"]:
            if replay.get(field) != original_receipt.get(field):
                raise RuntimeError(f"{current_label} changed legacy receipt {field}")
        for field in ["digest", "committed_sequence"]:
            if replay["idempotency"][field] != original_receipt["idempotency"][field]:
                raise RuntimeError(f"{current_label} changed the legacy receipt identity")

    address = unused_local_address()
    server = subprocess.Popen(
        [current, "server", "--db", database, "--addr", address, "--read-only"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        wait_for_server(server, address, current_label)
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
            f"{previous_label} client against {current_label} {case} server",
        )
    finally:
        stop_server(server)
    if client_rows != previous_rows:
        raise RuntimeError(
            f"{previous_label} client observed a changed {current_label} {case} result"
        )
    return {
        "storage_format": expected_format,
        "first_backend_clean": first_backend_clean,
        "rows": len(current_rows["rows"]),
        "data_compatible": True,
        "backup_compatible": True,
        "client_compatible": True,
        "legacy_receipt_compatible": True,
    }


def require_rejected(command, code, message=None):
    try:
        result = subprocess.run(command, text=True, capture_output=True, timeout=60)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"rejection check timed out: {command}") from error
    if not result.returncode:
        raise RuntimeError(f"old binary accepted unsupported data: {command}")
    encoded = result.stdout.strip() or result.stderr.strip()
    if encoded.startswith("{"):
        response = json.loads(encoded)
    else:
        # Opening a database can fail before the query JSON response exists.
        prefix, separator, detail = encoded.partition(": ")
        if not separator:
            raise RuntimeError(f"unclassified compatibility failure: {encoded}")
        response = {"error": {"code": prefix, "message": detail}}
    if response.get("error", {}).get("code") != code:
        raise RuntimeError(f"unexpected compatibility failure: {response}")
    if message and message not in response["error"].get("message", ""):
        raise RuntimeError(f"failure did not identify unsupported format: {response}")
    return response


def verify_reference_boundary(previous, current, work, source_format, reference_format):
    database = work / f"previous-format{source_format}.redb"
    run([current, "upgrade", "--db", database, "--target", str(reference_format),
         "--format", "json"])
    source = """struct Assignment {id: int, task: Option<int>}
table assignments: Assignment {key id}
create reference assignments (task) references tasks (id)
insert many assignments [{id: 1, task: Some(1)}, {id: 2, task: None}]
from assignments | sort id
"""
    before = require_query(
        run([current, "run", "--db", database, "--query", source, "--format", "json"]),
        "reference boundary setup",
    )
    for query in ["from assignments", "delete tasks"]:
        require_rejected(
            [previous, "run", "--db", database, "--query", query, "--format", "json"],
            "E_STORAGE",
            "unsupported storage_format_version",
        )
    after = require_query(
        run([current, "run", "--db", database, "--query",
             "from assignments | sort id", "--format", "json"]),
        "reference boundary after rejected old reader/writer",
    )
    if before != after:
        raise RuntimeError("old reader/writer changed reference rows or schema")
    run([current, "check", "--db", database, "--format", "json"])
    backup = work / f"references-format{reference_format}.backup.json"
    run([current, "backup", "--db", database, "--output", backup, "--format", "json"])
    if json.loads(backup.read_text()).get("format_version") != 7:
        raise RuntimeError("reference backup did not use format 7")
    rejected_path = work / f"old-restore-format{reference_format}.redb"
    require_rejected(
        [previous, "restore", "--backup", backup, "--db", rejected_path,
         "--format", "json"],
        "E_BACKUP",
        "unsupported backup format",
    )
    if rejected_path.exists():
        raise RuntimeError("old restore published an unsupported reference database")
    restored = work / f"current-restore-format{reference_format}.redb"
    run([current, "restore", "--backup", backup, "--db", restored, "--format", "json"])
    restored_rows = require_query(
        run([current, "run", "--db", restored, "--query",
             "from assignments | sort id", "--format", "json"]),
        "reference boundary restored rows",
    )
    if restored_rows != before:
        raise RuntimeError("current restore changed reference rows or schema")
    for path in [database, restored]:
        run([current, "check", "--db", path, "--format", "json"])
        result = require_rejected(
            [current, "run", "--db", path, "--query",
             "delete tasks | filter id == 1", "--format", "json"],
            "E_CONSTRAINT",
        )
        if (result.get("ok") is not False
                or result.get("error", {}).get("constraint") != "reference_restricted"):
            raise RuntimeError("current database/restore lost reference enforcement")
        missing = require_rejected(
            [current, "run", "--db", path, "--query",
             "insert assignments {id: 3, task: Some(999)}", "--format", "json"],
            "E_CONSTRAINT",
        )
        if missing.get("error", {}).get("constraint") != "reference_missing":
            raise RuntimeError("current database/restore accepted a missing target")
    return {
        "storage_format": reference_format,
        "backup_format": 7,
        "old_reader_rejected": True,
        "old_writer_rejected": True,
        "old_restore_rejected": True,
        "current_restore_preserves_constraints": True,
    }


def reference_boundary_skip_reasons(previous, current):
    reasons = []
    if previous["current_storage"]["format"] != 10:
        reasons.append("previous_default_is_not_format_10")
    if not {12, 13}.issubset(current["readable_storage_formats"]):
        reasons.append("current_does_not_read_both_reference_formats")
    if {12, 13}.intersection(previous["readable_storage_formats"]):
        reasons.append("previous_already_reads_reference_storage")
    if 7 in previous["readable_backup_formats"]:
        reasons.append("previous_already_reads_reference_backup")
    return reasons


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True, type=pathlib.Path)
    parser.add_argument("--checksum", required=True, type=pathlib.Path)
    parser.add_argument("--current-binary", required=True, type=pathlib.Path)
    parser.add_argument("--previous-version", required=True)
    parser.add_argument("--current-version", required=True)
    parser.add_argument("--work-dir", type=pathlib.Path)
    args = parser.parse_args()

    archive = args.archive.resolve()
    checksum = args.checksum.resolve()
    current = args.current_binary.resolve()
    for path in [archive, checksum, current]:
        if not path.is_file():
            raise RuntimeError(f"required file does not exist: {path}")

    previous_label = f"v{args.previous_version}"
    current_label = f"v{args.current_version}"
    archive_digest = verify_checksum(archive, checksum, previous_label)
    temporary = None
    if args.work_dir:
        work = args.work_dir.resolve()
        if work.exists() and (not work.is_dir() or any(work.iterdir())):
            raise RuntimeError(f"work directory must be empty: {work}")
        work.mkdir(parents=True, exist_ok=True)
    else:
        temporary = tempfile.TemporaryDirectory(prefix="unionid-release-compat-")
        work = pathlib.Path(temporary.name)

    previous = extract_previous_binary(archive, work, args.previous_version)
    previous_version = require_version(previous, args.previous_version)
    current_version = require_version(current, args.current_version)
    # The transport and schema contract must stay identical, but a release may
    # expand the readable range and choose a new default write format. The data
    # cases below prove the previous release's databases, backups, and clients
    # still work; here we only require that nothing previously readable was
    # dropped.
    for key in [
        "schema_version",
        "protocol_versions",
        "stream_protocol_versions",
    ]:
        if current_version.get(key) != previous_version.get(key):
            raise RuntimeError(f"{current_label} changed the frozen {key} capability")
    for key in ["readable_storage_formats", "readable_backup_formats"]:
        dropped = sorted(
            set(previous_version.get(key, [])) - set(current_version.get(key, []))
        )
        if dropped:
            raise RuntimeError(
                f"{current_label} no longer reads the {key} {dropped} that "
                f"{previous_label} read"
            )

    default_format = previous_version["current_storage"]["format"]
    journal_formats = {6: 7, 8: 9, 10: 11}
    if default_format not in journal_formats:
        raise RuntimeError(f"no journal compatibility case for format {default_format}")
    cases = {
        case: verify_database_case(
            previous, current, work, previous_label, current_label, case, storage_format, journal
        )
        for storage_format, journal in [
            (default_format, False),
            (journal_formats[default_format], True),
        ]
        for case in [f"format{storage_format}"]
    }

    reference_cases = {}
    skip_reasons = reference_boundary_skip_reasons(previous_version, current_version)
    if not skip_reasons:
        reference_cases = {
            f"format{target}": verify_reference_boundary(previous, current, work, source, target)
            for source, target in [(10, 12), (11, 13)]
        }

    print(
        json.dumps(
            {
                "schema_version": 1,
                "ok": True,
                "archive_sha256": archive_digest,
                "previous_version": previous_version["software_version"],
                "current_version": current_version["software_version"],
                "cases": cases,
                "reference_boundaries": reference_cases,
                "reference_boundary_verification": {
                    "status": "skipped" if skip_reasons else "passed",
                    "skip_reasons": skip_reasons,
                },
            },
            sort_keys=True,
        )
    )
    if temporary:
        temporary.cleanup()


if __name__ == "__main__":
    main()
