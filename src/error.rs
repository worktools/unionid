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
}

impl Error {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            span: None,
            constraint: None,
        }
    }

    pub fn at(mut self, span: Span) -> Self {
        self.span.get_or_insert(span);
        self
    }

    pub fn constraint(mut self, constraint: ConstraintKind) -> Self {
        self.constraint = Some(constraint);
        self
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
