#!/usr/bin/env python3
"""Exercise v0.11 user journeys using only a native archive's contents."""
import argparse
import hashlib
import json
import pathlib
import subprocess
import tempfile


def execute(binary, args, cwd, succeeds=True):
    result = subprocess.run([binary, *args], cwd=cwd, text=True, capture_output=True)
    if (result.returncode == 0) != succeeds:
        raise RuntimeError(f"unexpected command result: {args}\n{result.stdout}\n{result.stderr}")
    return result.stdout


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def validate(binary, root, work):
    fixture = root / "examples/people.parquet"
    digest = hashlib.sha256(fixture.read_bytes()).hexdigest()

    def run(args, succeeds=True):
        return json.loads(execute(binary, [*args, "--format", "json"], work, succeeds))

    metadata = run(["parquet", fixture, "--limit", "0"])
    require(metadata["rows_total"] == 3 and metadata["preview_rows"] == [], "metadata-only preview")
    preview = run(["parquet", fixture, "--limit", "2"])
    require(len(preview["preview_rows"]) == 2 and preview["preview_truncated"], "bounded preview")
    query = "from data | filter active | select {id, name} | sort id"
    rows = run(["parquet", fixture, "--query", query])
    require(rows["ok"] and [row["id"]["value"] for row in rows["rows"]] == [1, 3], "typed pipeline")
    plan = run(["parquet", fixture, "--query", "explain from data | select {id}"])
    require(plan["ok"] and plan["plan"]["external_scan"] == "parquet_scan", "external plan")
    denied = run(["parquet", fixture, "--query", "delete data"], succeeds=False)
    require(denied["error"]["code"] == "E_READ_ONLY", "read-only boundary")
    require(hashlib.sha256(fixture.read_bytes()).hexdigest() == digest, "source file changed")
    guide = execute(binary, ["docs", "show", "parquet"], work)
    require("--query" in guide and "E_READ_ONLY" in guide, "bundled Parquet guide")

    project = work / "evolving"
    execute(binary, ["init", project], work)
    migration = project / "migrations/0002_priority.unid"
    migration.write_text("migration m0002_priority {\n  parent m0001_initial\n  add field Task.priority: int = 0\n}\n")
    migration.write_text(execute(binary, ["fmt", "--file", migration], work))
    database = project / "data/tasks.redb"
    run(["migration", "apply", "--db", database, "--dir", project / "migrations"])
    schema = run(["schema", "print", "--db", database])
    source = project / "schema.unid"
    source.write_text(schema["normalized"])
    source.write_text(execute(binary, ["fmt", "--file", source], work))
    checked = run(["project", "check", "--dir", project])
    require(checked["ok"], "incremental migration project check")
    original = source.read_text()
    require("priority: int = 0" in original, "missing migrated field")
    source.write_text(original.replace("priority: int = 0", "priority: int = 1"))
    drift = run(["project", "check", "--dir", project], succeeds=False)
    require(drift["error"]["code"] == "E_SCHEMA", "schema drift was not rejected")

    accounts = work / "accounts.redb"
    example = run(["run", "--db", accounts, "--file", root / "examples/account_email.unid"])
    require(example["ok"] and len(example["rows"]) == 3, "optional email example")
    before = run(["run", "--db", accounts, "--query", "from users | sort id"])
    duplicate = run(["run", "--db", accounts, "--query", 'insert many users [{id: 4, email: None}, {id: 5, email: Some("alice@example.com")}]'], succeeds=False)
    require(duplicate["error"]["code"] == "E_CONSTRAINT", "duplicate email was accepted")
    after = run(["run", "--db", accounts, "--query", "from users | sort id"])
    require(before["rows"] == after["rows"] and before["schema"] == after["schema"], "duplicate batch was not atomic")
    return {"ok": True, "parquet_rows": 3, "project_incremental_migration": True, "optional_email_rollback": True}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--package-root", required=True, type=pathlib.Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="unionid-local-data-") as temporary:
        print(json.dumps(validate(args.binary.resolve(), args.package_root.resolve(), pathlib.Path(temporary))))


if __name__ == "__main__":
    main()
