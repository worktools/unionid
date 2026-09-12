use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use unionid::asynchronous::http::{self, Config};
use unionid::protocol::{PRODUCTION_VERSION, Request, Response};
use unionid::scalars::{Timestamp, Uuid};
use unionid::server::{execute_protocol_request, serve_until_concurrent};
use unionid::{
    AsyncTcpClient, CancelStatus, ConcurrentEngine, Engine, HttpClient, OperationOutcome, PageSpec,
    TcpClient, TypedStreamEvent,
};

const SCHEMA: &str = r#"type State =
  Draft
  | Published {at timestamp}

type Entry =
  id uuid
  title text
  state State
  created_at timestamp

table entries Entry
  key id

create index entries (created_at, id)"#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum State {
    Draft,
    Published { at: Timestamp },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    id: Uuid,
    title: String,
    state: State,
    created_at: Timestamp,
}

pub async fn verify() -> Result<(), Box<dyn std::error::Error>> {
    let expected = sample_rows()?;
    let direct_error = verify_direct(&expected)?;
    let tcp_error = verify_tcp(&expected).await?;
    let http_error = verify_http(&expected).await?;
    assert_eq!(tcp_error, direct_error);
    assert_eq!(http_error, direct_error);
    verify_lost_response_retry(&expected[0]).await?;
    verify_transport_deadline().await?;
    Ok(())
}

fn verify_direct(expected: &[Entry]) -> Result<String, Box<dyn std::error::Error>> {
    let mut engine = Engine::memory();
    checked(execute_protocol_request(
        &mut engine,
        request("direct-setup", SCHEMA)?,
    ))?;
    let inserted = checked(execute_protocol_request(
        &mut engine,
        insert_request("direct-insert", expected)?,
    ))?;
    assert_eq!(inserted.typed_rows::<Entry>()?, expected);
    let rows = checked(execute_protocol_request(
        &mut engine,
        request("direct-read", "from entries\nsort {created_at, id}")?,
    ))?;
    assert_eq!(rows.typed_rows::<Entry>()?, expected);
    Ok(failure_code(execute_protocol_request(
        &mut engine,
        request("direct-error", "from absent")?,
    )))
}

async fn verify_tcp(expected: &[Entry]) -> Result<String, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = Arc::clone(&shutdown);
    let server = std::thread::spawn(move || {
        serve_until_concurrent(
            listener,
            ConcurrentEngine::new(Engine::memory()),
            server_shutdown,
        )
        .unwrap()
    });

    let mut sync = TcpClient::connect(&address)?;
    checked(sync.request(&request("tcp-setup", SCHEMA)?)?)?;
    let inserted = checked(sync.request(&insert_request("tcp-insert", expected)?)?)?;
    assert_eq!(inserted.typed_rows::<Entry>()?, expected);
    let sync_rows = checked(sync.request(&request(
        "tcp-sync-read",
        "from entries\nsort {created_at, id}",
    )?)?)?
    .typed_rows::<Entry>()?;
    assert_eq!(sync_rows, expected);

    let asynchronous = AsyncTcpClient::connect(address.clone()).await?;
    let async_rows = checked(
        asynchronous
            .request(&request(
                "tcp-async-read",
                "from entries\nsort {created_at, id}",
            )?)
            .await?,
    )?
    .typed_rows::<Entry>()?;
    assert_eq!(async_rows, sync_rows);
    verify_async_pages(&asynchronous, expected).await?;
    verify_async_stream(&asynchronous, expected).await?;
    let error = failure_code(
        asynchronous
            .request(&request("tcp-error", "from absent")?)
            .await?,
    );

    drop(sync);
    drop(asynchronous);
    shutdown.store(true, Ordering::Release);
    let stats = server.join().unwrap();
    assert!(stats.accepted_connections >= 1);
    Ok(error)
}

async fn verify_http(expected: &[Entry]) -> Result<String, Box<dyn std::error::Error>> {
    let app = http::router(ConcurrentEngine::new(Engine::memory()), Config::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = HttpClient::connect(format!("http://{address}"))?;

    checked(client.request(&request("http-setup", SCHEMA)?).await?)?;
    let inserted = checked(
        client
            .request(&insert_request("http-insert", expected)?)
            .await?,
    )?;
    assert_eq!(inserted.typed_rows::<Entry>()?, expected);
    let rows = checked(
        client
            .request(&request(
                "http-read",
                "from entries\nsort {created_at, id}",
            )?)
            .await?,
    )?
    .typed_rows::<Entry>()?;
    assert_eq!(rows, expected);

    let page_request = request("http-page-1", "from entries\nsort {created_at, id}")?
        .with_page(PageSpec::forward(1));
    let first = client.page::<Entry>(&page_request).await?;
    let second = client
        .next_page::<Entry>(&page_request, &first.page, "http-page-2")
        .await?
        .ok_or("HTTP page unexpectedly ended")?;
    assert_eq!(first.rows, expected[..1]);
    assert_eq!(second.rows, expected[1..]);

    let stream_request = request("http-stream", "from entries\nsort {created_at, id}")?;
    let mut stream = client.stream(&stream_request).await?;
    let operation_id = stream.operation_id().to_owned();
    assert_eq!(collect_http_stream(&mut stream).await?, expected);
    let terminal = client.cancel("http-cancel", operation_id).await?;
    assert_eq!(terminal.result.status, CancelStatus::AlreadyTerminal);
    assert_eq!(terminal.result.outcome, Some(OperationOutcome::Completed));

    let error = failure_code(
        client
            .request(&request("http-error", "from absent")?)
            .await?,
    );
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    Ok(error)
}

async fn verify_async_pages(
    client: &AsyncTcpClient,
    expected: &[Entry],
) -> Result<(), Box<dyn std::error::Error>> {
    let request = request("tcp-page-1", "from entries\nsort {created_at, id}")?
        .with_page(PageSpec::forward(1));
    let first = client.page::<Entry>(&request).await?;
    let second = client
        .next_page::<Entry>(&request, &first.page, "tcp-page-2")
        .await?
        .ok_or("TCP page unexpectedly ended")?;
    assert_eq!(first.rows, expected[..1]);
    assert_eq!(second.rows, expected[1..]);
    Ok(())
}

async fn verify_async_stream(
    client: &AsyncTcpClient,
    expected: &[Entry],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut stream = client
        .stream(&request(
            "tcp-stream",
            "from entries\nsort {created_at, id}",
        )?)
        .await?;
    let operation_id = stream.operation_id().to_owned();
    assert_eq!(collect_tcp_stream(&mut stream).await?, expected);
    let terminal = client.cancel("tcp-cancel", operation_id).await?;
    assert_eq!(terminal.result.status, CancelStatus::AlreadyTerminal);
    assert_eq!(terminal.result.outcome, Some(OperationOutcome::Completed));
    Ok(())
}

async fn collect_tcp_stream(
    stream: &mut unionid::AsyncTcpStream,
) -> Result<Vec<Entry>, unionid::Error> {
    let mut rows = Vec::new();
    while let Some(event) = stream.next_event::<Entry>().await? {
        match event {
            TypedStreamEvent::Schema { .. } => {}
            TypedStreamEvent::Row { row, .. } => rows.push(row),
            TypedStreamEvent::Complete { .. } => return Ok(rows),
            TypedStreamEvent::Error { error, .. } => return Err(error),
        }
    }
    Err(unionid::Error::new(
        "E_CONSUMER",
        "stream ended without a terminal frame",
    ))
}

async fn collect_http_stream(
    stream: &mut unionid::HttpStream,
) -> Result<Vec<Entry>, unionid::Error> {
    let mut rows = Vec::new();
    while let Some(event) = stream.next_event::<Entry>().await? {
        match event {
            TypedStreamEvent::Schema { .. } => {}
            TypedStreamEvent::Row { row, .. } => rows.push(row),
            TypedStreamEvent::Complete { .. } => return Ok(rows),
            TypedStreamEvent::Error { error, .. } => return Err(error),
        }
    }
    Err(unionid::Error::new(
        "E_CONSUMER",
        "stream ended without a terminal frame",
    ))
}

async fn verify_lost_response_retry(entry: &Entry) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    let mut engine = Engine::memory();
    checked(execute_protocol_request(
        &mut engine,
        request("retry-setup", SCHEMA)?,
    ))?;
    let engine = ConcurrentEngine::new(engine);
    let worker_engine = engine.clone();
    let worker = std::thread::spawn(move || {
        for attempt in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let request = read_request(&mut socket);
            let response = worker_engine.execute_protocol_request(request);
            if attempt == 0 {
                socket.write_all(b"{\n").unwrap();
            } else {
                serde_json::to_writer(&mut socket, &response).unwrap();
                socket.write_all(b"\n").unwrap();
            }
        }
    });
    let client = AsyncTcpClient::connect(address).await?;
    let insert = insert_request("retry-attempt", std::slice::from_ref(entry))?
        .with_idempotency_key("external-sdk-retry")?;
    let replay = checked(client.request_retrying(&insert, 2).await?)?;
    assert!(replay.idempotency.as_ref().unwrap().replayed);
    worker.join().unwrap();
    let rows = checked(engine.execute_protocol_request(request("retry-read", "from entries")?))?;
    let rows = rows.typed_rows::<Entry>()?;
    assert_eq!(rows.as_slice(), std::slice::from_ref(entry));
    Ok(())
}

async fn verify_transport_deadline() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?.to_string();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let _ = read_request(&mut socket);
        std::thread::sleep(Duration::from_millis(100));
    });
    let client = AsyncTcpClient::with_timeout(address, Duration::from_millis(10)).await?;
    let error = client
        .request(&request("deadline", "from entries")?)
        .await
        .unwrap_err();
    assert_eq!(error.code, "E_TIMEOUT");
    worker.join().unwrap();
    Ok(())
}

fn request(id: &str, source: &str) -> Result<Request, unionid::Error> {
    Request::query(id, source).with_version(PRODUCTION_VERSION)
}

fn insert_request(id: &str, rows: &[Entry]) -> Result<Request, unionid::Error> {
    request(id, "insert many entries $rows\nreturning")?.with_serde_param("rows", &rows)
}

fn checked(response: Response) -> Result<Response, unionid::Error> {
    if response.ok {
        Ok(response)
    } else {
        Err(response.error.unwrap_or_else(|| {
            unionid::Error::new("E_CONSUMER", "request failed without a structured error")
        }))
    }
}

fn failure_code(response: Response) -> String {
    assert!(!response.ok);
    response.error.expect("failed response has an error").code
}

fn sample_rows() -> Result<Vec<Entry>, unionid::Error> {
    let published = Timestamp::from_str("2026-09-12T09:30:00Z")?;
    Ok(vec![
        Entry {
            id: Uuid::from_str("018f0000-0000-7000-8000-000000000011")?,
            title: "draft".into(),
            state: State::Draft,
            created_at: Timestamp::from_str("2026-09-12T09:00:00Z")?,
        },
        Entry {
            id: Uuid::from_str("018f0000-0000-7000-8000-000000000012")?,
            title: "published".into(),
            state: State::Published { at: published },
            created_at: published,
        },
    ])
}

fn read_request(socket: &mut TcpStream) -> Request {
    let mut line = String::new();
    BufReader::new(socket.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}
