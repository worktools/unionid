mod common;

use common::TempDir;
use std::collections::BTreeMap;
use unionid::{Engine, InputStatus, Value, format_source, input_status};

const SCHEMA: &str = r#"type State = Pending | Running {attempt int}
type Item = {id int, left int, right int, state State}
table items Item
  key id
create unique index items (left)"#;
const ROWS: &str = r#"insert many items [
  {id = 1, left = 10, right = 20, state = Pending},
  {id = 2, left = 30, right = 40, state = Pending},
]"#;
const UPDATE: &str = r#"update items
filter id == $id
set {
  left = right,
  right = left,
  state = match state {
    Pending => Running {attempt = $attempt},
    current => current,
  },
}
returning {right, state, left}"#;

#[test]
fn field_set_swaps_and_adt_parameters_survive_redb_reopen() {
    let dir = TempDir::new();
    let path = dir.0.join("field-sets.redb");
    let mut memory = Engine::memory();
    let mut durable = Engine::open_redb(&path).unwrap();
    for engine in [&mut memory, &mut durable] {
        assert!(engine.execute(SCHEMA).ok);
        assert!(engine.execute(ROWS).ok);
        let prepared = engine.prepare(UPDATE).unwrap();
        let result = engine.execute_prepared(
            &prepared,
            BTreeMap::from([
                ("id".into(), Value::Int(1)),
                ("attempt".into(), Value::Int(3)),
            ]),
        );
        assert!(result.ok, "{}", result.message);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["right", "state", "left"]
        );
        assert!(result.rows[0]["left"].cmp_eq(&Value::Int(20)));
        assert!(result.rows[0]["right"].cmp_eq(&Value::Int(10)));
        assert_eq!(
            result.rows[0]["state"].source_text(),
            "Running {attempt = 3}"
        );
    }
    drop(durable);
    let mut reopened = Engine::open_redb(&path).unwrap();
    assert_eq!(
        serde_json::to_value(memory.execute("from items | sort id")).unwrap(),
        serde_json::to_value(reopened.execute("from items | sort id")).unwrap()
    );
}

#[test]
fn field_set_errors_are_checked_before_scan_and_rollback_all_assignments() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    for source in [
        "update items | set {left = 2, missing = 3}",
        "update items | set {left = 2, state = match state {Pending => Pending}}",
        "update items | set {left = 2, right = true}",
        "update items | set {left = 2} | set left = 3",
    ] {
        assert!(!engine.execute(source).ok, "{source}");
    }
    assert!(engine.execute(ROWS).ok);
    let before = serde_json::to_value(engine.execute("from items | sort id")).unwrap();
    for (source, code) in [
        ("update items | set {left = 5, right = 6}", "E_CONSTRAINT"),
        (
            "update items | set {left = right, right = 1 / (id - 2)}",
            "E_ARITH",
        ),
        (
            "update items | set {left = right, right = left} | returning missing",
            "E_FIELD",
        ),
    ] {
        let result = engine.execute(source);
        assert!(!result.ok, "{source}");
        assert_eq!(result.error.unwrap().code, code);
        assert_eq!(
            serde_json::to_value(engine.execute("from items | sort id")).unwrap(),
            before
        );
    }
}

#[test]
fn field_set_formatter_and_repl_have_explicit_boundaries() {
    let legacy = "update items\nset left = right\nset right = left\nreturning left, right";
    let formatted = format_source(legacy).unwrap();
    assert_eq!(
        formatted,
        "update items\nset {\n  left = right,\n  right = left,\n}\nreturning {left, right}\n"
    );
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert_eq!(
        format_source(&format_source(UPDATE).unwrap()).unwrap(),
        format_source(UPDATE).unwrap()
    );
    for source in [
        "update items | set {left = 1}",
        "update items | set {left = 1,}",
    ] {
        assert_eq!(input_status(source), InputStatus::Complete);
    }
    for source in [
        "update items | set {",
        "update items | set {left =",
        "update items | set {left = 1,",
    ] {
        assert!(
            matches!(input_status(source), InputStatus::Incomplete(_)),
            "{source}"
        );
    }
    for (source, message) in [
        (
            "update items | set {}",
            "set requires at least one assignment",
        ),
        (
            "update items | set {left = 1 right = 2}",
            "expected ',' between set assignments",
        ),
        (
            "update items | set {left = 1, left = 2}",
            "duplicate update field 'left'",
        ),
    ] {
        let InputStatus::Invalid(error) = input_status(source) else {
            panic!("{source}")
        };
        assert_eq!(error.message, message);
        assert!(error.span.is_some());
    }
}

#[test]
fn returning_budget_failure_does_not_publish_a_field_set() {
    let mut engine = Engine::memory();
    assert!(engine.execute("type Row = {id int, payload text}\ntable rows Row\ninsert rows {id = 1, payload = \"old\"}").ok);
    let result = engine.execute_with_params(
        "update rows | set {id = 2, payload = $payload} | returning",
        BTreeMap::from([(
            "payload".into(),
            Value::Text("x".repeat(unionid::db::MAX_RETURNING_BYTES + 1)),
        )]),
    );
    assert!(!result.ok);
    assert_eq!(result.error.unwrap().code, "E_LIMIT");
    let row = engine.execute("from rows");
    assert!(row.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(row.rows[0]["payload"].cmp_eq(&Value::Text("old".into())));
}

#[test]
fn nested_paths_and_multiline_results_keep_the_same_canonical_meaning() {
    let source = r#"type Meta = {a int, b int}
type Row = {id int, meta Meta, active bool}
table rows Row
insert rows {id = 1, meta = {a = 2, b = 3}, active = false}
update rows
set {
  meta.a = (
    meta.b + 1
  ),
  meta.b = meta.a,
  active = id > 0 and meta.a < 4 and meta.b > 1 and not active and meta.a != meta.b and meta.b < 100,
}
returning"#;
    let formatted = format_source(source).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    let original = Engine::memory().execute(source);
    let canonical = Engine::memory().execute(&formatted);
    assert!(original.ok, "{}", original.message);
    assert!(canonical.ok, "{}", canonical.message);
    assert_eq!(
        serde_json::to_value(original).unwrap(),
        serde_json::to_value(canonical).unwrap()
    );
    let mut engine = Engine::memory();
    assert!(engine.execute(source).ok);
    let failed = engine.execute("update rows | set {meta = {a = 1, b = 2}, meta.a = 3}");
    assert!(!failed.ok);
    assert_eq!(failed.error.unwrap().code, "E_QUERY");
}
