mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use unionid::{
    Engine, Value,
    scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid},
};

const SCHEMA: &str = "struct Sample {id: int, label: text}\ntable samples: Sample {key id}";

#[derive(Debug, Deserialize, PartialEq)]
struct TextRow {
    value: String,
}

#[test]
fn all_scalar_text_representations_have_golden_values_and_typed_params() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, label: \"original\"}")
            .ok
    );
    let vectors = vec![
        ("int", Value::Int(i64::MAX), "9223372036854775807"),
        ("int", Value::Int(i64::MIN), "-9223372036854775808"),
        ("float", Value::Float(1.0), "1.0"),
        ("float", Value::Float(-0.0), "0.0"),
        ("float", Value::Float(f64::from_bits(1)), "5e-324"),
        ("bool", Value::Bool(true), "true"),
        ("bool", Value::Bool(false), "false"),
        ("text", Value::Text("我\n\"\\".into()), "我\n\"\\"),
        (
            "uuid",
            "F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6"
                .parse::<Uuid>()
                .unwrap()
                .into(),
            "f81d4fae-7dec-11d0-a765-00a0c91e6bf6",
        ),
        (
            "date",
            "2026-10-03".parse::<Date>().unwrap().into(),
            "2026-10-03",
        ),
        (
            "timestamp",
            "2026-10-03T08:00:00.000001+08:00"
                .parse::<Timestamp>()
                .unwrap()
                .into(),
            "2026-10-03T00:00:00.000001Z",
        ),
        (
            "duration",
            Duration::from_microseconds(60_000_000).into(),
            "1minute",
        ),
        (
            "duration",
            Duration::from_microseconds(0).into(),
            "0microseconds",
        ),
        (
            "duration",
            Duration::from_microseconds(-90_000_000).into(),
            "-90seconds",
        ),
        (
            "Decimal<8, 2>",
            Decimal::parse("12", 8, 2).unwrap().into(),
            "12.00",
        ),
        (
            "bytes",
            "DEADBEEF".parse::<Bytes>().unwrap().into(),
            "deadbeef",
        ),
        ("bytes", Bytes::new(vec![]).unwrap().into(), ""),
    ];
    for (ty, input, expected) in vectors {
        let query = format!(
            "from samples | let show = (value: {ty}) -> to_text value | derive value = show $input | select value"
        );
        let prepared = engine.prepare(&query).unwrap();
        let response =
            engine.execute_prepared(&prepared, BTreeMap::from([("input".into(), input.clone())]));
        assert!(response.ok, "{ty}: {}", response.message);
        assert_eq!(
            response.typed_rows::<TextRow>().unwrap(),
            vec![TextRow {
                value: expected.into()
            }]
        );
        let mut request = unionid::protocol::Request::query("to-text", &query)
            .with_version(2)
            .unwrap();
        request
            .params
            .insert("input".into(), unionid::protocol::WireValue::from(&input));
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{ty}: {}", response.message);
        let decoded: unionid::protocol::Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(
            decoded.typed_rows::<TextRow>().unwrap(),
            vec![TextRow {
                value: expected.into()
            }]
        );
    }
}

#[test]
fn text_conversion_composes_with_nominal_scalars_formatter_and_v1_output() {
    let mut engine = Engine::memory();
    assert!(engine.execute("type Count = int\nstruct Record {id: int, count: Count, created: date}\ntable records: Record {key id}\ninsert records {id: 1, count: 12, created: @2026-10-03}").ok);
    let query = "from records | derive value = concat (to_text count) (concat \"@\" (to_text created)) | select value";
    let canonical = unionid::format_source(query).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    assert_eq!(
        engine.execute(&canonical).typed_rows::<TextRow>().unwrap(),
        vec![TextRow {
            value: "12@2026-10-03".into()
        }]
    );
    let explain = engine.execute(&format!("explain {query}"));
    assert!(explain.ok, "{}", explain.message);
    assert!(explain.rows.is_empty());
    assert_eq!(explain.plan.unwrap().result_schema[0].ty, "text");
    for version in [1, 2] {
        let request = unionid::protocol::Request::query("label", query)
            .with_version(version)
            .unwrap();
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        assert_eq!(
            response.typed_rows::<TextRow>().unwrap(),
            vec![TextRow {
                value: "12@2026-10-03".into()
            }]
        );
    }
    let response = engine.execute("from records | filter to_text count == \"12\" | select id");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn compound_adts_and_untyped_parameters_are_rejected_on_empty_inputs() {
    let mut engine = Engine::memory();
    let schema = engine.execute("enum State {Pending, Running {worker: text}}\nstruct Entry {id: int, state: State, maybe: Option<int>, list: List<int>, tuple: (int, text), dictionary: Map<text, int>, meta: {name: text}}\ntable entries: Entry {key id}");
    assert!(schema.ok, "{}", schema.message);
    for argument in [
        "state",
        "maybe",
        "list",
        "tuple",
        "dictionary",
        "meta",
        "None",
    ] {
        let response = engine.execute(&format!("from entries | derive value = to_text {argument}"));
        assert!(!response.ok, "{argument}");
        assert_eq!(response.error.unwrap().code, "E_TYPE");
    }
    let error = engine
        .prepare("from entries | derive value = to_text $unknown")
        .expect_err("to_text must not guess an input type");
    assert_eq!(error.code, "E_TYPE");
}

#[test]
fn text_conversion_works_in_update_and_migration_then_reopens() {
    let dir = common::TempDir::new();
    let path = dir.0.join("text.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute(SCHEMA).ok);
        assert!(
            engine
                .execute("insert samples {id: 1, label: \"original\"}")
                .ok
        );
        let response = engine.execute(
            "update samples | set label = concat \"item-\" (to_text id) | returning label",
        );
        assert!(response.ok, "{}", response.message);
        let response = engine
            .execute("migration text_id {change field Sample.id to text using old -> to_text old}");
        assert!(response.ok, "{}", response.message);
        engine.check_integrity().unwrap();
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        engine
            .execute("from samples | derive value = concat id label | select value")
            .typed_rows::<TextRow>()
            .unwrap(),
        vec![TextRow {
            value: "1item-1".into()
        }]
    );
    engine.check_integrity().unwrap();
}

#[test]
fn hex_expansion_is_bounded_before_allocation_and_rolls_back_prior_writes() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, label: \"original\"}")
            .ok
    );
    let source = "insert samples {id: 2, label: \"new\"}\nfrom samples | let show = (value: bytes) -> to_text value | derive value = show $payload | select value";
    let input = Bytes::new(vec![0; 8 * 1024 * 1024 + 1]).unwrap();
    let response =
        engine.execute_with_params(source, BTreeMap::from([("payload".into(), input.into())]));
    assert!(!response.ok);
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_LIMIT");
    assert!(
        error.message.contains("to_text result"),
        "{}",
        error.message
    );
    assert_eq!(engine.execute("from samples").rows.len(), 1);
}
