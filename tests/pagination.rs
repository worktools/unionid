mod common;

use std::collections::BTreeMap;

use common::TempDir;
use redb::ReadableDatabase;
use unionid::protocol::Request;
use unionid::server::execute_protocol_request;
use unionid::{Engine, PageAccessKind, PageDirection, PageSpec, Value};

const BASE_QUERY: &str = "from tasks\nfilter active == true\nsort {-priority, id}";

fn setup(engine: &mut Engine) {
    let response = engine.execute(
        r#"type Task =
  id int
  priority int
  title text
  active bool
table tasks Task
  key id
insert tasks {id = 1, priority = 2, title = "one", active = true}
insert tasks {id = 2, priority = 2, title = "two", active = true}
insert tasks {id = 3, priority = 1, title = "three", active = true}
insert tasks {id = 4, priority = 1, title = "four", active = true}
insert tasks {id = 5, priority = 0, title = "five", active = true}
insert tasks {id = 6, priority = 9, title = "hidden", active = false}"#,
    );
    assert!(response.ok, "{}", response.message);
}

fn ids(response: &unionid::QueryResponse) -> Vec<i64> {
    response
        .rows
        .iter()
        .map(|row| match row.get("id") {
            Some(Value::Int(id)) => *id,
            other => panic!("expected id, got {other:?}"),
        })
        .collect()
}

#[test]
fn memory_pages_traverse_duplicate_prefixes_in_both_directions() {
    let mut engine = Engine::memory();
    setup(&mut engine);

    let first =
        engine.execute("from tasks | filter active == true | sort {-priority, id} | page 2");
    assert!(first.ok, "{}", first.message);
    assert_eq!(ids(&first), [1, 2]);
    let first_page = first.page.unwrap();
    assert!(first_page.has_more);
    assert!(first_page.previous_cursor.is_none());
    let first_next = first_page.next_cursor.unwrap();

    let second = engine.execute_page(BASE_QUERY, PageSpec::after(2, first_next));
    assert!(second.ok, "{}", second.message);
    assert_eq!(ids(&second), [3, 4]);
    let second_page = second.page.unwrap();
    assert!(second_page.has_more);
    let second_next = second_page.next_cursor.unwrap();
    let second_previous = second_page.previous_cursor.unwrap();

    let last = engine.execute_page(BASE_QUERY, PageSpec::after(2, second_next));
    assert!(last.ok, "{}", last.message);
    assert_eq!(ids(&last), [5]);
    assert!(!last.page.unwrap().has_more);

    let back = engine.execute_page(BASE_QUERY, PageSpec::before(2, second_previous));
    assert!(back.ok, "{}", back.message);
    assert_eq!(ids(&back), [1, 2]);
    assert!(!back.page.unwrap().has_more);
}

#[test]
fn projection_can_hide_cursor_order_fields() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let query = format!("{BASE_QUERY}\nselect title");
    let first = engine.execute_page(&query, PageSpec::forward(2));
    assert!(first.ok, "{}", first.message);
    assert_eq!(first.rows[0].keys().collect::<Vec<_>>(), ["title"]);
    let next = first.page.unwrap().next_cursor.unwrap();
    let second = engine.execute_page(&query, PageSpec::after(2, next));
    assert!(second.ok, "{}", second.message);
    assert_eq!(
        second
            .rows
            .iter()
            .map(|row| row["title"].source_text())
            .collect::<Vec<_>>(),
        ["\"three\"", "\"four\""]
    );
}

#[test]
fn cursors_bind_query_params_schema_sequence_and_database() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let query = "from tasks\nfilter priority >= $minimum\nsort {-priority, id}";
    let params = BTreeMap::from([("minimum".into(), Value::Int(0))]);
    let first = engine.execute_with_params_page(query, params.clone(), None, PageSpec::forward(2));
    let cursor = first.page.unwrap().next_cursor.unwrap();

    let changed_params = engine.execute_with_params_page(
        query,
        BTreeMap::from([("minimum".into(), Value::Int(1))]),
        None,
        PageSpec::after(2, cursor.clone()),
    );
    assert_eq!(changed_params.error.unwrap().code, "E_CURSOR_QUERY");

    let changed_query = engine.execute_page(
        "from tasks\nfilter active == true\nsort {priority, id}",
        PageSpec::after(2, cursor.clone()),
    );
    assert_eq!(changed_query.error.unwrap().code, "E_CURSOR_QUERY");

    let mut other = Engine::memory();
    setup(&mut other);
    let wrong_database = other.execute_with_params_page(
        query,
        params.clone(),
        None,
        PageSpec::after(2, cursor.clone()),
    );
    assert_eq!(wrong_database.error.unwrap().code, "E_CURSOR_INTEGRITY");

    assert!(
        engine
            .execute("insert tasks {id = 7, priority = 0, title = \"new\", active = true}")
            .ok
    );
    let stale = engine.execute_with_params_page(query, params, None, PageSpec::after(2, cursor));
    assert_eq!(stale.error.unwrap().code, "E_CURSOR_STALE");

    let mut schema_engine = Engine::memory();
    setup(&mut schema_engine);
    let cursor = schema_engine
        .execute_page(BASE_QUERY, PageSpec::forward(2))
        .page
        .unwrap()
        .next_cursor
        .unwrap();
    assert!(schema_engine.execute("create index tasks (active)").ok);
    let changed_schema = schema_engine.execute_page(BASE_QUERY, PageSpec::after(2, cursor));
    assert_eq!(changed_schema.error.unwrap().code, "E_CURSOR_SCHEMA");
}

#[test]
fn reads_rollbacks_and_idempotency_replays_do_not_stale_a_cursor() {
    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let mut engine = Engine::memory();
    setup(&mut engine);
    engine
        .execute_idempotent_with_params(
            "task-7",
            DIGEST,
            "insert tasks {id = 7, priority = 0, title = \"seven\", active = true}",
            BTreeMap::new(),
            None,
        )
        .unwrap();
    let cursor = engine
        .execute_page(BASE_QUERY, PageSpec::forward(2))
        .page
        .unwrap()
        .next_cursor
        .unwrap();
    assert!(
        !engine
            .execute("insert tasks {id = 1, priority = 0, title = \"duplicate\", active = true}")
            .ok
    );
    assert!(engine.execute("from tasks | filter id == 1").ok);
    assert!(
        engine
            .execute_idempotent_with_params(
                "task-7",
                DIGEST,
                "not parsed during replay",
                BTreeMap::new(),
                None,
            )
            .unwrap()
            .replayed
    );
    let resumed = engine.execute_page(BASE_QUERY, PageSpec::after(2, cursor));
    assert!(resumed.ok, "{}", resumed.message);
    assert_eq!(ids(&resumed), [3, 4]);
}

#[test]
fn page_shape_order_and_tampering_fail_with_stable_codes() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    for (query, code) in [
        ("from tasks | page 2", "E_PAGE_ORDER"),
        ("from tasks | sort priority | page 2", "E_PAGE_ORDER"),
        ("from tasks | sort id | page 0", "E_PAGE_SHAPE"),
        ("from tasks | sort id | take 2 | page 2", "E_PAGE_SHAPE"),
        ("from tasks | sort id | page 2 | select id", "E_PAGE_SHAPE"),
    ] {
        let response = engine.execute(query);
        assert_eq!(response.error.unwrap().code, code, "{query}");
    }

    let first = engine.execute_page(BASE_QUERY, PageSpec::forward(2));
    let cursor = first.page.unwrap().next_cursor.unwrap();
    let wrong_direction = engine.execute_page(BASE_QUERY, PageSpec::before(2, cursor.clone()));
    assert_eq!(wrong_direction.error.unwrap().code, "E_CURSOR_QUERY");
    let wrong_limit = engine.execute_page(BASE_QUERY, PageSpec::after(3, cursor.clone()));
    assert_eq!(wrong_limit.error.unwrap().code, "E_CURSOR_QUERY");
    let mut bytes = cursor.into_bytes();
    let position = bytes.len() / 2;
    bytes[position] = if bytes[position] == b'A' { b'B' } else { b'A' };
    let tampered = engine.execute_page(
        BASE_QUERY,
        PageSpec::after(2, String::from_utf8(bytes).unwrap()),
    );
    assert!(matches!(
        tampered.error.unwrap().code.as_str(),
        "E_CURSOR_CODEC" | "E_CURSOR_INTEGRITY"
    ));

    let conflict = engine.execute_with_params_page(
        &format!("{BASE_QUERY}\npage 2"),
        BTreeMap::new(),
        None,
        PageSpec::forward(2),
    );
    assert_eq!(conflict.error.unwrap().code, "E_PAGE_SHAPE");

    let multiple = engine.execute("from tasks | sort id | page 2\nfrom tasks");
    assert_eq!(multiple.error.unwrap().code, "E_PAGE_SHAPE");

    let oversized = engine.execute_page(BASE_QUERY, PageSpec::after(2, "x".repeat(8193)));
    assert_eq!(oversized.error.unwrap().code, "E_CURSOR_LIMIT");
}

#[test]
fn redb_reopen_preserves_cursor_but_logical_restore_rotates_identity() {
    let dir = TempDir::new();
    let source = dir.0.join("source.redb");
    let archive = dir.0.join("backup.json");
    let restored = dir.0.join("restored.redb");
    let cursor = {
        let mut engine = Engine::open_redb(&source).unwrap();
        setup(&mut engine);
        engine
            .execute_page(BASE_QUERY, PageSpec::forward(2))
            .page
            .unwrap()
            .next_cursor
            .unwrap()
    };

    let mut reopened = Engine::open_redb(&source).unwrap();
    let second = reopened.execute_page(BASE_QUERY, PageSpec::after(2, cursor.clone()));
    assert!(second.ok, "{}", second.message);
    assert_eq!(ids(&second), [3, 4]);
    drop(reopened);

    unionid::backup::create(&source, &archive).unwrap();
    unionid::backup::restore(&archive, &restored).unwrap();
    let mut restored = Engine::open_redb(&restored).unwrap();
    let rejected = restored.execute_page(BASE_QUERY, PageSpec::after(2, cursor));
    assert_eq!(rejected.error.unwrap().code, "E_CURSOR_INTEGRITY");
}

#[test]
fn legacy_redb_meta_is_upgraded_with_cursor_identity() {
    const META: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("meta");
    let dir = TempDir::new();
    let path = dir.0.join("legacy.redb");
    drop(Engine::open_redb(&path).unwrap());
    {
        let database = redb::Database::open(&path).unwrap();
        let transaction = database.begin_write().unwrap();
        {
            let mut meta = transaction.open_table(META).unwrap();
            meta.insert("storage_format_version", 1_u32.to_be_bytes().as_slice())
                .unwrap();
            meta.insert("catalog_codec_version", 2_u16.to_be_bytes().as_slice())
                .unwrap();
            meta.insert("value_codec_version", 1_u16.to_be_bytes().as_slice())
                .unwrap();
            meta.insert("index_key_version", 1_u16.to_be_bytes().as_slice())
                .unwrap();
            meta.insert("migration_codec_version", 1_u16.to_be_bytes().as_slice())
                .unwrap();
            meta.insert("receipt_codec_version", 1_u16.to_be_bytes().as_slice())
                .unwrap();
            meta.remove("cursor_instance_id").unwrap();
            meta.remove("cursor_secret").unwrap();
        }
        transaction.commit().unwrap();
    }
    drop(Engine::open_redb(&path).unwrap());
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_read().unwrap();
    let meta = transaction.open_table(META).unwrap();
    assert_eq!(
        meta.get("storage_format_version").unwrap().unwrap().value(),
        3_u32.to_be_bytes()
    );
    assert_eq!(
        meta.get("cursor_instance_id")
            .unwrap()
            .unwrap()
            .value()
            .len(),
        16
    );
    assert_eq!(
        meta.get("cursor_secret").unwrap().unwrap().value().len(),
        32
    );
}

#[test]
fn version_one_and_explain_expose_page_metadata() {
    let mut engine = Engine::memory();
    setup(&mut engine);
    let request = Request::query("page-1", BASE_QUERY).with_page(PageSpec::forward(2));
    let response = execute_protocol_request(&mut engine, request);
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.page.as_ref().unwrap().direction,
        PageDirection::Forward
    );
    assert_eq!(response.rows.len(), 2);

    let explained = engine.execute(
        "explain\n  from tasks\n  filter active == true\n  sort {-priority, id}\n  page 2",
    );
    assert!(explained.ok, "{}", explained.message);
    let page = explained.plan.unwrap().page.unwrap();
    assert_eq!(page.access, PageAccessKind::SortedScan);
    assert_eq!(page.candidate_rows, 6);
    assert_eq!(page.read_limit, 3);
    assert_eq!(page.order.last().unwrap().column, "id");
    assert!(!page.resume_boundary);
}
