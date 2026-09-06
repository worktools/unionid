use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::migration::{MigrationApply, MigrationPlan, MigrationStatus, load_directory};
use crate::{
    Engine, Error, InputStatus, ProtocolRequest, ProtocolResponse, QueryAccessKind, QueryResponse,
    QueryStageKind, SchemaCheck, Value, backup, input_status,
};

pub fn run_local(source: Option<String>, json: bool) -> Result<(), String> {
    run_local_engine(Engine::memory(), source, json)
}

pub fn run_local_redb(
    path: impl Into<std::path::PathBuf>,
    source: Option<String>,
    json: bool,
) -> Result<(), String> {
    let engine = Engine::open_redb(path).map_err(|error| error.to_string())?;
    run_local_engine(engine, source, json)
}

pub fn check_redb(path: impl Into<std::path::PathBuf>, json: bool) -> Result<(), String> {
    let mut engine = Engine::open_redb(path).map_err(|error| error.to_string())?;
    let report = engine
        .check_integrity()
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "redb integrity verified ({})\nschema revision {}\nschema hash {}",
            if report.backend_clean {
                "backend was clean"
            } else {
                "backend repair completed"
            },
            report.schema.revision,
            report.schema.hash
        );
    }
    Ok(())
}

pub fn backup_create(db: PathBuf, output: PathBuf, json: bool) -> Result<(), String> {
    let info = backup::create(db, output).map_err(|error| error.to_string())?;
    print_lifecycle(&info, "backup created", json)
}

pub fn backup_restore(backup_path: PathBuf, db: PathBuf, json: bool) -> Result<(), String> {
    let info = backup::restore(backup_path, db).map_err(|error| error.to_string())?;
    print_lifecycle(&info, "backup restored", json)
}

pub fn import_legacy(
    snapshot: Option<PathBuf>,
    wal: Option<PathBuf>,
    db: PathBuf,
    json: bool,
) -> Result<(), String> {
    let info = backup::import_legacy(snapshot, wal, db).map_err(|error| error.to_string())?;
    print_lifecycle(&info, "legacy database imported", json)
}

fn print_lifecycle(info: &crate::BackupInfo, message: &str, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(info).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{message}\nformat {}\nchecksum {}\nschema revision {}\nschema hash {}\n{} migration(s)",
            info.format_version,
            info.checksum,
            info.schema.revision,
            info.schema.hash,
            info.migration_count
        );
    }
    Ok(())
}

pub fn migration_new(directory: impl AsRef<Path>, name: &str) -> Result<PathBuf, String> {
    let directory = directory.as_ref();
    let (path, id, parent) = next_migration(directory, name)?;
    let mut source = format!("migration {id}\n");
    if let Some(parent) = parent {
        source.push_str(&format!("  parent {parent}\n"));
    }
    source.push_str("  # Add one or more schema operations here\n");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create '{}': {error}", path.display()))?;
    file.write_all(source.as_bytes())
        .map_err(|error| format!("write '{}': {error}", path.display()))?;
    Ok(path)
}

pub fn schema_check(path: impl AsRef<Path>, json: bool) -> Result<(), String> {
    let path = path.as_ref();
    let source = read_source(
        std::fs::File::open(path).map_err(|error| format!("open '{}': {error}", path.display()))?,
    )?;
    let checked = Engine::check_schema(&source).map_err(|error| error.to_string())?;
    print_schema_check(&checked, json)
}

pub fn schema_print(db: impl Into<PathBuf>, json: bool) -> Result<(), String> {
    let db = db.into();
    require_existing_database(&db)?;
    let engine = Engine::open_redb(db).map_err(|error| error.to_string())?;
    let checked = SchemaCheck {
        schema: engine.schema_info(),
        normalized: engine.schema(),
    };
    print_schema_check(&checked, json)
}

pub fn migration_diff(
    db: impl Into<PathBuf>,
    schema_path: impl AsRef<Path>,
    directory: impl AsRef<Path>,
    name: &str,
    json: bool,
) -> Result<(), String> {
    let schema_path = schema_path.as_ref();
    let target_source =
        read_source(std::fs::File::open(schema_path).map_err(|error| {
            format!("open target schema '{}': {error}", schema_path.display())
        })?)?;
    let db = db.into();
    let engine = if db.exists() {
        Engine::open_redb(db).map_err(|error| error.to_string())?
    } else {
        Engine::memory()
    };
    let (path, id, parent) = next_migration(directory.as_ref(), name)?;
    let result = engine
        .diff_schema(&target_source, &id, parent.as_deref())
        .map_err(|error| error.to_string())?;
    if result.operations.is_empty() {
        return Err("schema already matches the target; no migration was created".into());
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create '{}': {error}", path.display()))?;
    file.write_all(result.migration_source.as_bytes())
        .map_err(|error| format!("write '{}': {error}", path.display()))?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "path": path,
                "diff": result,
            }))
            .map_err(|error| error.to_string())?
        );
    } else {
        println!("{}", path.display());
        for operation in &result.operations {
            println!(
                "  {}{}{}",
                operation.description,
                if operation.destructive {
                    " [destructive]"
                } else {
                    ""
                },
                if operation.requires_input {
                    " [requires input]"
                } else {
                    ""
                }
            );
        }
        for impact in &result.impacts {
            for table in &impact.tables {
                println!(
                    "  affects {} through {}: {} row(s), {} index(es)",
                    table.table, impact.type_name, table.rows, table.indexes
                );
            }
        }
        for warning in &result.warnings {
            eprintln!("warning: {warning}");
        }
        if !result.runnable {
            eprintln!("draft requires explicit edits to every todo line before plan/apply");
        }
    }
    Ok(())
}

fn next_migration(
    directory: &Path,
    name: &str,
) -> Result<(PathBuf, String, Option<String>), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("create '{}': {error}", directory.display()))?;
    let existing = load_directory(directory).map_err(|error| error.to_string())?;
    let number = existing
        .iter()
        .filter_map(|file| {
            file.path
                .as_ref()?
                .file_stem()?
                .to_str()?
                .split('_')
                .next()?
                .parse::<usize>()
                .ok()
        })
        .max()
        .unwrap_or(existing.len())
        .checked_add(1)
        .ok_or_else(|| "migration number exhausted".to_string())?;
    let slug = migration_slug(name)?;
    Ok((
        directory.join(format!("{number:04}_{slug}.uid")),
        format!("m{number:04}_{slug}"),
        existing.last().map(|file| file.id.clone()),
    ))
}

fn print_schema_check(checked: &SchemaCheck, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(checked).map_err(|error| error.to_string())?
        );
    } else {
        println!("{}", checked.normalized);
        eprintln!(
            "schema revision {}\nschema hash {}",
            checked.schema.revision, checked.schema.hash
        );
    }
    Ok(())
}

pub fn migration_plan(
    db: impl Into<PathBuf>,
    directory: impl AsRef<Path>,
    json: bool,
) -> Result<(), String> {
    let db = db.into();
    let files = load_directory(directory).map_err(|error| error.to_string())?;
    let engine = if db.exists() {
        Engine::open_redb(db).map_err(|error| error.to_string())?
    } else {
        Engine::memory()
    };
    let plan = engine
        .plan_migrations(&files)
        .map_err(|error| error.to_string())?;
    print_migration_plan(&plan, json)
}

pub fn migration_apply(
    db: impl Into<PathBuf>,
    directory: impl AsRef<Path>,
    json: bool,
) -> Result<(), String> {
    let files = load_directory(directory).map_err(|error| error.to_string())?;
    let mut engine = Engine::open_redb(db).map_err(|error| error.to_string())?;
    let result = engine
        .apply_migrations(&files)
        .map_err(|error| error.to_string())?;
    print_migration_apply(&result, json)
}

pub fn migration_status(
    db: impl Into<PathBuf>,
    directory: impl AsRef<Path>,
    json: bool,
) -> Result<(), String> {
    let db = db.into();
    require_existing_database(&db)?;
    let files = load_directory(directory).map_err(|error| error.to_string())?;
    let engine = Engine::open_redb(db).map_err(|error| error.to_string())?;
    let status = engine
        .migration_status(&files)
        .map_err(|error| error.to_string())?;
    print_migration_status(&status, json)
}

fn require_existing_database(path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!(
            "database '{}' does not exist; apply migrations to create it",
            path.display()
        ))
    }
}

fn migration_slug(name: &str) -> Result<String, String> {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('_');
            }
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        Err("migration name must contain an ASCII letter or digit".into())
    } else {
        Ok(slug)
    }
}

fn print_migration_plan(plan: &MigrationPlan, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(plan).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    println!(
        "schema {} -> {}\n{} applied, {} pending",
        plan.current_schema.revision,
        plan.target_schema.revision,
        plan.applied_count,
        plan.pending.len()
    );
    for migration in &plan.pending {
        println!(
            "{}{}",
            migration.id,
            if migration.destructive {
                " [destructive]"
            } else {
                ""
            }
        );
        println!(
            "  schema {} {} -> {} {}",
            migration.before.revision,
            migration.before.hash,
            migration.after.revision,
            migration.after.hash
        );
        for operation in &migration.operations {
            println!("  {operation}");
        }
    }
    Ok(())
}

fn print_migration_apply(result: &MigrationApply, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(result).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "applied {} migration(s), skipped {}\nschema revision {}\nschema hash {}",
            result.applied.len(),
            result.skipped.len(),
            result.schema.revision,
            result.schema.hash
        );
        for id in &result.applied {
            println!("  applied {id}");
        }
    }
    Ok(())
}

fn print_migration_status(status: &MigrationStatus, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(status).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "schema revision {}\nschema hash {}\n{} applied, {} pending",
            status.schema.revision,
            status.schema.hash,
            status.applied.len(),
            status.pending.len()
        );
        for entry in &status.applied {
            println!("  applied {}", entry.id);
        }
        for id in &status.pending {
            println!("  pending {id}");
        }
    }
    Ok(())
}

fn run_local_engine(mut engine: Engine, source: Option<String>, json: bool) -> Result<(), String> {
    if let Some(source) = source {
        return print_response(&engine.execute(&source), json);
    }
    repl(Some(&mut engine), "", json)
}

pub fn run_cli(addr: &str, source: Option<String>, json: bool) -> Result<(), String> {
    if let Some(source) = source {
        return print_response(&send_one(addr, &source)?, json);
    }
    repl(None, addr, json)
}

fn repl(mut engine: Option<&mut Engine>, addr: &str, json: bool) -> Result<(), String> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if !interactive {
        let source = read_source(stdin.lock())?;
        if source.trim().is_empty() {
            return Ok(());
        }
        let response = match engine {
            Some(engine) => engine.execute(&source),
            None => send_one(addr, &source)?,
        };
        return print_response(&response, json);
    }
    eprintln!(
        "Enter a script. A blank line runs it when ready; .quit exits. Local mode also supports .schema and .tables."
    );
    let mut input = ReplInput::new();
    loop {
        eprint!("{}", input.prompt());
        io::stderr().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line).map_err(|e| e.to_string())?;
        if input.is_empty() && matches!(line.trim(), ".quit" | "quit" | "exit") {
            break;
        }
        if input.is_empty()
            && let Some(engine) = engine.as_deref()
        {
            match line.trim() {
                ".schema" => {
                    println!("{}", engine.schema());
                    continue;
                }
                ".tables" => {
                    println!("{}", engine.tables().join("\n"));
                    continue;
                }
                _ => {}
            }
        }
        match input.accept(&line, n == 0) {
            ReplAction::Continue | ReplAction::Ready => {}
            ReplAction::Execute(source) => {
                let response = match engine.as_deref_mut() {
                    Some(engine) => Ok(engine.execute(&source)),
                    None => send_one(addr, &source),
                };
                match response.and_then(|response| print_response(&response, json)) {
                    Ok(()) => {}
                    Err(error) => eprintln!("{error}"),
                }
            }
            ReplAction::NeedMore(error) => {
                eprintln!("{error}\ninput is incomplete; continue typing")
            }
            ReplAction::Reject(error) => eprintln!("{error}"),
            ReplAction::ExitIncomplete(error) => {
                eprintln!("{error}\ninput ended before the script was complete");
                break;
            }
            ReplAction::Exit => break,
        }
        if n == 0 {
            break;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum ReplAction {
    Continue,
    Ready,
    Execute(String),
    NeedMore(Error),
    Reject(Error),
    ExitIncomplete(Error),
    Exit,
}

struct ReplInput {
    source: String,
    status: InputStatus,
}

impl ReplInput {
    fn new() -> Self {
        Self {
            source: String::new(),
            status: InputStatus::Complete,
        }
    }

    fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    fn prompt(&self) -> &'static str {
        if self.is_empty() {
            "unionid> "
        } else if matches!(self.status, InputStatus::Complete) {
            "  ready> "
        } else {
            "     ..> "
        }
    }

    fn accept(&mut self, line: &str, eof: bool) -> ReplAction {
        if eof {
            return match &self.status {
                _ if self.is_empty() => ReplAction::Exit,
                InputStatus::Complete => ReplAction::Execute(std::mem::take(&mut self.source)),
                InputStatus::Incomplete(error) => ReplAction::ExitIncomplete(error.clone()),
                InputStatus::Invalid(error) => ReplAction::Reject(error.clone()),
            };
        }

        if line.trim().is_empty() {
            return match &self.status {
                _ if self.is_empty() => ReplAction::Continue,
                InputStatus::Complete => ReplAction::Execute(std::mem::take(&mut self.source)),
                InputStatus::Incomplete(error) => ReplAction::NeedMore(error.clone()),
                InputStatus::Invalid(error) => ReplAction::Reject(error.clone()),
            };
        }

        if self.is_empty() && line.trim_start().starts_with('#') {
            return ReplAction::Continue;
        }

        self.source.push_str(line);
        if self.source.len() > crate::syntax::MAX_SOURCE_BYTES {
            self.reset();
            return ReplAction::Reject(Error::new("E_LIMIT", "source exceeds 1 MiB"));
        }
        self.status = input_status(&self.source);
        match &self.status {
            InputStatus::Complete => ReplAction::Ready,
            InputStatus::Incomplete(_) => ReplAction::Continue,
            InputStatus::Invalid(error) => {
                let error = error.clone();
                self.reset();
                ReplAction::Reject(error)
            }
        }
    }

    fn reset(&mut self) {
        self.source.clear();
        self.status = InputStatus::Complete;
    }
}

pub fn read_source(reader: impl Read) -> Result<String, String> {
    let mut source = String::new();
    reader
        .take((crate::syntax::MAX_SOURCE_BYTES + 1) as u64)
        .read_to_string(&mut source)
        .map_err(|e| format!("read script: {e}"))?;
    if source.len() > crate::syntax::MAX_SOURCE_BYTES {
        return Err("source exceeds 1 MiB".into());
    }
    Ok(source)
}

pub fn send_one(addr: &str, query: &str) -> Result<QueryResponse, String> {
    if query.len() > crate::syntax::MAX_SOURCE_BYTES {
        return Err("source exceeds 1 MiB".into());
    }
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut stream, &serde_json::json!({"query": query}))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"\n")
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(16 * 1024 * 1024 + 1)
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    if line.len() > 16 * 1024 * 1024 {
        return Err("response exceeds 16 MiB".into());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("decode response: {e}"))
}

/// Send one versioned request. The request and response are each one JSON line;
/// embedded newlines in `query` remain part of the JSON string.
pub fn send_request(addr: &str, request: &ProtocolRequest) -> Result<ProtocolResponse, String> {
    if request.query.len() > crate::syntax::MAX_SOURCE_BYTES {
        return Err("source exceeds 1 MiB".into());
    }
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut stream, request).map_err(|e| e.to_string())?;
    stream
        .write_all(b"\n")
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(16 * 1024 * 1024 + 1)
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    if line.len() > 16 * 1024 * 1024 {
        return Err("response exceeds 16 MiB".into());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("decode response: {e}"))
}

fn print_response(response: &QueryResponse, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(response).map_err(|e| e.to_string())?
        );
    }
    if !response.ok {
        return Err(response.message.clone());
    }
    for warning in &response.warnings {
        eprintln!("warning: {warning}");
    }
    if !json {
        if let Some(plan) = &response.plan {
            let access = match plan.access.kind {
                QueryAccessKind::FullScan => "full_scan",
                QueryAccessKind::PrimaryKeyLookup => "primary_key_lookup",
                QueryAccessKind::SecondaryIndexLookup => "secondary_index_lookup",
            };
            let index = plan
                .access
                .index
                .as_deref()
                .map(|index| format!(" via {index}"))
                .unwrap_or_default();
            let condition = plan
                .access
                .condition
                .as_deref()
                .map(|condition| format!(" using {condition}"))
                .unwrap_or_default();
            println!("table | {}", plan.table);
            println!(
                "access | {access}{index}{condition} ({} of {} row(s))",
                plan.access.estimated_rows, plan.access.table_rows
            );
            println!(
                "stages | {}",
                plan.stages
                    .iter()
                    .map(|stage| query_stage_name(&stage.kind))
                    .collect::<Vec<_>>()
                    .join(" -> ")
            );
            println!(
                "result | {}",
                plan.result_schema
                    .iter()
                    .map(|column| format!("{} {}", column.name, column.ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        } else if response.columns.is_empty() {
            println!("{}", response.message);
        } else {
            println!(
                "{}",
                response
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            for row in &response.rows {
                println!(
                    "{}",
                    response
                        .columns
                        .iter()
                        .map(|c| row.get(&c.name).map(display_value).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
            }
            println!("{}", response.message);
        }
    }
    Ok(())
}

fn query_stage_name(stage: &QueryStageKind) -> &'static str {
    match stage {
        QueryStageKind::Let => "let",
        QueryStageKind::Filter => "filter",
        QueryStageKind::FilterMatch => "filter_match",
        QueryStageKind::Derive => "derive",
        QueryStageKind::DeriveMatch => "derive_match",
        QueryStageKind::Aggregate => "aggregate",
        QueryStageKind::Select => "select",
        QueryStageKind::Sort => "sort",
        QueryStageKind::Take => "take",
    }
}

pub fn display_value(value: &Value) -> String {
    value.source_text()
}

#[cfg(test)]
mod tests {
    use super::{ReplAction, ReplInput};

    #[test]
    fn repl_tracks_continuation_ready_submission_and_eof() {
        let mut input = ReplInput::new();
        assert_eq!(input.prompt(), "unionid> ");
        assert_eq!(input.accept("type Task =\n", false), ReplAction::Continue);
        assert_eq!(input.prompt(), "     ..> ");

        let ReplAction::NeedMore(error) = input.accept("\n", false) else {
            panic!("blank input must retain an incomplete script");
        };
        assert_eq!(error.code, "E_INCOMPLETE");
        assert!(!input.is_empty());

        assert_eq!(input.accept("  id int\n", false), ReplAction::Ready);
        assert_eq!(input.prompt(), "  ready> ");
        assert_eq!(
            input.accept("\n", false),
            ReplAction::Execute("type Task =\n  id int\n".into())
        );
        assert!(input.is_empty());
        assert_eq!(input.prompt(), "unionid> ");

        assert_eq!(input.accept("from tasks |\n", false), ReplAction::Continue);
        assert_eq!(input.accept("filter id == 1\n", false), ReplAction::Ready);
        assert_eq!(
            input.accept("", true),
            ReplAction::Execute("from tasks |\nfilter id == 1\n".into())
        );
    }

    #[test]
    fn repl_rejects_invalid_input_and_reports_incomplete_eof() {
        let mut input = ReplInput::new();
        let ReplAction::Reject(error) = input.accept("\tfrom tasks\n", false) else {
            panic!("tab indentation must be rejected immediately");
        };
        assert_eq!(error.code, "E_SYNTAX");
        assert!(error.span.is_some());
        assert!(input.is_empty());

        assert_eq!(input.accept("from tasks |\n", false), ReplAction::Continue);
        let ReplAction::ExitIncomplete(error) = input.accept("", true) else {
            panic!("EOF must report a buffered incomplete script");
        };
        assert_eq!(error.code, "E_INCOMPLETE");

        let mut empty = ReplInput::new();
        assert_eq!(empty.accept("\n", false), ReplAction::Continue);
        assert_eq!(
            empty.accept("# comment only\n", false),
            ReplAction::Continue
        );
        assert!(empty.is_empty());
        assert_eq!(empty.accept("", true), ReplAction::Exit);
    }
}
