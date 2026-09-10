use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use unionid::protocol::{PRODUCTION_VERSION, Request, Response};
use unionid::server::{CancelStatus, ConcurrencyStats, ConcurrentEngine, serve_until_concurrent};
use unionid::stream::{self, Frame as StreamFrame};
use unionid::{Engine, PageSpec};

type AnyResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const WARMUPS: usize = 3;
const MIN_ROWS: usize = 100;
const MAX_ROWS: usize = 100_000;
const MAX_REQUESTS: usize = 10_000;
const STREAM_ROWS: usize = 128;
const NORMAL_STREAM_PAYLOAD_BYTES: usize = 128 * 1024;
const SLOW_STREAM_PAYLOAD_BYTES: usize = 256 * 1024;

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

const STREAM_SCHEMA: &str = r#"type StreamItem =
  id int
  payload text
table stream_items StreamItem
  key id
type Counter =
  id int
  value int
table counters Counter
  key id
"#;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
    stream_cases: Vec<StreamCaseReport>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StreamCaseReport {
    adapter: Adapter,
    consumer: StreamConsumer,
    rows: usize,
    payload_bytes: usize,
    accepted_micros: u64,
    terminal_micros: u64,
    frames: usize,
    rows_received: usize,
    encoded_bytes: u64,
    largest_frame_bytes: usize,
    terminal: String,
    outcomes: BTreeMap<String, usize>,
    cancel_status: Option<CancelStatus>,
    cancel_micros: Option<u64>,
    writer_probe_micros: Option<u64>,
    writer_probe_sequence: Option<String>,
    emitting_before_writer_probe: bool,
    cancel_accepted_after_writer_started: bool,
    writer_probe_before_terminal_read: bool,
    operations_after: usize,
    schema_revision: u64,
    schema_hash: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StreamConsumer {
    Normal,
    Slow,
}

impl StreamConsumer {
    fn parse(value: &str) -> AnyResult<Self> {
        match value {
            "normal" => Ok(Self::Normal),
            "slow" => Ok(Self::Slow),
            _ => Err(format!("unknown stream consumer '{value}'").into()),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Slow => "slow",
        }
    }
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

#[derive(Default)]
struct WorkerReport {
    samples: Vec<Sample>,
    errors: BTreeMap<String, usize>,
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

struct StreamConnection {
    reader: BufReader<TcpStream>,
}

enum CancelClient {
    Tcp(TcpStream),
    Http {
        address: SocketAddr,
        socket: TcpStream,
    },
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
        [_, command, root, adapter, rows, consumer] if command == "stream-measure" => {
            let report = measure_stream(
                Path::new(root),
                Adapter::parse(adapter)?,
                rows.parse()?,
                StreamConsumer::parse(consumer)?,
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
    let mut stream_cases = Vec::new();
    for adapter in [Adapter::Tcp, Adapter::Http] {
        for consumer in [StreamConsumer::Normal, StreamConsumer::Slow] {
            let case_root = root.join(format!("{}-stream-{}", adapter.name(), consumer.name()));
            let output = Command::new(&executable)
                .args([
                    "stream-measure",
                    case_root.to_str().ok_or("work path is not UTF-8")?,
                    adapter.name(),
                    &rows.to_string(),
                    consumer.name(),
                ])
                .output()?;
            if !output.status.success() {
                return Err(format!(
                    "{} {} stream failed: {}",
                    adapter.name(),
                    consumer.name(),
                    String::from_utf8_lossy(&output.stderr)
                )
                .into());
            }
            stream_cases.push(serde_json::from_slice(&output.stdout)?);
        }
    }
    let report = EvaluationReport {
        evidence_only_not_sla: true,
        environment: environment()?,
        rows,
        requests_per_client: requests,
        cases,
        stream_cases,
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
        validate_response(
            RequestKind::Point,
            &warmup.send(&request)?,
            &schema,
            0,
            index,
            rows,
        )?;
    }
    drop(warmup);
    warmup_runtime.stop()?;

    // Queue observations are lifetime aggregates. Reopen the prepared database so
    // the measured runtime starts with clean counters after adapter-specific warmup.
    let runtime = AdapterRuntime::start(adapter, Engine::open_redb(&database)?)?;
    let baseline = runtime.engine.stats();
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut handles = Vec::new();
    let mut releases = Vec::new();
    for client_id in 0..clients {
        let endpoint = runtime.endpoint.clone();
        let schema = schema.clone();
        let ready_tx = ready_tx.clone();
        let (release_tx, release_rx) = mpsc::channel();
        releases.push(release_tx);
        handles.push(std::thread::spawn(move || -> WorkerReport {
            let mut report = WorkerReport {
                samples: Vec::with_capacity(requests),
                errors: BTreeMap::new(),
            };
            let mut client = match Client::connect(&endpoint) {
                Ok(client) => {
                    let _ = ready_tx.send((client_id, true));
                    client
                }
                Err(_) => {
                    add_errors(&mut report.errors, "connect", requests);
                    let _ = ready_tx.send((client_id, false));
                    return report;
                }
            };
            if release_rx.recv().is_err() {
                add_errors(&mut report.errors, "coordination", requests);
                return report;
            }
            for index in 0..requests {
                let kind = request_kind(index, read_percent);
                let request = match build_request(kind, client_id, index, rows) {
                    Ok(request) => request,
                    Err(_) => {
                        increment_error(&mut report.errors, "request_build");
                        continue;
                    }
                };
                let sample_started = Instant::now();
                let response = match client.send(&request) {
                    Ok(response) => response,
                    Err(_) => {
                        increment_error(&mut report.errors, "transport");
                        continue;
                    }
                };
                let micros = elapsed_micros(sample_started.elapsed());
                let sequence =
                    match validate_response(kind, &response, &schema, client_id, index, rows) {
                        Ok(sequence) => sequence,
                        Err(_) => {
                            increment_error(&mut report.errors, "response_validation");
                            continue;
                        }
                    };
                report.samples.push(Sample {
                    micros,
                    sequence,
                    kind,
                });
            }
            report
        }));
    }
    drop(ready_tx);
    let mut ready = vec![false; clients];
    for _ in 0..clients {
        if let Ok((client_id, is_ready)) = ready_rx.recv() {
            ready[client_id] = is_ready;
        }
    }
    let started = Instant::now();
    for (is_ready, release) in ready.into_iter().zip(releases) {
        if is_ready {
            let _ = release.send(());
        }
    }
    let mut samples = Vec::with_capacity(clients * requests);
    let mut errors = BTreeMap::new();
    for handle in handles {
        let report = handle.join().map_err(|_| "load client panicked")?;
        samples.extend(report.samples);
        merge_errors(&mut errors, report.errors);
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
        errors,
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
    client: usize,
    index: usize,
    rows: usize,
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
    if response.request_id != format!("load-{client}-{index}") {
        return Err("response request identity mismatch".into());
    }
    let sequence = match kind {
        RequestKind::Point => {
            if response.rows.len() != 1 || response.page.as_ref().map(|page| page.limit) != Some(1)
            {
                return Err("point-read response shape mismatch".into());
            }
            validate_task_columns(response)?;
            validate_task_row(&response.rows[0])?;
            validate_ids(response, &[(client * 97 + index) % rows])?;
            response.page.as_ref().unwrap().snapshot_sequence.parse()?
        }
        RequestKind::Page => {
            if response.rows.is_empty()
                || response.rows.len() > 5
                || response.page.as_ref().map(|page| page.limit) != Some(5)
            {
                return Err("page response shape mismatch".into());
            }
            validate_task_columns(response)?;
            let tenant = (client + index) % 16;
            let mut previous = None;
            for row in &response.rows {
                validate_task_row(row)?;
                if wire_int(row.get("tenant")) != Some(tenant) {
                    return Err("page tenant mismatch".into());
                }
                let id = wire_int(row.get("id")).ok_or("page id is not an int")?;
                if previous.is_some_and(|previous| id <= previous) {
                    return Err("page rows are not in stable ascending order".into());
                }
                previous = Some(id);
            }
            response.page.as_ref().unwrap().snapshot_sequence.parse()?
        }
        RequestKind::Update => {
            if response.affected_rows != Some(1) || response.rows.len() != 1 {
                return Err("conditional-update response shape mismatch".into());
            }
            validate_returning_id(response)?;
            validate_ids(response, &[(client * 97 + index) % rows])?;
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
            validate_returning_id(response)?;
            let base = rows + (client * MAX_REQUESTS + index) * 2;
            validate_ids(response, &[base, base + 1])?;
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

fn validate_task_columns(response: &Response) -> AnyResult<()> {
    let expected = [
        ("id", "int"),
        ("tenant", "int"),
        ("title", "text"),
        ("state", "State"),
        ("tags", "list text"),
        ("touches", "int"),
    ];
    if response.columns.len() != expected.len()
        || response
            .columns
            .iter()
            .zip(expected)
            .any(|(column, (name, ty))| column.name != name || column.ty != ty)
    {
        return Err("task result schema mismatch".into());
    }
    Ok(())
}

fn validate_task_row(row: &BTreeMap<String, unionid::protocol::WireValue>) -> AnyResult<()> {
    use unionid::protocol::WireValue;

    if row.len() != 6
        || wire_int(row.get("id")).is_none()
        || wire_int(row.get("tenant")).is_none()
        || wire_int(row.get("touches")).is_none()
        || !matches!(row.get("title"), Some(WireValue::Text { .. }))
        || !matches!(row.get("tags"), Some(WireValue::List { .. }))
        || !matches!(row.get("state"), Some(WireValue::Named { .. }))
    {
        return Err("task row keys or value types mismatch".into());
    }
    Ok(())
}

fn validate_returning_id(response: &Response) -> AnyResult<()> {
    if response.columns.len() != 1
        || response.columns[0].name != "id"
        || response.columns[0].ty != "int"
        || response.rows.iter().any(|row| row.len() != 1)
    {
        return Err("returning id schema mismatch".into());
    }
    Ok(())
}

fn validate_ids(response: &Response, expected: &[usize]) -> AnyResult<()> {
    let actual = response
        .rows
        .iter()
        .map(|row| wire_int(row.get("id")).ok_or("returned id is not an int"))
        .collect::<Result<Vec<_>, _>>()?;
    if actual != expected {
        return Err("returned ids mismatch".into());
    }
    Ok(())
}

fn wire_int(value: Option<&unionid::protocol::WireValue>) -> Option<usize> {
    match value {
        Some(unionid::protocol::WireValue::Int { value }) => value.parse().ok(),
        _ => None,
    }
}

fn measure_stream(
    root: &Path,
    adapter: Adapter,
    requested_rows: usize,
    consumer: StreamConsumer,
) -> AnyResult<StreamCaseReport> {
    if adapter == Adapter::Embedded {
        return Err("stream transport evaluation requires TCP or HTTP".into());
    }
    if !(MIN_ROWS..=MAX_ROWS).contains(&requested_rows) {
        return Err(format!("rows must be between {MIN_ROWS} and {MAX_ROWS}").into());
    }
    fs::create_dir_all(root)?;
    let rows = match consumer {
        StreamConsumer::Normal => requested_rows.min(STREAM_ROWS),
        StreamConsumer::Slow => STREAM_ROWS,
    };
    let payload_bytes = match consumer {
        StreamConsumer::Normal => NORMAL_STREAM_PAYLOAD_BYTES,
        StreamConsumer::Slow => SLOW_STREAM_PAYLOAD_BYTES,
    };
    let database = root.join("stream-load.redb");
    prepare_stream(&database, rows, payload_bytes)?;
    let mut engine = Engine::open_redb(&database)?;
    let integrity = engine.check_integrity()?;
    if !integrity.backend_clean || integrity.versions.format != 6 {
        return Err("prepared stream database did not pass format-6 integrity checking".into());
    }
    let schema = engine.schema_info();
    let runtime = AdapterRuntime::start(adapter, engine)?;
    let request_id = format!("{}-{}-stream", adapter.name(), consumer.name());
    let request = stream::Request::Query {
        stream_version: stream::VERSION,
        request: Request::query(request_id.clone(), "from stream_items\nsort id".to_owned())
            .with_version(PRODUCTION_VERSION)?,
    };
    let mut cancel_client = if consumer == StreamConsumer::Slow {
        Some(CancelClient::connect(&runtime.endpoint)?)
    } else {
        None
    };
    let started = Instant::now();
    let mut connection = StreamConnection::connect(&runtime.endpoint, &request)?;
    let (accepted, accepted_bytes) = connection.read_frame()?;
    let accepted_micros = elapsed_micros(started.elapsed());
    let operation_id = match accepted {
        StreamFrame::Accepted {
            stream_version,
            request_id: frame_request,
            operation_id,
        } if stream_version == stream::VERSION && frame_request == request_id => operation_id,
        _ => return Err("stream did not begin with a matching accepted frame".into()),
    };

    let mut cancel_status = None;
    let mut cancel_micros = None;
    let mut writer_probe_micros = None;
    let mut writer_probe_sequence = None;
    let mut emitting_before_writer_probe = false;
    let mut cancel_accepted_after_writer_started = false;
    let mut writer_probe_before_terminal_read = false;
    if consumer == StreamConsumer::Slow {
        wait_for_emitting(&runtime.engine)?;
        emitting_before_writer_probe = runtime.engine.stats().emitting_operations == 1;
        if !emitting_before_writer_probe {
            return Err("slow consumer did not retain bounded stream backpressure".into());
        }

        let mut control = cancel_client
            .take()
            .expect("slow consumer owns a cancellation channel");
        let cancel_request_id = request_id.clone();
        let cancel_operation_id = operation_id.clone();
        let writer_started = Arc::new(AtomicBool::new(false));
        let cancel_writer_started = Arc::clone(&writer_started);
        let canceler = std::thread::spawn(move || -> AnyResult<_> {
            // Start cancellation only after the writer invocation begins, but
            // do not wait for the writer or race the transport idle timeout.
            while !cancel_writer_started.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let cancel_started = Instant::now();
            let response = control.send(&cancel_request_id, &cancel_operation_id)?;
            Ok((response, elapsed_micros(cancel_started.elapsed())))
        });

        let write = Request::query(
            "stream-writer",
            "update counters\nfilter id == 0\nset value = value + 1\nreturning id",
        )
        .with_idempotency_key(format!("{}-stream-writer", adapter.name()))?
        .with_version(PRODUCTION_VERSION)?;
        writer_started.store(true, Ordering::Release);
        let writer_started = Instant::now();
        let response = runtime.engine.execute_protocol_request(write);
        writer_probe_micros = Some(elapsed_micros(writer_started.elapsed()));
        if !response.ok || response.affected_rows != Some(1) {
            return Err("writer probe did not commit before slow-stream terminal delivery".into());
        }
        writer_probe_sequence = Some(
            response
                .idempotency
                .as_ref()
                .ok_or("writer probe has no idempotency metadata")?
                .committed_sequence
                .clone(),
        );
        writer_probe_before_terminal_read = true;

        let (response, elapsed) = canceler
            .join()
            .map_err(|_| "stream cancellation client panicked")??;
        cancel_micros = Some(elapsed);
        cancel_status = Some(response.result.status);
        cancel_accepted_after_writer_started = response.result.status == CancelStatus::Accepted;
        if response.result.status != CancelStatus::Accepted {
            return Err("explicit stream cancellation was not accepted".into());
        }
    }

    let mut frame_count = 1;
    let mut encoded_bytes = u64::try_from(accepted_bytes)?;
    let mut largest_frame_bytes = accepted_bytes;
    let mut rows_received = 0;
    let mut saw_schema = false;
    let mut terminal = None;
    let mut outcomes = BTreeMap::from([
        ("cancelled".to_owned(), 0),
        ("completed".to_owned(), 0),
        ("rejected".to_owned(), 0),
        ("timeout".to_owned(), 0),
    ]);
    while terminal.is_none() {
        let (frame, bytes) = connection.read_frame()?;
        if bytes > stream::MAX_FRAME_BYTES {
            return Err("stream frame exceeded the protocol byte limit".into());
        }
        frame_count += 1;
        largest_frame_bytes = largest_frame_bytes.max(bytes);
        let encoded_before_frame = encoded_bytes;
        encoded_bytes = encoded_bytes.saturating_add(u64::try_from(bytes)?);
        match frame {
            StreamFrame::Schema {
                stream_version,
                request_id: frame_request,
                operation_id: frame_operation,
                columns,
                schema: frame_schema,
            } => {
                if saw_schema
                    || rows_received != 0
                    || stream_version != stream::VERSION
                    || frame_request != request_id
                    || frame_operation != operation_id
                    || frame_schema != schema
                    || columns.len() != 2
                    || columns[0].name != "id"
                    || columns[1].name != "payload"
                {
                    return Err("stream schema frame order or identity mismatch".into());
                }
                saw_schema = true;
            }
            StreamFrame::Row {
                stream_version,
                request_id: frame_request,
                operation_id: frame_operation,
                sequence,
                row,
            } => {
                if !saw_schema
                    || stream_version != stream::VERSION
                    || frame_request != request_id
                    || frame_operation != operation_id
                    || sequence.parse::<usize>()? != rows_received
                    || wire_int(row.get("id")) != Some(rows_received)
                {
                    return Err("stream row frame order, sequence, or identity mismatch".into());
                }
                rows_received += 1;
                if consumer == StreamConsumer::Slow {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            StreamFrame::Complete {
                stream_version,
                request_id: frame_request,
                operation_id: frame_operation,
                row_count,
                encoded_bytes: declared_bytes,
                ..
            } => {
                if !saw_schema
                    || stream_version != stream::VERSION
                    || frame_request != request_id
                    || frame_operation != operation_id
                    || consumer != StreamConsumer::Normal
                    || row_count.parse::<usize>()? != rows
                    || rows_received != rows
                    || declared_bytes.parse::<u64>()? != encoded_before_frame
                {
                    return Err("stream completion frame mismatch".into());
                }
                *outcomes.get_mut("completed").unwrap() += 1;
                terminal = Some("complete".to_owned());
            }
            StreamFrame::Error {
                stream_version,
                request_id: frame_request,
                operation_id: frame_operation,
                error,
                ..
            } => {
                if !saw_schema
                    || stream_version != stream::VERSION
                    || frame_request != request_id
                    || frame_operation != operation_id
                {
                    return Err("stream error frame order or identity mismatch".into());
                }
                let category = match error.code.as_str() {
                    "E_CANCELLED" => "cancelled",
                    "E_TIMEOUT" => "timeout",
                    _ => "rejected",
                };
                *outcomes.get_mut(category).unwrap() += 1;
                terminal = Some(error.code);
            }
            StreamFrame::Accepted { .. } => {
                return Err("stream emitted more than one accepted frame".into());
            }
        }
    }
    let terminal_micros = elapsed_micros(started.elapsed());
    if consumer == StreamConsumer::Slow && terminal.as_deref() != Some("E_CANCELLED") {
        return Err("slow stream did not finish with E_CANCELLED".into());
    }
    if encoded_bytes
        > u64::try_from(stream::MAX_EMITTED_BYTES.saturating_add(stream::MAX_FRAME_BYTES))?
    {
        return Err("stream exceeded the total encoded-byte budget".into());
    }
    wait_for_operations(&runtime.engine, 0)?;
    let operations_after = runtime.engine.stats().registered_operations;
    runtime.stop()?;

    let mut verifier = Engine::open_redb(&database)?;
    let integrity = verifier.check_integrity()?;
    if !integrity.backend_clean || verifier.schema_info() != schema {
        return Err("post-stream integrity or schema validation failed".into());
    }
    let response = require_ok(verifier.execute("from counters\nfilter id == 0\nselect value"))?;
    let expected_value = usize::from(consumer == StreamConsumer::Slow);
    if response.rows.len() != 1
        || !matches!(response.rows[0].get("value"), Some(unionid::Value::Int(value)) if *value == expected_value as i64)
    {
        return Err("writer probe effect mismatch after stream completion".into());
    }

    Ok(StreamCaseReport {
        adapter,
        consumer,
        rows,
        payload_bytes,
        accepted_micros,
        terminal_micros,
        frames: frame_count,
        rows_received,
        encoded_bytes,
        largest_frame_bytes,
        terminal: terminal.unwrap(),
        outcomes,
        cancel_status,
        cancel_micros,
        writer_probe_micros,
        writer_probe_sequence,
        emitting_before_writer_probe,
        cancel_accepted_after_writer_started,
        writer_probe_before_terminal_read,
        operations_after,
        schema_revision: schema.revision,
        schema_hash: schema.hash,
    })
}

fn prepare_stream(path: &Path, rows: usize, payload_bytes: usize) -> AnyResult<()> {
    let mut engine = Engine::open_redb(path.to_path_buf())?;
    require_ok(engine.execute(STREAM_SCHEMA))?;
    require_ok(engine.execute("insert counters {id = 0, value = 0}"))?;
    let payload = "x".repeat(payload_bytes);
    for start in 0..rows {
        let values = (start..rows.min(start + 1))
            .map(|id| format!("{{id = {id}, payload = \"{payload}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        require_ok(engine.execute(&format!("insert many stream_items [{values}]")))?;
    }
    Ok(())
}

fn wait_for_emitting(engine: &ConcurrentEngine) -> AnyResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.stats().emitting_operations != 1 {
        if Instant::now() >= deadline {
            return Err("stream did not enter emitting phase".into());
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    Ok(())
}

fn wait_for_operations(engine: &ConcurrentEngine, expected: usize) -> AnyResult<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.stats().registered_operations != expected {
        if Instant::now() >= deadline {
            return Err("stream operation did not reach its terminal state".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
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

impl StreamConnection {
    fn connect(endpoint: &Endpoint, request: &stream::Request) -> AnyResult<Self> {
        match endpoint {
            Endpoint::Tcp(address) => {
                let mut socket = TcpStream::connect(address)?;
                configure_stream_socket(&socket)?;
                serde_json::to_writer(&mut socket, request)?;
                socket.write_all(b"\n")?;
                socket.flush()?;
                Ok(Self {
                    reader: BufReader::new(socket),
                })
            }
            Endpoint::Http(address) => {
                let body = serde_json::to_vec(request)?;
                let mut socket = TcpStream::connect(address)?;
                configure_stream_socket(&socket)?;
                write!(
                    socket,
                    "POST /v1/stream/query HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )?;
                socket.write_all(&body)?;
                socket.flush()?;
                let mut reader = BufReader::new(socket);
                let mut status = String::new();
                reader.read_line(&mut status)?;
                if !status.starts_with("HTTP/1.1 200 ") {
                    return Err(format!("HTTP stream returned {status:?}").into());
                }
                let mut ndjson = false;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line)? == 0 {
                        return Err("HTTP stream ended before response headers".into());
                    }
                    if line == "\r\n" {
                        break;
                    }
                    if line.to_ascii_lowercase().starts_with("content-type:")
                        && line.to_ascii_lowercase().contains("application/x-ndjson")
                    {
                        ndjson = true;
                    }
                }
                if !ndjson {
                    return Err("HTTP stream response is not application/x-ndjson".into());
                }
                Ok(Self { reader })
            }
            Endpoint::Embedded(_) => Err("embedded endpoint has no NDJSON transport".into()),
        }
    }

    fn read_frame(&mut self) -> AnyResult<(StreamFrame, usize)> {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err("stream transport closed before a terminal frame".into());
        }
        Ok((serde_json::from_str(&line)?, bytes))
    }
}

fn configure_stream_socket(socket: &TcpStream) -> AnyResult<()> {
    socket.set_read_timeout(Some(Duration::from_secs(25)))?;
    socket.set_write_timeout(Some(Duration::from_secs(25)))?;
    Ok(())
}

impl CancelClient {
    fn connect(endpoint: &Endpoint) -> AnyResult<Self> {
        let client = match endpoint {
            Endpoint::Tcp(address) => {
                let socket = TcpStream::connect(address)?;
                socket.set_read_timeout(Some(Duration::from_secs(25)))?;
                socket.set_write_timeout(Some(Duration::from_secs(25)))?;
                Self::Tcp(socket)
            }
            Endpoint::Http(address) => {
                let socket = TcpStream::connect(address)?;
                socket.set_read_timeout(Some(Duration::from_secs(25)))?;
                socket.set_write_timeout(Some(Duration::from_secs(25)))?;
                Self::Http {
                    address: *address,
                    socket,
                }
            }
            Endpoint::Embedded(_) => {
                return Err("embedded endpoint has no NDJSON transport".into());
            }
        };
        // The TCP server polls a nonblocking listener every 100 ms. Give both
        // real adapters time to accept this control connection before starting
        // the stream so cancellation does not measure accept-loop scheduling.
        std::thread::sleep(Duration::from_millis(150));
        Ok(client)
    }

    fn send(
        &mut self,
        stream_request_id: &str,
        operation_id: &str,
    ) -> AnyResult<stream::CancelResponse> {
        let cancel_request_id = format!("cancel-{stream_request_id}");
        let request = stream::Request::Cancel {
            stream_version: stream::VERSION,
            request_id: cancel_request_id.clone(),
            operation_id: operation_id.to_owned(),
        };
        let response: stream::CancelResponse = match self {
            Self::Tcp(socket) => {
                serde_json::to_writer(&mut *socket, &request)?;
                socket.write_all(b"\n")?;
                socket.flush()?;
                let mut line = String::new();
                BufReader::new(socket.try_clone()?).read_line(&mut line)?;
                serde_json::from_str(&line)?
            }
            Self::Http { address, socket } => {
                let body = serde_json::to_vec(&request)?;
                write!(
                    socket,
                    "POST /v1/stream/cancel HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )?;
                socket.write_all(&body)?;
                socket.flush()?;
                let mut response = Vec::new();
                socket.read_to_end(&mut response)?;
                let body_start = response
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .ok_or("HTTP cancel response has no body")?
                    + 4;
                serde_json::from_slice(&response[body_start..])?
            }
        };
        if response.stream_version != stream::VERSION
            || response.request_id != cancel_request_id
            || response.operation_id != operation_id
            || !response.ok
        {
            return Err("stream cancel response identity or version mismatch".into());
        }
        Ok(response)
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
    let decoded: serde_json::Value = serde_json::from_slice(&body)?;
    if decoded.get("stream_version").is_some() {
        return handle_http_stream(&mut stream, engine, serde_json::from_value(decoded)?);
    }
    let request: Request = serde_json::from_value(decoded)?;
    let response = engine.execute_protocol_request(request);
    write_http_json(&mut stream, &response)
}

fn handle_http_stream(
    socket: &mut TcpStream,
    engine: ConcurrentEngine,
    request: stream::Request,
) -> AnyResult<()> {
    match request {
        stream::Request::Cancel {
            stream_version,
            request_id,
            operation_id,
        } => {
            let response = if stream_version == stream::VERSION {
                match stream::cancel(&engine, request_id.clone(), operation_id) {
                    Ok(response) => serde_json::to_value(response)?,
                    Err(error) => serde_json::to_value(stream::error_response(request_id, error))?,
                }
            } else {
                serde_json::to_value(stream::error_response(
                    request_id,
                    unionid::Error::new("E_STREAM_VERSION", "supported stream version is 1"),
                ))?
            };
            write_http_json(socket, &response)
        }
        stream::Request::Query {
            stream_version,
            request,
        } => {
            let request_id = request.request_id.clone();
            if stream_version != stream::VERSION {
                return write_http_json(
                    socket,
                    &stream::error_response(
                        request_id,
                        unionid::Error::new("E_STREAM_VERSION", "supported stream version is 1"),
                    ),
                );
            }
            let accepted = match stream::accept(
                &engine,
                request,
                Instant::now() + Duration::from_secs(25),
                None,
            ) {
                Ok(accepted) => accepted,
                Err(error) => {
                    return write_http_json(socket, &stream::error_response(request_id, error));
                }
            };
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n"
            )?;
            socket.write_all(accepted.accepted_bytes())?;
            socket.flush()?;
            let receiver = accepted.start();
            while let Ok(chunk) = receiver.recv() {
                socket.write_all(chunk.as_bytes())?;
                socket.flush()?;
            }
            Ok(())
        }
    }
}

fn write_http_json(socket: &mut TcpStream, response: &impl Serialize) -> AnyResult<()> {
    let encoded = serde_json::to_vec(response)?;
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        encoded.len()
    )?;
    socket.write_all(&encoded)?;
    socket.flush()?;
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
    post_http_json(address, "/v2/query", request)
}

fn post_http_json<T: Serialize, R: DeserializeOwned>(
    address: SocketAddr,
    path: &str,
    request: &T,
) -> AnyResult<R> {
    let body = serde_json::to_vec(request)?;
    let mut stream = TcpStream::connect(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(25)))?;
    stream.set_write_timeout(Some(Duration::from_secs(25)))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
    errors: BTreeMap<String, usize>,
    elapsed: Duration,
    queue: QueueReport,
    database_bytes: u64,
    schema: unionid::SchemaInfo,
) -> AnyResult<CaseReport> {
    if samples.len() + errors.values().sum::<usize>() != clients * requests {
        return Err("accounted request count mismatch".into());
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
    let sequence_min = if latencies.is_empty() {
        0
    } else {
        sequence_min
    };
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
        errors,
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
    if sorted.is_empty() {
        return 0;
    }
    let rank = sorted.len().saturating_mul(percent).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn increment_error(errors: &mut BTreeMap<String, usize>, category: &str) {
    add_errors(errors, category, 1);
}

fn add_errors(errors: &mut BTreeMap<String, usize>, category: &str, count: usize) {
    *errors.entry(category.to_owned()).or_default() += count;
}

fn merge_errors(target: &mut BTreeMap<String, usize>, source: BTreeMap<String, usize>) {
    for (category, count) in source {
        *target.entry(category).or_default() += count;
    }
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
