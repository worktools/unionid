mod common;

use common::TempDir;
use serde_json::Value;
use std::path::Path;
use unionid::backup::incremental::ArchiveLimits;
use unionid::migration::MigrationEntry;
use unionid::protocol::{PRODUCTION_VERSION, Request, Response};
use unionid::server::execute_protocol_request;
use unionid::{
    Engine, IdempotencyPruneOptions, MigrationFile, MigrationMaintenancePhase, SchemaInfo, backup,
};

#[derive(Debug, PartialEq, Eq)]
struct State {
    schema: SchemaInfo,
    ledger: Vec<MigrationEntry>,
    sequence: u64,
    receipts: usize,
    rows: Value,
}

const MIGRATIONS: [&str; 3] = [
    "migration add_note\n  add field Item.note text = \"migrated\"",
    "migration score_to_bool\n  parent add_note\n  change field Item.score to bool\n    using old -> old > 0",
    "migration remove_pending\n  parent score_to_bool\n  drop variant State.Pending\n    using old -> State.Done",
];

fn state(path: &Path, combination: &str) -> State {
    let mut engine = Engine::open_redb(path).unwrap();
    engine.check_integrity().unwrap();
    inspect(&mut engine, combination)
}

fn inspect(engine: &mut Engine, combination: &str) -> State {
    let sequence = engine.backup_journal_status().unwrap().head_sequence;
    let response = engine.execute("from items | sort id");
    assert!(
        response.ok,
        "combination={combination}, sequence={sequence}, error={:?}",
        response.error
    );
    State {
        schema: engine.schema_info(),
        ledger: engine.migration_history().to_vec(),
        sequence,
        receipts: engine.idempotency_status().unwrap().count,
        rows: serde_json::to_value(response.rows).unwrap(),
    }
}

fn reject_duplicate(path: &Path, combination: &str) {
    let before = state(path, combination);
    let mut engine = Engine::open_redb(path).unwrap();
    let response = engine.execute("update items | filter id == 2 | set active = true");
    assert!(
        !response.ok,
        "combination={combination}, sequence={} accepted duplicate active label",
        before.sequence
    );
    assert_eq!(
        response.error.as_ref().map(|error| error.code.as_str()),
        Some("E_CONSTRAINT"),
        "combination={combination}, sequence={}, error={:?}",
        before.sequence,
        response.error
    );
    assert_eq!(
        response.error.as_ref().unwrap().constraint,
        Some(unionid::error::ConstraintKind::PartialUnique)
    );
    drop(engine);
    assert_eq!(state(path, combination), before);
}

fn migration_journey(partial_index: bool) {
    let combination = if partial_index {
        "journal+phased-migration+partial-unique"
    } else {
        "journal+phased-migration"
    };
    eprintln!("combination={combination}");
    let temp = TempDir::new();
    let path = temp.0.join("source.redb");
    let archive = temp.0.join("archive");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let response = engine.execute(
            r#"
enum State {Pending, Done}
struct Item {id: int, label: text, score: int, active: bool, state: State}
table items: Item {key id}
insert items {id: 1, label: "shared", score: 2, active: true, state: Pending}
insert items {id: 2, label: "shared", score: 0, active: false, state: Done}
"#,
        );
        assert!(response.ok, "{combination}: {:?}", response.error);
        if partial_index {
            let response = engine.execute("create unique index items (label) if active == true");
            assert!(response.ok, "{combination}: {:?}", response.error);
        }
    }
    backup::incremental::init(&path, &archive, Default::default()).unwrap();
    let mut checkpoints = vec![state(&path, combination)];
    assert_eq!(checkpoints[0].rows.as_array().unwrap().len(), 2);
    let mut files = Vec::new();
    for source in MIGRATIONS {
        eprintln!(
            "combination={combination}, sequence={}, migration={source}",
            checkpoints.last().unwrap().sequence
        );
        files.push(MigrationFile::parse(source).unwrap());
        let mut phases = Vec::new();
        let mut complete = false;
        for _ in 0..32 {
            // Close between every maintenance action, including cutover/reclaim.
            let mut engine = Engine::open_redb(&path).unwrap();
            let progress = engine.advance_migrations(&files, 1).unwrap();
            assert!(progress.committed_steps <= 1, "{combination}");
            if let Some(maintenance) = progress.status.maintenance {
                phases.push(maintenance.phase);
            }
            if progress.complete {
                complete = true;
                break;
            }
        }
        assert!(complete, "combination={combination}, migration={source}");
        assert!(phases.contains(&MigrationMaintenancePhase::Ready));
        assert!(phases.contains(&MigrationMaintenancePhase::Reclaimable));
        let snapshot = state(&path, combination);
        assert_eq!(snapshot.sequence, checkpoints.last().unwrap().sequence + 1);
        assert_eq!(snapshot.ledger.len(), files.len());
        assert_eq!(snapshot.rows.as_array().unwrap().len(), 2);
        let mut engine = Engine::open_redb(&path).unwrap();
        let query = match files.len() {
            1 => "from items | filter note == \"migrated\"",
            2 => "from items | filter (id == 1 && score == true) || (id == 2 && score == false)",
            _ => "from items | filter state == Done",
        };
        let response = engine.execute(query);
        assert!(response.ok, "{combination}: {:?}", response.error);
        assert_eq!(response.rows.len(), 2, "{combination}, query={query}");
        drop(engine);
        checkpoints.push(snapshot);
    }

    if partial_index {
        reject_duplicate(&path, combination);
    }
    let logical = temp.0.join("logical.json");
    backup::create(&path, &logical).unwrap();
    let restored = temp.0.join("logical.redb");
    backup::restore(&logical, &restored).unwrap();
    assert_eq!(state(&restored, combination), *checkpoints.last().unwrap());
    if partial_index {
        reject_duplicate(&restored, combination);
    }

    backup::incremental::export(&path, &archive, Default::default()).unwrap();
    backup::incremental::verify(&archive, ArchiveLimits::default()).unwrap();
    for expected in checkpoints {
        eprintln!(
            "combination={combination}, restoring sequence={}",
            expected.sequence
        );
        let target = temp.0.join(format!("sequence-{}.redb", expected.sequence));
        backup::incremental::restore(
            &archive,
            &target,
            expected.sequence,
            ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(
            state(&target, combination),
            expected,
            "combination={combination}, restore sequence={}",
            expected.sequence
        );
        if partial_index {
            reject_duplicate(&target, combination);
        }
    }
}

#[test]
fn journal_phased_migrations_restore_each_schema_boundary() {
    migration_journey(false);
}

#[test]
fn partial_unique_index_survives_phased_migrations_and_both_restore_paths() {
    migration_journey(true);
}

fn receipt_request(key: &str) -> Request {
    Request::query(
        "matrix-attempt",
        "update items | set score = score + 1 | returning {id, score}",
    )
    .with_version(PRODUCTION_VERSION)
    .unwrap()
    .with_idempotency_key(key)
    .unwrap()
}

fn receipt_restore(path: &Path, expected: &State, replies: &[(&str, &Response)], pruned: bool) {
    const COMBINATION: &str = "journal+receipts+prune+replay";
    assert_eq!(state(path, COMBINATION), *expected);
    let mut engine = Engine::open_redb(path).unwrap();
    for (key, original) in replies {
        let reply = execute_protocol_request(&mut engine, receipt_request(key));
        let mut saved = serde_json::to_value(original).unwrap();
        saved["idempotency"]["replayed"] = Value::Bool(true);
        assert_eq!(
            serde_json::to_value(reply).unwrap(),
            saved,
            "combination={COMBINATION}, sequence={}, key={key}",
            expected.sequence
        );
    }
    drop(engine);
    assert_eq!(state(path, COMBINATION), *expected, "replay changed state");
    if pruned {
        let mut engine = Engine::open_redb(path).unwrap();
        let reused = execute_protocol_request(&mut engine, receipt_request("first"));
        assert!(reused.ok, "{COMBINATION}: {:?}", reused.error);
        assert!(!reused.idempotency.unwrap().replayed);
        let response = engine.execute("from items");
        assert!(response.ok, "{COMBINATION}: {:?}", response.error);
        assert!(response.rows[0]["score"].cmp_eq(&unionid::Value::Int(3)));
        drop(engine);
        let after = state(path, COMBINATION);
        assert_eq!(after.sequence, expected.sequence + 1);
        assert_eq!(after.receipts, 2);
    }
}

#[test]
fn journal_receipt_pruning_preserves_replay_and_reuse_at_each_restore_boundary() {
    const COMBINATION: &str = "journal+receipts+prune+replay";
    let temp = TempDir::new();
    let path = temp.0.join("source.redb");
    let archive = temp.0.join("archive");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let response = engine.execute(
            "struct Item {id: int, score: int}\ntable items: Item {key id}\ninsert items {id: 1, score: 0}",
        );
        assert!(response.ok, "{COMBINATION}: {:?}", response.error);
    }
    backup::incremental::init(&path, &archive, Default::default()).unwrap();
    let mut checkpoints = vec![state(&path, COMBINATION)];
    let mut replies = Vec::new();
    for key in ["first", "retained"] {
        let mut engine = Engine::open_redb(&path).unwrap();
        let response = execute_protocol_request(&mut engine, receipt_request(key));
        assert!(response.ok, "{COMBINATION}: {:?}", response.error);
        assert!(!response.idempotency.as_ref().unwrap().replayed);
        replies.push(response);
        drop(engine);
        checkpoints.push(state(&path, COMBINATION));
    }
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let options = IdempotencyPruneOptions {
            completed_before_unix_ms: None,
            committed_through_sequence: Some(checkpoints[1].sequence),
            max_receipts: 1,
        };
        let preview = engine.plan_idempotency_prune(options.clone()).unwrap();
        assert_eq!(preview.selected_count, 1);
        assert!(!preview.applied);
        assert_eq!(engine.idempotency_status().unwrap().count, 2);
        assert_eq!(
            engine.backup_journal_status().unwrap().head_sequence,
            checkpoints.last().unwrap().sequence
        );
        let applied = engine.prune_idempotency_receipts(options).unwrap();
        assert!(applied.applied);
        assert_eq!(applied.selected_count, 1);
        assert_eq!(applied.remaining_count, 1);
    }
    checkpoints.push(state(&path, COMBINATION));
    for pair in checkpoints.windows(2) {
        assert_eq!(pair[1].sequence, pair[0].sequence + 1);
    }
    assert_eq!(checkpoints.last().unwrap().receipts, 1);
    let logical = temp.0.join("logical.json");
    backup::create(&path, &logical).unwrap();
    let restored = temp.0.join("logical.redb");
    backup::restore(&logical, &restored).unwrap();
    receipt_restore(
        &restored,
        checkpoints.last().unwrap(),
        &[("retained", &replies[1])],
        true,
    );

    backup::incremental::export(&path, &archive, Default::default()).unwrap();
    backup::incremental::verify(&archive, ArchiveLimits::default()).unwrap();
    for (index, expected) in checkpoints.into_iter().enumerate() {
        eprintln!(
            "combination={COMBINATION}, restoring sequence={}",
            expected.sequence
        );
        let target = temp.0.join(format!("receipt-{}.redb", expected.sequence));
        backup::incremental::restore(
            &archive,
            &target,
            expected.sequence,
            ArchiveLimits::default(),
        )
        .unwrap();
        let present = match index {
            0 => vec![],
            1 => vec![("first", &replies[0])],
            2 => vec![("first", &replies[0]), ("retained", &replies[1])],
            _ => vec![("retained", &replies[1])],
        };
        receipt_restore(&target, &expected, &present, index == 3);
    }
}

const ATOMIC_SCRIPT: &str = "update items | filter id == 1 && score == 0 | set score = score + 1\nexpect affected == 1\nupdate items | filter id == 1 | set score = score + 1 | returning {id, score}\nexpect affected == 1";

fn atomic_request(source: &str) -> Request {
    Request::query("lost-response", source)
        .with_version(PRODUCTION_VERSION)
        .unwrap()
        .with_idempotency_key("atomic-first")
        .unwrap()
}

#[test]
#[ignore = "subprocess helper: exits after commit without returning a response"]
fn atomic_receipt_commit_exit_child() {
    let Some(path) = std::env::var_os("UNIONID_MATRIX_COMMIT_DB") else {
        return;
    };
    let mut engine = Engine::open_redb(std::path::PathBuf::from(path)).unwrap();
    let reply = execute_protocol_request(&mut engine, atomic_request(ATOMIC_SCRIPT));
    assert!(reply.ok, "{:?}", reply.error);
    assert!(!reply.idempotency.unwrap().replayed);
    // Neither the response nor an Engine destructor reaches the parent.
    std::process::exit(0);
}

fn atomic_replay(path: &Path, expected: &State) {
    const COMBINATION: &str = "journal+atomic-expect+receipt+commit-exit";
    assert_eq!(state(path, COMBINATION), *expected);
    let mut engine = Engine::open_redb(path).unwrap();
    let reply = execute_protocol_request(&mut engine, atomic_request(ATOMIC_SCRIPT));
    assert!(reply.ok, "{COMBINATION}: {:?}", reply.error);
    assert!(reply.idempotency.as_ref().unwrap().replayed);
    assert_eq!(
        reply.idempotency.as_ref().unwrap().committed_sequence,
        expected.sequence.to_string()
    );
    assert_eq!(reply.statements.len(), 4);
    assert_eq!(reply.schema, Some(expected.schema.clone()));
    assert_eq!(
        reply.typed_rows::<serde_json::Value>().unwrap(),
        vec![serde_json::json!({"id": 1, "score": 2})]
    );
    drop(engine);
    assert_eq!(state(path, COMBINATION), *expected);
}

#[test]
fn atomic_expect_receipt_survives_commit_exit_and_both_restore_paths() {
    const COMBINATION: &str = "journal+atomic-expect+receipt+commit-exit";
    let temp = TempDir::new();
    let path = temp.0.join("source.redb");
    let archive = temp.0.join("archive");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute("struct Item {id: int, score: int}\ntable items: Item {key id}\ninsert items {id: 1, score: 0}").ok);
    }
    backup::incremental::init(&path, &archive, Default::default()).unwrap();
    let before = state(&path, COMBINATION);
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let rejected = execute_protocol_request(
            &mut engine,
            atomic_request("update items | set score = 5\nexpect affected == 2"),
        );
        assert!(!rejected.ok);
        let error = rejected.error.unwrap();
        assert_eq!(error.code, "E_EXPECTATION");
        assert_eq!(error.statement_index, Some(2));
    }
    assert_eq!(state(&path, COMBINATION), before);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "atomic_receipt_commit_exit_child"])
        .env("UNIONID_MATRIX_COMMIT_DB", &path)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{COMBINATION}: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    let after = state(&path, COMBINATION);
    assert_eq!(after.sequence, before.sequence + 1);
    assert_eq!(after.receipts, 1);
    atomic_replay(&path, &after);

    let logical = temp.0.join("logical.json");
    backup::create(&path, &logical).unwrap();
    let restored = temp.0.join("logical.redb");
    backup::restore(&logical, &restored).unwrap();
    atomic_replay(&restored, &after);
    backup::incremental::export(&path, &archive, Default::default()).unwrap();
    backup::incremental::verify(&archive, ArchiveLimits::default()).unwrap();
    for (expected, replay) in [(&before, false), (&after, true)] {
        eprintln!(
            "combination={COMBINATION}, restoring sequence={}",
            expected.sequence
        );
        let target = temp.0.join(format!("atomic-{}.redb", expected.sequence));
        backup::incremental::restore(
            &archive,
            &target,
            expected.sequence,
            ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(state(&target, COMBINATION), *expected);
        if replay {
            atomic_replay(&target, expected);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ReferenceState {
    items: State,
    parents: Value,
}

fn reference_state(path: &Path) -> ReferenceState {
    let items = state(path, "journal+existing-references+key-migration");
    let mut engine = Engine::open_redb(path).unwrap();
    let response = engine.execute("from parents | sort id");
    assert!(response.ok, "{:?}", response.error);
    ReferenceState {
        items,
        parents: serde_json::to_value(response.rows).unwrap(),
    }
}

fn reference_rejections(path: &Path, text_key: bool) {
    let before = reference_state(path);
    let mut engine = Engine::open_redb(path).unwrap();
    let orphan = if text_key {
        "insert items {id: 2, parent: \"missing\"}"
    } else {
        "insert items {id: 2, parent: 99}"
    };
    for (source, kind) in [
        (orphan, unionid::error::ConstraintKind::ReferenceMissing),
        (
            "delete parents",
            unionid::error::ConstraintKind::ReferenceRestricted,
        ),
    ] {
        let response = engine.execute(source);
        assert!(!response.ok, "accepted {source}");
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_CONSTRAINT");
        assert_eq!(error.constraint, Some(kind));
    }
    drop(engine);
    assert_eq!(reference_state(path), before);
}

#[test]
fn existing_references_and_target_key_conversion_survive_both_restore_paths() {
    const COMBINATION: &str = "journal+existing-references+key-migration";
    let temp = TempDir::new();
    let path = temp.0.join("source.redb");
    let archive = temp.0.join("archive");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(12).unwrap();
        let response = engine.execute("struct Parent {id: int}\nstruct Item {id: int, parent: int}\ntable parents: Parent {key id}\ntable items: Item {key id}\ninsert parents {id: 7}\ninsert items {id: 1, parent: 99}");
        assert!(response.ok, "{COMBINATION}: {:?}", response.error);
    }
    backup::incremental::init(&path, &archive, Default::default()).unwrap();
    let mut checkpoints = vec![reference_state(&path)];
    let link = MigrationFile::parse(
        "migration link\n  add reference items (parent) references parents (id)",
    )
    .unwrap();
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let error = engine
            .apply_migrations(std::slice::from_ref(&link))
            .unwrap_err();
        assert_eq!(error.code, "E_CONSTRAINT", "{error:?}");
        assert_eq!(
            error.constraint,
            Some(unionid::error::ConstraintKind::ReferenceMissing)
        );
    }
    assert_eq!(reference_state(&path), checkpoints[0]);
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute("update items | set parent = 7").ok);
    }
    checkpoints.push(reference_state(&path));
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        engine
            .apply_migrations(std::slice::from_ref(&link))
            .unwrap();
    }
    checkpoints.push(reference_state(&path));
    reference_rejections(&path, false);
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let orphan_key = MigrationFile::parse("migration orphan_key\n  parent link\n  drop reference items (parent) references parents (id)\n  change field Parent.id to int\n    using old -> 99\n  add reference items (parent) references parents (id)").unwrap();
        let error = engine
            .apply_migrations(&[link.clone(), orphan_key])
            .unwrap_err();
        assert_eq!(error.code, "E_CONSTRAINT", "{error:?}");
        assert_eq!(
            error.constraint,
            Some(unionid::error::ConstraintKind::ReferenceMissing)
        );
    }
    assert_eq!(reference_state(&path), checkpoints[2]);
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let conversion = MigrationFile::parse("migration text_key\n  parent link\n  drop reference items (parent) references parents (id)\n  change field Parent.id to text\n    using old -> \"seven\"\n  change field Item.parent to text\n    using old -> \"seven\"\n  add reference items (parent) references parents (id)").unwrap();
        engine.apply_migrations(&[link, conversion]).unwrap();
        assert!(
            engine
                .execute("from parents | filter id == \"seven\"")
                .rows
                .len()
                == 1
        );
        assert!(
            engine
                .execute("from items | filter parent == \"seven\"")
                .rows
                .len()
                == 1
        );
    }
    checkpoints.push(reference_state(&path));
    reference_rejections(&path, true);
    for pair in checkpoints.windows(2) {
        assert_eq!(pair[1].items.sequence, pair[0].items.sequence + 1);
    }
    let logical = temp.0.join("logical.json");
    backup::create(&path, &logical).unwrap();
    let restored = temp.0.join("logical.redb");
    backup::restore(&logical, &restored).unwrap();
    assert_eq!(reference_state(&restored), *checkpoints.last().unwrap());
    reference_rejections(&restored, true);
    backup::incremental::export(&path, &archive, Default::default()).unwrap();
    backup::incremental::verify(&archive, ArchiveLimits::default()).unwrap();
    for (index, expected) in checkpoints.into_iter().enumerate() {
        eprintln!(
            "combination={COMBINATION}, restoring sequence={}",
            expected.items.sequence
        );
        let target = temp
            .0
            .join(format!("references-{}.redb", expected.items.sequence));
        backup::incremental::restore(
            &archive,
            &target,
            expected.items.sequence,
            ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(reference_state(&target), expected);
        if index >= 2 {
            reference_rejections(&target, index == 3);
        }
    }
}

fn wire_rows(
    snapshot: &State,
) -> Vec<std::collections::BTreeMap<String, unionid::protocol::WireValue>> {
    let rows: Vec<std::collections::BTreeMap<String, unionid::Value>> =
        serde_json::from_value(snapshot.rows.clone()).unwrap();
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(key, value)| (key, unionid::protocol::WireValue::from(&value)))
                .collect()
        })
        .collect()
}

fn read_only_service(path: &Path, expected: &State) {
    let mut server = common::Server::start(&["--db", path.to_str().unwrap(), "--read-only"]);
    let request = |source: &str| {
        Request::query("matrix-read-only", source)
            .with_version(PRODUCTION_VERSION)
            .unwrap()
    };
    let rows = unionid::cli::send_request(&server.addr, &request("from items | sort id")).unwrap();
    assert!(rows.ok, "{:?}", rows.error);
    assert_eq!(rows.schema, Some(expected.schema.clone()));
    assert_eq!(rows.rows, wire_rows(expected));
    let rejected = unionid::cli::send_request(
        &server.addr,
        &request("migration readonly_probe {\nadd field Item.audit int = 0\n}"),
    )
    .unwrap();
    assert!(!rejected.ok);
    assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
    server.shutdown();
    assert_eq!(state(path, "read-only+phased-migration"), *expected);
}

#[test]
fn concurrent_snapshots_cross_migration_reclaim_then_restore_to_read_only_services() {
    use std::time::{Duration, Instant};
    use unionid::server::ConcurrentEngine;
    use unionid::stream::{self, Frame};

    const COMBINATION: &str = "journal+phased-migration+concurrent-snapshots+read-only";
    let temp = TempDir::new();
    let path = temp.0.join("source.redb");
    let archive = temp.0.join("archive");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute("enum State {Pending, Done}\nstruct Item {id: int, score: int, state: State}\ntable items: Item {key id}").ok);
        let rows = (0..32)
            .map(|id| {
                format!(
                    "{{id: {id}, score: {}, state: {}}}",
                    id % 2,
                    if id % 2 == 0 { "Pending" } else { "Done" }
                )
            })
            .collect::<Vec<_>>()
            .join(",\n");
        let response = engine.execute(&format!("insert many items [{rows}]"));
        assert!(response.ok, "{:?}", response.error);
    }
    backup::incremental::init(&path, &archive, Default::default()).unwrap();
    let before = state(&path, COMBINATION);
    let shared = ConcurrentEngine::new(Engine::open_redb(&path).unwrap());
    let mut receivers = Vec::new();
    for id in 0..2 {
        let receiver = stream::accept(
            &shared,
            Request::query(format!("snapshot-{id}"), "from items | sort id")
                .with_version(PRODUCTION_VERSION)
                .unwrap(),
            Instant::now() + Duration::from_secs(30),
            None,
        )
        .unwrap()
        .start();
        let frame: Frame = serde_json::from_slice(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        let Frame::Schema { schema, .. } = frame else {
            panic!("{COMBINATION}: {frame:?}")
        };
        assert_eq!(schema, before.schema);
        receivers.push(receiver);
    }
    // 32 rows cannot fit in the eight-frame channel: both snapshots stay
    // alive until drained, without sleeps or a race against query completion.
    assert_eq!(shared.stats().active_reads, 2);
    let mut checkpoints = vec![before];
    let mut files = Vec::new();
    for source in MIGRATIONS {
        files.push(MigrationFile::parse(source).unwrap());
        let mut complete = false;
        for _ in 0..32 {
            let progress =
                shared.with_exclusive(|engine| engine.advance_migrations(&files, 1).unwrap());
            if progress.complete {
                complete = true;
                break;
            }
        }
        assert!(complete, "{COMBINATION}: {source}");
        assert_eq!(shared.stats().active_reads, 2);
        // Physical check is intentionally deferred until retained snapshots
        // are released; redb rejects it while a read transaction is alive.
        let snapshot = shared.with_exclusive(|engine| inspect(engine, COMBINATION));
        assert_eq!(snapshot.sequence, checkpoints.last().unwrap().sequence + 1);
        assert_eq!(snapshot.ledger.len(), files.len());
        checkpoints.push(snapshot);
    }
    for receiver in receivers {
        let mut rows = Vec::new();
        loop {
            let frame: Frame = serde_json::from_slice(
                receiver
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
            match frame {
                Frame::Row { row, .. } => rows.push(row),
                Frame::Complete { row_count, .. } => {
                    assert_eq!(row_count, "32");
                    break;
                }
                other => panic!("{COMBINATION}: {other:?}"),
            }
        }
        assert_eq!(rows, wire_rows(&checkpoints[0]));
    }
    assert_eq!(shared.stats().active_reads, 0);
    assert!(shared.stats().peak_active_reads >= 2);
    shared.with_exclusive(|engine| {
        engine.check_integrity().unwrap();
    });
    drop(shared);
    assert_eq!(state(&path, COMBINATION), *checkpoints.last().unwrap());
    let logical = temp.0.join("logical.json");
    backup::create(&path, &logical).unwrap();
    let restored = temp.0.join("logical.redb");
    backup::restore(&logical, &restored).unwrap();
    assert_eq!(state(&restored, COMBINATION), *checkpoints.last().unwrap());
    read_only_service(&restored, checkpoints.last().unwrap());
    backup::incremental::export(&path, &archive, Default::default()).unwrap();
    backup::incremental::verify(&archive, ArchiveLimits::default()).unwrap();
    for expected in checkpoints {
        eprintln!(
            "combination={COMBINATION}, restoring sequence={}",
            expected.sequence
        );
        let target = temp.0.join(format!("snapshot-{}.redb", expected.sequence));
        backup::incremental::restore(
            &archive,
            &target,
            expected.sequence,
            ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(state(&target, COMBINATION), expected);
        read_only_service(&target, &expected);
    }
}
