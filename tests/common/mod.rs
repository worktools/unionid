#![allow(dead_code)]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redb::{Database as RedbDatabase, Durability, TableDefinition};
use unionid::Engine;

static NEXT: AtomicU64 = AtomicU64::new(0);

const REDB_META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const REDB_CATALOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("catalog");
const REDB_ROWS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("rows");
const REDB_SECONDARY_INDEX: TableDefinition<&[u8], u8> = TableDefinition::new("secondary_index");

/// Rewrite a freshly created file as an empty storage-format-3 database so tests
/// can exercise the explicit upgrade chain from the oldest bounded format.
pub fn create_empty_format3(path: &std::path::Path) {
    drop(Engine::open_redb(path).unwrap());
    let database = RedbDatabase::open(path).unwrap();
    let mut transaction = database.begin_write().unwrap();
    transaction.set_durability(Durability::Immediate).unwrap();
    transaction.set_two_phase_commit(true);
    transaction.open_table(REDB_CATALOG).unwrap();
    transaction.open_table(REDB_ROWS).unwrap();
    transaction.open_table(REDB_SECONDARY_INDEX).unwrap();
    let mut meta = transaction.open_table(REDB_META).unwrap();
    for (key, value) in [
        ("storage_format_version", 3_u32.to_be_bytes().to_vec()),
        ("catalog_codec_version", 2_u16.to_be_bytes().to_vec()),
        ("value_codec_version", 1_u16.to_be_bytes().to_vec()),
        ("index_key_version", 1_u16.to_be_bytes().to_vec()),
        ("migration_codec_version", 1_u16.to_be_bytes().to_vec()),
        ("receipt_codec_version", 1_u16.to_be_bytes().to_vec()),
    ] {
        meta.insert(key, value.as_slice()).unwrap();
    }
    drop(meta);
    transaction.commit().unwrap();
    drop(database);
}

/// Build an empty database at a supported legacy format (6, 7, 8, or 9) via the
/// public upgrade chain, leaving the file closed.
pub fn create_empty_format(path: &std::path::Path, target: u32) {
    create_empty_format3(path);
    let mut engine = Engine::open_redb(path).unwrap();
    for step in [4, 5, 6, 8] {
        if step > target {
            break;
        }
        engine.upgrade_storage(step).unwrap();
    }
    if target == 7 || target == 9 {
        let status = engine.backup_journal_status().unwrap();
        engine
            .enable_backup_journal(unionid::BackupJournalConfig::new(
                "legacy",
                status.head_sequence,
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ))
            .unwrap();
    }
    assert_eq!(
        engine.introspection().storage_versions.unwrap().format,
        target
    );
}

pub struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "unionid-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Server {
    child: Child,
    pub addr: String,
}
impl Server {
    pub fn start(options: &[&str]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["server", "--addr", "127.0.0.1:0"])
            .args(options)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            addr: String::new(),
        };
        let stdout = server.child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server startup deadline")
            .unwrap();
        server.addr = line
            .trim()
            .strip_prefix("unionid server listening on ")
            .expect("server readiness message")
            .to_string();
        server
    }

    pub fn shutdown(&mut self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success(), "failed to send SIGTERM to server");
        wait(&mut self.child);
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn wait(child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI failed to exit before deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
