#!/usr/bin/env python3
"""Exercise v0.13 user journeys using only the supplied native binary."""
import argparse
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location(
    "atomic_journey", pathlib.Path(__file__).with_name("validate-atomic-writes.py")
)
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)
require = helpers.require


def validate(binary, work):
    def raw(args, expected=0):
        result = subprocess.run([binary, *args], cwd=work, text=True,
                                capture_output=True, timeout=60)
        require(result.returncode == expected, f"{args}: {result.stdout}\n{result.stderr}")
        return result

    def command(args, expected=0):
        return json.loads(raw([*args, "--format", "json"], expected).stdout)

    migrations = work / "migrations"
    queries = work / "queries"
    migrations.mkdir()
    queries.mkdir()
    database = work / "jobs.redb"
    (migrations / "001.unid").write_text(
        "migration initial {\n add enum State { Pending, Done }\n"
        " add struct Job { id: int, state: State }\n add table jobs: Job {key id}\n}\n"
    )
    command(["migration", "apply", "--db", database, "--dir", migrations])
    command(["run", "--db", database, "--query", "insert jobs {id: 1, state: Pending}"])
    (migrations / "002.unid").write_text("migration expand {parent initial\n add variant State.Cancelled\n}\n")
    (queries / "jobs.unid").write_text(
        "from jobs\nderive done = match state { Pending => false, Done => true }\nselect {id, done}\n"
    )
    before = database.read_bytes()
    for action in ["plan", "rehearse", "apply"]:
        report = command(["migration", action, "--db", database, "--dir", migrations,
                          "--queries", queries], 3)
        require(not report["ok"] and not report["query_validation"]["valid"], "stale query accepted")
        if action != "apply":
            require(database.read_bytes() == before, "preview changed source bytes")
    (queries / "jobs.unid").write_text("from jobs\nselect {id}\n")
    report = command(["migration", "apply", "--db", database, "--dir", migrations, "--queries", queries])
    require(report["query_validation"]["valid"], "valid query rejected")
    command(["check", "--db", database])

    project = work / "project"
    raw(["init", project])
    project_db = project / "data/tasks.redb"
    project_migrations = project / "migrations"
    command(["migration", "apply", "--db", project_db, "--dir", project_migrations])
    schema = project / "schema.unid"
    schema.write_text(schema.read_text().replace("  state: State\n", "  state: State\n  priority: int = 0\n"))
    command(["migration", "diff", "--db", project_db, "--schema", schema,
             "--dir", project_migrations, "--name", "priority"])
    command(["project", "check", "--dir", project])
    saved = work / "saved.unid"
    saved.write_text(raw(["schema", "print", "--db", project_db, "--format", "source"]).stdout)
    raw(["fmt", "--check", saved, project_migrations / "0002_priority.unid"])
    a, b = work / "a.unid", work / "b.unid"
    for path in [a, b]:
        path.write_text("struct Example {id: int}")
    raw(["fmt", "--write", a, b])
    raw(["fmt", "--check", a, b])

    receipts = work / "receipts.redb"
    command(["run", "--db", receipts, "--query",
             "struct Counter {id: int, value: int}\ntable counters: Counter {key id}\ninsert counters {id: 1, value: 0}"])
    envelope = {"version": 2, "request_id": "increment", "params": {},
                "query": "update counters | set value = value + 1 | returning {value}",
                "idempotency_key": "delivery"}
    with helpers.server(binary, receipts, work) as address:
        first = helpers.request(address, envelope)
        require(first["ok"] and not first["idempotency"]["replayed"], "initial write")
    time.sleep(1.1)
    with helpers.server(binary, receipts, work) as address:
        require(helpers.request(address, envelope)["idempotency"]["replayed"], "policy enabled by restart")
    status = command(["receipts", "status", "--db", receipts])
    require(status["capacity"]["state"] == "normal", "capacity status")
    source_bytes = receipts.read_bytes()
    preview = command(["receipts", "retain", "--db", receipts, "--min-age-seconds", "1"])
    require(preview["selected_count"] == 1 and not preview["applied"], "retention preview")
    require(receipts.read_bytes() == source_bytes, "retention preview changed source")
    flags = ["--receipt-retention-seconds", "1", "--receipt-retention-interval-seconds", "1",
             "--receipt-retention-max-receipts", "1"]
    with helpers.server(binary, receipts, work, extra=flags) as address:
        deadline = time.monotonic() + 10
        while True:
            response = helpers.request(address, envelope)
            require(response["ok"], "retry failed")
            if not response["idempotency"]["replayed"]:
                break
            require(time.monotonic() < deadline, "service never cleaned expired receipt")
            time.sleep(0.05)
        require(helpers.request(address, envelope)["idempotency"]["replayed"], "new receipt not replayed")
    rows = command(["run", "--db", receipts, "--query", "from counters | filter value == 2"])
    require(len(rows["rows"]) == 1, "unexpected cleanup/retry effect")
    command(["check", "--db", receipts])
    return {"ok": True, "migration_query_preflight": True, "canonical_generated_source": True,
            "batch_formatter": True, "receipt_preview": True, "opt_in_service_retention": True}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="unionid-deployment-safety-") as temporary:
        print(json.dumps(validate(args.binary.resolve(), pathlib.Path(temporary))))


if __name__ == "__main__":
    main()
