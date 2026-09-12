//! Async typed client for unionid's versioned HTTP query endpoint.

use std::time::Duration;

use serde::de::DeserializeOwned;

use super::{MAX_RESPONSE_BYTES, validate_response};
use crate::db::{PageInfo, TypedPage};
use crate::error::{Error, Result};
use crate::protocol::{Request, Response};

pub const QUERY_PATH: &str = "/v1/query";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// An asynchronous typed client with pooled HTTP connections.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    query_url: reqwest::Url,
    timeout: Duration,
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
        Self::from_client(base_url, client, timeout)
    }

    /// Use an application-configured client for proxy, TLS, or authentication settings.
    pub fn from_client(
        base_url: impl AsRef<str>,
        client: reqwest::Client,
        timeout: Duration,
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
        query_url.set_path(QUERY_PATH);
        query_url.set_query(None);
        query_url.set_fragment(None);
        Ok(Self {
            client,
            query_url,
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
        self.page(&request).await.map(Some)
    }
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
