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

fn state(path: &Path, combination: &str) -> State {
    let mut engine = Engine::open_redb(path).unwrap();
    engine.check_integrity().unwrap();
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
    for source in [
        "migration add_note\n  add field Item.note text = \"migrated\"",
        "migration score_to_bool\n  parent add_note\n  change field Item.score to bool\n    using old -> old > 0",
        "migration remove_pending\n  parent score_to_bool\n  drop variant State.Pending\n    using old -> State.Done",
    ] {
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
