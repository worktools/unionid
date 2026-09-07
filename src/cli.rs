use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustyline::history::DefaultHistory;
use rustyline::{CompletionType, Config, Editor, error::ReadlineError};

use crate::migration::{MigrationApply, MigrationPlan, MigrationStatus, load_directory};
pub use crate::repl::HistoryOptions;
use crate::repl::{CompletionHelper, HistoryStore};
use crate::{
    Engine, Error, IdempotencyPruneOptions, InputStatus, Introspection, IntrospectionKind,
    PageDirection, ProtocolRequest, ProtocolResponse, QueryAccessKind, QueryResponse,
    QueryStageKind, SchemaCheck, StorageMode, Value, backup, input_status,
};

pub fn run_local(source: Option<String>, json: bool) -> Result<(), String> {
    run_local_with_options(source, json, HistoryOptions::default())
}

pub fn run_local_with_options(
    source: Option<String>,
    json: bool,
    history: HistoryOptions,
) -> Result<(), String> {
    run_local_engine(Engine::memory(), source, json, history)
}

pub fn format_source(source: &str, check: bool) -> Result<(), String> {
    let formatted = crate::format_source(source).map_err(|error| error.to_string())?;
    if check {
        if source == formatted {
            return Ok(());
        }
        return Err("input is not canonically formatted".into());
    }
    io::stdout()
        .write_all(formatted.as_bytes())
        .map_err(|error| format!("write formatted source: {error}"))
}

pub fn run_local_redb(
    path: impl Into<std::path::PathBuf>,
    source: Option<String>,
    json: bool,
) -> Result<(), String> {
    run_local_redb_with_options(path, source, json, HistoryOptions::default())
}

pub fn run_local_redb_read_only(
    path: impl Into<std::path::PathBuf>,
    source: Option<String>,
    json: bool,
) -> Result<(), String> {
    run_local_redb_read_only_with_options(path, source, json, HistoryOptions::default(), true)
}

pub fn run_local_redb_with_options(
    path: impl Into<std::path::PathBuf>,
    source: Option<String>,
    json: bool,
    history: HistoryOptions,
) -> Result<(), String> {
    run_local_redb_read_only_with_options(path, source, json, history, false)
}

pub fn run_local_redb_read_only_with_options(
    path: impl Into<std::path::PathBuf>,
    source: Option<String>,
    json: bool,
    history: HistoryOptions,
    read_only: bool,
) -> Result<(), String> {
    let engine = if read_only {
        Engine::open_redb_read_only(path)
    } else {
        Engine::open_redb(path)
    }
    .map_err(|error| error.to_string())?;
    run_local_engine(engine, source, json, history)
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

pub fn receipt_status(path: PathBuf, json: bool) -> Result<(), String> {
    let engine = Engine::open_redb(path).map_err(|error| error.to_string())?;
    let status = engine
        .idempotency_status()
        .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&status).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "receipts {}/{}\nencoded bytes {}/{}\ndurability {:?}",
            status.count,
            status.max_count,
            status.encoded_bytes,
            status.max_encoded_bytes,
            status.durability
        );
        if let Some(oldest) = status.oldest {
            println!(
                "oldest {} sequence {} completed {}",
                oldest.key, oldest.committed_sequence, oldest.completed_at_unix_ms
            );
        }
        if let Some(newest) = status.newest {
            println!(
                "newest {} sequence {} completed {}",
                newest.key, newest.committed_sequence, newest.completed_at_unix_ms
            );
        }
    }
    Ok(())
}

pub fn receipt_prune(
    path: PathBuf,
    options: IdempotencyPruneOptions,
    confirm: bool,
    json: bool,
) -> Result<(), String> {
    let mut engine = Engine::open_redb(path).map_err(|error| error.to_string())?;
    let result = if confirm {
        engine.prune_idempotency_receipts(options)
    } else {
        engine.plan_idempotency_prune(options)
    }
    .map_err(|error| error.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&result).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "{} {} receipt(s), {} encoded bytes; {} remain",
            if result.applied {
                "pruned"
            } else {
                "would prune"
            },
            result.selected_count,
            result.selected_encoded_bytes,
            result.remaining_count
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
            "{message}\nformat {}\nchecksum {}\nschema revision {}\nschema hash {}\n{} migration(s)\n{} idempotency receipt(s)",
            info.format_version,
            info.checksum,
            info.schema.revision,
            info.schema.hash,
            info.migration_count,
            info.receipt_count
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

fn run_local_engine(
    mut engine: Engine,
    source: Option<String>,
    json: bool,
    history: HistoryOptions,
) -> Result<(), String> {
    if let Some(source) = source {
        return print_response(&engine.execute(&source), json);
    }
    repl(Some(&mut engine), "", json, history)
}

pub fn run_cli(addr: &str, source: Option<String>, json: bool) -> Result<(), String> {
    run_cli_with_options(addr, source, json, HistoryOptions::default())
}

pub fn run_cli_with_options(
    addr: &str,
    source: Option<String>,
    json: bool,
    history: HistoryOptions,
) -> Result<(), String> {
    if let Some(source) = source {
        return print_response(&send_one(addr, &source)?, json);
    }
    repl(None, addr, json, history)
}

fn repl(
    mut engine: Option<&mut Engine>,
    addr: &str,
    json: bool,
    history_options: HistoryOptions,
) -> Result<(), String> {
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
    eprintln!("Enter a script. A blank line runs it when ready; .help lists commands.");
    let mut introspection = load_introspection(engine.as_deref(), addr, IntrospectionKind::Schema)
        .map_err(|error| {
            eprintln!(
                "catalog completion unavailable: {error}; keyword completion remains available"
            );
        })
        .ok();
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .auto_add_history(false)
        .build();
    let mut editor = Editor::<CompletionHelper, DefaultHistory>::with_config(config)
        .map_err(|error| format!("initialize line editor: {error}"))?;
    editor.set_helper(Some(CompletionHelper::new(introspection.as_ref())));
    let loaded = HistoryStore::load(&history_options);
    if let Some(warning) = loaded.warning {
        eprintln!("warning: {warning}");
    }
    for entry in loaded.entries {
        let _ = editor.add_history_entry(entry);
    }
    let mut history = loaded.store;
    let mut input = ReplInput::new();
    loop {
        let (line, eof) = match editor.readline(input.prompt()) {
            Ok(line) => (format!("{line}\n"), false),
            Err(ReadlineError::Eof) => (String::new(), true),
            Err(ReadlineError::Interrupted) => {
                input.reset();
                eprintln!("input cleared");
                continue;
            }
            Err(error) => return Err(format!("read input: {error}")),
        };
        let command = line.trim();
        if input.is_empty() && matches!(command, ".quit" | "quit" | "exit") {
            remember(&mut editor, &mut history, command);
            break;
        }
        if input.is_empty() && command == ".help" {
            remember(&mut editor, &mut history, command);
            print_repl_help();
            continue;
        }
        if input.is_empty()
            && let Some(kind) = introspection_command(command)
        {
            remember(&mut editor, &mut history, command);
            match load_introspection(engine.as_deref(), addr, kind) {
                Ok(current) => {
                    print_introspection(&current, kind, json)?;
                    introspection = Some(current);
                    editor
                        .helper_mut()
                        .expect("REPL helper is installed")
                        .set_catalog(introspection.as_ref());
                }
                Err(error) => eprintln!("{error}"),
            }
            continue;
        }
        match input.accept(&line, eof) {
            ReplAction::Continue | ReplAction::Ready => {}
            ReplAction::Execute(source) => {
                remember(&mut editor, &mut history, &source);
                let response = match engine.as_deref_mut() {
                    Some(engine) => Ok(engine.execute(&source)),
                    None => send_one(addr, &source),
                };
                match response.and_then(|response| {
                    let changed = response.schema.as_ref()
                        != introspection.as_ref().map(|current| &current.schema);
                    let result = print_response(&response, json);
                    if changed && response.ok {
                        match load_introspection(
                            engine.as_deref(),
                            addr,
                            IntrospectionKind::Schema,
                        ) {
                            Ok(current) => {
                                introspection = Some(current);
                                editor
                                    .helper_mut()
                                    .expect("REPL helper is installed")
                                    .set_catalog(introspection.as_ref());
                            }
                            Err(error) => eprintln!(
                                "catalog completion unavailable: {error}; keyword completion remains available"
                            ),
                        }
                    }
                    result
                }) {
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
        if eof {
            break;
        }
    }
    Ok(())
}

fn remember(
    editor: &mut Editor<CompletionHelper, DefaultHistory>,
    history: &mut HistoryStore,
    source: &str,
) {
    let source = source.trim_end();
    if source.is_empty() {
        return;
    }
    let _ = editor.add_history_entry(source);
    if let Err(error) = history.append(source) {
        eprintln!("warning: {error}; history disabled for this session");
        history.disable();
    }
}

fn load_introspection(
    engine: Option<&Engine>,
    addr: &str,
    kind: IntrospectionKind,
) -> Result<Introspection, String> {
    match engine {
        Some(engine) => Ok(engine.introspection()),
        None => send_introspection(addr, kind),
    }
}

pub fn send_introspection(addr: &str, kind: IntrospectionKind) -> Result<Introspection, String> {
    let request = ProtocolRequest::introspection("cli-introspection", kind);
    let response = send_request(addr, &request)?;
    if !response.ok {
        return Err(response.message);
    }
    response
        .introspection
        .ok_or_else(|| "server returned no introspection payload".into())
}

fn introspection_command(command: &str) -> Option<IntrospectionKind> {
    match command {
        ".schema" => Some(IntrospectionKind::Schema),
        ".tables" => Some(IntrospectionKind::Tables),
        ".types" => Some(IntrospectionKind::Types),
        ".storage" => Some(IntrospectionKind::Storage),
        _ => None,
    }
}

fn print_introspection(
    introspection: &Introspection,
    kind: IntrospectionKind,
    json: bool,
) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "kind": kind,
                "introspection": introspection,
            }))
            .map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    match kind {
        IntrospectionKind::Schema => {
            if introspection.schema_source.is_empty() {
                println!("(empty schema)");
            } else {
                println!("{}", introspection.schema_source);
            }
        }
        IntrospectionKind::Tables => print_names(&introspection.tables, "tables"),
        IntrospectionKind::Types => print_names(&introspection.types, "types"),
        IntrospectionKind::Storage => {
            let mode = match introspection.storage {
                StorageMode::Memory => "memory",
                StorageMode::Redb => "redb",
                StorageMode::LegacyWal => "legacy_wal",
                StorageMode::LegacyWalSnapshot => "legacy_wal_snapshot",
            };
            println!("mode {mode}");
            println!(
                "read only {}",
                if introspection.read_only { "yes" } else { "no" }
            );
            println!("schema revision {}", introspection.schema.revision);
            println!("schema hash {}", introspection.schema.hash);
            println!("migrations {}", introspection.migration_count);
            if let Some(head) = &introspection.migration_head {
                println!("migration head {head}");
            }
        }
    }
    Ok(())
}

fn print_names(names: &[String], noun: &str) {
    if names.is_empty() {
        println!("(no {noun})");
    } else {
        println!("{}", names.join("\n"));
    }
}

fn print_repl_help() {
    eprintln!(
        ".schema   show the canonical schema\n.tables   list tables\n.types    list named types\n.storage  show storage, schema identity, and migration head\n.help     show this help\n.quit     exit\n\nTab completes language keywords and catalog names. A blank line submits a complete script."
    );
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
            if let Some(page) = &plan.page {
                println!(
                    "page | {} {} via sorted_scan (read at most {}, cursor at most {} bytes)",
                    page.limit,
                    page_direction_name(page.direction),
                    page.read_limit,
                    page.max_cursor_bytes
                );
            }
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
        if let Some(page) = &response.page {
            println!(
                "page | {} {} at sequence {} (has_more: {})",
                page.limit,
                page_direction_name(page.direction),
                page.snapshot_sequence,
                page.has_more
            );
            if let Some(cursor) = &page.previous_cursor {
                println!("previous_cursor | {cursor}");
            }
            if let Some(cursor) = &page.next_cursor {
                println!("next_cursor | {cursor}");
            }
        }
    }
    Ok(())
}

fn page_direction_name(direction: PageDirection) -> &'static str {
    match direction {
        PageDirection::Forward => "forward",
        PageDirection::Backward => "backward",
    }
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
        QueryStageKind::Page => "page",
    }
}

pub fn display_value(value: &Value) -> String {
    value.source_text()
}

#[cfg(test)]
mod tests {
    use super::{IntrospectionKind, ReplAction, ReplInput, introspection_command};

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

    #[test]
    fn every_documented_introspection_command_maps_to_a_protocol_kind() {
        assert_eq!(
            introspection_command(".schema"),
            Some(IntrospectionKind::Schema)
        );
        assert_eq!(
            introspection_command(".tables"),
            Some(IntrospectionKind::Tables)
        );
        assert_eq!(
            introspection_command(".types"),
            Some(IntrospectionKind::Types)
        );
        assert_eq!(
            introspection_command(".storage"),
            Some(IntrospectionKind::Storage)
        );
        assert_eq!(introspection_command(".unknown"), None);
    }
}
