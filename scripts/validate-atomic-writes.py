#!/usr/bin/env python3
"""Accept v0.12 atomic writes using only an extracted native package."""
import argparse
import contextlib
import json
import pathlib
import queue
import socket
import subprocess
import tempfile
import threading


SETUP = """struct Account {id: int, balance: int}
table accounts: Account {key id}
create index accounts (balance)
insert many accounts [{id: 1, balance: 100}, {id: 2, balance: 0}]
"""


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def transfer(sender, recipient):
    return f"""update accounts
filter id == {sender} && balance >= 50
set balance = balance - 50
expect affected == 1
update accounts
filter id == {recipient}
set balance = balance + 50
expect affected == 1
"""


def execute(binary, args, cwd, expected=0):
    result = subprocess.run([binary, *args], cwd=cwd, text=True, capture_output=True, timeout=60)
    require(result.returncode == expected, f"command failed: {args}\n{result.stdout}\n{result.stderr}")
    return json.loads(result.stdout)


@contextlib.contextmanager
def server(binary, database, cwd):
    process = subprocess.Popen(
        [binary, "server", "--addr", "127.0.0.1:0", "--db", database],
        cwd=cwd, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    ready = queue.Queue()
    threading.Thread(target=lambda: ready.put(process.stdout.readline()), daemon=True).start()
    try:
        line = ready.get(timeout=10).strip()
        prefix = "unionid server listening on "
        require(line.startswith(prefix), f"unexpected server startup: {line}")
        host, port = line[len(prefix):].rsplit(":", 1)
        yield host, int(port)
    finally:
        process.terminate()
        try:
            process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=10)


def request(address, envelope):
    with socket.create_connection(address, timeout=10) as connection:
        with connection.makefile("rwb") as stream:
            stream.write(json.dumps(envelope).encode() + b"\n")
            stream.flush()
            encoded = stream.readline(16 * 1024 * 1024 + 1)
            require(encoded.endswith(b"\n") and len(encoded) <= 16 * 1024 * 1024, "response frame")
            return json.loads(encoded)


def validate(binary, root, work):
    def command(args, expected=0):
        return execute(binary, [*args, "--format", "json"], work, expected)

    manifest = command(["agent"])
    require(manifest["atomic_scripts"]["max_statements"] == 4096, "guard capability")
    require("statement_index" in manifest["error_contract"]["fields"], "error capability")
    docs = command(["docs", "query"])
    require("expect affected == 1" in docs["reference"], "bundled guard guide")
    require(any(example["name"] == "atomic-claim" for example in docs["examples"]), "bundled guard example")
    claim = command(["run", "--file", root / "examples/atomic_claim.unid"])
    require(claim["ok"] and claim["affected_rows"] == 1, "runnable claim")
    require(claim["statements"][-1]["kind"] == "expect" and len(claim["rows"]) == 1, "trailing returning")

    database = work / "accounts.redb"
    command(["run", "--db", database, "--query", SETUP])
    before = command(["run", "--db", database, "--query", "from accounts | sort id"])
    for sender, recipient, index in [(2, 1, 2), (1, 99, 4)]:
        failed = command(["run", "--db", database, "--query", transfer(sender, recipient)], 3)
        require(failed["error"]["code"] == "E_EXPECTATION", "guard rejection")
        require(failed["error"]["statement_index"] == index, "statement index")
        require(not failed["rows"] and not failed.get("statements", []), "uncommitted results")
        after = command(["run", "--db", database, "--query", "from accounts | sort id"])
        require(before["rows"] == after["rows"] and before["schema"] == after["schema"], "transfer rollback")

    repository = work / "archive"
    command(["backup", "incremental", "init", "--db", database, "--repo", repository])
    envelope = {"version": 2, "request_id": "transfer", "query": transfer(1, 2),
                "params": {}, "idempotency_key": "native-transfer"}
    with server(binary, database, work) as address:
        first = request(address, envelope)
        require(first["ok"] and len(first["statements"]) == 4, "ordered transfer summary")
        require(first["statements"][0]["affected_rows"] == 1, "debit count")
        require(first["statements"][2]["affected_rows"] == 1, "credit count")
        sequence = first["idempotency"]["committed_sequence"]

    backup = work / "accounts.backup.json"
    logical = work / "logical.redb"
    journal = work / "journal.redb"
    command(["backup", "--db", database, "--output", backup])
    command(["restore", "--backup", backup, "--db", logical])
    command(["backup", "incremental", "export", "--db", database, "--repo", repository])
    command(["backup", "incremental", "verify", "--repo", repository])
    command(["restore", "incremental", "--repo", repository, "--db", journal, "--at-sequence", str(sequence)])
    for path in [database, logical, journal]:
        command(["check", "--db", path])
        with server(binary, path, work) as address:
            replay = request(address, {**envelope, "request_id": "retry"})
            require(replay["ok"] and replay["idempotency"]["replayed"], "durable receipt replay")
            require(replay["statements"] == first["statements"], "exact saved summaries")
        rows = command(["run", "--db", path, "--query", "from accounts | filter balance == 50"])
        require(len(rows["rows"]) == 2, "replay repeated transfer effects")
    return {"ok": True, "transfer_rollback": True, "statement_index": True,
            "guarded_returning": True, "receipt_restart_backup_journal": True,
            "bundled_agent_docs": True}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--package-root", required=True, type=pathlib.Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="unionid-atomic-writes-") as temporary:
        print(json.dumps(validate(args.binary.resolve(), args.package_root.resolve(), pathlib.Path(temporary))))


if __name__ == "__main__":
    main()
