use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::{Engine, QueryResponse, Value};

pub fn run_local(source: Option<String>, json: bool) -> Result<(), String> {
    let mut engine = Engine::memory();
    if let Some(source) = source {
        return print_response(&engine.execute(&source), json);
    }
    repl(Some(&mut engine), "", json)
}

pub fn run_cli(addr: &str, source: Option<String>, json: bool) -> Result<(), String> {
    if let Some(source) = source {
        return print_response(&send_one(addr, &source)?, json);
    }
    repl(None, addr, json)
}

fn repl(mut engine: Option<&mut Engine>, addr: &str, json: bool) -> Result<(), String> {
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if !interactive {
        let source = read_source(stdin.lock())?;
        if source.trim().is_empty() {
            return Ok(());
        }
        let response = match engine {
            Some(engine) => engine.execute(&source),
            None => send_one(addr, &source)?,
        };
        return print_response(&response, json);
    }
    eprintln!(
        "Enter a script, then a blank line to run. .quit exits; local mode also supports .schema and .tables."
    );
    let mut buffer = String::new();
    loop {
        eprint!(
            "{}",
            if buffer.is_empty() {
                "unionid> "
            } else {
                "     ..> "
            }
        );
        io::stderr().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        let n = stdin.read_line(&mut line).map_err(|e| e.to_string())?;
        if buffer.is_empty() && matches!(line.trim(), ".quit" | "quit" | "exit") {
            break;
        }
        if buffer.is_empty()
            && let Some(engine) = engine.as_deref()
        {
            match line.trim() {
                ".schema" => {
                    println!("{}", engine.schema());
                    continue;
                }
                ".tables" => {
                    println!("{}", engine.tables().join("\n"));
                    continue;
                }
                _ => {}
            }
        }
        if !line.trim().is_empty() {
            buffer.push_str(&line);
        }
        if buffer.len() > crate::syntax::MAX_SOURCE_BYTES {
            eprintln!("source exceeds 1 MiB");
            buffer.clear();
        }
        if (line.trim().is_empty() || n == 0) && !buffer.trim().is_empty() {
            let response = match engine.as_deref_mut() {
                Some(engine) => Ok(engine.execute(&buffer)),
                None => send_one(addr, &buffer),
            };
            match response.and_then(|r| print_response(&r, json)) {
                Ok(()) => {}
                Err(e) => eprintln!("{e}"),
            }
            buffer.clear();
        }
        if n == 0 {
            break;
        }
    }
    Ok(())
}

pub fn read_source(reader: impl Read) -> Result<String, String> {
    let mut source = String::new();
    reader
        .take((crate::syntax::MAX_SOURCE_BYTES + 1) as u64)
        .read_to_string(&mut source)
        .map_err(|e| format!("read script: {e}"))?;
    if source.len() > crate::syntax::MAX_SOURCE_BYTES {
        return Err("source exceeds 1 MiB".into());
    }
    Ok(source)
}

pub fn send_one(addr: &str, query: &str) -> Result<QueryResponse, String> {
    if query.len() > crate::syntax::MAX_SOURCE_BYTES {
        return Err("source exceeds 1 MiB".into());
    }
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut stream, &serde_json::json!({"query": query}))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"\n")
        .and_then(|_| stream.flush())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(16 * 1024 * 1024 + 1)
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;
    if line.len() > 16 * 1024 * 1024 {
        return Err("response exceeds 16 MiB".into());
    }
    serde_json::from_str(line.trim()).map_err(|e| format!("decode response: {e}"))
}

fn print_response(response: &QueryResponse, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string(response).map_err(|e| e.to_string())?
        );
    }
    if !response.ok {
        return Err(response.message.clone());
    }
    for warning in &response.warnings {
        eprintln!("warning: {warning}");
    }
    if !json {
        if response.columns.is_empty() {
            println!("{}", response.message);
        } else {
            println!(
                "{}",
                response
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            for row in &response.rows {
                println!(
                    "{}",
                    response
                        .columns
                        .iter()
                        .map(|c| row.get(&c.name).map(display_value).unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join(" | ")
                );
            }
            println!("{}", response.message);
        }
    }
    Ok(())
}

pub fn display_value(value: &Value) -> String {
    match value {
        Value::Int(v) => v.to_string(),
        Value::Float(v) => format!("{v:?}"),
        Value::Bool(v) => v.to_string(),
        Value::Text(v) => serde_json::to_string(v).unwrap_or_default(),
        Value::Null => "null".into(),
        Value::Named { value, .. } => display_value(value),
        Value::Record(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(k, v)| format!("{k} = {}", display_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Tuple(vs) => format!(
            "({})",
            vs.iter().map(display_value).collect::<Vec<_>>().join(", ")
        ),
        Value::List(vs) => format!(
            "[{}]",
            vs.iter().map(display_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Option(None) => "None".into(),
        Value::Option(Some(v)) => format!("Some ({})", display_value(v)),
        Value::Enum(v) => {
            if v.args.is_empty() {
                v.variant.clone()
            } else if v.args.len() == 1 && matches!(v.args[0], Value::Record(_)) {
                format!("{} {}", v.variant, display_value(&v.args[0]))
            } else {
                format!(
                    "{}({})",
                    v.variant,
                    v.args
                        .iter()
                        .map(display_value)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }
}
