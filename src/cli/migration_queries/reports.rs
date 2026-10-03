//! Typed report selection, independent of migration/copy ownership.
use super::{CommandError, Output, QueryRehearsal};
use crate::migration::query_validation::{
    MigrationQuery, MigrationQueryApplyV2, MigrationQueryErrorV2, MigrationQueryPlanV2,
    QueryValidation, QueryValidationV2,
};
use crate::migration::{MigrationApply, MigrationFile, MigrationPlan};
use crate::{Engine, Error};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub enum ReportMode {
    Full,
    Compact,
}

#[derive(Clone, Serialize)]
#[serde(untagged)]
pub enum Validation {
    Full(Box<QueryValidation>),
    Compact(Box<QueryValidationV2>),
}

impl Validation {
    pub(super) fn valid(&self) -> bool {
        match self {
            Self::Full(v) => v.valid,
            Self::Compact(v) => v.valid,
        }
    }
    pub fn print_with_details(&self, verbose: bool) {
        match self {
            Self::Full(v) => super::print_validation_with_details(v, verbose),
            Self::Compact(v) => print_compact(v, verbose),
        }
    }
}

pub struct ReportCommandError {
    pub error: Box<Error>,
    pub query_validation: Option<Validation>,
    pub retained_copy: Option<PathBuf>,
}
impl From<Error> for ReportCommandError {
    fn from(error: Error) -> Self {
        Self {
            error: Box::new(error),
            query_validation: None,
            retained_copy: None,
        }
    }
}
impl From<CommandError> for ReportCommandError {
    fn from(error: CommandError) -> Self {
        Self {
            error: error.error,
            query_validation: error.query_validation.map(Validation::Full),
            retained_copy: error.retained_copy,
        }
    }
}
impl From<MigrationQueryErrorV2> for ReportCommandError {
    fn from(error: MigrationQueryErrorV2) -> Self {
        Self {
            error: error.error,
            query_validation: error.query_validation.map(Validation::Compact),
            retained_copy: None,
        }
    }
}
impl ReportCommandError {
    pub(super) fn into_full(self) -> CommandError {
        CommandError {
            error: self.error,
            query_validation: self.query_validation.map(|v| match v {
                Validation::Full(v) => v,
                Validation::Compact(_) => unreachable!("full report mode"),
            }),
            retained_copy: self.retained_copy,
        }
    }
}

#[derive(Serialize)]
pub struct CompactRehearsal {
    #[serde(flatten)]
    pub rehearsal: crate::cli::MigrationRehearsal,
    pub query_validation: QueryValidationV2,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retained_copy: Option<PathBuf>,
}
#[derive(Serialize)]
#[serde(untagged)]
pub enum CompactOutput {
    Plan(Box<MigrationQueryPlanV2>),
    Apply(Box<MigrationQueryApplyV2>),
    Rehearse(Box<CompactRehearsal>),
}
#[derive(Serialize)]
#[serde(untagged)]
pub enum ReportOutput {
    Full(Output),
    Compact(CompactOutput),
}

impl ReportMode {
    pub(super) fn plan(
        self,
        engine: &Engine,
        files: &[MigrationFile],
        queries: &[MigrationQuery],
    ) -> Result<(MigrationPlan, Validation), ReportCommandError> {
        match self {
            Self::Full => {
                let p = engine.plan_migrations_with_queries(files, queries)?;
                Ok((p.plan, Validation::Full(Box::new(p.query_validation))))
            }
            Self::Compact => {
                let p = engine.plan_migrations_with_queries_v2(files, queries)?;
                Ok((p.plan, Validation::Compact(Box::new(p.query_validation))))
            }
        }
    }
    pub(super) fn apply(
        self,
        engine: &mut Engine,
        files: &[MigrationFile],
        queries: &[MigrationQuery],
    ) -> Result<(MigrationApply, Validation), ReportCommandError> {
        match self {
            Self::Full => {
                let p = engine
                    .apply_migrations_with_queries(files, queries)
                    .map_err(CommandError::from)?;
                Ok((p.applied, Validation::Full(Box::new(p.query_validation))))
            }
            Self::Compact => {
                let p = engine.apply_migrations_with_queries_v2(files, queries)?;
                Ok((p.applied, Validation::Compact(Box::new(p.query_validation))))
            }
        }
    }
}

impl ReportOutput {
    pub(super) fn plan(plan: MigrationPlan, validation: Validation) -> Self {
        match validation {
            Validation::Full(v) => Self::Full(Output::Plan(Box::new(
                crate::migration::query_validation::MigrationQueryPlan {
                    plan,
                    query_validation: *v,
                },
            ))),
            Validation::Compact(v) => {
                Self::Compact(CompactOutput::Plan(Box::new(MigrationQueryPlanV2 {
                    plan,
                    query_validation: *v,
                })))
            }
        }
    }
    pub(super) fn apply(applied: MigrationApply, validation: Validation) -> Self {
        match validation {
            Validation::Full(v) => Self::Full(Output::Apply(Box::new(
                crate::migration::query_validation::MigrationQueryApply {
                    applied,
                    query_validation: *v,
                },
            ))),
            Validation::Compact(v) => {
                Self::Compact(CompactOutput::Apply(Box::new(MigrationQueryApplyV2 {
                    applied,
                    query_validation: *v,
                })))
            }
        }
    }
    pub(super) fn rehearse(
        rehearsal: crate::cli::MigrationRehearsal,
        validation: Validation,
        retained_copy: Option<PathBuf>,
    ) -> Self {
        match validation {
            Validation::Full(v) => Self::Full(Output::Rehearse(Box::new(QueryRehearsal {
                rehearsal,
                query_validation: *v,
                retained_copy,
            }))),
            Validation::Compact(v) => {
                Self::Compact(CompactOutput::Rehearse(Box::new(CompactRehearsal {
                    rehearsal,
                    query_validation: *v,
                    retained_copy,
                })))
            }
        }
    }
    pub(super) fn into_full(self) -> Output {
        match self {
            Self::Full(output) => output,
            Self::Compact(_) => unreachable!("full report mode"),
        }
    }
    pub fn print_with_details(&self, json: bool, verbose: bool) -> Result<(), String> {
        if let Self::Full(output) = self {
            return output.print_with_details(json, verbose);
        }
        if json {
            println!(
                "{}",
                serde_json::to_string(self).map_err(|e| e.to_string())?
            );
            return Ok(());
        }
        let Self::Compact(output) = self else {
            unreachable!()
        };
        let validation = match output {
            CompactOutput::Plan(p) => {
                super::super::print_migration_plan(&p.plan, false)?;
                &p.query_validation
            }
            CompactOutput::Apply(p) => {
                super::super::print_migration_apply(&p.applied, false)?;
                &p.query_validation
            }
            CompactOutput::Rehearse(p) => {
                super::super::print_migration_rehearsal(&p.rehearsal, false)?;
                if let Some(path) = &p.retained_copy {
                    println!("retained copy: {}", path.display());
                }
                &p.query_validation
            }
        };
        print_compact(validation, verbose);
        Ok(())
    }
}

fn print_compact(report: &QueryValidationV2, verbose: bool) {
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
        let count: usize = file
            .failures
            .iter()
            .map(|f| f.last_checkpoint - f.first_checkpoint + 1)
            .sum();
        let print = |index: usize, error: &Error| {
            let checkpoint = &report.checkpoints[index];
            println!(
                "  {} (schema {}): {}",
                checkpoint.migration_id.as_deref().unwrap_or("current"),
                checkpoint.schema.revision,
                error
            );
        };
        if verbose {
            for failure in &file.failures {
                for index in failure.first_checkpoint..=failure.last_checkpoint {
                    print(index, &failure.error);
                }
            }
        } else {
            if let Some(first) = file.failures.first() {
                print(first.first_checkpoint, &first.error);
            }
            if count > 1 {
                let last = file.failures.last().unwrap();
                print(last.last_checkpoint, &last.error);
            }
            if count > 2 {
                println!(
                    "  {} intermediate checkpoint failures omitted; use --verbose or --format json for the complete trace",
                    count - 2
                );
            }
        }
    }
    println!(
        "Generated clients still require the exact schema hash; regenerate before deployment."
    );
}
