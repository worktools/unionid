//! Transport-neutral NDJSON streaming protocol and bounded frame producer.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::control::ExecutionControl;
use crate::db::{QueryRowSink, ResponseColumn};
use crate::protocol::{Request as ProtocolRequest, WireValue};
use crate::server::{
    CancelResult, ConcurrentEngine, OperationOutcome, ReadOperation, StreamReadExecution,
};
use crate::{Error, SchemaInfo, Value};

pub const VERSION: u32 = 1;
pub const MAX_QUEUED_FRAMES: usize = 8;
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_EMITTED_BYTES: usize = 256 * 1024 * 1024;
const BACKPRESSURE_POLL: Duration = Duration::from_millis(100);
const TERMINAL_SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Query {
        stream_version: u32,
        request: ProtocolRequest,
    },
    Cancel {
        stream_version: u32,
        request_id: String,
        operation_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "frame", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Accepted {
        stream_version: u32,
        request_id: String,
        operation_id: String,
    },
    Schema {
        stream_version: u32,
        request_id: String,
        operation_id: String,
        columns: Vec<ResponseColumn>,
        schema: SchemaInfo,
    },
    Row {
        stream_version: u32,
        request_id: String,
        operation_id: String,
        sequence: String,
        row: BTreeMap<String, WireValue>,
    },
    Complete {
        stream_version: u32,
        request_id: String,
        operation_id: String,
        row_count: String,
        encoded_bytes: String,
        warnings: Vec<String>,
    },
    Error {
        stream_version: u32,
        request_id: String,
        operation_id: String,
        emitted_rows: String,
        error: Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CancelResponse {
    pub stream_version: u32,
    pub request_id: String,
    pub operation_id: String,
    pub ok: bool,
    #[serde(flatten)]
    pub result: CancelResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub stream_version: u32,
    pub request_id: String,
    pub ok: bool,
    pub error: Error,
}

pub struct AcceptedStream {
    operation: ReadOperation,
    operation_id: String,
    request_id: String,
    accepted: Vec<u8>,
}

impl AcceptedStream {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn accepted_bytes(&self) -> &[u8] {
        &self.accepted
    }

    /// Start execution only after the adapter has completely flushed accepted.
    pub fn start(self) -> StreamReceiver {
        let (sender, receiver) = sync_channel(MAX_QUEUED_FRAMES);
        let budget = Arc::new(ByteBudget::default());
        let accepted_bytes = self.accepted.len();
        std::thread::spawn(move || produce(self.operation, accepted_bytes, sender, budget));
        StreamReceiver { receiver }
    }
}

pub struct StreamChunk {
    bytes: Vec<u8>,
    _reservation: ByteReservation,
}

impl StreamChunk {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub struct StreamReceiver {
    receiver: Receiver<StreamChunk>,
}

impl StreamReceiver {
    pub fn recv(&self) -> Result<StreamChunk, std::sync::mpsc::RecvError> {
        self.receiver.recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<StreamChunk, std::sync::mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

pub fn accept(
    engine: &ConcurrentEngine,
    request: ProtocolRequest,
    deadline: Instant,
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<AcceptedStream, Error> {
    if Instant::now() >= deadline {
        return Err(Error::new("E_TIMEOUT", "stream deadline already expired"));
    }
    let request_id = request.request_id.clone();
    let operation = engine.register_read_with_shutdown(request, deadline, shutdown)?;
    let operation_id = operation.id().to_owned();
    let accepted = encode(&Frame::Accepted {
        stream_version: VERSION,
        request_id: request_id.clone(),
        operation_id: operation_id.clone(),
    })?;
    Ok(AcceptedStream {
        operation,
        operation_id,
        request_id,
        accepted,
    })
}

pub fn cancel(
    engine: &ConcurrentEngine,
    request_id: String,
    operation_id: String,
) -> Result<CancelResponse, Error> {
    if request_id.len() > crate::protocol::MAX_REQUEST_ID_BYTES {
        return Err(Error::new("E_LIMIT", "request_id exceeds byte limit"));
    }
    let result = engine.cancel(&operation_id)?;
    Ok(CancelResponse {
        stream_version: VERSION,
        request_id,
        operation_id,
        ok: true,
        result,
    })
}

pub fn error_response(request_id: String, error: Error) -> ErrorResponse {
    ErrorResponse {
        stream_version: VERSION,
        request_id,
        ok: false,
        error,
    }
}

pub fn encode(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| Error::new("E_PROTOCOL", format!("encode stream frame: {error}")))?;
    if bytes.len().saturating_add(1) > MAX_FRAME_BYTES {
        return Err(Error::new(
            "E_STREAM_LIMIT",
            format!("stream frame exceeds {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn produce(
    operation: ReadOperation,
    accepted_bytes: usize,
    sender: SyncSender<StreamChunk>,
    budget: Arc<ByteBudget>,
) {
    let control = operation.stream_control();
    let operation_id = operation.id().to_owned();
    let request_id = operation.request_id().to_owned();
    let version = operation.version();
    let mut output = PipelineFrameSink {
        request_id,
        operation_id: operation_id.clone(),
        version,
        sender: &sender,
        budget: &budget,
        control,
        emitted_rows: 0,
        encoded_bytes: accepted_bytes,
    };
    let mut execution = operation.start_stream(&mut output);
    let result = if execution.response.ok {
        Ok(())
    } else {
        Err(execution
            .response
            .error
            .take()
            .unwrap_or_else(|| Error::new("E_QUERY", "stream query failed")))
    };
    let emitted_rows = output.emitted_rows;
    let encoded_bytes = output.encoded_bytes;
    drop(output);
    let proposed = match &result {
        Ok(()) => OperationOutcome::Completed,
        Err(error) if error.code == "E_CANCELLED" => OperationOutcome::Cancelled,
        Err(_) => OperationOutcome::Failed,
    };
    let outcome = execution.finish(proposed);
    let terminal = match (result, outcome) {
        (Ok(()), OperationOutcome::Completed) => Frame::Complete {
            stream_version: VERSION,
            request_id: execution.request_id.clone(),
            operation_id: execution.operation_id.clone(),
            row_count: emitted_rows.to_string(),
            encoded_bytes: encoded_bytes.to_string(),
            warnings: std::mem::take(&mut execution.response.warnings),
        },
        (Err(error), OperationOutcome::Failed) => error_frame(&execution, emitted_rows, error),
        (_, OperationOutcome::Cancelled) => error_frame(
            &execution,
            emitted_rows,
            Error::new("E_CANCELLED", "read operation cancelled"),
        ),
        (_, OperationOutcome::Failed) => error_frame(
            &execution,
            emitted_rows,
            Error::new("E_INTERNAL", "stream operation failed"),
        ),
        (_, OperationOutcome::Completed) => return,
    };
    if send_frame(&execution, &sender, &budget, terminal, None).is_err() {
        execution.cancel_local();
    }
}

struct PipelineFrameSink<'a> {
    request_id: String,
    operation_id: String,
    version: u32,
    sender: &'a SyncSender<StreamChunk>,
    budget: &'a Arc<ByteBudget>,
    control: ExecutionControl,
    emitted_rows: usize,
    encoded_bytes: usize,
}

impl QueryRowSink for PipelineFrameSink<'_> {
    fn begin(&mut self, columns: &[ResponseColumn], schema: &SchemaInfo) -> Result<(), Error> {
        let frame = Frame::Schema {
            stream_version: VERSION,
            request_id: self.request_id.clone(),
            operation_id: self.operation_id.clone(),
            columns: columns.to_vec(),
            schema: schema.clone(),
        };
        self.encoded_bytes = self.encoded_bytes.saturating_add(send_frame_controlled(
            &self.control,
            self.sender,
            self.budget,
            frame,
            Some(self.encoded_bytes),
        )?);
        Ok(())
    }

    fn row(&mut self, row: BTreeMap<String, Value>) -> Result<(), Error> {
        self.control.checkpoint()?;
        let row = row
            .into_iter()
            .map(|(name, value)| (name, WireValue::from(&value)))
            .collect::<BTreeMap<_, _>>();
        if self.version == crate::protocol::VERSION && row.values().any(WireValue::requires_v2) {
            return Err(Error::new(
                "E_PROTOCOL_TYPE",
                "production scalar rows require protocol version 2",
            ));
        }
        let frame = Frame::Row {
            stream_version: VERSION,
            request_id: self.request_id.clone(),
            operation_id: self.operation_id.clone(),
            sequence: self.emitted_rows.to_string(),
            row,
        };
        self.encoded_bytes = self.encoded_bytes.saturating_add(send_frame_controlled(
            &self.control,
            self.sender,
            self.budget,
            frame,
            Some(self.encoded_bytes),
        )?);
        self.emitted_rows = self.emitted_rows.saturating_add(1);
        Ok(())
    }
}

fn error_frame(execution: &StreamReadExecution, emitted_rows: usize, error: Error) -> Frame {
    Frame::Error {
        stream_version: VERSION,
        request_id: execution.request_id.clone(),
        operation_id: execution.operation_id.clone(),
        emitted_rows: emitted_rows.to_string(),
        error,
    }
}

fn send_frame(
    execution: &StreamReadExecution,
    sender: &SyncSender<StreamChunk>,
    budget: &Arc<ByteBudget>,
    frame: Frame,
    accounted_bytes: Option<usize>,
) -> Result<usize, Error> {
    send_frame_controlled(execution, sender, budget, frame, accounted_bytes)
}

trait StreamCheckpoint {
    fn checkpoint(&self) -> Result<(), Error>;
}

impl StreamCheckpoint for StreamReadExecution {
    fn checkpoint(&self) -> Result<(), Error> {
        StreamReadExecution::checkpoint(self)
    }
}

impl StreamCheckpoint for ExecutionControl {
    fn checkpoint(&self) -> Result<(), Error> {
        ExecutionControl::checkpoint(self)
    }
}

fn send_frame_controlled(
    execution: &dyn StreamCheckpoint,
    sender: &SyncSender<StreamChunk>,
    budget: &Arc<ByteBudget>,
    frame: Frame,
    accounted_bytes: Option<usize>,
) -> Result<usize, Error> {
    let bytes = encode(&frame)?;
    let size = bytes.len();
    if accounted_bytes.is_some_and(|used| used.saturating_add(size) > MAX_EMITTED_BYTES) {
        return Err(Error::new(
            "E_STREAM_LIMIT",
            format!("stream exceeds {MAX_EMITTED_BYTES} encoded byte limit"),
        ));
    }
    let checked = accounted_bytes.is_some();
    let terminal_deadline = (!checked).then(|| Instant::now() + TERMINAL_SEND_TIMEOUT);
    let reservation = budget.reserve(size, execution, checked, terminal_deadline)?;
    let mut chunk = StreamChunk {
        bytes,
        _reservation: reservation,
    };
    loop {
        if checked {
            execution.checkpoint()?;
        } else if terminal_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(Error::new(
                "E_TIMEOUT",
                "stream terminal delivery deadline exceeded",
            ));
        }
        match sender.try_send(chunk) {
            Ok(()) => return Ok(size),
            Err(TrySendError::Full(returned)) => {
                chunk = returned;
                std::thread::sleep(BACKPRESSURE_POLL);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(Error::new("E_CANCELLED", "stream consumer disconnected"));
            }
        }
    }
}

#[derive(Default)]
struct ByteBudget {
    used: Mutex<usize>,
    ready: Condvar,
}

impl ByteBudget {
    fn reserve(
        self: &Arc<Self>,
        size: usize,
        execution: &dyn StreamCheckpoint,
        checked: bool,
        terminal_deadline: Option<Instant>,
    ) -> Result<ByteReservation, Error> {
        if size > MAX_QUEUED_BYTES {
            return Err(Error::new(
                "E_STREAM_LIMIT",
                format!("stream frame exceeds {MAX_QUEUED_BYTES} queued byte limit"),
            ));
        }
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while used.saturating_add(size) > MAX_QUEUED_BYTES {
            if checked {
                execution.checkpoint()?;
            } else if terminal_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Error::new(
                    "E_TIMEOUT",
                    "stream terminal delivery deadline exceeded",
                ));
            }
            let (next, _) = self
                .ready
                .wait_timeout(used, BACKPRESSURE_POLL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            used = next;
        }
        *used += size;
        Ok(ByteReservation {
            budget: Arc::clone(self),
            size,
        })
    }
}

struct ByteReservation {
    budget: Arc<ByteBudget>,
    size: usize,
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *used -= self.size;
        self.budget.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;

    fn wait_for(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition did not become true");
            std::thread::yield_now();
        }
    }

    fn decode(chunk: &StreamChunk) -> Frame {
        serde_json::from_slice(chunk.as_bytes()).unwrap()
    }

    #[test]
    fn producer_emits_one_typed_sequence_and_releases_the_operation() {
        let engine = ConcurrentEngine::new(Engine::memory());
        assert!(engine.execute("create table items (id int, value text)").ok);
        assert!(engine.execute("insert items {id: 1, value: \"one\"}").ok);
        let accepted = accept(
            &engine,
            ProtocolRequest::query("stream-one", "from items | sort id"),
            Instant::now() + Duration::from_secs(2),
            None,
        )
        .unwrap();
        let accepted_frame: Frame = serde_json::from_slice(accepted.accepted_bytes()).unwrap();
        assert!(matches!(accepted_frame, Frame::Accepted { .. }));
        let receiver = accepted.start();
        let frames = std::iter::from_fn(|| receiver.recv().ok())
            .map(|chunk| decode(&chunk))
            .collect::<Vec<_>>();
        assert!(matches!(frames[0], Frame::Schema { .. }));
        assert!(matches!(frames[1], Frame::Row { ref sequence, .. } if sequence == "0"));
        assert!(matches!(
            frames[2],
            Frame::Complete {
                ref row_count,
                ..
            } if row_count == "1"
        ));
        assert_eq!(engine.stats().registered_operations, 0);
    }

    #[test]
    fn bounded_backpressure_keeps_one_snapshot_and_cancel_ends_partial_stream() {
        let engine = ConcurrentEngine::new(Engine::memory());
        assert!(engine.execute("create table items (id int)").ok);
        let source = (0..32)
            .map(|id| format!("insert items {{id: {id}}}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(engine.execute(&source).ok);
        let accepted = accept(
            &engine,
            ProtocolRequest::query("stream-cancel", "from items | sort id"),
            Instant::now() + Duration::from_secs(2),
            None,
        )
        .unwrap();
        let operation_id = accepted.operation_id().to_owned();
        let receiver = accepted.start();
        let schema = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(decode(&schema), Frame::Schema { .. }));
        let first_row = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(decode(&first_row), Frame::Row { .. }));
        wait_for(|| engine.stats().active_reads == 1);
        assert_eq!(
            engine.cancel(&operation_id).unwrap().status,
            crate::server::CancelStatus::Accepted
        );

        let frames = std::iter::from_fn(|| receiver.recv().ok())
            .map(|chunk| decode(&chunk))
            .collect::<Vec<_>>();
        assert!(
            matches!(frames.last(), Some(Frame::Error { error, .. }) if error.code == "E_CANCELLED")
        );
        assert_eq!(engine.stats().registered_operations, 0);
        assert_eq!(engine.stats().active_reads, 0);
    }

    #[test]
    fn dropping_a_consumer_cancels_and_releases_the_registry_entry() {
        let engine = ConcurrentEngine::new(Engine::memory());
        assert!(engine.execute("create table items (id int)").ok);
        let source = (0..32)
            .map(|id| format!("insert items {{id: {id}}}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(engine.execute(&source).ok);
        let accepted = accept(
            &engine,
            ProtocolRequest::query("drop-stream", "from items"),
            Instant::now() + Duration::from_secs(2),
            None,
        )
        .unwrap();
        let operation_id = accepted.operation_id().to_owned();
        let receiver = accepted.start();
        let first = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut partial_write = Vec::new();
        partial_write.extend_from_slice(&first.as_bytes()[..first.as_bytes().len() / 2]);
        assert!(!partial_write.ends_with(b"\n"));
        drop(receiver);
        wait_for(|| engine.stats().registered_operations == 0);
        assert_eq!(engine.stats().active_reads, 0);
        assert_eq!(
            engine.cancel(&operation_id).unwrap().outcome,
            Some(OperationOutcome::Cancelled)
        );
    }

    #[test]
    fn deadline_and_shutdown_end_a_backpressured_stream() {
        fn populated() -> ConcurrentEngine {
            let engine = ConcurrentEngine::new(Engine::memory());
            assert!(engine.execute("create table items (id int)").ok);
            let source = (0..32)
                .map(|id| format!("insert items {{id: {id}}}"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(engine.execute(&source).ok);
            engine
        }

        let engine = populated();
        let accepted = accept(
            &engine,
            ProtocolRequest::query("deadline", "from items | sort id"),
            Instant::now() + Duration::from_millis(100),
            None,
        )
        .unwrap();
        let receiver = accepted.start();
        assert!(matches!(
            decode(&receiver.recv().unwrap()),
            Frame::Schema { .. }
        ));
        std::thread::sleep(Duration::from_millis(150));
        let frames = std::iter::from_fn(|| receiver.recv().ok())
            .map(|chunk| decode(&chunk))
            .collect::<Vec<_>>();
        assert!(
            matches!(frames.last(), Some(Frame::Error { error, .. }) if error.code == "E_TIMEOUT")
        );
        assert_eq!(engine.stats().registered_operations, 0);

        let engine = populated();
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let accepted = accept(
            &engine,
            ProtocolRequest::query("shutdown", "from items | sort id"),
            Instant::now() + Duration::from_secs(2),
            Some(Arc::clone(&shutdown)),
        )
        .unwrap();
        let receiver = accepted.start();
        assert!(matches!(
            decode(&receiver.recv().unwrap()),
            Frame::Schema { .. }
        ));
        shutdown.store(true, std::sync::atomic::Ordering::Release);
        let frames = std::iter::from_fn(|| receiver.recv().ok())
            .map(|chunk| decode(&chunk))
            .collect::<Vec<_>>();
        assert!(
            matches!(frames.last(), Some(Frame::Error { error, .. }) if error.code == "E_SHUTDOWN")
        );
        assert_eq!(engine.stats().registered_operations, 0);
    }
}
