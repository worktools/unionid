use std::path::{Path, PathBuf};

use unionid::backup::incremental::{
    ArchiveLimits, Compression, IncrementalExportOptions, IncrementalInitOptions,
};
use unionid::{Engine, backup};

struct TempTree(PathBuf);

impl TempTree {
    fn new(name: &str) -> Self {
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!(
            "unionid-{name}-{}-{:x}",
            std::process::id(),
            u64::from_be_bytes(nonce)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create_database(path: &Path) {
    let mut engine = Engine::open_redb(path.to_path_buf()).unwrap();
    assert!(
        engine
            .execute(
                r#"
type Item = {
  id int,
  label text,
}

table items Item
  key id

insert items {id = 1, label = "one"}
"#,
            )
            .ok
    );
}

#[test]
fn init_export_list_verify_and_retry_form_a_contiguous_chain() {
    let temp = TempTree::new("incremental-workflow");
    let db = temp.path().join("app.redb");
    let repo = temp.path().join("archive");
    create_database(&db);

    let init = backup::incremental::init(
        &db,
        &repo,
        IncrementalInitOptions {
            compression: Compression::None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(init.previous_storage_format, 6);
    assert_eq!(init.current_storage_format, 7);
    assert!(!init.resumed);

    let mut engine = Engine::open_redb(db.clone()).unwrap();
    for label in ["two", "three", "four", "five", "six"] {
        assert!(
            engine
                .execute(&format!(
                    "update items | filter id == 1 | set label = \"{label}\""
                ))
                .ok
        );
    }
    drop(engine);

    let exported = backup::incremental::export(
        &db,
        &repo,
        IncrementalExportOptions {
            compression: Compression::None,
            max_commits_per_segment: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(exported.exported_commits, 5);
    assert_eq!(exported.created_segments, 3);
    let listed = backup::incremental::list(&repo).unwrap();
    assert_eq!(listed.segments.len(), 3);
    assert_eq!(listed.recoverable_last_sequence, init.baseline_sequence + 5);
    let verified = backup::incremental::verify(&repo, ArchiveLimits::default()).unwrap();
    assert_eq!(verified.segment_count, 3);

    let status = Engine::open_redb(db.clone())
        .unwrap()
        .backup_journal_status()
        .unwrap();
    assert_eq!(status.commit_count, 0);
    assert_eq!(
        status.exported_sequence,
        Some(listed.recoverable_last_sequence)
    );
    let retry = backup::incremental::export(&db, &repo, Default::default()).unwrap();
    assert!(retry.no_op);

    // Routine export authenticates the baseline and archive head only. Older
    // immutable segments remain the explicit, potentially expensive `verify`
    // surface rather than making each newly sealed commit scan the whole chain.
    let first_segment = repo.join(&listed.segments[0].path);
    let mut bytes = std::fs::read(&first_segment).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&first_segment, bytes).unwrap();
    let mut engine = Engine::open_redb(db.clone()).unwrap();
    assert!(
        engine
            .execute("update items | filter id == 1 | set label = \"seven\"")
            .ok
    );
    drop(engine);
    let next = backup::incremental::export(&db, &repo, Default::default()).unwrap();
    assert_eq!(next.exported_commits, 1);
    assert!(backup::incremental::verify(&repo, ArchiveLimits::default()).is_err());
}

#[test]
fn verify_rejects_changed_artifact_and_list_reports_orphans() {
    let temp = TempTree::new("incremental-corruption");
    let db = temp.path().join("app.redb");
    let repo = temp.path().join("archive");
    create_database(&db);
    backup::incremental::init(&db, &repo, Default::default()).unwrap();

    std::fs::write(repo.join("segments/orphan.uis"), b"orphan").unwrap();
    let listed = backup::incremental::list(&repo).unwrap();
    let verified = backup::incremental::verify(&repo, ArchiveLimits::default()).unwrap();
    assert_eq!(verified.orphan_files, ["segments/orphan.uis"]);

    let baseline = repo.join(&listed.baseline.path);
    let mut bytes = std::fs::read(&baseline).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(baseline, bytes).unwrap();
    let error = backup::incremental::verify(&repo, ArchiveLimits::default()).unwrap_err();
    assert!(matches!(
        error.code.as_str(),
        "E_BACKUP_ARCHIVE" | "E_BACKUP_CHAIN"
    ));
}

#[test]
fn restore_replays_each_declared_sequence_to_a_fresh_database() {
    let temp = TempTree::new("incremental-restore");
    let db = temp.path().join("app.redb");
    let repo = temp.path().join("archive");
    create_database(&db);
    let initialized = backup::incremental::init(
        &db,
        &repo,
        IncrementalInitOptions {
            compression: Compression::None,
            ..Default::default()
        },
    )
    .unwrap();
    let mut engine = Engine::open_redb(db.clone()).unwrap();
    for label in ["two", "three"] {
        assert!(
            engine
                .execute(&format!(
                    "update items | filter id == 1 | set label = \"{label}\""
                ))
                .ok
        );
    }
    drop(engine);
    backup::incremental::export(
        &db,
        &repo,
        IncrementalExportOptions {
            compression: Compression::None,
            ..Default::default()
        },
    )
    .unwrap();

    let baseline_target = temp.path().join("baseline.redb");
    let baseline = backup::incremental::restore(
        &repo,
        &baseline_target,
        initialized.baseline_sequence,
        ArchiveLimits::default(),
    )
    .unwrap();
    assert_eq!(baseline.restored_sequence, initialized.baseline_sequence);
    let mut restored = Engine::open_redb(baseline_target).unwrap();
    assert!(restored.check_integrity().unwrap().backend_clean);
    assert!(restored.execute("from items | filter label == \"one\"").ok);

    let final_target = temp.path().join("final.redb");
    let final_sequence = initialized.baseline_sequence + 2;
    backup::incremental::restore(
        &repo,
        &final_target,
        final_sequence,
        ArchiveLimits::default(),
    )
    .unwrap();
    let mut restored = Engine::open_redb(&final_target).unwrap();
    assert!(restored.check_integrity().unwrap().backend_clean);
    let response = restored.execute("from items | filter label == \"three\"");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);

    let cli_target = temp.path().join("cli.redb");
    let final_sequence_text = final_sequence.to_string();
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "restore",
            "incremental",
            "--repo",
            repo.to_str().unwrap(),
            "--db",
            cli_target.to_str().unwrap(),
            "--at-sequence",
            &final_sequence_text,
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(report["restored_sequence"], final_sequence);

    let existing = backup::incremental::restore(
        &repo,
        &final_target,
        final_sequence,
        ArchiveLimits::default(),
    )
    .unwrap_err();
    assert_eq!(existing.code, "E_BACKUP");

    let after_head = backup::incremental::restore(
        &repo,
        temp.path().join("after-head.redb"),
        final_sequence + 1,
        ArchiveLimits::default(),
    )
    .unwrap_err();
    assert_eq!(after_head.code, "E_BACKUP_AFTER_HEAD");

    let error = backup::incremental::restore(
        &repo,
        temp.path().join("outside.redb"),
        initialized.baseline_sequence - 1,
        ArchiveLimits::default(),
    )
    .unwrap_err();
    assert_eq!(error.code, "E_BACKUP_BEFORE_BASELINE");
}
