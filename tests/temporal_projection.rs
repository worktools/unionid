mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use unionid::{
    Engine, Value,
    scalars::{Date, Timestamp},
};

const SCHEMA: &str =
    "struct Event {id: int, at: timestamp, day: date}\ntable events: Event {key id}";
const SEED: &str = "insert events {id: 1, at: @2026-10-02T18:00:00Z, day: @1970-01-01}";

#[derive(Debug, Deserialize, PartialEq)]
struct DateRow {
    value: Date,
}
#[derive(Debug, Deserialize, PartialEq)]
struct TimestampRow {
    value: Timestamp,
}

fn params(source: &str) -> BTreeMap<String, Value> {
    BTreeMap::from([("input".into(), source.parse::<Timestamp>().unwrap().into())])
}

#[test]
fn date_projection_has_explicit_offsets_and_civil_boundary_golden_values() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(engine.execute(SEED).ok);
    for (input, offset, expected) in [
        ("1970-01-01T00:00:00Z", "Z", "1970-01-01"),
        ("1969-12-31T23:30:00Z", "+01:00", "1970-01-01"),
        ("1970-01-01T00:15:00Z", "-01:00", "1969-12-31"),
        ("2024-02-29T23:45:00Z", "+00:30", "2024-03-01"),
        ("2026-10-03T02:00:00Z", "-03:30", "2026-10-02"),
        ("0001-01-01T00:00:00Z", "+00:00", "0001-01-01"),
        ("9999-12-31T23:59:59.999999Z", "z", "9999-12-31"),
    ] {
        let prepared = engine
            .prepare(&format!(
                "from events | derive value = date_of $input \"{offset}\" | select value"
            ))
            .unwrap();
        let response = engine.execute_prepared(&prepared, params(input));
        assert!(response.ok, "{input} {offset}: {}", response.message);
        assert_eq!(
            response.typed_rows::<DateRow>().unwrap()[0]
                .value
                .to_string(),
            expected
        );
    }
}

#[test]
fn calendar_and_clock_truncation_floor_and_are_idempotent_at_fixed_offsets() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(engine.execute(SEED).ok);
    for (unit, expected) in [
        ("year", "2023-12-31T18:30:00Z"),
        ("month", "2024-02-29T18:30:00Z"),
        ("week", "2024-02-25T18:30:00Z"),
        ("day", "2024-03-02T18:30:00Z"),
        ("hour", "2024-03-03T12:30:00Z"),
        ("minute", "2024-03-03T13:14:00Z"),
        ("second", "2024-03-03T13:14:15Z"),
        ("millisecond", "2024-03-03T13:14:15.123Z"),
        ("microsecond", "2024-03-03T13:14:15.123456Z"),
    ] {
        let prepared = engine.prepare(&format!("from events | derive value = timestamp_trunc $input \"{unit}\" \"+05:30\" | select value")).unwrap();
        let input: Timestamp = "2024-03-03T13:14:15.123456Z".parse().unwrap();
        let response = engine.execute_prepared(&prepared, params(&input.to_string()));
        assert!(response.ok, "{unit}: {}", response.message);
        let value = response.typed_rows::<TimestampRow>().unwrap()[0].value;
        assert_eq!(value.to_string(), expected);
        assert!(value <= input);
        assert_eq!(
            engine
                .execute_prepared(&prepared, params(expected))
                .typed_rows::<TimestampRow>()
                .unwrap()[0]
                .value,
            value
        );
    }
    for (unit, expected) in [
        ("second", "1969-12-31T23:59:59Z"),
        ("millisecond", "1969-12-31T23:59:59.999Z"),
        ("day", "1969-12-31T00:00:00Z"),
        ("week", "1969-12-29T00:00:00Z"),
    ] {
        let prepared = engine.prepare(&format!("from events | derive value = timestamp_trunc $input \"{unit}\" \"Z\" | select value")).unwrap();
        let response = engine.execute_prepared(&prepared, params("1969-12-31T23:59:59.999999Z"));
        assert!(response.ok, "{}", response.message);
        assert_eq!(
            response.typed_rows::<TimestampRow>().unwrap()[0]
                .value
                .to_string(),
            expected
        );
    }
}

#[test]
fn temporal_metadata_is_static_and_checked_before_empty_scans() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    for offset in [
        "\"America/New_York\"",
        "\"-00:00\"",
        "\"+24:00\"",
        "\"+00:60\"",
        "\"+8:00\"",
        "$offset",
        "day",
    ] {
        let error = engine
            .prepare(&format!("from events | derive value = date_of at {offset}"))
            .expect_err("invalid fixed offset");
        assert_eq!(error.code, "E_TEMPORAL_OFFSET", "{offset}");
    }
    for unit in ["\"fortnight\"", "$unit", "day", "1"] {
        let error = engine
            .prepare(&format!(
                "from events | derive value = timestamp_trunc at {unit} \"Z\""
            ))
            .expect_err("invalid unit");
        assert_eq!(error.code, "E_TEMPORAL_UNIT", "{unit}");
    }
    for expression in [
        "date_of day \"Z\"",
        "date_of 1 \"Z\"",
        "timestamp_trunc day \"day\" \"Z\"",
        "date_of at",
        "timestamp_trunc at \"day\"",
    ] {
        let response = engine.execute(&format!("from events | derive value = {expression}"));
        assert!(!response.ok, "{expression}");
        assert_eq!(response.error.unwrap().code, "E_TYPE");
    }
    assert!(engine.execute(SEED).ok);
    for expression in [
        "date_of @0001-01-01T00:00:00Z \"-00:01\"",
        "date_of @9999-12-31T23:59:59.999999Z \"+00:01\"",
        "timestamp_trunc @0001-01-01T00:00:00Z \"day\" \"+08:00\"",
        "timestamp_trunc @9999-12-31T23:59:59.999999Z \"microsecond\" \"+00:01\"",
    ] {
        let response = engine.execute(&format!("from events | derive value = {expression}"));
        assert!(!response.ok, "{expression}");
        assert_eq!(response.error.unwrap().code, "E_ARITH");
        let plan = engine.execute(&format!(
            "explain from events | derive value = {expression}"
        ));
        assert!(plan.ok, "{}", plan.message);
        assert!(plan.rows.is_empty());
    }
}

#[test]
fn nominal_timestamps_share_formatter_and_native_wire_contracts() {
    let mut engine = Engine::memory();
    assert!(engine.execute("type Moment = timestamp\nstruct NamedEvent {id: int, at: Moment}\ntable events: NamedEvent {key id}\ninsert events {id: 1, at: @2026-10-02T18:00:00Z}").ok);
    let query = "from events | derive value = date_of (timestamp_trunc at \"day\" \"+08:00\") \"+08:00\" | select value";
    let canonical = unionid::format_source(query).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    assert_eq!(
        engine.execute(&canonical).typed_rows::<DateRow>().unwrap()[0]
            .value
            .to_string(),
        "2026-10-03"
    );
    let request = unionid::protocol::Request::query("day", &canonical)
        .with_version(2)
        .unwrap();
    let response = unionid::server::execute_protocol_request(&mut engine, request);
    assert!(response.ok, "{}", response.message);
    let decoded: unionid::protocol::Response =
        serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
    assert_eq!(
        decoded.typed_rows::<DateRow>().unwrap()[0]
            .value
            .to_string(),
        "2026-10-03"
    );
    let request = unionid::protocol::Request::query(
        "legacy",
        format!("insert events {{id: 2, at: @2026-10-02T18:00:00Z}}\n{canonical}"),
    );
    let response = unionid::server::execute_protocol_request(&mut engine, request);
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_PROTOCOL_TYPE");
    assert_eq!(engine.execute("from events | select id").rows.len(), 1);
}

#[test]
fn temporal_projection_failures_roll_back_rows_schema_and_indexes_durably() {
    let dir = common::TempDir::new();
    let path = dir.0.join("temporal.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute(SCHEMA).ok);
        assert!(engine.execute(SEED).ok);
        assert!(engine.execute("insert events {id: 2, at: @9999-12-31T23:59:59.999999Z, day: @1970-01-01}\ncreate index events (day)").ok);
        let before = serde_json::to_value(engine.execute("from events | sort id").rows).unwrap();
        let response = engine.execute("insert events {id: 3, at: @2026-10-02T18:00:00Z, day: @1970-01-01}\nupdate events | set day = date_of at \"+08:00\"");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_ARITH");
        assert_eq!(
            serde_json::to_value(engine.execute("from events | sort id").rows).unwrap(),
            before
        );
        let schema = engine.schema_info();
        for conversion in [
            "timestamp using old -> timestamp_trunc old \"day\" \"+08:00\"",
            "date using old -> date_of old \"+08:00\"",
        ] {
            let response = engine.execute(&format!(
                "migration invalid_projection {{change field Event.at to {conversion}}}"
            ));
            assert!(!response.ok);
            assert_eq!(response.error.unwrap().code, "E_ARITH");
            assert_eq!(engine.schema_info(), schema);
            assert_eq!(
                serde_json::to_value(engine.execute("from events | sort id").rows).unwrap(),
                before
            );
        }
        assert!(engine.execute("delete events | filter id == 2").ok);
        assert!(
            engine
                .execute("update events | set day = date_of at \"+08:00\"")
                .ok
        );
        let response = engine.execute("migration day_bucket {change field Event.at to timestamp using old -> timestamp_trunc old \"day\" \"+08:00\"}");
        assert!(response.ok, "{}", response.message);
        let response = engine.execute("migration civil_date {change field Event.at to date using old -> date_of old \"+08:00\"}");
        assert!(response.ok, "{}", response.message);
        engine.check_integrity().unwrap();
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        engine
            .execute("from events | filter day == @2026-10-03 | derive value = at | select value")
            .typed_rows::<DateRow>()
            .unwrap()[0]
            .value
            .to_string(),
        "2026-10-03"
    );
    engine.check_integrity().unwrap();
}
