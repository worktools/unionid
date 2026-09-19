use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    /// A duplicate value under a whole-table unique index.
    Unique,
    /// A duplicate value under a partial unique index.
    PartialUnique,
    /// A duplicate value for a primary key.
    PrimaryKey,
    /// A mutation that requires a primary key on a table without one.
    PrimaryKeyMissing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    /// Stable, value-free classification for `E_CONSTRAINT` errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraint: Option<ConstraintKind>,
    /// Optional, value-free next step an agent or user can act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Error {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            span: None,
            constraint: None,
            hint: None,
        }
    }

    pub fn at(mut self, span: Span) -> Self {
        self.span.get_or_insert(span);
        self
    }

    /// Classify an `E_CONSTRAINT` error and attach the matching stable hint.
    pub fn constraint(mut self, constraint: ConstraintKind) -> Self {
        self.constraint = Some(constraint);
        self.hint = constraint.default_hint().map(ToOwned::to_owned);
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl ConstraintKind {
    /// Value-free hint text shared by the CLI and machine-readable responses.
    pub const fn default_hint(self) -> Option<&'static str> {
        match self {
            Self::Unique => Some(
                "the value already exists under a unique index; update the existing row or choose a different key",
            ),
            Self::PartialUnique => Some(
                "this partial unique index only constrains rows whose `if` predicate is true; soft-delete or move the row out of the predicate to release the key",
            ),
            Self::PrimaryKey => {
                Some("use `upsert` to replace the row that already owns this primary key")
            }
            Self::PrimaryKeyMissing => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(span) = self.span {
            write!(f, " (line {}, column {})", span.line, span.column)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
