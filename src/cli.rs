use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::migration::{MigrationApply, MigrationPlan, MigrationStatus, load_directory};
use crate::{Engine, QueryResponse, SchemaCheck, Value};

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
        "Enter a script, then a blank line to run. .quit exits; local mode also supports .schema and .tables."
    );
    let mut buffer = String::new();
    loop {
        eprint!(
            "{}",
            if buffer.is_empty() {
                "unionid> "
            } else {
                "     ..> "
            }
        );
        io::stderr().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line).map_err(|e| e.to_string())?;
        if buffer.is_empty() && matches!(line.trim(), ".quit" | "quit" | "exit") {
            break;
        }
        if buffer.is_empty()
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
        if !line.trim().is_empty() {
            buffer.push_str(&line);
        }
        if buffer.len() > crate::syntax::MAX_SOURCE_BYTES {
            eprintln!("source exceeds 1 MiB");
            buffer.clear();
        }
        if (line.trim().is_empty() || n == 0) && !buffer.trim().is_empty() {
            let response = match engine.as_deref_mut() {
                Some(engine) => Ok(engine.execute(&buffer)),
                None => send_one(addr, &buffer),
            };
            match response.and_then(|r| print_response(&r, json)) {
                Ok(()) => {}
                Err(e) => eprintln!("{e}"),
            }
            buffer.clear();
        }
        if n == 0 {
            break;
        }
    }
    Ok(())
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
        if response.columns.is_empty() {
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

pub fn display_value(value: &Value) -> String {
    value.source_text()
}
