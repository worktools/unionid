use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;

use crate::db::QueryResponse;

pub fn run_cli(addr: &str, query: Option<String>) -> Result<(), String> {
    if let Some(q) = query {
        let response = send_one(addr, &q)?;
        print_response(&response)?;
        return Ok(());
    }

    run_repl(addr)
}

fn run_repl(addr: &str) -> Result<(), String> {
    println!("connected target: {addr}");
    println!("enter query; type 'exit' to quit");

    let stdin = io::stdin();
    let mut line = String::new();

    loop {
        print!("unionid> ");
        io::stdout()
            .flush()
            .map_err(|e| format!("flush prompt: {e}"))?;

        line.clear();
        stdin
            .read_line(&mut line)
            .map_err(|e| format!("read stdin: {e}"))?;
        let input = line.trim();

        if input.is_empty() {
            continue;
        }

        let response = send_one(addr, input)?;
        print_response(&response)?;

        if input.eq_ignore_ascii_case("exit") || input.eq_ignore_ascii_case("quit") {
            break;
        }
    }

    Ok(())
}

fn send_one(addr: &str, query: &str) -> Result<QueryResponse, String> {
    let mut stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr} failed: {e}"))?;

    stream
        .write_all(query.as_bytes())
        .map_err(|e| format!("write query: {e}"))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("write newline: {e}"))?;
    stream.flush().map_err(|e| format!("flush query: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read response: {e}"))?;

    serde_json::from_str(line.trim()).map_err(|e| format!("parse response json: {e}"))
}

fn print_response(resp: &QueryResponse) -> Result<(), String> {
    if resp.ok {
        println!("ok: {}", resp.message);
    } else {
        println!("error: {}", resp.message);
    }

    if !resp.rows.is_empty() {
        let pretty = serde_json::to_string_pretty(&resp.rows)
            .map_err(|e| format!("serialize rows: {e}"))?;
        println!("{pretty}");
    }

    Ok(())
}
