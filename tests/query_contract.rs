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

    let union = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\naggregate {total = count}\nunion {\n  from tasks\n  aggregate {total = count}\n}",
    )
    .unwrap();
    assert_eq!(union.result.cardinality, QueryCardinality::Many);

    let except = unionid::query_contract::describe(
        SCHEMA,
        "from tasks\naggregate {total = count}\nexcept {\n  from tasks\n  aggregate {total = count}\n}",
    )
    .unwrap();
    assert_eq!(except.result.cardinality, QueryCardinality::AtMostOne);
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

#[test]
fn generates_typed_rust_params_rows_and_cardinality_calls() {
    let generated = unionid::codegen::rust_query(
        SCHEMA,
        "from tasks\nfilter id == $id\nselect {id, state}\ntake 1",
        "find_task",
    )
    .unwrap();
    assert!(generated.contains("pub struct FindTaskParams {"));
    assert!(generated.contains("pub id: i64,"));
    assert!(generated.contains("pub struct FindTaskRow {"));
    assert!(generated.contains("pub state: State,"));
    assert!(generated.contains("pub fn find_task("));
    assert!(generated.contains("params: FindTaskParams,"));
    assert!(generated.contains(") -> unionid::Result<Option<FindTaskRow>>"));
    assert!(generated.contains("FIND_TASK_SCHEMA_REVISION"));
    assert!(generated.contains("E_SCHEMA_CHANGED"));
    assert!(generated.contains("Value::from_serde(&params.id)"));

    let mutation = unionid::codegen::rust_query(
        SCHEMA,
        "insert tasks $task\nreturning {id, state}",
        "create_task",
    )
    .unwrap();
    assert!(mutation.contains("pub task: Task,"));
    assert!(mutation.contains("pub struct CreateTaskOutput {"));
    assert!(mutation.contains("pub rows: CreateTaskRow,"));
    assert!(mutation.contains("pub affected_rows: usize,"));
}

#[test]
fn generates_multiple_queries_with_one_shared_schema_model() {
    let queries = [
        (
            "create_task".into(),
            "insert tasks $task\nreturning {id, state}".into(),
        ),
        (
            "find_task".into(),
            "from tasks | filter id == $id | select {id, state} | take 1".into(),
        ),
    ];
    let generated = unionid::codegen::rust_query_bundle(SCHEMA, &queries).unwrap();
    assert_eq!(generated.matches("pub enum State {").count(), 1);
    assert_eq!(generated.matches("pub struct Task {").count(), 1);
    assert!(generated.contains("pub mod create_task {"));
    assert!(generated.contains("pub task: Task,"));
    assert!(generated.contains("pub mod find_task {"));
    assert!(generated.contains("pub state: State,"));
    let reversed = queries.into_iter().rev().collect::<Vec<_>>();
    assert_eq!(
        generated,
        unionid::codegen::rust_query_bundle(SCHEMA, &reversed).unwrap()
    );

    let collision = unionid::codegen::rust_query_bundle(
        SCHEMA,
        &[
            ("find-task".into(), "from tasks".into()),
            ("find_task".into(), "from tasks".into()),
        ],
    )
    .unwrap_err();
    assert_eq!(collision.code, "E_QUERY_BINDING");
    assert!(collision.message.contains("collides"));

    let keyword = unionid::codegen::rust_query_bundle(
        SCHEMA,
        &[("type".into(), "from tasks | take 1".into())],
    )
    .unwrap();
    assert!(keyword.contains("pub mod type_ {"));
    assert!(keyword.contains("pub const TYPE_SOURCE"));
}

#[test]
fn cli_generates_rust_query_file_with_an_inferred_or_explicit_name() {
    let dir = TempDir::new();
    let schema = dir.0.join("schema.uid");
    let query = dir.0.join("find-task.uid");
    let output = dir.0.join("find_task.rs");
    std::fs::write(&schema, SCHEMA).unwrap();
    std::fs::write(&query, "from tasks | filter id == $id | take 1").unwrap();

    let command = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "query",
            "rust",
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
    let generated = std::fs::read_to_string(&output).unwrap();
    assert!(generated.contains("pub struct FindTaskParams"));
    assert!(generated.contains("pub fn find_task("));

    let explicit = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "query",
            "rust",
            "--schema",
            schema.to_str().unwrap(),
            "--file",
            query.to_str().unwrap(),
            "--name",
            "lookup_task",
        ])
        .output()
        .unwrap();
    assert!(explicit.status.success());
    assert!(
        String::from_utf8(explicit.stdout)
            .unwrap()
            .contains("pub fn lookup_task(")
    );
}

#[test]
fn cli_generates_a_deterministic_query_directory_bundle_without_partial_failures() {
    let dir = TempDir::new();
    let schema = dir.0.join("schema.uid");
    let queries = dir.0.join("queries");
    let nested = queries.join("tasks");
    let output = dir.0.join("queries.rs");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(&schema, SCHEMA).unwrap();
    std::fs::write(
        queries.join("create.uid"),
        "insert tasks $task\nreturning {id, state}",
    )
    .unwrap();
    std::fs::write(
        nested.join("find.uid"),
        "from tasks | filter id == $id | take 1",
    )
    .unwrap();

    let command = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "query",
            "rust",
            "--schema",
            schema.to_str().unwrap(),
            "--dir",
            queries.to_str().unwrap(),
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
    let generated = std::fs::read_to_string(&output).unwrap();
    assert!(
        generated.find("pub mod create").unwrap() < generated.find("pub mod tasks_find").unwrap()
    );
    assert_eq!(generated.matches("pub enum State {").count(), 1);

    std::fs::write(&output, "keep this complete output").unwrap();
    std::fs::write(queries.join("create!.uid"), "from missing").unwrap();
    let failed = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "query",
            "rust",
            "--schema",
            schema.to_str().unwrap(),
            "--dir",
            queries.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("query 'create!':"));
    assert_eq!(
        std::fs::read_to_string(output).unwrap(),
        "keep this complete output"
    );
}

#[test]
fn generated_result_fields_escape_nested_paths_for_serde() {
    let schema = r#"type Endpoint = {host text, port int}
type Config = {id int, endpoint Endpoint}
table configs Config
  key id"#;
    let generated = unionid::codegen::rust_query(
        schema,
        "from configs | select {endpoint.host, endpoint.port}",
        "list_endpoints",
    )
    .unwrap();
    assert!(generated.contains("#[serde(rename = \"endpoint.host\")]"));
    assert!(generated.contains("pub endpoint_host: String,"));
    assert!(generated.contains("#[serde(rename = \"endpoint.port\")]"));
    assert!(generated.contains("pub endpoint_port: i64,"));
}

#[test]
fn generated_zero_row_queries_have_a_unit_result() {
    let generated =
        unionid::codegen::rust_query(SCHEMA, "from tasks | take 0", "no_tasks").unwrap();
    assert!(generated.contains("params: NoTasksParams,"));
    assert!(generated.contains(") -> unionid::Result<()>"));
    assert!(generated.contains("let _ = params;"));
    assert!(!generated.contains("typed_rows::<NoTasksRow>"));
}

#[test]
fn database_generation_preserves_the_live_migration_identity() {
    let dir = TempDir::new();
    let database = dir.0.join("app.redb");
    let expected = {
        let mut engine = Engine::open_redb(&database).unwrap();
        assert!(engine.execute(SCHEMA).ok);
        assert!(engine.execute("type Extra = text").ok);
        let expected = engine.schema_info();
        let generated = unionid::codegen::rust_query_for_engine(
            &engine,
            "from tasks | filter id == $id | take 1",
            "find_task",
        )
        .unwrap();
        assert!(generated.contains(&format!("SCHEMA_REVISION: u64 = {};", expected.revision)));
        assert!(generated.contains(&expected.hash));
        expected
    };
    let query = dir.0.join("find_task.uid");
    let query_directory = dir.0.join("queries");
    let rust_output = dir.0.join("find_task.rs");
    let bundle_output = dir.0.join("queries.rs");
    let json_output = dir.0.join("find_task.json");
    std::fs::create_dir(&query_directory).unwrap();
    std::fs::write(&query, "from tasks | filter id == $id | take 1").unwrap();
    std::fs::write(
        query_directory.join("find_task.uid"),
        "from tasks | filter id == $id | take 1",
    )
    .unwrap();

    for (subcommand, output) in [("rust", &rust_output), ("describe", &json_output)] {
        let command = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args([
                "query",
                subcommand,
                "--db",
                database.to_str().unwrap(),
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
    }
    let generated = std::fs::read_to_string(rust_output).unwrap();
    assert!(generated.contains(&format!("SCHEMA_REVISION: u64 = {};", expected.revision)));
    assert!(generated.contains(&expected.hash));
    let bundled = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "query",
            "rust",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            query_directory.to_str().unwrap(),
            "--output",
            bundle_output.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        bundled.status.success(),
        "{}",
        String::from_utf8_lossy(&bundled.stderr)
    );
    let bundled = std::fs::read_to_string(bundle_output).unwrap();
    assert!(bundled.contains(&format!("SCHEMA_REVISION: u64 = {};", expected.revision)));
    assert!(bundled.contains(&expected.hash));
    let described: QueryDescription =
        serde_json::from_slice(&std::fs::read(json_output).unwrap()).unwrap();
    assert_eq!(described.schema, expected.into());
}
