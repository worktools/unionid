//! An official client for the JSON-lines TCP protocol.
//!
//! One request is one line; the server returns one response line. The client
//! composes typed [`Request`] envelopes and decodes results with
//! [`Response::typed_rows`], so applications do not hand-write wire framing.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::{Error, Result};
use crate::protocol::{Request, Response};

/// Maximum accepted response line, matching the server response budget.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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
        serde_json::from_slice(&line)
            .map_err(|error| Error::new("E_PROTOCOL", format!("decode response: {error}")))
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
