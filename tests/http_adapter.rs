use std::pin::Pin;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use unionid::asynchronous::http::{self, Config};
use unionid::protocol::PRODUCTION_VERSION;
use unionid::scalars::Uuid;
use unionid::stream::{self, Frame};
use unionid::{ConcurrentEngine, Engine, PageSpec, ProtocolRequest, ProtocolResponse};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Item {
    id: i64,
    label: String,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct NativeItem {
    id: i64,
    token: Uuid,
}

async fn post_json<T: Serialize>(
    app: &axum::Router,
    path: &str,
    value: &T,
) -> axum::response::Response {
    app.clone()
        .oneshot(
            HttpRequest::post(path)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(value).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn query(app: &axum::Router, request: ProtocolRequest) -> ProtocolResponse {
    let response = post_json(app, http::QUERY_PATH, &request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn official_http_adapter_runs_typed_mutations_and_pages() {
    let app = http::router(ConcurrentEngine::new(Engine::memory()), Config::default());
    let setup = query(
        &app,
        ProtocolRequest::query(
            "setup",
            "type Item =\n  id int\n  label text\ntable items Item\n  key id",
        ),
    )
    .await;
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
        .unwrap()
        .with_idempotency_key("http-items")
        .unwrap();
    let inserted = query(&app, insert).await;
    assert_eq!(inserted.typed_rows::<Item>().unwrap(), values);

    let first = query(
        &app,
        ProtocolRequest::query("page-1", "from items\nsort id").with_page(PageSpec::forward(2)),
    )
    .await
    .typed_page::<Item>()
    .unwrap();
    assert_eq!(first.rows, values[..2]);

    let second = query(
        &app,
        ProtocolRequest::query("page-2", "from items\nsort id")
            .with_page(first.page.next_page().unwrap()),
    )
    .await
    .typed_page::<Item>()
    .unwrap();
    assert_eq!(second.rows, values[2..]);
    assert!(second.page.next_page().is_none());

    let token = Uuid::from_bytes([7; 16]);
    let native_query = "from items\nderive token = $token\nsort id\nselect { id, token }";
    let version_one = ProtocolRequest::query("native-v1", native_query)
        .with_serde_param("token", &token)
        .unwrap();
    let rejected = query(&app, version_one).await;
    assert_eq!(rejected.error.unwrap().code, "E_PROTOCOL_TYPE");

    let version_two = ProtocolRequest::query("native-v2", native_query)
        .with_version(PRODUCTION_VERSION)
        .unwrap()
        .with_serde_param("token", &token)
        .unwrap();
    let native = query(&app, version_two)
        .await
        .typed_rows::<NativeItem>()
        .unwrap();
    assert_eq!(native.len(), values.len());
    assert!(native.iter().all(|row| row.token == token));
}

#[tokio::test]
async fn official_http_stream_can_be_cancelled_after_accepted() {
    let app = http::router(ConcurrentEngine::new(Engine::memory()), Config::default());
    assert!(
        query(
            &app,
            ProtocolRequest::query(
                "setup",
                "type Item =\n  id int\n  label text\ntable items Item\n  key id"
            ),
        )
        .await
        .ok
    );

    let response = post_json(
        &app,
        http::STREAM_PATH,
        &stream::Request::Query {
            stream_version: stream::VERSION,
            request: ProtocolRequest::query("stream", "from items\nsort id"),
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let accepted = std::future::poll_fn(|context| Pin::new(&mut body).poll_next(context))
        .await
        .unwrap()
        .unwrap();
    let accepted: Frame = serde_json::from_slice(&accepted).unwrap();
    let operation_id = match accepted {
        Frame::Accepted { operation_id, .. } => operation_id,
        frame => panic!("expected accepted frame, got {frame:?}"),
    };

    let cancelled = post_json(
        &app,
        http::CANCEL_PATH,
        &stream::Request::Cancel {
            stream_version: stream::VERSION,
            request_id: "cancel".into(),
            operation_id: operation_id.clone(),
        },
    )
    .await;
    let cancelled = to_bytes(cancelled.into_body(), 1024 * 1024).await.unwrap();
    let cancelled: stream::CancelResponse = serde_json::from_slice(&cancelled).unwrap();
    assert!(cancelled.ok);

    let mut terminal = None;
    while let Some(chunk) =
        std::future::poll_fn(|context| Pin::new(&mut body).poll_next(context)).await
    {
        let frame: Frame = serde_json::from_slice(&chunk.unwrap()).unwrap();
        if matches!(frame, Frame::Complete { .. } | Frame::Error { .. }) {
            terminal = Some(frame);
            break;
        }
    }
    assert!(matches!(
        terminal,
        Some(Frame::Error { operation_id: ref id, ref error, .. })
            if id == &operation_id && error.code == "E_CANCELLED"
    ));
}

#[tokio::test]
async fn official_http_adapter_preserves_deadline_errors() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute("type Item =\n  id int\n  label text\ntable items Item\n  key id")
            .ok
    );
    let app = http::router(
        ConcurrentEngine::new(engine),
        Config {
            request_timeout: Duration::ZERO,
        },
    );
    let response = query(&app, ProtocolRequest::query("expired", "from items")).await;
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TIMEOUT");
    assert!(response.page.is_none());
}
