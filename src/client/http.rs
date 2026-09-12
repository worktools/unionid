//! Async typed client for unionid's versioned HTTP query endpoint.

use std::time::Duration;

use serde::de::DeserializeOwned;

pub use super::TypedStreamEvent;
use super::{
    MAX_RESPONSE_BYTES, StreamValidator, validate_response, validate_stream_error_response,
};
use crate::db::{PageInfo, TypedPage};
use crate::error::{Error, Result};
use crate::protocol::{Request, Response};
use crate::stream::{self as stream_protocol, CancelResponse, Frame};

/// Versioned request/response endpoint used by [`HttpClient`].
pub const QUERY_PATH: &str = "/v1/query";
/// Versioned NDJSON read endpoint used by [`HttpClient::stream`].
pub const STREAM_PATH: &str = "/v1/stream";
/// Versioned cancellation endpoint used by [`HttpClient::cancel`].
pub const CANCEL_PATH: &str = "/v1/stream/cancel";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// An asynchronous typed client with pooled HTTP connections.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    query_url: reqwest::Url,
    stream_url: reqwest::Url,
    cancel_url: reqwest::Url,
    timeout: Duration,
}

/// A bounded NDJSON response whose accepted capability has been validated.
pub struct HttpStream {
    response: reqwest::Response,
    validator: Option<StreamValidator>,
    buffered: Vec<u8>,
    emitted_bytes: usize,
}

impl HttpClient {
    /// Connect to an HTTP service origin such as `http://127.0.0.1:3000`.
    pub fn connect(base_url: impl AsRef<str>) -> Result<Self> {
        Self::with_timeout(base_url, DEFAULT_TIMEOUT)
    }

    /// Connect with a per-attempt transport deadline.
    pub fn with_timeout(base_url: impl AsRef<str>, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| Error::new("E_CONFIG", format!("build HTTP client: {error}")))?;
        Self::from_client_inner(base_url, client, timeout, true)
    }

    /// Use an application-configured client for proxy, TLS, or authentication settings.
    pub fn from_client(
        base_url: impl AsRef<str>,
        client: reqwest::Client,
        timeout: Duration,
    ) -> Result<Self> {
        Self::from_client_inner(base_url, client, timeout, false)
    }

    /// Use an application-configured client against an HTTP loopback service.
    ///
    /// This explicit constructor accepts plaintext only for `localhost` or a
    /// loopback IP. Use [`Self::from_client`] for every remote service.
    pub fn from_local_client(
        base_url: impl AsRef<str>,
        client: reqwest::Client,
        timeout: Duration,
    ) -> Result<Self> {
        Self::from_client_inner(base_url, client, timeout, true)
    }

    fn from_client_inner(
        base_url: impl AsRef<str>,
        client: reqwest::Client,
        timeout: Duration,
        allow_loopback_http: bool,
    ) -> Result<Self> {
        if timeout.is_zero() {
            return Err(Error::new(
                "E_CONFIG",
                "HTTP client timeout must be greater than zero",
            ));
        }
        let mut query_url = reqwest::Url::parse(base_url.as_ref())
            .map_err(|error| Error::new("E_CONFIG", format!("invalid HTTP base URL: {error}")))?;
        if !matches!(query_url.scheme(), "http" | "https") || query_url.cannot_be_a_base() {
            return Err(Error::new(
                "E_CONFIG",
                "HTTP base URL must use http or https and include an origin",
            ));
        }
        if query_url.scheme() == "http" && (!allow_loopback_http || !is_loopback_origin(&query_url))
        {
            return Err(Error::new(
                "E_CONFIG",
                "plaintext HTTP is allowed only for explicit loopback clients",
            ));
        }
        query_url.set_path(QUERY_PATH);
        query_url.set_query(None);
        query_url.set_fragment(None);
        let mut stream_url = query_url.clone();
        stream_url.set_path(STREAM_PATH);
        let mut cancel_url = query_url.clone();
        cancel_url.set_path(CANCEL_PATH);
        Ok(Self {
            client,
            query_url,
            stream_url,
            cancel_url,
            timeout,
        })
    }

    pub async fn query(
        &self,
        request_id: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Response> {
        self.request(&Request::query(request_id, source)).await
    }

    /// Send one versioned request through the pooled HTTP client.
    pub async fn request(&self, request: &Request) -> Result<Response> {
        self.request_attempt(request)
            .await
            .map_err(|failure| failure.error)
    }

    async fn request_attempt(
        &self,
        request: &Request,
    ) -> std::result::Result<Response, AttemptFailure> {
        let encoded = serde_json::to_vec(request).map_err(|error| {
            AttemptFailure::permanent(Error::new(
                "E_PROTOCOL",
                format!("encode HTTP request: {error}"),
            ))
        })?;
        let response = self
            .client
            .post(self.query_url.clone())
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .body(encoded)
            .send()
            .await
            .map_err(AttemptFailure::transport)?;
        if !response.status().is_success() {
            let retryable = response.status().is_server_error();
            return Err(AttemptFailure {
                error: Error::new(
                    "E_HTTP_STATUS",
                    format!("HTTP query returned status {}", response.status()),
                ),
                retryable,
            });
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(AttemptFailure::permanent(response_limit()));
        }
        let mut encoded = Vec::new();
        let mut response = response;
        while let Some(chunk) = response.chunk().await.map_err(AttemptFailure::transport)? {
            if encoded.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(AttemptFailure::permanent(response_limit()));
            }
            encoded.extend_from_slice(&chunk);
        }
        let response = serde_json::from_slice(&encoded).map_err(|error| {
            AttemptFailure::retryable(Error::new(
                "E_PROTOCOL",
                format!("decode HTTP response: {error}"),
            ))
        })?;
        validate_response(request, response).map_err(AttemptFailure::permanent)
    }

    /// Retry a request after a transport or incomplete-response failure.
    ///
    /// An idempotency key is mandatory because the server may have committed a
    /// mutation before the client observes a failed response body.
    pub async fn request_retrying(&self, request: &Request, attempts: usize) -> Result<Response> {
        if request.idempotency_key.is_none() {
            return Err(Error::new(
                "E_CONFIG",
                "automatic retry requires an idempotency key",
            ));
        }
        let mut last = None;
        for _ in 0..attempts.max(1) {
            match self.request_attempt(request).await {
                Ok(response) => return Ok(response),
                Err(failure) => {
                    let retryable = failure.retryable;
                    last = Some(failure.error);
                    if !retryable {
                        break;
                    }
                }
            }
        }
        Err(last.expect("attempts >= 1 records an error"))
    }

    /// Execute and decode one typed keyset page.
    pub async fn page<T: DeserializeOwned>(&self, request: &Request) -> Result<TypedPage<T>> {
        self.request(request).await?.typed_page()
    }

    /// Continue after `current`, preserving the query, parameters, and schema condition.
    pub async fn next_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        current: &PageInfo,
        request_id: impl Into<String>,
    ) -> Result<Option<TypedPage<T>>> {
        self.continue_page(request, current.next_page(), request_id)
            .await
    }

    /// Continue before `current`, preserving the query, parameters, and schema condition.
    pub async fn previous_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        current: &PageInfo,
        request_id: impl Into<String>,
    ) -> Result<Option<TypedPage<T>>> {
        self.continue_page(request, current.previous_page(), request_id)
            .await
    }

    /// Start a read-only NDJSON stream and validate its accepted handshake.
    pub async fn stream(&self, request: &Request) -> Result<HttpStream> {
        let command = stream_protocol::Request::Query {
            stream_version: stream_protocol::VERSION,
            request: request.clone(),
        };
        let encoded = serde_json::to_vec(&command)
            .map_err(|error| Error::new("E_PROTOCOL", format!("encode stream request: {error}")))?;
        let response = self
            .client
            .post(self.stream_url.clone())
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .body(encoded)
            .send()
            .await
            .map_err(|failure| AttemptFailure::transport(failure).error)?;
        require_success(&response, "HTTP stream")?;
        let operation_id = response
            .headers()
            .get("x-unionid-operation-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let Some(operation_id) = operation_id else {
            let encoded = read_bounded(response, MAX_RESPONSE_BYTES).await?;
            if let Ok(response) = serde_json::from_slice::<stream_protocol::ErrorResponse>(&encoded)
            {
                validate_stream_error_response(&response, &request.request_id)?;
                return Err(response.error);
            }
            return Err(Error::new(
                "E_PROTOCOL",
                "stream response has no operation capability",
            ));
        };
        let mut output = HttpStream {
            response,
            validator: None,
            buffered: Vec::new(),
            emitted_bytes: 0,
        };
        let accepted = output
            .read_frame()
            .await?
            .ok_or_else(|| Error::new("E_PROTOCOL", "stream ended before the accepted frame"))?;
        output.validator = Some(StreamValidator::accepted(request, operation_id, &accepted)?);
        Ok(output)
    }

    /// Cancel a stream using its server-issued bearer capability.
    pub async fn cancel(
        &self,
        request_id: impl Into<String>,
        operation_id: impl Into<String>,
    ) -> Result<CancelResponse> {
        let request_id = request_id.into();
        let operation_id = operation_id.into();
        let command = stream_protocol::Request::Cancel {
            stream_version: stream_protocol::VERSION,
            request_id: request_id.clone(),
            operation_id: operation_id.clone(),
        };
        let encoded = serde_json::to_vec(&command)
            .map_err(|error| Error::new("E_PROTOCOL", format!("encode cancel request: {error}")))?;
        let response = self
            .client
            .post(self.cancel_url.clone())
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .body(encoded)
            .send()
            .await
            .map_err(|failure| AttemptFailure::transport(failure).error)?;
        require_success(&response, "HTTP cancel")?;
        let encoded = read_bounded(response, MAX_RESPONSE_BYTES).await?;
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
            "decode HTTP cancel response failed",
        ))
    }

    async fn continue_page<T: DeserializeOwned>(
        &self,
        request: &Request,
        page: Option<crate::query::PageSpec>,
        request_id: impl Into<String>,
    ) -> Result<Option<TypedPage<T>>> {
        let Some(page) = page else {
            return Ok(None);
        };
        let mut request = request.clone();
        request.request_id = request_id.into();
        request.page = Some(page);
        request.idempotency_key = None;
        self.page(&request).await.map(Some)
    }
}

impl HttpStream {
    /// Return the request identity copied into every stream frame.
    pub fn request_id(&self) -> &str {
        self.validator().request_id()
    }

    /// Return the server-issued bearer capability used for cancellation.
    pub fn operation_id(&self) -> &str {
        self.validator().operation_id()
    }

    /// Read and validate the next raw protocol frame.
    pub async fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.validator().terminal() {
            return Ok(None);
        }
        let Some(frame) = self.read_frame().await? else {
            return Err(Error::new(
                "E_IO",
                "stream closed before a complete or error frame",
            ));
        };
        self.validator_mut().validate_next(&frame)?;
        if self.validator().terminal() && !self.buffered.is_empty() {
            return Err(Error::new(
                "E_PROTOCOL",
                "stream included data after its terminal frame",
            ));
        }
        Ok(Some(frame))
    }

    /// Read the next event and decode row frames into an application type.
    pub async fn next_event<T: DeserializeOwned>(&mut self) -> Result<Option<TypedStreamEvent<T>>> {
        let Some(frame) = self.next_frame().await? else {
            return Ok(None);
        };
        Ok(Some(self.validator().typed_event(frame)?))
    }

    fn validator(&self) -> &StreamValidator {
        self.validator
            .as_ref()
            .expect("HttpStream is returned only after an accepted frame")
    }

    fn validator_mut(&mut self) -> &mut StreamValidator {
        self.validator
            .as_mut()
            .expect("HttpStream is returned only after an accepted frame")
    }

    async fn read_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            if let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if newline.saturating_add(1) > stream_protocol::MAX_FRAME_BYTES {
                    return Err(Error::new(
                        "E_STREAM_LIMIT",
                        "stream frame exceeds the byte limit",
                    ));
                }
                let mut line = self.buffered.drain(..=newline).collect::<Vec<_>>();
                line.pop();
                let frame: Frame = serde_json::from_slice(&line).map_err(|error| {
                    Error::new("E_PROTOCOL", format!("decode stream frame: {error}"))
                })?;
                if let Some(validator) = &self.validator {
                    super::validate_frame_identity(
                        &frame,
                        validator.request_id(),
                        validator.operation_id(),
                    )?;
                }
                return Ok(Some(frame));
            }
            if self.buffered.len() >= stream_protocol::MAX_FRAME_BYTES {
                return Err(Error::new(
                    "E_STREAM_LIMIT",
                    "stream frame exceeds the byte limit",
                ));
            }
            let Some(chunk) = self
                .response
                .chunk()
                .await
                .map_err(|failure| AttemptFailure::transport(failure).error)?
            else {
                if self.buffered.is_empty() {
                    return Ok(None);
                }
                return Err(Error::new(
                    "E_IO",
                    "stream closed before a complete frame line",
                ));
            };
            self.emitted_bytes = self.emitted_bytes.saturating_add(chunk.len());
            if self.emitted_bytes > stream_protocol::MAX_EMITTED_BYTES {
                return Err(Error::new(
                    "E_STREAM_LIMIT",
                    "stream exceeds the total byte limit",
                ));
            }
            self.buffered.extend_from_slice(&chunk);
        }
    }
}

fn require_success(response: &reqwest::Response, operation: &str) -> Result<()> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(Error::new(
            "E_HTTP_STATUS",
            format!("{operation} returned status {}", response.status()),
        ))
    }
}

async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(Error::new(
            "E_LIMIT",
            format!("HTTP response exceeds {limit} bytes"),
        ));
    }
    let mut encoded = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|failure| AttemptFailure::transport(failure).error)?
    {
        if encoded.len().saturating_add(chunk.len()) > limit {
            return Err(Error::new(
                "E_LIMIT",
                format!("HTTP response exceeds {limit} bytes"),
            ));
        }
        encoded.extend_from_slice(&chunk);
    }
    Ok(encoded)
}

fn is_loopback_origin(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

struct AttemptFailure {
    error: Error,
    retryable: bool,
}

impl AttemptFailure {
    fn permanent(error: Error) -> Self {
        Self {
            error,
            retryable: false,
        }
    }

    fn retryable(error: Error) -> Self {
        Self {
            error,
            retryable: true,
        }
    }

    fn transport(error: reqwest::Error) -> Self {
        let error = if error.is_timeout() {
            Error::new(
                "E_TIMEOUT",
                format!("HTTP request deadline expired: {error}"),
            )
        } else {
            Error::new("E_IO", format!("HTTP request failed: {error}"))
        };
        Self::retryable(error)
    }
}

fn response_limit() -> Error {
    Error::new(
        "E_LIMIT",
        format!("HTTP response exceeds {MAX_RESPONSE_BYTES} bytes"),
    )
}
