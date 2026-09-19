use std::collections::BTreeMap;

mod common;
use common::TempDir;
use unionid::codec::{decode_value, encode_value_v2, encode_value_v3};
use unionid::model::ScalarType;
use unionid::portable::TypeShape;
use unionid::{Engine, Value, WireValue, format_source};

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
