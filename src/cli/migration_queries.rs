//! Query-aware migration commands preserve structured errors through presentation.
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::migration::query_validation::{
    MigrationQueryApply, MigrationQueryError, MigrationQueryPlan, QueryValidation,
    load_query_directory,
};
use crate::{Engine, Error};

pub enum Action {
    Plan,
    Apply,
    Rehearse { copy: Option<PathBuf> },
}

#[derive(Serialize)]
pub struct QueryRehearsal {
    #[serde(flatten)]
    pub rehearsal: super::MigrationRehearsal,
    pub query_validation: QueryValidation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retained_copy: Option<PathBuf>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Output {
    Plan(Box<MigrationQueryPlan>),
    Apply(Box<MigrationQueryApply>),
    Rehearse(Box<QueryRehearsal>),
}

pub struct CommandError {
    pub error: Box<Error>,
    pub query_validation: Option<Box<QueryValidation>>,
    pub retained_copy: Option<PathBuf>,
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        MigrationQueryError::from(error).into()
    }
}

impl From<MigrationQueryError> for CommandError {
    fn from(error: MigrationQueryError) -> Self {
        Self {
            error: error.error,
            query_validation: error.query_validation,
            retained_copy: None,
        }
    }
}

fn require_valid(validation: &QueryValidation) -> Result<(), CommandError> {
    if validation.valid {
        return Ok(());
    }
    Err(CommandError {
        error: Box::new(Error::new("E_MIGRATION", "saved queries are invalid for the target schema")
            .with_hint("inspect query_validation and update the failing saved queries before applying migrations")),
        query_validation: Some(Box::new(validation.clone())),
        retained_copy: None,
    })
}

/// Resolve an explicit query set or discover the migrations directory's sibling.
/// Return None for legacy execution when discovery finds no queries or is disabled.
pub fn run_with_discovery(
    action: Action,
    db: &Path,
    directory: &Path,
    query_directory: Option<&Path>,
    no_queries: bool,
) -> Result<Option<Output>, CommandError> {
    if no_queries {
        eprintln!("warning: saved-query preflight explicitly disabled (--no-queries)");
        return Ok(None);
    }
    let queries = if let Some(path) = query_directory {
        load_query_directory(path)?
    } else {
        let migrations = std::fs::canonicalize(directory)
            .map_err(|error| Error::new("E_IO", format!("resolve migration directory: {error}")))?;
        let path = migrations
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("queries");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(
                    Error::new("E_IO", format!("inspect saved-query directory: {error}")).into(),
                );
            }
            Ok(_) => {}
        }
        let queries = crate::migration::query_validation::load_optional_query_directory(&path)?;
        if queries.is_empty() {
            return Ok(None);
        }
        queries
    };
    run_loaded(action, db, directory, queries).map(Some)
}

pub fn run(
    action: Action,
    db: &Path,
    directory: &Path,
    query_directory: &Path,
) -> Result<Output, CommandError> {
    // Load once, before opening a database, and reuse these exact immutable sources.
    let queries = load_query_directory(query_directory)?;
    run_loaded(action, db, directory, queries)
}

fn run_loaded(
    action: Action,
    db: &Path,
    directory: &Path,
    queries: Vec<crate::migration::query_validation::MigrationQuery>,
) -> Result<Output, CommandError> {
    for query in &queries {
        if let Some(warning) = crate::syntax::legacy_extension_warning(Path::new(&query.path)) {
            eprintln!("warning: {warning}");
        }
    }
    super::warn_deprecated_source_directory(directory);
    let files = crate::migration::load_directory(directory)?;
    match action {
        Action::Plan => {
            // Opening redb can update recovery metadata even in Engine read-only
            // mode. A locked copy keeps this observational command byte-preserving.
            let copy = if db.try_exists().map_err(io_error)? {
                Some(RehearsalCopy::create(db, None)?)
            } else {
                None
            };
            let engine = match &copy {
                Some(copy) => Engine::open_redb(&copy.path)?,
                None => Engine::memory(),
            };
            let plan = engine.plan_migrations_with_queries(&files, &queries)?;
            require_valid(&plan.query_validation)?;
            Ok(Output::Plan(Box::new(plan)))
        }
        Action::Apply => {
            if !db.try_exists().map_err(io_error)? {
                let plan = Engine::memory().plan_migrations_with_queries(&files, &queries)?;
                require_valid(&plan.query_validation)?;
            }
            // Preflight again under this actual Engine's write ownership. The
            // memory check above only avoids creating a file for invalid new DBs.
            let mut engine = Engine::open_redb(db)?;
            Ok(Output::Apply(Box::new(
                engine.apply_migrations_with_queries(&files, &queries)?,
            )))
        }
        Action::Rehearse { copy } => {
            let copy = RehearsalCopy::create(db, copy)?;
            let retained_copy = copy.keep.then(|| copy.path.clone());
            let started = std::time::Instant::now();
            let outcome = (|| -> Result<Output, CommandError> {
                let mut engine = Engine::open_redb(&copy.path)?;
                let source_schema = engine.schema_info();
                let applied = engine.apply_migrations_with_queries(&files, &queries)?;
                let validation = applied.query_validation;
                let integrity = engine.check_integrity().map_err(|error| CommandError {
                    error: Box::new(error),
                    query_validation: Some(Box::new(validation.clone())),
                    retained_copy: None,
                })?;
                let copy_bytes = std::fs::metadata(&copy.path)
                    .map_err(|error| CommandError {
                        error: Box::new(io_error(error)),
                        query_validation: Some(Box::new(validation.clone())),
                        retained_copy: None,
                    })?
                    .len();
                Ok(Output::Rehearse(Box::new(QueryRehearsal {
                    rehearsal: super::MigrationRehearsal {
                        schema_version: 2,
                        source_bytes: copy.source_bytes,
                        copy_bytes,
                        source_schema,
                        schema: applied.applied.schema,
                        applied: applied.applied.applied,
                        skipped: applied.applied.skipped,
                        elapsed_micros: started.elapsed().as_micros(),
                        migration_profile: engine.last_migration_profile(),
                        check_profile: integrity.profile,
                        checked: integrity.backend_clean,
                    },
                    query_validation: validation,
                    retained_copy: retained_copy.clone(),
                })))
            })();
            outcome.map_err(|mut error| {
                error.retained_copy = retained_copy;
                error
            })
        }
    }
}

impl Output {
    pub fn print(&self, json: bool) -> Result<(), String> {
        if json {
            println!(
                "{}",
                serde_json::to_string(self).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        let validation = match self {
            Self::Plan(plan) => {
                super::print_migration_plan(&plan.plan, false)?;
                &plan.query_validation
            }
            Self::Apply(applied) => {
                super::print_migration_apply(&applied.applied, false)?;
                &applied.query_validation
            }
            Self::Rehearse(report) => {
                super::print_migration_rehearsal(&report.rehearsal, false)?;
                if let Some(path) = &report.retained_copy {
                    println!("retained copy: {}", path.display());
                }
                &report.query_validation
            }
        };
        print_validation(validation);
        Ok(())
    }
}

pub fn print_validation(report: &QueryValidation) {
    println!(
        "queries: {} checked, target valid: {}",
        report.checked_files, report.valid
    );
    for file in &report.files {
        let review = if !file.valid {
            "invalid"
        } else if file.parameters_changed != Some(false) || file.result_changed != Some(false) {
            "valid; review parameter/result contract"
        } else {
            "valid"
        };
        println!("{}: {review}", file.path);
        for failure in &file.failures {
            println!(
                "  {} (schema {}): {}",
                failure.migration_id.as_deref().unwrap_or("current"),
                failure.schema.revision,
                failure.error
            );
        }
    }
    println!(
        "Generated clients still require the exact schema hash; regenerate before deployment."
    );
}

fn io_error(error: std::io::Error) -> Error {
    Error::new("E_IO", format!("migration database copy: {error}"))
}

/// Own only a newly created copy, never an existing user file. Keep explicitly
/// requested copies after apply errors; remove automatic copies on every return.
pub(super) struct RehearsalCopy {
    pub(super) path: PathBuf,
    pub(super) source_bytes: u64,
    keep: bool,
}

impl RehearsalCopy {
    pub(super) fn create(source: &Path, requested: Option<PathBuf>) -> Result<Self, Error> {
        let keep = requested.is_some();
        let path = match requested {
            Some(path) => path,
            None => {
                let mut nonce = [0_u8; 16];
                getrandom::fill(&mut nonce).map_err(|e| {
                    Error::new("E_IO", format!("generate rehearsal copy name: {e}"))
                })?;
                let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
                std::env::temp_dir().join(format!(
                    "unionid-rehearsal-{}-{nonce}.redb",
                    std::process::id()
                ))
            }
        };
        let mut input = File::open(source).map_err(io_error)?;
        input
            .try_lock()
            .map_err(|e| Error::new("E_BUSY", format!("lock rehearsal source: {e}")))?;
        let metadata = input.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(Error::new(
                "E_IO",
                "rehearsal source must be a regular file",
            ));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&path).map_err(io_error)?;
        let mut copy = Self {
            path,
            source_bytes: metadata.len(),
            keep: false,
        };
        let copied = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
        drop(output);
        copied.map_err(io_error)?;
        copy.keep = keep;
        Ok(copy)
    }
}

impl Drop for RehearsalCopy {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
