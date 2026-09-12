//! Async adapters that run the synchronous Engine off an async executor.
//!
//! These helpers wrap the same `ConcurrentEngine` entry point in
//! `spawn_blocking`, so an async framework handler does not need its own
//! thread-pool or a `Mutex<Engine>`. Deadline and error semantics are the
//! synchronous execution semantics.

use std::time::{Duration, Instant};

use crate::db::SchemaInfo;
use crate::error::{Error, Result};
use crate::protocol::{Request, Response};
use crate::server::ConcurrentEngine;

#[cfg(feature = "http")]
pub mod http;

/// Run a blocking closure on the async runtime's blocking pool.
pub async fn run<T: Send + 'static>(operation: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|join| Error::new("E_RUNTIME", format!("blocking worker failed: {join}")))
}

/// Execute a version-1 protocol request with an absolute `deadline`.
pub async fn execute_protocol_request(
    engine: ConcurrentEngine,
    request: Request,
    deadline: Duration,
) -> Response {
    let request_id = request.request_id.clone();
    let schema = request.schema.clone().unwrap_or(SchemaInfo {
        revision: 0,
        hash: String::new(),
    });
    let deadline = Instant::now()
        .checked_add(deadline)
        .unwrap_or_else(Instant::now);
    match run(move || engine.execute_protocol_request_until(request, deadline)).await {
        Ok(response) => response,
        Err(error) => Response::failure(request_id, error, schema),
    }
}
