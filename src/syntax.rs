//! Lexer and layout-aware parser. Newlines inside strings are never separators.
use std::collections::{BTreeMap, BTreeSet};

use crate::error::{Error, Result, Span};
use crate::model::{Column, EnumType, EnumValue, EnumVariantDef, MAX_DEPTH, ScalarType, Value};
use crate::query::{
    CmpOp, LocatedStatement, MatchArm, MatchCondition, MatchPattern, MatchPredicate, Pipeline,
    Predicate, Stage, Statement,
};

pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Ident(String),
    Number(String),
    Text(String),
    Open(char),
    Close(char),
    Comma,
    Colon,
    Dot,
    Pipe,
    Minus,
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

fn syntax(message: impl Into<String>, span: Span) -> Error {
    Error::new("E_SYNTAX", message).at(span)
}

fn lex(source: &str) -> Result<Vec<Token>> {
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
                '-' => {
                    pos += 1;
                    Kind::Minus
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
    if let Some((_, span)) = brackets.last() {
        return Err(syntax("unclosed bracket", *span));
    }
    let end = Span {
        line: last_line + 1,
        column: 1,
    };
    for _ in 1..indentation.len() {
        tokens.push(Token {
            kind: Kind::Dedent,
            span: end,
        });
    }
    tokens.push(Token {
        kind: Kind::End,
        span: end,
    });
    Ok(tokens)
}

pub fn parse(source: &str) -> Result<Vec<LocatedStatement>> {
    Parser {
        tokens: lex(source)?,
        pos: 0,
    }
    .script()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn token(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn kind(&self) -> &Kind {
        &self.token().kind
    }
    fn error(&self, message: impl Into<String>) -> Error {
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
            } else if self.word("from") {
                self.pipeline()?
            } else {
                return Err(self.error(
                    "expected type / table / insert / from (or legacy create table/index)",
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
            self.expect_word("index")?;
            let table = self.identifier()?;
            self.expect(Kind::Open('('))?;
            let column = self.path()?;
            self.expect(Kind::Close(')'))?;
            Ok(Statement::CreateIndex { table, column })
        }
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_word("insert")?;
        let table = self.identifier()?;
        let values = if *self.kind() == Kind::Newline {
            self.block()?;
            self.record(Kind::Dedent, 0)?
        } else {
            self.value(0)?
        };
        Ok(Statement::Insert { table, values })
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
                if s.contains(['.', 'e', 'E']) {
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
            Kind::Open('{') => self.record(Kind::Close('}'), depth + 1)?,
            Kind::Open('[') => Value::List(self.values(']', depth + 1)?),
            Kind::Open('(') => Value::Tuple(self.values(')', depth + 1)?),
            Kind::Ident(s) if s == "true" => Value::Bool(true),
            Kind::Ident(s) if s == "false" => Value::Bool(false),
            Kind::Ident(s) if s == "null" => Value::Null,
            Kind::Ident(mut name) => {
                while self.eat(Kind::Dot) {
                    name.push('.');
                    name.push_str(&self.identifier()?);
                }
                let args = if self.eat(Kind::Open('(')) {
                    self.values(')', depth + 1)?
                } else if matches!(
                    self.kind(),
                    Kind::Open('{')
                        | Kind::Open('[')
                        | Kind::Number(_)
                        | Kind::Text(_)
                        | Kind::Ident(_)
                ) {
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
            let stage = if self.word("filter") {
                self.bump();
                if self.word("match") {
                    Stage::FilterMatch(self.match_predicate()?)
                } else {
                    let column = self.path()?;
                    let op = self.comparison_operator()?;
                    Stage::Filter(Predicate {
                        column,
                        op,
                        value: self.value(0)?,
                    })
                }
            } else if self.word("select") {
                self.bump();
                let braced = self.eat(Kind::Open('{'));
                self.newlines();
                let mut fields = Vec::new();
                let mut seen = BTreeSet::new();
                loop {
                    let name = self.path()?;
                    if !seen.insert(name.clone()) {
                        return Err(self.error(format!("duplicate selected field '{name}'")));
                    }
                    fields.push(name);
                    if braced {
                        self.newlines();
                    }
                    if !self.eat(Kind::Comma) {
                        break;
                    }
                    if braced {
                        self.newlines();
                        if *self.kind() == Kind::Close('}') {
                            break;
                        }
                    }
                }
                if braced {
                    self.expect(Kind::Close('}'))?;
                }
                Stage::Select(fields)
            } else if self.word("sort") {
                self.bump();
                let descending = self.eat(Kind::Minus);
                let column = self.path()?;
                Stage::Sort { column, descending }
            } else if self.word("take") || self.word("limit") {
                self.bump();
                let token = self.bump();
                let Kind::Number(n) = token.kind else {
                    return Err(syntax("expected a nonnegative row count", token.span));
                };
                Stage::Limit(
                    n.parse()
                        .map_err(|_| syntax("invalid row count", token.span))?,
                )
            } else {
                if piped {
                    return Err(self.error("expected filter / select / sort / take after '|'"));
                }
                break;
            };
            stages.push(stage);
        }
        Ok(Statement::Pipeline(Pipeline { from, stages }))
    }

    fn is_pipeline_stage(&self) -> bool {
        ["filter", "select", "sort", "take", "limit"]
            .iter()
            .any(|word| self.word(word))
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
        self.block()?;
        let mut arms = Vec::new();
        self.newlines();
        while *self.kind() != Kind::Dedent {
            let pattern = self.match_pattern()?;
            match self.bump() {
                Token {
                    kind: Kind::Op(op), ..
                } if op == "=>" => {}
                token => return Err(syntax("expected '=>' after match pattern", token.span)),
            }
            let condition = self.match_condition()?;
            arms.push(MatchArm { pattern, condition });
            if *self.kind() == Kind::Dedent {
                break;
            }
            self.expect(Kind::Newline)?;
            self.newlines();
        }
        self.expect(Kind::Dedent)?;
        if arms.is_empty() {
            return Err(self.error("match requires at least one branch"));
        }
        Ok(MatchPredicate { column, arms })
    }

    fn match_pattern(&mut self) -> Result<MatchPattern> {
        let mut name = self.identifier()?;
        if name == "_" {
            return Ok(MatchPattern::Wildcard);
        }
        while self.eat(Kind::Dot) {
            name.push('.');
            name.push_str(&self.identifier()?);
        }
        let mut fields = Vec::new();
        let mut rest = false;
        let record = self.eat(Kind::Open('{'));
        if record {
            self.newlines();
            while *self.kind() != Kind::Close('}') {
                if self.eat(Kind::Dot) {
                    self.expect(Kind::Dot)?;
                    if rest {
                        return Err(self.error("record pattern can contain '..' only once"));
                    }
                    rest = true;
                } else {
                    fields.push(self.identifier()?);
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
        }
        Ok(MatchPattern::Variant {
            name,
            fields,
            record,
            rest,
            variant_id: None,
        })
    }

    fn match_condition(&mut self) -> Result<MatchCondition> {
        if self.word("true") {
            self.bump();
            return Ok(MatchCondition::Bool(true));
        }
        if self.word("false") {
            self.bump();
            return Ok(MatchCondition::Bool(false));
        }
        let binding = self.path()?;
        if matches!(self.kind(), Kind::Op(_)) {
            let op = self.comparison_operator()?;
            Ok(MatchCondition::Compare {
                binding,
                op,
                value: self.value(0)?,
            })
        } else {
            Ok(MatchCondition::Binding(binding))
        }
    }
}
