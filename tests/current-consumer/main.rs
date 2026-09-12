use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use unionid::backup;
use unionid::migration::MigrationFile;
use unionid::protocol::{PRODUCTION_VERSION, Request, Response};
use unionid::scalars::{Timestamp, Uuid};
use unionid::server::execute_protocol_request;
use unionid::{Engine, PageSpec};

mod sdk;

const INITIAL: &str = r#"migration m0001_current_consumer
  add type State = Draft | Published {at timestamp}
  add type Entry =
    id uuid
    title text
    state State
    tags list text = []
    note option text = None
    created_at timestamp
  add table entries Entry key id
  add index entries (created_at, id)
"#;

const UPGRADE: &str = r#"migration m0002_current_consumer_priority
  parent m0001_current_consumer
  add field Entry.priority int = 1
  add index entries (priority, created_at, id)
"#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum State {
    Draft,
    Published { at: Timestamp },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EntryV1 {
    id: Uuid,
    title: String,
    state: State,
    tags: Vec<String>,
    note: Option<String>,
    created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct EntryV2 {
    id: Uuid,
    title: String,
    state: State,
    tags: Vec<String>,
    note: Option<String>,
    created_at: Timestamp,
    priority: i64,
}

fn request(id: &str, source: &str) -> Result<Request, unionid::Error> {
    Request::query(id, source).with_version(PRODUCTION_VERSION)
}

fn checked(engine: &mut Engine, request: Request) -> Result<Response, unionid::Error> {
    let response = execute_protocol_request(engine, request);
    if response.ok {
        Ok(response)
    } else {
        Err(response.error.unwrap_or_else(|| {
            unionid::Error::new("E_CONSUMER", "request failed without a structured error")
        }))
    }
}

fn rows() -> Result<Vec<EntryV1>, unionid::Error> {
    let published_at = Timestamp::from_str("2026-09-10T10:00:00Z")?;
    Ok(vec![
        EntryV1 {
            id: Uuid::from_str("018f0000-0000-7000-8000-000000000001")?,
            title: "draft".into(),
            state: State::Draft,
            tags: vec!["current".into(), "typed".into()],
            note: None,
            created_at: Timestamp::from_str("2026-09-10T09:00:00Z")?,
        },
        EntryV1 {
            id: Uuid::from_str("018f0000-0000-7000-8000-000000000002")?,
            title: "published".into(),
            state: State::Published { at: published_at },
            tags: vec!["current".into(), "protocol-v2".into()],
            note: Some("packaged consumer".into()),
            created_at: published_at,
        },
    ])
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: current-consumer <empty-work-directory>")?,
    );
    std::fs::create_dir_all(&root)?;
    let database = root.join("current.redb");
    let restored = root.join("restored.redb");
    let archive = root.join("current.backup.json");
    if [&database, &restored, &archive]
        .iter()
        .any(|path| path.try_exists().unwrap_or(true))
    {
        return Err("current consumer requires an empty work directory".into());
    }

    let initial = MigrationFile::parse(INITIAL)?;
    let upgrade = MigrationFile::parse(UPGRADE)?;
    let expected = rows()?;
    let mut engine = Engine::open_redb(&database)?;
    engine.apply_migrations(std::slice::from_ref(&initial))?;
    assert_eq!(engine.introspection().storage_versions.unwrap().format, 6);

    let insert = request("insert-attempt-1", "insert many entries $rows\nreturning")?
        .with_serde_param("rows", &expected)?
        .with_idempotency_key("current-consumer-seed")?;
    let first = checked(&mut engine, insert.clone())?;
    assert!(!first.idempotency.as_ref().unwrap().replayed);
    assert_eq!(first.typed_rows::<EntryV1>()?, expected);

    // Deliberately retry the exact mutation with a new attempt identity, as an
    // application would after losing the first response.
    let replay = checked(
        &mut engine,
        Request {
            request_id: "insert-attempt-2".into(),
            ..insert
        },
    )?;
    assert!(replay.idempotency.as_ref().unwrap().replayed);
    assert_eq!(replay.typed_rows::<EntryV1>()?, expected);

    let query = "from entries\nsort {created_at, id}";
    let first_page = checked(
        &mut engine,
        request("page-1", query)?.with_page(PageSpec::forward(1)),
    )?
    .typed_page::<EntryV1>()?;
    assert_eq!(first_page.rows, expected[..1]);
    let next_page = first_page.page.next_page().unwrap();

    drop(engine);
    let mut engine = Engine::open_redb(&database)?;
    let second_page = checked(
        &mut engine,
        request("page-2-after-restart", query)?.with_page(next_page.clone()),
    )?
    .typed_page::<EntryV1>()?;
    assert_eq!(second_page.rows, expected[1..]);
    assert!(second_page.page.next_page().is_none());

    engine.apply_migrations(&[initial, upgrade])?;
    let stale = execute_protocol_request(
        &mut engine,
        request("stale-page", query)?.with_page(next_page),
    );
    assert_eq!(
        stale.error.as_ref().map(|error| error.code.as_str()),
        Some("E_CURSOR_SCHEMA")
    );

    let upgraded = checked(
        &mut engine,
        request("after-migration", "from entries\nsort {created_at, id}")?,
    )?
    .typed_rows::<EntryV2>()?;
    assert_eq!(upgraded.len(), expected.len());
    assert!(upgraded.iter().all(|entry| entry.priority == 1));
    let integrity = engine.check_integrity()?;
    assert!(integrity.backend_clean);
    assert_eq!(integrity.versions.format, 6);
    drop(engine);

    let created = backup::create(&database, &archive)?;
    let recovered = backup::restore(&archive, &restored)?;
    assert_eq!(created, recovered);
    let mut restored_engine = Engine::open_redb(&restored)?;
    let restored_rows = checked(
        &mut restored_engine,
        request("restored", "from entries\nsort {created_at, id}")?,
    )?
    .typed_rows::<EntryV2>()?;
    assert_eq!(restored_rows, upgraded);
    assert_eq!(restored_engine.check_integrity()?.versions.format, 6);

    sdk::verify().await?;

    println!(
        "current-consumer: packaged Engine, TCP, async TCP and HTTP typed SDK journeys passed"
    );
    Ok(())
}
