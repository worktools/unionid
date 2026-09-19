mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use common::TempDir;
use serde::de::DeserializeOwned;
use unionid::migration::load_directory;
use unionid::{
    BackupInfo, Engine, MigrationApply, MigrationPlan, QueryAccessKind, QueryResponse, Value,
};

struct Scenario {
    name: &'static str,
    storage_target: Option<u32>,
    initial_migration: &'static str,
    initial_script: &'static str,
    restart_query: &'static str,
    upgrade_migration: &'static str,
    final_query: &'static str,
    explain_query: &'static str,
    expected_access: QueryAccessKind,
    expected_index: &'static str,
    assert_before: fn(&QueryResponse),
    assert_after: fn(&QueryResponse),
}

#[test]
fn task_queue_survives_state_transition_migration_and_restore() {
    verify(Scenario {
        name: "task_queue",
        storage_target: None,
        initial_migration: r#"migration m0001_task_queue
  add type TaskState = Queued | Running {worker text, attempt int} | Done {result text}
  add type Task =
    id int
    title text
    state TaskState
  add table tasks Task key id
  add index tasks.state
"#,
        initial_script: r#"insert tasks {id = 1, title = "compile release", state = Queued}
insert tasks {id = 2, title = "publish notes", state = Queued}
update tasks
filter id == 1 and state == Queued
set state = Running {worker = "alice", attempt = 1}
upsert tasks {id = 2, title = "publish notes", state = Done {result = "ready"}}
from tasks
sort id
"#,
        restart_query: "from tasks | filter state == Running {worker = \"alice\", attempt = 1}",
        upgrade_migration: r#"migration m0002_task_priority
  parent m0001_task_queue
  rename variant TaskState.Running to Claimed
  add variant TaskState.Cancelled {reason text}
  change field Task.title to option text
    using old -> Some old
  add field Task.priority int = 0
  add index tasks.priority
"#,
        final_query: "from tasks | select {id, title, state, priority} | sort id",
        explain_query: "explain from tasks | filter priority == 0",
        expected_access: QueryAccessKind::SecondaryIndexLookup,
        expected_index: "tasks.priority",
        assert_before: assert_task_before,
        assert_after: assert_task_after,
    });
}

#[test]
fn nested_config_survives_deep_adt_migration_and_restore() {
    verify(Scenario {
        name: "nested_config",
        storage_target: None,
        initial_migration: r#"migration m0001_nested_config
  add type Retry =
    attempts int
    delay_ms int
  add type Source = Local {path text} | Remote {url text, retry Retry}
  add type Config =
    id text
    source Source
    tags list text
  add table configs Config key id
  add index configs.source
"#,
        initial_script: r#"insert configs {
  id = "prod"
  source = Remote {url = "https://old.example", retry = {attempts = 3, delay_ms = 1000}}
  tags = ["durable", "sync"]
}
update configs
filter id == "prod"
set source = Remote {url = "https://new.example", retry = {attempts: 5, delay_ms = 1000}}
from configs
filter match source
  Remote {url, retry} => url == "https://new.example" and retry.attempts == 5
  Local {..} => false
"#,
        restart_query: r#"from configs
filter source == Remote {
  url = "https://new.example"
  retry = {attempts: 5, delay_ms = 1000}
}
"#,
        upgrade_migration: r#"migration m0002_http_source
  parent m0001_nested_config
  add field Retry.backoff_ms int = 250
  rename variant Source.Remote to Http
  change variant Source.Http to {url text, retry Retry, headers list text}
    using old -> {url = old.url, retry = old.retry, headers: []}
"#,
        final_query: r#"from configs
filter match source
  Http {url, retry, headers} =>
    url == "https://new.example" and retry.backoff_ms == 250 and length headers == 0
  Local {..} => false
select {id, source, tags}
"#,
        explain_query: r#"explain from configs
filter source == Http {
  url = "https://new.example"
  retry = {attempts: 5, delay_ms = 1000, backoff_ms: 250}
  headers: []
}
"#,
        expected_access: QueryAccessKind::SecondaryIndexLookup,
        expected_index: "configs.source",
        assert_before: assert_config_before,
        assert_after: assert_config_after,
    });
}

#[test]
fn session_key_lifecycle_survives_migration_and_restore() {
    verify(Scenario {
        name: "session_lifecycle",
        storage_target: None,
        initial_migration: r#"migration m0001_sessions
  add type SessionState = Active | Expired {reason text}
  add type Session =
    id text
    user text
    state SessionState
    tags list text
  add table sessions Session key id
  add index sessions.state
"#,
        initial_script: r#"insert sessions {id = "s1", user = "alice", state = Active, tags = ["web"]}
insert sessions {id = "s2", user = "bob", state = Active, tags = ["api"]}
upsert sessions {id = "s1", user = "alice-v2", state = Active, tags = ["web", "refreshed"]}
delete sessions | filter id == "s2"
from sessions | filter id == "s1"
"#,
        restart_query: "from sessions | sort id",
        upgrade_migration: r#"migration m0002_session_generation
  parent m0001_sessions
  rename variant SessionState.Active to Ready
  add field Session.generation int = 1
  add index sessions.generation
"#,
        final_query: "from sessions | filter id == \"s1\" | select {id, user, state, tags, generation}",
        explain_query: "explain from sessions | filter generation == 1",
        expected_access: QueryAccessKind::SecondaryIndexLookup,
        expected_index: "sessions.generation",
        assert_before: assert_session_before,
        assert_after: assert_session_after,
    });
}

#[test]
fn typed_metadata_survives_mutation_migration_restart_check_and_restore() {
    verify(Scenario {
        name: "typed_metadata",
        storage_target: None,
        initial_migration: r#"migration m0001_typed_metadata
  add type Attribute = Text(text) | Number(int) | Enabled(bool)
  add type Account =
    id text
    attributes Map<text, Attribute>
    legacy_plan Map<text, Attribute>
  add table accounts Account key id
  add unique index accounts.attributes
"#,
        initial_script: r#"insert accounts {
  id = "alice"
  attributes = map {
    "plan": Text("pro")
    "region": Text("eu")
    "seats": Number(5)
  }
  legacy_plan = map {"plan": Text("pro")}
}
upsert accounts {
  id = "bob"
  attributes = map {"plan": Text("free")}
  legacy_plan = map {}
}
update accounts
filter id == "alice"
set attributes = map {
  "enabled": Enabled(true)
  "plan": Text("pro")
  "region": Text("eu")
  "seats": Number(6)
}
from accounts
sort id
derive {
  plan = get attributes "plan"
  attribute_keys = keys attributes
  attribute_count = length attributes
}"#,
        restart_query: r#"from accounts
filter contains_key attributes "plan"
sort id
derive {
  plan = get attributes "plan"
  attribute_keys = keys attributes
  attribute_count = length attributes
}"#,
        upgrade_migration: r#"migration m0002_normalize_metadata
  parent m0001_typed_metadata
  rename field Account.attributes to metadata
  change field Account.legacy_plan to Option<Attribute>
    using old -> get old "plan"
  add field Account.provider_metadata: Map<text, Attribute> = map {}
"#,
        final_query: r#"from accounts
filter contains_key metadata "plan"
sort id
derive {
  plan = get metadata "plan"
  metadata_entries = entries metadata
  metadata_count = length metadata
}
select {id, metadata, legacy_plan, provider_metadata, plan, metadata_entries, metadata_count}"#,
        explain_query: r#"explain from accounts
filter metadata == map {
  "enabled": Enabled(true)
  "plan": Text("pro")
  "region": Text("eu")
  "seats": Number(6)
}"#,
        expected_access: QueryAccessKind::SecondaryIndexLookup,
        expected_index: "accounts.metadata",
        assert_before: assert_metadata_before,
        assert_after: assert_metadata_after,
    });
}

#[test]
fn soft_deleted_email_survives_transition_migration_and_restore() {
    verify(Scenario {
        name: "partial_email",
        storage_target: None,
        initial_migration: r#"migration m0001_partial_email
  add type Account =
    id text
    email text
    deleted_at Option<text>
  add table accounts Account key id
  add unique index accounts (email) if deleted_at == None
"#,
        initial_script: r#"insert accounts {id = "alice", email = "shared@example.com", deleted_at = None}
insert accounts {id = "bob", email = "shared@example.com", deleted_at = Some "2026-01-01"}
update accounts
filter id == "bob"
set email = "bob@example.com"
update accounts
filter id == "alice"
set deleted_at = Some "2026-02-01"
upsert accounts {id = "dave", email = "shared@example.com", deleted_at = None}
from accounts
sort id
"#,
        restart_query: "from accounts | sort id",
        upgrade_migration: r#"migration m0002_partial_email_note
  parent m0001_partial_email
  add field Account.note text = ""
  add index accounts (note)
"#,
        final_query: "from accounts | select {id, email, deleted_at, note} | sort id",
        explain_query: "explain from accounts | filter deleted_at == None | filter email == \"shared@example.com\"",
        expected_access: QueryAccessKind::SecondaryIndexLookup,
        expected_index: "accounts.email",
        assert_before: assert_partial_email_before,
        assert_after: assert_partial_email_after,
    });
}

#[test]
fn active_external_id_survives_state_transitions_and_restore() {
    verify(Scenario {
        name: "active_external_id",
        storage_target: None,
        initial_migration: r#"migration m0001_active_external_id
  add type JobState = Pending | Active
  add type Job =
    id int
    provider text
    external_id text
    state JobState
  add table jobs Job key id
  add unique index jobs (provider, external_id) if state == Active
"#,
        initial_script: r#"insert jobs {id = 1, provider = "acme", external_id = "42", state = Active}
insert jobs {id = 2, provider = "acme", external_id = "42", state = Pending}
insert jobs {id = 3, provider = "other", external_id = "42", state = Active}
update jobs
filter id == 1
set state = Pending
update jobs
filter id == 2
set state = Active
upsert jobs {id = 3, provider = "other", external_id = "43", state = Active}
from jobs
sort id
"#,
        restart_query: "from jobs | sort id",
        upgrade_migration: r#"migration m0002_active_external_id_owner
  parent m0001_active_external_id
  add field Job.owner Option<text> = None
  add index jobs.owner
"#,
        final_query: "from jobs | select {id, provider, external_id, state, owner} | sort id",
        explain_query: "explain from jobs | filter state == Active | filter provider == \"acme\" | filter external_id == \"42\"",
        expected_access: QueryAccessKind::CompositeLookup,
        expected_index: "jobs (provider, external_id)",
        assert_before: assert_active_job_before,
        assert_after: assert_active_job_after,
    });
}

fn verify(scenario: Scenario) {
    let dir = TempDir::new();
    let database = dir.0.join(format!("{}.redb", scenario.name));
    let restored = dir.0.join(format!("{}-restored.redb", scenario.name));
    let archive = dir.0.join(format!("{}.backup.json", scenario.name));
    let migrations = dir.0.join("migrations");
    std::fs::create_dir(&migrations).unwrap();
    write_migration(&migrations, "0001_initial.unid", scenario.initial_migration);

    if let Some(target) = scenario.storage_target {
        let target = target.to_string();
        let upgraded: serde_json::Value = run_json(
            scenario.name,
            &[
                "upgrade",
                "--db",
                path(&database),
                "--target",
                &target,
                "--format",
                "json",
            ],
        );
        assert_eq!(upgraded["format"], scenario.storage_target.unwrap());
        assert_eq!(
            upgraded["changed"], true,
            "{} storage upgrade",
            scenario.name
        );
    }

    let initial: MigrationApply = run_json(
        scenario.name,
        &[
            "migration",
            "apply",
            "--db",
            path(&database),
            "--dir",
            path(&migrations),
            "--format",
            "json",
        ],
    );
    assert_eq!(initial.applied.len(), 1, "{} initial apply", scenario.name);
    assert_eq!(
        initial.schema.revision, 1,
        "{} initial revision",
        scenario.name
    );

    let before = run_query(scenario.name, &database, scenario.initial_script);
    (scenario.assert_before)(&before);
    let restarted = run_query(scenario.name, &database, scenario.restart_query);
    (scenario.assert_before)(&restarted);

    write_migration(&migrations, "0002_upgrade.unid", scenario.upgrade_migration);
    let plan: MigrationPlan = run_json(
        scenario.name,
        &[
            "migration",
            "plan",
            "--db",
            path(&database),
            "--dir",
            path(&migrations),
            "--format",
            "json",
        ],
    );
    assert_eq!(plan.applied_count, 1, "{} applied plan", scenario.name);
    assert_eq!(plan.pending.len(), 1, "{} pending plan", scenario.name);
    assert_eq!(
        plan.current_schema.revision, 1,
        "{} plan source",
        scenario.name
    );
    assert_eq!(
        plan.target_schema.revision, 2,
        "{} plan target",
        scenario.name
    );

    let rehearsal = dir.0.join(format!("{}-rehearsal.redb", scenario.name));
    let rehearsed: serde_json::Value = run_json(
        scenario.name,
        &[
            "migration",
            "rehearse",
            "--db",
            path(&database),
            "--dir",
            path(&migrations),
            "--copy",
            path(&rehearsal),
            "--format",
            "json",
        ],
    );
    assert_eq!(rehearsed["checked"], true, "{} rehearsal", scenario.name);
    assert_eq!(
        rehearsed["source_schema"]["revision"], 1,
        "{} rehearsal source",
        scenario.name
    );
    assert_eq!(
        rehearsed["schema"]["revision"], 2,
        "{} rehearsal target",
        scenario.name
    );

    let applied: MigrationApply = run_json(
        scenario.name,
        &[
            "migration",
            "apply",
            "--db",
            path(&database),
            "--dir",
            path(&migrations),
            "--format",
            "json",
        ],
    );
    assert_eq!(applied.applied.len(), 1, "{} upgrade apply", scenario.name);
    assert_eq!(
        applied.schema, plan.target_schema,
        "{} planned hash",
        scenario.name
    );

    let after = run_query(scenario.name, &database, scenario.final_query);
    (scenario.assert_after)(&after);
    let integrity: serde_json::Value = run_json(
        scenario.name,
        &["check", "--db", path(&database), "--format", "json"],
    );
    assert_eq!(
        integrity["schema"]["revision"], applied.schema.revision,
        "{} checked revision",
        scenario.name
    );
    assert_eq!(
        integrity["schema"]["hash"], applied.schema.hash,
        "{} checked hash",
        scenario.name
    );

    let created: BackupInfo = run_json(
        scenario.name,
        &[
            "backup",
            "--db",
            path(&database),
            "--output",
            path(&archive),
            "--format",
            "json",
        ],
    );
    let recovered: BackupInfo = run_json(
        scenario.name,
        &[
            "restore",
            "--backup",
            path(&archive),
            "--db",
            path(&restored),
            "--format",
            "json",
        ],
    );
    assert_eq!(created, recovered, "{} backup metadata", scenario.name);
    assert_eq!(
        created.schema, applied.schema,
        "{} backup schema",
        scenario.name
    );
    assert_eq!(
        created.migration_count, 2,
        "{} backup ledger",
        scenario.name
    );

    let files = load_directory(&migrations).unwrap();
    let mut source = Engine::open_redb(&database).unwrap();
    let mut copy = Engine::open_redb(&restored).unwrap();
    assert_eq!(
        source.schema_info(),
        copy.schema_info(),
        "{} schema",
        scenario.name
    );
    assert_eq!(
        source.schema(),
        copy.schema(),
        "{} normalized schema",
        scenario.name
    );
    assert_eq!(
        source.migration_status(&files).unwrap(),
        copy.migration_status(&files).unwrap(),
        "{} migration ledger",
        scenario.name
    );
    assert_eq!(
        source.check_integrity().unwrap().schema,
        copy.check_integrity().unwrap().schema,
        "{} integrity schema",
        scenario.name
    );

    let source_rows = ok(&mut source, scenario.final_query, scenario.name);
    let restored_rows = ok(&mut copy, scenario.final_query, scenario.name);
    (scenario.assert_after)(&source_rows);
    (scenario.assert_after)(&restored_rows);
    assert_eq!(
        row_snapshot(&after),
        row_snapshot(&source_rows),
        "{} post-migration restart",
        scenario.name
    );
    assert_eq!(
        row_snapshot(&source_rows),
        row_snapshot(&restored_rows),
        "{} restored typed rows",
        scenario.name
    );

    let source_plan = ok(&mut source, scenario.explain_query, scenario.name)
        .plan
        .unwrap();
    let restored_plan = ok(&mut copy, scenario.explain_query, scenario.name)
        .plan
        .unwrap();
    assert_eq!(source_plan.access.kind, scenario.expected_access);
    assert_eq!(
        source_plan.access.index.as_deref(),
        Some(scenario.expected_index)
    );
    assert_eq!(source_plan.access, restored_plan.access);
    assert_eq!(source_plan.stages, restored_plan.stages);
}

fn run_query(stage: &str, database: &Path, source: &str) -> QueryResponse {
    let response: QueryResponse = run_json(
        stage,
        &[
            "run",
            "--db",
            path(database),
            "--query",
            source,
            "--format",
            "json",
        ],
    );
    assert!(response.ok, "{stage}: {}", response.message);
    response
}

fn run_json<T: DeserializeOwned>(stage: &str, arguments: &[&str]) -> T {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{stage}: unionid {} failed\nstdout: {}\nstderr: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{stage}: decode unionid {} JSON: {error}\n{}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn write_migration(directory: &Path, name: &str, source: &str) {
    std::fs::write(directory.join(name), source).unwrap();
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn ok(engine: &mut Engine, source: &str, scenario: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{scenario}: {}\n{source}", response.message);
    response
}

fn row_snapshot(response: &QueryResponse) -> Vec<Vec<(String, String)>> {
    response
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|(name, value)| (name.clone(), value.source_text()))
                .collect()
        })
        .collect()
}

fn only_row(response: &QueryResponse) -> &BTreeMap<String, Value> {
    assert_eq!(response.rows.len(), 1, "{}", response.message);
    &response.rows[0]
}

fn assert_text(row: &BTreeMap<String, Value>, field: &str, expected: &str) {
    assert!(
        row[field].cmp_eq(&Value::Text(expected.into())),
        "{field}: {}",
        row[field].source_text()
    );
}

fn assert_int(row: &BTreeMap<String, Value>, field: &str, expected: i64) {
    assert!(
        row[field].cmp_eq(&Value::Int(expected)),
        "{field}: {}",
        row[field].source_text()
    );
}

fn assert_task_before(response: &QueryResponse) {
    if response.rows.len() == 2 {
        assert_int(&response.rows[0], "id", 1);
        assert_int(&response.rows[1], "id", 2);
    } else {
        assert_int(only_row(response), "id", 1);
    }
    assert!(
        response.rows[0]["state"]
            .source_text()
            .starts_with("Running")
    );
}

fn assert_task_after(response: &QueryResponse) {
    assert_eq!(response.rows.len(), 2);
    assert_int(&response.rows[0], "id", 1);
    assert_int(&response.rows[0], "priority", 0);
    assert!(
        response.rows[0]["state"]
            .source_text()
            .starts_with("Claimed")
    );
    assert!(
        response.rows[0]["title"].cmp_eq(&Value::Option(Some(Box::new(Value::Text(
            "compile release".into()
        )))))
    );
    assert!(
        response.rows[1]["title"].cmp_eq(&Value::Option(Some(Box::new(Value::Text(
            "publish notes".into()
        )))))
    );
    assert!(response.rows[1]["state"].source_text().starts_with("Done"));
}

fn assert_config_before(response: &QueryResponse) {
    let row = only_row(response);
    assert_text(row, "id", "prod");
    let source = row["source"].source_text();
    assert!(source.starts_with("Remote"), "{source}");
    assert!(source.contains("attempts: 5"), "{source}");
}

fn assert_config_after(response: &QueryResponse) {
    let row = only_row(response);
    assert_text(row, "id", "prod");
    let source = row["source"].source_text();
    assert!(source.starts_with("Http"), "{source}");
    assert!(source.contains("backoff_ms: 250"), "{source}");
    assert!(source.contains("headers: []"), "{source}");
}

fn assert_session_before(response: &QueryResponse) {
    let row = only_row(response);
    assert_text(row, "id", "s1");
    assert_text(row, "user", "alice-v2");
    assert!(row["state"].source_text().starts_with("Active"));
    assert!(row["tags"].source_text().contains("refreshed"));
}

fn assert_session_after(response: &QueryResponse) {
    let row = only_row(response);
    assert_text(row, "id", "s1");
    assert_text(row, "user", "alice-v2");
    assert_int(row, "generation", 1);
    assert!(row["state"].source_text().starts_with("Ready"));
    assert!(row["tags"].source_text().contains("refreshed"));
}

fn assert_metadata_before(response: &QueryResponse) {
    assert_eq!(response.rows.len(), 2);
    let alice = &response.rows[0];
    assert_text(alice, "id", "alice");
    assert_int(alice, "attribute_count", 4);
    assert_eq!(alice["plan"].source_text(), "Some(Text(\"pro\"))");
    assert_eq!(
        alice["attribute_keys"].source_text(),
        r#"["enabled", "plan", "region", "seats"]"#
    );
    let attributes = alice["attributes"].source_text();
    assert!(attributes.contains(r#""seats": Number(6)"#), "{attributes}");

    let bob = &response.rows[1];
    assert_text(bob, "id", "bob");
    assert_int(bob, "attribute_count", 1);
    assert_eq!(bob["plan"].source_text(), "Some(Text(\"free\"))");
    assert_eq!(bob["attribute_keys"].source_text(), r#"["plan"]"#);
    assert_eq!(bob["legacy_plan"].source_text(), "map {}");
}

fn assert_metadata_after(response: &QueryResponse) {
    assert_eq!(response.rows.len(), 2);
    let alice = &response.rows[0];
    assert_text(alice, "id", "alice");
    assert_int(alice, "metadata_count", 4);
    assert_eq!(alice["plan"].source_text(), "Some(Text(\"pro\"))");
    assert_eq!(alice["legacy_plan"].source_text(), "Some(Text(\"pro\"))");
    assert_eq!(alice["provider_metadata"].source_text(), "map {}");
    let metadata = alice["metadata"].source_text();
    assert!(metadata.contains(r#""seats": Number(6)"#), "{metadata}");
    let entries = alice["metadata_entries"].source_text();
    assert!(
        entries.starts_with(r#"[("enabled", Enabled(true))"#),
        "{entries}"
    );
    assert!(entries.ends_with(r#"("seats", Number(6))]"#), "{entries}");

    let bob = &response.rows[1];
    assert_text(bob, "id", "bob");
    assert_int(bob, "metadata_count", 1);
    assert_eq!(bob["plan"].source_text(), "Some(Text(\"free\"))");
    assert_eq!(bob["legacy_plan"].source_text(), "None");
    assert_eq!(bob["provider_metadata"].source_text(), "map {}");
    assert_eq!(
        bob["metadata_entries"].source_text(),
        r#"[("plan", Text("free"))]"#
    );
}

fn assert_partial_email_before(response: &QueryResponse) {
    assert_eq!(response.rows.len(), 3);
    assert_text(&response.rows[0], "id", "alice");
    assert_text(&response.rows[0], "email", "shared@example.com");
    assert_eq!(
        response.rows[0]["deleted_at"].source_text(),
        r#"Some("2026-02-01")"#
    );
    assert_text(&response.rows[1], "id", "bob");
    assert_text(&response.rows[1], "email", "bob@example.com");
    assert_text(&response.rows[2], "id", "dave");
    assert_text(&response.rows[2], "email", "shared@example.com");
    assert_eq!(response.rows[2]["deleted_at"].source_text(), "None");
}

fn assert_partial_email_after(response: &QueryResponse) {
    assert_partial_email_before(response);
    for row in &response.rows {
        assert_text(row, "note", "");
    }
}

fn assert_active_job_before(response: &QueryResponse) {
    assert_eq!(response.rows.len(), 3);
    assert_int(&response.rows[0], "id", 1);
    assert_text(&response.rows[0], "provider", "acme");
    assert_text(&response.rows[0], "external_id", "42");
    assert_eq!(response.rows[0]["state"].source_text(), "Pending");
    assert_int(&response.rows[1], "id", 2);
    assert_text(&response.rows[1], "provider", "acme");
    assert_text(&response.rows[1], "external_id", "42");
    assert_eq!(response.rows[1]["state"].source_text(), "Active");
    assert_int(&response.rows[2], "id", 3);
    assert_text(&response.rows[2], "provider", "other");
    assert_text(&response.rows[2], "external_id", "43");
    assert_eq!(response.rows[2]["state"].source_text(), "Active");
}

fn assert_active_job_after(response: &QueryResponse) {
    assert_active_job_before(response);
    for row in &response.rows {
        assert_eq!(row["owner"].source_text(), "None");
    }
}
