//! Tokio client for unionid's versioned TCP request and streaming protocols.

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout_at};

use super::{
    DEFAULT_TIMEOUT, MAX_RESPONSE_BYTES, StreamValidator, TypedStreamEvent, validate_response,
    validate_stream_error_response,
};
use crate::db::{PageInfo, TypedPage};
use crate::error::{Error, Result};
use crate::protocol::{Request, Response};
use crate::stream::{self as stream_protocol, CancelResponse, Frame};

/// A cloneable asynchronous client that serializes requests over one reusable connection.
#[derive(Clone)]
pub struct AsyncTcpClient {
    address: Arc<str>,
    timeout: Duration,
    connection: Arc<Mutex<Option<BufStream<TcpStream>>>>,
}

/// A dedicated asynchronous TCP stream with a fixed absolute deadline.
pub struct AsyncTcpStream {
    stream: BufStream<TcpStream>,
    validator: StreamValidator,
    deadline: Instant,
    emitted_bytes: usize,
}

impl AsyncTcpClient {
    /// Connect using the default 30-second per-attempt deadline.
    pub async fn connect(address: impl Into<String>) -> Result<Self> {
        Self::with_timeout(address, DEFAULT_TIMEOUT).await
    }

    /// Connect using one absolute deadline for each request or stream.
    pub async fn with_timeout(address: impl Into<String>, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            return Err(Error::new(
                "E_CONFIG",
                "TCP client timeout must be greater than zero",
            ));
        }
        let address: Arc<str> = Arc::from(address.into());
        let deadline = deadline_after(timeout)?;
        let stream = connect_until(&address, deadline).await?;
        Ok(Self {
            address,
            timeout,
            connection: Arc::new(Mutex::new(Some(stream))),
        })
    }

    /// Execute source through a versioned request envelope.
    pub async fn query(
        &self,
        request_id: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Response> {
        self.request(&Request::query(request_id, source)).await
    }

    /// Send one request over the pooled connection and validate its identity.
    pub async fn request(&self, request: &Request) -> Result<Response> {
        let deadline = deadline_after(self.timeout)?;
        let guard = timeout_at(deadline, self.connection.lock())
            .await
            .map_err(|_| deadline_error("wait for the TCP connection"))?;
        self.request_locked(guard, request, deadline).await
    }

    /// Retry a keyed request on a fresh connection after an incomplete attempt.
    pub async fn request_retrying(&self, request: &Request, attempts: usize) -> Result<Response> {
        if request.idempotency_key.is_none() {
            return Err(Error::new(
                "E_CONFIG",
                "automatic retry requires an idempotency key",
            ));
        }
        let mut last = None;
        for _ in 0..attempts.max(1) {
            match self.request(request).await {
                Ok(response) => return Ok(response),
                Err(error) => last = Some(error),
            }
        }
        Err(last.expect("attempts >= 1 records an error"))
    }

    /// Execute and decode one typed keyset page.
    pub async fn page<T: DeserializeOwned>(&self, request: &Request) -> Result<TypedPage<T>> {
        self.request(request).await?.typed_page()
    }

    /// Continue after `current`, preserving the typed query and parameters.
    pub async fn next_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        current: &PageInfo,
        request_id: impl Into<String>,
    ) -> Result<Option<TypedPage<T>>> {
        self.continue_page(request, current.next_page(), request_id)
            .await
    }

    /// Continue before `current`, preserving the typed query and parameters.
    pub async fn previous_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        current: &PageInfo,
        request_id: impl Into<String>,
    ) -> Result<Option<TypedPage<T>>> {
        self.continue_page(request, current.previous_page(), request_id)
            .await
    }

    /// Start a read-only NDJSON stream on a dedicated connection.
    pub async fn stream(&self, request: &Request) -> Result<AsyncTcpStream> {
        let deadline = deadline_after(self.timeout)?;
        let mut stream = connect_until(&self.address, deadline).await?;
        let command = stream_protocol::Request::Query {
            stream_version: stream_protocol::VERSION,
            request: request.clone(),
        };
        write_json_line_until(&mut stream, &command, deadline).await?;
        let first =
            read_line_until(&mut stream, stream_protocol::MAX_FRAME_BYTES, deadline).await?;
        let emitted_bytes = first.len().saturating_add(1);
        let frame = match serde_json::from_slice::<Frame>(&first) {
            Ok(frame) => frame,
            Err(frame_error) => {
                if let Ok(response) =
                    serde_json::from_slice::<stream_protocol::ErrorResponse>(&first)
                {
                    validate_stream_error_response(&response, &request.request_id)?;
                    return Err(response.error);
                }
                return Err(Error::new(
                    "E_PROTOCOL",
                    format!("decode accepted stream frame: {frame_error}"),
                ));
            }
        };
        let operation_id = match &frame {
            Frame::Accepted { operation_id, .. } => operation_id.clone(),
            _ => {
                return Err(Error::new(
                    "E_PROTOCOL",
                    "stream must begin with an accepted frame",
                ));
            }
        };
        let validator = StreamValidator::accepted(request, operation_id, &frame)?;
        Ok(AsyncTcpStream {
            stream,
            validator,
            deadline,
            emitted_bytes,
        })
    }

    /// Cancel a stream through a separate control connection.
    pub async fn cancel(
        &self,
        request_id: impl Into<String>,
        operation_id: impl Into<String>,
    ) -> Result<CancelResponse> {
        let request_id = request_id.into();
        let operation_id = operation_id.into();
        let deadline = deadline_after(self.timeout)?;
        let mut stream = connect_until(&self.address, deadline).await?;
        let command = stream_protocol::Request::Cancel {
            stream_version: stream_protocol::VERSION,
            request_id: request_id.clone(),
            operation_id: operation_id.clone(),
        };
        write_json_line_until(&mut stream, &command, deadline).await?;
        let encoded = read_line_until(&mut stream, MAX_RESPONSE_BYTES, deadline).await?;
        if let Ok(response) = serde_json::from_slice::<CancelResponse>(&encoded) {
            if !response.ok
                || response.stream_version != stream_protocol::VERSION
                || response.request_id != request_id
                || response.operation_id != operation_id
            {
                return Err(Error::new(
                    "E_PROTOCOL",
                    "cancel response identity does not match the request",
                ));
            }
            return Ok(response);
        }
        if let Ok(response) = serde_json::from_slice::<stream_protocol::ErrorResponse>(&encoded) {
            validate_stream_error_response(&response, &request_id)?;
            return Err(response.error);
        }
        Err(Error::new(
            "E_PROTOCOL",
            "decode TCP cancel response failed",
        ))
    }

    async fn request_locked(
        &self,
        mut connection: tokio::sync::MutexGuard<'_, Option<BufStream<TcpStream>>>,
        request: &Request,
        deadline: Instant,
    ) -> Result<Response> {
        if connection.is_none() {
            *connection = Some(connect_until(&self.address, deadline).await?);
        }
        let encoded = serde_json::to_vec(request)
            .map_err(|error| Error::new("E_PROTOCOL", format!("encode request: {error}")))?;
        let result = async {
            let stream = connection.as_mut().expect("connection was established");
            write_line(stream, &encoded).await?;
            let line = read_line(stream, MAX_RESPONSE_BYTES).await?;
            let response = serde_json::from_slice(&line)
                .map_err(|error| Error::new("E_PROTOCOL", format!("decode response: {error}")))?;
            validate_response(request, response)
        };
        match timeout_at(deadline, result).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => {
                connection.take();
                Err(error)
            }
            Err(_) => {
                connection.take();
                Err(deadline_error("complete the TCP request"))
            }
        }
    }

    async fn continue_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        page: Option<crate::PageSpec>,
        request_id: impl Into<String>,
    ) -> Result<Option<TypedPage<T>>> {
        let Some(page) = page else {
            return Ok(None);
        };
        let continuation = Request {
            request_id: request_id.into(),
            page: Some(page),
            idempotency_key: None,
            ..request.clone()
        };
        self.page(&continuation).await.map(Some)
    }
}

impl AsyncTcpStream {
    /// Return the request identity copied into every stream frame.
    pub fn request_id(&self) -> &str {
        self.validator.request_id()
    }

    /// Return the server-issued bearer capability used for cancellation.
    pub fn operation_id(&self) -> &str {
        self.validator.operation_id()
    }

    /// Read and validate the next raw protocol frame.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.validator.terminal() {
            return Ok(None);
        }
        let encoded = read_line_until(
            &mut self.stream,
            stream_protocol::MAX_FRAME_BYTES,
            self.deadline,
        )
        .await?;
        self.emitted_bytes = self
            .emitted_bytes
            .saturating_add(encoded.len().saturating_add(1));
        if self.emitted_bytes > stream_protocol::MAX_EMITTED_BYTES {
            return Err(Error::new(
                "E_STREAM_LIMIT",
                "stream exceeds the total byte limit",
            ));
        }
        let frame = serde_json::from_slice(&encoded)
            .map_err(|error| Error::new("E_PROTOCOL", format!("decode stream frame: {error}")))?;
        self.validator.validate_next(&frame)?;
        Ok(Some(frame))
    }

    /// Read the next event and decode row frames into an application type.
    pub async fn next_event<T: DeserializeOwned>(&mut self) -> Result<Option<TypedStreamEvent<T>>> {
        let Some(frame) = self.next_frame().await? else {
            return Ok(None);
        };
        Ok(Some(self.validator.typed_event(frame)?))
    }
}

async fn connect_until(address: &str, deadline: Instant) -> Result<BufStream<TcpStream>> {
    let stream = timeout_at(deadline, TcpStream::connect(address))
        .await
        .map_err(|_| deadline_error("connect to the TCP service"))?
        .map_err(|error| Error::new("E_IO", format!("connect {address}: {error}")))?;
    Ok(BufStream::new(stream))
}

async fn write_json_line_until(
    stream: &mut BufStream<TcpStream>,
    value: &impl serde::Serialize,
    deadline: Instant,
) -> Result<()> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| Error::new("E_PROTOCOL", format!("encode request: {error}")))?;
    timeout_at(deadline, write_line(stream, &encoded))
        .await
        .map_err(|_| deadline_error("write the TCP request"))?
}

async fn write_line(stream: &mut BufStream<TcpStream>, encoded: &[u8]) -> Result<()> {
    stream
        .write_all(encoded)
        .await
        .map_err(|error| Error::new("E_IO", format!("write request: {error}")))?;
    stream
        .write_all(b"\n")
        .await
        .map_err(|error| Error::new("E_IO", format!("write request: {error}")))?;
    stream
        .flush()
        .await
        .map_err(|error| Error::new("E_IO", format!("flush request: {error}")))
}

async fn read_line_until(
    stream: &mut BufStream<TcpStream>,
    limit: usize,
    deadline: Instant,
) -> Result<Vec<u8>> {
    timeout_at(deadline, read_line(stream, limit))
        .await
        .map_err(|_| deadline_error("read the TCP response"))?
}

async fn read_line(stream: &mut BufStream<TcpStream>, limit: usize) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = stream
            .fill_buf()
            .await
            .map_err(|error| Error::new("E_IO", format!("read response: {error}")))?;
        if available.is_empty() {
            return Err(Error::new(
                "E_IO",
                "connection closed before a complete response line",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if line.len().saturating_add(take) > limit {
            return Err(Error::new(
                "E_LIMIT",
                format!("response exceeds {limit} bytes"),
            ));
        }
        line.extend_from_slice(&available[..take]);
        let consumed = take + usize::from(newline.is_some());
        stream.consume(consumed);
        if newline.is_some() {
            return Ok(line);
        }
    }
}

fn deadline_after(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| Error::new("E_CONFIG", "TCP client timeout exceeds the supported range"))
}

fn deadline_error(operation: &str) -> Error {
    Error::new("E_TIMEOUT", format!("deadline exceeded while {operation}"))
}
