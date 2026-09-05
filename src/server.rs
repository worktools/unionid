use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::{Engine, Error, QueryResponse};

pub const MAX_FRAME_BYTES: usize = crate::syntax::MAX_SOURCE_BYTES * 6 + 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    query: String,
}

pub fn run_server(
    addr: &str,
    wal_path: Option<PathBuf>,
    snapshot_path: Option<PathBuf>,
    snapshot_every: usize,
) -> Result<(), String> {
    let engine =
        Engine::open(wal_path, snapshot_path, snapshot_every).map_err(|e| e.to_string())?;
    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    println!(
        "unionid server listening on {}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    serve(listener, engine)
}

pub fn serve(listener: TcpListener, engine: Engine) -> Result<(), String> {
    let engine = Arc::new(Mutex::new(engine));
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| format!("accept: {e}"))?;
        if active.fetch_add(1, Ordering::Relaxed) >= 64 {
            active.fetch_sub(1, Ordering::Relaxed);
            drop(stream);
            continue;
        }
        let engine = Arc::clone(&engine);
        let active = Arc::clone(&active);
        std::thread::spawn(move || {
            struct Connection(Arc<AtomicUsize>);
            impl Drop for Connection {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::Relaxed);
                }
            }
            let _connection = Connection(active);
            if let Err(error) = handle(stream, engine) {
                eprintln!("client error: {error}");
            }
        });
    }
    Ok(())
}

fn handle(mut writer: TcpStream, engine: Arc<Mutex<Engine>>) -> Result<(), String> {
    writer
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    writer
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
    loop {
        let mut line = String::new();
        let n = (&mut reader)
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_line(&mut line)
            .map_err(|e| format!("read request: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        let oversized = n > MAX_FRAME_BYTES;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let quit = input.eq_ignore_ascii_case("quit") || input.eq_ignore_ascii_case("exit");
        let response = if oversized {
            QueryResponse::failure(Error::new("E_LIMIT", "request frame too large"))
        } else if quit {
            QueryResponse::ok_message("bye")
        } else {
            let source = if input.starts_with('{') {
                serde_json::from_str::<Request>(input)
                    .map(|r| r.query)
                    .map_err(|e| {
                        Error::new(
                            "E_PROTOCOL",
                            format!("expected JSON object with a query string: {e}"),
                        )
                    })
            } else {
                Ok(input.into())
            };
            match source {
                Ok(source) => engine
                    .lock()
                    .map_err(|_| "engine lock poisoned".to_string())?
                    .execute(&source),
                Err(error) => QueryResponse::failure(error),
            }
        };
        serde_json::to_writer(&mut writer, &response)
            .map_err(|e| format!("encode response: {e}"))?;
        writer
            .write_all(b"\n")
            .and_then(|_| writer.flush())
            .map_err(|e| format!("write response: {e}"))?;
        if quit || oversized {
            return Ok(());
        }
    }
}
