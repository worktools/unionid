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

#[derive(Debug, Deserialize, PartialEq)]
struct Rounded {
    value: i64,
}

#[derive(Debug, Deserialize, PartialEq)]
struct DecimalConverted {
    value: unionid::scalars::Decimal,
}

#[test]
fn explicit_decimal_results_match_assignment_and_migration_targets_before_scan() {
    let mut engine = Engine::memory();
    assert!(engine.execute("type Money = Decimal<4, 2>\nstruct Price {id: int, qty: int, amount: Money}\ntable prices: Price {key id}").ok);
    let schema = engine.schema_info();
    for expression in [
        "int_to_decimal qty 8 3",
        "decimal_parse \"1.25\" 8 3",
        "decimal_rescale amount 8 3",
        "decimal_round amount 8 3 \"exact\"",
        "decimal_mul amount amount 8 3 \"exact\"",
        "decimal_div amount amount 8 3 \"exact\"",
    ] {
        let source = format!("update prices | set amount = {expression}");
        for source in [source.clone(), format!("explain {source}")] {
            let response = engine.execute(&source);
            assert!(!response.ok, "{source}");
            assert_eq!(response.error.unwrap().code, "E_TYPE");
            let error = engine
                .prepare(&source)
                .expect_err("mismatched decimal output must not depend on row count");
            assert_eq!(error.code, "E_TYPE");
        }
    }
    let response = engine.execute("migration invalid_precision {change field Price.qty to Decimal<4, 2> using old -> int_to_decimal old 8 3}");
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TYPE");
    assert_eq!(engine.schema_info(), schema);
    assert!(
        engine
            .execute("insert prices {id: 1, qty: 12, amount: decimal \"0.00\"}")
            .ok
    );
    let before = serde_json::to_value(engine.execute("from prices").rows).unwrap();
    let response = engine.execute("insert prices {id: 2, qty: 1, amount: decimal \"0.00\"}\nupdate prices | set amount = int_to_decimal qty 8 3");
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TYPE");
    assert_eq!(
        serde_json::to_value(engine.execute("from prices").rows).unwrap(),
        before
    );
    let response = engine.execute("update prices | set amount = int_to_decimal qty 4 2");
    assert!(response.ok, "{}", response.message);
    #[derive(Deserialize)]
    struct Money(unionid::scalars::Decimal);
    #[derive(Deserialize)]
    struct MoneyRow {
        value: Money,
    }
    assert_eq!(
        engine
            .execute("from prices | derive value = amount | select value")
            .typed_rows::<MoneyRow>()
            .unwrap()[0]
            .value
            .0
            .to_string(),
        "12.00"
    );
}

#[test]
fn integer_decimal_conversion_preserves_units_and_full_i64_precision() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, qty: 12, price: 0.5}")
            .ok
    );
    let prepared = engine
        .prepare("from samples | derive value = int_to_decimal $input 38 19 | select value")
        .unwrap();
    for input in [0, 12, -12, i64::MIN, i64::MAX] {
        let response = engine.execute_prepared(
            &prepared,
            BTreeMap::from([("input".into(), Value::Int(input))]),
        );
        assert!(response.ok, "{input}: {}", response.message);
        let value = response.typed_rows::<DecimalConverted>().unwrap()[0].value;
        assert_eq!(value.coefficient(), i128::from(input) * 10_i128.pow(19));
        assert_eq!(value.scale(), 19);
    }
    let query = "from samples | derive value = (int_to_decimal qty 8 2) + decimal_parse \"0.50\" 8 2 | select value";
    let canonical = unionid::format_source(query).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    assert_eq!(
        engine
            .execute(&canonical)
            .typed_rows::<DecimalConverted>()
            .unwrap()[0]
            .value
            .to_string(),
        "12.50"
    );
    let request = unionid::protocol::Request::query("decimal-cast", query)
        .with_version(2)
        .unwrap();
    let response = unionid::server::execute_protocol_request(&mut engine, request);
    assert!(response.ok, "{}", response.message);
    let decoded: unionid::protocol::Response =
        serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
    assert_eq!(
        decoded.typed_rows::<DecimalConverted>().unwrap()[0]
            .value
            .to_string(),
        "12.50"
    );
    let request = unionid::protocol::Request::query(
        "legacy-cast",
        "insert samples {id: 2, qty: 1, price: 0.0}\nfrom samples | derive value = int_to_decimal qty 8 2",
    );
    let response = unionid::server::execute_protocol_request(&mut engine, request);
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_PROTOCOL_TYPE");
    assert_eq!(engine.execute("from samples").rows.len(), 1);
    let explain = engine.execute("explain from samples | derive value = int_to_decimal 100 2 0");
    assert!(explain.ok, "{}", explain.message);
    assert!(explain.rows.is_empty());
    assert_eq!(
        explain.plan.unwrap().result_schema.last().unwrap().ty,
        "Decimal<2, 0>"
    );
}

#[test]
fn integer_decimal_target_and_input_are_bound_before_empty_scans() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    for target in [
        "0 0",
        "39 0",
        "8 9",
        "-1 0",
        "8 -1",
        "$precision 2",
        "8 $scale",
        "8 2.0",
    ] {
        let error = engine
            .prepare(&format!(
                "from samples | derive value = int_to_decimal qty {target}"
            ))
            .expect_err("invalid decimal target");
        assert_eq!(error.code, "E_DECIMAL_TYPE", "{target}");
    }
    for input in ["price", "\"12\"", "None", "true"] {
        let response = engine.execute(&format!(
            "from samples | derive value = int_to_decimal {input} 8 2"
        ));
        assert!(!response.ok, "{input}");
        assert_eq!(response.error.unwrap().code, "E_TYPE");
    }
}

#[test]
fn integer_decimal_overflow_is_atomic_and_success_survives_reopen() {
    let dir = common::TempDir::new();
    let path = dir.0.join("decimal-cast.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute("struct Amount {id: int, qty: int, amount: Decimal<4, 2>}\ntable amounts: Amount {key id}\ninsert amounts {id: 1, qty: 12, amount: decimal \"0.00\"}\ninsert amounts {id: 2, qty: 100, amount: decimal \"0.00\"}").ok);
        let before = serde_json::to_value(engine.execute("from amounts | sort id").rows).unwrap();
        let response = engine.execute("insert amounts {id: 3, qty: 3, amount: decimal \"0.00\"}\nupdate amounts | set amount = int_to_decimal qty 4 2");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_DECIMAL_RANGE");
        assert_eq!(
            serde_json::to_value(engine.execute("from amounts | sort id").rows).unwrap(),
            before
        );
        let schema = engine.schema_info();
        let response = engine.execute("migration too_small {change field Amount.qty to Decimal<4, 2> using old -> int_to_decimal old 4 2}");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_DECIMAL_RANGE");
        assert_eq!(engine.schema_info(), schema);
        assert_eq!(
            serde_json::to_value(engine.execute("from amounts | sort id").rows).unwrap(),
            before
        );
        // Scaling can exceed i128 even before the target precision check.
        let response = engine.execute("from amounts | derive value = int_to_decimal 2 38 38");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_DECIMAL_RANGE");
        assert!(engine.execute("delete amounts | filter id == 2").ok);
        let response = engine
            .execute("update amounts | set amount = int_to_decimal qty 4 2 | returning amount");
        assert!(response.ok, "{}", response.message);
        let response = engine.execute("migration exact_amount {change field Amount.qty to Decimal<4, 2> using old -> int_to_decimal old 4 2}");
        assert!(response.ok, "{}", response.message);
        engine.check_integrity().unwrap();
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        engine
            .execute("from amounts | derive value = qty | select value")
            .typed_rows::<DecimalConverted>()
            .unwrap()[0]
            .value
            .to_string(),
        "12.00"
    );
    engine.check_integrity().unwrap();
}

#[test]
fn float_conversion_rounds_the_actual_binary_value_with_explicit_modes() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    assert!(
        engine
            .execute("insert samples {id: 1, qty: 0, price: 0.0}")
            .ok
    );
    let cases = [
        ("exact", 2.0, 2),
        ("exact", -0.0, 0),
        ("toward_zero", 2.9, 2),
        ("toward_zero", -2.9, -2),
        ("away_from_zero", 2.1, 3),
        ("away_from_zero", -2.1, -3),
        ("floor", 2.9, 2),
        ("floor", -2.1, -3),
        ("ceil", 2.1, 3),
        ("ceil", -2.9, -2),
        ("half_up", 2.5, 3),
        ("half_up", -2.5, -3),
        ("half_even", 2.5, 2),
        ("half_even", -2.5, -2),
        ("half_even", 3.5, 4),
        ("half_even", -3.5, -4),
        ("half_up", f64::from_bits(0.5_f64.to_bits() - 1), 0),
        ("half_up", -f64::from_bits(0.5_f64.to_bits() - 1), 0),
        ("ceil", f64::from_bits(1), 1),
        ("floor", -f64::from_bits(1), -1),
        ("exact", -9_223_372_036_854_775_808.0, i64::MIN),
        ("exact", 9_223_372_036_854_774_784.0, i64::MAX - 1023),
    ];
    for (mode, input, output) in cases {
        let prepared = engine
            .prepare(&format!(
                "from samples | derive value = float_to_int $input \"{mode}\" | select value"
            ))
            .unwrap();
        let response = engine.execute_prepared(
            &prepared,
            BTreeMap::from([("input".into(), Value::Float(input))]),
        );
        assert!(response.ok, "{mode} {input}: {}", response.message);
        assert_eq!(
            response.typed_rows::<Rounded>().unwrap(),
            vec![Rounded { value: output }]
        );
    }
    for mode in [
        "exact",
        "toward_zero",
        "away_from_zero",
        "floor",
        "ceil",
        "half_up",
        "half_even",
    ] {
        let prepared = engine
            .prepare(&format!(
                "from samples | derive value = float_to_int $input \"{mode}\" | select value"
            ))
            .unwrap();
        for input in [
            9_223_372_036_854_775_808.0,
            -9_223_372_036_854_777_856.0,
            f64::MAX,
            -f64::MAX,
        ] {
            let response = engine.execute_prepared(
                &prepared,
                BTreeMap::from([("input".into(), Value::Float(input))]),
            );
            assert!(!response.ok, "{mode} {input}");
            assert_eq!(response.error.unwrap().code, "E_CAST_RANGE");
        }
    }
    let response = engine.execute("from samples | derive value = float_to_int 0.5 \"exact\"");
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_CAST_PRECISION");
}

#[test]
fn float_conversion_validates_modes_before_scanning_and_shares_wire_contract() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    for mode in ["\"unknown\"", "$mode", "12", "price"] {
        let error = engine
            .prepare(&format!(
                "from samples | derive value = float_to_int price {mode}"
            ))
            .expect_err("invalid mode must fail before scanning");
        assert_eq!(error.code, "E_CAST_MODE", "{mode}");
    }
    let bad_type = engine.execute("from samples | derive value = float_to_int qty \"floor\"");
    assert!(!bad_type.ok);
    assert_eq!(bad_type.error.unwrap().code, "E_TYPE");
    assert!(
        engine
            .execute("insert samples {id: 1, qty: 0, price: -2.5}")
            .ok
    );
    let query = "from samples | derive value = float_to_int price \"half_up\" | select value";
    let canonical = unionid::format_source(query).unwrap();
    assert_eq!(unionid::format_source(&canonical).unwrap(), canonical);
    assert_eq!(
        engine.execute(&canonical).typed_rows::<Rounded>().unwrap(),
        vec![Rounded { value: -3 }]
    );
    for version in [1, 2] {
        let request = unionid::protocol::Request::query("round", query)
            .with_version(version)
            .unwrap();
        let response = unionid::server::execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        let decoded: unionid::protocol::Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(
            decoded.typed_rows::<Rounded>().unwrap(),
            vec![Rounded { value: -3 }]
        );
    }
    let explain =
        engine.execute("explain from samples | derive value = float_to_int 0.5 \"exact\"");
    assert!(explain.ok, "{}", explain.message);
    assert!(explain.rows.is_empty());
}

#[test]
fn float_range_failure_rolls_back_durable_write_and_schema_conversion() {
    let dir = common::TempDir::new();
    let path = dir.0.join("round.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        assert!(engine.execute(SCHEMA).ok);
        assert!(engine.execute("insert samples {id: 1, qty: 1, price: 2.5}\ninsert samples {id: 2, qty: 2, price: 9223372036854775808.0}").ok);
        let before = serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap();
        let response = engine.execute("insert samples {id: 3, qty: 3, price: 3.0}\nupdate samples | set qty = float_to_int price \"half_even\"");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_CAST_RANGE");
        assert_eq!(
            serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap(),
            before
        );
        let schema = engine.schema_info();
        let response = engine.execute("migration out_of_range {change field Sample.price to int using old -> float_to_int old \"floor\"}");
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "E_CAST_RANGE");
        assert_eq!(engine.schema_info(), schema);
        assert_eq!(
            serde_json::to_value(engine.execute("from samples | sort id").rows).unwrap(),
            before
        );
        assert!(engine.execute("delete samples | filter id == 2").ok);
        let response = engine.execute("migration rounded_price {change field Sample.price to int using old -> float_to_int old \"half_up\"}");
        assert!(response.ok, "{}", response.message);
        engine.check_integrity().unwrap();
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    assert_eq!(
        engine
            .execute("from samples | derive value = price | select value")
            .typed_rows::<Rounded>()
            .unwrap(),
        vec![Rounded { value: 3 }]
    );
    engine.check_integrity().unwrap();
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
