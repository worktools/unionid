//! An official client for the JSON-lines TCP protocol.
//!
//! One request is one line; the server returns one response line. The client
//! composes typed [`Request`] envelopes and decodes results with
//! [`Response::typed_rows`], so applications do not hand-write wire framing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[cfg(any(feature = "asynchronous", feature = "http-client"))]
use serde::de::DeserializeOwned;

use crate::SchemaInfo;
use crate::db::ResponseColumn;
use crate::error::{Error, Result};
#[cfg(any(feature = "asynchronous", feature = "http-client"))]
use crate::protocol::decode_typed_row;
use crate::protocol::{Request, Response};
#[cfg(any(feature = "asynchronous", feature = "http-client"))]
use crate::stream::{self as stream_protocol, Frame};

#[cfg(feature = "asynchronous")]
pub mod asynchronous;
#[cfg(feature = "asynchronous")]
pub use asynchronous::{AsyncTcpClient, AsyncTcpStream};

#[cfg(feature = "http-client")]
pub mod http;

/// Maximum accepted response line, matching the server response budget.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One typed event after a stream's accepted handshake.
#[derive(Debug, Clone, PartialEq)]
pub enum TypedStreamEvent<T> {
    Schema {
        columns: Vec<ResponseColumn>,
        schema: SchemaInfo,
    },
    Row {
        sequence: String,
        row: T,
    },
    Complete {
        row_count: String,
        encoded_bytes: String,
        warnings: Vec<String>,
    },
    Error {
        emitted_rows: String,
        error: Error,
    },
}

#[cfg(any(feature = "asynchronous", feature = "http-client"))]
pub(crate) struct StreamValidator {
    request_id: String,
    operation_id: String,
    protocol_version: u32,
    schema_seen: bool,
    terminal: bool,
    row_count: usize,
}

#[cfg(any(feature = "asynchronous", feature = "http-client"))]
impl StreamValidator {
    pub(crate) fn accepted(request: &Request, operation_id: String, frame: &Frame) -> Result<Self> {
        validate_frame_identity(frame, &request.request_id, &operation_id)?;
        if !matches!(frame, Frame::Accepted { .. }) {
            return Err(Error::new(
                "E_PROTOCOL",
                "stream must begin with an accepted frame",
            ));
        }
        Ok(Self {
            request_id: request.request_id.clone(),
            operation_id,
            protocol_version: request.version,
            schema_seen: false,
            terminal: false,
            row_count: 0,
        })
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn terminal(&self) -> bool {
        self.terminal
    }

    pub(crate) fn validate_next(&mut self, frame: &Frame) -> Result<()> {
        validate_frame_identity(frame, &self.request_id, &self.operation_id)?;
        match frame {
            Frame::Accepted { .. } => {
                return Err(Error::new(
                    "E_PROTOCOL",
                    "stream sent accepted more than once",
                ));
            }
            Frame::Schema { .. } if self.schema_seen => {
                return Err(Error::new(
                    "E_PROTOCOL",
                    "stream sent schema more than once",
                ));
            }
            Frame::Schema { .. } => self.schema_seen = true,
            Frame::Row { .. } if !self.schema_seen => {
                return Err(Error::new("E_PROTOCOL", "stream sent a row before schema"));
            }
            Frame::Complete { .. } if !self.schema_seen => {
                return Err(Error::new("E_PROTOCOL", "stream completed before schema"));
            }
            Frame::Row { sequence, .. } => {
                if sequence != &self.row_count.to_string() {
                    return Err(Error::new(
                        "E_PROTOCOL",
                        "stream row sequence is not contiguous",
                    ));
                }
                self.row_count = self.row_count.checked_add(1).ok_or_else(|| {
                    Error::new(
                        "E_STREAM_LIMIT",
                        "stream row count exceeds the client limit",
                    )
                })?;
            }
            Frame::Complete { row_count, .. } => {
                if row_count != &self.row_count.to_string() {
                    return Err(Error::new(
                        "E_PROTOCOL",
                        "stream complete row count does not match emitted rows",
                    ));
                }
                self.terminal = true;
            }
            Frame::Error { emitted_rows, .. } => {
                if emitted_rows != &self.row_count.to_string() {
                    return Err(Error::new(
                        "E_PROTOCOL",
                        "stream error row count does not match emitted rows",
                    ));
                }
                self.terminal = true;
            }
        }
        Ok(())
    }

    pub(crate) fn typed_event<T: DeserializeOwned>(
        &self,
        frame: Frame,
    ) -> Result<TypedStreamEvent<T>> {
        let event = match frame {
            Frame::Schema {
                columns, schema, ..
            } => TypedStreamEvent::Schema { columns, schema },
            Frame::Row { sequence, row, .. } => TypedStreamEvent::Row {
                sequence,
                row: decode_typed_row(self.protocol_version, &row, self.row_count)?,
            },
            Frame::Complete {
                row_count,
                encoded_bytes,
                warnings,
                ..
            } => TypedStreamEvent::Complete {
                row_count,
                encoded_bytes,
                warnings,
            },
            Frame::Error {
                emitted_rows,
                error,
                ..
            } => TypedStreamEvent::Error {
                emitted_rows,
                error,
            },
            Frame::Accepted { .. } => {
                return Err(Error::new(
                    "E_PROTOCOL",
                    "accepted is a handshake rather than a stream event",
                ));
            }
        };
        Ok(event)
    }
}

#[cfg(any(feature = "asynchronous", feature = "http-client"))]
pub(crate) fn validate_stream_error_response(
    response: &stream_protocol::ErrorResponse,
    request_id: &str,
) -> Result<()> {
    if response.ok
        || response.stream_version != stream_protocol::VERSION
        || response.request_id != request_id
    {
        return Err(Error::new(
            "E_PROTOCOL",
            "stream error response identity does not match the request",
        ));
    }
    Ok(())
}

#[cfg(any(feature = "asynchronous", feature = "http-client"))]
pub(crate) fn validate_frame_identity(
    frame: &Frame,
    request_id: &str,
    operation_id: &str,
) -> Result<()> {
    let (version, frame_request_id, frame_operation_id) = match frame {
        Frame::Accepted {
            stream_version,
            request_id,
            operation_id,
        }
        | Frame::Schema {
            stream_version,
            request_id,
            operation_id,
            ..
        }
        | Frame::Row {
            stream_version,
            request_id,
            operation_id,
            ..
        }
        | Frame::Complete {
            stream_version,
            request_id,
            operation_id,
            ..
        }
        | Frame::Error {
            stream_version,
            request_id,
            operation_id,
            ..
        } => (*stream_version, request_id, operation_id),
    };
    if version != stream_protocol::VERSION
        || frame_request_id != request_id
        || frame_operation_id != operation_id
    {
        return Err(Error::new(
            "E_PROTOCOL",
            "stream frame identity does not match the accepted operation",
        ));
    }
    Ok(())
}

/// A synchronous typed client over the versioned TCP JSON-lines protocol.
pub struct TcpClient {
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl TcpClient {
    pub fn connect(addr: impl AsRef<str>) -> Result<Self> {
        let addr = addr.as_ref();
        let stream = TcpStream::connect(addr)
            .map_err(|error| Error::new("E_IO", format!("connect {addr}: {error}")))?;
        stream
            .set_read_timeout(Some(DEFAULT_TIMEOUT))
            .map_err(|error| Error::new("E_IO", format!("set read timeout: {error}")))?;
        stream
            .set_write_timeout(Some(DEFAULT_TIMEOUT))
            .map_err(|error| Error::new("E_IO", format!("set write timeout: {error}")))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|error| Error::new("E_IO", format!("clone stream: {error}")))?,
        );
        Ok(Self { stream, reader })
    }

    pub fn query(
        &mut self,
        request_id: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Response> {
        self.request(&Request::query(request_id, source))
    }

    /// Send one request and read one response line.
    pub fn request(&mut self, request: &Request) -> Result<Response> {
        let encoded = serde_json::to_vec(request)
            .map_err(|error| Error::new("E_PROTOCOL", format!("encode request: {error}")))?;
        self.stream
            .write_all(&encoded)
            .and_then(|()| self.stream.write_all(b"\n"))
            .and_then(|()| self.stream.flush())
            .map_err(|error| Error::new("E_IO", format!("write request: {error}")))?;
        let mut line = Vec::new();
        let mut limited = (&mut self.reader).take((MAX_RESPONSE_BYTES + 1) as u64);
        limited
            .read_until(b'\n', &mut line)
            .map_err(|error| Error::new("E_IO", format!("read response: {error}")))?;
        if line.last() != Some(&b'\n') {
            return Err(Error::new(
                "E_IO",
                "connection closed before a complete response line",
            ));
        }
        line.pop();
        if line.len() > MAX_RESPONSE_BYTES {
            return Err(Error::new(
                "E_LIMIT",
                format!("response exceeds {MAX_RESPONSE_BYTES} bytes"),
            ));
        }
        let response = serde_json::from_slice(&line)
            .map_err(|error| Error::new("E_PROTOCOL", format!("decode response: {error}")))?;
        validate_response(request, response)
    }

    /// Reconnect and resend a request after a transport failure. Automatic
    /// retry requires an idempotency key so a mutation cannot run twice.
    pub fn request_retrying(
        &mut self,
        addr: impl AsRef<str>,
        request: &Request,
        attempts: usize,
    ) -> Result<Response> {
        if request.idempotency_key.is_none() {
            return Err(Error::new(
                "E_CONFIG",
                "automatic retry requires an idempotency key",
            ));
        }
        let addr = addr.as_ref();
        let attempts = attempts.max(1);
        let mut last = None;
        for attempt in 0..attempts {
            match self.request(request) {
                Ok(response) => return Ok(response),
                Err(error) => {
                    last = Some(error);
                    if attempt + 1 < attempts {
                        *self = Self::connect(addr)?;
                    }
                }
            }
        }
        Err(last.expect("attempts >= 1 records an error"))
    }
}

pub(crate) fn validate_response(request: &Request, response: Response) -> Result<Response> {
    if response.request_id != request.request_id {
        return Err(Error::new(
            "E_PROTOCOL",
            format!(
                "response request_id '{}' does not match request '{}'",
                response.request_id, request.request_id
            ),
        ));
    }
    if response.version != request.version {
        return Err(Error::new(
            "E_PROTOCOL_VERSION",
            format!(
                "response version {} does not match request version {}",
                response.version, request.version
            ),
        ));
    }
    Ok(response)
}
