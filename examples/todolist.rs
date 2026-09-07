//! A production-shaped HTTP todo service using unionid's versioned data protocol.
//!
//! Run with `cargo run --example todolist`. The client side never opens the
//! database or calls `Engine`: migrations, typed DML/query, restart, integrity
//! checks, backup, and restore all cross HTTP.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};
use unionid::backup;
use unionid::migration::MigrationFile;
use unionid::protocol::{Request, Response, VERSION};
use unionid::server::{execute_protocol_request, execute_protocol_request_until};
use unionid::{Engine, Error, PageSpec, SchemaInfo};

const HTTP_EXECUTION_TIMEOUT: Duration = Duration::from_secs(5);

const INITIAL: &str = r#"migration m0001_todolist
  add type Retry =
    attempts int
    delay_ms int
  add type Reminder = Off | On {retry Retry, channel text}
  add type Label = System {name text} | User {name text, color text}
  add type Due = Never | At {unix_ms int} | Window {start int, end int}
  add type Status = Inbox | InProgress {attempt int, device text} | Blocked {reason text} | Done {at int}
  add type Checklist = Empty | Items {entries list text}
  add type Task =
    id int
    title text
    status Status
    reminder Reminder
    labels list Label
    due Due
    checklist Checklist
    estimate option int
    location (float, float)
  add table todos Task key id
  add index todos.status
"#;

const UPGRADE: &str = r#"migration m0002_todolist_upgrade
  parent m0001_todolist
  rename variant Status.InProgress to Claimed
  add field Retry.backoff_ms int = 250
  add field Task.priority int = 1
  add index todos.priority
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RetryV1 {
    attempts: i64,
    delay_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum ReminderV1 {
    Off,
    On { retry: RetryV1, channel: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum StatusV1 {
    Inbox,
    InProgress { attempt: i64, device: String },
    Blocked { reason: String },
    Done { at: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum Label {
    System { name: String },
    User { name: String, color: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum Due {
    Never,
    At { unix_ms: i64 },
    Window { start: i64, end: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum Checklist {
    Empty,
    Items { entries: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct TaskV1 {
    id: i64,
    title: String,
    status: StatusV1,
    reminder: ReminderV1,
    labels: Vec<Label>,
    due: Due,
    checklist: Checklist,
    estimate: Option<i64>,
    location: (f64, f64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RetryV2 {
    attempts: i64,
    delay_ms: i64,
    backoff_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum ReminderV2 {
    Off,
    On { retry: RetryV2, channel: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
enum StatusV2 {
    Inbox,
    Claimed { attempt: i64, device: String },
    Blocked { reason: String },
    Done { at: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct TaskV2 {
    id: i64,
    title: String,
    status: StatusV2,
    reminder: ReminderV2,
    labels: Vec<Label>,
    due: Due,
    checklist: Checklist,
    estimate: Option<i64>,
    location: (f64, f64),
    priority: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdminRequest {
    version: u32,
    request_id: String,
    #[serde(default)]
    migrations: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AdminResponse {
    version: u32,
    request_id: String,
    ok: bool,
    result: Option<JsonValue>,
    error: Option<Error>,
}

struct Service {
    database: PathBuf,
    restored: PathBuf,
    archive: PathBuf,
    engine: Option<Engine>,
    shutdown: Option<oneshot::Sender<()>>,
}

type Shared = Arc<Mutex<Service>>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("unionid-todolist-http-{}", std::process::id()))
        });
    std::fs::create_dir_all(&root)?;
    let database = root.join("todos.redb");
    let restored = root.join("todos-restored.redb");
    let archive = root.join("todos.backup.json");
    for path in [&database, &restored, &archive] {
        if path.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to overwrite {}; choose an empty work directory",
                    path.display()
                ),
            )
            .into());
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let state = Arc::new(Mutex::new(Service {
        engine: Some(Engine::open_redb(&database)?),
        database,
        restored,
        archive: archive.clone(),
        shutdown: Some(shutdown_tx),
    }));
    let app = Router::new()
        .route("/v1/query", post(query))
        .route("/v1/restored/query", post(restored_query))
        .route("/v1/admin/migrations/plan", post(migration_plan))
        .route("/v1/admin/migrations/apply", post(migration_apply))
        .route("/v1/admin/migrations/status", post(migration_status))
        .route("/v1/admin/restart", post(restart))
        .route("/v1/admin/check", post(check))
        .route("/v1/admin/backup", post(create_backup))
        .route("/v1/admin/restore", post(restore_backup))
        .route("/v1/admin/shutdown", post(shutdown))
        .with_state(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    let initial: AdminResponse = post_json(
        address,
        "/v1/admin/migrations/apply",
        &admin("migration-initial", vec![INITIAL]),
    )
    .await?;
    ensure_admin_ok(&initial)?;
    let schema_v1: SchemaInfo = serde_json::from_value(initial.result.unwrap())?;

    let tasks = vec![
        TaskV1 {
            id: 9_007_199_254_740_993,
            title: "write release notes".into(),
            status: StatusV1::Inbox,
            reminder: ReminderV1::On {
                retry: RetryV1 {
                    attempts: 2,
                    delay_ms: 1_000,
                },
                channel: "email".into(),
            },
            labels: vec![
                Label::System {
                    name: "release".into(),
                },
                Label::User {
                    name: "docs".into(),
                    color: "blue".into(),
                },
            ],
            due: Due::Window {
                start: 1_000,
                end: 5_000,
            },
            checklist: Checklist::Items {
                entries: vec!["draft".into(), "review".into()],
            },
            estimate: Some(90),
            location: (31.2304, 121.4737),
        },
        TaskV1 {
            id: 2,
            title: "publish crate".into(),
            status: StatusV1::InProgress {
                attempt: 1,
                device: "laptop".into(),
            },
            reminder: ReminderV1::Off,
            labels: vec![Label::System {
                name: "release".into(),
            }],
            due: Due::Never,
            checklist: Checklist::Empty,
            estimate: None,
            location: (0.0, 0.0),
        },
        TaskV1 {
            id: 1,
            title: "triage pagination".into(),
            status: StatusV1::Blocked {
                reason: "waiting for review".into(),
            },
            reminder: ReminderV1::Off,
            labels: vec![],
            due: Due::At { unix_ms: 900 },
            checklist: Checklist::Empty,
            estimate: Some(20),
            location: (1.0, 1.0),
        },
        TaskV1 {
            id: 3,
            title: "verify adapters".into(),
            status: StatusV1::Done { at: 800 },
            reminder: ReminderV1::Off,
            labels: vec![Label::System {
                name: "pagination".into(),
            }],
            due: Due::Never,
            checklist: Checklist::Items {
                entries: vec!["rust".into(), "tcp".into(), "http".into()],
            },
            estimate: Some(30),
            location: (2.0, 2.0),
        },
    ];
    let insert = Request::query("insert-todos", "insert many todos $rows\nreturning")
        .with_serde_param("rows", &tasks)?
        .with_idempotency_key("seed-todos-v1")?;
    let lost = post_json_then_lose_response(address, "/v1/query", &insert).await;
    assert!(lost.is_err(), "the example must inject a lost response");
    let retry = Request {
        request_id: "insert-todos-retry".into(),
        ..insert.clone()
    };
    let inserted: Response = post_json(address, "/v1/query", &retry).await?;
    assert!(inserted.idempotency.as_ref().unwrap().replayed);
    assert_eq!(inserted.typed_rows::<TaskV1>()?, tasks);

    let claim = Request::query(
        "claim-todo",
        r#"update todos
filter match status
  Inbox => true
  _ => false
set status = InProgress {attempt = 1, device = "worker-1"}
set reminder = On {retry = {attempts = 3, delay_ms = 2000}, channel = "slack"}
returning"#,
    );
    let claimed: Response = post_json(address, "/v1/query", &claim).await?;
    assert_eq!(claimed.affected_rows, Some(1));

    let wrong_type = Request::query("typed-error", "from todos | filter id == $id")
        .with_serde_param("id", &"not-an-int")?;
    let rejected: Response = post_json(address, "/v1/query", &wrong_type).await?;
    assert_eq!(
        rejected.error.as_ref().map(|error| error.code.as_str()),
        Some("E_TYPE")
    );

    let page_query = "from todos\nsort id";
    let first_page: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-page-1", page_query).with_page(PageSpec::forward(2)),
    )
    .await?;
    let first_page = first_page.typed_page::<TaskV1>()?;
    assert_eq!(
        first_page
            .rows
            .iter()
            .map(|task| task.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    let second_page = first_page.page.next_page().unwrap();

    post_json_then_disconnect(
        address,
        "/v1/query",
        &Request::query("http-disconnect", page_query).with_page(PageSpec::forward(1)),
    )
    .await?;
    let after_disconnect: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-after-disconnect", page_query).with_page(PageSpec::forward(1)),
    )
    .await?;
    assert!(after_disconnect.ok, "{}", after_disconnect.message);

    ensure_admin_ok(
        &post_json::<_, AdminResponse>(address, "/v1/admin/restart", &admin("restart", vec![]))
            .await?,
    )?;
    let reopened_page: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-page-2-after-restart", page_query).with_page(second_page.clone()),
    )
    .await?;
    let reopened_page = reopened_page.typed_page::<TaskV1>()?;
    assert_eq!(
        reopened_page
            .rows
            .iter()
            .map(|task| task.id)
            .collect::<Vec<_>>(),
        [3, 9_007_199_254_740_993]
    );
    assert!(reopened_page.page.next_page().is_none());

    let mut invalid_page = second_page.clone();
    let invalid_cursor = invalid_page.cursor.as_mut().unwrap();
    let replacement = if invalid_cursor.ends_with('A') {
        'B'
    } else {
        'A'
    };
    invalid_cursor.pop();
    invalid_cursor.push(replacement);
    let invalid: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-invalid-cursor", page_query).with_page(invalid_page),
    )
    .await?;
    assert_eq!(
        invalid.error.as_ref().map(|error| error.code.as_str()),
        Some("E_CURSOR_INTEGRITY")
    );

    let migrations = vec![INITIAL, UPGRADE];
    let planned: AdminResponse = post_json(
        address,
        "/v1/admin/migrations/plan",
        &admin("migration-plan", migrations.clone()),
    )
    .await?;
    ensure_admin_ok(&planned)?;
    let upgraded: AdminResponse = post_json(
        address,
        "/v1/admin/migrations/apply",
        &admin("migration-upgrade", migrations.clone()),
    )
    .await?;
    ensure_admin_ok(&upgraded)?;

    let stale_page: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-page-after-migration", page_query).with_page(second_page),
    )
    .await?;
    assert_eq!(
        stale_page.error.as_ref().map(|error| error.code.as_str()),
        Some("E_CURSOR_SCHEMA")
    );
    let status: AdminResponse = post_json(
        address,
        "/v1/admin/migrations/status",
        &admin("migration-status", migrations),
    )
    .await?;
    ensure_admin_ok(&status)?;

    let stale = Request {
        schema: Some(schema_v1),
        ..Request::query("stale-schema", "from todos")
    };
    let rejected: Response = post_json(address, "/v1/query", &stale).await?;
    assert_eq!(
        rejected.error.as_ref().map(|error| error.code.as_str()),
        Some("E_SCHEMA_CHANGED")
    );

    let current: Response = post_json(
        address,
        "/v1/query",
        &Request::query("current", "from todos | sort id"),
    )
    .await?;
    let current_tasks = current.typed_rows::<TaskV2>()?;
    assert_eq!(current_tasks.len(), 4);
    let upgraded_first: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-upgraded-page-1", page_query).with_page(PageSpec::forward(2)),
    )
    .await?;
    let upgraded_first = upgraded_first.typed_page::<TaskV2>()?;
    let upgraded_second: Response = post_json(
        address,
        "/v1/query",
        &Request::query("http-upgraded-page-2", page_query)
            .with_page(upgraded_first.page.next_page().unwrap()),
    )
    .await?;
    let upgraded_second = upgraded_second.typed_page::<TaskV2>()?;
    let paged_tasks = upgraded_first
        .rows
        .into_iter()
        .chain(upgraded_second.rows)
        .collect::<Vec<_>>();
    assert_eq!(paged_tasks, current_tasks);
    let explain: Response = post_json(
        address,
        "/v1/query",
        &Request::query("explain", "explain from todos | filter priority == 1"),
    )
    .await?;
    assert_eq!(
        explain.plan.and_then(|plan| plan.access.index),
        Some("todos.priority".into())
    );
    ensure_admin_ok(
        &post_json::<_, AdminResponse>(address, "/v1/admin/check", &admin("check", vec![])).await?,
    )?;

    let backup_response: AdminResponse =
        post_json(address, "/v1/admin/backup", &admin("backup", vec![])).await?;
    ensure_admin_ok(&backup_response)?;
    let restore_response: AdminResponse =
        post_json(address, "/v1/admin/restore", &admin("restore", vec![])).await?;
    ensure_admin_ok(&restore_response)?;
    assert_eq!(backup_response.result, restore_response.result);
    let restored_rows: Response = post_json(
        address,
        "/v1/restored/query",
        &Request::query("restored", "from todos | sort id"),
    )
    .await?;
    assert_eq!(restored_rows.typed_rows::<TaskV2>()?, current_tasks);
    let restored_replay: Response = post_json(
        address,
        "/v1/restored/query",
        &Request {
            request_id: "restored-idempotency-replay".into(),
            ..insert
        },
    )
    .await?;
    assert!(restored_replay.idempotency.as_ref().unwrap().replayed);
    assert_eq!(restored_replay.typed_rows::<TaskV1>()?, tasks);

    ensure_admin_ok(
        &post_json::<_, AdminResponse>(address, "/v1/admin/shutdown", &admin("shutdown", vec![]))
            .await?,
    )?;
    server.await??;
    println!(
        "HTTP todo flow passed: typed ADTs/pages, disconnect, lost-response replay, restart, migration, check, backup/restore ({})",
        archive.display()
    );
    Ok(())
}

fn admin(request_id: &str, migrations: Vec<&str>) -> AdminRequest {
    AdminRequest {
        version: VERSION,
        request_id: request_id.into(),
        migrations: migrations.into_iter().map(str::to_owned).collect(),
    }
}

fn ensure_admin_ok(response: &AdminResponse) -> Result<(), Error> {
    if response.ok {
        Ok(())
    } else {
        Err(response
            .error
            .clone()
            .unwrap_or_else(|| Error::new("E_PROTOCOL", "admin request failed")))
    }
}

async fn query(State(state): State<Shared>, Json(request): Json<Request>) -> Json<Response> {
    let mut service = state.lock().await;
    let response = match service.engine.as_mut() {
        Some(engine) => {
            execute_protocol_request_until(engine, request, Instant::now() + HTTP_EXECUTION_TIMEOUT)
        }
        None => Response::failure(
            request.request_id,
            engine_unavailable(),
            unavailable_schema(),
        ),
    };
    Json(response)
}

async fn restored_query(
    State(state): State<Shared>,
    Json(request): Json<Request>,
) -> Json<Response> {
    let service = state.lock().await;
    let response = match Engine::open_redb(&service.restored) {
        Ok(mut engine) => execute_protocol_request(&mut engine, request),
        Err(error) => Response::failure(
            request.request_id,
            error,
            SchemaInfo {
                revision: 0,
                hash: String::new(),
            },
        ),
    };
    Json(response)
}

async fn migration_plan(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    admin_with_engine(state, request, |engine, files| {
        engine.plan_migrations(files)
    })
    .await
}

async fn migration_apply(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    admin_with_engine(state, request, |engine, files| {
        engine.apply_migrations(files).map(|result| result.schema)
    })
    .await
}

async fn migration_status(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    admin_with_engine(state, request, |engine, files| {
        engine.migration_status(files)
    })
    .await
}

async fn admin_with_engine<T: Serialize>(
    state: Shared,
    request: AdminRequest,
    operation: impl FnOnce(&mut Engine, &[MigrationFile]) -> Result<T, Error>,
) -> Json<AdminResponse> {
    if let Some(response) = validate_admin(&request) {
        return Json(response);
    }
    let files = match parse_migrations(&request.migrations) {
        Ok(files) => files,
        Err(error) => return Json(admin_error(request.request_id, error)),
    };
    let mut service = state.lock().await;
    Json(match service.engine.as_mut() {
        Some(engine) => match operation(engine, &files) {
            Ok(result) => admin_ok(request.request_id, result),
            Err(error) => admin_error(request.request_id, error),
        },
        None => admin_error(request.request_id, engine_unavailable()),
    })
}

async fn restart(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    if let Some(response) = validate_admin(&request) {
        return Json(response);
    }
    let mut service = state.lock().await;
    service.engine.take();
    match Engine::open_redb(&service.database) {
        Ok(engine) => {
            service.engine = Some(engine);
            Json(admin_ok(request.request_id, json!({"reopened": true})))
        }
        Err(error) => Json(admin_error(request.request_id, error)),
    }
}

async fn check(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    if let Some(response) = validate_admin(&request) {
        return Json(response);
    }
    let mut service = state.lock().await;
    Json(match service.engine.as_mut() {
        Some(engine) => match engine.check_integrity() {
            Ok(result) => admin_ok(request.request_id, result),
            Err(error) => admin_error(request.request_id, error),
        },
        None => admin_error(request.request_id, engine_unavailable()),
    })
}

async fn create_backup(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    if let Some(response) = validate_admin(&request) {
        return Json(response);
    }
    let mut service = state.lock().await;
    service.engine.take();
    let result = backup::create(&service.database, &service.archive);
    let reopen = Engine::open_redb(&service.database);
    Json(match (result, reopen) {
        (_, Err(error)) => admin_error(request.request_id, error),
        (Ok(result), Ok(engine)) => {
            service.engine = Some(engine);
            admin_ok(request.request_id, result)
        }
        (Err(error), Ok(engine)) => {
            service.engine = Some(engine);
            admin_error(request.request_id, error)
        }
    })
}

async fn restore_backup(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    if let Some(response) = validate_admin(&request) {
        return Json(response);
    }
    let service = state.lock().await;
    Json(match backup::restore(&service.archive, &service.restored) {
        Ok(result) => admin_ok(request.request_id, result),
        Err(error) => admin_error(request.request_id, error),
    })
}

async fn shutdown(
    State(state): State<Shared>,
    Json(request): Json<AdminRequest>,
) -> Json<AdminResponse> {
    if let Some(response) = validate_admin(&request) {
        return Json(response);
    }
    let mut service = state.lock().await;
    if let Some(shutdown) = service.shutdown.take() {
        let _ = shutdown.send(());
    }
    Json(admin_ok(request.request_id, json!({"stopped": true})))
}

fn validate_admin(request: &AdminRequest) -> Option<AdminResponse> {
    (request.version != VERSION).then(|| {
        admin_error(
            request.request_id.clone(),
            Error::new(
                "E_PROTOCOL_VERSION",
                format!("unsupported protocol version {}", request.version),
            ),
        )
    })
}

fn parse_migrations(sources: &[String]) -> Result<Vec<MigrationFile>, Error> {
    sources.iter().cloned().map(MigrationFile::parse).collect()
}

fn admin_ok(request_id: String, result: impl Serialize) -> AdminResponse {
    match serde_json::to_value(result) {
        Ok(result) => AdminResponse {
            version: VERSION,
            request_id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => admin_error(
            request_id,
            Error::new("E_PROTOCOL", format!("encode response: {error}")),
        ),
    }
}

fn admin_error(request_id: String, error: Error) -> AdminResponse {
    AdminResponse {
        version: VERSION,
        request_id,
        ok: false,
        result: None,
        error: Some(error),
    }
}

fn engine_unavailable() -> Error {
    Error::new(
        "E_STORAGE",
        "source database is unavailable; retry the restart operation",
    )
}

fn unavailable_schema() -> SchemaInfo {
    SchemaInfo {
        revision: 0,
        hash: String::new(),
    }
}

async fn post_json<T: Serialize, R: DeserializeOwned>(
    address: std::net::SocketAddr,
    path: &str,
    value: &T,
) -> Result<R, Box<dyn std::error::Error>> {
    let body = serde_json::to_vec(value)?;
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(&body).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let body_start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("HTTP response has no body")?
        + 4;
    Ok(serde_json::from_slice(&response[body_start..])?)
}

async fn post_json_then_lose_response<T: Serialize>(
    address: std::net::SocketAddr,
    path: &str,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    let _: JsonValue = post_json(address, path, value).await?;
    Err(std::io::Error::new(
        std::io::ErrorKind::ConnectionReset,
        "injected response loss after the server committed the request",
    )
    .into())
}

async fn post_json_then_disconnect<T: Serialize>(
    address: std::net::SocketAddr,
    path: &str,
    value: &T,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = serde_json::to_vec(value)?;
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await?;
    Ok(())
}
