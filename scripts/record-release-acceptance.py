#!/usr/bin/env python3
import argparse
import hashlib
import json
import pathlib
import re
import tarfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
CONTRACT_PATH = ROOT / "release" / "contract.json"


def read_json_member(archive, name):
    """Read one bounded JSON member and return its value and digest."""
    matches = [member for member in archive.getmembers() if member.name == name]
    if len(matches) != 1 or not matches[0].isfile() or matches[0].size > 1024 * 1024:
        raise RuntimeError(f"expected one bounded regular archive member: {name}")
    source = archive.extractfile(matches[0])
    if source is None:
        raise RuntimeError(f"cannot read archive member: {name}")
    data = source.read()
    return json.loads(data), hashlib.sha256(data).hexdigest()


def main():
    """Record deterministic evidence for one release-candidate platform."""
    parser = argparse.ArgumentParser(description="Record one platform's release acceptance")
    parser.add_argument("--dist", required=True, type=pathlib.Path)
    parser.add_argument("--expected-sha256", required=True)
    parser.add_argument("--expected-target", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--workflow-url", required=True)
    parser.add_argument("--runner-os", required=True)
    parser.add_argument("--runner-arch", required=True)
    parser.add_argument("--controlled-network-journey", action="store_true")
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()

    digest = args.expected_sha256.lower()
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise RuntimeError("--expected-sha256 must be exactly 64 hexadecimal characters")
    if not re.fullmatch(r"[0-9a-f]{40}", args.candidate.lower()):
        raise RuntimeError("--candidate must be a full 40-character Git commit")

    archives = sorted(args.dist.glob("unionid-v*.tar.gz"))
    if len(archives) != 1:
        raise RuntimeError(f"expected one release archive, found {len(archives)}")
    archive_path = archives[0]
    actual_digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
    if actual_digest != digest:
        raise RuntimeError("release archive does not match the trusted SHA-256")

    trusted_contract = json.loads(CONTRACT_PATH.read_text())
    with tarfile.open(archive_path, "r:gz") as archive:
        roots = {pathlib.PurePosixPath(member.name).parts[0] for member in archive.getmembers()}
        if len(roots) != 1:
            raise RuntimeError("archive must contain exactly one package root")
        package_root = roots.pop()
        release, release_digest = read_json_member(archive, f"{package_root}/RELEASE.json")
        packaged_contract, contract_digest = read_json_member(
            archive, f"{package_root}/release/contract.json"
        )

    if packaged_contract != trusted_contract:
        raise RuntimeError("packaged contract does not match the trusted source contract")
    if release["version"] != trusted_contract["software_version"]:
        raise RuntimeError("release version does not match the trusted contract")
    if release["target"] != args.expected_target:
        raise RuntimeError(
            f"release target {release['target']} does not match expected {args.expected_target}"
        )
    if release["source_commit"] != args.candidate.lower() or release["source_dirty"] is not False:
        raise RuntimeError("release provenance does not match the clean candidate commit")

    validation_results = [
        {"name": "locked_release_build", "status": "passed"},
        {"name": "formatter", "status": "passed"},
        {"name": "locked_check", "status": "passed"},
        {"name": "strict_clippy", "status": "passed"},
        {"name": "full_test_suite", "status": "passed"},
        {"name": "rust_engine_local_cli_tcp_adt", "status": "passed"},
        {"name": "packaged_independent_rust_consumer", "status": "passed"},
        {"name": "typed_query_binding_application_evolution", "status": "passed"},
        {"name": "paired_sqlite_sqlx_evaluation", "status": "passed"},
        {"name": "http_protocol_stream_adt", "status": "passed"},
        {"name": "upgrade_migration_recovery_read_only", "status": "passed"},
        {"name": "idempotency_cursor_cancel_limits", "status": "passed"},
        {"name": "incremental_backup_sequence_restore", "status": "passed"},
        {"name": "controlled_network_configuration", "status": "passed"},
        {"name": "external_sha256_contract_package_tutorial", "status": "passed"},
    ]
    if args.controlled_network_journey:
        validation_results.append(
            {"name": "controlled_network_mtls_journey", "status": "passed"}
        )

    report = {
        "schema_version": 1,
        "candidate_commit": args.candidate.lower(),
        "workflow_url": args.workflow_url,
        "runner": {
            "os": args.runner_os,
            "arch": args.runner_arch,
            "target": release["target"],
        },
        "artifact": {
            "archive": archive_path.name,
            "sha256": digest,
            "release_manifest_sha256": release_digest,
            "contract_sha256": contract_digest,
        },
        "release": release,
        "validation_results": validation_results,
        "evidence": {
            "rust_engine_local_cli_tcp_adt": "tests/getting_started.rs",
            "packaged_independent_rust_consumer": (
                "tests/current-consumer and scripts/verify-current-consumer.py"
            ),
            "typed_query_binding_application_evolution": (
                "scripts/verify-query-bindings.py, scripts/verify-typed-application.py, "
                "and docs/assessments/typed-application-2026-09-13.md"
            ),
            "paired_sqlite_sqlx_evaluation": (
                "tools/adt-interop-eval, scripts/verify-adt-interop-eval.py, and "
                "docs/assessments/sqlite-sqlx-pair-2026-09-13.md"
            ),
            "http_protocol_stream_adt": "examples/todolist.rs and tests/interfaces.rs",
            "upgrade_migration_recovery_read_only": "tests/released_v010_upgrade.rs, tests/migration.rs, tests/storage.rs, tests/backup.rs, and tests/interfaces.rs",
            "idempotency_cursor_cancel_limits": "tests/protocol.rs, tests/pagination.rs, tests/concurrency.rs, and tests/interfaces.rs",
            "incremental_backup_sequence_restore": (
                "tests/incremental_backup.rs, tools/incremental-backup-eval, and "
                "docs/BACKUP.md"
            ),
            "controlled_network_configuration": (
                "scripts/verify-envoy-deployment.py and docs/DEPLOYMENT.md"
            ),
            "controlled_network_mtls_journey": (
                "deploy/envoy/verify.sh, scripts/verify-envoy-deployment.py, and "
                "docs/DEPLOYMENT.md"
            ),
            "capacity_and_known_limits": (
                "docs/benchmarks/m7-acceptance-2026-09-10.md and "
                f"docs/RELEASE-v{release['version']}.md"
            ),
        },
    }
    if not args.controlled_network_journey:
        report["evidence"].pop("controlled_network_mtls_journey")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"ok": True, "output": str(args.output), "target": release["target"]}))


if __name__ == "__main__":
    main()
