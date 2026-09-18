mod common;

use common::TempDir;
use serde::{Deserialize, Serialize};
use unionid::{
    Engine, QueryAccessKind, Value, backup, format_source, protocol::Request, scalars::Decimal,
    server::execute_protocol_request,
};

const SETUP: &str = r#"type Currency = CNY | USD

type Invoice = {
  id int,
  amount decimal 18 2,
  currency Currency,
}

table invoices Invoice
  key id

create index invoices (amount)

insert many invoices [
  {id = 1, amount = decimal "19.9", currency = CNY},
  {id = 2, amount = decimal "-0.00", currency = USD},
  {id = 3, amount = decimal "5", currency = CNY},
]"#;

#[test]
fn decimal_types_literals_queries_arithmetic_and_sum_are_exact() {
    let mut engine = Engine::memory();
    let formatted = format_source(SETUP).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert!(engine.execute(&formatted).ok);
    let result = engine.execute(
        r#"from invoices
derive {with_fee = amount + decimal "0.10", reversed = -amount}
filter amount >= decimal "0"
sort amount
select {id, amount, with_fee, reversed}"#,
    );
    assert!(result.ok, "{}", result.message);
    assert_eq!(result.rows[0]["amount"].source_text(), "decimal \"0.00\"");
    assert_eq!(
        result.rows[2]["with_fee"].source_text(),
        "decimal \"20.00\""
    );

    let aggregate = engine.execute(
        "from invoices | aggregate {total = sum amount, smallest = min amount, largest = max amount}",
    );
    assert!(aggregate.ok, "{}", aggregate.message);
    assert_eq!(
        aggregate.rows[0]["total"].source_text(),
        "decimal \"24.90\""
    );

    let average = engine.execute("from invoices | aggregate {mean = avg amount}");
    let error = average
        .error
        .expect("decimal avg must require explicit rounding");
    assert_eq!(error.code, "E_TYPE");
    assert!(error.message.contains("precision and rounding"));

    let average =
        engine.execute(r#"from invoices | aggregate {mean = decimal_avg amount 18 2 "half_even"}"#);
    assert!(average.ok, "{}", average.message);
    assert_eq!(
        average.rows[0]["mean"].source_text(),
        "Some(decimal \"8.30\")"
    );
}

#[test]
fn explicit_decimal_multiply_divide_and_rounding_cover_ties_and_errors() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                r#"type Number = {id int, value decimal 18 2}
table numbers Number
  key id
insert many numbers [
  {id = 1, value = decimal "2.50"},
  {id = 2, value = decimal "-2.50"},
]"#,
            )
            .ok
    );
    let result = engine.execute(
        r#"from numbers
sort id
derive {
  product_even = decimal_mul value decimal "1.25" 18 2 "half_even"
  product_up = decimal_mul value decimal "1.25" 18 2 "half_up"
  quotient = decimal_div value decimal "3.00" 18 3 "half_even"
  rounded_even = decimal_round value 18 0 "half_even"
  rounded_up = decimal_round value 18 0 "half_up"
}"#,
    );
    assert!(result.ok, "{}", result.message);
    let source = r#"from numbers | aggregate {mean = decimal_avg value 18 2 "half_even"}"#;
    let formatted = format_source(source).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert_eq!(
        result.rows[0]["product_even"].source_text(),
        "decimal \"3.12\""
    );
    assert_eq!(
        result.rows[0]["product_up"].source_text(),
        "decimal \"3.13\""
    );
    assert_eq!(
        result.rows[0]["quotient"].source_text(),
        "decimal \"0.833\""
    );
    assert_eq!(
        result.rows[0]["rounded_even"].source_text(),
        "decimal \"2\""
    );
    assert_eq!(result.rows[0]["rounded_up"].source_text(), "decimal \"3\"");
    assert_eq!(
        result.rows[1]["rounded_even"].source_text(),
        "decimal \"-2\""
    );
    assert_eq!(result.rows[1]["rounded_up"].source_text(), "decimal \"-3\"");

    let exact = engine
        .execute(r#"from numbers | derive bad = decimal_div value decimal "3.00" 18 2 "exact""#);
    assert_eq!(exact.error.unwrap().code, "E_ARITH");
    let mutation = engine.execute(
        r#"update numbers | filter id == 1 | set value = decimal_div value decimal "3.00" 18 2 "exact""#,
    );
    assert_eq!(mutation.error.unwrap().code, "E_ARITH");
    assert_eq!(
        engine.execute("from numbers | filter id == 1").rows[0]["value"].source_text(),
        "decimal \"2.50\""
    );
    let zero = engine.execute(
        r#"from numbers | derive bad = decimal_div value decimal "0.00" 18 2 "half_even""#,
    );
    assert_eq!(zero.error.unwrap().code, "E_ARITH");
    let bad_mode =
        engine.execute(r#"from numbers | derive bad = decimal_round value 18 2 "bankers""#);
    assert_eq!(bad_mode.error.unwrap().code, "E_DECIMAL_ROUNDING");
}

#[test]
fn decimal_range_is_checked_at_each_arithmetic_and_sum_step() {
    let mut engine = Engine::memory();
    assert!(engine.execute("type Amount = {id int, value decimal 5 2}\ntable amounts Amount\n  key id\ninsert many amounts [{id = 1, value = decimal \"999.99\"}, {id = 2, value = decimal \"0.01\"}, {id = 3, value = decimal \"-0.01\"}]").ok);
    let arithmetic = engine.execute(
        "update amounts | filter id == 1 | set value = value + decimal \"0.01\" - decimal \"0.01\"",
    );
    assert_eq!(arithmetic.error.unwrap().code, "E_ARITH");
    assert_eq!(
        engine.execute("from amounts | filter id == 1").rows[0]["value"].source_text(),
        "decimal \"999.99\""
    );

    let sum = engine.execute("from amounts | sort id | aggregate {total = sum value}");
    assert_eq!(sum.error.unwrap().code, "E_ARITH");
}

#[test]
fn indexed_and_scanned_decimal_queries_agree() {
    let schema = "type Price = {id int, value decimal 8 2}\ntable prices Price\n  key id\ninsert many prices [{id = 1, value = decimal \"1.20\"}, {id = 2, value = decimal \"-2.00\"}, {id = 3, value = decimal \"1.2\"}]";
    let query = "from prices | filter value == decimal \"1.20\" | sort id";
    let mut scanned = Engine::memory();
    assert!(scanned.execute(schema).ok);
    let scanned_rows = scanned.execute(query).rows;
    assert_eq!(
        scanned
            .execute(&format!("explain {query}"))
            .plan
            .unwrap()
            .access
            .kind,
        QueryAccessKind::OrderedScan
    );

    let mut indexed = Engine::memory();
    assert!(indexed.execute(schema).ok);
    assert!(indexed.execute("create index prices (value)").ok);
    let indexed_rows = indexed.execute(query).rows;
    assert_eq!(
        indexed
            .execute(&format!("explain {query}"))
            .plan
            .unwrap()
            .access
            .kind,
        QueryAccessKind::SecondaryIndexLookup
    );
    let canonical = |rows: &[std::collections::BTreeMap<String, Value>]| {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|(name, value)| (name.clone(), value.source_text()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(canonical(&indexed_rows), canonical(&scanned_rows));
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct NativeInvoice {
    id: i64,
    amount: Decimal,
    currency: Currency,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Currency {
    #[serde(rename = "CNY")]
    Cny,
    #[serde(rename = "USD")]
    Usd,
}

#[test]
fn source_rust_wire_redb_backup_and_cursor_preserve_decimal() {
    let dir = TempDir::new();
    let db = dir.0.join("decimal.redb");
    let archive = dir.0.join("decimal.backup.json");
    let restored = dir.0.join("decimal-restored.redb");
    let row = NativeInvoice {
        id: 1,
        amount: Decimal::parse("19.9", 18, 2).unwrap(),
        currency: Currency::Cny,
    };
    let mut engine = Engine::open_redb(&db).unwrap();
    assert!(engine.execute("type Currency = CNY | USD\ntype Invoice = {id int, amount decimal 18 2, currency Currency}\ntable invoices Invoice\n  key id\ncreate unique index invoices (amount)").ok);
    let response = execute_protocol_request(
        &mut engine,
        Request::query("insert", "insert invoices $row\nreturning")
            .with_version(2)
            .unwrap()
            .with_serde_param("row", &row)
            .unwrap(),
    );
    let decoded = response.typed_rows::<NativeInvoice>().unwrap();
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], row);
    assert!(
        engine
            .execute("from invoices | sort {amount, id} | page 1")
            .ok
    );
    drop(engine);
    backup::create(&db, &archive).unwrap();
    backup::restore(&archive, &restored).unwrap();
    let mut restored = Engine::open_redb(&restored).unwrap();
    let response = execute_protocol_request(
        &mut restored,
        Request::query("read", "from invoices")
            .with_version(2)
            .unwrap(),
    );
    assert_eq!(response.typed_rows::<NativeInvoice>().unwrap(), [row]);
}

#[test]
fn decimal_parse_and_rescale_migrations_are_exact_and_atomic() {
    let mut engine = Engine::memory();
    let response = engine.execute(
        r#"type Legacy = {id int, amount text}
table invoices Legacy
  key id
insert invoices {id = 1, amount = "19.9"}
migration parse_amount
  change field Legacy.amount to decimal 18 2 using old -> decimal_parse old 18 2
migration narrow_amount
  change field Legacy.amount to decimal 10 2 using old -> decimal_rescale old 10 2
from invoices"#,
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.rows[0]["amount"].source_text(),
        "decimal \"19.90\""
    );

    let before = engine.schema_info();
    let failed = engine.execute("migration inexact\n  change field Legacy.amount to decimal 10 0 using old -> decimal_rescale old 10 0");
    assert_eq!(failed.error.unwrap().code, "E_DECIMAL_RANGE");
    assert_eq!(engine.schema_info(), before);
}

#[test]
fn invalid_decimal_types_literals_and_deferred_operations_fail_closed() {
    let mut engine = Engine::memory();
    for source in [
        "type Bad = {v decimal 0 0}",
        "type Bad = {v decimal 39 0}",
        "type Bad = {v decimal 4 5}",
        "type Bad = {v decimal 5 2}\ntable bad Bad\ninsert bad {v = decimal \"1e2\"}",
        "type Bad = {v decimal 5 2}\ntable bad Bad\ninsert bad {v = decimal \"1.234\"}",
    ] {
        assert!(!engine.execute(source).ok, "accepted {source}");
    }
    assert!(engine.execute("type Good = {id int, v decimal 5 2}\ntable good Good\n  key id\ninsert good {id = 1, v = decimal \"2.00\"}").ok);
    for query in [
        "from good | derive bad = v * decimal \"2.00\"",
        "from good | derive bad = v / decimal \"2.00\"",
    ] {
        assert_eq!(engine.execute(query).error.unwrap().code, "E_TYPE");
    }
}
