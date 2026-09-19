use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::migration::MigrationFile;
use crate::{Engine, Error, Span};

pub const PROJECT_CHECK_VERSION: u32 = 1;
pub const MAX_PROJECT_FILES: usize = 256;
pub const MAX_PROJECT_ENTRIES: usize = 512;
pub const MAX_PROJECT_SOURCE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PROJECT_QUERY_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCheckPhase {
    Schema,
    Migrations,
    Queries,
}

impl ProjectCheckPhase {
    fn index(self) -> usize {
        match self {
            Self::Schema => 0,
            Self::Migrations => 1,
            Self::Queries => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCheckStatus {
    NotChecked,
    Passed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectCheckStage {
    pub phase: ProjectCheckPhase,
    pub status: ProjectCheckStatus,
    pub files: usize,
    pub source_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectCheckError {
    pub phase: ProjectCheckPhase,
    pub path: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    /// Optional, value-free next step carried over from the source error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectCheckReport {
    pub schema_version: u32,
    pub ok: bool,
    pub stages: [ProjectCheckStage; 3],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProjectCheckError>,
}

impl ProjectCheckReport {
    fn new() -> Self {
        Self {
            schema_version: PROJECT_CHECK_VERSION,
            ok: false,
            stages: [
                ProjectCheckStage {
                    phase: ProjectCheckPhase::Schema,
                    status: ProjectCheckStatus::NotChecked,
                    files: 0,
                    source_bytes: 0,
                },
                ProjectCheckStage {
                    phase: ProjectCheckPhase::Migrations,
                    status: ProjectCheckStatus::NotChecked,
                    files: 0,
                    source_bytes: 0,
                },
                ProjectCheckStage {
                    phase: ProjectCheckPhase::Queries,
                    status: ProjectCheckStatus::NotChecked,
                    files: 0,
                    source_bytes: 0,
                },
            ],
            error: None,
        }
    }

    fn fail(&mut self, failure: CheckFailure) {
        self.stages[failure.phase.index()].status = ProjectCheckStatus::Failed;
        let error = *failure.error;
        self.error = Some(ProjectCheckError {
            phase: failure.phase,
            path: failure.path,
            code: error.code,
            message: error.message,
            span: error.span,
            hint: error.hint,
        });
    }
}

struct CheckFailure {
    phase: ProjectCheckPhase,
    path: String,
    error: Box<Error>,
}

impl CheckFailure {
    fn new(phase: ProjectCheckPhase, path: impl Into<String>, error: Error) -> Self {
        Self {
            phase,
            path: path.into(),
            error: Box::new(error),
        }
    }
}

struct ProjectBudget {
    entries: usize,
    files: usize,
    bytes: usize,
    paths: BTreeSet<String>,
}

struct ProjectSource {
    relative: String,
    source: String,
}

/// Validate the conventional project schema, migration history, and static queries.
pub fn check(directory: impl AsRef<Path>) -> ProjectCheckReport {
    let mut report = ProjectCheckReport::new();
    let mut budget = ProjectBudget {
        entries: 0,
        files: 0,
        bytes: 0,
        paths: BTreeSet::new(),
    };
    let root = match checked_project_root(directory.as_ref()) {
        Ok(root) => root,
        Err(failure) => {
            report.fail(failure);
            return report;
        }
    };

    let schema = match check_schema_source(&root, &mut budget) {
        Ok(schema) => schema,
        Err(failure) => {
            report.fail(failure);
            return report;
        }
    };
    report.stages[0] = ProjectCheckStage {
        phase: ProjectCheckPhase::Schema,
        status: ProjectCheckStatus::Passed,
        files: 1,
        source_bytes: schema.source.len(),
    };

    let (migration_files, migration_bytes) = match check_migrations(&root, &schema, &mut budget) {
        Ok(migrations) => migrations,
        Err(failure) => {
            report.fail(failure);
            return report;
        }
    };
    report.stages[1] = ProjectCheckStage {
        phase: ProjectCheckPhase::Migrations,
        status: ProjectCheckStatus::Passed,
        files: migration_files,
        source_bytes: migration_bytes,
    };

    let (query_files, query_bytes) = match check_queries(&root, &schema.source, &mut budget) {
        Ok(queries) => queries,
        Err(failure) => {
            report.fail(failure);
            return report;
        }
    };
    report.stages[2] = ProjectCheckStage {
        phase: ProjectCheckPhase::Queries,
        status: ProjectCheckStatus::Passed,
        files: query_files,
        source_bytes: query_bytes,
    };
    report.ok = true;
    report
}

struct CheckedSchema {
    source: String,
    hash: String,
}

fn checked_project_root(directory: &Path) -> Result<PathBuf, CheckFailure> {
    let phase = ProjectCheckPhase::Schema;
    let metadata = std::fs::symlink_metadata(directory).map_err(|error| {
        CheckFailure::new(
            phase,
            ".",
            Error::new("E_CONFIG", format!("read project directory: {error}")),
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(CheckFailure::new(
            phase,
            ".",
            Error::new("E_CONFIG", "project path must be a real directory"),
        ));
    }
    std::fs::canonicalize(directory).map_err(|error| {
        CheckFailure::new(
            phase,
            ".",
            Error::new("E_CONFIG", format!("resolve project directory: {error}")),
        )
    })
}

fn check_schema_source(
    root: &Path,
    budget: &mut ProjectBudget,
) -> Result<CheckedSchema, CheckFailure> {
    let phase = ProjectCheckPhase::Schema;
    let source = read_project_source(root, Path::new("schema.unid"), phase, budget)?;
    require_canonical(&source, phase)?;
    let checked = Engine::check_schema(&source.source)
        .map_err(|error| CheckFailure::new(phase, &source.relative, error))?;
    Ok(CheckedSchema {
        source: source.source,
        hash: checked.schema.hash,
    })
}

fn check_migrations(
    root: &Path,
    schema: &CheckedSchema,
    budget: &mut ProjectBudget,
) -> Result<(usize, usize), CheckFailure> {
    let phase = ProjectCheckPhase::Migrations;
    let sources = collect_project_sources(root, Path::new("migrations"), false, phase, budget)?;
    if sources.is_empty() {
        return Err(CheckFailure::new(
            phase,
            "migrations",
            Error::new("E_MIGRATION", "migration directory contains no .unid files"),
        ));
    }
    let bytes = sources.iter().map(|source| source.source.len()).sum();
    let mut files = Vec::with_capacity(sources.len());
    for source in sources {
        require_canonical(&source, phase)?;
        let mut file = MigrationFile::parse(source.source)
            .map_err(|error| CheckFailure::new(phase, &source.relative, error))?;
        file.path = Some(PathBuf::from(&source.relative));
        files.push(file);
    }
    validate_project_migration_chain(&files)?;
    let plan = Engine::memory().plan_migrations(&files).map_err(|error| {
        let path = files
            .iter()
            .filter_map(|file| file.path.as_ref())
            .find(|path| error.message.contains(&path.display().to_string()))
            .map_or_else(
                || "migrations".to_owned(),
                |path| path.display().to_string(),
            );
        CheckFailure::new(phase, path, error)
    })?;
    if plan.target_schema.hash != schema.hash {
        return Err(CheckFailure::new(
            phase,
            "schema.unid",
            Error::new(
                "E_SCHEMA",
                format!(
                    "declarative schema hash {} does not match migration target {}",
                    schema.hash, plan.target_schema.hash
                ),
            )
            .at(Span { line: 1, column: 1 }),
        ));
    }
    Ok((files.len(), bytes))
}

fn validate_project_migration_chain(files: &[MigrationFile]) -> Result<(), CheckFailure> {
    let phase = ProjectCheckPhase::Migrations;
    let mut ids = BTreeSet::new();
    let mut expected_parent: Option<&str> = None;
    for file in files {
        let path = file.path.as_ref().map_or_else(
            || "migrations".to_owned(),
            |path| path.display().to_string(),
        );
        if !ids.insert(file.id.as_str()) {
            return Err(CheckFailure::new(
                phase,
                path,
                Error::new(
                    "E_MIGRATION",
                    format!("duplicate migration ID '{}'", file.id),
                )
                .at(Span { line: 1, column: 1 }),
            ));
        }
        if file.parent.as_deref() != expected_parent {
            return Err(CheckFailure::new(
                phase,
                path,
                Error::new(
                    "E_MIGRATION",
                    format!(
                        "migration '{}' expects parent {:?}, file order requires {:?}",
                        file.id, file.parent, expected_parent
                    ),
                )
                .at(Span { line: 1, column: 1 }),
            ));
        }
        expected_parent = Some(&file.id);
    }
    crate::migration::validate_files(files).map_err(|error| {
        CheckFailure::new(
            ProjectCheckPhase::Migrations,
            "migrations",
            error.at(Span { line: 1, column: 1 }),
        )
    })
}

fn check_queries(
    root: &Path,
    schema_source: &str,
    budget: &mut ProjectBudget,
) -> Result<(usize, usize), CheckFailure> {
    let phase = ProjectCheckPhase::Queries;
    let sources = collect_project_sources(root, Path::new("queries"), true, phase, budget)?;
    if sources.is_empty() {
        return Err(CheckFailure::new(
            phase,
            "queries",
            Error::new("E_QUERY", "query directory contains no .unid files"),
        ));
    }
    let bytes = sources.iter().map(|source| source.source.len()).sum();
    for source in &sources {
        require_canonical(source, phase)?;
        crate::query_contract::describe(schema_source, &source.source)
            .map_err(|error| CheckFailure::new(phase, &source.relative, error))?;
    }
    Ok((sources.len(), bytes))
}

fn require_canonical(source: &ProjectSource, phase: ProjectCheckPhase) -> Result<(), CheckFailure> {
    let formatted = crate::format_source(&source.source)
        .map_err(|error| CheckFailure::new(phase, &source.relative, error))?;
    if formatted != source.source {
        return Err(CheckFailure::new(
            phase,
            &source.relative,
            Error::new("E_INPUT", "source is not canonically formatted")
                .at(first_difference_span(&source.source, &formatted)),
        ));
    }
    Ok(())
}

fn first_difference_span(source: &str, formatted: &str) -> Span {
    let mut line = 1;
    let mut column = 1;
    for (actual, expected) in source.chars().zip(formatted.chars()) {
        if actual != expected {
            break;
        }
        if actual == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    Span { line, column }
}

fn collect_project_sources(
    root: &Path,
    relative_directory: &Path,
    recursive: bool,
    phase: ProjectCheckPhase,
    budget: &mut ProjectBudget,
) -> Result<Vec<ProjectSource>, CheckFailure> {
    fn visit(
        root: &Path,
        relative_directory: &Path,
        recursive: bool,
        depth: usize,
        phase: ProjectCheckPhase,
        budget: &mut ProjectBudget,
        output: &mut Vec<ProjectSource>,
    ) -> Result<(), CheckFailure> {
        if depth > MAX_PROJECT_QUERY_DEPTH {
            return Err(CheckFailure::new(
                phase,
                relative_string(relative_directory, phase)?,
                Error::new(
                    "E_LIMIT",
                    format!("project query depth exceeds {MAX_PROJECT_QUERY_DEPTH}"),
                ),
            ));
        }
        let directory = root.join(relative_directory);
        let metadata = std::fs::symlink_metadata(&directory).map_err(|error| {
            CheckFailure::new(
                phase,
                relative_string(relative_directory, phase).unwrap_or_else(|_| ".".into()),
                Error::new("E_CONFIG", format!("read project directory: {error}")),
            )
        })?;
        if !metadata.file_type().is_dir() {
            return Err(CheckFailure::new(
                phase,
                relative_string(relative_directory, phase)?,
                Error::new("E_CONFIG", "project path must be a real directory"),
            ));
        }
        let canonical = std::fs::canonicalize(&directory).map_err(|error| {
            CheckFailure::new(
                phase,
                relative_string(relative_directory, phase).unwrap_or_else(|_| ".".into()),
                Error::new("E_CONFIG", format!("resolve project directory: {error}")),
            )
        })?;
        if !canonical.starts_with(root) {
            return Err(CheckFailure::new(
                phase,
                relative_string(relative_directory, phase)?,
                Error::new("E_CONFIG", "project path escapes the project directory"),
            ));
        }
        let mut entries = std::fs::read_dir(&directory)
            .map_err(|error| {
                CheckFailure::new(
                    phase,
                    relative_string(relative_directory, phase).unwrap_or_else(|_| ".".into()),
                    Error::new("E_IO", format!("read project directory: {error}")),
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                CheckFailure::new(
                    phase,
                    relative_string(relative_directory, phase).unwrap_or_else(|_| ".".into()),
                    Error::new("E_IO", format!("read project entry: {error}")),
                )
            })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let relative = relative_directory.join(entry.file_name());
            let relative_text = relative_string(&relative, phase)?;
            if budget.entries >= MAX_PROJECT_ENTRIES {
                return Err(CheckFailure::new(
                    phase,
                    relative_text,
                    Error::new(
                        "E_LIMIT",
                        format!("project contains more than {MAX_PROJECT_ENTRIES} entries"),
                    ),
                ));
            }
            budget.entries += 1;
            let file_type = entry.file_type().map_err(|error| {
                CheckFailure::new(
                    phase,
                    &relative_text,
                    Error::new("E_IO", format!("inspect project entry: {error}")),
                )
            })?;
            if file_type.is_dir() && recursive {
                visit(root, &relative, recursive, depth + 1, phase, budget, output)?;
                continue;
            }
            if !file_type.is_file()
                || relative.extension().and_then(|value| value.to_str()) != Some("unid")
            {
                return Err(CheckFailure::new(
                    phase,
                    relative_text,
                    Error::new(
                        "E_CONFIG",
                        "project source directories accept only regular .unid files",
                    ),
                ));
            }
            output.push(read_project_source(root, &relative, phase, budget)?);
        }
        Ok(())
    }

    let mut output = Vec::new();
    visit(
        root,
        relative_directory,
        recursive,
        0,
        phase,
        budget,
        &mut output,
    )?;
    Ok(output)
}

fn read_project_source(
    root: &Path,
    relative: &Path,
    phase: ProjectCheckPhase,
    budget: &mut ProjectBudget,
) -> Result<ProjectSource, CheckFailure> {
    let relative_text = relative_string(relative, phase)?;
    if !budget.paths.insert(relative_text.clone()) {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new("E_CONFIG", "duplicate normalized project path"),
        ));
    }
    if budget.files >= MAX_PROJECT_FILES {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new(
                "E_LIMIT",
                format!("project contains more than {MAX_PROJECT_FILES} source files"),
            ),
        ));
    }
    let path = root.join(relative);
    let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
        CheckFailure::new(
            phase,
            &relative_text,
            Error::new("E_CONFIG", format!("read project source: {error}")),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new("E_CONFIG", "project source must be a regular file"),
        ));
    }
    if metadata.len() > crate::syntax::MAX_SOURCE_BYTES as u64 {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new(
                "E_LIMIT",
                format!(
                    "project source exceeds {} bytes",
                    crate::syntax::MAX_SOURCE_BYTES
                ),
            ),
        ));
    }
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        CheckFailure::new(
            phase,
            &relative_text,
            Error::new("E_CONFIG", format!("resolve project source: {error}")),
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new("E_CONFIG", "project source escapes the project directory"),
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(|error| {
            CheckFailure::new(
                phase,
                &relative_text,
                Error::new("E_IO", format!("open project source: {error}")),
            )
        })?
        .take((crate::syntax::MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CheckFailure::new(
                phase,
                &relative_text,
                Error::new("E_IO", format!("read project source: {error}")),
            )
        })?;
    if bytes.len() > crate::syntax::MAX_SOURCE_BYTES {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new(
                "E_LIMIT",
                format!(
                    "project source exceeds {} bytes",
                    crate::syntax::MAX_SOURCE_BYTES
                ),
            ),
        ));
    }
    if budget.bytes.saturating_add(bytes.len()) > MAX_PROJECT_SOURCE_BYTES {
        return Err(CheckFailure::new(
            phase,
            &relative_text,
            Error::new(
                "E_LIMIT",
                format!("project source exceeds {MAX_PROJECT_SOURCE_BYTES} total bytes"),
            ),
        ));
    }
    let source = String::from_utf8(bytes).map_err(|_| {
        CheckFailure::new(
            phase,
            &relative_text,
            Error::new("E_INPUT", "project source must be UTF-8"),
        )
    })?;
    budget.files += 1;
    budget.bytes += source.len();
    Ok(ProjectSource {
        relative: relative_text,
        source,
    })
}

fn relative_string(path: &Path, phase: ProjectCheckPhase) -> Result<String, CheckFailure> {
    let value = path.to_str().ok_or_else(|| {
        CheckFailure::new(
            phase,
            ".",
            Error::new("E_CONFIG", "project paths must be UTF-8"),
        )
    })?;
    Ok(value.replace(std::path::MAIN_SEPARATOR, "/"))
}
