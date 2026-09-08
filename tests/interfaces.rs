mod common;
use common::{Server, TempDir, wait};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use unionid::{
    Engine, IdempotencyPruneOptions, IntrospectionKind, MigrationApply, MigrationFile,
    MigrationPlan, MigrationStatus, ProtocolRequest, QueryResponse, ReceiptOperationResult,
    SchemaCheck, StorageMode, UpsertAction, Value, WireValue, backup, cli,
};

#[test]
fn local_cli_executes_file_and_reports_errors_with_nonzero_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--file", "examples/tasks.uid", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.rows.len(), 1);
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--query", "from absent", "--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.error.unwrap().code, "E_TABLE");
}

#[test]
fn receipt_cli_previews_before_explicit_bounded_pruning() {
    let dir = TempDir::new();
    let path = dir.0.join("receipt-cli.redb");
    let sequence;
    {
        let mut engine = Engine::open_redb(&path).unwrap();
        sequence = engine
            .execute_idempotent_with_params(
                "create-table",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "create table entries (id int)",
                BTreeMap::new(),
                None,
            )
            .unwrap()
            .committed_sequence;
    }
    let status = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "receipts",
            "status",
            "--db",
            path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: unionid::IdempotencyStatus = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status.count, 1);

    let prune = |confirm: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_unionid"));
        command.args([
            "receipts",
            "prune",
            "--db",
            path.to_str().unwrap(),
            "--through-sequence",
            &sequence.to_string(),
            "--max-receipts",
            "1",
            "--format",
            "json",
        ]);
        if confirm {
            command.arg("--confirm");
        }
        command.output().unwrap()
    };
    let preview = prune(false);
    assert!(preview.status.success());
    let preview: unionid::IdempotencyPruneResult = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview.selected_count, 1);
    assert!(!preview.applied);

    let applied = prune(true);
    assert!(applied.status.success());
    let applied: unionid::IdempotencyPruneResult = serde_json::from_slice(&applied.stdout).unwrap();
    assert!(applied.applied);
    assert_eq!(applied.remaining_count, 0);

    let invalid = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["receipts", "prune", "--db", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("E_IDEMPOTENCY_PRUNE"));
}

#[test]
fn fmt_cli_formats_file_and_stdin_and_checks_canonical_input() {
    let dir = TempDir::new();
    let source_path = dir.0.join("input.uid");
    std::fs::write(
        &source_path,
        "type Task={id int,title text}\nfrom tasks|take 1",
    )
    .unwrap();

    let formatted = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["fmt", "--file", source_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        formatted.status.success(),
        "{}",
        String::from_utf8_lossy(&formatted.stderr)
    );
    let canonical = String::from_utf8(formatted.stdout).unwrap();
    assert_eq!(
        canonical,
        "type Task = {\n  id int,\n  title text,\n}\n\nfrom tasks\ntake 1\n"
    );

    let mut stdin = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["fmt"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    stdin
        .stdin
        .take()
        .unwrap()
        .write_all(b"from tasks|select id,title")
        .unwrap();
    let stdin = stdin.wait_with_output().unwrap();
    assert!(stdin.status.success());
    assert_eq!(
        String::from_utf8(stdin.stdout).unwrap(),
        "from tasks\nselect {id, title}\n"
    );

    let noncanonical = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["fmt", "--file", source_path.to_str().unwrap(), "--check"])
        .output()
        .unwrap();
    assert!(!noncanonical.status.success());
    assert!(String::from_utf8_lossy(&noncanonical.stderr).contains("not canonically formatted"));

    std::fs::write(&source_path, &canonical).unwrap();
    assert!(
        Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["fmt", "--file", source_path.to_str().unwrap(), "--check"])
            .status()
            .unwrap()
            .success()
    );

    std::fs::write(&source_path, "from tasks | unknown\n").unwrap();
    let invalid = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["fmt", "--file", source_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    let error = String::from_utf8_lossy(&invalid.stderr);
    assert!(error.contains("E_SYNTAX"), "{error}");
    assert!(error.contains("line 1, column"), "{error}");
}

#[test]
fn migration_cli_creates_plans_applies_and_reports_status() {
    let dir = TempDir::new();
    let migrations = dir.0.join("migrations");
    let database = dir.0.join("state.redb");
    let first = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "new",
            "Initial Tasks",
            "--dir",
            migrations.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_path = migrations.join("0001_initial_tasks.uid");
    std::fs::write(
        &first_path,
        "migration m0001_initial_tasks\n  add type Task =\n    id int\n    title text\n  add table tasks Task key id\n",
    )
    .unwrap();
    let second = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "new",
            "Task priority",
            "--dir",
            migrations.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_path = migrations.join("0002_task_priority.uid");
    let generated = std::fs::read_to_string(&second_path).unwrap();
    assert!(generated.contains("parent m0001_initial_tasks"));
    std::fs::write(
        &second_path,
        "migration m0002_task_priority\n  parent m0001_initial_tasks\n  add field Task.priority int = 0\n",
    )
    .unwrap();

    let initial_plan = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "plan",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        initial_plan.status.success(),
        "{}",
        String::from_utf8_lossy(&initial_plan.stderr)
    );
    let initial_plan: MigrationPlan = serde_json::from_slice(&initial_plan.stdout).unwrap();
    assert_eq!(initial_plan.pending.len(), 2);
    assert!(!database.exists(), "plan must not create the database file");

    let apply = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "apply",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );
    let applied: MigrationApply = serde_json::from_slice(&apply.stdout).unwrap();
    assert_eq!(applied.applied.len(), 2);
    assert_eq!(applied.schema.revision, 2);

    let plan = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "plan",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan: MigrationPlan = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(plan.applied_count, 2);
    assert!(plan.pending.is_empty());

    let status = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "status",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: MigrationStatus = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status.applied.len(), 2);
    assert!(status.pending.is_empty());

    std::fs::write(
        first_path,
        "migration m0001_initial_tasks\n  add type Task =\n    id int\n    title text\n  add table tasks Task key id\n# checksum drift\n",
    )
    .unwrap();
    let changed = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "plan",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("was changed"));
}

#[test]
fn schema_cli_checks_diffs_and_prints_the_applied_target() {
    let dir = TempDir::new();
    let schema = dir.0.join("schema.uid");
    let migrations = dir.0.join("migrations");
    let database = dir.0.join("state.redb");
    std::fs::write(
        &schema,
        "type State = Pending | Complete\ntype Task =\n  id int\n  state State\ntable tasks Task\n  key id\ncreate index tasks (state)\n",
    )
    .unwrap();

    let checked = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "schema",
            "check",
            "--file",
            schema.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let checked: SchemaCheck = serde_json::from_slice(&checked.stdout).unwrap();

    let diff = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "diff",
            "--db",
            database.to_str().unwrap(),
            "--schema",
            schema.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
            "--name",
            "initial",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        diff.status.success(),
        "{}",
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff: serde_json::Value = serde_json::from_slice(&diff.stdout).unwrap();
    assert_eq!(diff["diff"]["runnable"], true);
    assert!(!database.exists(), "diff must not create the live database");

    let apply = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "migration",
            "apply",
            "--db",
            database.to_str().unwrap(),
            "--dir",
            migrations.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        apply.status.success(),
        "{}",
        String::from_utf8_lossy(&apply.stderr)
    );

    let printed = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "schema",
            "print",
            "--db",
            database.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        printed.status.success(),
        "{}",
        String::from_utf8_lossy(&printed.stderr)
    );
    let printed: SchemaCheck = serde_json::from_slice(&printed.stdout).unwrap();
    assert_eq!(printed.normalized, checked.normalized);
}

#[test]
fn local_cli_reports_affected_rows_for_mutations() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--query",
            "create table tasks (id int, attempts int)\ninsert tasks {id: 1, attempts: 2}\nupdate tasks\nfilter id == 1\nset attempts = attempts + 1\nreturning id, attempts",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.affected_rows, Some(1));
    assert_eq!(response.rows.len(), 1);
    assert!(response.rows[0]["attempts"].cmp_eq(&Value::Int(3)));
}

#[test]
fn local_cli_reports_the_structured_upsert_action() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--query",
            "type Item =\n  id int\ntable items Item\n  key id\nupsert items {id = 1}",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.affected_rows, Some(1));
    assert_eq!(response.upsert_action, Some(UpsertAction::Inserted));
}

#[test]
fn local_cli_reports_bulk_upsert_actions_in_input_order() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--query",
            "type Item =\n  id int\n  value text\ntable items Item\n  key id\ninsert items {id = 1, value = \"old\"}\nupsert many items [{id = 1, value = \"new\"}, {id = 2, value = \"second\"}]\nreturning id, value",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.affected_rows, Some(2));
    assert_eq!(
        response.upsert_actions,
        [UpsertAction::Updated, UpsertAction::Inserted]
    );
    assert!(response.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(response.rows[1]["id"].cmp_eq(&Value::Int(2)));
}

#[test]
fn cli_eof_exits_and_query_does_not_wait_for_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--memory"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait(&mut child);
    assert!(child.wait().unwrap().success());
    let mut child = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--query", "create table t (id int)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait(&mut child);
    assert!(child.wait().unwrap().success());
}

#[test]
fn cli_help_uses_current_structured_examples_that_execute() {
    let help = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for expected in [
        "unionid cli --memory",
        "unionid cli --db app.redb",
        "from tasks | take 10",
        ".schema  .tables  .types  .storage  .help  .quit",
        "--history <PATH>",
        "--no-history",
    ] {
        assert!(help.contains(expected), "missing help text: {expected}");
    }

    let memory = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--memory", "--query", "create table items (id int)"])
        .output()
        .unwrap();
    assert!(
        memory.status.success(),
        "{}",
        String::from_utf8_lossy(&memory.stderr)
    );

    let dir = TempDir::new();
    let database = dir.0.join("help-example.redb");
    let redb = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "cli",
            "--db",
            database.to_str().unwrap(),
            "--query",
            "create table items (id int)",
        ])
        .output()
        .unwrap();
    assert!(
        redb.status.success(),
        "{}",
        String::from_utf8_lossy(&redb.stderr)
    );

    let server = Server::start(&[]);
    assert!(
        cli::send_one(
            &server.addr,
            "type Task = {\n  id int,\n}\ntable tasks Task\n  key id"
        )
        .unwrap()
        .ok
    );
    let remote = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "cli",
            "--addr",
            &server.addr,
            "--query",
            "from tasks | take 10",
        ])
        .output()
        .unwrap();
    assert!(
        remote.status.success(),
        "{}",
        String::from_utf8_lossy(&remote.stderr)
    );
}

#[test]
fn multiline_stdin_is_a_single_atomic_batch() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--memory", "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("../examples/tasks.uid"))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn tcp_and_embedded_engine_have_identical_typed_results() {
    let server = Server::start(&[]);
    let script = include_str!("../examples/tasks.uid");
    let remote = cli::send_one(&server.addr, script).unwrap();
    let local = Engine::memory().execute(script);
    assert!(remote.ok, "{}", remote.message);
    assert_eq!(
        serde_json::to_value(remote).unwrap(),
        serde_json::to_value(local).unwrap()
    );
    let error = cli::send_one(&server.addr, "from tasks | select typo").unwrap();
    assert!(!error.ok);
}

#[test]
fn legacy_line_protocol_and_cli_work_against_the_same_engine() {
    let server = Server::start(&[]);
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream
        .write_all(b"create table users (id int, name text)\n")
        .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(serde_json::from_str::<QueryResponse>(&response).unwrap().ok);
    for query in [
        "insert users {id:1,name:\"alice\"}",
        "from users | filter id = 1 | select name",
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["cli", "--addr", &server.addr, "--query", query])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let result = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--addr", &server.addr, "--query", "from absent"])
        .output()
        .unwrap();
    assert!(!result.status.success());
}

#[test]
fn server_restart_recovers_typed_data_and_index() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("db.snapshot");
    let args = [
        "--wal-path",
        wal.to_str().unwrap(),
        "--snapshot-path",
        snapshot.to_str().unwrap(),
        "--snapshot-every",
        "1",
    ];
    {
        let server = Server::start(&args);
        let response = cli::send_one(&server.addr, include_str!("../examples/tasks.uid")).unwrap();
        assert!(response.ok);
    }
    let server = Server::start(&args);
    let response = cli::send_one(&server.addr, "from tasks | filter id == 1").unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn local_run_and_server_share_the_redb_database() {
    let dir = TempDir::new();
    let path = dir.0.join("state.redb");
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--db",
            path.to_str().unwrap(),
            "--query",
            "type Entry =\n  id int\n  value option text\ntable entries Entry\n  key id\nupsert entries {id = 1, value = Some \"saved\"}",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inserted: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(inserted.upsert_action, Some(UpsertAction::Inserted));
    let server = Server::start(&["--db", path.to_str().unwrap()]);
    let blocked = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--db",
            path.to_str().unwrap(),
            "--query",
            "from entries",
        ])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("E_BUSY"));
    let updated = cli::send_one(
        &server.addr,
        "upsert entries {id = 1, value = Some \"replaced\"}",
    )
    .unwrap();
    assert!(updated.ok, "{}", updated.message);
    assert_eq!(updated.upsert_action, Some(UpsertAction::Updated));
    let response = cli::send_one(&server.addr, "from entries | filter id == 1").unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
    assert!(
        response.rows[0]["value"].cmp_eq(&Value::Option(Some(Box::new(Value::Text(
            "replaced".into()
        )))))
    );
    drop(server);
    let check = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["check", "--db", path.to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(report["backend"], "redb");
    assert_eq!(report["backend_clean"], true);
    assert_eq!(report["schema"]["revision"], 1);
}

#[test]
fn concurrent_requests_are_serialized_and_invalid_json_is_reported() {
    let server = Server::start(&[]);
    assert!(
        cli::send_one(&server.addr, "type R =\n  id int\ntable t R\n  key id")
            .unwrap()
            .ok
    );
    let threads = (0..16)
        .map(|id| {
            let addr = server.addr.clone();
            std::thread::spawn(move || {
                assert!(
                    cli::send_one(&addr, &format!("insert t {{id = {id}}}"))
                        .unwrap()
                        .ok
                )
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(
        cli::send_one(&server.addr, "from t").unwrap().rows.len(),
        16
    );
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream.write_all(b"{\"query\":7}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<QueryResponse>(&line)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_PROTOCOL"
    );
}

#[test]
fn versioned_tcp_protocol_echoes_ids_and_binds_lossless_parameters() {
    let server = Server::start(&[]);
    let setup = r#"type Item =
  id int
  note text
table items Item
  key id
insert items
  id = 9007199254740993
  note = "quoted \"text\"\nwith | pipe""#;
    assert!(cli::send_one(&server.addr, setup).unwrap().ok);

    let request = ProtocolRequest {
        version: unionid::protocol::VERSION,
        request_id: "tcp-1".into(),
        query: "from items\nfilter id == $id\nselect {id, note}".into(),
        introspect: None,
        params: BTreeMap::from([(
            "id".into(),
            WireValue::Int {
                value: "9007199254740993".into(),
            },
        )]),
        schema: None,
        idempotency_key: None,
        receipts: None,
        page: None,
    };
    let response = cli::send_request(&server.addr, &request).unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.version, unionid::protocol::VERSION);
    assert_eq!(response.request_id, "tcp-1");
    assert!(matches!(
        response.rows[0].get("id"),
        Some(WireValue::Int { value }) if value == "9007199254740993"
    ));
    assert!(matches!(
        response.rows[0].get("note"),
        Some(WireValue::Text { value }) if value == "quoted \"text\"\nwith | pipe"
    ));

    let unsupported = ProtocolRequest {
        version: 99,
        request_id: "tcp-2".into(),
        ..request
    };
    let response = cli::send_request(&server.addr, &unsupported).unwrap();
    assert_eq!(response.request_id, "tcp-2");
    assert_eq!(response.error.unwrap().code, "E_PROTOCOL_VERSION");

    let missing = ProtocolRequest {
        version: unionid::protocol::VERSION,
        request_id: "tcp-3".into(),
        query: "from items | filter id == $id".into(),
        introspect: None,
        params: BTreeMap::new(),
        schema: None,
        idempotency_key: None,
        receipts: None,
        page: None,
    };
    assert_eq!(
        cli::send_request(&server.addr, &missing)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_PARAM_MISSING"
    );
    let wrong_schema = ProtocolRequest {
        request_id: "tcp-4".into(),
        params: BTreeMap::from([("id".into(), WireValue::Int { value: "1".into() })]),
        schema: Some(unionid::SchemaInfo {
            revision: 0,
            hash: "stale".into(),
        }),
        ..missing
    };
    assert_eq!(
        cli::send_request(&server.addr, &wrong_schema)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_SCHEMA_CHANGED"
    );
}

#[test]
fn versioned_tcp_idempotency_replays_after_restart_and_rejects_conflicts() {
    let dir = TempDir::new();
    let path = dir.0.join("tcp-idempotency.redb");
    let path_arg = path.to_str().unwrap();
    let mut server = Server::start(&["--db", path_arg]);
    assert!(
        cli::send_one(
            &server.addr,
            "type Entry =\n  id int\n  value text\ntable entries Entry\n  key id"
        )
        .unwrap()
        .ok
    );
    let request = ProtocolRequest::query(
        "attempt-1",
        "insert entries {id = 1, value = \"once\"}\nreturning",
    )
    .with_idempotency_key("entry-1")
    .unwrap();
    let first = cli::send_request(&server.addr, &request).unwrap();
    assert!(first.ok, "{}", first.message);
    let first_idempotency = first.idempotency.unwrap();
    assert!(!first_idempotency.replayed);
    assert_eq!(
        first_idempotency.durability,
        unionid::IdempotencyDurability::Durable
    );
    server.shutdown();

    let server = Server::start(&["--db", path_arg]);
    let replay_request = ProtocolRequest {
        request_id: "attempt-2".into(),
        ..request.clone()
    };
    let replay = cli::send_request(&server.addr, &replay_request).unwrap();
    assert!(replay.ok, "{}", replay.message);
    assert_eq!(replay.request_id, "attempt-2");
    let replay_idempotency = replay.idempotency.unwrap();
    assert!(replay_idempotency.replayed);
    assert_eq!(replay_idempotency.digest, first_idempotency.digest);
    assert_eq!(
        replay_idempotency.committed_sequence,
        first_idempotency.committed_sequence
    );

    let conflict = ProtocolRequest::query(
        "attempt-3",
        "insert entries {id = 2, value = \"different\"}",
    )
    .with_idempotency_key("entry-1")
    .unwrap();
    let conflict = cli::send_request(&server.addr, &conflict).unwrap();
    assert_eq!(conflict.error.unwrap().code, "E_IDEMPOTENCY_CONFLICT");
    assert_eq!(
        cli::send_one(&server.addr, "from entries")
            .unwrap()
            .rows
            .len(),
        1
    );
    let keyed_read = ProtocolRequest::query("read", "from entries")
        .with_idempotency_key("read-key")
        .unwrap();
    assert_eq!(
        cli::send_request(&server.addr, &keyed_read)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_IDEMPOTENCY_NOT_MUTATION"
    );
    let keyed_introspection = ProtocolRequest {
        idempotency_key: Some("inspect-key".into()),
        ..ProtocolRequest::introspection("inspect", IntrospectionKind::Storage)
    };
    assert_eq!(
        cli::send_request(&server.addr, &keyed_introspection)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_IDEMPOTENCY_NOT_MUTATION"
    );

    let status = cli::send_request(
        &server.addr,
        &ProtocolRequest::receipt_status("receipt-status"),
    )
    .unwrap();
    let ReceiptOperationResult::Status(status) = status.receipts.unwrap() else {
        panic!("expected receipt status");
    };
    assert_eq!(status.count, 1);
    let options = IdempotencyPruneOptions {
        completed_before_unix_ms: None,
        committed_through_sequence: Some(first_idempotency.committed_sequence.parse().unwrap()),
        max_receipts: 1,
    };
    let preview = cli::send_request(
        &server.addr,
        &ProtocolRequest::receipt_prune("receipt-preview", options.clone(), false),
    )
    .unwrap();
    let ReceiptOperationResult::Prune(preview) = preview.receipts.unwrap() else {
        panic!("expected receipt prune preview");
    };
    assert_eq!(preview.selected_count, 1);
    assert!(!preview.applied);
    let applied = cli::send_request(
        &server.addr,
        &ProtocolRequest::receipt_prune("receipt-apply", options, true),
    )
    .unwrap();
    let ReceiptOperationResult::Prune(applied) = applied.receipts.unwrap() else {
        panic!("expected applied receipt prune");
    };
    assert!(applied.applied);
    assert_eq!(applied.remaining_count, 0);
}

#[test]
fn versioned_tcp_bulk_inserts_typed_row_lists() {
    let server = Server::start(&[]);
    let setup = r#"type State = Pending | Done
type Event =
  id int
  note text = "new"
  state State = Pending
table events Event
  key id
create index events (state)"#;
    assert!(cli::send_one(&server.addr, setup).unwrap().ok);

    let row = |id: &str| WireValue::Record {
        fields: BTreeMap::from([("id".into(), WireValue::Int { value: id.into() })]),
    };
    let request = ProtocolRequest {
        version: unionid::protocol::VERSION,
        request_id: "bulk-insert".into(),
        query: "insert many events $rows\nreturning id, note, state".into(),
        introspect: None,
        params: BTreeMap::from([(
            "rows".into(),
            WireValue::List {
                items: vec![row("2"), row("1")],
            },
        )]),
        schema: None,
        idempotency_key: None,
        receipts: None,
        page: None,
    };
    let response = cli::send_request(&server.addr, &request).unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.request_id, "bulk-insert");
    assert_eq!(response.affected_rows, Some(2));
    assert!(matches!(&response.rows[0]["id"], WireValue::Int { value } if value == "2"));
    assert!(matches!(&response.rows[1]["id"], WireValue::Int { value } if value == "1"));
    assert!(matches!(&response.rows[0]["note"], WireValue::Text { value } if value == "new"));

    let stored = cli::send_one(&server.addr, "from events | sort id").unwrap();
    assert!(stored.ok, "{}", stored.message);
    assert_eq!(stored.rows.len(), 2);
    assert!(stored.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(stored.rows[1]["id"].cmp_eq(&Value::Int(2)));
}

#[test]
fn versioned_tcp_bulk_upserts_typed_row_lists() {
    let server = Server::start(&[]);
    let setup = r#"type Item =
  id int
  value text
table items Item
  key id
insert items {id = 1, value = "old"}"#;
    assert!(cli::send_one(&server.addr, setup).unwrap().ok);

    let row = |id: &str, value: &str| WireValue::Record {
        fields: BTreeMap::from([
            ("id".into(), WireValue::Int { value: id.into() }),
            (
                "value".into(),
                WireValue::Text {
                    value: value.into(),
                },
            ),
        ]),
    };
    let request = ProtocolRequest {
        version: unionid::protocol::VERSION,
        request_id: "bulk-upsert".into(),
        query: "upsert many items $rows\nreturning id, value".into(),
        introspect: None,
        params: BTreeMap::from([(
            "rows".into(),
            WireValue::List {
                items: vec![row("1", "new"), row("2", "second")],
            },
        )]),
        schema: None,
        idempotency_key: None,
        receipts: None,
        page: None,
    };
    let response = cli::send_request(&server.addr, &request).unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.request_id, "bulk-upsert");
    assert_eq!(response.affected_rows, Some(2));
    assert_eq!(
        response.upsert_actions,
        [UpsertAction::Updated, UpsertAction::Inserted]
    );
    assert!(matches!(&response.rows[0]["value"], WireValue::Text { value } if value == "new"));
    assert!(matches!(&response.rows[1]["value"], WireValue::Text { value } if value == "second"));

    let stored = cli::send_one(&server.addr, "from items | sort id").unwrap();
    assert!(stored.ok, "{}", stored.message);
    assert_eq!(stored.rows.len(), 2);
    assert!(stored.rows[0]["value"].cmp_eq(&Value::Text("new".into())));
}

#[test]
fn versioned_tcp_updates_adts_with_match_and_parameters() {
    let server = Server::start(&[]);
    let setup = r#"type State =
  Queued {attempt int}
  | Running {worker text, attempt int}
  | Done

type Job =
  id int
  priority int
  ready bool
  state State

table jobs Job
  key id

insert jobs {id = 1, priority = 1, ready = false, state = Queued {attempt = 2}}
insert jobs {id = 2, priority = 2, ready = false, state = Queued {attempt = 5}}"#;
    assert!(cli::send_one(&server.addr, setup).unwrap().ok);

    let request = ProtocolRequest {
        version: unionid::protocol::VERSION,
        request_id: "match-update".into(),
        query: r#"update jobs
filter id >= $id
sort {-priority, id}
take 1
set state =
  match state
    Queued {attempt} => Running {worker = $worker, attempt = attempt + 1}
    current => current
returning id, state"#
            .into(),
        introspect: None,
        params: BTreeMap::from([
            ("id".into(), WireValue::Int { value: "1".into() }),
            (
                "worker".into(),
                WireValue::Text {
                    value: "tcp-worker".into(),
                },
            ),
        ]),
        schema: None,
        idempotency_key: None,
        receipts: None,
        page: None,
    };
    let response = cli::send_request(&server.addr, &request).unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.request_id, "match-update");
    assert_eq!(response.affected_rows, Some(1));
    assert_eq!(response.columns.len(), 2);
    assert_eq!(response.rows.len(), 1);
    assert!(matches!(response.rows[0]["id"], WireValue::Int { .. }));
    assert!(matches!(response.rows[0]["state"], WireValue::Named { .. }));

    assert!(matches!(response.rows[0]["id"], WireValue::Int { ref value } if value == "2"));

    let rows = cli::send_one(&server.addr, "from jobs | filter id == 2").unwrap();
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(
        rows.rows[0]["state"].source_text(),
        "Running {attempt = 6, worker = \"tcp-worker\"}"
    );
    let untouched = cli::send_one(&server.addr, "from jobs | filter id == 1").unwrap();
    assert_eq!(
        untouched.rows[0]["state"].source_text(),
        "Queued {attempt = 2}"
    );

    let boolean_update = ProtocolRequest {
        version: unionid::protocol::VERSION,
        request_id: "boolean-update".into(),
        query: r#"update jobs
set ready =
  match state
    Queued {attempt} => attempt < $threshold
    Running {attempt, ..} => attempt >= $threshold
    Done => false
returning id, ready"#
            .into(),
        introspect: None,
        params: BTreeMap::from([("threshold".into(), WireValue::Int { value: "5".into() })]),
        schema: None,
        idempotency_key: None,
        receipts: None,
        page: None,
    };
    let response = cli::send_request(&server.addr, &boolean_update).unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.affected_rows, Some(2));
    assert!(
        response
            .rows
            .iter()
            .all(|row| matches!(row["ready"], WireValue::Bool { value: true }))
    );
}

#[test]
fn introspection_is_consistent_across_memory_redb_and_all_tcp_commands() {
    let setup = "type Task =\n  id int\n  title text\ntable tasks Task\n  key id";
    let mut local = Engine::memory();
    assert!(local.execute(setup).ok);
    let expected = local.introspection();

    let memory_server = Server::start(&[]);
    assert!(cli::send_one(&memory_server.addr, setup).unwrap().ok);
    for kind in [
        IntrospectionKind::Schema,
        IntrospectionKind::Tables,
        IntrospectionKind::Types,
        IntrospectionKind::Storage,
    ] {
        assert_eq!(
            cli::send_introspection(&memory_server.addr, kind).unwrap(),
            expected
        );
    }

    let dir = TempDir::new();
    let path = dir.0.join("introspection.redb");
    let redb_server = Server::start(&["--db", path.to_str().unwrap()]);
    assert!(cli::send_one(&redb_server.addr, setup).unwrap().ok);
    let redb = cli::send_introspection(&redb_server.addr, IntrospectionKind::Storage).unwrap();
    assert_eq!(redb.storage, StorageMode::Redb);
    assert_eq!(redb.schema, expected.schema);
    assert_eq!(redb.schema_source, expected.schema_source);
    assert_eq!(redb.tables, expected.tables);
    assert_eq!(redb.types, expected.types);
    assert_eq!(redb.fields, expected.fields);

    let mut invalid = ProtocolRequest::introspection("invalid", IntrospectionKind::Schema);
    invalid.query = "from tasks".into();
    let response = cli::send_request(&memory_server.addr, &invalid).unwrap();
    assert_eq!(response.error.unwrap().code, "E_PROTOCOL");

    let disconnected =
        cli::send_introspection("127.0.0.1:0", IntrospectionKind::Tables).unwrap_err();
    assert!(disconnected.contains("connect 127.0.0.1:0"));
}

#[test]
fn connection_limit_returns_busy_and_recovers_after_a_client_leaves() {
    let server = Server::start(&[]);
    let mut clients = Vec::new();
    for _ in 0..unionid::server::MAX_CONNECTIONS {
        let stream = TcpStream::connect(&server.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        reader.get_mut().write_all(b"from absent\n").unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<QueryResponse>(&line)
                .unwrap()
                .error
                .unwrap()
                .code,
            "E_TABLE"
        );
        // The response confirms admission while the connection stays open.
        clients.push(reader);
    }
    let busy = cli::send_one(&server.addr, "create table rejected (id int)").unwrap();
    assert!(!busy.ok);
    assert_eq!(busy.error.unwrap().code, "E_BUSY");

    // A rejected client need not send anything or close its write half for the
    // server to deliver E_BUSY and finish the bounded rejection path.
    let idle = TcpStream::connect(&server.addr).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut idle = BufReader::new(idle);
    let mut line = String::new();
    idle.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<QueryResponse>(&line)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_BUSY"
    );
    line.clear();
    assert_eq!(idle.read_line(&mut line).unwrap(), 0);

    let mut leaving = clients.pop().unwrap();
    leaving.get_mut().write_all(b"quit\n").unwrap();
    let mut line = String::new();
    leaving.read_line(&mut line).unwrap();
    assert!(serde_json::from_str::<QueryResponse>(&line).unwrap().ok);
    drop(leaving);

    // The worker releases its slot just after sending the quit response.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = cli::send_one(&server.addr, "from rejected").unwrap();
        match response.error.unwrap().code.as_str() {
            "E_TABLE" => break, // Rejected requests never execute.
            "E_BUSY" if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            code => panic!("connection slot did not recover: {code}"),
        }
    }
    let existing = &mut clients[0];
    existing
        .get_mut()
        .write_all(b"create table recovered (id int)\n")
        .unwrap();
    let mut line = String::new();
    existing.read_line(&mut line).unwrap();
    assert!(serde_json::from_str::<QueryResponse>(&line).unwrap().ok);
}

#[test]
fn oversized_and_deep_requests_do_not_block_healthy_clients() {
    let server = Server::start(&[]);
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(&vec![b'x'; unionid::server::MAX_FRAME_BYTES + 1])
        .unwrap();
    stream.write_all(b"\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<QueryResponse>(&line)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_LIMIT"
    );

    let mut value = r#"{"type":"int","value":"1"}"#.to_string();
    for _ in 0..200 {
        value = format!(r#"{{"type":"list","items":[{value}]}}"#);
    }
    let request = format!(
        r#"{{"version":1,"request_id":"deep","query":"from absent | filter id == $id","params":{{"id":{value}}}}}"#
    );
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut reader = BufReader::new(stream);
    line.clear();
    reader.read_line(&mut line).unwrap();
    let response: QueryResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(response.error.unwrap().code, "E_PROTOCOL");

    assert_eq!(
        cli::send_one(&server.addr, "from still_healthy")
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_TABLE"
    );
}

#[test]
fn sigterm_gracefully_closes_idle_clients_and_releases_redb() {
    let temp = TempDir::new();
    let db = temp.0.join("graceful.redb");
    let db_arg = db.to_string_lossy().into_owned();
    let mut server = Server::start(&["--db", &db_arg]);
    assert!(
        cli::send_one(
            &server.addr,
            "create table entries (id int)\ninsert entries {id = 1}"
        )
        .unwrap()
        .ok
    );
    let idle = TcpStream::connect(&server.addr).unwrap();
    server.shutdown();
    drop(idle);

    let mut reopened = Engine::open_redb(&db).unwrap();
    let response = reopened.execute("from entries | filter id == 1");
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn read_only_cli_and_server_allow_observation_without_publishing_writes() {
    let temp = TempDir::new();
    let db = temp.0.join("read-only.redb");
    let before_backup = temp.0.join("before.json");
    let after_backup = temp.0.join("after.json");
    let db_arg = db.to_string_lossy().into_owned();
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        let migration = MigrationFile::parse(
            "migration m0001_initial\n  add type Entry =\n    id int\n    value text\n  add table entries Entry key id\n  add index entries.value\n",
        )
        .unwrap();
        engine.apply_migrations(&[migration]).unwrap();
        assert!(
            engine
                .execute("insert entries {id = 1, value = \"original\"}")
                .ok
        );
    }
    let before = backup::create(&db, &before_backup).unwrap();

    let mut server = Server::start(&["--db", &db_arg, "--read-only"]);
    let read = cli::send_one(&server.addr, "from entries | filter id == 1").unwrap();
    assert!(read.ok, "{}", read.message);
    assert_eq!(read.rows.len(), 1);
    let storage = cli::send_introspection(&server.addr, IntrospectionKind::Storage).unwrap();
    assert_eq!(storage.storage, StorageMode::Redb);
    assert!(storage.read_only);
    let rejected = cli::send_one(
        &server.addr,
        "update entries\nfilter id == 1\nset value = \"changed\"",
    )
    .unwrap();
    assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");
    server.shutdown();

    let read = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--db",
            &db_arg,
            "--read-only",
            "--query",
            "from entries | filter id == 1",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    let read: QueryResponse = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read.rows.len(), 1);

    let rejected = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--db",
            &db_arg,
            "--read-only",
            "--query",
            "delete entries",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let rejected: QueryResponse = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(rejected.error.unwrap().code, "E_READ_ONLY");

    let mut reopened = Engine::open_redb(&db).unwrap();
    let rows = reopened.execute("from entries | sort id");
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(reopened.migration_history().len(), 1);
    let plan = reopened.execute("explain from entries | filter value == \"original\"");
    assert!(plan.ok, "{}", plan.message);
    assert_eq!(
        plan.plan.unwrap().access.kind,
        unionid::QueryAccessKind::SecondaryIndexLookup
    );
    drop(reopened);
    let after = backup::create(&db, &after_backup).unwrap();
    assert_eq!(
        after, before,
        "logical data, RowIds, indexes, and ledger changed"
    );
}

#[test]
fn read_only_mode_requires_an_existing_durable_database() {
    let temp = TempDir::new();
    let missing = temp.0.join("missing.redb");
    let missing_arg = missing.to_string_lossy().into_owned();
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "run",
            "--db",
            &missing_arg,
            "--read-only",
            "--query",
            "from entries",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_CONFIG"));
    assert!(!missing.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--read-only", "--query", "from entries"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--db"));
}

#[test]
fn embedded_server_shutdown_returns_request_statistics() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&shutdown);
    let server = std::thread::spawn(move || {
        unionid::server::serve_until(listener, Engine::memory(), signal).unwrap()
    });
    let response = cli::send_one(&addr, "from absent").unwrap();
    assert_eq!(response.error.unwrap().code, "E_TABLE");
    shutdown.store(true, Ordering::Release);
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted_connections, 1);
    assert_eq!(stats.rejected_connections, 0);
    assert_eq!(stats.requests, 1);
    assert_eq!(stats.failed_requests, 1);
}
