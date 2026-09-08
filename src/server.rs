use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::protocol::{
    MAX_INTROSPECTION_BYTES, MAX_REQUEST_ID_BYTES, ReceiptOperation, ReceiptOperationResult,
    Request as ProtocolRequest, Response as ProtocolResponse, VERSION, supported_version,
};
use crate::{Engine, Error, QueryResponse};

pub const MAX_FRAME_BYTES: usize = crate::syntax::MAX_SOURCE_BYTES * 6 + 256;
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 64;
pub const MAX_CONCURRENT_READS: usize = 8;
const CONNECTION_POLL: Duration = Duration::from_millis(100);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const EXECUTION_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ServerStats {
    pub accepted_connections: usize,
    pub rejected_connections: usize,
    pub requests: usize,
    pub failed_requests: usize,
    pub concurrency: ConcurrencyStats,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct ConcurrencyStats {
    pub active_reads: usize,
    pub queued_reads: usize,
    pub active_writes: usize,
    pub queued_writes: usize,
    pub peak_active_reads: usize,
    pub max_active_reads: usize,
}

#[derive(Clone)]
pub struct ConcurrentEngine {
    inner: Arc<ConcurrentEngineInner>,
}

struct ConcurrentEngineInner {
    engine: Mutex<Engine>,
    read_slots: Mutex<usize>,
    read_ready: Condvar,
    active_reads: AtomicUsize,
    queued_reads: AtomicUsize,
    active_writes: AtomicUsize,
    queued_writes: AtomicUsize,
    peak_active_reads: AtomicUsize,
}

impl ConcurrentEngine {
    pub fn new(engine: Engine) -> Self {
        Self {
            inner: Arc::new(ConcurrentEngineInner {
                engine: Mutex::new(engine),
                read_slots: Mutex::new(0),
                read_ready: Condvar::new(),
                active_reads: AtomicUsize::new(0),
                queued_reads: AtomicUsize::new(0),
                active_writes: AtomicUsize::new(0),
                queued_writes: AtomicUsize::new(0),
                peak_active_reads: AtomicUsize::new(0),
            }),
        }
    }

    pub fn stats(&self) -> ConcurrencyStats {
        ConcurrencyStats {
            active_reads: self.inner.active_reads.load(Ordering::Relaxed),
            queued_reads: self.inner.queued_reads.load(Ordering::Relaxed),
            active_writes: self.inner.active_writes.load(Ordering::Relaxed),
            queued_writes: self.inner.queued_writes.load(Ordering::Relaxed),
            peak_active_reads: self.inner.peak_active_reads.load(Ordering::Relaxed),
            max_active_reads: MAX_CONCURRENT_READS,
        }
    }

    pub fn execute(&self, source: &str) -> QueryResponse {
        self.execute_until(source, Instant::now() + EXECUTION_TIMEOUT, None)
    }

    fn execute_until(
        &self,
        source: &str,
        deadline: Instant,
        shutdown: Option<&AtomicBool>,
    ) -> QueryResponse {
        if source_is_read_only(source) {
            match self.with_read_snapshot(deadline, shutdown, |snapshot| {
                snapshot.execute_with_params_until(
                    source,
                    std::collections::BTreeMap::new(),
                    None,
                    deadline,
                )
            }) {
                Ok(response) => response,
                Err(error) => self.failure_with_schema(error),
            }
        } else {
            self.with_writer(|engine| {
                engine.execute_with_params_until(
                    source,
                    std::collections::BTreeMap::new(),
                    None,
                    deadline,
                )
            })
        }
    }

    pub fn execute_protocol_request(&self, request: ProtocolRequest) -> ProtocolResponse {
        self.execute_protocol_request_until(request, Instant::now() + EXECUTION_TIMEOUT)
    }

    pub fn execute_protocol_request_until(
        &self,
        request: ProtocolRequest,
        deadline: Instant,
    ) -> ProtocolResponse {
        self.execute_protocol_request_until_shutdown(request, deadline, None)
    }

    /// Run maintenance that needs exclusive mutable access to the Engine.
    /// Reads already holding snapshots continue on their captured commit;
    /// snapshot capture and other writes wait until this operation returns.
    pub fn with_exclusive<T>(&self, operation: impl FnOnce(&mut Engine) -> T) -> T {
        self.with_writer(operation)
    }

    fn execute_protocol_request_until_shutdown(
        &self,
        request: ProtocolRequest,
        deadline: Instant,
        shutdown: Option<&AtomicBool>,
    ) -> ProtocolResponse {
        if protocol_request_is_read_only(&request) {
            let request_id = request.request_id.clone();
            let version = request.version;
            match self.with_read_snapshot(deadline, shutdown, |snapshot| {
                execute_protocol_request_until(snapshot, request, deadline)
            }) {
                Ok(response) => response,
                Err(error) => {
                    let mut response =
                        ProtocolResponse::failure(request_id, error, self.schema_info());
                    response.version = version;
                    response
                }
            }
        } else {
            self.with_writer(|engine| execute_protocol_request_until(engine, request, deadline))
        }
    }

    fn schema_info(&self) -> crate::SchemaInfo {
        self.inner
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .schema_info()
    }

    fn failure_with_schema(&self, error: Error) -> QueryResponse {
        let mut response = QueryResponse::failure(error);
        response.schema = Some(self.schema_info());
        response
    }

    fn with_writer<T>(&self, execute: impl FnOnce(&mut Engine) -> T) -> T {
        self.inner.queued_writes.fetch_add(1, Ordering::AcqRel);
        let mut engine = self
            .inner
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.inner.queued_writes.fetch_sub(1, Ordering::AcqRel);
        self.inner.active_writes.fetch_add(1, Ordering::AcqRel);
        struct ActiveWrite<'a>(&'a AtomicUsize);
        impl Drop for ActiveWrite<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _active = ActiveWrite(&self.inner.active_writes);
        execute(&mut engine)
    }

    fn with_read_snapshot<T>(
        &self,
        deadline: Instant,
        shutdown: Option<&AtomicBool>,
        execute: impl FnOnce(&mut Engine) -> T,
    ) -> Result<T, Error> {
        let mut permit = self.acquire_read(deadline, shutdown)?;
        let mut snapshot = self
            .inner
            .engine
            .lock()
            .map_err(|_| Error::new("E_INTERNAL", "engine lock poisoned"))?
            .read_snapshot();
        permit.start();
        let result = execute(&mut snapshot);
        drop(permit);
        Ok(result)
    }

    fn acquire_read(
        &self,
        deadline: Instant,
        shutdown: Option<&AtomicBool>,
    ) -> Result<ReadPermit<'_>, Error> {
        self.inner.queued_reads.fetch_add(1, Ordering::AcqRel);
        if shutdown.is_some_and(|signal| signal.load(Ordering::Acquire)) {
            self.inner.queued_reads.fetch_sub(1, Ordering::AcqRel);
            return Err(Error::new("E_SHUTDOWN", "server is shutting down"));
        }
        let mut active = self
            .inner
            .read_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *active >= MAX_CONCURRENT_READS {
            if shutdown.is_some_and(|signal| signal.load(Ordering::Acquire)) {
                self.inner.queued_reads.fetch_sub(1, Ordering::AcqRel);
                return Err(Error::new("E_SHUTDOWN", "server is shutting down"));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.inner.queued_reads.fetch_sub(1, Ordering::AcqRel);
                return Err(Error::new(
                    "E_TIMEOUT",
                    "request deadline expired while waiting for a read snapshot",
                ));
            };
            let wait = remaining.min(CONNECTION_POLL);
            let (next, _) = self
                .inner
                .read_ready
                .wait_timeout(active, wait)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active = next;
        }
        *active += 1;
        Ok(ReadPermit {
            owner: self,
            started: false,
        })
    }
}

struct ReadPermit<'a> {
    owner: &'a ConcurrentEngine,
    started: bool,
}

impl ReadPermit<'_> {
    fn start(&mut self) {
        debug_assert!(!self.started);
        self.started = true;
        self.owner.inner.queued_reads.fetch_sub(1, Ordering::AcqRel);
        let current = self.owner.inner.active_reads.fetch_add(1, Ordering::AcqRel) + 1;
        self.owner
            .inner
            .peak_active_reads
            .fetch_max(current, Ordering::AcqRel);
    }
}

impl Drop for ReadPermit<'_> {
    fn drop(&mut self) {
        let mut active = self
            .owner
            .inner
            .read_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active -= 1;
        if self.started {
            self.owner.inner.active_reads.fetch_sub(1, Ordering::AcqRel);
        } else {
            self.owner.inner.queued_reads.fetch_sub(1, Ordering::AcqRel);
        }
        self.owner.inner.read_ready.notify_one();
    }
}

fn source_is_read_only(source: &str) -> bool {
    crate::syntax::parse(source).map_or(true, |statements| {
        statements
            .iter()
            .all(|located| !located.statement.is_mutating())
    })
}

fn protocol_request_is_read_only(request: &ProtocolRequest) -> bool {
    match request.receipts.as_ref() {
        Some(ReceiptOperation::Prune { confirm: true, .. }) => false,
        Some(ReceiptOperation::Status | ReceiptOperation::Prune { .. }) => true,
        None if request.introspect.is_some() => true,
        None => source_is_read_only(&request.query),
    }
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
    fn snapshot(&self, concurrency: ConcurrencyStats) -> ServerStats {
        ServerStats {
            accepted_connections: self.accepted.load(Ordering::Relaxed),
            rejected_connections: self.rejected.load(Ordering::Relaxed),
            requests: self.requests.load(Ordering::Relaxed),
            failed_requests: self.failed.load(Ordering::Relaxed),
            concurrency,
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
    Legacy(Box<QueryResponse>),
    Versioned(Box<ProtocolResponse>),
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
                "version": response.version,
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
        "unionid server stopped: accepted={}, rejected={}, requests={}, failed={}, peak_reads={}, active_reads={}, queued_reads={}, active_writes={}, queued_writes={}",
        stats.accepted_connections,
        stats.rejected_connections,
        stats.requests,
        stats.failed_requests,
        stats.concurrency.peak_active_reads,
        stats.concurrency.active_reads,
        stats.concurrency.queued_reads,
        stats.concurrency.active_writes,
        stats.concurrency.queued_writes,
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
    serve_until_concurrent(listener, ConcurrentEngine::new(engine), shutdown)
}

/// Serve TCP requests through a reusable concurrent execution boundary.
/// Keeping a clone lets an embedding application inspect live queue/active
/// counts or reuse the same committed state from an HTTP adapter.
pub fn serve_until_concurrent(
    listener: TcpListener,
    engine: ConcurrentEngine,
    shutdown: Arc<AtomicBool>,
) -> Result<ServerStats, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("set listener nonblocking: {error}"))?;
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
                let engine = engine.clone();
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
    let concurrency = engine.stats();
    drop(engine);
    Ok(stats.snapshot(concurrency))
}

fn reject_busy(mut stream: TcpStream) -> Result<(), String> {
    // Rejection must not create another worker or leave the accept loop waiting
    // indefinitely on a client that does not read its response.
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|e| format!("set rejection timeout: {e}"))?;
    write_response(
        &mut stream,
        &OutgoingResponse::Legacy(Box::new(QueryResponse::failure(Error::new(
            "E_BUSY",
            "active connection limit reached; retry after a connection closes",
        )))),
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
    engine: ConcurrentEngine,
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
            OutgoingResponse::Legacy(Box::new(QueryResponse::failure(Error::new(
                "E_LIMIT",
                "request frame too large",
            ))))
        } else if quit {
            OutgoingResponse::Legacy(Box::new(QueryResponse::ok_message("bye")))
        } else {
            if input.starts_with('{') {
                execute_json_request_concurrent(input, &engine, &shutdown)
            } else {
                OutgoingResponse::Legacy(Box::new(engine.execute_until(
                    input,
                    Instant::now() + EXECUTION_TIMEOUT,
                    Some(&shutdown),
                )))
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

fn execute_json_request_concurrent(
    input: &str,
    engine: &ConcurrentEngine,
    shutdown: &AtomicBool,
) -> OutgoingResponse {
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
            Ok(request) => OutgoingResponse::Legacy(Box::new(engine.execute_until(
                &request.query,
                Instant::now() + EXECUTION_TIMEOUT,
                Some(shutdown),
            ))),
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
    let version = decoded
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or(VERSION);
    let request = match serde_json::from_value::<ProtocolRequest>(decoded) {
        Ok(request) => request,
        Err(error) => {
            let mut response = ProtocolResponse::failure(
                request_id,
                Error::new("E_PROTOCOL", format!("invalid versioned request: {error}")),
                engine.schema_info(),
            );
            response.version = version;
            return OutgoingResponse::Versioned(Box::new(response));
        }
    };
    OutgoingResponse::Versioned(Box::new(engine.execute_protocol_request_until_shutdown(
        request,
        Instant::now() + EXECUTION_TIMEOUT,
        Some(shutdown),
    )))
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

#[cfg(test)]
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
            Ok(request) => OutgoingResponse::Legacy(Box::new(engine.execute_with_params_until(
                &request.query,
                std::collections::BTreeMap::new(),
                None,
                Instant::now() + EXECUTION_TIMEOUT,
            ))),
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
    let version = decoded
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or(VERSION);
    let request = match serde_json::from_value::<ProtocolRequest>(decoded) {
        Ok(request) => request,
        Err(error) => {
            let mut response = ProtocolResponse::failure(
                request_id,
                Error::new("E_PROTOCOL", format!("invalid versioned request: {error}")),
                engine.schema_info(),
            );
            response.version = version;
            return OutgoingResponse::Versioned(Box::new(response));
        }
    };
    OutgoingResponse::Versioned(Box::new(execute_protocol_request(engine, request)))
}

/// Execute the stable versioned data protocol independently of its transport.
///
/// TCP uses this directly, and HTTP adapters can deserialize the same
/// `ProtocolRequest` and serialize the returned `ProtocolResponse` without
/// duplicating version, parameter, schema, or introspection semantics.
pub fn execute_protocol_request(engine: &mut Engine, request: ProtocolRequest) -> ProtocolResponse {
    execute_protocol_request_until(engine, request, Instant::now() + EXECUTION_TIMEOUT)
}

/// Execute a versioned protocol request with an adapter-owned deadline.
///
/// HTTP adapters can use a shorter service budget while retaining the exact
/// validation, page, typed-value, and error semantics used by the TCP server.
pub fn execute_protocol_request_until(
    engine: &mut Engine,
    request: ProtocolRequest,
    deadline: Instant,
) -> ProtocolResponse {
    let version = request.version;
    let mut response = execute_versioned_request(engine, request, deadline);
    response.version = version;
    response
}

fn execute_versioned_request(
    engine: &mut Engine,
    request: ProtocolRequest,
    deadline: Instant,
) -> ProtocolResponse {
    if !supported_version(request.version) {
        return ProtocolResponse::failure(
            request.request_id,
            Error::new(
                "E_PROTOCOL_VERSION",
                format!(
                    "unsupported protocol version {}; supported versions are 1 and 2",
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
    if let Some(operation) = request.receipts {
        if !request.query.is_empty()
            || !request.params.is_empty()
            || request.schema.is_some()
            || request.introspect.is_some()
            || request.idempotency_key.is_some()
            || request.page.is_some()
        {
            return ProtocolResponse::failure(
                request.request_id,
                Error::new(
                    "E_PROTOCOL",
                    "receipt operations cannot include query, params, schema, introspection, an idempotency key, or page",
                ),
                engine.schema_info(),
            );
        }
        let result = match operation {
            ReceiptOperation::Status => engine
                .idempotency_status()
                .map(ReceiptOperationResult::Status),
            ReceiptOperation::Prune { options, confirm } if confirm => engine
                .prune_idempotency_receipts(options)
                .map(ReceiptOperationResult::Prune),
            ReceiptOperation::Prune { options, .. } => engine
                .plan_idempotency_prune(options)
                .map(ReceiptOperationResult::Prune),
        };
        return match result {
            Ok(result) => ProtocolResponse::from_receipt_operation(
                request.request_id,
                result,
                engine.schema_info(),
            ),
            Err(error) => {
                ProtocolResponse::failure(request.request_id, error, engine.schema_info())
            }
        };
    }
    if request.introspect.is_some() {
        if request.idempotency_key.is_some() {
            return ProtocolResponse::failure(
                request.request_id,
                Error::new(
                    "E_IDEMPOTENCY_NOT_MUTATION",
                    "idempotency keys are not valid for introspection requests",
                ),
                engine.schema_info(),
            );
        }
        if !request.query.is_empty()
            || !request.params.is_empty()
            || request.schema.is_some()
            || request.page.is_some()
        {
            return ProtocolResponse::failure(
                request.request_id,
                Error::new(
                    "E_PROTOCOL",
                    "introspection requests cannot include query, params, schema, or page",
                ),
                engine.schema_info(),
            );
        }
        if request.version == VERSION
            && let Err(error) = engine.preflight_protocol_v1_introspection()
        {
            return ProtocolResponse::failure(request.request_id, error, engine.schema_info());
        }
        return introspection_protocol_response(request.request_id, engine.introspection());
    }
    let digest = if request.idempotency_key.is_some() {
        match request.canonical_digest() {
            Ok(digest) => Some(digest),
            Err(error) => {
                return ProtocolResponse::failure(request.request_id, error, engine.schema_info());
            }
        }
    } else {
        None
    };
    if request.idempotency_key.is_some() && request.page.is_some() {
        return ProtocolResponse::failure(
            request.request_id,
            Error::new(
                "E_PAGE_SHAPE",
                "page requests are read-only and cannot use an idempotency key",
            ),
            engine.schema_info(),
        );
    }
    let parameters = match request.decode_params() {
        Ok(parameters) => parameters,
        Err(error) => {
            return ProtocolResponse::failure(request.request_id, error, engine.schema_info());
        }
    };
    if request.version == VERSION {
        let replay = match (&request.idempotency_key, &digest) {
            (Some(key), Some(digest)) => match engine.preflight_protocol_v1_receipt(key, digest) {
                Ok(replay) => replay,
                Err(error) => {
                    return ProtocolResponse::failure(
                        request.request_id,
                        error,
                        engine.schema_info(),
                    );
                }
            },
            _ => false,
        };
        if !replay
            && let Err(error) =
                engine.preflight_protocol_v1(&request.query, &parameters, request.page.clone())
        {
            return ProtocolResponse::failure(request.request_id, error, engine.schema_info());
        }
    }
    if let (Some(key), Some(digest)) = (request.idempotency_key, digest) {
        return match engine.execute_idempotent_with_params_until(
            &key,
            &digest,
            &request.query,
            parameters,
            request.schema.as_ref(),
            deadline,
        ) {
            Ok(result) => ProtocolResponse::from_idempotent(request.request_id, key, result),
            Err(error) => {
                ProtocolResponse::failure(request.request_id, error, engine.schema_info())
            }
        };
    }
    let response = if let Some(page) = request.page {
        engine.execute_with_params_page_until(
            &request.query,
            parameters,
            request.schema.as_ref(),
            page,
            deadline,
        )
    } else {
        engine.execute_with_params_until(
            &request.query,
            parameters,
            request.schema.as_ref(),
            deadline,
        )
    };
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
    OutgoingResponse::Legacy(Box::new(QueryResponse::failure(error)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition did not become true");
            std::thread::yield_now();
        }
    }

    #[test]
    fn read_snapshot_observes_one_commit_without_blocking_the_next_write() {
        let shared = ConcurrentEngine::new(Engine::memory());
        assert!(shared.execute("create table items (id int, value text)").ok);
        assert!(shared.execute("insert items {id: 1, value: \"before\"}").ok);

        let mut permit = shared
            .acquire_read(Instant::now() + Duration::from_secs(2), None)
            .unwrap();
        let mut snapshot = { shared.inner.engine.lock().unwrap().read_snapshot() };
        permit.start();
        let writer = shared.clone();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let response = writer.execute("update items | set value = \"after\"");
            sent.send(response).unwrap();
        });
        let write = received
            .recv_timeout(Duration::from_secs(2))
            .expect("a held read snapshot must not retain the writer mutex");
        assert!(write.ok, "{}", write.message);

        let old = snapshot.execute("from items | select value");
        assert!(old.ok, "{}", old.message);
        assert!(matches!(
            &old.rows[0]["value"],
            crate::Value::Text(value) if value == "before"
        ));
        drop(permit);

        let current = shared.execute("from items | select value");
        assert!(current.ok, "{}", current.message);
        assert!(matches!(
            &current.rows[0]["value"],
            crate::Value::Text(value) if value == "after"
        ));
        assert_eq!(shared.stats().peak_active_reads, 1);
    }

    #[test]
    fn read_snapshot_admission_is_bounded_and_shutdown_aware() {
        let shared = ConcurrentEngine::new(Engine::memory());
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut permits = (0..MAX_CONCURRENT_READS)
            .map(|_| {
                let mut permit = shared.acquire_read(deadline, None).unwrap();
                permit.start();
                permit
            })
            .collect::<Vec<_>>();
        assert_eq!(shared.stats().active_reads, MAX_CONCURRENT_READS);

        let waiting = shared.clone();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut permit = waiting
                .acquire_read(Instant::now() + Duration::from_secs(2), None)
                .unwrap();
            permit.start();
            sent.send(true).unwrap();
        });
        wait_for(|| shared.stats().queued_reads == 1);
        permits.pop();
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap());

        let shutdown = AtomicBool::new(true);
        let error = shared
            .acquire_read(Instant::now() + Duration::from_secs(2), Some(&shutdown))
            .err()
            .unwrap();
        assert_eq!(error.code, "E_SHUTDOWN");
        drop(permits);
        wait_for(|| shared.stats().active_reads == 0);
        assert_eq!(shared.stats().max_active_reads, MAX_CONCURRENT_READS);
    }

    #[test]
    fn service_stats_expose_a_queued_writer() {
        let shared = ConcurrentEngine::new(Engine::memory());
        let lock = shared.inner.engine.lock().unwrap();
        let waiting = shared.clone();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sent.send(waiting.execute("create table items (id int)"))
                .unwrap()
        });
        wait_for(|| shared.stats().queued_writes == 1);
        assert_eq!(shared.stats().active_writes, 0);
        drop(lock);
        assert!(received.recv_timeout(Duration::from_secs(2)).unwrap().ok);
        assert_eq!(shared.stats().queued_writes, 0);
    }

    #[test]
    fn response_encoder_stops_at_the_byte_limit() {
        let small = OutgoingResponse::Legacy(Box::new(QueryResponse::ok_message("ok")));
        assert!(encode_limited(&small).is_ok());
        let oversized = OutgoingResponse::Versioned(Box::new(ProtocolResponse::from_query(
            "large-request",
            QueryResponse::ok_message("x".repeat(MAX_RESPONSE_BYTES)),
        )));
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

    #[test]
    fn version_two_parse_and_limit_errors_keep_the_request_version() {
        let mut engine = Engine::memory();
        let input = r#"{"version":2,"request_id":"bad-v2","query":"create table items (id int)","unknown":true}"#;
        let OutgoingResponse::Versioned(response) = execute_json_request(input, &mut engine) else {
            panic!("expected a versioned response");
        };
        assert_eq!(response.version, 2);
        assert_eq!(response.request_id, "bad-v2");
        assert_eq!(response.error.unwrap().code, "E_PROTOCOL");
        assert_eq!(engine.execute("from items").error.unwrap().code, "E_TABLE");
        let mut response =
            ProtocolResponse::from_query("large-v2", QueryResponse::ok_message("ok"));
        response.version = 2;
        let fallback = OutgoingResponse::Versioned(Box::new(response)).limit_fallback();
        assert_eq!(fallback["version"], 2);
        assert_eq!(fallback["error"]["code"], "E_LIMIT");
    }
}
