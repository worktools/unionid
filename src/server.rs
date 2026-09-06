use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::{Engine, Error, QueryResponse};

pub const MAX_FRAME_BYTES: usize = crate::syntax::MAX_SOURCE_BYTES * 6 + 256;
pub const MAX_CONNECTIONS: usize = 64;

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
        if active.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::Relaxed);
            if let Err(error) = reject_busy(stream) {
                eprintln!("reject client: {error}");
            }
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

fn reject_busy(mut stream: TcpStream) -> Result<(), String> {
    // Rejection must not create another worker or leave the accept loop waiting
    // indefinitely on a client that does not read its response.
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| format!("set rejection timeout: {e}"))?;
    write_response(
        &mut stream,
        &QueryResponse::failure(Error::new(
            "E_BUSY",
            "active connection limit reached; retry after a connection closes",
        )),
    )?;
    // Closing with unread request bytes can reset the connection and discard
    // E_BUSY. Half-close first, then give the client a bounded chance to finish.
    stream
        .shutdown(Shutdown::Write)
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut remaining = MAX_FRAME_BYTES + 1;
    let mut buffer = [0; 4096];
    while remaining > 0 {
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        stream
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let size = remaining.min(buffer.len());
        match stream.read(&mut buffer[..size]) {
            Ok(0) => break,
            Ok(n) => remaining -= n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // Timeout or disconnected peer; rejection is done.
        }
    }
    Ok(())
}

fn write_response(stream: &mut TcpStream, response: &QueryResponse) -> Result<(), String> {
    serde_json::to_writer(&mut *stream, response).map_err(|e| format!("encode response: {e}"))?;
    stream
        .write_all(b"\n")
        .and_then(|_| stream.flush())
        .map_err(|e| format!("write response: {e}"))
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
        write_response(&mut writer, &response)?;
        if quit || oversized {
            return Ok(());
        }
    }
}
