mod common;

use common::TempDir;
use serde_json::Value;
use std::path::Path;
use unionid::backup::incremental::ArchiveLimits;
use unionid::migration::MigrationEntry;
use unionid::{Engine, MigrationFile, MigrationMaintenancePhase, SchemaInfo, backup};

#[derive(Debug, PartialEq, Eq)]
struct State {
    schema: SchemaInfo,
    ledger: Vec<MigrationEntry>,
    sequence: u64,
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
