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
    assert!(listed.gaps.is_empty());
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
