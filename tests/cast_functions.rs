mod common;

use serde::Deserialize;
use std::collections::BTreeMap;
use unionid::{Engine, Value};

const SCHEMA: &str =
    "struct Sample {id: int, qty: int, price: float}\ntable samples: Sample {key id}";

#[derive(Debug, Deserialize, PartialEq)]
struct Converted {
    value: f64,
}

#[test]
fn integer_conversion_checks_exact_binary_precision_including_endpoints() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, qty: 0, price: 0.5}")
            .ok
    );
    let prepared = engine
        .prepare("from samples | derive value = int_to_float $qty | select value")
        .unwrap();
    for value in [
        0,
        1,
        -1,
        1_i64 << 53,
        -(1_i64 << 53),
        (1_i64 << 53) + 2,
        i64::MIN,
        i64::MAX - 1023,
    ] {
        let response = engine.execute_prepared(
            &prepared,
            BTreeMap::from([("qty".into(), Value::Int(value))]),
        );
        assert!(response.ok, "{value}: {}", response.message);
        assert_eq!(
            response.typed_rows::<Converted>().unwrap(),
            vec![Converted {
                value: value as f64
            }]
        );
    }
    for value in [(1_i64 << 53) + 1, -((1_i64 << 53) + 1), i64::MAX] {
        let response = engine.execute_prepared(
            &prepared,
            BTreeMap::from([("qty".into(), Value::Int(value))]),
        );
        assert!(!response.ok, "{value}");
        assert_eq!(response.error.unwrap().code, "E_CAST_PRECISION");
    }
}

#[test]
fn conversion_composes_with_arithmetic_formatter_explain_and_wire() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, qty: 2, price: 0.5}")
            .ok
    );
    let query = "from samples | derive value = (int_to_float qty) + price | select value";
    let canonical = unionid::format_source(query).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    assert_eq!(
        engine
            .execute(&canonical)
            .typed_rows::<Converted>()
            .unwrap(),
        vec![Converted { value: 2.5 }]
    );
    let explain = engine.execute(
        "explain from samples | derive value = int_to_float 9223372036854775807 | select value",
    );
    assert!(explain.ok, "{}", explain.message);
    assert!(explain.rows.is_empty());
    for version in [1, 2] {
        let request = unionid::protocol::Request::query("cast", query)
            .with_version(version)
            .unwrap();
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        let decoded: unionid::protocol::Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(
            decoded.typed_rows::<Converted>().unwrap(),
            vec![Converted { value: 2.5 }]
        );
    }
    let mut empty = Engine::memory();
    assert!(empty.execute(SCHEMA).ok);
    for argument in ["price", "\"2\"", "None", "true"] {
        let response = empty.execute(&format!(
            "from samples | derive value = int_to_float {argument}"
        ));
        assert!(!response.ok, "{argument}");
        assert_eq!(response.error.unwrap().code, "E_TYPE");
    }
}

#[test]
fn precision_failure_rolls_back_mutations_and_migration_durably() {
    let dir = common::TempDir::new();
    let path = dir.0.join("cast.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute(SCHEMA).ok);
        assert!(engine.execute("insert samples {id: 1, qty: 2, price: 0.5}\ninsert samples {id: 2, qty: 9007199254740993, price: 1.5}").ok);
        let before = serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap();
        let response = engine.execute("insert samples {id: 3, qty: 3, price: 3.0}\nupdate samples | set price = int_to_float qty");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_CAST_PRECISION");
        assert_eq!(
            serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap(),
            before
        );
        let schema = engine.schema_info();
        let response = engine.execute("migration invalid_cast {change field Sample.qty to float using old -> int_to_float old}");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_CAST_PRECISION");
        assert_eq!(engine.schema_info(), schema);
        assert_eq!(
            serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap(),
            before
        );
        assert!(engine.execute("delete samples | filter id == 2").ok);
        let response = engine.execute(
            "migration exact_cast {change field Sample.qty to float using old -> int_to_float old}",
        );
        assert!(response.ok, "{}", response.message);
        engine.check_integrity().unwrap();
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        engine
            .execute("from samples | derive value = qty | select value")
            .typed_rows::<Converted>()
            .unwrap(),
        vec![Converted { value: 2.0 }]
    );
    engine.check_integrity().unwrap();
}
