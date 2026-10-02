mod common;

use std::collections::BTreeMap;

use common::TempDir;
use serde::Deserialize;
use unionid::{Engine, Value};

const SCHEMA: &str = "struct Sample {id: int, raw: text}\ntable samples: Sample {key id}";

#[derive(Debug, Deserialize, PartialEq)]
struct Normalized {
    lowered: String,
    uppered: String,
    trimmed: String,
}

#[test]
fn unicode_normalization_and_prepared_filters_share_typed_expressions() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, raw: \"　Straße İ　\"}")
            .ok
    );
    let result = engine.execute("from samples\nderive {lowered = lower raw, uppered = upper raw, trimmed = trim raw}\nselect {lowered, uppered, trimmed}");
    assert!(result.ok, "{}", result.message);
    assert_eq!(
        result.typed_rows::<Normalized>().unwrap(),
        vec![Normalized {
            lowered: "　straße i\u{307}　".into(),
            uppered: "　STRASSE İ　".into(),
            trimmed: "Straße İ".into(),
        }]
    );
    let prepared = engine
        .prepare("from samples | filter lower (trim raw) == lower $needle")
        .unwrap();
    let result = engine.execute_prepared(
        &prepared,
        BTreeMap::from([("needle".into(), Value::Text("STRAßE İ".into()))]),
    );
    assert!(result.ok, "{}", result.message);
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn text_functions_reject_bad_types_before_scanning_empty_inputs() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    for expression in ["lower 12", "upper None", "trim true"] {
        let response = engine.execute(&format!("from samples | derive label = {expression}"));
        assert!(!response.ok, "{expression}");
        assert_eq!(response.error.unwrap().code, "E_TYPE");
    }
}

#[test]
fn normalization_updates_preserve_unique_rollback_and_durable_values() {
    let dir = TempDir::new();
    let path = dir.0.join("normalization.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute(SCHEMA).ok);
        assert!(engine.execute("create unique index samples (raw)\ninsert many samples [{id: 1, raw: \" A \"}, {id: 2, raw: \"a\"}]").ok);
        let before = serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap();
        let rejected = engine.execute("update samples | set raw = lower (trim raw)");
        assert!(!rejected.ok);
        assert_eq!(rejected.error.unwrap().code, "E_CONSTRAINT");
        assert_eq!(
            serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap(),
            before
        );
        let response = engine.execute("delete samples | filter id == 2\nupdate samples | set raw = lower (trim raw) | returning raw");
        assert!(response.ok, "{}", response.message);
        assert!(matches!(&response.rows[0]["raw"], Value::Text(value) if value == "a"));
        let response = engine.execute(
            "migration capitalize {change field Sample.raw to text using old -> upper old}",
        );
        assert!(response.ok, "{}", response.message);
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    let response = engine.execute("from samples");
    assert!(response.ok);
    assert!(matches!(&response.rows[0]["raw"], Value::Text(value) if value == "A"));
    engine.check_integrity().unwrap();
}
