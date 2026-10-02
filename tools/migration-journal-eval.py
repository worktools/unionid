#!/usr/bin/env python3
"""Compare isolated migration RSS with/without journal and verify restored rows."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import tempfile
import time


def command(binary, *arguments, json_output=False):
    arguments = [str(binary), *map(str, arguments)]
    if json_output:
        arguments += ["--format", "json"]
    result = subprocess.run(arguments, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip())
    return json.loads(result.stdout) if json_output else result.stdout


def verify_rows(binary, database, rows):
    cursor = None
    seen = 0
    digest = hashlib.sha256()
    schema = None
    while True:
        query = "from items | sort id | page 1000"
        if cursor:
            query += " after " + json.dumps(cursor)
        response = command(binary, "run", "--db", database, "--read-only",
                           "--query", query, json_output=True)
        assert response["ok"], response
        if schema is None:
            schema = response["schema"]
        assert response["schema"] == schema
        for row in response["rows"]:
            expected = {"id": {"kind": "Int", "value": seen},
                        "label": {"kind": "Text", "value": f"row-{seen}"},
                        "body": {"kind": "Text", "value": "x" * 120},
                        "note": {"kind": "Text", "value": "migrated"}}
            assert row == expected, f"unexpected row at {seen}"
            digest.update(json.dumps(row, sort_keys=True, separators=(",", ":")).encode())
            seen += 1
        cursor = response["page"].get("next_cursor")
        if not cursor:
            break
        assert response["rows"], "empty nonterminal page"
    assert seen == rows, (seen, rows)
    command(binary, "check", "--db", database)
    return {"rows": seen, "sha256": digest.hexdigest(), "schema": schema}


def measure(binary, database, migrations):
    # This worker spawns only the migration; RSS cannot inherit the maximum
    # from earlier inserts, archive creation, checks or restoration processes.
    started = time.monotonic_ns()
    command(binary, "migration", "apply", "--db", database, "--dir", migrations)
    peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    return {"elapsed_micros": (time.monotonic_ns() - started) // 1000,
            "peak_rss_bytes": peak if sys.platform == "darwin" else peak * 1024}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--previous-binary", type=Path)
    parser.add_argument("--rows", default="60000,100000")
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--measure-db", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--migrations", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    if args.measure_db:
        print(json.dumps(measure(args.binary, args.measure_db, args.migrations)))
        return
    sizes = [int(value) for value in args.rows.split(",")]
    if not 1 <= args.samples <= 10 or not sizes or any(not 1 <= n <= 100000 for n in sizes):
        parser.error("use 1–10 samples and row counts in 1–100000")
    if not args.output:
        parser.error("--output is required")
    binaries = [("current", args.binary)]
    if args.previous_binary:
        binaries.insert(0, ("previous", args.previous_binary.resolve()))
    report = {"version": 1, "platform": sys.platform, "machine": os.uname().machine,
              "samples": args.samples, "binaries": {}, "datasets": []}
    for name, binary in binaries:
        report["binaries"][name] = {
            "path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
            "version": command(binary, "version", json_output=True)}
    with tempfile.TemporaryDirectory(prefix="unionid-migration-journal-eval-") as workspace:
        workspace = Path(workspace)
        migrations = workspace / "migrations"
        migrations.mkdir()
        (migrations / "m0001_note.unid").write_text(
            'migration m0001_note\n  add field Item.note text = "migrated"\n')
        for rows in sizes:
            seed = workspace / "seed.redb"
            command(args.binary, "run", "--db", seed, "--query",
                    "struct Item {id: int, label: text, body: text}\n"
                    "table items: Item {key id}\ncreate index items (label)")
            source = workspace / "batch.unid"
            for start in range(0, rows, 2000):
                values = ["{id: " + str(i) + ", label: " + json.dumps(f"row-{i}")
                          + ", body: " + json.dumps("x" * 120) + "}"
                          for i in range(start, min(start + 2000, rows))]
                source.write_text("insert many items [" + ",".join(values) + "]\n")
                assert source.stat().st_size < 1024 * 1024
                command(args.binary, "run", "--db", seed, "--file", source)
            dataset = {"rows": rows, "runs": []}
            for name, binary in binaries:
                for journaling in [False, True]:
                    for sample in range(args.samples):
                        case = workspace / "case"
                        case.mkdir()
                        database = case / "app.redb"
                        shutil.copyfile(seed, database)
                        archive = case / "archive"
                        if journaling:
                            initialized = command(binary, "backup", "incremental", "init",
                                                  "--db", database, "--repo", archive,
                                                  json_output=True)
                        worker = subprocess.check_output([
                            sys.executable, str(Path(__file__).resolve()), "--binary", str(binary),
                            "--measure-db", str(database), "--migrations", str(migrations)], text=True)
                        measured = json.loads(worker)
                        measured.update({"binary": name, "journal": journaling, "sample": sample})
                        measured["source"] = verify_rows(binary, database, rows)
                        if journaling:
                            command(binary, "backup", "incremental", "export",
                                    "--db", database, "--repo", archive)
                            restored = case / "restored.redb"
                            command(binary, "restore", "incremental", "--repo", archive,
                                    "--db", restored, "--at-sequence",
                                    initialized["baseline_sequence"] + 1)
                            measured["restored"] = verify_rows(binary, restored, rows)
                            assert measured["restored"] == measured["source"]
                        dataset["runs"].append(measured)
                        print(json.dumps({"rows": rows, **{k: measured[k] for k in
                              ["binary", "journal", "sample", "elapsed_micros", "peak_rss_bytes"]}}),
                              flush=True)
                        shutil.rmtree(case)
            report["datasets"].append(dataset)
            seed.unlink()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
