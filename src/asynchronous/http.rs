//! Official Axum adapter for the versioned query and streaming protocols.
//!
//! The adapter owns no database semantics. It runs the same
//! [`ConcurrentEngine`] entry point on Tokio's blocking pool and forwards the
//! transport-neutral stream frames without decoding or rebuilding rows.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::post;
use axum::{Json, Router};
use futures_core::Stream;

use crate::Error;
use crate::asynchronous;
use crate::protocol::{Request as ProtocolRequest, Response as ProtocolResponse};
use crate::server::ConcurrentEngine;
use crate::stream::{self, AcceptedStream};

pub const QUERY_PATH: &str = "/v1/query";
pub const STREAM_PATH: &str = "/v1/stream";
pub const CANCEL_PATH: &str = "/v1/stream/cancel";

/// Configuration shared by the official HTTP protocol routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Total execution budget applied when a request reaches the adapter.
    pub request_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
struct AdapterState {
    engine: ConcurrentEngine,
    config: Config,
}

/// Build versioned query, stream, and cancellation routes.
///
/// The returned router already owns its state and can be served directly or
/// merged with application routes. Authentication, TLS, and application
/// authorization remain the embedding service's responsibility.
pub fn router(engine: ConcurrentEngine, config: Config) -> Router {
    Router::new()
        .route(QUERY_PATH, post(query))
        .route(STREAM_PATH, post(stream_query))
        .route(CANCEL_PATH, post(cancel_stream))
        .with_state(AdapterState { engine, config })
}

async fn query(
    State(state): State<AdapterState>,
    Json(request): Json<ProtocolRequest>,
) -> Json<ProtocolResponse> {
    Json(
        asynchronous::execute_protocol_request(state.engine, request, state.config.request_timeout)
            .await,
    )
}

async fn stream_query(
    State(state): State<AdapterState>,
    Json(command): Json<stream::Request>,
) -> AxumResponse {
    let stream::Request::Query {
        stream_version,
        request,
    } = command
    else {
        return Json(stream::error_response(
            String::new(),
            Error::new("E_STREAM_SHAPE", "expected a stream query request"),
        ))
        .into_response();
    };
    let request_id = request.request_id.clone();
    if stream_version != stream::VERSION {
        return Json(stream::error_response(
            request_id,
            Error::new("E_STREAM_VERSION", "supported stream version is 1"),
        ))
        .into_response();
    }
    let deadline = match Instant::now().checked_add(state.config.request_timeout) {
        Some(deadline) => deadline,
        None => {
            return Json(stream::error_response(
                request_id,
                Error::new("E_TIMEOUT", "stream deadline exceeds the supported range"),
            ))
            .into_response();
        }
    };
    let accepted = match stream::accept(&state.engine, request, deadline, None) {
        Ok(accepted) => accepted,
        Err(error) => return Json(stream::error_response(request_id, error)).into_response(),
    };
    let operation_id = accepted.operation_id().to_owned();
    let accepted_bytes = Bytes::copy_from_slice(accepted.accepted_bytes());
    AxumResponse::builder()
        .header("content-type", "application/x-ndjson")
        .header("x-unionid-operation-id", operation_id)
        .body(Body::from_stream(HttpNdjson {
            accepted: Some(accepted),
            accepted_bytes: Some(accepted_bytes),
            receiver: None,
        }))
        .expect("static HTTP stream response is valid")
}

async fn cancel_stream(
    State(state): State<AdapterState>,
    Json(command): Json<stream::Request>,
) -> AxumResponse {
    let stream::Request::Cancel {
        stream_version,
        request_id,
        operation_id,
    } = command
    else {
        return Json(stream::error_response(
            String::new(),
            Error::new("E_STREAM_SHAPE", "expected a stream cancel request"),
        ))
        .into_response();
    };
    if stream_version != stream::VERSION {
        return Json(stream::error_response(
            request_id,
            Error::new("E_STREAM_VERSION", "supported stream version is 1"),
        ))
        .into_response();
    }
    match stream::cancel(&state.engine, request_id.clone(), operation_id) {
        Ok(response) => Json(response).into_response(),
        Err(error) => Json(stream::error_response(request_id, error)).into_response(),
    }
}

struct HttpNdjson {
    accepted: Option<AcceptedStream>,
    accepted_bytes: Option<Bytes>,
    receiver: Option<tokio::sync::mpsc::Receiver<Result<Bytes, Infallible>>>,
}

impl Stream for HttpNdjson {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(bytes) = self.accepted_bytes.take() {
            return Poll::Ready(Some(Ok(bytes)));
        }
        if self.receiver.is_none() {
            let receiver = self
                .accepted
                .take()
                .expect("HTTP stream starts only after yielding accepted")
                .start();
            let (sender, output) = tokio::sync::mpsc::channel(1);
            std::thread::spawn(move || {
                while let Ok(chunk) = receiver.recv() {
                    if sender
                        .blocking_send(Ok(Bytes::from(chunk.into_bytes())))
                        .is_err()
                    {
                        return;
                    }
                }
            });
            self.receiver = Some(output);
        }
        Pin::new(self.receiver.as_mut().expect("receiver was initialized")).poll_recv(context)
    }
}
