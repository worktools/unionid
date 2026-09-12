use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use unionid::asynchronous;
use unionid::asynchronous::http::{self, Config};
use unionid::{ConcurrentEngine, Engine, HttpClient, PageSpec, ProtocolRequest};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Item {
    id: i64,
    label: String,
}

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), server)
}

fn schema() -> &'static str {
    "type Item =\n  id int\n  label text\ntable items Item\n  key id"
}

#[tokio::test]
async fn typed_http_client_reuses_the_official_adapter_and_continues_pages() {
    let app = http::router(ConcurrentEngine::new(Engine::memory()), Config::default());
    let (base_url, server) = serve(app).await;
    let client = HttpClient::connect(base_url).unwrap();

    let setup = client.query("setup", schema()).await.unwrap();
    assert!(setup.ok, "{}", setup.message);
    let values = vec![
        Item {
            id: 1,
            label: "a".into(),
        },
        Item {
            id: 2,
            label: "b".into(),
        },
        Item {
            id: 3,
            label: "c".into(),
        },
    ];
    let insert = ProtocolRequest::query("insert", "insert many items $rows\nreturning")
        .with_serde_param("rows", &values)
        .unwrap();
    assert_eq!(
        client
            .request(&insert)
            .await
            .unwrap()
            .typed_rows::<Item>()
            .unwrap(),
        values
    );

    let page_request =
        ProtocolRequest::query("page-1", "from items\nsort id").with_page(PageSpec::forward(2));
    let first = client.page::<Item>(&page_request).await.unwrap();
    assert_eq!(first.rows, values[..2]);
    let second = client
        .next_page::<Item>(&page_request, &first.page, "page-2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.rows, values[2..]);
    let previous = client
        .previous_page::<Item>(&page_request, &second.page, "page-back")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(previous.rows, values[..2]);
    assert!(
        client
            .next_page::<Item>(&page_request, &second.page, "page-3")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(page_request.request_id, "page-1");
    assert_eq!(page_request.page, Some(PageSpec::forward(2)));

    server.abort();
}

#[derive(Clone)]
struct FlakyState {
    engine: ConcurrentEngine,
    calls: Arc<AtomicUsize>,
}

async fn lose_first_response(
    State(state): State<FlakyState>,
    Json(request): Json<ProtocolRequest>,
) -> axum::response::Response {
    let response =
        asynchronous::execute_protocol_request(state.engine, request, Duration::from_secs(5)).await;
    if state.calls.fetch_add(1, Ordering::SeqCst) == 0 {
        ([("content-type", "application/json")], "{").into_response()
    } else {
        Json(response).into_response()
    }
}

#[tokio::test]
async fn typed_http_client_retries_a_lost_committed_response_exactly_once() {
    let mut engine = Engine::memory();
    assert!(engine.execute(schema()).ok);
    let state = FlakyState {
        engine: ConcurrentEngine::new(engine),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route(http::QUERY_PATH, post(lose_first_response))
        .with_state(state.clone());
    let (base_url, server) = serve(app).await;
    let client = HttpClient::connect(base_url).unwrap();
    let row = Item {
        id: 1,
        label: "committed".into(),
    };
    let request = ProtocolRequest::query("lost", "insert items $row\nreturning")
        .with_serde_param("row", &row)
        .unwrap()
        .with_idempotency_key("lost-http-response")
        .unwrap();

    let replay = client.request_retrying(&request, 2).await.unwrap();
    assert!(replay.ok, "{}", replay.message);
    assert_eq!(replay.typed_rows::<Item>().unwrap(), [row]);
    assert_eq!(
        replay
            .idempotency
            .as_ref()
            .map(|metadata| metadata.replayed),
        Some(true)
    );
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    let rows = client
        .query("rows", "from items")
        .await
        .unwrap()
        .typed_rows::<Item>()
        .unwrap();
    assert_eq!(rows.len(), 1);

    server.abort();
}

#[tokio::test]
async fn typed_http_client_requires_a_key_for_automatic_retry() {
    let client = HttpClient::connect("http://127.0.0.1:1").unwrap();
    let error = client
        .request_retrying(&ProtocolRequest::query("read", "from items"), 2)
        .await
        .unwrap_err();
    assert_eq!(error.code, "E_CONFIG");
}

async fn slow_query() -> &'static str {
    tokio::time::sleep(Duration::from_millis(100)).await;
    "too late"
}

#[tokio::test]
async fn typed_http_client_reports_transport_deadlines_with_the_stable_code() {
    let app = Router::new().route(http::QUERY_PATH, post(slow_query));
    let (base_url, server) = serve(app).await;
    let client = HttpClient::with_timeout(base_url, Duration::from_millis(10)).unwrap();
    let error = client.query("deadline", "from items").await.unwrap_err();
    assert_eq!(error.code, "E_TIMEOUT");
    server.abort();
}

async fn reject_request(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
    calls.fetch_add(1, Ordering::SeqCst);
    StatusCode::UNAUTHORIZED
}

#[tokio::test]
async fn typed_http_client_does_not_retry_permanent_http_statuses() {
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(http::QUERY_PATH, post(reject_request))
        .with_state(Arc::clone(&calls));
    let (base_url, server) = serve(app).await;
    let client = HttpClient::connect(base_url).unwrap();
    let request = ProtocolRequest::query("unauthorized", "from items")
        .with_idempotency_key("unauthorized")
        .unwrap();
    let error = client.request_retrying(&request, 3).await.unwrap_err();
    assert_eq!(error.code, "E_HTTP_STATUS");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
}
