use crate::model::{Column, ScalarType, Value};

#[derive(Debug, Clone)]
pub enum Statement {
    CreateTable(CreateTableStmt),
    CreateIndex(CreateIndexStmt),
    Insert(InsertStmt),
    Pipeline(Pipeline),
}

impl Statement {
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::CreateTable(_) | Self::CreateIndex(_) | Self::Insert(_)
        )
    }
}

#[derive(Debug, Clone)]
pub struct CreateTableStmt {
    pub table: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone)]
pub struct InsertStmt {
    pub table: String,
    pub values: Vec<(String, Value)>,
}

#[derive(Debug, Clone)]
pub struct CreateIndexStmt {
    pub table: String,
    pub column: String,
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub from: String,
    pub stages: Vec<Stage>,
}

#[derive(Debug, Clone)]
pub enum Stage {
    Filter(Predicate),
    Select(Vec<String>),
    Limit(usize),
}

#[derive(Debug, Clone)]
pub struct Predicate {
    pub column: String,
    pub op: CmpOp,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

pub fn parse_statement(input: &str) -> Result<Statement, String> {
    let text = input.trim();

    if text.is_empty() {
        return Err("empty statement".to_string());
    }

    let lower = text.to_lowercase();
    if lower.starts_with("create table ") {
        return parse_create_table(text).map(Statement::CreateTable);
    }

    if lower.starts_with("create index ") {
        return parse_create_index(text).map(Statement::CreateIndex);
    }

    if lower.starts_with("insert ") {
        return parse_insert(text).map(Statement::Insert);
    }

    if lower.starts_with("from ") {
        return parse_pipeline(text).map(Statement::Pipeline);
    }

    Err(
        "unsupported statement; use create table / create index / insert / from pipeline"
            .to_string(),
    )
}

fn parse_create_index(input: &str) -> Result<CreateIndexStmt, String> {
    let rest = input["create index ".len()..].trim();
    let open = rest.find('(').ok_or("missing '(' in create index")?;
    let close = rest.rfind(')').ok_or("missing ')' in create index")?;
    if close <= open {
        return Err("invalid create index block".to_string());
    }

    let table = rest[..open].trim().to_string();
    if table.is_empty() {
        return Err("table name cannot be empty".to_string());
    }

    let column = rest[open + 1..close].trim().to_string();
    if column.is_empty() {
        return Err("index column cannot be empty".to_string());
    }
    if column.contains(',') || column.contains(' ') {
        return Err("create index currently supports single column".to_string());
    }

    Ok(CreateIndexStmt { table, column })
}

fn parse_create_table(input: &str) -> Result<CreateTableStmt, String> {
    let rest = input["create table ".len()..].trim();
    let open = rest.find('(').ok_or("missing '(' in create table")?;
    let close = rest.rfind(')').ok_or("missing ')' in create table")?;
    if close <= open {
        return Err("invalid column definition block".to_string());
    }

    let table = rest[..open].trim().to_string();
    if table.is_empty() {
        return Err("table name cannot be empty".to_string());
    }

    let cols_raw = &rest[open + 1..close];
    let mut columns = Vec::new();
    for part in split_top_level_csv(cols_raw) {
        let def = part.trim();
        if def.is_empty() {
            continue;
        }

        let mut iter = def.split_whitespace();
        let name = iter
            .next()
            .ok_or_else(|| format!("missing column name in '{def}'"))?;
        let ty_str = def[name.len()..].trim();
        if ty_str.is_empty() {
            return Err(format!("missing column type in '{def}'"));
        }

        columns.push(Column {
            name: name.to_string(),
            ty: ScalarType::parse(ty_str)?,
        });
    }

    if columns.is_empty() {
        return Err("table schema cannot be empty".to_string());
    }

    Ok(CreateTableStmt { table, columns })
}

fn parse_insert(input: &str) -> Result<InsertStmt, String> {
    let rest = input["insert ".len()..].trim();
    let open = rest.find('{').ok_or("missing '{' in insert")?;
    let close = rest.rfind('}').ok_or("missing '}' in insert")?;
    if close <= open {
        return Err("invalid insert value block".to_string());
    }

    let table = rest[..open].trim().to_string();
    if table.is_empty() {
        return Err("table name cannot be empty".to_string());
    }

    let body = &rest[open + 1..close];
    let mut values = Vec::new();
    for pair in split_top_level_csv(body) {
        let trimmed = pair.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(idx) = trimmed.find(':') else {
            return Err(format!("invalid key:value pair '{trimmed}'"));
        };
        let key = trimmed[..idx].trim().to_string();
        let value = Value::parse_literal(trimmed[idx + 1..].trim())?;
        values.push((key, value));
    }

    if values.is_empty() {
        return Err("insert value map cannot be empty".to_string());
    }

    Ok(InsertStmt { table, values })
}

fn parse_pipeline(input: &str) -> Result<Pipeline, String> {
    let segments = input
        .split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return Err("invalid pipeline".to_string());
    }

    let from = segments[0]
        .strip_prefix("from ")
        .ok_or("pipeline must start with 'from <table>'")?
        .trim()
        .to_string();

    if from.is_empty() {
        return Err("from table cannot be empty".to_string());
    }

    let mut stages = Vec::new();
    for stage in segments.iter().skip(1) {
        if let Some(rest) = stage.strip_prefix("filter ") {
            stages.push(Stage::Filter(parse_predicate(rest.trim())?));
            continue;
        }

        if let Some(rest) = stage.strip_prefix("select ") {
            let cols = rest
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if cols.is_empty() {
                return Err("select list cannot be empty".to_string());
            }
            stages.push(Stage::Select(cols));
            continue;
        }

        if let Some(rest) = stage.strip_prefix("limit ") {
            let n = rest
                .trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid limit value '{rest}'"))?;
            stages.push(Stage::Limit(n));
            continue;
        }

        return Err(format!("unsupported stage '{stage}'"));
    }

    Ok(Pipeline { from, stages })
}

fn parse_predicate(input: &str) -> Result<Predicate, String> {
    let mut split = input
        .splitn(3, char::is_whitespace)
        .filter(|s| !s.is_empty());
    let column = split
        .next()
        .ok_or("missing filter column")?
        .trim()
        .to_string();
    let op_raw = split.next().ok_or("missing filter operator")?.trim();
    let value_raw = split.next().ok_or("missing filter value")?.trim();

    let op = match op_raw {
        "=" | "==" => CmpOp::Eq,
        "!=" => CmpOp::Ne,
        ">" => CmpOp::Gt,
        ">=" => CmpOp::Gte,
        "<" => CmpOp::Lt,
        "<=" => CmpOp::Lte,
        _ => return Err(format!("unsupported operator '{op_raw}'")),
    };

    Ok(Predicate {
        column,
        op,
        value: Value::parse_literal(value_raw)?,
    })
}

fn split_top_level_csv(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut paren_depth = 0i32;

    for ch in input.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            '(' if !in_quotes => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' if !in_quotes => {
                paren_depth -= 1;
                current.push(ch);
            }
            ',' if !in_quotes && paren_depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }

    out
}
