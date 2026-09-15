#!/usr/bin/env python3
import argparse
import json
import pathlib
import signal
import subprocess
import sys
import time


def run(command):
    """Run one JSON CLI command and return its decoded response."""
    result = subprocess.run(command, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(
            f"command failed ({' '.join(map(str, command))}):\n{result.stderr}"
        )
    return json.loads(result.stdout)


def assert_result(response, expected_rows):
    """Require a successful query response with the expected row count."""
    if not response.get("ok") or len(response.get("rows", [])) != expected_rows:
        raise RuntimeError(f"unexpected response: {json.dumps(response, ensure_ascii=False)}")


def main():
    """Validate the packaged local, recovery, and TCP tutorial journey."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--work-dir", required=True, type=pathlib.Path)
    parser.add_argument("--tutorial-dir", type=pathlib.Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    work = args.work_dir.resolve()
    tutorial = (
        args.tutorial_dir
        or pathlib.Path(__file__).resolve().parent.parent
        / "examples"
        / "getting-started"
    ).resolve()
    if work.exists() and any(work.iterdir()):
        raise RuntimeError(f"work directory must be empty: {work}")
    work.mkdir(parents=True, exist_ok=True)

    local_db = work / "local.redb"
    run(
        [
            binary,
            "run",
            "--db",
            local_db,
            "--file",
            tutorial / "01_setup.uid",
            "--format",
            "json",
        ]
    )
    before = run(
        [
            binary,
            "run",
            "--db",
            local_db,
            "--file",
            tutorial / "02_running.uid",
            "--format",
            "json",
        ]
    )
    assert_result(before, 1)
    update = run(
        [
            binary,
            "run",
            "--db",
            local_db,
            "--file",
            tutorial / "03_update.uid",
            "--format",
            "json",
        ]
    )
    if update.get("affected_rows") != 1:
        raise RuntimeError("tutorial update did not affect exactly one row")
    local = run(
        [
            binary,
            "run",
            "--db",
            local_db,
            "--file",
            tutorial / "04_reopen.uid",
            "--format",
            "json",
        ]
    )
    assert_result(local, 2)
    integrity = run([binary, "check", "--db", local_db, "--format", "json"])
    if integrity.get("backend") != "redb" or not integrity.get("backend_clean"):
        raise RuntimeError("local redb integrity check failed")
    diagnosis = run([binary, "doctor", "--db", local_db, "--format", "json"])
    if diagnosis.get("database", {}).get("storage") != "redb":
        raise RuntimeError("doctor did not identify the tutorial redb database")

    logical_backup = work / "local.backup.json"
    restored_db = work / "restored.redb"
    run(
        [
            binary,
            "backup",
            "--db",
            local_db,
            "--output",
            logical_backup,
            "--format",
            "json",
        ]
    )
    run(
        [
            binary,
            "restore",
            "--backup",
            logical_backup,
            "--db",
            restored_db,
            "--format",
            "json",
        ]
    )
    restored = run(
        [
            binary,
            "run",
            "--db",
            restored_db,
            "--file",
            tutorial / "04_reopen.uid",
            "--format",
            "json",
        ]
    )
    assert_result(restored, 2)
    for key in ["columns", "rows", "schema"]:
        if restored[key] != local[key]:
            raise RuntimeError(f"restored typed {key} differ from the source")
    run([binary, "check", "--db", restored_db, "--format", "json"])

    tcp_db = work / "tcp.redb"
    server = subprocess.Popen(
        [binary, "server", "--addr", "127.0.0.1:0", "--db", tcp_db],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        deadline = time.monotonic() + 10
        line = ""
        while time.monotonic() < deadline:
            line = server.stdout.readline()
            if "listening on" in line:
                break
            if server.poll() is not None:
                raise RuntimeError(f"server exited early: {server.stderr.read()}")
        else:
            raise RuntimeError("server did not report its address")
        address = line.strip().rsplit(" ", 1)[-1]
        base = [binary, "cli", "--addr", address]
        run(base + ["--file", tutorial / "01_setup.uid", "--format", "json"])
        run(base + ["--file", tutorial / "03_update.uid", "--format", "json"])
        remote = run(base + ["--file", tutorial / "04_reopen.uid", "--format", "json"])
        assert_result(remote, 2)
        for key in ["columns", "rows", "schema"]:
            if remote[key] != local[key]:
                raise RuntimeError(f"local/TCP typed {key} differ")
    finally:
        if server.poll() is None:
            server.send_signal(signal.SIGTERM)
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
    run([binary, "check", "--db", tcp_db, "--format", "json"])
    print(
        json.dumps(
            {
                "ok": True,
                "rows": len(local["rows"]),
                "restored_rows": len(restored["rows"]),
                "schema": local["schema"],
            }
        )
    )


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"tutorial validation failed: {error}", file=sys.stderr)
        sys.exit(1)
