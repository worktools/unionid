use std::collections::BTreeMap;

mod common;
use common::TempDir;
use unionid::backup::incremental::{BackupJournalConfig, BackupJournalState};
use unionid::codec::{decode_value, encode_value_v2, encode_value_v3};
use unionid::model::ScalarType;
use unionid::portable::TypeShape;
use unionid::{Engine, PageSpec, Value, WireValue, backup, format_source};

const TEST_BACKUP_CHECKSUM: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

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
