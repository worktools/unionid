mod common;

use common::TempDir;
use serde::{Deserialize, Serialize};
use unionid::{
    Engine, Value,
    db::Database,
    query::{Lookup, Pipeline, Stage, Statement},
};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct User {
    id: i64,
    name: String,
    email: String,
    note: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Line {
    id: i64,
    order_id: i64,
    sku: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct OrderWithLines {
    id: i64,
    customer: String,
    lines: Vec<Line>,
}

const LOOKUP_SCHEMA: &str = r#"
type Order = {id int, customer text}
type Line = {id int, order_id int, sku text}
table orders Order
  key id
table order_lines Line
  key id
create index order_lines (order_id)
insert orders {id = 1, customer = "Ada"}
insert orders {id = 2, customer = "Grace"}
insert orders {id = 3, customer = "Linus"}
insert order_lines {id = 10, order_id = 1, sku = "A"}
insert order_lines {id = 11, order_id = 1, sku = "B"}
insert order_lines {id = 12, order_id = 2, sku = "C"}
"#;

const SCHEMA: &str = "\
type User = {
  id int,
  name text,
  email text,
  note text,
}
table users User
  key id
create unique index users (email)
create index users (name)
insert users { id = 1, name = \"alice\", email = \"a@x\", note = \"n1\" }
insert users { id = 2, name = \"bob\", email = \"b@x\", note = \"n2\" }
insert users { id = 3, name = \"alice\", email = \"c@x\", note = \"n3\" }";

fn engine() -> Engine {
    let mut engine = Engine::memory();
    let response = engine.execute(SCHEMA);
    assert!(response.ok, "{}", response.message);
    engine
}

fn id_of(row: &Option<std::collections::BTreeMap<String, Value>>) -> Option<i64> {
    row.as_ref().and_then(|row| match row.get("id") {
        Some(Value::Int(id)) => Some(*id),
        _ => None,
    })
}

#[test]
fn fetch_by_key_preserves_input_order_and_missing_keys() {
    let mut engine = engine();
    let rows = engine
        .fetch_by_key(
            "users",
            "id",
            &[Value::Int(3), Value::Int(99), Value::Int(1)],
        )
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(id_of(&rows[0]), Some(3));
    assert!(rows[1].is_none());
    assert_eq!(id_of(&rows[2]), Some(1));
}

#[test]
fn typed_fetch_by_key_decodes_application_types() {
    let mut engine = engine();
    let rows = engine
        .typed_fetch_by_key::<User>("users", "id", &[Value::Int(2), Value::Int(42)])
        .unwrap();
    assert_eq!(
        rows[0],
        Some(User {
            id: 2,
            name: "bob".into(),
            email: "b@x".into(),
            note: "n2".into(),
        })
    );
    assert_eq!(rows[1], None);
}

#[test]
fn fetch_by_key_uses_a_unique_secondary_index() {
    let mut engine = engine();
    let rows = engine
        .fetch_by_key("users", "email", &[Value::Text("c@x".into())])
        .unwrap();
    assert_eq!(id_of(&rows[0]), Some(3));
}

#[test]
fn fetch_by_key_rejects_non_unique_matches() {
    let mut engine = engine();
    let error = engine
        .fetch_by_key("users", "name", &[Value::Text("alice".into())])
        .unwrap_err();
    assert_eq!(error.code, "E_RELATION_NOT_UNIQUE");
}

#[test]
fn fetch_by_key_requires_an_indexed_key_column() {
    let mut engine = engine();
    let error = engine
        .fetch_by_key("users", "note", &[Value::Text("n1".into())])
        .unwrap_err();
    assert_eq!(error.code, "E_RELATION_KEY");
}

#[test]
fn fetch_by_key_matches_manual_single_key_queries() {
    let mut engine = engine();
    let fetched = engine
        .fetch_by_key("users", "id", &[Value::Int(1), Value::Int(2)])
        .unwrap();
    let first = engine.execute("from users | filter id == 1 | take 1");
    assert!(first.ok, "{}", first.message);
    let second = engine.execute("from users | filter id == 2 | take 1");
    assert!(second.ok, "{}", second.message);
    assert_eq!(
        serde_json::to_value(fetched[0].as_ref()).unwrap(),
        serde_json::to_value(first.rows.first()).unwrap()
    );
    assert_eq!(
        serde_json::to_value(fetched[1].as_ref()).unwrap(),
        serde_json::to_value(second.rows.first()).unwrap()
    );
}

#[test]
fn fetch_by_key_is_bounded() {
    let mut engine = engine();
    let keys = (0..=Engine::MAX_BATCH_KEYS)
        .map(|id| Value::Int(id as i64))
        .collect::<Vec<_>>();
    let error = engine.fetch_by_key("users", "id", &keys).unwrap_err();
    assert_eq!(error.code, "E_LIMIT");
}

#[test]
fn fetch_by_key_works_on_a_durable_database() {
    let dir = TempDir::new();
    let path = dir.0.join("relational.redb");
    let mut engine = Engine::open_redb(&path).unwrap();
    assert!(engine.execute(SCHEMA).ok);
    let rows = engine
        .typed_fetch_by_key::<User>("users", "id", &[Value::Int(2)])
        .unwrap();
    assert_eq!(rows[0].as_ref().map(|user| user.id), Some(2));
    drop(engine);
    // Reopen from disk and fetch again over the recovered snapshot.
    let mut engine = Engine::open_redb(&path).unwrap();
    let rows = engine
        .fetch_by_key("users", "id", &[Value::Int(2), Value::Int(7)])
        .unwrap();
    assert_eq!(id_of(&rows[0]), Some(2));
    assert!(rows[1].is_none());
}

#[test]
fn fetch_by_key_rejects_identifiers_that_could_inject_statements() {
    let mut engine = engine();
    for (table, column) in [
        ("users\nupdate users | set note = \"x\"", "id"),
        ("users", "id\nupdate users | set note = \"x\""),
        ("users", "id; drop table users"),
        ("users", "id || true"),
        ("", "id"),
        ("users", "id.bad path"),
    ] {
        let error = engine
            .fetch_by_key(table, column, &[Value::Int(1)])
            .unwrap_err();
        assert!(
            matches!(error.code.as_str(), "E_TABLE" | "E_FIELD"),
            "{table:?}.{column:?} -> {} {}",
            error.code,
            error.message
        );
    }
    // No injected statement may have run.
    let rows = engine.execute("from users");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 3);
}

#[test]
fn lookup_builds_typed_nested_lists_and_preserves_driver_rows() {
    let mut engine = Engine::memory();
    assert!(engine.execute(LOOKUP_SCHEMA).ok);
    let response = engine.execute(
        "from orders\nsort id\nlookup lines from order_lines on order_id == id take 3\nselect {id, customer, lines}",
    );
    assert!(response.ok, "{}", response.message);
    for row in &response.rows {
        let Value::Int(order_id) = row["id"] else {
            panic!("order id must be an int")
        };
        let manual = engine.execute(&format!(
            "from order_lines | filter order_id == {order_id} | take 3"
        ));
        assert!(manual.ok, "{}", manual.message);
        assert_eq!(
            serde_json::to_value(&row["lines"]).unwrap(),
            serde_json::to_value(Value::List(
                manual.rows.into_iter().map(Value::Record).collect()
            ))
            .unwrap()
        );
    }
    let rows = response.typed_rows::<OrderWithLines>().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].lines.iter().map(|line| line.id).collect::<Vec<_>>(),
        [10, 11]
    );
    assert_eq!(
        rows[1].lines.iter().map(|line| line.id).collect::<Vec<_>>(),
        [12]
    );
    assert!(rows[2].lines.is_empty());
}

#[test]
fn lookup_after_page_is_stable_bounded_and_visible_in_explain() {
    let mut engine = Engine::memory();
    assert!(engine.execute(LOOKUP_SCHEMA).ok);
    let query = "from orders\nsort id\npage 2\nlookup lines from order_lines on order_id == id take 3\nselect {id, customer, lines}";
    let first = engine.execute(query);
    assert!(first.ok, "{}", first.message);
    assert_eq!(
        first
            .typed_rows::<OrderWithLines>()
            .unwrap()
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let cursor = first.page.unwrap().next_cursor.unwrap();
    let second = engine.execute(&format!(
        "from orders\nsort id\npage 2 after {}\nlookup lines from order_lines on order_id == id take 3\nselect {{id, customer, lines}}",
        serde_json::to_string(&cursor).unwrap()
    ));
    assert!(second.ok, "{}", second.message);
    assert_eq!(
        second
            .typed_rows::<OrderWithLines>()
            .unwrap()
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [3]
    );

    let explained = engine.execute(&format!("explain\n  {}", query.replace('\n', "\n  ")));
    assert!(explained.ok, "{}", explained.message);
    let plan = explained.plan.unwrap();
    assert_eq!(plan.lookups.len(), 1);
    assert_eq!(plan.lookups[0].index, "order_lines.order_id");
    assert_eq!(plan.lookups[0].per_row_limit, 3);

    let selected_before_lookup = engine.execute(
        "from orders\nsort id\npage 2\nselect {id, customer}\nlookup lines from order_lines on order_id == id take 3",
    );
    assert!(
        selected_before_lookup.ok,
        "{}",
        selected_before_lookup.message
    );
    let selected_rows = selected_before_lookup
        .typed_rows::<OrderWithLines>()
        .unwrap();
    assert_eq!(selected_rows.len(), 2);
    assert_eq!(selected_rows[0].lines.len(), 2);
}

#[test]
fn lookup_rejects_unindexed_mismatched_and_over_limit_relations() {
    let mut engine = Engine::memory();
    assert!(engine.execute(LOOKUP_SCHEMA).ok);
    let unindexed =
        engine.execute("from orders | lookup lines from order_lines on sku == customer take 3");
    assert_eq!(unindexed.error.unwrap().code, "E_RELATION_KEY");

    assert!(engine.execute("create index order_lines (sku)").ok);
    let mismatched =
        engine.execute("from orders | lookup lines from order_lines on sku == id take 3");
    assert_eq!(mismatched.error.unwrap().code, "E_TYPE");

    assert!(
        engine
            .execute("insert order_lines {id = 13, order_id = 1, sku = \"D\"}")
            .ok
    );
    let overflow = engine.execute(
        "from orders | filter id == 1 | lookup lines from order_lines on order_id == id take 2",
    );
    assert_eq!(overflow.error.unwrap().code, "E_RELATION_LIMIT");
}

#[test]
fn lookup_formatter_is_idempotent() {
    let source = "from orders | sort id | page 20 | lookup lines from order_lines on order_id == id take 100 | select {id, lines}";
    let formatted = unionid::format_source(source).unwrap();
    assert_eq!(unionid::format_source(&formatted).unwrap(), formatted);
    assert!(formatted.contains("lookup lines from order_lines on order_id == id take 100"));
}

#[test]
fn direct_lookup_ast_cannot_bypass_the_match_limit() {
    let mut database = Database::default();
    for statement in unionid::syntax::parse(
        "type Order = {id int}\ntype Line = {id int, order_id int}\ntable orders Order\n  key id\ntable order_lines Line\n  key id\ncreate index order_lines (order_id)",
    )
    .unwrap()
    {
        database.execute(statement.statement).unwrap();
    }
    for limit in [0, usize::MAX] {
        let error = database
            .execute(Statement::Pipeline(Pipeline {
                from: "orders".into(),
                stages: vec![Stage::Lookup(Lookup {
                    name: "lines".into(),
                    table: "order_lines".into(),
                    target_key: "order_id".into(),
                    source_key: "id".into(),
                    limit,
                    output_type: None,
                    index: None,
                })],
            }))
            .unwrap_err();
        assert_eq!(error.code, "E_LIMIT");
    }
}

#[test]
fn lookup_reads_nested_rows_after_redb_reopen() {
    let dir = TempDir::new();
    let path = dir.0.join("lookup.redb");
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        let setup = engine.execute(LOOKUP_SCHEMA);
        assert!(setup.ok, "{}", setup.message);
    }
    let mut engine = Engine::open_redb(&path).unwrap();
    let response = engine.execute(
        "from orders | filter id == 1 | lookup lines from order_lines on order_id == id take 3",
    );
    assert!(response.ok, "{}", response.message);
    let rows = response.typed_rows::<OrderWithLines>().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].lines.iter().map(|line| line.id).collect::<Vec<_>>(),
        [10, 11]
    );
}

#[test]
fn paged_lookup_keeps_a_ten_thousand_row_driver_bounded() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "type Order = {id int, customer text}\n\
                 type Line = {id int, order_id int}\n\
                 table orders Order\n  key id\n\
                 table order_lines Line\n  key id\n\
                 create index order_lines (order_id)"
            )
            .ok
    );
    let values = Value::List(
        (0..10_000)
            .map(|id| {
                Value::Record(std::collections::BTreeMap::from([
                    ("id".into(), Value::Int(id)),
                    ("customer".into(), Value::Text(format!("c{id}"))),
                ]))
            })
            .collect(),
    );
    let inserted = engine.execute_with_params(
        "insert many orders $rows",
        std::collections::BTreeMap::from([("rows".into(), values)]),
    );
    assert!(inserted.ok, "{}", inserted.message);

    let response = engine.execute(
        "from orders\nsort id\npage 25\nlookup lines from order_lines on order_id == id take 10",
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 25);
    assert!(response.rows.iter().all(|row| matches!(
        row.get("lines"),
        Some(Value::List(values)) if values.is_empty()
    )));
    let observation = response.execution.unwrap();
    assert!(observation.rows_decoded <= 26, "{observation:?}");
    assert!(
        observation.working_peak_bytes < 1024 * 1024,
        "{observation:?}"
    );
}
