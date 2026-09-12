mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use common::Server;
use serde::{Deserialize, Serialize};
use unionid::{
    AsyncTcpClient, CancelStatus, ConcurrentEngine, Engine, OperationOutcome, PageSpec,
    ProtocolRequest, ProtocolResponse, TypedStreamEvent, scalars::Uuid,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Job {
    id: i64,
    state: State,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum State {
    Idle,
    Running { worker: String, attempt: i64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct NativeJob {
    id: Uuid,
    state: State,
}

fn schema() -> &'static str {
    r#"type State =
  Idle
  | Running {worker text, attempt int}

type Job =
  id int
  state State

table jobs Job
  key id"#
}

#[tokio::test]
async fn async_tcp_client_reuses_connections_pages_and_streams_adts() {
    let server = Server::start(&[]);
    let client = AsyncTcpClient::connect(server.addr.clone()).await.unwrap();
    let setup = client.query("setup", schema()).await.unwrap();
    assert!(setup.ok, "{}", setup.message);
    let jobs = vec![
        Job {
            id: 1,
            state: State::Idle,
        },
        Job {
            id: 2,
            state: State::Running {
                worker: "alpha".into(),
                attempt: 1,
            },
        },
        Job {
            id: 3,
            state: State::Idle,
        },
    ];
    let insert = ProtocolRequest::query("insert", "insert many jobs $rows\nreturning")
        .with_serde_param("rows", &jobs)
        .unwrap();
    assert_eq!(
        client
            .request(&insert)
            .await
            .unwrap()
            .typed_rows::<Job>()
            .unwrap(),
        jobs
    );

    let page_request =
        ProtocolRequest::query("page-1", "from jobs\nsort id").with_page(PageSpec::forward(2));
    let first = client.page::<Job>(&page_request).await.unwrap();
    let second = client
        .next_page::<Job>(&page_request, &first.page, "page-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.rows, jobs[..2]);
    assert_eq!(second.rows, jobs[2..]);

    let mut stream = client
        .stream(&ProtocolRequest::query("stream", "from jobs\nsort id"))
        .await
        .unwrap();
    let operation_id = stream.operation_id().to_owned();
    let mut streamed = Vec::new();
    let mut completed = false;
    while let Some(event) = stream.next_event::<Job>().await.unwrap() {
        match event {
            TypedStreamEvent::Schema { columns, .. } => assert_eq!(columns.len(), 2),
            TypedStreamEvent::Row { row, .. } => streamed.push(row),
            TypedStreamEvent::Complete { row_count, .. } => {
                assert_eq!(row_count, "3");
                completed = true;
            }
            TypedStreamEvent::Error { error, .. } => panic!("unexpected stream error: {error}"),
        }
    }
    assert_eq!(streamed, jobs);
    assert!(completed);
    let terminal = client
        .cancel("cancel-terminal", operation_id)
        .await
        .unwrap();
    assert_eq!(terminal.result.status, CancelStatus::AlreadyTerminal);
    assert_eq!(terminal.result.outcome, Some(OperationOutcome::Completed));

    let native_setup = client
        .query(
            "native-setup",
            "type NativeJob = {id uuid, state State}\ntable native_jobs NativeJob\n  key id",
        )
        .await
        .unwrap();
    assert!(native_setup.ok, "{}", native_setup.message);
    let native = NativeJob {
        id: "018f67a4-2f44-7aa3-8f2b-14f7a2f66210".parse().unwrap(),
        state: State::Idle,
    };
    let native_insert =
        ProtocolRequest::query("native-insert", "insert native_jobs $row\nreturning")
            .with_version(2)
            .unwrap()
            .with_serde_param("row", &native)
            .unwrap();
    assert_eq!(
        client
            .request(&native_insert)
            .await
            .unwrap()
            .typed_rows::<NativeJob>()
            .unwrap(),
        [native]
    );

    let cloned = client.clone();
    let (left, right) = tokio::join!(
        client.query("clone-left", "from jobs\nfilter id == 1"),
        cloned.query("clone-right", "from jobs\nfilter id == 2")
    );
    assert!(left.unwrap().ok);
    assert!(right.unwrap().ok);
}

#[tokio::test]
async fn async_tcp_client_retries_a_lost_committed_response_with_the_same_key() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let mut engine = Engine::memory();
    assert!(engine.execute(schema()).ok);
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
    let client = AsyncTcpClient::connect(address).await.unwrap();
    let job = Job {
        id: 1,
        state: State::Idle,
    };
    let request = ProtocolRequest::query("lost", "insert jobs $row\nreturning")
        .with_serde_param("row", &job)
        .unwrap()
        .with_idempotency_key("async-tcp-lost")
        .unwrap();
    let response = client.request_retrying(&request, 2).await.unwrap();
    assert_eq!(response.typed_rows::<Job>().unwrap(), [job]);
    assert_eq!(
        response
            .idempotency
            .as_ref()
            .map(|metadata| metadata.replayed),
        Some(true)
    );
    worker.join().unwrap();
    let rows = engine.execute_protocol_request(ProtocolRequest::query("rows", "from jobs"));
    assert_eq!(rows.typed_rows::<Job>().unwrap().len(), 1);
}

#[tokio::test]
async fn async_tcp_client_applies_one_deadline_to_the_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let _ = read_request(&mut socket);
        std::thread::sleep(Duration::from_millis(100));
    });
    let client = AsyncTcpClient::with_timeout(address, Duration::from_millis(10))
        .await
        .unwrap();
    let error = client.query("deadline", "from jobs").await.unwrap_err();
    assert_eq!(error.code, "E_TIMEOUT");
    worker.join().unwrap();
}

#[tokio::test]
async fn async_tcp_client_requires_a_key_for_automatic_retry() {
    let server = Server::start(&[]);
    let client = AsyncTcpClient::connect(server.addr.clone()).await.unwrap();
    let error = client
        .request_retrying(&ProtocolRequest::query("read", "from jobs"), 2)
        .await
        .unwrap_err();
    assert_eq!(error.code, "E_CONFIG");
}

fn read_request(socket: &mut TcpStream) -> ProtocolRequest {
    let mut line = String::new();
    BufReader::new(socket.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[allow(dead_code)]
fn assert_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<AsyncTcpClient>();
    check::<Arc<AsyncTcpClient>>();
    check::<ProtocolResponse>();
}
