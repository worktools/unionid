mod common;

use common::TempDir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::process::Command;
use unionid::backup::incremental::{
    ArchiveLimits, IncrementalExportOptions, IncrementalInitOptions,
};
use unionid::error::ConstraintKind;
use unionid::migration::{MigrationEntry, MigrationMaintenancePhase};
use unionid::protocol::{PRODUCTION_VERSION, Request, Response};
use unionid::server::execute_protocol_request;
use unionid::{Engine, MigrationFile, QueryResponse, SchemaInfo, backup};

const SCHEMA: &str = r#"
enum OrderKey {Web(int), Import(text)}
struct Header {tenant: int, code: OrderKey, label: text}
struct Owner {id: int, name: text}
struct Order {id: int, header: Header, owner: Option<int>}
struct Line {id: int, tenant: int, order_key: OrderKey, units: int}
table owners: Owner {key id}
table orders: Order {key id}
table lines: Line {key id}
create unique index orders (header.tenant, header.code)
create reference lines (tenant, order_key) references orders (header.tenant, header.code)
create reference orders (owner) references owners (id)
insert owners {id: 9, name: "owner"}
insert orders {id: 1, header: {tenant: 3, code: Web(11), label: "checkout"}, owner: Some(9)}
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum OrderKey {
    Web(i64),
    Import(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SubmittedLine {
    id: i64,
    tenant: i64,
    order_key: OrderKey,
    units: i64,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
struct MigratedLine {
    id: i64,
    tenant: i64,
    order_ref: OrderKey,
    units: i64,
    note: String,
}

fn submitted_line() -> SubmittedLine {
    SubmittedLine {
        id: 7,
        tenant: 3,
        order_key: OrderKey::Web(11),
        units: 2,
    }
}

fn receipt_request(line: &SubmittedLine, key: &str) -> Request {
    Request::query(
        "checkout-attempt",
        "insert lines $line | returning {id, tenant, order_key, units}",
    )
    .with_version(PRODUCTION_VERSION)
    .unwrap()
    .with_serde_param("line", line)
    .unwrap()
    .with_idempotency_key(key)
    .unwrap()
}

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{source}: {}", response.message);
    response
}

#[derive(Debug, PartialEq, Eq)]
struct State {
    schema: SchemaInfo,
    ledger: Vec<MigrationEntry>,
    sequence: u64,
    receipts: usize,
    rows: Vec<serde_json::Value>,
}

fn state(engine: &mut Engine) -> State {
    State {
        schema: engine.schema_info(),
        ledger: engine.migration_history().to_vec(),
        sequence: engine.backup_journal_status().unwrap().head_sequence,
        receipts: engine.idempotency_status().unwrap().count,
        rows: ["owners", "orders", "lines"]
            .into_iter()
            .map(|table| {
                serde_json::to_value(ok(engine, &format!("from {table} | sort id")).rows).unwrap()
            })
            .collect(),
    }
}

fn rejected(engine: &mut Engine, source: &str, kind: ConstraintKind) {
    let before = state(engine);
    let response = engine.execute(source);
    assert!(!response.ok, "accepted {source}");
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert_eq!(error.constraint, Some(kind));
    assert!(error.hint.is_some());
    assert_eq!(state(engine), before);
}

fn replay(engine: &mut Engine, committed: &Response) {
    let before = state(engine);
    let replay = execute_protocol_request(
        engine,
        receipt_request(&submitted_line(), "checkout-line-7"),
    );
    assert!(replay.ok, "{}", replay.message);
    let metadata = replay.idempotency.as_ref().unwrap();
    assert!(metadata.replayed);
    assert_eq!(
        metadata.committed_sequence,
        committed.idempotency.as_ref().unwrap().committed_sequence
    );
    assert_eq!(replay.schema, committed.schema);
    assert_eq!(replay.rows, committed.rows);
    assert_eq!(
        replay.typed_rows::<SubmittedLine>().unwrap(),
        vec![submitted_line()]
    );
    assert_eq!(state(engine), before);
}

fn verify_migrated(engine: &mut Engine, expected: &State, committed: &Response) {
    assert_eq!(state(engine), *expected);
    assert_eq!(
        ok(engine, "from lines")
            .typed_rows::<MigratedLine>()
            .unwrap(),
        vec![MigratedLine {
            id: 7,
            tenant: 3,
            order_ref: OrderKey::Web(11),
            units: 2,
            note: "migrated".into(),
        }]
    );
    replay(engine, committed);
    rejected(engine, "delete orders", ConstraintKind::ReferenceRestricted);
    rejected(
        engine,
        "update orders | set header.code = Web(12)",
        ConstraintKind::ReferenceRestricted,
    );
    rejected(engine, "delete owners", ConstraintKind::ReferenceRestricted);
    rejected(
        engine,
        "upsert lines {id: 7, tenant: 4, order_ref: Web(11), units: 9}",
        ConstraintKind::ReferenceMissing,
    );
    // A successful check may repair redb allocator metadata. Its documented
    // backend_clean=false outcome still requires fully validated logical state.
    let checked = engine.check_integrity().unwrap();
    assert_eq!(checked.schema, expected.schema);
    assert!(checked.profile.bounded);
    assert_eq!(checked.profile.rows_checked, 3);
    assert_eq!(state(engine), *expected);
}

fn reject_checksummed_orphan_backup(archive: &Path, directory: &Path) {
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(archive).unwrap()).unwrap();
    let tenant = &mut envelope["database"]["objects"]["lines"]["rows"][0]["fields"]["tenant"];
    assert_eq!(*tenant, serde_json::json!({"kind": "Int", "value": 3}));
    *tenant = serde_json::json!({"kind": "Int", "value": 4});
    // Keep shape, IDs, watermarks and schema unchanged, with a valid checksum.
    // Rejection must come from the missing composite reference target.
    let payload = serde_json::to_vec(&serde_json::json!({
        "database": envelope["database"], "receipts": envelope["receipts"]
    }))
    .unwrap();
    envelope["checksum"] = serde_json::json!(format!("sha256:{:x}", Sha256::digest(payload)));
    let forged = directory.join("orphan.backup.json");
    std::fs::write(&forged, serde_json::to_vec(&envelope).unwrap()).unwrap();
    let destination_dir = directory.join("rejected-restore");
    std::fs::create_dir(&destination_dir).unwrap();
    let target = destination_dir.join("orphan.redb");
    let rejected = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["restore", "--backup"])
        .arg(&forged)
        .arg("--db")
        .arg(&target)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let error: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(error["error"]["code"], "E_STORAGE");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("reference"), "{message}");
    assert!(!message.contains("checksum"), "{message}");
    assert!(!target.exists());
    assert_eq!(std::fs::read_dir(destination_dir).unwrap().count(), 0);
}

fn recovery_journey(journal: bool) {
    let dir = TempDir::new();
    let path = dir.0.join("checkout.redb");
    let archive = dir.0.join("chain");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        ok(&mut engine, SCHEMA);
    }
    let baseline = journal.then(|| {
        backup::incremental::init(&path, &archive, IncrementalInitOptions::default()).unwrap()
    });
    let committed;
    let before_migration;
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let before = state(&mut engine);
        let mut orphan = submitted_line();
        orphan.tenant = 4;
        let failed =
            execute_protocol_request(&mut engine, receipt_request(&orphan, "orphan-line-7"));
        assert!(!failed.ok);
        assert_eq!(
            failed.error.unwrap().constraint,
            Some(ConstraintKind::ReferenceMissing)
        );
        assert_eq!(state(&mut engine), before);
        committed = execute_protocol_request(
            &mut engine,
            receipt_request(&submitted_line(), "checkout-line-7"),
        );
        assert!(committed.ok, "{}", committed.message);
        assert!(!committed.idempotency.as_ref().unwrap().replayed);
        assert_eq!(
            committed.typed_rows::<SubmittedLine>().unwrap(),
            vec![submitted_line()]
        );
        before_migration = state(&mut engine);
        assert_eq!(before_migration.receipts, 1);
        replay(&mut engine, &committed);
    }

    let migration = MigrationFile::parse(
        "migration m0001_line_name {\nrename field Line.order_key to order_ref\nadd field Line.note text = \"migrated\"\n}",
    )
    .unwrap();
    let mut ready = false;
    let mut reclaimable = false;
    let mut complete = false;
    for _ in 0..32 {
        // Every maintenance step crosses an actual close/reopen boundary.
        let mut engine = Engine::open_redb(&path).unwrap();
        let progress = engine
            .advance_migrations(std::slice::from_ref(&migration), 1)
            .unwrap();
        assert!(progress.committed_steps <= 1);
        if let Some(info) = progress.status.maintenance {
            match info.phase {
                MigrationMaintenancePhase::Building | MigrationMaintenancePhase::Ready => {
                    ready |= info.phase == MigrationMaintenancePhase::Ready;
                    assert_eq!(state(&mut engine), before_migration);
                    replay(&mut engine, &committed);
                }
                MigrationMaintenancePhase::Reclaimable => reclaimable = true,
                MigrationMaintenancePhase::Aborting => panic!("valid checkout migration aborted"),
            }
        }
        if progress.complete {
            complete = true;
            break;
        }
    }
    assert!(ready && reclaimable && complete);
    let expected = {
        let mut engine = Engine::open_redb(&path).unwrap();
        let expected = state(&mut engine);
        assert_eq!(expected.ledger.len(), 1);
        assert_eq!(expected.sequence, before_migration.sequence + 1);
        verify_migrated(&mut engine, &expected, &committed);
        let compact = engine.compact_storage().unwrap();
        assert!(compact.identity_preserved);
        assert_eq!(compact.schema, expected.schema);
        assert_eq!(compact.storage.format, if journal { 13 } else { 12 });
        verify_migrated(&mut engine, &expected, &committed);
        expected
    };
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        verify_migrated(&mut engine, &expected, &committed);
    }

    let backup_file = dir.0.join("checkout.backup.json");
    let created = backup::create(&path, &backup_file).unwrap();
    assert_eq!(created.format_version, 7);
    assert_eq!(created.receipt_count, 1);
    reject_checksummed_orphan_backup(&backup_file, &dir.0);
    let restored = dir.0.join("logical-restored.redb");
    backup::restore(&backup_file, &restored).unwrap();
    {
        let mut engine = Engine::open_redb(&restored).unwrap();
        verify_migrated(&mut engine, &expected, &committed);
        // Optional ownership can be cleared explicitly before removing its target.
        ok(
            &mut engine,
            "update orders | set owner = None\ndelete owners",
        );
        engine.check_integrity().unwrap();
    }

    if let Some(baseline) = baseline {
        let exported =
            backup::incremental::export(&path, &archive, IncrementalExportOptions::default())
                .unwrap();
        assert_eq!(exported.exported_commits, 2); // receipt write + migration cutover
        let before_target = dir.0.join("before-migration.redb");
        backup::incremental::restore(
            &archive,
            &before_target,
            baseline.baseline_sequence + 1,
            ArchiveLimits::default(),
        )
        .unwrap();
        let mut old = Engine::open_redb(before_target).unwrap();
        assert_eq!(state(&mut old), before_migration);
        replay(&mut old, &committed);
        old.check_integrity().unwrap();
        let after_target = dir.0.join("after-migration.redb");
        backup::incremental::restore(
            &archive,
            &after_target,
            expected.sequence,
            ArchiveLimits::default(),
        )
        .unwrap();
        let mut new = Engine::open_redb(after_target).unwrap();
        verify_migrated(&mut new, &expected, &committed);
    }
}

#[test]
fn reference_checkout_receipt_survives_migration_compaction_and_logical_restore() {
    recovery_journey(false);
}

#[test]
fn reference_checkout_receipt_and_cutover_restore_at_both_journal_boundaries() {
    recovery_journey(true);
}
