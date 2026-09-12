use std::process::Command;

mod common;
use common::TempDir;
use unionid::portable::TypeShape;
use unionid::{
    Engine, QUERY_DESCRIPTION_VERSION, QueryCardinality, QueryDescription, QueryOperation,
};

const SCHEMA: &str = r#"type State = Pending | Running {attempt int}
type Task = {id int, state State, score int}
table tasks Task
  key id"#;

#[test]
fn describes_bound_parameters_projection_derive_and_cardinality() {
    let description = unionid::query_contract::describe(
        SCHEMA,
        r#"from tasks
filter id >= $minimum
derive retryable =
  match state
    Pending => false
    Running {attempt} => attempt < $limit
select {id, retryable}"#,
    )
    .unwrap();
    assert_eq!(description.version, QUERY_DESCRIPTION_VERSION);
    assert_eq!(description.operation, QueryOperation::Read);
    assert_eq!(description.result.cardinality, QueryCardinality::Many);
    assert!(!description.result.affected_rows);
    assert_eq!(
        description
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect::<Vec<_>>(),
        ["limit", "minimum"]
    );
    assert!(
        description
            .parameters
            .iter()
            .all(|parameter| matches!(parameter.shape, TypeShape::Int { .. }))
    );
    assert_eq!(
        description
            .result
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "retryable"]
    );
    assert!(matches!(
        description.result.fields[1].shape,
        TypeShape::Bool
    ));
    assert!(description.query_digest.starts_with("sha256:"));
}

#[test]
fn describes_aggregate_and_bounded_read_cardinality() {
    let aggregate = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\naggregate\n  total = count\n  highest = max score",
    )
    .unwrap();
    assert_eq!(aggregate.result.cardinality, QueryCardinality::ExactlyOne);
    assert!(matches!(
        aggregate.result.fields[1].shape,
        TypeShape::Option { .. }
    ));

    let filtered = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\naggregate\n  total = count\nfilter total > 0",
    )
    .unwrap();
    assert_eq!(filtered.result.cardinality, QueryCardinality::AtMostOne);

    let bounded = unionid::query_contract::describe(SCHEMA, "from tasks | take 1").unwrap();
    assert_eq!(bounded.result.cardinality, QueryCardinality::AtMostOne);
}

#[test]
fn describes_returning_and_mutation_metadata() {
    let insert =
        unionid::query_contract::describe(SCHEMA, "insert tasks $task\nreturning {id, state}")
            .unwrap();
    assert_eq!(insert.operation, QueryOperation::Insert);
    assert_eq!(insert.result.cardinality, QueryCardinality::ExactlyOne);
    assert!(insert.result.affected_rows);
    assert!(matches!(
        insert.parameters[0].shape,
        TypeShape::Ref { ref name, .. } if name == "Task"
    ));
    assert!(matches!(
        insert.result.fields[1].shape,
        TypeShape::Ref { ref name, .. } if name == "State"
    ));

    let batch =
        unionid::query_contract::describe(SCHEMA, "insert many tasks $tasks\nreturning {id}")
            .unwrap();
    assert_eq!(batch.operation, QueryOperation::InsertMany);
    assert_eq!(batch.result.cardinality, QueryCardinality::Many);
    assert!(matches!(batch.parameters[0].shape, TypeShape::List { .. }));

    let update = unionid::query_contract::describe(
        SCHEMA,
        "update tasks\nfilter id == $id\nset score = $score",
    )
    .unwrap();
    assert_eq!(update.operation, QueryOperation::Update);
    assert_eq!(update.result.cardinality, QueryCardinality::None);
    assert!(update.result.fields.is_empty());
    assert!(update.result.affected_rows);
}

#[test]
fn canonical_digest_ignores_layout_but_tracks_query_shape() {
    let inline = unionid::query_contract::describe(
        SCHEMA,
        "from tasks | filter id >= $minimum | select {id, score}",
    )
    .unwrap();
    let multiline = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\nfilter id >= $minimum\nselect {id, score}",
    )
    .unwrap();
    assert_eq!(inline.canonical_source, multiline.canonical_source);
    assert_eq!(inline.query_digest, multiline.query_digest);

    let changed = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\nfilter id >= $minimum\nselect {id, state}",
    )
    .unwrap();
    assert_ne!(inline.query_digest, changed.query_digest);
}

#[test]
fn generation_errors_are_source_located_and_files_are_single_operation() {
    let field =
        unionid::query_contract::describe(SCHEMA, "from tasks | select missing").unwrap_err();
    assert_eq!(field.code, "E_FIELD");
    assert!(field.span.is_some());

    let coverage = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\nderive attempt = match state\n  Running {attempt} => attempt",
    )
    .unwrap_err();
    assert!(coverage.span.is_some());

    let multiple =
        unionid::query_contract::describe(SCHEMA, "from tasks | take 1\nfrom tasks | take 1")
            .unwrap_err();
    assert_eq!(multiple.code, "E_QUERY_FILE");
    assert!(multiple.span.is_some());
}

#[test]
fn cli_emits_the_same_versioned_contract() {
    let dir = TempDir::new();
    let schema = dir.0.join("schema.uid");
    let query = dir.0.join("find.uid");
    let output = dir.0.join("find.contract.json");
    std::fs::write(&schema, SCHEMA).unwrap();
    std::fs::write(&query, "from tasks | filter id == $id | take 1").unwrap();
    let command = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "query",
            "describe",
            "--schema",
            schema.to_str().unwrap(),
            "--file",
            query.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        command.status.success(),
        "{}",
        String::from_utf8_lossy(&command.stderr)
    );
    let description: QueryDescription =
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(description.version, QUERY_DESCRIPTION_VERSION);
    assert_eq!(description.schema.hash.len(), 71);
    assert_eq!(description.result.cardinality, QueryCardinality::AtMostOne);
}

#[test]
fn prepared_execution_still_rejects_a_different_runtime_schema() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SCHEMA).ok);
    let prepared = engine.prepare("from tasks | filter id == $id").unwrap();
    let description = engine
        .describe_query("from tasks | filter id == $id")
        .unwrap();
    assert_eq!(description.schema, prepared.schema().clone().into());
    assert!(engine.execute("type Extra = text").ok);
    let response = engine.execute_prepared(
        &prepared,
        std::collections::BTreeMap::from([("id".into(), unionid::Value::Int(1))]),
    );
    assert_eq!(response.error.unwrap().code, "E_SCHEMA_CHANGED");
}
