mod common;

use std::collections::BTreeMap;

use common::TempDir;
use serde::Deserialize;
use unionid::{Engine, Value};

const SCHEMA: &str = "struct Sample {id: int, raw: text}\ntable samples: Sample {key id}";
const NORMALIZE_QUERY: &str = "from samples\nderive {lowered = lower raw, uppered = upper raw, trimmed = trim raw}\nselect {lowered, uppered, trimmed}";

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
    let result = engine.execute(NORMALIZE_QUERY);
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
fn text_functions_share_explain_formatter_and_both_wire_versions() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(engine.execute("insert samples {id: 1, raw: \" A \"}").ok);
    let expected = engine
        .execute(NORMALIZE_QUERY)
        .typed_rows::<Normalized>()
        .unwrap();
    let canonical = unionid::format_source(NORMALIZE_QUERY).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    assert_eq!(
        engine
            .execute(&canonical)
            .typed_rows::<Normalized>()
            .unwrap(),
        expected
    );
    let before = serde_json::to_value(engine.execute("from samples").rows).unwrap();
    let plan = engine.execute(&format!(
        "explain\n{}",
        NORMALIZE_QUERY
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    assert!(plan.ok, "{}", plan.message);
    assert!(plan.rows.is_empty());
    assert_eq!(plan.plan.unwrap().result_schema.len(), 3);
    assert_eq!(
        serde_json::to_value(engine.execute("from samples").rows).unwrap(),
        before
    );
    for version in [1, 2] {
        let request = unionid::protocol::Request::query("normalization", NORMALIZE_QUERY)
            .with_version(version)
            .unwrap();
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: unionid::protocol::Response = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.typed_rows::<Normalized>().unwrap(), expected);
    }
}

#[test]
fn text_functions_reject_bad_types_before_scanning_empty_inputs() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    for expression in [
        "lower 12",
        "upper None",
        "trim true",
        "starts_with raw 12",
        "ends_with 12 raw",
        "contains_text raw None",
        "concat raw 12",
    ] {
        let response = engine.execute(&format!("from samples | derive label = {expression}"));
        assert!(!response.ok, "{expression}");
        assert_eq!(response.error.unwrap().code, "E_TYPE");
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct Label {
    label: String,
}

#[test]
fn concatenation_is_explicit_typed_and_composes_across_interfaces() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, raw: \"　Ada　\"}")
            .ok
    );
    let source = "from samples | derive label = concat (lower (trim raw)) $suffix | select label";
    let prepared = engine.prepare(source).unwrap();
    let params = BTreeMap::from([("suffix".into(), Value::Text("さん".into()))]);
    let response = engine.execute_prepared(&prepared, params);
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.typed_rows::<Label>().unwrap(),
        vec![Label {
            label: "adaさん".into()
        }]
    );
    let query =
        "from samples | derive label = concat (concat \"\" (trim raw)) \"!\" | select label";
    let canonical = unionid::format_source(query).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    for version in [1, 2] {
        let request = unionid::protocol::Request::query("concat", &canonical)
            .with_version(version)
            .unwrap();
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        assert_eq!(
            response.typed_rows::<Label>().unwrap(),
            vec![Label {
                label: "Ada!".into()
            }]
        );
    }
    let explanation = engine.execute("explain from samples | derive label = concat raw \"!\"");
    assert!(explanation.ok, "{}", explanation.message);
    assert!(explanation.rows.is_empty());
    let response = engine.execute(
        "migration label {change field Sample.raw to text using old -> concat (trim old) \"!\"}",
    );
    assert!(response.ok, "{}", response.message);
    let response = engine.execute("update samples | set raw = concat raw \"?\" | returning raw");
    assert!(response.ok, "{}", response.message);
    assert!(matches!(&response.rows[0]["raw"], Value::Text(value) if value == "Ada!?"));
}

#[test]
fn concatenation_rejects_oversized_results_before_allocating_them() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(engine.execute("insert samples {id: 1, raw: \"small\"}").ok);
    let prepared = engine
        .prepare("update samples | set raw = concat $large $large")
        .unwrap();
    let response = engine.execute_prepared(
        &prepared,
        BTreeMap::from([(
            "large".into(),
            Value::Text("x".repeat(unionid::codec::MAX_VALUE_BYTES / 2 + 1)),
        )]),
    );
    assert!(!response.ok);
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_LIMIT");
    assert!(
        error.message.contains("concat text result exceeds"),
        "{}",
        error.message
    );
    let unchanged = engine.execute("from samples");
    assert!(matches!(&unchanged.rows[0]["raw"], Value::Text(value) if value == "small"));
}

#[derive(Debug, Deserialize, PartialEq)]
struct Matches {
    prefix: bool,
    suffix: bool,
    canonical: bool,
    normalized_match: bool,
}

#[test]
fn text_matching_is_exact_and_composes_with_explicit_normalization() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    // The second row uses e + U+0301, not the precomposed character U+00E9.
    assert!(engine.execute("insert many samples [{id: 1, raw: \"東京Straße\"}, {id: 2, raw: \"é\"}, {id: 3, raw: \"\"}]").ok);
    let query = "from samples\nsort id\nderive {prefix = starts_with raw \"東京\", suffix = ends_with raw \"ße\", canonical = contains_text raw \"é\", normalized_match = contains_text (lower raw) \"straße\"}\nselect {prefix, suffix, canonical, normalized_match}";
    let result = engine.execute(query);
    assert!(result.ok, "{}", result.message);
    let expected = vec![
        Matches {
            prefix: true,
            suffix: true,
            canonical: false,
            normalized_match: true,
        },
        Matches {
            prefix: false,
            suffix: false,
            canonical: false,
            normalized_match: false,
        },
        Matches {
            prefix: false,
            suffix: false,
            canonical: false,
            normalized_match: false,
        },
    ];
    assert_eq!(result.typed_rows::<Matches>().unwrap(), expected);
    let canonical = unionid::format_source(query).unwrap();
    let plan_source = canonical
        .lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let plan = engine.execute(&format!("explain\n{plan_source}"));
    assert!(plan.ok, "{}", plan.message);
    assert!(plan.rows.is_empty());
    assert_eq!(
        engine.execute(&canonical).typed_rows::<Matches>().unwrap(),
        expected
    );
    for version in [1, 2] {
        let request = unionid::protocol::Request::query("matching", &canonical)
            .with_version(version)
            .unwrap();
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.typed_rows::<Matches>().unwrap(), expected);
    }
    let empty = engine.execute("from samples | filter starts_with raw \"\" && ends_with raw \"\" && contains_text raw \"\"");
    assert!(empty.ok, "{}", empty.message);
    assert_eq!(empty.rows.len(), 3);
    let prepared = engine
        .prepare("from samples | filter starts_with raw $prefix && ends_with raw $suffix")
        .unwrap();
    let result = engine.execute_prepared(
        &prepared,
        BTreeMap::from([
            ("prefix".into(), Value::Text("東京".into())),
            ("suffix".into(), Value::Text("ße".into())),
        ]),
    );
    assert!(result.ok, "{}", result.message);
    assert_eq!(result.rows.len(), 1);
    let response = engine.execute(
        "migration classify {change field Sample.raw to bool using old -> contains_text old \"ß\"}",
    );
    assert!(response.ok, "{}", response.message);
    let result = engine.execute("from samples | filter raw");
    assert!(result.ok, "{}", result.message);
    assert_eq!(result.rows.len(), 1);
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
