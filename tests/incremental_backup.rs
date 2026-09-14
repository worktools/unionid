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

    let middle_target = temp.path().join("middle.redb");
    backup::incremental::restore(
        &repo,
        &middle_target,
        initialized.baseline_sequence + 1,
        ArchiveLimits::default(),
    )
    .unwrap();
    let mut middle = Engine::open_redb(middle_target).unwrap();
    let response = middle.execute("from items | filter label == \"two\"");
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

#[test]
fn checkpoint_prune_and_confirmed_disable_bound_retained_history() {
    let temp = TempTree::new("incremental-lifecycle");
    let db = temp.path().join("app.redb");
    let repo = temp.path().join("archive");
    create_database(&db);
    backup::incremental::init(&db, &repo, Default::default()).unwrap();

    let mut engine = Engine::open_redb(db.clone()).unwrap();
    assert!(engine.execute("update items | set label = \"two\"").ok);
    assert!(engine.execute("update items | set label = \"three\"").ok);
    drop(engine);
    backup::incremental::export(&db, &repo, Default::default()).unwrap();

    let checkpoint = backup::incremental::checkpoint(&db, &repo, Default::default()).unwrap();
    assert!(checkpoint.recoverable_first_sequence > checkpoint.previous_first_sequence);
    let listed = backup::incremental::list(&repo).unwrap();
    assert_eq!(
        listed.recoverable_first_sequence,
        checkpoint.recoverable_first_sequence
    );
    assert_eq!(
        listed.recoverable_last_sequence,
        checkpoint.recoverable_first_sequence
    );
    assert!(listed.segments.is_empty());
    backup::incremental::verify(&repo, ArchiveLimits::default()).unwrap();

    let floor = checkpoint.recoverable_first_sequence.to_string();
    let cli_preview = std::process::Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "backup",
            "incremental",
            "prune",
            "--repo",
            repo.to_str().unwrap(),
            "--before-sequence",
            &floor,
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(cli_preview.status.success());
    let cli_preview: serde_json::Value = serde_json::from_slice(&cli_preview.stdout).unwrap();
    assert_eq!(cli_preview["applied"], false);

    let preview = backup::incremental::prune(
        &repo,
        checkpoint.recoverable_first_sequence,
        false,
        ArchiveLimits::default(),
    )
    .unwrap();
    assert!(!preview.applied);
    assert!(!preview.selected_files.is_empty());
    for relative in &preview.selected_files {
        assert!(repo.join(relative).exists());
    }
    let applied = backup::incremental::prune(
        &repo,
        checkpoint.recoverable_first_sequence,
        true,
        ArchiveLimits::default(),
    )
    .unwrap();
    assert!(applied.applied);
    assert_eq!(applied.selected_files, preview.selected_files);
    assert!(
        backup::incremental::prune(
            &repo,
            checkpoint.recoverable_first_sequence,
            true,
            ArchiveLimits::default(),
        )
        .unwrap()
        .selected_files
        .is_empty()
    );

    let mut engine = Engine::open_redb(db.clone()).unwrap();
    assert!(engine.execute("update items | set label = \"four\"").ok);
    drop(engine);
    assert!(backup::incremental::disable(&db, &repo, false, false).is_err());
    let preview = backup::incremental::disable(&db, &repo, true, false).unwrap();
    assert!(!preview.applied);
    assert_eq!(
        preview.lost_first_sequence,
        Some(checkpoint.recoverable_first_sequence + 1)
    );
    let disabled = backup::incremental::disable(&db, &repo, true, true).unwrap();
    assert!(disabled.applied);
    assert_eq!(disabled.lost_first_sequence, preview.lost_first_sequence);
    assert_eq!(
        backup::incremental::list(&repo).unwrap().state,
        unionid::backup::incremental::ManifestState::Sealed
    );
    assert_eq!(
        Engine::open_redb(&db)
            .unwrap()
            .backup_journal_status()
            .unwrap()
            .state,
        unionid::backup::incremental::BackupJournalState::Disabled
    );

    let no_op = std::process::Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "backup",
            "incremental",
            "disable",
            "--db",
            db.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(no_op.status.success());
    assert_eq!(
        String::from_utf8(no_op.stdout).unwrap(),
        format!(
            "incremental archive already sealed at sequence {}\n",
            checkpoint.recoverable_last_sequence
        )
    );
}
