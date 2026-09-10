use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use unionid::protocol::{PRODUCTION_VERSION, Request, Response};
use unionid::server::{ConcurrencyStats, ConcurrentEngine, serve_until_concurrent};
use unionid::{Engine, PageSpec};

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const WARMUPS: usize = 3;
const MIN_ROWS: usize = 100;
const MAX_ROWS: usize = 100_000;
const MAX_REQUESTS: usize = 10_000;

const SCHEMA: &str = r#"type State = Pending | Active {owner text} | Done
type Task =
  id int
  tenant int
  title text
  state State
  tags list text
  touches int
table tasks Task
  key id
create index tasks (tenant, id)
"#;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Adapter {
    Embedded,
    Tcp,
    Http,
}

impl Adapter {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "embedded" => Ok(Self::Embedded),
            "tcp" => Ok(Self::Tcp),
            "http" => Ok(Self::Http),
            _ => Err(format!("unknown adapter '{value}'").into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Tcp => "tcp",
            Self::Http => "http",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CaseReport {
    adapter: Adapter,
    rows: usize,
    clients: usize,
    read_percent: usize,
    requests_per_client: usize,
    warmups: usize,
    samples_micros: Vec<u64>,
    p50_micros: u64,
    p95_micros: u64,
    p99_micros: u64,
    elapsed_micros: u64,
    throughput_per_second: f64,
    point_reads: usize,
    pages: usize,
    conditional_updates: usize,
    batch_writes: usize,
    sequence_min: String,
    sequence_max: String,
    errors: BTreeMap<String, usize>,
    queue: QueueReport,
    peak_rss_bytes: u64,
    database_bytes: u64,
    schema_revision: u64,
    schema_hash: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueueReport {
    read_admissions: u64,
    read_wait_total_micros: u64,
    read_wait_max_micros: u64,
    write_admissions: u64,
    write_wait_total_micros: u64,
    write_wait_max_micros: u64,
    peak_active_reads: usize,
}

#[derive(Serialize)]
struct EvaluationReport {
    evidence_only_not_sla: bool,
    environment: EnvironmentReport,
    rows: usize,
    requests_per_client: usize,
    cases: Vec<CaseReport>,
}

#[derive(Serialize)]
struct EnvironmentReport {
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    rustc: String,
}

#[derive(Default)]
struct Counts {
    point_reads: usize,
    pages: usize,
    conditional_updates: usize,
    batch_writes: usize,
}

struct Sample {
    micros: u64,
    sequence: u64,
    kind: RequestKind,
}

#[derive(Clone, Copy)]
enum RequestKind {
    Point,
    Page,
    Update,
    Batch,
}

struct AdapterRuntime {
    engine: ConcurrentEngine,
    endpoint: Endpoint,
    shutdown: Option<Arc<AtomicBool>>,
    server: Option<JoinHandle<AnyResult<()>>>,
}

#[derive(Clone)]
enum Endpoint {
    Embedded(ConcurrentEngine),
    Tcp(SocketAddr),
    Http(SocketAddr),
}

enum Client {
    Embedded(ConcurrentEngine),
    Tcp {
        writer: TcpStream,
        reader: BufReader<TcpStream>,
    },
    Http(SocketAddr),
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("service load evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, command, root, adapter, rows, clients, read_percent, requests]
            if command == "measure" =>
        {
            let report = measure(
                Path::new(root),
                Adapter::parse(adapter)?,
                rows.parse()?,
                clients.parse()?,
                read_percent.parse()?,
                requests.parse()?,
            )?;
            serde_json::to_writer(std::io::stdout(), &report)?;
            Ok(())
        }
        [_, root, rows, requests] => evaluate(Path::new(root), rows.parse()?, requests.parse()?, false),
        [_, root, rows, requests, flag] if flag == "--smoke" => {
            evaluate(Path::new(root), rows.parse()?, requests.parse()?, true)
        }
        _ => Err("usage: unionid-service-load-eval <work-directory> <rows> <requests-per-client> [--smoke]".into()),
    }
}

fn evaluate(root: &Path, rows: usize, requests: usize, smoke: bool) -> AnyResult<()> {
    validate(rows, if smoke { 2 } else { 8 }, 50, requests)?;
    if root.exists() {
        return Err(format!("work directory already exists: {}", root.display()).into());
    }
    fs::create_dir_all(root)?;
    let executable = std::env::current_exe()?;
    let adapters = [Adapter::Embedded, Adapter::Tcp, Adapter::Http];
    let clients: &[usize] = if smoke { &[2] } else { &[1, 4, 8] };
    let read_percents: &[usize] = if smoke { &[50] } else { &[90, 50] };
    let mut cases = Vec::new();
    for adapter in adapters {
        for &client_count in clients {
            for &read_percent in read_percents {
                let case_root =
                    root.join(format!("{}-{client_count}-{read_percent}", adapter.name()));
                let output = Command::new(&executable)
                    .args([
                        "measure",
                        case_root.to_str().ok_or("work path is not UTF-8")?,
                        adapter.name(),
                        &rows.to_string(),
                        &client_count.to_string(),
                        &read_percent.to_string(),
                        &requests.to_string(),
                    ])
                    .output()?;
                if !output.status.success() {
                    return Err(format!(
                        "{} {client_count} clients {read_percent}% reads failed: {}",
                        adapter.name(),
                        String::from_utf8_lossy(&output.stderr)
                    )
                    .into());
                }
                cases.push(serde_json::from_slice(&output.stdout)?);
            }
        }
    }
    let report = EvaluationReport {
        evidence_only_not_sla: true,
        environment: environment()?,
        rows,
        requests_per_client: requests,
        cases,
    };
    serde_json::to_writer_pretty(std::io::stdout(), &report)?;
    println!();
    Ok(())
}

fn measure(
    root: &Path,
    adapter: Adapter,
    rows: usize,
    clients: usize,
    read_percent: usize,
    requests: usize,
) -> AnyResult<CaseReport> {
    validate(rows, clients, read_percent, requests)?;
    fs::create_dir_all(root)?;
    let database = root.join("service-load.redb");
    prepare(&database, rows)?;
    let mut engine = Engine::open_redb(&database)?;
    let integrity = engine.check_integrity()?;
    if !integrity.backend_clean || integrity.versions.format != 6 {
        return Err("prepared database did not pass format-6 integrity checking".into());
    }
    let schema = engine.schema_info();
    let warmup_runtime = AdapterRuntime::start(adapter, engine)?;

    let mut warmup = Client::connect(&warmup_runtime.endpoint)?;
    for index in 0..WARMUPS {
        let request = build_request(RequestKind::Point, 0, index, rows)?;
        validate_response(RequestKind::Point, &warmup.send(&request)?, &schema)?;
    }
    drop(warmup);
    warmup_runtime.stop()?;

    // Queue observations are lifetime aggregates. Reopen the prepared database so
    // the measured runtime starts with clean counters after adapter-specific warmup.
    let runtime = AdapterRuntime::start(adapter, Engine::open_redb(&database)?)?;
    let baseline = runtime.engine.stats();
    let barrier = Arc::new(Barrier::new(clients + 1));
    let started = Instant::now();
    let mut handles = Vec::new();
    for client_id in 0..clients {
        let endpoint = runtime.endpoint.clone();
        let barrier = Arc::clone(&barrier);
        let schema = schema.clone();
        handles.push(std::thread::spawn(move || -> AnyResult<Vec<Sample>> {
            let mut client = Client::connect(&endpoint)?;
            let mut samples = Vec::with_capacity(requests);
            barrier.wait();
            for index in 0..requests {
                let kind = request_kind(index, read_percent);
                let request = build_request(kind, client_id, index, rows)?;
                let sample_started = Instant::now();
                let response = client.send(&request)?;
                let micros = elapsed_micros(sample_started.elapsed());
                let sequence = validate_response(kind, &response, &schema)?;
                samples.push(Sample {
                    micros,
                    sequence,
                    kind,
                });
            }
            Ok(samples)
        }));
    }
    barrier.wait();
    let mut samples = Vec::with_capacity(clients * requests);
    for handle in handles {
        samples.extend(handle.join().map_err(|_| "load client panicked")??);
    }
    let elapsed = started.elapsed();
    let final_stats = runtime.engine.stats();
    runtime.stop()?;

    let mut verifier = Engine::open_redb(&database)?;
    let integrity = verifier.check_integrity()?;
    if !integrity.backend_clean || verifier.schema_info() != schema {
        return Err("post-load integrity or schema validation failed".into());
    }
    drop(verifier);
    summarize(
        adapter,
        rows,
        clients,
        read_percent,
        requests,
        samples,
        elapsed,
        queue_delta(baseline, final_stats),
        fs::metadata(database)?.len(),
        schema,
    )
}

fn prepare(path: &Path, rows: usize) -> AnyResult<()> {
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    require_ok(engine.execute(SCHEMA))?;
    for start in (0..rows).step_by(500) {
        let end = rows.min(start + 500);
        let values = (start..end).map(record).collect::<Vec<_>>().join(",");
        require_ok(engine.execute(&format!("insert many tasks [{values}]")))?;
    }
    let response = require_ok(engine.execute("from tasks | aggregate {total = count}"))?;
    if !matches!(&response.rows[0]["total"], unionid::Value::Int(total) if *total == rows as i64) {
        return Err("prepared row count mismatch".into());
    }
    Ok(())
}

fn record(id: usize) -> String {
    let state = match id % 3 {
        0 => "Pending".to_owned(),
        1 => format!("Active {{owner = \"worker-{:02}\"}}", id % 16),
        _ => "Done".to_owned(),
    };
    format!(
        "{{id = {id}, tenant = {}, title = \"task-{id:06}-fixed-width\", state = {state}, tags = [\"service\", \"mixed\"], touches = 0}}",
        id % 16
    )
}

fn request_kind(index: usize, read_percent: usize) -> RequestKind {
    let slot = (index * 37 + 11) % 100;
    if slot < read_percent {
        if index.is_multiple_of(2) {
            RequestKind::Point
        } else {
            RequestKind::Page
        }
    } else if index.is_multiple_of(2) {
        RequestKind::Update
    } else {
        RequestKind::Batch
    }
}

fn build_request(
    kind: RequestKind,
    client: usize,
    index: usize,
    rows: usize,
) -> AnyResult<Request> {
    let request_id = format!("load-{client}-{index}");
    let mut request = match kind {
        RequestKind::Point => Request::query(
            request_id,
            format!(
                "from tasks\nfilter id == {}\nsort id",
                (client * 97 + index) % rows
            ),
        )
        .with_page(PageSpec::forward(1)),
        RequestKind::Page => Request::query(
            request_id,
            format!(
                "from tasks\nfilter tenant == {}\nsort {{tenant, id}}",
                (client + index) % 16
            ),
        )
        .with_page(PageSpec::forward(5)),
        RequestKind::Update => {
            let id = (client * 97 + index) % rows;
            Request::query(
                request_id,
                format!("update tasks\nfilter id == {id}\nset touches = touches + 1\nreturning id"),
            )
            .with_idempotency_key(format!("load-update-{client}-{index}"))?
        }
        RequestKind::Batch => {
            let base = rows + (client * MAX_REQUESTS + index) * 2;
            Request::query(
                request_id,
                format!(
                    "insert many tasks [{},{}]\nreturning id",
                    record(base),
                    record(base + 1)
                ),
            )
            .with_idempotency_key(format!("load-batch-{client}-{index}"))?
        }
    };
    request = request.with_version(PRODUCTION_VERSION)?;
    Ok(request)
}

fn validate_response(
    kind: RequestKind,
    response: &Response,
    schema: &unionid::SchemaInfo,
) -> AnyResult<u64> {
    if !response.ok {
        let code = response
            .error
            .as_ref()
            .map_or("E_UNKNOWN", |error| &error.code);
        return Err(format!("request failed with {code}: {}", response.message).into());
    }
    if response.version != PRODUCTION_VERSION || response.schema.as_ref() != Some(schema) {
        return Err("response protocol or schema identity mismatch".into());
    }
    let sequence = match kind {
        RequestKind::Point => {
            if response.rows.len() != 1 || response.page.as_ref().map(|page| page.limit) != Some(1)
            {
                return Err("point-read response shape mismatch".into());
            }
            response.page.as_ref().unwrap().snapshot_sequence.parse()?
        }
        RequestKind::Page => {
            if response.rows.is_empty()
                || response.rows.len() > 5
                || response.page.as_ref().map(|page| page.limit) != Some(5)
            {
                return Err("page response shape mismatch".into());
            }
            response.page.as_ref().unwrap().snapshot_sequence.parse()?
        }
        RequestKind::Update => {
            if response.affected_rows != Some(1) || response.rows.len() != 1 {
                return Err("conditional-update response shape mismatch".into());
            }
            response
                .idempotency
                .as_ref()
                .ok_or("update has no idempotency metadata")?
                .committed_sequence
                .parse()?
        }
        RequestKind::Batch => {
            if response.affected_rows != Some(2) || response.rows.len() != 2 {
                return Err("batch response shape mismatch".into());
            }
            response
                .idempotency
                .as_ref()
                .ok_or("batch has no idempotency metadata")?
                .committed_sequence
                .parse()?
        }
    };
    Ok(sequence)
}

impl AdapterRuntime {
    fn start(adapter: Adapter, engine: Engine) -> AnyResult<Self> {
        let engine = ConcurrentEngine::new(engine);
        match adapter {
            Adapter::Embedded => Ok(Self {
                endpoint: Endpoint::Embedded(engine.clone()),
                engine,
                shutdown: None,
                server: None,
            }),
            Adapter::Tcp => {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                let address = listener.local_addr()?;
                let shutdown = Arc::new(AtomicBool::new(false));
                let server_engine = engine.clone();
                let server_shutdown = Arc::clone(&shutdown);
                let server = std::thread::spawn(move || {
                    serve_until_concurrent(listener, server_engine, server_shutdown)
                        .map(|_| ())
                        .map_err(Into::into)
                });
                Ok(Self {
                    endpoint: Endpoint::Tcp(address),
                    engine,
                    shutdown: Some(shutdown),
                    server: Some(server),
                })
            }
            Adapter::Http => {
                let listener = TcpListener::bind("127.0.0.1:0")?;
                listener.set_nonblocking(true)?;
                let address = listener.local_addr()?;
                let shutdown = Arc::new(AtomicBool::new(false));
                let server_engine = engine.clone();
                let server_shutdown = Arc::clone(&shutdown);
                let server = std::thread::spawn(move || {
                    serve_http(listener, server_engine, server_shutdown)
                });
                Ok(Self {
                    endpoint: Endpoint::Http(address),
                    engine,
                    shutdown: Some(shutdown),
                    server: Some(server),
                })
            }
        }
    }

    fn stop(mut self) -> AnyResult<()> {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.store(true, Ordering::Release);
        }
        if let Some(server) = self.server.take() {
            server.join().map_err(|_| "adapter server panicked")??;
        }
        Ok(())
    }
}

impl Client {
    fn connect(endpoint: &Endpoint) -> AnyResult<Self> {
        match endpoint {
            Endpoint::Embedded(engine) => Ok(Self::Embedded(engine.clone())),
            Endpoint::Tcp(address) => {
                let writer = TcpStream::connect(address)?;
                writer.set_read_timeout(Some(Duration::from_secs(25)))?;
                writer.set_write_timeout(Some(Duration::from_secs(25)))?;
                let reader = BufReader::new(writer.try_clone()?);
                Ok(Self::Tcp { writer, reader })
            }
            Endpoint::Http(address) => Ok(Self::Http(*address)),
        }
    }

    fn send(&mut self, request: &Request) -> AnyResult<Response> {
        match self {
            Self::Embedded(engine) => Ok(engine.execute_protocol_request(request.clone())),
            Self::Tcp { writer, reader } => {
                serde_json::to_writer(&mut *writer, request)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                let mut line = String::new();
                reader.read_line(&mut line)?;
                if line.is_empty() {
                    return Err("TCP adapter closed before a response".into());
                }
                Ok(serde_json::from_str(&line)?)
            }
            Self::Http(address) => post_http(*address, request),
        }
    }
}

fn serve_http(
    listener: TcpListener,
    engine: ConcurrentEngine,
    shutdown: Arc<AtomicBool>,
) -> AnyResult<()> {
    let active = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                active.fetch_add(1, Ordering::AcqRel);
                let active = Arc::clone(&active);
                let engine = engine.clone();
                std::thread::spawn(move || {
                    let _guard = ActiveConnection(active);
                    if let Err(error) = handle_http(stream, engine) {
                        eprintln!("HTTP load adapter error: {error}");
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    while active.load(Ordering::Acquire) != 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn handle_http(mut stream: TcpStream, engine: ConcurrentEngine) -> AnyResult<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(25)))?;
    stream.set_write_timeout(Some(Duration::from_secs(25)))?;
    let body = read_http_body(&mut stream)?;
    let request: Request = serde_json::from_slice(&body)?;
    let response = engine.execute_protocol_request(request);
    let encoded = serde_json::to_vec(&response)?;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        encoded.len()
    )?;
    stream.write_all(&encoded)?;
    stream.flush()?;
    Ok(())
}

fn read_http_body(stream: &mut TcpStream) -> AnyResult<Vec<u8>> {
    let mut input = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("HTTP request ended before headers".into());
        }
        input.extend_from_slice(&buffer[..count]);
        if input.len() > unionid::server::MAX_FRAME_BYTES + 8192 {
            return Err("HTTP request exceeds bounded input".into());
        }
        if let Some(position) = input.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&input[..header_end])?;
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim())
        })
        .ok_or("HTTP request has no content-length")?
        .parse::<usize>()?;
    if length > unionid::server::MAX_FRAME_BYTES {
        return Err("HTTP body exceeds protocol frame limit".into());
    }
    while input.len() < header_end + length {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err("HTTP request ended before body".into());
        }
        input.extend_from_slice(&buffer[..count]);
    }
    Ok(input[header_end..header_end + length].to_vec())
}

fn post_http(address: SocketAddr, request: &Request) -> AnyResult<Response> {
    let body = serde_json::to_vec(request)?;
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(25)))?;
    stream.set_write_timeout(Some(Duration::from_secs(25)))?;
    write!(
        stream,
        "POST /v2/query HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let body_start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("HTTP response has no body")?
        + 4;
    Ok(serde_json::from_slice(&response[body_start..])?)
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    adapter: Adapter,
    rows: usize,
    clients: usize,
    read_percent: usize,
    requests: usize,
    samples: Vec<Sample>,
    elapsed: Duration,
    queue: QueueReport,
    database_bytes: u64,
    schema: unionid::SchemaInfo,
) -> AnyResult<CaseReport> {
    if samples.len() != clients * requests {
        return Err("accepted sample count mismatch".into());
    }
    let mut counts = Counts::default();
    let mut sequence_min = u64::MAX;
    let mut sequence_max = 0;
    let mut latencies = Vec::with_capacity(samples.len());
    for sample in samples {
        latencies.push(sample.micros);
        sequence_min = sequence_min.min(sample.sequence);
        sequence_max = sequence_max.max(sample.sequence);
        match sample.kind {
            RequestKind::Point => counts.point_reads += 1,
            RequestKind::Page => counts.pages += 1,
            RequestKind::Update => counts.conditional_updates += 1,
            RequestKind::Batch => counts.batch_writes += 1,
        }
    }
    latencies.sort_unstable();
    Ok(CaseReport {
        adapter,
        rows,
        clients,
        read_percent,
        requests_per_client: requests,
        warmups: WARMUPS,
        p50_micros: percentile(&latencies, 50),
        p95_micros: percentile(&latencies, 95),
        p99_micros: percentile(&latencies, 99),
        samples_micros: latencies,
        elapsed_micros: elapsed_micros(elapsed),
        throughput_per_second: (clients * requests) as f64 / elapsed.as_secs_f64(),
        point_reads: counts.point_reads,
        pages: counts.pages,
        conditional_updates: counts.conditional_updates,
        batch_writes: counts.batch_writes,
        sequence_min: sequence_min.to_string(),
        sequence_max: sequence_max.to_string(),
        errors: BTreeMap::new(),
        queue,
        peak_rss_bytes: peak_rss_bytes()?,
        database_bytes,
        schema_revision: schema.revision,
        schema_hash: schema.hash,
    })
}

fn queue_delta(before: ConcurrencyStats, after: ConcurrencyStats) -> QueueReport {
    QueueReport {
        read_admissions: after.read_admissions.saturating_sub(before.read_admissions),
        read_wait_total_micros: after
            .read_queue_wait_micros
            .saturating_sub(before.read_queue_wait_micros),
        read_wait_max_micros: after.max_read_queue_wait_micros,
        write_admissions: after
            .write_admissions
            .saturating_sub(before.write_admissions),
        write_wait_total_micros: after
            .write_queue_wait_micros
            .saturating_sub(before.write_queue_wait_micros),
        write_wait_max_micros: after.max_write_queue_wait_micros,
        peak_active_reads: after.peak_active_reads,
    }
}

fn percentile(sorted: &[u64], percent: usize) -> u64 {
    let rank = sorted.len().saturating_mul(percent).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn elapsed_micros(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
}

fn require_ok(response: unionid::QueryResponse) -> AnyResult<unionid::QueryResponse> {
    if response.ok {
        Ok(response)
    } else {
        Err(response.message.into())
    }
}

fn validate(rows: usize, clients: usize, read_percent: usize, requests: usize) -> AnyResult<()> {
    if !(MIN_ROWS..=MAX_ROWS).contains(&rows) {
        return Err(format!("rows must be between {MIN_ROWS} and {MAX_ROWS}").into());
    }
    if !matches!(clients, 1 | 2 | 4 | 8) {
        return Err("clients must be 1, 2, 4, or 8".into());
    }
    if !matches!(read_percent, 50 | 90) {
        return Err("read percent must be 50 or 90".into());
    }
    if requests == 0 || requests > MAX_REQUESTS {
        return Err(format!("requests per client must be between 1 and {MAX_REQUESTS}").into());
    }
    Ok(())
}

fn environment() -> AnyResult<EnvironmentReport> {
    Ok(EnvironmentReport {
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        logical_cpus: std::thread::available_parallelism()?.get(),
        rustc: String::from_utf8(Command::new("rustc").arg("--version").output()?.stdout)?
            .trim()
            .to_owned(),
    })
}

fn peak_rss_bytes() -> AnyResult<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the provided structure on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success.
    let peak = u64::try_from(unsafe { usage.assume_init() }.ru_maxrss)?;
    Ok(if cfg!(target_os = "macos") {
        peak
    } else {
        peak.saturating_mul(1024)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles_keep_the_small_sample_tail() {
        let samples = (1..=24).collect::<Vec<_>>();
        assert_eq!(percentile(&samples, 50), 12);
        assert_eq!(percentile(&samples, 95), 23);
        assert_eq!(percentile(&samples, 99), 24);
    }

    #[test]
    fn deterministic_mix_contains_expected_operations() {
        let kinds = (0..100)
            .map(|index| request_kind(index, 90))
            .collect::<Vec<_>>();
        let reads = kinds
            .iter()
            .filter(|kind| matches!(kind, RequestKind::Point | RequestKind::Page))
            .count();
        assert_eq!(reads, 90);
        assert!(kinds.iter().any(|kind| matches!(kind, RequestKind::Point)));
        assert!(kinds.iter().any(|kind| matches!(kind, RequestKind::Page)));
        assert!(kinds.iter().any(|kind| matches!(kind, RequestKind::Update)));
        assert!(kinds.iter().any(|kind| matches!(kind, RequestKind::Batch)));
    }
}
