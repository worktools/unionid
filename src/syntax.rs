//! Lexer and layout-aware parser. Newlines inside strings are never separators.
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, Span};
use crate::model::{Column, EnumType, EnumValue, EnumVariantDef, MAX_DEPTH, ScalarType, Value};
use crate::query::{
    Aggregate, AggregateAssignment, AggregateFunction, ArithmeticOp, BoolExpression, CmpOp,
    DeriveExpression, DeriveMatch, LocalBinding, LocalParameter, LocatedStatement, MatchArm,
    MatchField, MatchPattern, MatchPayload, MatchPredicate, MatchValue, MatchValueArm,
    MatchValueField, MatchValuePayload, MigrationTransform, PageDirection, PageSpec, Pipeline,
    Returning, ScalarExpression, SchemaMigration, SetAssignment, SetValue, SortKey, Stage,
    Statement,
};

pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

/// Syntactic readiness of a source buffer, without schema or type checking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "error", rename_all = "snake_case")]
pub enum InputStatus {
    /// The buffer parses as a complete script and can be submitted.
    Complete,
    /// More source can complete the buffer, for example an indented block or delimiter.
    Incomplete(Error),
    /// The buffer is already invalid and appending source cannot repair it.
    Invalid(Error),
}

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Ident(String),
    Number(String),
    Temporal(String),
    Text(String),
    Open(char),
    Close(char),
    Comma,
    Colon,
    Dot,
    Pipe,
    Plus,
    Minus,
    Star,
    Slash,
    Parameter(String),
    Op(String),
    Newline,
    Indent,
    Dedent,
    End,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    span: Span,
}

struct LexOutput {
    tokens: Vec<Token>,
    unclosed: Option<(char, Span)>,
}

fn syntax(message: impl Into<String>, span: Span) -> Error {
    Error::new("E_SYNTAX", message).at(span)
}

fn lex_source(source: &str) -> Result<LexOutput> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Error::new("E_LIMIT", "source exceeds 1 MiB"));
    }
    let mut tokens = Vec::new();
    let mut indentation = vec![0];
    let mut brackets = Vec::new();
    let mut last_line = 1;
    for (line_index, line) in source.lines().enumerate() {
        let line_number = line_index + 1;
        last_line = line_number;
        let chars = line.chars().collect::<Vec<_>>();
        let mut pos = 0;
        while pos < chars.len() && chars[pos] == ' ' {
            pos += 1;
        }
        if pos < chars.len() && chars[pos] == '\t' {
            return Err(syntax(
                "use spaces for indentation, not tabs",
                Span {
                    line: line_number,
                    column: pos + 1,
                },
            ));
        }
        if pos == chars.len() || chars[pos] == '#' {
            continue;
        }
        let span = Span {
            line: line_number,
            column: pos + 1,
        };
        if brackets.is_empty() {
            if pos > *indentation.last().unwrap() {
                if indentation.len() >= MAX_DEPTH {
                    return Err(Error::new("E_LIMIT", "indentation is too deep").at(span));
                }
                indentation.push(pos);
                tokens.push(Token {
                    kind: Kind::Indent,
                    span,
                });
            } else {
                while pos < *indentation.last().unwrap() {
                    indentation.pop();
                    tokens.push(Token {
                        kind: Kind::Dedent,
                        span,
                    });
                }
                if pos != *indentation.last().unwrap() {
                    return Err(syntax(
                        "dedent does not match an earlier indentation level",
                        span,
                    ));
                }
            }
        }
        while pos < chars.len() {
            let ch = chars[pos];
            if ch == '#' {
                break;
            }
            if ch.is_whitespace() {
                pos += 1;
                continue;
            }
            let start = pos;
            let span = Span {
                line: line_number,
                column: start + 1,
            };
            let kind = match ch {
                '@' => {
                    pos += 1;
                    let value_start = pos;
                    while pos < chars.len()
                        && (chars[pos].is_ascii_alphanumeric()
                            || matches!(chars[pos], '-' | ':' | '+' | '.'))
                    {
                        pos += 1;
                    }
                    if pos == value_start {
                        return Err(syntax("temporal literal requires a value after '@'", span));
                    }
                    Kind::Temporal(chars[value_start..pos].iter().collect())
                }
                '"' => {
                    pos += 1;
                    let mut escaped = false;
                    let mut closed = false;
                    while pos < chars.len() {
                        let c = chars[pos];
                        pos += 1;
                        if escaped {
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == '"' {
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return Err(syntax(
                            "unterminated string; use \\n for a newline inside a string",
                            span,
                        ));
                    }
                    let literal: String = chars[start..pos].iter().collect();
                    let value = serde_json::from_str::<String>(&literal)
                        .map_err(|e| syntax(format!("invalid string escape: {e}"), span))?;
                    Kind::Text(value)
                }
                c if c.is_ascii_digit()
                    || (c == '-' && chars.get(pos + 1).is_some_and(char::is_ascii_digit)) =>
                {
                    pos += 1;
                    while pos < chars.len()
                        && (chars[pos].is_ascii_alphanumeric()
                            || matches!(chars[pos], '.' | '+' | '-' | '_'))
                    {
                        pos += 1;
                    }
                    Kind::Number(chars[start..pos].iter().collect())
                }
                c if c.is_ascii_alphabetic() || c == '_' => {
                    pos += 1;
                    while pos < chars.len()
                        && (chars[pos].is_ascii_alphanumeric() || chars[pos] == '_')
                    {
                        pos += 1;
                    }
                    Kind::Ident(chars[start..pos].iter().collect())
                }
                '{' | '(' | '[' => {
                    if brackets.len() >= MAX_DEPTH {
                        return Err(
                            Error::new("E_LIMIT", "brackets are too deeply nested").at(span)
                        );
                    }
                    brackets.push((ch, span));
                    pos += 1;
                    Kind::Open(ch)
                }
                '}' | ')' | ']' => {
                    let matching = match ch {
                        '}' => '{',
                        ')' => '(',
                        _ => '[',
                    };
                    if !matches!(brackets.pop(), Some((open, _)) if open == matching) {
                        return Err(syntax("unmatched closing bracket", span));
                    }
                    pos += 1;
                    Kind::Close(ch)
                }
                '=' | '!' | '>' | '<' => {
                    pos += 1;
                    if (ch == '=' && chars.get(pos) == Some(&'>')) || chars.get(pos) == Some(&'=') {
                        pos += 1;
                    }
                    Kind::Op(chars[start..pos].iter().collect())
                }
                ',' => {
                    pos += 1;
                    Kind::Comma
                }
                ':' => {
                    pos += 1;
                    Kind::Colon
                }
                '.' => {
                    pos += 1;
                    Kind::Dot
                }
                '|' => {
                    pos += 1;
                    Kind::Pipe
                }
                '+' => {
                    pos += 1;
                    Kind::Plus
                }
                '-' => {
                    pos += 1;
                    if chars.get(pos) == Some(&'>') {
                        pos += 1;
                        Kind::Op("->".into())
                    } else {
                        Kind::Minus
                    }
                }
                '*' => {
                    pos += 1;
                    Kind::Star
                }
                '/' => {
                    pos += 1;
                    Kind::Slash
                }
                '$' => {
                    pos += 1;
                    let name_start = pos;
                    if !chars
                        .get(pos)
                        .is_some_and(|ch| ch.is_ascii_alphabetic() || *ch == '_')
                    {
                        return Err(syntax(
                            "parameter names must start with a letter or underscore",
                            span,
                        ));
                    }
                    pos += 1;
                    while pos < chars.len()
                        && (chars[pos].is_ascii_alphanumeric() || chars[pos] == '_')
                    {
                        pos += 1;
                    }
                    Kind::Parameter(chars[name_start..pos].iter().collect())
                }
                ';' => {
                    return Err(syntax(
                        "semicolons are not used; put the next statement on a new line",
                        span,
                    ));
                }
                _ => return Err(syntax(format!("unexpected character '{ch}'"), span)),
            };
            tokens.push(Token { kind, span });
            if tokens.len() > 100_000 {
                return Err(Error::new("E_LIMIT", "too many tokens").at(span));
            }
        }
        tokens.push(Token {
            kind: Kind::Newline,
            span: Span {
                line: line_number,
                column: chars.len() + 1,
            },
        });
    }
    let unclosed = brackets.last().copied();
    let end = Span {
        line: last_line + 1,
        column: 1,
    };
    if unclosed.is_none() {
        for _ in 1..indentation.len() {
            tokens.push(Token {
                kind: Kind::Dedent,
                span: end,
            });
        }
    }
    tokens.push(Token {
        kind: Kind::End,
        span: end,
    });
    Ok(LexOutput { tokens, unclosed })
}

fn lex(source: &str) -> Result<Vec<Token>> {
    let output = lex_source(source)?;
    if let Some((_, span)) = output.unclosed {
        Err(syntax("unclosed bracket", span))
    } else {
        Ok(output.tokens)
    }
}

pub fn parse(source: &str) -> Result<Vec<LocatedStatement>> {
    Parser {
        tokens: lex(source)?,
        pos: 0,
        needs_more: Cell::new(false),
    }
    .script()
}

/// Classify whether `source` is complete, can be continued, or is already invalid.
///
/// This uses lexer layout/delimiter state and parser EOF state. It does not open a
/// database or resolve names and types.
pub fn input_status(source: &str) -> InputStatus {
    if source.trim().is_empty() {
        return InputStatus::Complete;
    }
    let output = match lex_source(source) {
        Ok(output) => output,
        Err(error) => return InputStatus::Invalid(error),
    };
    let unclosed = output.unclosed;
    if !output.tokens.iter().any(|token| {
        !matches!(
            token.kind,
            Kind::Newline | Kind::Indent | Kind::Dedent | Kind::End
        )
    }) {
        return InputStatus::Complete;
    }
    let mut parser = Parser {
        tokens: output.tokens,
        pos: 0,
        needs_more: Cell::new(false),
    };
    match parser.script() {
        Ok(_) => InputStatus::Complete,
        Err(error) if parser.needs_more.get() => {
            let error = if let Some((open, span)) = unclosed {
                Error::new("E_INCOMPLETE", format!("unclosed '{open}' delimiter")).at(span)
            } else {
                Error {
                    code: "E_INCOMPLETE".into(),
                    message: error.message,
                    span: error.span,
                }
            };
            InputStatus::Incomplete(error)
        }
        Err(error) => InputStatus::Invalid(error),
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    needs_more: Cell<bool>,
}

impl Parser {
    fn token(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn kind(&self) -> &Kind {
        &self.token().kind
    }
    fn error(&self, message: impl Into<String>) -> Error {
        let end_span = self.tokens.last().map(|token| token.span);
        if end_span == Some(self.token().span) && matches!(self.kind(), Kind::Dedent | Kind::End) {
            self.needs_more.set(true);
        }
        syntax(message, self.token().span)
    }
    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if t.kind != Kind::End {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, kind: Kind) -> bool {
        if *self.kind() == kind {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, kind: Kind) -> Result<()> {
        if self.eat(kind.clone()) {
            Ok(())
        } else {
            Err(self.error(format!("expected {kind:?}")))
        }
    }
    fn word(&self, word: &str) -> bool {
        matches!(self.kind(), Kind::Ident(s) if s.eq_ignore_ascii_case(word))
    }
    fn parenthesized_match_starts(&self) -> bool {
        if *self.kind() != Kind::Open('(') {
            return false;
        }
        self.tokens[self.pos + 1..]
            .iter()
            .find(|token| token.kind != Kind::Newline)
            .is_some_and(|token| matches!(&token.kind, Kind::Ident(word) if word.eq_ignore_ascii_case("match")))
    }
    fn expect_word(&mut self, word: &str) -> Result<()> {
        if self.word(word) {
            self.bump();
            Ok(())
        } else {
            Err(self.error(format!("expected '{word}'")))
        }
    }
    fn identifier(&mut self) -> Result<String> {
        match self.kind().clone() {
            Kind::Ident(s) => {
                self.bump();
                Ok(s)
            }
            _ => Err(self.error("expected an identifier")),
        }
    }
    fn newlines(&mut self) {
        while self.eat(Kind::Newline) {}
    }
    fn depth(&self, depth: usize) -> Result<()> {
        if depth >= MAX_DEPTH {
            Err(Error::new("E_LIMIT", "type/value nesting is too deep").at(self.token().span))
        } else {
            Ok(())
        }
    }
    fn block(&mut self) -> Result<()> {
        self.expect(Kind::Newline)?;
        self.newlines();
        self.expect(Kind::Indent)
    }
    fn path(&mut self) -> Result<String> {
        let mut p = self.identifier()?;
        while self.eat(Kind::Dot) {
            p.push('.');
            p.push_str(&self.identifier()?);
        }
        Ok(p)
    }

    fn script(&mut self) -> Result<Vec<LocatedStatement>> {
        let mut out = Vec::new();
        self.newlines();
        while *self.kind() != Kind::End {
            let span = self.token().span;
            let statement = if self.word("type") {
                self.define_type()?
            } else if self.word("table") {
                self.table()?
            } else if self.word("create") {
                self.create()?
            } else if self.word("insert") {
                self.insert()?
            } else if self.word("upsert") {
                self.upsert()?
            } else if self.word("update") {
                self.update()?
            } else if self.word("delete") {
                self.delete()?
            } else if self.word("migration") {
                self.migration()?
            } else if self.word("explain") {
                self.explain()?
            } else if self.word("from") {
                self.pipeline()?
            } else {
                return Err(self.error(
                    "expected type / table / insert / upsert / update / delete / migration / explain / from (or legacy create table/index)",
                ));
            };
            out.push(LocatedStatement { statement, span });
            // A consumed layout block already established a physical statement boundary.
            if !matches!(self.kind(), Kind::Newline | Kind::End)
                && !matches!(
                    self.tokens[self.pos.saturating_sub(1)].kind,
                    Kind::Newline | Kind::Dedent
                )
            {
                return Err(
                    self.error("unexpected trailing input; separate statements with a newline")
                );
            }
            self.newlines();
        }
        if out.is_empty() {
            return Err(self.error("empty script"));
        }
        Ok(out)
    }

    fn explain(&mut self) -> Result<Statement> {
        self.expect_word("explain")?;
        let nested = *self.kind() == Kind::Newline;
        if nested {
            self.block()?;
        }
        let Statement::Pipeline(pipeline) = self.pipeline()? else {
            unreachable!("pipeline parser must return a pipeline statement")
        };
        if nested {
            self.expect(Kind::Dedent)?;
        }
        Ok(Statement::Explain(pipeline))
    }

    fn define_type(&mut self) -> Result<Statement> {
        self.expect_word("type")?;
        let name = self.identifier()?;
        if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
            return Err(self.error("type names must start with an uppercase letter"));
        }
        self.expect(Kind::Op("=".into()))?;
        let ty = if *self.kind() == Kind::Newline {
            self.block()?;
            if *self.kind() == Kind::Pipe
                || matches!(self.kind(), Kind::Ident(s) if s.starts_with(|c: char| c.is_ascii_uppercase()))
            {
                let ty = self.variants(true, false, 0)?;
                self.expect(Kind::Dedent)?;
                ty
            } else {
                ScalarType::Record(self.fields(Kind::Dedent, 0)?)
            }
        } else if *self.kind() == Kind::Pipe
            || (matches!(self.kind(), Kind::Ident(s) if s.starts_with(|c: char| c.is_ascii_uppercase()))
                && matches!(
                    self.tokens.get(self.pos + 1).map(|t| &t.kind),
                    Some(Kind::Pipe | Kind::Open('{') | Kind::Open('('))
                ))
        {
            self.variants(false, false, 0)?
        } else {
            self.ty(0)?
        };
        Ok(Statement::DefineType { name, ty })
    }

    fn fields(&mut self, end: Kind, depth: usize) -> Result<Vec<Column>> {
        self.depth(depth)?;
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        self.newlines();
        while *self.kind() != end {
            let name = self.identifier()?;
            if !seen.insert(name.clone()) {
                return Err(self.error(format!("duplicate field '{name}'")));
            }
            let ty = self.ty(depth + 1)?;
            let default = if self.eat(Kind::Op("=".into())) {
                Some(if *self.kind() == Kind::Newline {
                    self.block()?;
                    self.record(Kind::Dedent, depth + 1)?
                } else {
                    self.value(depth + 1)?
                })
            } else {
                None
            };
            out.push(Column {
                name,
                ty,
                default,
                id: 0,
            });
            if *self.kind() == end {
                break;
            }
            let after_block = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
            if !self.eat(Kind::Comma) && !self.eat(Kind::Newline) && !after_block {
                return Err(self.error("expected a newline or comma between fields"));
            }
            self.newlines();
        }
        self.expect(end)?;
        if out.is_empty() {
            return Err(self.error("record type requires at least one field"));
        }
        Ok(out)
    }

    fn ty(&mut self, depth: usize) -> Result<ScalarType> {
        self.depth(depth)?;
        if self.eat(Kind::Open('{')) {
            return Ok(ScalarType::Record(
                self.fields(Kind::Close('}'), depth + 1)?,
            ));
        }
        if self.eat(Kind::Open('(')) {
            self.newlines();
            let first = self.ty(depth + 1)?;
            if self.eat(Kind::Comma) {
                let mut ts = vec![first];
                self.newlines();
                while *self.kind() != Kind::Close(')') {
                    ts.push(self.ty(depth + 1)?);
                    self.newlines();
                    if !self.eat(Kind::Comma) {
                        break;
                    }
                    self.newlines();
                }
                self.expect(Kind::Close(')'))?;
                return Ok(ScalarType::Tuple(ts));
            }
            self.newlines();
            self.expect(Kind::Close(')'))?;
            return Ok(first);
        }
        let name = self.identifier()?;
        Ok(match name.as_str() {
            "int" | "i64" | "integer" => ScalarType::Int,
            "float" | "f64" | "double" => ScalarType::Float,
            "bool" | "boolean" => ScalarType::Bool,
            "text" | "string" => ScalarType::Text,
            "uuid" => ScalarType::Uuid,
            "date" => ScalarType::Date,
            "timestamp" => ScalarType::Timestamp,
            "duration" => ScalarType::Duration,
            "bytes" => ScalarType::Bytes,
            "option" => ScalarType::Option(Box::new(self.ty(depth + 1)?)),
            "list" => ScalarType::List(Box::new(self.ty(depth + 1)?)),
            "enum" => {
                self.expect(Kind::Open('('))?;
                let ty = self.variants(false, true, depth + 1)?;
                self.expect(Kind::Close(')'))?;
                ty
            }
            _ => ScalarType::Named(name),
        })
    }

    fn variants(&mut self, layout: bool, legacy: bool, depth: usize) -> Result<ScalarType> {
        self.depth(depth)?;
        let mut variants = Vec::new();
        let mut seen = BTreeSet::new();
        self.eat(Kind::Pipe);
        self.newlines();
        loop {
            let name = self.identifier()?;
            if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
                return Err(self.error("variant names must start with an uppercase letter"));
            }
            if !seen.insert(name.clone()) {
                return Err(self.error(format!("duplicate variant '{name}'")));
            }
            let mut args = Vec::new();
            if self.eat(Kind::Open('(')) {
                self.newlines();
                while *self.kind() != Kind::Close(')') {
                    args.push(self.ty(depth + 1)?);
                    self.newlines();
                    if !self.eat(Kind::Comma) {
                        break;
                    }
                    self.newlines();
                }
                self.expect(Kind::Close(')'))?;
            } else if matches!(self.kind(), Kind::Open('{') | Kind::Ident(_)) {
                args.push(self.ty(depth + 1)?);
            }
            if layout && *self.kind() == Kind::Newline {
                self.newlines();
                if self.eat(Kind::Indent) {
                    if !args.is_empty() {
                        return Err(self.error("variant already has an inline payload"));
                    }
                    args.push(ScalarType::Record(self.fields(Kind::Dedent, depth + 1)?));
                }
            }
            variants.push(EnumVariantDef { name, args, id: 0 });
            if legacy {
                if !self.eat(Kind::Comma) {
                    break;
                }
                self.newlines();
                if *self.kind() == Kind::Close(')') {
                    break;
                }
            } else {
                if layout {
                    self.newlines();
                }
                if !self.eat(Kind::Pipe) {
                    break;
                }
            }
        }
        Ok(ScalarType::Enum(EnumType { variants }))
    }

    fn table(&mut self) -> Result<Statement> {
        self.expect_word("table")?;
        let table = self.identifier()?;
        let row_type = self.identifier()?;
        let mut key = None;
        if *self.kind() == Kind::Newline {
            self.newlines();
            if self.eat(Kind::Indent) {
                self.expect_word("key")?;
                key = Some(self.identifier()?);
                self.newlines();
                self.expect(Kind::Dedent)?;
            }
        }
        Ok(Statement::TypedTable {
            table,
            row_type,
            key,
        })
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_word("create")?;
        if self.word("table") {
            self.bump();
            let table = self.identifier()?;
            self.expect(Kind::Open('('))?;
            let columns = self.fields(Kind::Close(')'), 0)?;
            Ok(Statement::CreateTable { table, columns })
        } else {
            let unique = if self.word("unique") {
                self.bump();
                true
            } else {
                false
            };
            self.expect_word("index")?;
            let table = self.identifier()?;
            self.expect(Kind::Open('('))?;
            let column = self.path()?;
            self.expect(Kind::Close(')'))?;
            Ok(Statement::CreateIndex {
                table,
                column,
                unique,
            })
        }
    }

    fn migration(&mut self) -> Result<Statement> {
        self.expect_word("migration")?;
        let name = self.identifier()?;
        self.block()?;
        let parent = if self.word("parent") {
            self.bump();
            let parent = self.identifier()?;
            self.expect(Kind::Newline)?;
            self.newlines();
            Some(parent)
        } else {
            None
        };
        let mut steps = Vec::new();
        while *self.kind() != Kind::Dedent {
            steps.push(self.migration_step()?);
            if *self.kind() == Kind::Dedent {
                break;
            }
            let after_block = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
            if !after_block {
                self.expect(Kind::Newline)?;
            }
            self.newlines();
        }
        self.expect(Kind::Dedent)?;
        if steps.is_empty() {
            return Err(self.error("migration requires at least one schema operation"));
        }
        Ok(Statement::Migration {
            name,
            parent,
            steps,
        })
    }

    fn migration_step(&mut self) -> Result<SchemaMigration> {
        if self.word("add") {
            self.bump();
            if self.word("type") {
                let Statement::DefineType { name, ty } = self.define_type()? else {
                    unreachable!()
                };
                Ok(SchemaMigration::AddType { name, ty })
            } else if self.word("table") {
                self.bump();
                let table = self.identifier()?;
                let row_type = self.identifier()?;
                let key = if self.word("key") {
                    self.bump();
                    Some(self.path()?)
                } else {
                    None
                };
                Ok(SchemaMigration::AddTable {
                    table,
                    row_type,
                    key,
                })
            } else if self.word("field") {
                self.bump();
                let (owner, name) = self.migration_member("field")?;
                let ty = self.ty(0)?;
                self.expect(Kind::Op("=".into()))?;
                let default = self.value(0)?;
                Ok(SchemaMigration::AddField {
                    owner,
                    column: Column {
                        name,
                        ty,
                        default: Some(default),
                        id: 0,
                    },
                })
            } else if self.word("index") {
                self.bump();
                let (table, column) = self.migration_table_path()?;
                Ok(SchemaMigration::AddIndex {
                    table,
                    column,
                    unique: false,
                })
            } else if self.word("unique") {
                self.bump();
                self.expect_word("index")?;
                let (table, column) = self.migration_table_path()?;
                Ok(SchemaMigration::AddIndex {
                    table,
                    column,
                    unique: true,
                })
            } else {
                self.expect_word("variant")?;
                let (owner, name) = self.migration_member("variant")?;
                let args = self.migration_variant_args()?;
                Ok(SchemaMigration::AddVariant { owner, name, args })
            }
        } else if self.word("drop") {
            self.bump();
            if self.word("type") {
                self.bump();
                Ok(SchemaMigration::DropType {
                    name: self.identifier()?,
                })
            } else if self.word("table") {
                self.bump();
                Ok(SchemaMigration::DropTable {
                    table: self.identifier()?,
                })
            } else if self.word("field") {
                self.bump();
                let (owner, field) = self.migration_member("field")?;
                Ok(SchemaMigration::DropField { owner, field })
            } else if self.word("default") {
                self.bump();
                let (owner, field) = self.migration_member("default")?;
                Ok(SchemaMigration::DropDefault { owner, field })
            } else if self.word("index") {
                self.bump();
                let (table, column) = self.migration_table_path()?;
                Ok(SchemaMigration::DropIndex { table, column })
            } else if self.word("key") {
                self.bump();
                Ok(SchemaMigration::DropKey {
                    table: self.identifier()?,
                })
            } else {
                self.expect_word("variant")?;
                let (owner, variant) = self.migration_member("variant")?;
                let transform = self.optional_migration_transform()?;
                Ok(SchemaMigration::DropVariant {
                    owner,
                    variant,
                    transform,
                })
            }
        } else if self.word("rename") {
            self.bump();
            if self.word("type") {
                self.bump();
                let from = self.identifier()?;
                self.expect_word("to")?;
                Ok(SchemaMigration::RenameType {
                    from,
                    to: self.identifier()?,
                })
            } else if self.word("table") {
                self.bump();
                let from = self.identifier()?;
                self.expect_word("to")?;
                Ok(SchemaMigration::RenameTable {
                    from,
                    to: self.identifier()?,
                })
            } else if self.word("field") {
                self.bump();
                let (owner, from) = self.migration_member("field")?;
                self.expect_word("to")?;
                Ok(SchemaMigration::RenameField {
                    owner,
                    from,
                    to: self.identifier()?,
                })
            } else {
                self.expect_word("variant")?;
                let (owner, from) = self.migration_member("variant")?;
                self.expect_word("to")?;
                Ok(SchemaMigration::RenameVariant {
                    owner,
                    from,
                    to: self.identifier()?,
                })
            }
        } else if self.word("change") {
            self.bump();
            if self.word("default") {
                self.bump();
                let (owner, field) = self.migration_member("default")?;
                self.expect_word("to")?;
                Ok(SchemaMigration::ChangeDefault {
                    owner,
                    field,
                    value: self.value(0)?,
                })
            } else if self.word("field") {
                self.bump();
                let (owner, field) = self.migration_member("field")?;
                self.expect_word("to")?;
                let ty = self.ty(0)?;
                let transform = self.required_migration_transform()?;
                Ok(SchemaMigration::ChangeField {
                    owner,
                    field,
                    ty,
                    transform,
                })
            } else {
                self.expect_word("variant")?;
                let (owner, variant) = self.migration_member("variant")?;
                self.expect_word("to")?;
                let args = self.migration_variant_args()?;
                let transform = self.required_migration_transform()?;
                Ok(SchemaMigration::ChangeVariant {
                    owner,
                    variant,
                    args,
                    transform,
                })
            }
        } else if self.word("set") {
            self.bump();
            self.expect_word("key")?;
            let (table, column) = self.migration_member("key")?;
            Ok(SchemaMigration::SetKey { table, column })
        } else {
            Err(self.error("expected add / drop / rename / change / set in migration"))
        }
    }

    fn migration_member(&mut self, kind: &str) -> Result<(String, String)> {
        let owner = self.identifier()?;
        self.expect(Kind::Dot)?;
        let member = self.identifier()?;
        if self.eat(Kind::Dot) {
            return Err(self.error(format!(
                "migration {kind} operations target a direct member of a named type"
            )));
        }
        Ok((owner, member))
    }

    fn migration_table_path(&mut self) -> Result<(String, String)> {
        let table = self.identifier()?;
        self.expect(Kind::Dot)?;
        Ok((table, self.path()?))
    }

    fn migration_variant_args(&mut self) -> Result<Vec<ScalarType>> {
        if self.eat(Kind::Open('{')) {
            return Ok(vec![ScalarType::Record(self.fields(Kind::Close('}'), 0)?)]);
        }
        if self.eat(Kind::Open('(')) {
            self.newlines();
            let mut args = Vec::new();
            while *self.kind() != Kind::Close(')') {
                args.push(self.ty(0)?);
                self.newlines();
                if !self.eat(Kind::Comma) {
                    break;
                }
                self.newlines();
            }
            self.expect(Kind::Close(')'))?;
            return Ok(args);
        }
        if matches!(self.kind(), Kind::Ident(_) | Kind::Open('(')) && !self.word("using") {
            return Ok(vec![self.ty(0)?]);
        }
        Ok(Vec::new())
    }

    fn optional_migration_transform(&mut self) -> Result<Option<MigrationTransform>> {
        let nested_using = *self.kind() == Kind::Newline
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|token| token.kind == Kind::Indent);
        if self.word("using") || nested_using {
            self.required_migration_transform().map(Some)
        } else {
            Ok(None)
        }
    }

    fn required_migration_transform(&mut self) -> Result<MigrationTransform> {
        let nested = *self.kind() == Kind::Newline;
        if nested {
            self.block()?;
        }
        self.expect_word("using")?;
        let binding = self.identifier()?;
        if binding != "_" && !binding.starts_with(|ch: char| ch.is_ascii_lowercase()) {
            return Err(self.error("migration bindings must start with a lowercase letter"));
        }
        self.expect(Kind::Op("->".into()))?;
        let value = self.match_value()?;
        if nested {
            self.newlines();
            self.expect(Kind::Dedent)?;
        }
        Ok(MigrationTransform { binding, value })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_word("insert")?;
        // Keep a table literally named `many` usable for existing single-row
        // writes; the bulk form always has another identifier for its table.
        let many = self.word("many")
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|token| matches!(token.kind, Kind::Ident(_)));
        if many {
            self.bump();
        }
        let table = self.identifier()?;
        if let Kind::Parameter(parameter) = self.kind().clone() {
            self.bump();
            let returning = self.optional_returning()?;
            return Ok(if many {
                Statement::InsertManyParameter {
                    table,
                    parameter,
                    parameter_type: None,
                    returning,
                }
            } else {
                Statement::InsertParameter {
                    table,
                    parameter,
                    parameter_type: None,
                    returning,
                }
            });
        }
        if many {
            let values = self.value(0)?;
            if !matches!(values, Value::List(_)) {
                return Err(self.error("insert many requires a list of rows"));
            }
            let returning = self.optional_returning()?;
            return Ok(Statement::InsertMany {
                table,
                values,
                returning,
            });
        }
        let values = self.row_value()?;
        let returning = self.optional_returning()?;
        Ok(Statement::Insert {
            table,
            values,
            returning,
        })
    }

    fn upsert(&mut self) -> Result<Statement> {
        self.expect_word("upsert")?;
        let many = self.word("many")
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|token| matches!(token.kind, Kind::Ident(_)));
        if many {
            self.bump();
        }
        let table = self.identifier()?;
        if let Kind::Parameter(parameter) = self.kind().clone() {
            self.bump();
            let returning = self.optional_returning()?;
            return Ok(if many {
                Statement::UpsertManyParameter {
                    table,
                    parameter,
                    parameter_type: None,
                    returning,
                }
            } else {
                Statement::UpsertParameter {
                    table,
                    parameter,
                    parameter_type: None,
                    returning,
                }
            });
        }
        if many {
            let values = self.value(0)?;
            if !matches!(values, Value::List(_)) {
                return Err(self.error("upsert many requires a list of rows"));
            }
            let returning = self.optional_returning()?;
            return Ok(Statement::UpsertMany {
                table,
                values,
                returning,
            });
        }
        let values = self.row_value()?;
        let returning = self.optional_returning()?;
        Ok(Statement::Upsert {
            table,
            values,
            returning,
        })
    }

    fn row_value(&mut self) -> Result<Value> {
        if *self.kind() == Kind::Newline {
            self.block()?;
            self.record(Kind::Dedent, 0)
        } else {
            self.value(0)
        }
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_word("update")?;
        let from = self.identifier()?;
        let mut stages = Vec::new();
        let mut assignments = Vec::new();
        let mut seen = BTreeSet::new();
        let mut setting = false;
        let mut returning = None;
        loop {
            let piped = self.eat(Kind::Pipe);
            let newline = self.eat(Kind::Newline);
            self.newlines();
            let after_layout = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent
                && self.is_update_stage();
            if !piped && !newline && !after_layout {
                break;
            }
            if self.word("filter") && !setting {
                stages.push(self.filter_stage()?);
            } else if self.word("sort") && !setting {
                stages.push(self.sort_stage()?);
            } else if self.word("take") && !setting {
                stages.push(self.take_stage()?);
            } else if self.word("set") {
                setting = true;
                self.bump();
                let braced = self.eat(Kind::Open('{'));
                if braced {
                    self.newlines();
                    if *self.kind() == Kind::Close('}') {
                        return Err(self.error("set requires at least one assignment"));
                    }
                }
                loop {
                    let path = self.path()?;
                    if !seen.insert(path.clone()) {
                        return Err(self.error(format!("duplicate update field '{path}'")));
                    }
                    self.expect(Kind::Op("=".into()))?;
                    let nested = !braced && *self.kind() == Kind::Newline;
                    if nested {
                        self.block()?;
                    } else if braced {
                        self.newlines();
                    }
                    let value = if self.word("match") {
                        SetValue::Match(self.match_value_expression(path.clone())?)
                    } else {
                        SetValue::Expression(self.bool_expression(0, nested || braced)?)
                    };
                    if nested {
                        self.expect(Kind::Dedent)?;
                    }
                    assignments.push(SetAssignment { path, value });
                    if !braced {
                        break;
                    }
                    self.newlines();
                    if self.eat(Kind::Close('}')) {
                        break;
                    }
                    if !self.eat(Kind::Comma) {
                        return Err(self.error("expected ',' between set assignments"));
                    }
                    self.newlines();
                    if self.eat(Kind::Close('}')) {
                        break;
                    }
                }
            } else if self.word("filter") {
                return Err(self.error("update filters must appear before set assignments"));
            } else if self.word("sort") || self.word("take") {
                return Err(
                    self.error("update sort and take stages must appear before set assignments")
                );
            } else if self.word("returning") {
                returning = Some(self.returning_clause()?);
                break;
            } else {
                if piped {
                    return Err(self.error(
                        "expected filter, sort, take, set, or returning after '|' in update",
                    ));
                }
                break;
            }
        }
        if assignments.is_empty() {
            return Err(self.error("update requires at least one set assignment"));
        }
        Ok(Statement::Update {
            target: Pipeline { from, stages },
            assignments,
            returning,
        })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_word("delete")?;
        let from = self.identifier()?;
        let mut stages = Vec::new();
        let mut returning = None;
        loop {
            let piped = self.eat(Kind::Pipe);
            let newline = self.eat(Kind::Newline);
            self.newlines();
            let after_layout = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent
                && self.is_mutation_selection_stage();
            if !piped && !newline && !after_layout {
                break;
            }
            if self.word("filter") {
                stages.push(self.filter_stage()?);
            } else if self.word("sort") {
                stages.push(self.sort_stage()?);
            } else if self.word("take") {
                stages.push(self.take_stage()?);
            } else if self.word("returning") {
                returning = Some(self.returning_clause()?);
                break;
            } else {
                if piped {
                    return Err(
                        self.error("expected filter, sort, take, or returning after '|' in delete")
                    );
                }
                break;
            }
        }
        Ok(Statement::Delete {
            target: Pipeline { from, stages },
            returning,
        })
    }

    fn filter_stage(&mut self) -> Result<Stage> {
        self.expect_word("filter")?;
        if self.word("match") || self.parenthesized_match_starts() {
            let parenthesized = self.eat(Kind::Open('('));
            self.newlines();
            let predicate = self.match_predicate()?;
            if parenthesized {
                self.newlines();
                self.expect(Kind::Close(')'))?;
            }
            Ok(Stage::FilterMatch(predicate))
        } else {
            let nested = *self.kind() == Kind::Newline;
            if nested {
                self.block()?;
            }
            let expression = self.bool_expression(0, nested)?;
            if nested {
                self.expect(Kind::Dedent)?;
            }
            Ok(Stage::Filter(expression))
        }
    }

    fn is_update_stage(&self) -> bool {
        self.is_mutation_selection_stage() || self.word("set") || self.word("returning")
    }

    fn is_mutation_selection_stage(&self) -> bool {
        self.word("filter") || self.word("sort") || self.word("take")
    }

    fn optional_returning(&mut self) -> Result<Option<Returning>> {
        let piped = self.eat(Kind::Pipe);
        let newline = self.eat(Kind::Newline);
        self.newlines();
        let after_layout = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
        if self.word("returning") && (piped || newline || after_layout) {
            return self.returning_clause().map(Some);
        }
        if piped {
            return Err(self.error("expected returning after '|' in row write"));
        }
        Ok(None)
    }

    fn returning_clause(&mut self) -> Result<Returning> {
        self.expect_word("returning")?;
        if matches!(self.kind(), Kind::Newline | Kind::Dedent | Kind::End) {
            return Ok(Returning { fields: Vec::new() });
        }
        let braced = self.eat(Kind::Open('{'));
        self.newlines();
        let mut fields = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            let field = self.path()?;
            if !seen.insert(field.clone()) {
                return Err(self.error(format!("duplicate returning field '{field}'")));
            }
            fields.push(field);
            self.newlines();
            if !self.eat(Kind::Comma) {
                break;
            }
            self.newlines();
            if braced && *self.kind() == Kind::Close('}') {
                break;
            }
        }
        if braced {
            self.expect(Kind::Close('}'))?;
        }
        Ok(Returning { fields })
    }

    fn record(&mut self, end: Kind, depth: usize) -> Result<Value> {
        self.depth(depth)?;
        let mut fields = BTreeMap::new();
        self.newlines();
        while *self.kind() != end {
            let name = self.identifier()?;
            if !self.eat(Kind::Op("=".into())) && !self.eat(Kind::Colon) {
                return Err(self.error("expected '=' after a field name"));
            }
            let value = if *self.kind() == Kind::Newline {
                self.block()?;
                self.record(Kind::Dedent, depth + 1)?
            } else {
                self.value(depth + 1)?
            };
            if fields.insert(name.clone(), value).is_some() {
                return Err(self.error(format!("duplicate field '{name}'")));
            }
            if *self.kind() == end {
                break;
            }
            let after_block = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
            if !self.eat(Kind::Comma) && !self.eat(Kind::Newline) && !after_block {
                return Err(self.error("expected a newline or comma between values"));
            }
            self.newlines();
        }
        self.expect(end)?;
        Ok(Value::Record(fields))
    }

    fn values(&mut self, close: char, depth: usize) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        self.newlines();
        while *self.kind() != Kind::Close(close) {
            if *self.kind() == Kind::End {
                return Err(self.error(format!("expected closing '{close}'")));
            }
            values.push(self.value(depth + 1)?);
            self.newlines();
            if !self.eat(Kind::Comma) {
                break;
            }
            self.newlines();
        }
        self.expect(Kind::Close(close))?;
        Ok(values)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        self.depth(depth)?;
        let token = self.bump();
        Ok(match token.kind {
            Kind::Text(s) => Value::Text(s),
            Kind::Number(raw) => {
                let s = raw.replace('_', "");
                if s.ends_with(char::is_alphabetic) {
                    Value::Duration(s.parse()?)
                } else if s.contains(['.', 'e', 'E']) {
                    let v: f64 = s
                        .parse()
                        .map_err(|_| syntax("invalid float literal", token.span))?;
                    if !v.is_finite() {
                        return Err(syntax("float must be finite", token.span));
                    }
                    Value::Float(v)
                } else {
                    Value::Int(s.parse().map_err(|_| {
                        syntax(
                            "integer literal is outside i64 range or malformed",
                            token.span,
                        )
                    })?)
                }
            }
            Kind::Temporal(source) => {
                if source.contains(['T', 't']) {
                    Value::Timestamp(source.parse()?)
                } else {
                    Value::Date(source.parse()?)
                }
            }
            Kind::Open('{') => self.record(Kind::Close('}'), depth + 1)?,
            Kind::Open('[') => Value::List(self.values(']', depth + 1)?),
            Kind::Open('(') => Value::Tuple(self.values(')', depth + 1)?),
            Kind::Ident(s) if s == "true" => Value::Bool(true),
            Kind::Ident(s) if s == "false" => Value::Bool(false),
            Kind::Ident(s) if s == "null" => Value::Null,
            Kind::Ident(s) if s == "uuid" => {
                let token = self.bump();
                let Kind::Text(source) = token.kind else {
                    return Err(syntax("uuid literal expects quoted text", token.span));
                };
                Value::Uuid(source.parse()?)
            }
            Kind::Ident(s) if s == "bytes" => {
                let token = self.bump();
                let Kind::Text(source) = token.kind else {
                    return Err(syntax(
                        "bytes literal expects quoted lowercase hex",
                        token.span,
                    ));
                };
                if source.bytes().any(|byte| matches!(byte, b'A'..=b'F')) {
                    return Err(syntax(
                        "bytes source literal must use lowercase hex",
                        token.span,
                    ));
                }
                Value::Bytes(source.parse()?)
            }
            Kind::Ident(mut name) => {
                while self.eat(Kind::Dot) {
                    name.push('.');
                    name.push_str(&self.identifier()?);
                }
                let args = if self.eat(Kind::Open('(')) {
                    self.values(')', depth + 1)?
                } else if match self.kind() {
                    Kind::Open('{') | Kind::Open('[') | Kind::Number(_) | Kind::Text(_) => true,
                    Kind::Ident(word) => !matches!(word.as_str(), "and" | "or"),
                    _ => false,
                } {
                    vec![self.value(depth + 1)?]
                } else {
                    Vec::new()
                };
                Value::Enum(EnumValue {
                    variant: name,
                    args,
                    id: 0,
                })
            }
            _ => return Err(syntax("expected a literal value", token.span)),
        })
    }

    fn pipeline(&mut self) -> Result<Statement> {
        self.expect_word("from")?;
        let from = self.identifier()?;
        let mut stages = Vec::new();
        loop {
            let piped = self.eat(Kind::Pipe);
            let newline = self.eat(Kind::Newline);
            self.newlines();
            let after_layout = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent
                && self.is_pipeline_stage();
            if !piped && !newline && !after_layout {
                break;
            }
            let stage = if self.word("let") {
                Stage::Let(self.local_binding()?)
            } else if self.word("filter") {
                self.filter_stage()?
            } else if self.word("derive") {
                self.bump();
                let braced = self.eat(Kind::Open('{'));
                if braced {
                    self.newlines();
                }
                let mut seen = BTreeSet::new();
                loop {
                    let name = self.identifier()?;
                    if !seen.insert(name.clone()) {
                        return Err(Error::new(
                            "E_QUERY",
                            format!("duplicate derive field '{name}'"),
                        )
                        .at(self.token().span));
                    }
                    stages.push(self.derived_field(name, braced)?);
                    if !braced || self.field_set_end("derive")? {
                        break;
                    }
                }
                continue;
            } else if self.word("aggregate") {
                Stage::Aggregate(self.aggregate(Vec::new())?)
            } else if self.word("group") {
                self.bump();
                let group_by = self.path_list("group", "group field")?;
                let parenthesized = self.eat(Kind::Open('('));
                if parenthesized {
                    self.newlines();
                } else {
                    self.block()?;
                }
                if !self.word("aggregate") {
                    return Err(self.error("group inner pipeline must contain aggregate"));
                }
                let aggregate = self.aggregate(group_by)?;
                self.newlines();
                self.expect(if parenthesized {
                    Kind::Close(')')
                } else {
                    Kind::Dedent
                })?;
                Stage::Aggregate(aggregate)
            } else if self.word("select") {
                self.bump();
                let braced = self.eat(Kind::Open('{'));
                self.newlines();
                let mut fields = Vec::new();
                let mut seen = BTreeSet::new();
                loop {
                    let name = self.path()?;
                    if !seen.insert(name.clone()) {
                        return Err(Error::new(
                            "E_QUERY",
                            format!("duplicate selected field '{name}'"),
                        )
                        .at(self.token().span));
                    }
                    if matches!(self.kind(), Kind::Op(op) if op == "=") {
                        if name.contains('.') {
                            return Err(
                                self.error("computed select aliases must be simple field names")
                            );
                        }
                        stages.push(self.derived_field(name.clone(), braced)?);
                    }
                    fields.push(name);
                    if braced {
                        if self.field_set_end("select")? {
                            break;
                        }
                    } else if !self.eat(Kind::Comma) {
                        break;
                    }
                }
                Stage::Select(fields)
            } else if self.word("sort") {
                self.sort_stage()?
            } else if self.word("take") || self.word("limit") {
                self.take_stage()?
            } else if self.word("page") {
                self.page_stage()?
            } else {
                if piped {
                    return Err(self.error(
                        "expected let / filter / derive / aggregate / group / select / sort / take / page after '|'",
                    ));
                }
                break;
            };
            stages.push(stage);
        }
        Ok(Statement::Pipeline(Pipeline { from, stages }))
    }

    // Surface field sets lower directly to the existing sequential derive IR.
    fn derived_field(&mut self, name: String, braced: bool) -> Result<Stage> {
        self.expect(Kind::Op("=".into()))?;
        let nested = !braced && *self.kind() == Kind::Newline;
        if nested {
            self.block()?;
        } else if braced {
            self.newlines();
        }
        let stage = if self.word("match") {
            Stage::DeriveMatch(self.match_value_expression(name)?)
        } else {
            Stage::Derive(DeriveExpression {
                name,
                expression: self.bool_expression(0, nested || braced)?,
                output_type: None,
            })
        };
        if nested {
            self.expect(Kind::Dedent)?;
        }
        Ok(stage)
    }

    fn field_set_end(&mut self, context: &str) -> Result<bool> {
        self.newlines();
        if self.eat(Kind::Close('}')) {
            return Ok(true);
        }
        if !self.eat(Kind::Comma) {
            return Err(self.error(format!("expected ',' between {context} fields")));
        }
        self.newlines();
        Ok(self.eat(Kind::Close('}')))
    }

    fn is_pipeline_stage(&self) -> bool {
        [
            "filter",
            "let",
            "derive",
            "aggregate",
            "group",
            "select",
            "sort",
            "take",
            "limit",
            "page",
        ]
        .iter()
        .any(|word| self.word(word))
    }

    fn sort_stage(&mut self) -> Result<Stage> {
        self.expect_word("sort")?;
        let braced = self.eat(Kind::Open('{'));
        self.newlines();
        let mut keys = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            let descending = self.eat(Kind::Minus);
            let column = self.path()?;
            if !seen.insert(column.clone()) {
                return Err(self.error(format!("duplicate sort field '{column}'")));
            }
            keys.push(SortKey { column, descending });
            if !braced {
                break;
            }
            self.newlines();
            if !self.eat(Kind::Comma) {
                break;
            }
            self.newlines();
            if *self.kind() == Kind::Close('}') {
                break;
            }
        }
        if braced {
            self.expect(Kind::Close('}'))?;
        }
        Ok(Stage::Sort(keys))
    }

    fn take_stage(&mut self) -> Result<Stage> {
        self.bump();
        let token = self.bump();
        let Kind::Number(n) = token.kind else {
            return Err(syntax("expected a nonnegative row count", token.span));
        };
        let normalized = n.replace('_', "");
        if let Some((start, end)) = normalized.split_once("..") {
            let start = start
                .parse::<usize>()
                .map_err(|_| syntax("invalid take range start", token.span))?;
            let end = end
                .parse::<usize>()
                .map_err(|_| syntax("invalid take range end", token.span))?;
            if start == 0 {
                return Err(syntax("take ranges start at 1", token.span));
            }
            let limit = end
                .checked_sub(start)
                .and_then(|distance| distance.checked_add(1))
                .ok_or_else(|| syntax("take range end must not precede its start", token.span))?;
            Ok(Stage::Take {
                offset: start - 1,
                limit,
            })
        } else {
            Ok(Stage::Take {
                offset: 0,
                limit: normalized
                    .parse()
                    .map_err(|_| syntax("invalid row count", token.span))?,
            })
        }
    }

    fn page_stage(&mut self) -> Result<Stage> {
        self.expect_word("page")?;
        let token = self.bump();
        let Kind::Number(number) = token.kind else {
            return Err(syntax("expected a positive page size", token.span));
        };
        let limit = number
            .replace('_', "")
            .parse::<usize>()
            .map_err(|_| syntax("invalid page size", token.span))?;
        let (direction, cursor) = if self.word("after") || self.word("before") {
            let direction = if self.word("after") {
                self.bump();
                PageDirection::Forward
            } else {
                self.bump();
                PageDirection::Backward
            };
            let token = self.bump();
            let Kind::Text(cursor) = token.kind else {
                return Err(syntax("expected an opaque cursor string", token.span));
            };
            (direction, Some(cursor))
        } else {
            (PageDirection::Forward, None)
        };
        Ok(Stage::Page(PageSpec {
            limit,
            direction,
            cursor,
        }))
    }

    fn path_list(&mut self, context: &str, item: &str) -> Result<Vec<String>> {
        let braced = self.eat(Kind::Open('{'));
        self.newlines();
        let mut paths = Vec::new();
        let mut seen = BTreeSet::new();
        loop {
            let path = self.path()?;
            if !seen.insert(path.clone()) {
                return Err(self.error(format!("duplicate {item} '{path}'")));
            }
            paths.push(path);
            if !braced {
                break;
            }
            self.newlines();
            if !self.eat(Kind::Comma) {
                break;
            }
            self.newlines();
            if *self.kind() == Kind::Close('}') {
                break;
            }
        }
        if braced {
            self.expect(Kind::Close('}'))?;
        }
        if paths.is_empty() {
            return Err(self.error(format!("{context} requires at least one field")));
        }
        Ok(paths)
    }

    fn aggregate(&mut self, group_by: Vec<String>) -> Result<Aggregate> {
        self.expect_word("aggregate")?;
        let braced = self.eat(Kind::Open('{'));
        if braced {
            self.newlines();
        } else {
            self.block()?;
        }
        self.newlines();
        let mut assignments = Vec::new();
        let mut seen = BTreeSet::new();
        let end = if braced {
            Kind::Close('}')
        } else {
            Kind::Dedent
        };
        while *self.kind() != end {
            let name = self.identifier()?;
            if !seen.insert(name.clone()) {
                return Err(self.error(format!("duplicate aggregate field '{name}'")));
            }
            self.expect(Kind::Op("=".into()))?;
            let function_name = self.identifier()?;
            let (function, input) = match function_name.as_str() {
                "count" => (AggregateFunction::Count, None),
                "sum" => (
                    AggregateFunction::Sum,
                    Some(self.scalar_expression(0, braced)?),
                ),
                "min" => (
                    AggregateFunction::Min,
                    Some(self.scalar_expression(0, braced)?),
                ),
                "max" => (
                    AggregateFunction::Max,
                    Some(self.scalar_expression(0, braced)?),
                ),
                _ => {
                    return Err(self.error(format!(
                        "unknown aggregate function '{function_name}'; expected count, sum, min, or max"
                    )));
                }
            };
            assignments.push(AggregateAssignment {
                name,
                function,
                input,
                output_type: None,
            });
            if braced {
                self.newlines();
                if *self.kind() == end {
                    break;
                }
                if !self.eat(Kind::Comma) {
                    return Err(self.error("expected ',' between aggregate fields"));
                }
                self.newlines();
            } else {
                if *self.kind() == end {
                    break;
                }
                self.expect(Kind::Newline)?;
                self.newlines();
            }
        }
        self.expect(end)?;
        if assignments.is_empty() {
            return Err(self.error("aggregate requires at least one output field"));
        }
        Ok(Aggregate {
            group_by,
            assignments,
        })
    }

    fn local_binding(&mut self) -> Result<LocalBinding> {
        self.expect_word("let")?;
        let span = self.token().span;
        let name = self.identifier()?;
        if !name.starts_with(|ch: char| ch.is_ascii_lowercase()) {
            return Err(self.error("local binding names must start with a lowercase letter"));
        }
        let annotation = if matches!(self.kind(), Kind::Op(operator) if operator == "=") {
            None
        } else {
            Some(self.ty(0)?)
        };
        self.expect(Kind::Op("=".into()))?;
        let parameters = self.local_parameters()?;
        let nested = *self.kind() == Kind::Newline;
        if nested {
            self.block()?;
        }
        let expression = self.bool_expression(0, nested)?;
        if nested {
            self.expect(Kind::Dedent)?;
        }
        Ok(LocalBinding {
            name,
            span,
            annotation,
            parameters,
            expression,
        })
    }

    fn local_parameters(&mut self) -> Result<Vec<LocalParameter>> {
        if matches!(self.kind(), Kind::Ident(_))
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|token| token.kind == Kind::Op("->".into()))
        {
            let name = self.local_parameter_name()?;
            self.expect(Kind::Op("->".into()))?;
            return Ok(vec![LocalParameter {
                name,
                annotation: None,
            }]);
        }
        if *self.kind() != Kind::Open('(') || !self.parenthesized_parameters_have_arrow() {
            return Ok(Vec::new());
        }
        self.bump();
        self.newlines();
        let mut parameters = Vec::new();
        let mut seen = BTreeSet::new();
        while *self.kind() != Kind::Close(')') {
            let name = self.local_parameter_name()?;
            if !seen.insert(name.clone()) {
                return Err(self.error(format!("duplicate local parameter '{name}'")));
            }
            let annotation = if matches!(self.kind(), Kind::Comma | Kind::Close(')')) {
                None
            } else {
                Some(self.ty(0)?)
            };
            parameters.push(LocalParameter { name, annotation });
            self.newlines();
            if !self.eat(Kind::Comma) {
                break;
            }
            self.newlines();
        }
        self.expect(Kind::Close(')'))?;
        self.expect(Kind::Op("->".into()))?;
        if parameters.is_empty() {
            return Err(self.error("local functions require at least one parameter"));
        }
        Ok(parameters)
    }

    fn local_parameter_name(&mut self) -> Result<String> {
        let name = self.identifier()?;
        if !name.starts_with(|ch: char| ch.is_ascii_lowercase()) {
            return Err(self.error("local parameter names must start with a lowercase letter"));
        }
        Ok(name)
    }

    fn parenthesized_parameters_have_arrow(&self) -> bool {
        let mut depth = 0usize;
        for position in self.pos..self.tokens.len() {
            match self.tokens[position].kind {
                Kind::Open('(') => depth += 1,
                Kind::Close(')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return self
                            .tokens
                            .get(position + 1)
                            .is_some_and(|token| token.kind == Kind::Op("->".into()));
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn comparison_operator(&mut self) -> Result<CmpOp> {
        match self.bump() {
            Token {
                kind: Kind::Op(s), ..
            } => match s.as_str() {
                "=" | "==" => Ok(CmpOp::Eq),
                "!=" => Ok(CmpOp::Ne),
                ">" => Ok(CmpOp::Gt),
                ">=" => Ok(CmpOp::Gte),
                "<" => Ok(CmpOp::Lt),
                "<=" => Ok(CmpOp::Lte),
                _ => Err(self.error("unsupported comparison operator")),
            },
            token => Err(syntax("expected a comparison operator", token.span)),
        }
    }

    fn match_predicate(&mut self) -> Result<MatchPredicate> {
        self.expect_word("match")?;
        let column = self.path()?;
        let braced = self.eat(Kind::Open('{'));
        if braced {
            self.newlines();
        } else {
            self.block()?;
        }
        let end = if braced {
            Kind::Close('}')
        } else {
            Kind::Dedent
        };
        let mut arms = Vec::new();
        self.newlines();
        while *self.kind() != end {
            let pattern = self.match_pattern()?;
            match self.bump() {
                Token {
                    kind: Kind::Op(op), ..
                } if op == "=>" => {}
                token => return Err(syntax("expected '=>' after match pattern", token.span)),
            }
            let condition = if braced {
                self.newlines();
                self.bool_expression(0, true)?
            } else {
                let nested = *self.kind() == Kind::Newline;
                if nested {
                    self.block()?;
                }
                let condition = self.bool_expression(0, nested)?;
                if nested {
                    self.expect(Kind::Dedent)?;
                }
                condition
            };
            arms.push(MatchArm { pattern, condition });
            if braced {
                self.newlines();
                if *self.kind() == end {
                    break;
                }
                if !self.eat(Kind::Comma) {
                    return Err(self.error("expected ',' between match branches"));
                }
                self.newlines();
                if *self.kind() == end {
                    break;
                }
            } else {
                let after_block = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
                if *self.kind() == end {
                    break;
                }
                if !after_block {
                    self.expect(Kind::Newline)?;
                }
                self.newlines();
            }
        }
        self.expect(end)?;
        if arms.is_empty() {
            return Err(self.error("match requires at least one branch"));
        }
        Ok(MatchPredicate { column, arms })
    }

    fn match_pattern(&mut self) -> Result<MatchPattern> {
        let pattern = self.nested_match_pattern(0)?;
        if matches!(
            pattern,
            MatchPattern::Wildcard | MatchPattern::Binding(_) | MatchPattern::Constructor { .. }
        ) {
            Ok(pattern)
        } else {
            Err(self.error("top-level match branch must name a constructor, binding, or '_'"))
        }
    }

    fn nested_match_pattern(&mut self, depth: usize) -> Result<MatchPattern> {
        self.depth(depth)?;
        match self.kind().clone() {
            Kind::Ident(name) => {
                self.bump();
                if name == "_" {
                    return Ok(MatchPattern::Wildcard);
                }
                if name.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                    self.constructor_pattern(name, depth + 1)
                } else {
                    Ok(MatchPattern::Binding(name))
                }
            }
            Kind::Open('(') => {
                self.bump();
                self.newlines();
                let first = self.nested_match_pattern(depth + 1)?;
                self.newlines();
                if !self.eat(Kind::Comma) {
                    self.expect(Kind::Close(')'))?;
                    return Ok(first);
                }
                let mut items = vec![first];
                self.newlines();
                while *self.kind() != Kind::Close(')') {
                    items.push(self.nested_match_pattern(depth + 1)?);
                    self.newlines();
                    if !self.eat(Kind::Comma) {
                        break;
                    }
                    self.newlines();
                }
                self.expect(Kind::Close(')'))?;
                Ok(MatchPattern::Tuple(items))
            }
            Kind::Open('{') => {
                self.bump();
                let (fields, rest) = self.match_record_pattern(depth + 1)?;
                Ok(MatchPattern::Record { fields, rest })
            }
            _ => Err(self.error("expected a binding, constructor, record, tuple, or '_' pattern")),
        }
    }

    fn constructor_pattern(&mut self, mut name: String, depth: usize) -> Result<MatchPattern> {
        while self.eat(Kind::Dot) {
            name.push('.');
            name.push_str(&self.identifier()?);
        }
        let payload = if self.eat(Kind::Open('{')) {
            let (fields, rest) = self.match_record_pattern(depth + 1)?;
            MatchPayload::Record { fields, rest }
        } else {
            let mut patterns = Vec::new();
            while matches!(self.kind(), Kind::Ident(_) | Kind::Open('(')) {
                patterns.push(self.nested_match_pattern(depth + 1)?);
            }
            if patterns.is_empty() {
                MatchPayload::Unit
            } else {
                MatchPayload::Positional(patterns)
            }
        };
        Ok(MatchPattern::Constructor {
            name,
            payload,
            tag: None,
        })
    }

    fn match_record_pattern(&mut self, depth: usize) -> Result<(Vec<MatchField>, bool)> {
        self.depth(depth)?;
        let mut fields = Vec::new();
        let mut rest = false;
        self.newlines();
        while *self.kind() != Kind::Close('}') {
            if self.eat(Kind::Dot) {
                self.expect(Kind::Dot)?;
                if rest {
                    return Err(self.error("record pattern can contain '..' only once"));
                }
                rest = true;
            } else {
                let field = self.identifier()?;
                let pattern = if self.eat(Kind::Op("=".into())) {
                    self.nested_match_pattern(depth + 1)?
                } else {
                    MatchPattern::Binding(field.clone())
                };
                fields.push(MatchField { field, pattern });
            }
            self.newlines();
            if !self.eat(Kind::Comma) {
                break;
            }
            self.newlines();
            if rest && *self.kind() != Kind::Close('}') {
                return Err(self.error("'..' must be the last item in a record pattern"));
            }
        }
        self.expect(Kind::Close('}'))?;
        Ok((fields, rest))
    }

    fn match_value_expression(&mut self, name: String) -> Result<DeriveMatch> {
        self.expect_word("match")?;
        let source = self.path()?;
        let braced = self.eat(Kind::Open('{'));
        if braced {
            self.newlines();
        } else {
            self.block()?;
        }
        let end = if braced {
            Kind::Close('}')
        } else {
            Kind::Dedent
        };
        let mut arms = Vec::new();
        self.newlines();
        while *self.kind() != end {
            let pattern = self.match_pattern()?;
            match self.bump() {
                Token {
                    kind: Kind::Op(op), ..
                } if op == "=>" => {}
                token => return Err(syntax("expected '=>' after match pattern", token.span)),
            }
            let result = if braced {
                self.newlines();
                self.match_value_expression_value(0, true)?
            } else {
                self.match_value()?
            };
            arms.push(MatchValueArm { pattern, result });
            if braced {
                self.newlines();
                if *self.kind() == end {
                    break;
                }
                if !self.eat(Kind::Comma) {
                    return Err(self.error("expected ',' between match branches"));
                }
                self.newlines();
                if *self.kind() == end {
                    break;
                }
            } else {
                if *self.kind() == end {
                    break;
                }
                let after_block = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
                if !after_block {
                    self.expect(Kind::Newline)?;
                }
                self.newlines();
            }
        }
        self.expect(end)?;
        if arms.is_empty() {
            return Err(self.error("match requires at least one branch"));
        }
        Ok(DeriveMatch {
            name,
            source,
            arms,
            output_type: None,
        })
    }

    fn match_value(&mut self) -> Result<MatchValue> {
        let multiline = *self.kind() == Kind::Newline;
        if multiline {
            self.block()?;
        }
        let value = self.match_value_expression_value(0, multiline)?;
        if multiline {
            self.expect(Kind::Dedent)?;
        }
        Ok(value)
    }

    fn match_value_expression_value(
        &mut self,
        depth: usize,
        multiline: bool,
    ) -> Result<MatchValue> {
        self.depth(depth)?;
        if matches!(
            self.kind(),
            Kind::Ident(_)
                | Kind::Parameter(_)
                | Kind::Number(_)
                | Kind::Text(_)
                | Kind::Minus
                | Kind::Open('(')
        ) {
            let checkpoint = self.pos;
            if let Ok(expression) = self.bool_expression(depth + 1, multiline)
                && bool_is_computed(&expression)
            {
                return Ok(MatchValue::Expression(expression));
            }
            self.pos = checkpoint;
        }
        match self.kind().clone() {
            Kind::Text(_) | Kind::Number(_) => Ok(MatchValue::Literal(self.value(depth)?)),
            Kind::Ident(name) if matches!(name.as_str(), "true" | "false" | "null") => {
                Ok(MatchValue::Literal(self.value(depth)?))
            }
            Kind::Ident(name) => {
                self.bump();
                if name.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                    self.match_value_constructor(name, depth + 1, multiline)
                } else {
                    let mut path = name;
                    while self.eat(Kind::Dot) {
                        path.push('.');
                        path.push_str(&self.identifier()?);
                    }
                    Ok(MatchValue::Binding(path))
                }
            }
            Kind::Open('{') => {
                self.bump();
                Ok(MatchValue::Record(
                    self.match_value_fields(depth + 1, multiline)?,
                ))
            }
            Kind::Open('[') => {
                self.bump();
                let mut values = Vec::new();
                self.newlines();
                while *self.kind() != Kind::Close(']') {
                    values.push(self.match_value_expression_value(depth + 1, multiline)?);
                    self.newlines();
                    if !self.eat(Kind::Comma) {
                        break;
                    }
                    self.newlines();
                }
                self.expect(Kind::Close(']'))?;
                Ok(MatchValue::List(values))
            }
            Kind::Open('(') => {
                self.bump();
                self.newlines();
                let first = self.match_value_expression_value(depth + 1, multiline)?;
                self.newlines();
                if !self.eat(Kind::Comma) {
                    self.expect(Kind::Close(')'))?;
                    return Ok(first);
                }
                let mut values = vec![first];
                self.newlines();
                while *self.kind() != Kind::Close(')') {
                    values.push(self.match_value_expression_value(depth + 1, multiline)?);
                    self.newlines();
                    if !self.eat(Kind::Comma) {
                        break;
                    }
                    self.newlines();
                }
                self.expect(Kind::Close(')'))?;
                Ok(MatchValue::Tuple(values))
            }
            _ => Err(self.error("expected a binding or value expression")),
        }
    }

    fn match_value_constructor(
        &mut self,
        mut name: String,
        depth: usize,
        multiline: bool,
    ) -> Result<MatchValue> {
        while self.eat(Kind::Dot) {
            name.push('.');
            name.push_str(&self.identifier()?);
        }
        let payload = if self.eat(Kind::Open('{')) {
            MatchValuePayload::Record(self.match_value_fields(depth + 1, multiline)?)
        } else {
            let mut values = Vec::new();
            while matches!(
                self.kind(),
                Kind::Ident(_)
                    | Kind::Parameter(_)
                    | Kind::Number(_)
                    | Kind::Text(_)
                    | Kind::Minus
                    | Kind::Open('(')
                    | Kind::Open('[')
            ) {
                values.push(self.match_value_expression_value(depth + 1, multiline)?);
            }
            if values.is_empty() {
                MatchValuePayload::Unit
            } else {
                MatchValuePayload::Positional(values)
            }
        };
        Ok(MatchValue::Constructor { name, payload })
    }

    fn match_value_fields(
        &mut self,
        depth: usize,
        multiline: bool,
    ) -> Result<Vec<MatchValueField>> {
        self.depth(depth)?;
        let mut fields = Vec::new();
        let mut seen = BTreeSet::new();
        self.newlines();
        while *self.kind() != Kind::Close('}') {
            let name = self.identifier()?;
            if !seen.insert(name.clone()) {
                return Err(self.error(format!("duplicate value field '{name}'")));
            }
            self.expect(Kind::Op("=".into()))?;
            let value = self.match_value_expression_value(depth + 1, multiline)?;
            fields.push(MatchValueField { name, value });
            if *self.kind() == Kind::Close('}') {
                break;
            }
            let after_block = self.tokens[self.pos.saturating_sub(1)].kind == Kind::Dedent;
            if !self.eat(Kind::Comma) && !self.eat(Kind::Newline) && !after_block {
                return Err(self.error("expected a newline or comma between value fields"));
            }
            self.newlines();
        }
        self.expect(Kind::Close('}'))?;
        Ok(fields)
    }

    fn bool_expression(&mut self, depth: usize, multiline: bool) -> Result<BoolExpression> {
        self.bool_or(depth, multiline)
    }

    fn bool_or(&mut self, depth: usize, multiline: bool) -> Result<BoolExpression> {
        self.depth(depth)?;
        let mut expression = self.bool_and(depth, multiline)?;
        let mut expression_depth = depth;
        self.expression_newlines(multiline);
        while self.word("or") {
            self.bump();
            self.expression_newlines(multiline);
            expression_depth += 1;
            self.depth(expression_depth)?;
            expression = BoolExpression::Or(
                Box::new(expression),
                Box::new(self.bool_and(expression_depth, multiline)?),
            );
            self.expression_newlines(multiline);
        }
        Ok(expression)
    }

    fn bool_and(&mut self, depth: usize, multiline: bool) -> Result<BoolExpression> {
        self.depth(depth)?;
        let mut expression = self.bool_not(depth, multiline)?;
        let mut expression_depth = depth;
        self.expression_newlines(multiline);
        while self.word("and") {
            self.bump();
            self.expression_newlines(multiline);
            expression_depth += 1;
            self.depth(expression_depth)?;
            expression = BoolExpression::And(
                Box::new(expression),
                Box::new(self.bool_not(expression_depth, multiline)?),
            );
            self.expression_newlines(multiline);
        }
        Ok(expression)
    }

    fn bool_not(&mut self, depth: usize, multiline: bool) -> Result<BoolExpression> {
        self.depth(depth)?;
        if self.word("not") {
            self.bump();
            self.expression_newlines(multiline);
            Ok(BoolExpression::Not(Box::new(
                self.bool_not(depth + 1, multiline)?,
            )))
        } else {
            self.bool_primary(depth)
        }
    }

    fn bool_primary(&mut self, depth: usize) -> Result<BoolExpression> {
        self.depth(depth)?;
        if self.word("contains") {
            self.bump();
            return Ok(BoolExpression::Contains {
                collection: self.scalar_argument(depth + 1, false)?,
                item: self.scalar_argument(depth + 1, false)?,
            });
        }
        if self.word("any") || self.word("all") {
            let all = self.word("all");
            self.bump();
            let collection = self.scalar_argument(depth + 1, false)?;
            self.expect(Kind::Open('('))?;
            self.newlines();
            let binding = self.identifier()?;
            if binding == "_" || !binding.starts_with(|ch: char| ch.is_ascii_lowercase()) {
                return Err(
                    self.error("list predicate bindings must start with a lowercase letter")
                );
            }
            self.expect(Kind::Op("->".into()))?;
            self.newlines();
            let predicate = Box::new(self.bool_expression(depth + 1, true)?);
            self.newlines();
            self.expect(Kind::Close(')'))?;
            return Ok(if all {
                BoolExpression::All {
                    collection,
                    binding,
                    predicate,
                }
            } else {
                BoolExpression::Any {
                    collection,
                    binding,
                    predicate,
                }
            });
        }
        if self.word("is_some") || self.word("is_none") {
            let is_none = self.word("is_none");
            self.bump();
            let value = self.scalar_expression(depth + 1, false)?;
            return Ok(if is_none {
                BoolExpression::IsNone(value)
            } else {
                BoolExpression::IsSome(value)
            });
        }
        if *self.kind() == Kind::Open('(') {
            let checkpoint = self.pos;
            if let Ok(left) = self.scalar_expression(depth, false) {
                return self.finish_bool_scalar(left, depth);
            }
            self.pos = checkpoint;
            self.expect(Kind::Open('('))?;
            self.newlines();
            let expression = self.bool_expression(depth + 1, true)?;
            self.newlines();
            self.expect(Kind::Close(')'))?;
            return Ok(expression);
        }
        let left = self.scalar_expression(depth, false)?;
        self.finish_bool_scalar(left, depth)
    }

    fn finish_bool_scalar(
        &mut self,
        left: ScalarExpression,
        depth: usize,
    ) -> Result<BoolExpression> {
        if matches!(self.kind(), Kind::Op(_)) {
            let op = self.comparison_operator()?;
            Ok(BoolExpression::Compare {
                left,
                op,
                right: self.scalar_expression(depth, false)?,
            })
        } else {
            Ok(BoolExpression::Value(left))
        }
    }

    fn scalar_expression(&mut self, depth: usize, multiline: bool) -> Result<ScalarExpression> {
        self.scalar_additive(depth, multiline)
    }

    fn scalar_additive(&mut self, depth: usize, multiline: bool) -> Result<ScalarExpression> {
        self.depth(depth)?;
        let mut expression = self.scalar_multiplicative(depth, multiline)?;
        let mut expression_depth = depth;
        self.expression_newlines(multiline);
        loop {
            let op = if self.eat(Kind::Plus) {
                ArithmeticOp::Add
            } else if self.eat(Kind::Minus) {
                ArithmeticOp::Subtract
            } else {
                break;
            };
            self.expression_newlines(multiline);
            expression_depth += 1;
            self.depth(expression_depth)?;
            expression = ScalarExpression::Arithmetic {
                left: Box::new(expression),
                op,
                right: Box::new(self.scalar_multiplicative(expression_depth, multiline)?),
                ty: None,
            };
            self.expression_newlines(multiline);
        }
        Ok(expression)
    }

    fn scalar_multiplicative(&mut self, depth: usize, multiline: bool) -> Result<ScalarExpression> {
        self.depth(depth)?;
        let mut expression = self.scalar_unary(depth, multiline)?;
        let mut expression_depth = depth;
        self.expression_newlines(multiline);
        loop {
            let op = if self.eat(Kind::Star) {
                ArithmeticOp::Multiply
            } else if self.eat(Kind::Slash) {
                ArithmeticOp::Divide
            } else {
                break;
            };
            self.expression_newlines(multiline);
            expression_depth += 1;
            self.depth(expression_depth)?;
            expression = ScalarExpression::Arithmetic {
                left: Box::new(expression),
                op,
                right: Box::new(self.scalar_unary(expression_depth, multiline)?),
                ty: None,
            };
            self.expression_newlines(multiline);
        }
        Ok(expression)
    }

    fn scalar_unary(&mut self, depth: usize, multiline: bool) -> Result<ScalarExpression> {
        self.depth(depth)?;
        if self.eat(Kind::Minus) {
            self.expression_newlines(multiline);
            return Ok(ScalarExpression::Negate {
                value: Box::new(self.scalar_unary(depth + 1, multiline)?),
                ty: None,
            });
        }
        if self.word("length") {
            self.bump();
            return Ok(ScalarExpression::Length(Box::new(
                self.scalar_unary(depth + 1, multiline)?,
            )));
        }
        self.scalar_application(depth, multiline)
    }

    fn scalar_application(&mut self, depth: usize, multiline: bool) -> Result<ScalarExpression> {
        let span = self.token().span;
        let expression = self.scalar_primary(depth, multiline)?;
        let ScalarExpression::Reference(name) = &expression else {
            return Ok(expression);
        };
        if name.contains('.') || !self.scalar_argument_starts() {
            return Ok(expression);
        }
        let name = name.clone();
        let mut arguments = Vec::new();
        while self.scalar_argument_starts() {
            arguments.push(self.scalar_argument(depth + 1, multiline)?);
        }
        Ok(ScalarExpression::Call {
            name,
            arguments,
            span,
        })
    }

    fn scalar_argument(&mut self, depth: usize, multiline: bool) -> Result<ScalarExpression> {
        self.depth(depth)?;
        if self.eat(Kind::Minus) {
            return Ok(ScalarExpression::Negate {
                value: Box::new(self.scalar_argument(depth + 1, multiline)?),
                ty: None,
            });
        }
        if self.word("length") {
            self.bump();
            return Ok(ScalarExpression::Length(Box::new(
                self.scalar_argument(depth + 1, multiline)?,
            )));
        }
        self.scalar_primary(depth, multiline)
    }

    fn scalar_argument_starts(&self) -> bool {
        match self.kind() {
            Kind::Parameter(_)
            | Kind::Text(_)
            | Kind::Number(_)
            | Kind::Temporal(_)
            | Kind::Open('{')
            | Kind::Open('[')
            | Kind::Open('(') => true,
            Kind::Ident(word) => !matches!(
                word.as_str(),
                "and"
                    | "or"
                    | "filter"
                    | "let"
                    | "derive"
                    | "aggregate"
                    | "group"
                    | "select"
                    | "sort"
                    | "take"
                    | "limit"
            ),
            _ => false,
        }
    }

    fn scalar_primary(&mut self, depth: usize, _multiline: bool) -> Result<ScalarExpression> {
        self.depth(depth)?;
        if *self.kind() == Kind::Open('(') {
            let checkpoint = self.pos;
            self.bump();
            self.newlines();
            if let Ok(expression) = self.scalar_expression(depth + 1, true) {
                self.newlines();
                if self.eat(Kind::Close(')')) {
                    return Ok(expression);
                }
            }
            self.pos = checkpoint;
            return Ok(ScalarExpression::Literal(self.value(depth + 1)?));
        }
        match self.kind().clone() {
            Kind::Parameter(name) => {
                self.bump();
                Ok(ScalarExpression::Parameter { name, ty: None })
            }
            Kind::Ident(name)
                if matches!(name.as_str(), "true" | "false" | "null")
                    || matches!(name.as_str(), "uuid" | "bytes")
                        && matches!(
                            self.tokens.get(self.pos + 1).map(|token| &token.kind),
                            Some(Kind::Text(_))
                        )
                    || name.starts_with(|ch: char| ch.is_ascii_uppercase()) =>
            {
                Ok(ScalarExpression::Literal(self.value(depth + 1)?))
            }
            Kind::Ident(_) => Ok(ScalarExpression::Reference(self.path()?)),
            Kind::Text(_)
            | Kind::Number(_)
            | Kind::Temporal(_)
            | Kind::Open('{')
            | Kind::Open('[') => Ok(ScalarExpression::Literal(self.value(depth + 1)?)),
            _ => {
                Err(self
                    .error("expected a field, binding, literal, length, or arithmetic expression"))
            }
        }
    }

    fn expression_newlines(&mut self, multiline: bool) {
        if multiline {
            self.newlines();
        }
    }
}

fn scalar_is_computed(expression: &ScalarExpression) -> bool {
    matches!(
        expression,
        ScalarExpression::Parameter { .. }
            | ScalarExpression::Ascribed { .. }
            | ScalarExpression::Call { .. }
            | ScalarExpression::Length(_)
            | ScalarExpression::Negate { .. }
            | ScalarExpression::Arithmetic { .. }
    )
}

fn bool_is_computed(expression: &BoolExpression) -> bool {
    match expression {
        BoolExpression::Value(expression) => scalar_is_computed(expression),
        BoolExpression::Compare { .. }
        | BoolExpression::Contains { .. }
        | BoolExpression::Any { .. }
        | BoolExpression::All { .. }
        | BoolExpression::IsSome(_)
        | BoolExpression::IsNone(_)
        | BoolExpression::Not(_)
        | BoolExpression::And(_, _)
        | BoolExpression::Or(_, _) => true,
    }
}
