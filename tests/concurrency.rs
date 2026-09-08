use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use unionid::protocol::{PRODUCTION_VERSION, Request};
use unionid::server::ConcurrentEngine;
use unionid::{Engine, IntrospectionKind};

struct TempDatabase(PathBuf);

impl TempDatabase {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "unionid-concurrency-{}-{nonce}.redb",
            std::process::id()
        )))
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn redb_protocol_reads_share_complete_commits_and_storage_identity() {
    let database = TempDatabase::new();
    let shared = ConcurrentEngine::new(Engine::open_redb(&database.0).unwrap());
    assert!(shared.execute("create table items (id int, value text)").ok);
    assert!(shared.execute("insert items {id: 1, value: \"one\"}").ok);

    let query = Request::query("read-v2", "from items | sort id")
        .with_version(PRODUCTION_VERSION)
        .unwrap();
    let response = shared.execute_protocol_request(query);
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.version, PRODUCTION_VERSION);
    assert_eq!(response.rows.len(), 1);

    let introspection = Request::introspection("storage-v2", IntrospectionKind::Storage)
        .with_version(PRODUCTION_VERSION)
        .unwrap();
    let response = shared.execute_protocol_request(introspection);
    assert!(response.ok, "{}", response.message);
    let introspection = response.introspection.unwrap();
    assert_eq!(introspection.schema.revision, 1);
    assert_eq!(introspection.storage_versions.unwrap().format, 5);
    assert!(!introspection.read_only);

    drop(shared);
    let mut reopened = Engine::open_redb(&database.0).unwrap();
    let rows = reopened.execute("from items | sort id");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn malformed_and_mutating_requests_keep_existing_protocol_errors() {
    let shared = ConcurrentEngine::new(Engine::memory());
    let malformed = shared.execute_protocol_request(Request::query("bad", "insert ???"));
    assert!(!malformed.ok);
    assert_eq!(malformed.error.unwrap().code, "E_SYNTAX");

    let created =
        shared.execute_protocol_request(Request::query("create", "create table items (id int)"));
    assert!(created.ok, "{}", created.message);
    let read = shared.execute_protocol_request(Request::query("read", "from items"));
    assert!(read.ok, "{}", read.message);
    assert_eq!(read.schema, created.schema);
}

#[test]
fn cancelled_redb_read_has_no_database_effect_and_survives_reopen() {
    let database = TempDatabase::new();
    let shared = ConcurrentEngine::new(Engine::open_redb(&database.0).unwrap());
    assert!(shared.execute("create table items (id int, value text)").ok);
    assert!(shared.execute("insert items {id: 1, value: \"kept\"}").ok);

    let operation = shared
        .register_read(
            Request::query("cancel-redb", "from items | sort id"),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    let operation_id = operation.id().to_owned();
    assert_eq!(
        shared.cancel(&operation_id).unwrap().status,
        unionid::server::CancelStatus::Accepted
    );
    let response = operation.start();
    assert_eq!(response.error.unwrap().code, "E_CANCELLED");

    let rows = shared.execute("from items | sort id");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 1);
    drop(shared);

    let mut reopened = Engine::open_redb(&database.0).unwrap();
    let rows = reopened.execute("from items | sort id");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 1);
}
