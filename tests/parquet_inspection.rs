mod common;

use std::fs::File;
use std::process::Command;
use std::sync::Arc;

use arrow_array::builder::{Int64Builder, MapBuilder, StringBuilder};
use arrow_array::types::Int64Type;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Decimal128Array, Int64Array, ListArray,
    RecordBatch, StringArray, StructArray, TimestampMicrosecondArray, UInt64Array,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use unionid::Value;

use common::TempDir;

fn write_fixture(path: &std::path::Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("active", DataType::Boolean, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int64Array::from(vec![1, 2, 3])),
            Arc::new(StringArray::from(vec![Some("Ada"), None, Some("Lin")])),
            Arc::new(BooleanArray::from(vec![true, false, true])),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn write_nested_fixture(path: &std::path::Path) {
    let profile = StructArray::from(vec![
        (
            Arc::new(Field::new("city", DataType::Utf8, false)),
            Arc::new(StringArray::from(vec!["Shanghai"])) as ArrayRef,
        ),
        (
            Arc::new(Field::new("score", DataType::Int64, true)),
            Arc::new(Int64Array::from(vec![Some(9)])) as ArrayRef,
        ),
    ]);
    let tags =
        ListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(vec![Some(1), None, Some(3)])]);
    let mut attributes = MapBuilder::new(None, StringBuilder::new(), Int64Builder::new());
    attributes.keys().append_value("priority");
    attributes.values().append_value(2);
    attributes.keys().append_value("attempt");
    attributes.values().append_value(1);
    attributes.append(true).unwrap();
    let attributes = attributes.finish();
    let amount = Decimal128Array::from(vec![Some(12_345_i128)])
        .with_precision_and_scale(12, 2)
        .unwrap();
    let occurred_at =
        TimestampMicrosecondArray::from(vec![1_700_000_000_000_000_i64]).with_timezone_utc();
    let columns = vec![
        Arc::new(profile) as ArrayRef,
        Arc::new(tags) as ArrayRef,
        Arc::new(attributes) as ArrayRef,
        Arc::new(amount) as ArrayRef,
        Arc::new(Date32Array::from(vec![19_000])) as ArrayRef,
        Arc::new(occurred_at) as ArrayRef,
    ];
    let schema = Arc::new(Schema::new(
        [
            "profile",
            "tags",
            "attributes",
            "amount",
            "day",
            "occurred_at",
        ]
        .into_iter()
        .zip(&columns)
        .map(|(name, column)| Field::new(name, column.data_type().clone(), false))
        .collect::<Vec<_>>(),
    ));
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns).unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn write_projection_fixture(path: &std::path::Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("too_large", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int64Array::from(vec![1])),
            Arc::new(UInt64Array::from(vec![u64::MAX])),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

#[test]
fn library_inspection_maps_schema_nullability_and_bounded_rows() {
    let temp = TempDir::new();
    let path = temp.0.join("people.parquet");
    write_fixture(&path);

    let report = unionid::parquet::inspect(&path, 2).unwrap();
    assert_eq!(report.rows_total, 3);
    assert_eq!(report.row_groups, 1);
    assert_eq!(report.preview_rows.len(), 2);
    assert!(report.preview_truncated);
    assert_eq!(report.columns[0].r#type, "int");
    assert_eq!(report.columns[1].r#type, "Option<text>");
    assert!(matches!(report.preview_rows[0]["id"], Value::Int(1)));
    assert!(matches!(
        report.preview_rows[0]["name"],
        Value::Option(Some(_))
    ));
    assert!(matches!(
        report.preview_rows[1]["name"],
        Value::Option(None)
    ));
}

#[test]
fn nested_products_collections_and_production_scalars_remain_typed() {
    let temp = TempDir::new();
    let path = temp.0.join("nested.parquet");
    write_nested_fixture(&path);

    let report = unionid::parquet::inspect(&path, 1).unwrap();
    assert_eq!(report.columns[0].r#type, "{ city text, score Option<int> }");
    assert_eq!(report.columns[1].r#type, "List<Option<int>>");
    assert_eq!(report.columns[2].r#type, "Map<text, Option<int>>");
    assert_eq!(report.columns[3].r#type, "Decimal<12, 2>");
    assert_eq!(report.columns[4].r#type, "date");
    assert_eq!(report.columns[5].r#type, "timestamp");
    let row = &report.preview_rows[0];
    assert!(matches!(row["profile"], Value::Record(_)));
    assert!(matches!(row["tags"], Value::List(_)));
    assert!(matches!(row["attributes"], Value::Map(_)));
    assert!(matches!(row["amount"], Value::Decimal(_)));
    assert!(matches!(row["day"], Value::Date(_)));
    assert!(matches!(row["occurred_at"], Value::Timestamp(_)));
}

#[test]
fn cli_prints_human_and_machine_readable_inspection() {
    let temp = TempDir::new();
    let path = temp.0.join("people.parquet");
    write_fixture(&path);

    let table = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["parquet", path.to_str().unwrap(), "--limit", "1"])
        .output()
        .unwrap();
    assert!(
        table.status.success(),
        "{}",
        String::from_utf8_lossy(&table.stderr)
    );
    let stdout = String::from_utf8(table.stdout).unwrap();
    assert!(stdout.contains("rows | 3"));
    assert!(stdout.contains("name | Option<text>"));
    assert!(stdout.contains("previewed 1 of 3 row(s) (truncated)"));

    let json = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "parquet",
            path.to_str().unwrap(),
            "--limit",
            "2",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["rows_total"], 3);
    assert_eq!(value["preview_rows"].as_array().unwrap().len(), 2);
}

#[test]
fn parquet_rows_use_the_normal_typed_query_pipeline() {
    let temp = TempDir::new();
    let path = temp.0.join("people.parquet");
    write_fixture(&path);

    let mut engine = unionid::parquet::query_engine(&path).unwrap();
    let response =
        engine.execute("from data | filter active | sort -id | select {id, name} | take 2");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 2);
    assert!(matches!(response.rows[0]["id"], Value::Int(3)));
    assert!(matches!(response.rows[1]["id"], Value::Int(1)));
    assert_eq!(response.columns[0].name, "id");
    assert_eq!(response.columns[1].name, "name");

    let explained = engine.execute("explain from data | filter active | select id");
    assert!(explained.ok, "{}", explained.message);
    let plan = explained.plan.unwrap();
    assert_eq!(plan.external_scan.as_deref(), Some("parquet_scan"));
    assert_eq!(plan.projected_columns, ["active", "id"]);

    let rejected = engine.execute("insert data {id = 4, name = None, active = true}");
    assert!(!rejected.ok);
    assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
}

#[test]
fn query_projection_skips_unused_column_values() {
    let temp = TempDir::new();
    let path = temp.0.join("projection.parquet");
    write_projection_fixture(&path);

    let mut engine = unionid::parquet::query_engine(&path).unwrap();
    let selected = engine.execute("from data | select id");
    assert!(selected.ok, "{}", selected.message);
    assert!(matches!(selected.rows[0]["id"], Value::Int(1)));

    let decoded = engine.execute("from data | select too_large");
    assert!(!decoded.ok);
    assert_eq!(decoded.error.unwrap().code, "E_PARQUET_VALUE");
}

#[test]
fn cli_executes_one_shot_parquet_queries_as_table_or_json() {
    let temp = TempDir::new();
    let path = temp.0.join("people.parquet");
    write_fixture(&path);

    let json = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "parquet",
            path.to_str().unwrap(),
            "--query",
            "from data | filter active | aggregate {people = count}",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["rows"][0]["people"]["value"], 2);

    let table = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "parquet",
            path.to_str().unwrap(),
            "--query",
            "from data | sort id | select {id, name} | take 1",
        ])
        .output()
        .unwrap();
    assert!(
        table.status.success(),
        "{}",
        String::from_utf8_lossy(&table.stderr)
    );
    let stdout = String::from_utf8(table.stdout).unwrap();
    assert!(stdout.contains("id | name"));
    assert!(stdout.contains("1 | Some(\"Ada\")"));
}

#[test]
fn corrupt_files_and_excessive_limits_fail_with_stable_codes() {
    let temp = TempDir::new();
    let path = temp.0.join("broken.parquet");
    std::fs::write(&path, b"not parquet").unwrap();

    let corrupt = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["parquet", path.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(!corrupt.status.success());
    let error: serde_json::Value = serde_json::from_slice(&corrupt.stdout).unwrap();
    assert_eq!(error["error"]["code"], "E_PARQUET_FORMAT");
    assert!(error["error"]["hint"].as_str().is_some());
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap())
    );
    assert_eq!(error["exit_code"], 3);

    let excessive = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "parquet",
            path.to_str().unwrap(),
            "--limit",
            "1001",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(!excessive.status.success());
    let error: serde_json::Value = serde_json::from_slice(&excessive.stdout).unwrap();
    assert_eq!(error["error"]["code"], "E_LIMIT");
}
