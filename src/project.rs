use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::migration::MigrationFile;
use crate::{Engine, QueryResponse};

const STARTER_SCHEMA: &str = r#"type State =
  Pending
  | Running {
    worker text,
    attempt int,
  }
  | Done {
    result text,
  }

type Task = {
  id int,
  title text,
  state State,
}

table tasks Task
  key id
"#;

const STARTER_MIGRATION: &str = r#"migration m0001_initial
  add type State =
    Pending
    | Running {
      worker text,
      attempt int,
    }
    | Done {
      result text,
    }
  add type Task = {
    id int,
    title text,
    state State,
  }
  add table tasks Task key id
"#;

const STARTER_SEED: &str = r#"insert many tasks [
  {id = 1, state = Running {attempt = 1, worker = "local"}, title = "learn ADTs"},
  {id = 2, state = Pending, title = "ship the app"},
]
returning {id, state}
"#;

const STARTER_QUERY: &str = r#"from tasks
filter (
  match state {
    Running {attempt, ..} => attempt >= 1,
    _ => false,
  }
)
select {id, title, state}
sort id
"#;

const STARTER_README: &str = r#"# Unionid starter / Unionid 入门项目

这个项目展示 Unionid 的核心路径：用代数数据类型定义数据，执行 migration，然后直接按 enum variant 查询。

This project shows Unionid's core path: define data with algebraic data types, apply a migration, and query enum variants directly.

From this directory / 在当前目录运行：

```sh
unionid migration apply --db data/tasks.redb --dir migrations
unionid run --db data/tasks.redb --file seed.unid
unionid run --db data/tasks.redb --file queries/list_running.unid
unionid doctor --db data/tasks.redb
unionid check --db data/tasks.redb
```

`schema.unid` is the declarative contract used by offline schema, query, and binding tools. `migrations/` is the ordered history applied to the database.

`schema.unid` 是离线 schema、query 和绑定工具使用的声明式契约；`migrations/` 是按顺序应用到数据库的演进历史。
"#;

const STARTER_GITIGNORE: &str = "data/\n";

/// Create a validated starter project and publish it as one directory rename.
pub fn init(directory: impl AsRef<Path>) -> Result<(), String> {
    let directory = resolved_destination(directory.as_ref())?;
    validate_starter_project()?;
    let existed = destination_is_missing_or_empty(&directory)?;
    let staging = create_private_sibling(&directory, "stage")?;
    if let Err(error) = write_starter_project(&staging) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    if let Err(error) = publish_starter_project(&staging, &directory, existed) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    println!("initialized Unionid project at {}", directory.display());
    println!("run these commands from that directory:");
    println!("  unionid migration apply --db data/tasks.redb --dir migrations");
    println!("  unionid run --db data/tasks.redb --file seed.unid");
    println!("  unionid run --db data/tasks.redb --file queries/list_running.unid");
    println!("  unionid doctor --db data/tasks.redb");
    println!("  unionid check --db data/tasks.redb");
    Ok(())
}

/// Resolve the parent once so publication and staging use the same real directory.
fn resolved_destination(directory: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(directory)
        .map_err(|error| format!("resolve '{}': {error}", directory.display()))?;
    let name = absolute.file_name().ok_or_else(|| {
        format!(
            "init destination '{}' must name a project directory",
            directory.display()
        )
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        format!(
            "init destination '{}' has no parent directory",
            directory.display()
        )
    })?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| format!("resolve parent '{}': {error}", parent.display()))?;
    Ok(parent.join(name))
}

/// Accept only a missing path or a real, empty directory without following a symlink.
fn destination_is_missing_or_empty(directory: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "init destination '{}' is not a directory",
                    directory.display()
                ));
            }
            let mut entries = std::fs::read_dir(directory)
                .map_err(|error| format!("read '{}': {error}", directory.display()))?;
            if entries
                .next()
                .transpose()
                .map_err(|error| format!("read entry in '{}': {error}", directory.display()))?
                .is_some()
            {
                return Err(format!(
                    "init destination '{}' is not empty; no files were changed",
                    directory.display()
                ));
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("inspect '{}': {error}", directory.display())),
    }
}

/// Create an unpredictable sibling directory with owner-only permissions on Unix.
fn create_private_sibling(directory: &Path, label: &str) -> Result<PathBuf, String> {
    for _ in 0..16 {
        let candidate = sibling_candidate(directory, label)?;
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("create '{}': {error}", candidate.display()));
            }
        }
    }
    Err(format!(
        "create private staging directory beside '{}' after repeated name collisions",
        directory.display()
    ))
}

/// Produce a random sibling name without exposing business data in the path.
fn sibling_candidate(directory: &Path, label: &str) -> Result<PathBuf, String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| format!("generate staging name: {error}"))?;
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let parent = directory
        .parent()
        .ok_or_else(|| format!("destination '{}' has no parent", directory.display()))?;
    Ok(parent.join(format!(".unionid-init-{label}-{suffix}")))
}

/// Publish a complete tree atomically, preserving an existing empty destination on failure.
fn publish_starter_project(staging: &Path, directory: &Path, existed: bool) -> Result<(), String> {
    if !existed {
        if std::fs::symlink_metadata(directory).is_ok() {
            return Err(format!(
                "init destination '{}' appeared while the project was being prepared; no files were changed",
                directory.display()
            ));
        }
        return std::fs::rename(staging, directory)
            .map_err(|error| format!("publish '{}': {error}", directory.display()));
    }

    let backup = move_existing_destination_aside(directory)?;
    if let Err(error) = destination_is_missing_or_empty(&backup).and_then(|is_present| {
        if is_present {
            Ok(())
        } else {
            Err(format!(
                "init destination '{}' disappeared while the project was being prepared",
                directory.display()
            ))
        }
    }) {
        let restored = std::fs::rename(&backup, directory);
        return match restored {
            Ok(()) => Err(error),
            Err(restore_error) => Err(format!(
                "{error}; also failed to restore '{}': {restore_error}",
                directory.display()
            )),
        };
    }

    if let Err(error) = std::fs::rename(staging, directory) {
        let restored = std::fs::rename(&backup, directory);
        return match restored {
            Ok(()) => Err(format!("publish '{}': {error}", directory.display())),
            Err(restore_error) => Err(format!(
                "publish '{}': {error}; also failed to restore the empty destination: {restore_error}",
                directory.display()
            )),
        };
    }
    let _ = std::fs::remove_dir(&backup);
    Ok(())
}

/// Atomically capture the current destination at a random sibling path.
fn move_existing_destination_aside(directory: &Path) -> Result<PathBuf, String> {
    for _ in 0..16 {
        let backup = sibling_candidate(directory, "previous")?;
        match std::fs::rename(directory, &backup) {
            Ok(()) => return Ok(backup),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "reserve existing destination '{}': {error}",
                    directory.display()
                ));
            }
        }
    }
    Err(format!(
        "reserve existing destination '{}' after repeated name collisions",
        directory.display()
    ))
}

/// Check every built-in artifact through the public language and execution paths.
fn validate_starter_project() -> Result<(), String> {
    for (name, source) in [
        ("schema", STARTER_SCHEMA),
        ("migration", STARTER_MIGRATION),
        ("seed", STARTER_SEED),
        ("query", STARTER_QUERY),
    ] {
        let formatted = crate::format_source(source)
            .map_err(|error| format!("invalid built-in starter {name}: {error}"))?;
        if formatted != source {
            return Err(format!(
                "built-in starter {name} is not canonically formatted"
            ));
        }
    }
    Engine::check_schema(STARTER_SCHEMA)
        .map_err(|error| format!("invalid built-in starter schema: {error}"))?;
    MigrationFile::parse(STARTER_MIGRATION)
        .map_err(|error| format!("invalid built-in starter migration: {error}"))?;
    crate::query_contract::describe(STARTER_SCHEMA, STARTER_QUERY)
        .map_err(|error| format!("invalid built-in starter query: {error}"))?;
    let mut engine = Engine::memory();
    validate_starter_response(engine.execute(STARTER_SCHEMA), "schema execution")?;
    validate_starter_response(engine.execute(STARTER_SEED), "seed")?;
    validate_starter_response(engine.execute(STARTER_QUERY), "query execution")?;
    Ok(())
}

/// Convert an unexpected built-in execution failure into an initializer error.
fn validate_starter_response(response: QueryResponse, part: &str) -> Result<(), String> {
    if let Some(error) = response.error {
        return Err(format!("invalid built-in starter {part}: {error}"));
    }
    if !response.ok {
        return Err(format!(
            "invalid built-in starter {part}: {}",
            response.message
        ));
    }
    Ok(())
}

/// Populate a private staging directory before its single publication rename.
fn write_starter_project(directory: &Path) -> Result<(), String> {
    for child in ["migrations", "queries", "data"] {
        let path = directory.join(child);
        std::fs::create_dir(&path)
            .map_err(|error| format!("create '{}': {error}", path.display()))?;
    }
    for (relative, contents) in [
        ("schema.unid", STARTER_SCHEMA),
        ("migrations/0001_initial.unid", STARTER_MIGRATION),
        ("seed.unid", STARTER_SEED),
        ("queries/list_running.unid", STARTER_QUERY),
        ("README.md", STARTER_README),
        (".gitignore", STARTER_GITIGNORE),
    ] {
        let path = directory.join(relative);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("create '{}': {error}", path.display()))?;
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("write '{}': {error}", path.display()))?;
    }
    Ok(())
}
