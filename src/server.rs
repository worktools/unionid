use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::protocol::{
    MAX_INTROSPECTION_BYTES, MAX_REQUEST_ID_BYTES, Request as ProtocolRequest,
    Response as ProtocolResponse, VERSION,
};
use crate::{Engine, Error, QueryResponse};

pub const MAX_FRAME_BYTES: usize = crate::syntax::MAX_SOURCE_BYTES * 6 + 256;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 64;
const CONNECTION_POLL: Duration = Duration::from_millis(100);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const EXECUTION_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ServerStats {
    pub accepted_connections: usize,
    pub rejected_connections: usize,
    pub requests: usize,
    pub failed_requests: usize,
}

#[derive(Default)]
struct RuntimeStats {
    active: AtomicUsize,
    accepted: AtomicUsize,
    rejected: AtomicUsize,
    requests: AtomicUsize,
    failed: AtomicUsize,
}

impl RuntimeStats {
    fn snapshot(&self) -> ServerStats {
        ServerStats {
            accepted_connections: self.accepted.load(Ordering::Relaxed),
            rejected_connections: self.rejected.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            failed_requests: self.failed.load(Ordering::Relaxed),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRequest {
    query: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum OutgoingResponse {
    Legacy(QueryResponse),
    Versioned(ProtocolResponse),
}

impl OutgoingResponse {
    fn ok(&self) -> bool {
        match self {
            Self::Legacy(response) => response.ok,
            Self::Versioned(response) => response.ok,
        }
    }

    fn limit_fallback(&self) -> serde_json::Value {
        let error = Error::new(
            "E_LIMIT",
            format!("response exceeds {MAX_RESPONSE_BYTES} byte limit"),
        );
        match self {
            Self::Legacy(_) => serde_json::to_value(QueryResponse::failure(error))
                .expect("limit response serialization cannot fail"),
            Self::Versioned(response) => serde_json::json!({
                "version": VERSION,
                "request_id": response.request_id,
                "ok": false,
                "message": error.to_string(),
                "columns": [],
                "rows": [],
                "error": error,
                "schema": response.schema,
            }),
        }
    }
}

pub fn run_server(
    addr: &str,
    wal_path: Option<PathBuf>,
    snapshot_path: Option<PathBuf>,
    snapshot_every: usize,
) -> Result<(), String> {
    run_server_with_db(addr, None, wal_path, snapshot_path, snapshot_every)
}

pub fn run_server_with_db(
    addr: &str,
    db_path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
    snapshot_path: Option<PathBuf>,
    snapshot_every: usize,
) -> Result<(), String> {
    run_server_with_db_read_only(
        addr,
        db_path,
        wal_path,
        snapshot_path,
        snapshot_every,
        false,
    )
}

pub fn run_server_with_db_read_only(
    addr: &str,
    db_path: Option<PathBuf>,
    wal_path: Option<PathBuf>,
    snapshot_path: Option<PathBuf>,
    snapshot_every: usize,
    read_only: bool,
) -> Result<(), String> {
    if db_path.is_some() && (wal_path.is_some() || snapshot_path.is_some() || snapshot_every > 0) {
        return Err("E_CONFIG: --db cannot be combined with WAL or snapshot options".into());
    }
    if read_only && db_path.is_none() {
        return Err("E_CONFIG: --read-only requires --db".into());
    }
    let engine = match db_path {
        Some(path) if read_only => Engine::open_redb_read_only(path),
        Some(path) => Engine::open_redb(path),
        None => Engine::open(wal_path, snapshot_path, snapshot_every),
    }
    .map_err(|e| e.to_string())?;
    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    println!(
        "unionid server listening on {}",
        listener.local_addr().map_err(|e| e.to_string())?
    );
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&shutdown);
    ctrlc::set_handler(move || signal.store(true, Ordering::Release))
        .map_err(|error| format!("install shutdown handler: {error}"))?;
    let stats = serve_until(listener, engine, shutdown)?;
    eprintln!(
        "unionid server stopped: accepted={}, rejected={}, requests={}, failed={}",
        stats.accepted_connections,
        stats.rejected_connections,
        stats.requests,
        stats.failed_requests
    );
    Ok(())
}

pub fn serve(listener: TcpListener, engine: Engine) -> Result<(), String> {
    serve_until(listener, engine, Arc::new(AtomicBool::new(false))).map(|_| ())
}

pub fn serve_until(
    listener: TcpListener,
    engine: Engine,
    shutdown: Arc<AtomicBool>,
) -> Result<ServerStats, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set listener nonblocking: {error}"))?;
    let engine = Arc::new(Mutex::new(engine));
    let stats = Arc::new(RuntimeStats::default());
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stats.active.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
                    stats.active.fetch_sub(1, Ordering::AcqRel);
                    stats.rejected.fetch_add(1, Ordering::Relaxed);
                    if let Err(error) = reject_busy(stream) {
                        eprintln!("reject client: {error}");
                    }
                    continue;
                }
                stats.accepted.fetch_add(1, Ordering::Relaxed);
                let engine = Arc::clone(&engine);
                let stats = Arc::clone(&stats);
                let shutdown = Arc::clone(&shutdown);
                std::thread::spawn(move || {
                    struct Connection(Arc<RuntimeStats>);
                    impl Drop for Connection {
                        fn drop(&mut self) {
                            self.0.active.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    let _connection = Connection(Arc::clone(&stats));
                    if let Err(error) = handle(stream, engine, shutdown, stats) {
                        eprintln!("client error: {error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(CONNECTION_POLL);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("accept: {error}")),
        }
    }
    while stats.active.load(Ordering::Acquire) != 0 {
        std::thread::sleep(CONNECTION_POLL);
    }
    drop(engine);
    Ok(stats.snapshot())
}

fn reject_busy(mut stream: TcpStream) -> Result<(), String> {
    // Rejection must not create another worker or leave the accept loop waiting
    // indefinitely on a client that does not read its response.
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| format!("set rejection timeout: {e}"))?;
    write_response(
        &mut stream,
        &OutgoingResponse::Legacy(QueryResponse::failure(Error::new(
            "E_BUSY",
            "active connection limit reached; retry after a connection closes",
        ))),
    )?;
    // Closing with unread request bytes can reset the connection and discard
    // E_BUSY. Half-close first, then give the client a bounded chance to finish.
    if let Err(error) = stream.shutdown(Shutdown::Write)
        && !matches!(
            error.kind(),
            std::io::ErrorKind::NotConnected | std::io::ErrorKind::InvalidInput
        )
    {
        return Err(error.to_string());
    }
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

fn write_response(stream: &mut TcpStream, response: &impl Serialize) -> Result<(), String> {
    let encoded = encode_limited(response).unwrap_or_else(|_| {
        serde_json::to_vec(&QueryResponse::failure(Error::new(
            "E_LIMIT",
            format!("response exceeds {MAX_RESPONSE_BYTES} byte limit"),
        )))
        .expect("limit response serialization cannot fail")
    });
    write_encoded(stream, &encoded)
}

fn write_outgoing_response(
    stream: &mut TcpStream,
    response: &OutgoingResponse,
) -> Result<bool, String> {
    let encoded = encode_limited(response);
    let limited = encoded.is_err();
    let encoded = encoded.unwrap_or_else(|_| {
        let fallback = response.limit_fallback();
        serde_json::to_vec(&fallback).expect("limit response serialization cannot fail")
    });
    write_encoded(stream, &encoded)?;
    Ok(limited)
}

fn write_encoded(stream: &mut TcpStream, encoded: &[u8]) -> Result<(), String> {
    stream
        .write_all(encoded)
        .and_then(|_| stream.write_all(b"\n"))
        .and_then(|_| stream.flush())
        .map_err(|e| format!("write response: {e}"))
}

fn encode_limited(response: &impl Serialize) -> Result<Vec<u8>, serde_json::Error> {
    struct LimitedWriter(Vec<u8>);
    impl Write for LimitedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
                return Err(std::io::Error::other("response limit exceeded"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = LimitedWriter(Vec::new());
    serde_json::to_writer(&mut writer, response)?;
    Ok(writer.0)
}

fn handle(
    mut writer: TcpStream,
    engine: Arc<Mutex<Engine>>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<RuntimeStats>,
) -> Result<(), String> {
    writer
        .set_read_timeout(Some(CONNECTION_POLL))
        .map_err(|e| e.to_string())?;
    writer
        .set_write_timeout(Some(IDLE_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(writer.try_clone().map_err(|e| e.to_string())?);
    let mut idle_deadline = Instant::now() + IDLE_TIMEOUT;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut line = String::new();
        let n = loop {
            let remaining = (MAX_FRAME_BYTES + 1).saturating_sub(line.len());
            if remaining == 0 {
                break line.len();
            }
            match (&mut reader).take(remaining as u64).read_line(&mut line) {
                Ok(n) => break n,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if shutdown.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if Instant::now() >= idle_deadline {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("read request: {error}")),
            }
        };
        if n == 0 && line.is_empty() {
            return Ok(());
        }
        idle_deadline = Instant::now() + IDLE_TIMEOUT;
        let oversized = line.len() > MAX_FRAME_BYTES;
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        stats.requests.fetch_add(1, Ordering::Relaxed);
        let quit = input.eq_ignore_ascii_case("quit") || input.eq_ignore_ascii_case("exit");
        let response = if oversized {
            OutgoingResponse::Legacy(QueryResponse::failure(Error::new(
                "E_LIMIT",
                "request frame too large",
            )))
        } else if quit {
            OutgoingResponse::Legacy(QueryResponse::ok_message("bye"))
        } else {
            let mut engine = engine
                .lock()
                .map_err(|_| "engine lock poisoned".to_string())?;
            if input.starts_with('{') {
                execute_json_request(input, &mut engine)
            } else {
                OutgoingResponse::Legacy(engine.execute_with_params_until(
                    input,
                    std::collections::BTreeMap::new(),
                    None,
                    Instant::now() + EXECUTION_TIMEOUT,
                ))
            }
        };
        let failed = !response.ok();
        let limited = write_outgoing_response(&mut writer, &response)?;
        if failed || limited {
            stats.failed.fetch_add(1, Ordering::Relaxed);
        }
        if oversized {
            drain_after_response(&mut writer, &mut reader)?;
            return Ok(());
        }
        if quit {
            return Ok(());
        }
    }
}

fn drain_after_response(writer: &mut TcpStream, reader: &mut impl Read) -> Result<(), String> {
    if let Err(error) = writer.shutdown(Shutdown::Write)
        && !matches!(
            error.kind(),
            std::io::ErrorKind::NotConnected | std::io::ErrorKind::InvalidInput
        )
    {
        return Err(error.to_string());
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut remaining = MAX_FRAME_BYTES + 1;
    let mut buffer = [0; 4096];
    while remaining > 0 {
        let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        writer
            .set_read_timeout(Some(timeout))
            .map_err(|error| error.to_string())?;
        let size = remaining.min(buffer.len());
        match reader.read(&mut buffer[..size]) {
            Ok(0) => break,
            Ok(read) => remaining -= read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

fn execute_json_request(input: &str, engine: &mut Engine) -> OutgoingResponse {
    let decoded = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(decoded) => decoded,
        Err(error) => {
            return legacy_error(Error::new(
                "E_PROTOCOL",
                format!("expected a JSON request object: {error}"),
            ));
        }
    };
    let versioned = decoded
        .as_object()
        .is_some_and(|object| object.contains_key("version"));
    if !versioned {
        return match serde_json::from_value::<LegacyRequest>(decoded) {
            Ok(request) => OutgoingResponse::Legacy(engine.execute_with_params_until(
                &request.query,
                std::collections::BTreeMap::new(),
                None,
                Instant::now() + EXECUTION_TIMEOUT,
            )),
            Err(error) => legacy_error(Error::new(
                "E_PROTOCOL",
                format!("expected JSON object with a query string: {error}"),
            )),
        };
    }
    let request_id = decoded
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let request = match serde_json::from_value::<ProtocolRequest>(decoded) {
        Ok(request) => request,
        Err(error) => {
            return protocol_error(
                request_id,
                Error::new("E_PROTOCOL", format!("invalid versioned request: {error}")),
                engine,
            );
        }
    };
    OutgoingResponse::Versioned(execute_protocol_request(engine, request))
}

/// Execute the stable versioned data protocol independently of its transport.
///
/// TCP uses this directly, and HTTP adapters can deserialize the same
/// `ProtocolRequest` and serialize the returned `ProtocolResponse` without
/// duplicating version, parameter, schema, or introspection semantics.
pub fn execute_protocol_request(engine: &mut Engine, request: ProtocolRequest) -> ProtocolResponse {
    if request.version != VERSION {
        return ProtocolResponse::failure(
            request.request_id,
            Error::new(
                "E_PROTOCOL_VERSION",
                format!(
                    "unsupported protocol version {}; supported version is {VERSION}",
                    request.version
                ),
            ),
            engine.schema_info(),
        );
    }
    if request.request_id.len() > MAX_REQUEST_ID_BYTES {
        return ProtocolResponse::failure(
            request.request_id,
            Error::new(
                "E_LIMIT",
                format!("request_id exceeds {MAX_REQUEST_ID_BYTES} byte limit"),
            ),
            engine.schema_info(),
        );
    }
    if request.introspect.is_some() {
        if !request.query.is_empty() || !request.params.is_empty() || request.schema.is_some() {
            return ProtocolResponse::failure(
                request.request_id,
                Error::new(
                    "E_PROTOCOL",
                    "introspection requests cannot include query, params, or schema",
                ),
                engine.schema_info(),
            );
        }
        return introspection_protocol_response(request.request_id, engine.introspection());
    }
    let parameters = match request.decode_params() {
        Ok(parameters) => parameters,
        Err(error) => {
            return ProtocolResponse::failure(request.request_id, error, engine.schema_info());
        }
    };
    let response = engine.execute_with_params_until(
        &request.query,
        parameters,
        request.schema.as_ref(),
        Instant::now() + EXECUTION_TIMEOUT,
    );
    ProtocolResponse::from_query(request.request_id, response)
}

fn introspection_protocol_response(
    request_id: String,
    introspection: crate::Introspection,
) -> ProtocolResponse {
    let schema = introspection.schema.clone();
    match serde_json::to_vec(&introspection) {
        Ok(encoded) if encoded.len() <= MAX_INTROSPECTION_BYTES => {
            ProtocolResponse::from_introspection(request_id, introspection)
        }
        Ok(_) => ProtocolResponse::failure(
            request_id,
            Error::new(
                "E_LIMIT",
                format!("introspection exceeds {MAX_INTROSPECTION_BYTES} byte response limit"),
            ),
            schema,
        ),
        Err(error) => ProtocolResponse::failure(
            request_id,
            Error::new("E_PROTOCOL", format!("encode introspection: {error}")),
            schema,
        ),
    }
}

fn legacy_error(error: Error) -> OutgoingResponse {
    OutgoingResponse::Legacy(QueryResponse::failure(error))
}

fn protocol_error(request_id: String, error: Error, engine: &Engine) -> OutgoingResponse {
    OutgoingResponse::Versioned(ProtocolResponse::failure(
        request_id,
        error,
        engine.schema_info(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_encoder_stops_at_the_byte_limit() {
        let small = OutgoingResponse::Legacy(QueryResponse::ok_message("ok"));
        assert!(encode_limited(&small).is_ok());
        let oversized = OutgoingResponse::Versioned(ProtocolResponse::from_query(
            "large-request",
            QueryResponse::ok_message("x".repeat(MAX_RESPONSE_BYTES)),
        ));
        assert!(encode_limited(&oversized).is_err());
        let fallback = oversized.limit_fallback();
        assert_eq!(fallback["request_id"], "large-request");
        assert_eq!(fallback["error"]["code"], "E_LIMIT");
    }

    #[test]
    fn introspection_has_a_smaller_explicit_response_limit() {
        let mut introspection = Engine::memory().introspection();
        introspection.schema_source = "x".repeat(MAX_INTROSPECTION_BYTES + 1);
        let response = introspection_protocol_response("large-introspection".into(), introspection);
        assert_eq!(response.request_id, "large-introspection");
        assert_eq!(response.error.unwrap().code, "E_LIMIT");
    }

    #[test]
    fn oversized_request_ids_are_rejected_before_mutations() {
        let mut engine = Engine::memory();
        let input = serde_json::json!({
            "version": VERSION,
            "request_id": "x".repeat(MAX_REQUEST_ID_BYTES + 1),
            "query": "create table items (id int)"
        })
        .to_string();
        let OutgoingResponse::Versioned(response) = execute_json_request(&input, &mut engine)
        else {
            panic!("versioned request must receive a versioned response");
        };
        assert_eq!(response.error.unwrap().code, "E_LIMIT");
        assert_eq!(engine.execute("from items").error.unwrap().code, "E_TABLE");
    }
}
