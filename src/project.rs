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

pub fn init(directory: impl AsRef<Path>) -> Result<(), String> {
    let directory = directory.as_ref();
    validate_starter_project()?;

    let root_created = match std::fs::symlink_metadata(directory) {
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
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(directory)
                .map_err(|error| format!("create '{}': {error}", directory.display()))?;
            true
        }
        Err(error) => return Err(format!("inspect '{}': {error}", directory.display())),
    };

    let mut created = Vec::new();
    let result = write_starter_project(directory, &mut created);
    if let Err(error) = result {
        cleanup_starter_project(directory, root_created, created);
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

enum StarterEntry {
    File(PathBuf),
    Directory(PathBuf),
}

fn write_starter_project(directory: &Path, created: &mut Vec<StarterEntry>) -> Result<(), String> {
    for child in ["migrations", "queries", "data"] {
        let path = directory.join(child);
        std::fs::create_dir(&path)
            .map_err(|error| format!("create '{}': {error}", path.display()))?;
        created.push(StarterEntry::Directory(path));
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
        created.push(StarterEntry::File(path.clone()));
        file.write_all(contents.as_bytes())
            .map_err(|error| format!("write '{}': {error}", path.display()))?;
    }
    Ok(())
}

fn cleanup_starter_project(directory: &Path, root_created: bool, created: Vec<StarterEntry>) {
    for entry in created.into_iter().rev() {
        match entry {
            StarterEntry::File(path) => {
                let _ = std::fs::remove_file(path);
            }
            StarterEntry::Directory(path) => {
                let _ = std::fs::remove_dir(path);
            }
        }
    }
    if root_created {
        let _ = std::fs::remove_dir(directory);
    }
}
