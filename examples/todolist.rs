//! End-to-end todo-list scenario for the durable ADT database.
//!
//! This is intentionally an application-shaped example rather than a minimal
//! CRUD demo. It exercises nested sums/products, a conditional state change,
//! a deep migration, process-boundary reopening, indexed explain, integrity
//! checking, and logical backup/restore.

use std::path::{Path, PathBuf};

use unionid::backup;
use unionid::migration::MigrationFile;
use unionid::{Engine, QueryResponse};

const INITIAL: &str = r#"migration m0001_todolist
  add type Retry =
    attempts int
    delay_ms int
  add type Reminder = Off | On {retry Retry, channel text}
  add type Label = System {name text} | User {name text, color text}
  add type Due = Never | At {unix_ms int} | Window {start int, end int}
  add type Status = Inbox | InProgress {attempt int, device text} | Blocked {reason text} | Done {at int}
  add type Checklist = Empty | Items {entries list text}
  add type Task =
    id int
    title text
    status Status
    reminder Reminder
    labels list Label
    due Due
    checklist Checklist
  add table todos Task key id
  add index todos.status
"#;

const UPGRADE: &str = r#"migration m0002_todolist_upgrade
  parent m0001_todolist
  rename variant Status.InProgress to Claimed
  add field Retry.backoff_ms int = 250
  add field Task.priority int = 1
  add index todos.priority
"#;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("unionid-todolist-{}", std::process::id()))
        });
    std::fs::create_dir_all(&root)?;
    let database = root.join("todos.redb");
    let restored = root.join("todos-restored.redb");
    let archive = root.join("todos.backup.json");
    for path in [&database, &restored, &archive] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }

    let initial = MigrationFile::parse(INITIAL)?;
    let upgrade = MigrationFile::parse(UPGRADE)?;
    {
        let mut engine = Engine::open_redb(&database)?;
        let applied = engine.apply_migrations(std::slice::from_ref(&initial))?;
        assert_eq!(applied.applied, ["m0001_todolist"]);
        let response = engine.execute(
            r#"insert todos {
  id = 1
  title = "write release notes"
  status = Inbox
  reminder = On {retry = {attempts = 2, delay_ms = 1000}, channel = "email"}
  labels = [System {name = "release"}, User {name = "docs", color = "blue"}]
  due = Window {start = 1000, end = 5000}
  checklist = Items {entries = ["draft", "review"]}
}
insert todos {
  id = 2
  title = "publish crate"
  status = InProgress {attempt = 1, device = "laptop"}
  reminder = Off
  labels = [System {name = "release"}]
  due = Never
  checklist = Empty
}
update todos
filter match status
  Inbox => true
  _ => false
set status = InProgress {attempt = 1, device = "worker-1"}
set reminder = On {retry = {attempts = 3, delay_ms = 2000}, channel = "slack"}
returning id, status, reminder
"#,
        );
        require_ok(response)?;

        let before_restart = require_ok(engine.execute(
            "from todos | filter status == InProgress {attempt = 1, device = \"worker-1\"} | select {id, title}",
        ))?;
        assert_eq!(before_restart.rows.len(), 1);
        println!("initial query: {} row", before_restart.rows.len());
    }

    // Reopening proves the row and the nested values crossed an application
    // process boundary (the engine is dropped before this handle is opened).
    let mut engine = Engine::open_redb(&database)?;
    let reopened = require_ok(engine.execute("from todos | sort id"))?;
    assert_eq!(reopened.rows.len(), 2);
    assert!(
        engine
            .migration_status(&[initial.clone(), upgrade.clone()])?
            .pending
            .len()
            == 1
    );
    println!("reopen: {} rows", reopened.rows.len());

    // Plan the complete chain before applying it; this also checks migration
    // parent validation and the target schema revision without changing data.
    let plan = engine.plan_migrations(&[initial.clone(), upgrade.clone()])?;
    assert_eq!(plan.pending.len(), 1);
    assert_eq!(plan.target_schema.revision, 2);
    engine.apply_migrations(&[initial, upgrade])?;

    let final_rows = require_ok(engine.execute(
        r#"from todos
filter match reminder
  On {retry, channel} => retry.backoff_ms == 250 and channel == "slack"
  Off => false
select {id, title, status, reminder, priority}
"#,
    ))?;
    assert_eq!(final_rows.rows.len(), 1);
    let explain = require_ok(engine.execute("explain from todos | filter priority == 1"))?;
    let plan = explain.plan.expect("explain response has a plan");
    assert_eq!(plan.access.index.as_deref(), Some("todos.priority"));
    let integrity = engine.check_integrity()?;
    assert!(integrity.backend_clean);
    let schema = engine.schema_info();
    let history = engine.migration_history().to_vec();
    drop(engine);

    let backup_info = backup::create(&database, &archive)?;
    let restored_info = backup::restore(&archive, &restored)?;
    assert_eq!(backup_info, restored_info);
    let mut recovered = Engine::open_redb(&restored)?;
    assert_eq!(recovered.schema_info(), schema);
    assert_eq!(recovered.migration_history(), history.as_slice());
    assert!(recovered.check_integrity()?.backend_clean);
    let recovered_rows = require_ok(recovered.execute(
        r#"from todos
filter match reminder
  On {retry, channel} => retry.backoff_ms == 250 and channel == "slack"
  Off => false
select {id, title, status, reminder, priority}
"#,
    ))?;
    assert_eq!(
        serde_json::to_value(&recovered_rows.rows)?,
        serde_json::to_value(&final_rows.rows)?,
    );

    println!(
        "todo-list flow passed: schema revision {}, {} migration(s), backup {}",
        schema.revision,
        history.len(),
        display_path(&archive)
    );
    Ok(())
}

fn require_ok(response: QueryResponse) -> Result<QueryResponse, unionid::Error> {
    match response.error {
        Some(error) => Err(error),
        None => Ok(response),
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}
