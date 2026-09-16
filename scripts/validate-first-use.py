#!/usr/bin/env python3
import argparse
import hashlib
import json
import pathlib
import subprocess
import sys


SOURCE_FILES = [
    "schema.unid",
    "seed.unid",
    "migrations/0001_initial.unid",
    "queries/list_running.unid",
]


def run(command, cwd=None, json_output=True):
    """Run one documented command and optionally decode its JSON output."""
    result = subprocess.run(command, cwd=cwd, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(
            f"command failed ({' '.join(map(str, command))}):\n{result.stderr}"
        )
    return json.loads(result.stdout) if json_output else result.stdout


def source_digests(project):
    """Capture the generated source contract without reading database files."""
    return {
        relative: hashlib.sha256((project / relative).read_bytes()).hexdigest()
        for relative in SOURCE_FILES
    }


def require_query(response, expected_rows):
    """Require a successful typed query response with a fixed row count."""
    if not response.get("ok") or len(response.get("rows", [])) != expected_rows:
        raise RuntimeError(f"unexpected query response: {json.dumps(response)}")


def require_starter_row(response):
    """Check the documented task identity and native sum-type wire shape."""
    require_query(response, 1)
    row = response["rows"][0]
    state = row.get("state", {}).get("value", {}).get("value", {})
    if (
        row.get("id") != {"kind": "Int", "value": 1}
        or row.get("title") != {"kind": "Text", "value": "learn ADTs"}
        or state.get("kind") != "Enum"
        or state.get("value", {}).get("variant") != "Running"
        or [column.get("name") for column in response.get("columns", [])]
        != ["id", "title", "state"]
    ):
        raise RuntimeError("starter query did not return the documented typed Running row")


def require_clean_redb(integrity, label):
    """Require a successful integrity report for an already-clean redb database."""
    if integrity.get("backend") != "redb" or integrity.get("backend_clean") is not True:
        raise RuntimeError(f"{label} database integrity check failed")


def main():
    """Validate the no-checkout init-to-restore journey from an empty directory."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=pathlib.Path)
    parser.add_argument("--work-dir", required=True, type=pathlib.Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    work = args.work_dir.resolve()
    if not binary.is_file():
        raise RuntimeError(f"unionid binary does not exist: {binary}")
    if work.exists() and (not work.is_dir() or any(work.iterdir())):
        raise RuntimeError(f"work directory must be an empty directory: {work}")
    work.mkdir(parents=True, exist_ok=True)

    project = work / "tasks"
    run([binary, "init", project], cwd=work, json_output=False)
    expected = [".gitignore", "README.md", *SOURCE_FILES]
    missing = [relative for relative in expected if not (project / relative).is_file()]
    if missing or not (project / "data").is_dir():
        raise RuntimeError(f"starter layout is incomplete: missing {missing}")
    before = source_digests(project)

    project_check = run(
        [binary, "project", "check", "--dir", ".", "--format", "json"],
        cwd=project,
    )
    if not project_check.get("ok") or [
        stage.get("phase") for stage in project_check.get("stages", [])
    ] != ["schema", "migrations", "queries"]:
        raise RuntimeError("project contract check did not pass every ordered stage")
    if any((project / "data").iterdir()):
        raise RuntimeError("project check created data before the first migration")

    run(
        [
            binary,
            "migration",
            "apply",
            "--db",
            "data/tasks.redb",
            "--dir",
            "migrations",
            "--format",
            "json",
        ],
        cwd=project,
    )
    seeded = run(
        [
            binary,
            "run",
            "--db",
            "data/tasks.redb",
            "--file",
            "seed.unid",
            "--format",
            "json",
        ],
        cwd=project,
    )
    if seeded.get("affected_rows") != 2:
        raise RuntimeError("starter seed did not insert exactly two rows")
    query_command = [
        binary,
        "run",
        "--db",
        "data/tasks.redb",
        "--file",
        "queries/list_running.unid",
        "--format",
        "json",
    ]
    queried = run(query_command, cwd=project)
    require_starter_row(queried)
    reopened = run(query_command, cwd=project)
    require_starter_row(reopened)
    for key in ["columns", "rows", "schema"]:
        if reopened[key] != queried[key]:
            raise RuntimeError(f"reopened typed {key} differ from the first query")

    diagnosis = run(
        [binary, "doctor", "--db", "data/tasks.redb", "--format", "json"],
        cwd=project,
    )
    schema = diagnosis.get("database", {}).get("schema")
    if diagnosis.get("database", {}).get("storage") != "redb" or not schema:
        raise RuntimeError("doctor did not report the starter redb schema")
    integrity = run(
        [binary, "check", "--db", "data/tasks.redb", "--format", "json"],
        cwd=project,
    )
    require_clean_redb(integrity, "starter")

    run(
        [
            binary,
            "backup",
            "--db",
            "data/tasks.redb",
            "--output",
            "data/tasks.backup.json",
            "--format",
            "json",
        ],
        cwd=project,
    )
    run(
        [
            binary,
            "restore",
            "--backup",
            "data/tasks.backup.json",
            "--db",
            "data/restored.redb",
            "--format",
            "json",
        ],
        cwd=project,
    )
    restored = run(
        [
            binary,
            "run",
            "--db",
            "data/restored.redb",
            "--file",
            "queries/list_running.unid",
            "--format",
            "json",
        ],
        cwd=project,
    )
    require_starter_row(restored)
    for key in ["columns", "rows", "schema"]:
        if restored[key] != queried[key]:
            raise RuntimeError(f"restored typed {key} differ from the source")
    restored_diagnosis = run(
        [binary, "doctor", "--db", "data/restored.redb", "--format", "json"],
        cwd=project,
    )
    if restored_diagnosis.get("database", {}).get("schema") != schema:
        raise RuntimeError("restored schema identity differs from the source")
    restored_integrity = run(
        [binary, "check", "--db", "data/restored.redb", "--format", "json"],
        cwd=project,
    )
    require_clean_redb(restored_integrity, "restored")

    if source_digests(project) != before:
        raise RuntimeError("the first-use journey changed generated project sources")
    print(
        json.dumps(
            {
                "schema_version": 1,
                "ok": True,
                "project_files": len(expected),
                "rows": 2,
                "query_rows": len(queried["rows"]),
                "restored_rows": len(restored["rows"]),
                "schema": schema,
            },
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"first-use validation failed: {error}", file=sys.stderr)
        sys.exit(1)
