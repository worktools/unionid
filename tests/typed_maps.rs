use std::collections::BTreeMap;
use std::process::Command;

mod common;
use common::TempDir;
use unionid::backup::incremental::{BackupJournalConfig, BackupJournalState};
use unionid::codec::MAX_VALUE_BYTES;
use unionid::codec::{decode_value, encode_value_v2, encode_value_v3};
use unionid::model::{EnumValue, MAX_DEPTH, MAX_MAP_ENTRIES, MAX_MAP_KEY_BYTES, ScalarType};
use unionid::portable::TypeShape;
use unionid::{Engine, PageSpec, Value, WireValue, backup, format_source};

const TEST_BACKUP_CHECKSUM: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const MAP_UPGRADE_PATH_ENV: &str = "UNIONID_TEST_MAP_UPGRADE_PATH";
const MAP_UPGRADE_READY_ENV: &str = "UNIONID_TEST_MAP_UPGRADE_READY";
const MAP_UPGRADE_COMMITTED_ENV: &str = "UNIONID_TEST_MAP_UPGRADE_COMMITTED";
const MAP_UPGRADE_TARGET_ENV: &str = "UNIONID_TEST_MAP_UPGRADE_TARGET";

#[test]
fn map_source_round_trips_through_memory_queries() {
    let mut engine = Engine::memory();
    let response = engine.execute(
        r#"struct Item {
  id: int
  attributes: Map<text, int> = map {}
}


table items: Item {
  key id
}

insert items {
  id: 1
  attributes: map {
    "z": 3
    "a": 1
  }
}

insert items {id: 2}

from items
sort id"#,
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 2);
    assert!(
        response.rows[0]["attributes"].cmp_eq(&Value::Map(BTreeMap::from([
            ("a".into(), Value::Int(1)),
            ("z".into(), Value::Int(3)),
        ])))
    );
    assert!(response.rows[1]["attributes"].cmp_eq(&Value::Map(BTreeMap::new())));

    let schema = engine.schema();
    assert!(schema.contains("Map<text, int>"));
    assert!(schema.contains("map {}"));

    let queried = engine.execute(
        r#"from items
filter attributes == map {"z": 3, "a": 1}
select {id, attributes}"#,
    );
    assert!(queried.ok, "{}", queried.message);
    assert_eq!(queried.rows.len(), 1);
    assert!(queried.rows[0]["attributes"].cmp_eq(&response.rows[0]["attributes"]));

    let mut matches = Engine::memory();
    let derived = matches.execute(
        r#"enum Attributes {
  Present(Map<text, int>)
  Empty
}
struct Row { id: int, attributes: Attributes }
table rows: Row { key id }
insert rows {id: 1, attributes: Present(map {"a": 1})}
insert rows {id: 2, attributes: Empty}
from rows
derive copied = match attributes {
  Present(entries) => entries
  Empty => map {},
}
sort id
select {id, copied}"#,
    );
    assert!(derived.ok, "{}", derived.message);
    assert!(
        derived.rows[0]["copied"]
            .cmp_eq(&Value::Map(BTreeMap::from([("a".into(), Value::Int(1),)])))
    );
    assert!(derived.rows[1]["copied"].cmp_eq(&Value::Map(BTreeMap::new())));
}

#[test]
fn map_queries_preserve_types_order_and_prepared_keys() {
    let mut engine = Engine::memory();
    let setup = engine.execute(
        r#"enum Attribute {
  Text(text)
  Number(int)
}
struct Account {
  id: int
  attributes: Map<text, Attribute>
}
table accounts: Account { key id }
insert accounts {
  id: 1
  attributes: map {
    "quota": Number(3)
    "plan": Text("pro")
  }
}
insert accounts {id: 2, attributes: map {}}"#,
    );
    assert!(setup.ok, "{}", setup.message);

    let response = engine.execute(
        r#"from accounts
derive {
  has_plan = contains_key attributes "plan"
  plan = get attributes "plan"
  attribute_keys = keys attributes
  attribute_values = values attributes
  attribute_entries = entries attributes
  attribute_count = length attributes
  has_pro = any (values attributes) (value -> value == Text("pro"))
  all_known = all (keys attributes) (key -> key == "plan" || key == "quota")
}
sort id"#,
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 2);
    let populated = &response.rows[0];
    assert!(populated["has_plan"].cmp_eq(&Value::Bool(true)));
    assert!(populated["attribute_count"].cmp_eq(&Value::Int(2)));
    assert!(populated["has_pro"].cmp_eq(&Value::Bool(true)));
    assert!(populated["all_known"].cmp_eq(&Value::Bool(true)));
    assert_eq!(
        populated["attribute_keys"].source_text(),
        r#"["plan", "quota"]"#
    );
    assert_eq!(
        populated["attribute_values"].source_text(),
        r#"[Text("pro"), Number(3)]"#
    );
    assert_eq!(
        populated["attribute_entries"].source_text(),
        r#"[("plan", Text("pro")), ("quota", Number(3))]"#
    );
    assert_eq!(populated["plan"].source_text(), r#"Some(Text("pro"))"#);

    let empty = &response.rows[1];
    assert!(empty["has_plan"].cmp_eq(&Value::Bool(false)));
    assert!(empty["attribute_count"].cmp_eq(&Value::Int(0)));
    assert!(empty["has_pro"].cmp_eq(&Value::Bool(false)));
    assert!(empty["all_known"].cmp_eq(&Value::Bool(true)));
    assert_eq!(empty["plan"].source_text(), "None");

    let prepared = engine
        .prepare(
            r#"from accounts
let lookup = (items, key: text) -> get items key
derive selected = lookup attributes $key
filter is_some selected
select {id, selected}"#,
        )
        .unwrap();
    assert_eq!(prepared.parameter_types()["key"], "text");
    let selected = engine.execute_prepared(
        &prepared,
        BTreeMap::from([("key".into(), Value::Text("quota".into()))]),
    );
    assert!(selected.ok, "{}", selected.message);
    assert_eq!(selected.rows.len(), 1);
    assert_eq!(
        selected.rows[0]["selected"].source_text(),
        "Some(Number(3))"
    );
}

#[test]
fn map_materialization_shares_the_collection_evaluation_budget() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "struct Item { id: int, attributes: Map<text, int> }\n\
                 table items: Item { key id }"
            )
            .ok
    );
    let insert = engine.prepare("insert items $row").unwrap();
    let attributes: BTreeMap<String, Value> = (0..4_096)
        .map(|index| (format!("key-{index:04}"), Value::Int(index)))
        .collect();
    for id in 0..25 {
        let row = Value::Record(BTreeMap::from([
            ("id".into(), Value::Int(id)),
            ("attributes".into(), Value::Map(attributes.clone())),
        ]));
        let inserted = engine.execute_prepared(&insert, BTreeMap::from([("row".into(), row)]));
        assert!(inserted.ok, "{}", inserted.message);
    }

    let response = engine.execute("from items\nderive copy = values attributes");
    assert!(
        !response.ok,
        "query returned {} rows: {}",
        response.rows.len(),
        response.message
    );
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_LIMIT");
    assert!(
        error
            .message
            .contains("collection element evaluation limit")
    );
}

#[test]
fn map_query_type_errors_are_reported_before_scanning() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "struct Item { id: int, attributes: Map<text, int> }\n\
                 table items: Item { key id }"
            )
            .ok
    );
    for (query, expected_message) in [
        (
            "from items | derive value = get id \"key\"",
            "expects a map",
        ),
        (
            "from items | derive value = contains_key attributes 1",
            "expected text",
        ),
        (
            "from items | derive value = keys attributes \"extra\"",
            "expects 1 arguments",
        ),
        ("from items | derive value = length id", "length expects"),
    ] {
        let response = engine.execute(query);
        assert!(!response.ok, "{query}");
        let error = response.error.unwrap();
        assert_eq!(error.code, "E_TYPE", "{query}: {}", error.message);
        assert!(
            error.message.contains(expected_message),
            "{query}: {}",
            error.message
        );
    }
}

#[test]
fn map_queries_work_in_match_update_and_migration_expressions() {
    let mut engine = Engine::memory();
    let setup = engine.execute(
        r#"enum Attributes {
  Present(Map<text, int>)
  Empty
}
struct Item {
  id: int
  attributes: Map<text, int>
  wrapped: Attributes
  configured: bool = false
}
table items: Item { key id }
insert items {
  id: 1
  attributes: map {"plan": 2}
  wrapped: Present(map {"plan": 3})
}"#,
    );
    assert!(setup.ok, "{}", setup.message);

    let matched = engine.execute(
        r#"from items
derive selected = match wrapped {
  Present(attributes) => get attributes "plan"
  Empty => None
}
select {id, selected}"#,
    );
    assert!(matched.ok, "{}", matched.message);
    assert_eq!(matched.rows[0]["selected"].source_text(), "Some(3)");

    let updated = engine.execute(
        r#"update items
filter id == 1
set configured = contains_key attributes "plan"
returning {configured}"#,
    );
    assert!(updated.ok, "{}", updated.message);
    assert!(updated.rows[0]["configured"].cmp_eq(&Value::Bool(true)));

    let migrated = engine.execute(
        r#"migration select_plan
  change field Item.attributes to Option<int>
    using old -> get old "plan""#,
    );
    assert!(migrated.ok, "{}", migrated.message);
    let row = engine.execute("from items | select {id, attributes}");
    assert!(row.ok, "{}", row.message);
    assert_eq!(row.rows[0]["attributes"].source_text(), "Some(2)");
}

#[test]
fn map_formatter_is_stable_and_sorts_keys() {
    let source = r#"struct Item { id: int, attributes: Map<text, int> = map { "z": 2, "a": 1 } }
table items: Item { key id }"#;
    let formatted = format_source(source).unwrap();
    assert!(formatted.contains("attributes: Map<text, int> = map {\"a\": 1, \"z\": 2}"));
    assert_eq!(format_source(&formatted).unwrap(), formatted);
}

#[test]
fn map_serde_wire_portable_and_codec_are_lossless() {
    let expected = BTreeMap::from([("a".to_owned(), 1_i64), ("z".to_owned(), 3_i64)]);
    let value = Value::from_serde(&expected).unwrap();
    assert_eq!(value.to_serde::<BTreeMap<String, i64>>().unwrap(), expected);

    let wire = WireValue::from(&value);
    assert!(wire.requires_v2());
    assert!(Value::try_from(wire).unwrap().cmp_eq(&value));

    let ty = ScalarType::Map(Box::new(ScalarType::Int));
    let catalog = unionid::model::Catalog::default();
    let bytes = encode_value_v3(&catalog, &ty, &value).unwrap();
    assert!(decode_value(&catalog, &ty, &bytes).unwrap().cmp_eq(&value));
    let error = encode_value_v2(&catalog, &ty, &value).unwrap_err();
    assert_eq!(error.code, "E_CODEC");

    let description = unionid::portable::describe(
        "struct Item { id: int, attributes: Map<text, int> }\ntable items: Item { key id }",
    )
    .unwrap();
    description.validate().unwrap();
    let TypeShape::Record { fields } = &description.types[0].shape else {
        panic!("Item should be a record")
    };
    assert!(matches!(
        fields
            .iter()
            .find(|field| field.name == "attributes")
            .unwrap()
            .shape,
        TypeShape::Map { .. }
    ));
}

#[test]
fn map_rejects_non_text_keys_duplicates_and_wrong_value_types() {
    for (source, code) in [
        ("struct Item { attributes: Map<int, int> }", "E_SYNTAX"),
        (
            "struct Item { attributes: Map<text, int> }\ntable items: Item\ninsert items {attributes: map {\"a\": 1, \"a\": 2}}",
            "E_DUPLICATE_KEY",
        ),
        (
            "struct Item { attributes: Map<text, int> }\ntable items: Item\ninsert items {attributes: map {\"a\": \"wrong\"}}",
            "E_TYPE",
        ),
    ] {
        let mut engine = Engine::memory();
        let response = engine.execute(source);
        assert!(!response.ok, "{source}");
        assert_eq!(response.error.unwrap().code, code, "{source}");
    }
}

#[test]
fn map_limits_fail_atomically_before_publishing_rows() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "struct Item { id: int, attributes: Map<text, int> }\n\
                 table items: Item { key id }"
            )
            .ok
    );
    let insert = engine.prepare("insert many items $rows").unwrap();

    let too_many = (0..=MAX_MAP_ENTRIES)
        .map(|index| (format!("key-{index}"), Value::Int(index as i64)))
        .collect();
    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([(
            "rows".into(),
            Value::List(vec![
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(1)),
                    ("attributes".into(), Value::Map(BTreeMap::new())),
                ])),
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(2)),
                    ("attributes".into(), Value::Map(too_many)),
                ])),
            ]),
        )]),
    );
    assert_eq!(response.error.unwrap().code, "E_MAP_LIMIT");
    assert!(engine.execute("from items").rows.is_empty());

    let response = engine.execute_prepared(
        &insert,
        BTreeMap::from([(
            "rows".into(),
            Value::List(vec![
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(1)),
                    ("attributes".into(), Value::Map(BTreeMap::new())),
                ])),
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(2)),
                    (
                        "attributes".into(),
                        Value::Map(BTreeMap::from([(
                            "x".repeat(MAX_MAP_KEY_BYTES + 1),
                            Value::Int(1),
                        )])),
                    ),
                ])),
            ]),
        )]),
    );
    assert_eq!(response.error.unwrap().code, "E_MAP_LIMIT");
    assert!(engine.execute("from items").rows.is_empty());

    let mut recursive = Engine::memory();
    assert!(
        recursive
            .execute(
                "enum Node { Next(Node), End }\n\
                 struct Tree { id: int, metadata: Map<text, Node> }\n\
                 table trees: Tree { key id }"
            )
            .ok
    );
    let insert = recursive.prepare("insert many trees $rows").unwrap();
    let mut node = Value::Enum(EnumValue {
        variant: "End".into(),
        args: Vec::new(),
        id: 0,
    });
    for _ in 0..MAX_DEPTH {
        node = Value::Enum(EnumValue {
            variant: "Next".into(),
            args: vec![node],
            id: 0,
        });
    }
    let response = recursive.execute_prepared(
        &insert,
        BTreeMap::from([(
            "rows".into(),
            Value::List(vec![
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(1)),
                    ("metadata".into(), Value::Map(BTreeMap::new())),
                ])),
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(2)),
                    (
                        "metadata".into(),
                        Value::Map(BTreeMap::from([("tree".into(), node)])),
                    ),
                ])),
            ]),
        )]),
    );
    assert_eq!(response.error.unwrap().code, "E_LIMIT");
    assert!(recursive.execute("from trees").rows.is_empty());

    let dir = TempDir::new();
    let path = dir.0.join("oversized-map.redb");
    let mut durable = Engine::open_redb(&path).unwrap();
    durable.upgrade_storage(8).unwrap();
    assert!(
        durable
            .execute(
                "struct Blob { id: int, metadata: Map<text, text> }\n\
                 table blobs: Blob { key id }"
            )
            .ok
    );
    let insert = durable.prepare("insert many blobs $rows").unwrap();
    let response = durable.execute_prepared(
        &insert,
        BTreeMap::from([(
            "rows".into(),
            Value::List(vec![
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(1)),
                    ("metadata".into(), Value::Map(BTreeMap::new())),
                ])),
                Value::Record(BTreeMap::from([
                    ("id".into(), Value::Int(2)),
                    (
                        "metadata".into(),
                        Value::Map(BTreeMap::from([(
                            "payload".into(),
                            Value::Text("x".repeat(MAX_VALUE_BYTES)),
                        )])),
                    ),
                ])),
            ]),
        )]),
    );
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_STORAGE");
    assert!(error.message.contains("encoded value exceeds"), "{error}");
    assert!(durable.execute("from blobs").rows.is_empty());
}

#[test]
fn redb_rejects_maps_before_publishing_any_schema_change() {
    let dir = TempDir::new();
    let path = dir.0.join("maps.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    let response = engine.execute(
        "struct Item { id: int, attributes: Map<text, int> }\ntable items: Item { key id }",
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_STORAGE_UPGRADE_REQUIRED");
    assert!(engine.tables().is_empty());
    assert!(!engine.schema().contains("Item"));
    drop(engine);

    let reopened = Engine::open_redb(&path).unwrap();
    assert!(reopened.tables().is_empty());
    assert!(!reopened.schema().contains("Item"));
}

#[test]
fn map_storage_upgrade_round_trips_indexes_cursors_and_backups() {
    let dir = TempDir::new();
    let path = dir.0.join("maps-v8.redb");
    let archive = dir.0.join("maps-v5.json");
    let restored = dir.0.join("maps-restored.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    let upgraded = engine.upgrade_storage(8).unwrap();
    assert_eq!((upgraded.previous_format, upgraded.format), (6, 8));
    let versions = engine.introspection().storage_versions.unwrap();
    assert_eq!(
        (
            versions.catalog_codec,
            versions.value_codec,
            versions.index_key_codec,
            versions.receipt_codec,
            versions.backup_codec,
        ),
        (5, 3, 4, 3, 5)
    );

    let response = engine.execute(
        r#"struct Item {
  id: int
  attributes: Map<text, int>
}
table items: Item { key id }
create unique index items (attributes)
insert items {id: 1, attributes: map {"a": 1}}
insert items {id: 2, attributes: map {"b": 2}}"#,
    );
    assert!(response.ok, "{}", response.message);
    let first = engine.execute_page("from items\nsort {attributes, id}", PageSpec::forward(1));
    assert!(first.ok, "{}", first.message);
    assert!(first.page.unwrap().next_cursor.unwrap().starts_with("u3."));
    assert!(engine.check_integrity().unwrap().backend_clean);
    drop(engine);

    let mut reopened = Engine::open_redb(&path).unwrap();
    assert_eq!(reopened.introspection().storage_versions.unwrap().format, 8);
    let rows = reopened.execute("from items\nsort id");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 2);
    assert!(reopened.check_integrity().unwrap().backend_clean);
    drop(reopened);

    let created = backup::create(&path, &archive).unwrap();
    assert_eq!(created.format_version, 5);
    let mut journal_engine = Engine::open_redb(&path).unwrap();
    let journal_head = journal_engine
        .backup_journal_status()
        .unwrap()
        .head_sequence;
    let enabled = journal_engine
        .enable_backup_journal(BackupJournalConfig::new(
            "maps-v8",
            journal_head,
            TEST_BACKUP_CHECKSUM,
        ))
        .unwrap();
    assert_eq!(enabled.storage_format, 9);
    drop(journal_engine);

    backup::restore(&archive, &restored).unwrap();
    let mut restored = Engine::open_redb(&restored).unwrap();
    assert_eq!(restored.introspection().storage_versions.unwrap().format, 8);
    assert_eq!(restored.execute("from items").rows.len(), 2);
    assert!(restored.check_integrity().unwrap().backend_clean);
}

#[test]
fn typed_map_storage_upgrade_child() {
    let Ok(path) = std::env::var(MAP_UPGRADE_PATH_ENV) else {
        return;
    };
    let ready = std::env::var(MAP_UPGRADE_READY_ENV).unwrap();
    let committed = std::env::var(MAP_UPGRADE_COMMITTED_ENV).unwrap();
    let target = std::env::var(MAP_UPGRADE_TARGET_ENV)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let mut engine = Engine::open_redb(path).unwrap();
    std::fs::write(ready, b"ready").unwrap();
    engine.upgrade_storage(target).unwrap();
    std::fs::write(committed, b"committed").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn terminated_format_6_and_7_map_upgrades_reopen_committed_state() {
    for journal in [false, true] {
        let dir = TempDir::new();
        let path = dir.0.join(if journal {
            "interrupted-format-9.redb"
        } else {
            "interrupted-format-8.redb"
        });
        let ready = dir.0.join("upgrade-ready");
        let committed = dir.0.join("upgrade-committed");
        let mut engine = Engine::open_redb(&path).unwrap();
        let mut source = String::from(
            "struct Entry { id: int, value: text }\n\
             table entries: Entry { key id }\n\
             create index entries (value)\n\
             insert many entries [",
        );
        for id in 0..5_000 {
            if id > 0 {
                source.push_str(", ");
            }
            source.push_str(&format!("{{id: {id}, value: \"value-{id}\"}}"));
        }
        source.push(']');
        let inserted = engine.execute(&source);
        assert!(inserted.ok, "{}", inserted.message);
        if journal {
            let status = engine.backup_journal_status().unwrap();
            let enabled = engine
                .enable_backup_journal(BackupJournalConfig::new(
                    "map-upgrade",
                    status.head_sequence,
                    TEST_BACKUP_CHECKSUM,
                ))
                .unwrap();
            assert_eq!(enabled.storage_format, 7);
        }
        drop(engine);

        let target = if journal { 9 } else { 8 };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "typed_map_storage_upgrade_child", "--nocapture"])
            .env(MAP_UPGRADE_PATH_ENV, &path)
            .env(MAP_UPGRADE_READY_ENV, &ready)
            .env(MAP_UPGRADE_COMMITTED_ENV, &committed)
            .env(MAP_UPGRADE_TARGET_ENV, target.to_string())
            .spawn()
            .unwrap();
        for _ in 0..1_000 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ready.exists(), "map upgrade child did not start");
        for _ in 0..30_000 {
            if committed.exists() {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "map upgrade child exited before publishing its durable commit"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            committed.exists(),
            "map upgrade child did not reach its durable commit"
        );
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());

        let mut reopened = Engine::open_redb(&path).unwrap();
        let format = reopened.introspection().storage_versions.unwrap().format;
        assert_eq!(format, target);
        assert!(reopened.check_integrity().unwrap().backend_clean);
        let row = reopened.execute("from entries | filter id == 4999");
        assert!(row.ok, "{}", row.message);
        assert_eq!(row.rows.len(), 1);
        assert!(row.rows[0]["value"].cmp_eq(&Value::Text("value-4999".into())));
        if journal {
            assert_eq!(
                reopened.backup_journal_status().unwrap().state,
                BackupJournalState::Active
            );
        }
        assert_eq!(
            reopened.introspection().storage_versions.unwrap().format,
            target
        );
        let map = reopened.execute(
            "struct Metadata { id: int, attributes: Map<text, text> }\n\
             table metadata: Metadata { key id }\n\
             insert metadata {id: 1, attributes: map {\"status\": \"ready\"}}",
        );
        assert!(map.ok, "{}", map.message);
        assert!(reopened.check_integrity().unwrap().backend_clean);
    }
}

#[test]
fn active_backup_journal_upgrades_from_format_7_to_9() {
    let dir = TempDir::new();
    let path = dir.0.join("maps-v9.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    let initial = engine.backup_journal_status().unwrap();
    let enabled = engine
        .enable_backup_journal(BackupJournalConfig::new(
            "maps",
            initial.head_sequence,
            TEST_BACKUP_CHECKSUM,
        ))
        .unwrap();
    assert_eq!(enabled.storage_format, 7);
    assert_eq!(enabled.state, BackupJournalState::Active);

    let upgraded = engine.upgrade_storage(9).unwrap();
    assert_eq!((upgraded.previous_format, upgraded.format), (7, 9));
    let status = engine.backup_journal_status().unwrap();
    assert_eq!(status.state, BackupJournalState::Active);
    assert_eq!(status.chain_id.as_deref(), Some("maps"));
    assert_eq!(status.storage_format, 9);
    assert_eq!(status.commit_count, 1);
    assert_eq!(status.head_sequence, initial.head_sequence + 1);
    let repeated = engine.upgrade_storage(9).unwrap();
    assert!(!repeated.changed);
    assert_eq!(engine.backup_journal_status().unwrap(), status);
    let response = engine.execute(
        "struct Item { id: int, labels: Map<text, text> }\ntable items: Item { key id }\ninsert items {id: 1, labels: map {\"kind\": \"test\"}}",
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(engine.backup_journal_status().unwrap().commit_count, 2);
    drop(engine);

    let mut reopened = Engine::open_redb(&path).unwrap();
    assert_eq!(reopened.introspection().storage_versions.unwrap().format, 9);
    assert_eq!(reopened.execute("from items").rows.len(), 1);
    assert!(reopened.check_integrity().unwrap().backend_clean);
}
