use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use unionid::protocol::Request;
use unionid::{
    ConcurrentEngine, Engine, ObservabilityEvent, ObserverConfig, QueryAccessKind, RequestObserver,
    RequestOperation, RequestTerminal, ResourceLimit,
};

#[derive(Default)]
struct Collector(Mutex<Vec<ObservabilityEvent>>);

impl RequestObserver for Collector {
    fn observe(&self, event: &ObservabilityEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

impl Collector {
    fn events(&self) -> Vec<ObservabilityEvent> {
        self.0.lock().unwrap().clone()
    }
}

fn observed(
    threshold: Option<Duration>,
    key: Option<Vec<u8>>,
) -> (ConcurrentEngine, Arc<Collector>) {
    let mut engine = Engine::memory();
    let setup = engine.execute(
        "type Item = {id int, secret text}\ntable private_items Item\n  key id\ninsert private_items {id = 1, secret = \"never-export-this\"}",
    );
    assert!(setup.ok, "{}", setup.message);
    let collector = Arc::new(Collector::default());
    let shared = ConcurrentEngine::with_observer(
        engine,
        ObserverConfig {
            slow_query_threshold: threshold,
            slow_query_sample_basis_points: 10_000,
            request_id_hmac_key: key,
        },
        collector.clone(),
    )
    .unwrap();
    (shared, collector)
}

#[test]
fn terminal_and_slow_events_are_correlated_and_value_free() {
    let (shared, collector) = observed(Some(Duration::ZERO), Some(b"explicit-test-key".to_vec()));
    let response = shared.execute_protocol_request(Request::query(
        "private-request-id",
        "from private_items | filter id == 1 | select {secret}",
    ));
    assert!(response.ok, "{}", response.message);

    let events = collector.events();
    assert_eq!(events.len(), 2);
    let terminal = match &events[0] {
        ObservabilityEvent::RequestTerminal(event) => event,
        event => panic!("unexpected first event: {event:?}"),
    };
    let slow = match &events[1] {
        ObservabilityEvent::SlowQuery(event) => event,
        event => panic!("unexpected second event: {event:?}"),
    };
    assert_eq!(terminal, slow);
    assert_eq!(terminal.operation, RequestOperation::Read);
    assert_eq!(terminal.terminal, RequestTerminal::Completed);
    assert_eq!(terminal.work.returned_rows, 1);
    assert!(terminal.phases.prepare_micros.is_some());
    assert!(terminal.phases.plan_micros.is_some());
    assert!(terminal.phases.execution_micros.is_some());
    assert_eq!(
        terminal.plan.as_ref().unwrap().access,
        QueryAccessKind::PrimaryKeyLookup
    );
    assert!(
        terminal
            .request_id_digest
            .as_deref()
            .unwrap()
            .starts_with("hmac1:")
    );

    let json = serde_json::to_string(&events).unwrap();
    for private in [
        "private-request-id",
        "private_items",
        "secret",
        "never-export-this",
        "explicit-test-key",
        "select {secret}",
    ] {
        assert!(
            !json.contains(private),
            "observability event leaked {private}"
        );
    }
}

#[test]
fn slow_events_are_disabled_by_default_and_request_ids_are_omitted() {
    let (shared, collector) = observed(None, None);
    assert!(shared.execute("from private_items").ok);
    let events = collector.events();
    assert_eq!(events.len(), 1);
    let ObservabilityEvent::RequestTerminal(event) = &events[0] else {
        panic!("slow event emitted while disabled")
    };
    assert_eq!(event.request_id_digest, None);
}

#[test]
fn concurrent_protocol_requests_have_one_terminal_event_each() {
    let (shared, collector) = observed(None, Some(b"correlation-key".to_vec()));
    let handles = (0..8)
        .map(|index| {
            let shared = shared.clone();
            std::thread::spawn(move || {
                shared.execute_protocol_request(Request::query(
                    format!("request-{index}"),
                    "from private_items | take 1",
                ))
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        assert!(handle.join().unwrap().ok);
    }
    let events = collector.events();
    assert_eq!(events.len(), 8);
    let mut digests = events
        .iter()
        .map(|event| match event {
            ObservabilityEvent::RequestTerminal(event) => event.request_id_digest.clone().unwrap(),
            ObservabilityEvent::SlowQuery(_) => panic!("slow event emitted while disabled"),
        })
        .collect::<Vec<_>>();
    digests.sort();
    digests.dedup();
    assert_eq!(digests.len(), 8);
}

#[test]
fn cancelled_stream_has_one_cancelled_query_terminal() {
    let (shared, collector) = observed(None, Some(b"stream-key".to_vec()));
    let operation = shared
        .register_read(
            Request::query("stream-request", "from private_items"),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
    let operation_id = operation.id().to_owned();
    assert!(
        unionid::stream::cancel(&shared, "cancel-request".to_owned(), operation_id.clone(),)
            .is_ok()
    );
    let response = operation.start();
    assert_eq!(response.error.unwrap().code, "E_CANCELLED");

    let events = collector.events();
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, ObservabilityEvent::RequestTerminal(event)
                if event.operation == RequestOperation::StreamQuery
                    && event.terminal == RequestTerminal::Cancelled)
            )
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, ObservabilityEvent::RequestTerminal(event)
                if event.operation == RequestOperation::StreamCancel)
            )
            .count(),
        1
    );
    let json = serde_json::to_string(&events).unwrap();
    assert!(!json.contains("cancel-request"));
    assert!(!json.contains(&operation_id));
}

#[test]
fn invalid_observer_configuration_is_rejected() {
    let error = ConcurrentEngine::with_observer(
        Engine::memory(),
        ObserverConfig {
            slow_query_threshold: None,
            slow_query_sample_basis_points: 10_001,
            request_id_hmac_key: None,
        },
        Arc::new(Collector::default()),
    )
    .err()
    .expect("invalid config must fail");
    assert_eq!(error.code, "E_OBSERVER_CONFIG");
}

#[test]
fn write_failure_and_deadline_terminal_states_are_explicit() {
    let (shared, collector) = observed(None, None);
    let write = shared.execute_protocol_request(Request::query(
        "write",
        "insert private_items {id = 2, secret = \"hidden\"}",
    ));
    assert!(write.ok, "{}", write.message);
    assert!(
        !shared
            .execute_protocol_request(Request::query("failed", "from absent_table"))
            .ok
    );
    let deadline = shared.execute_protocol_request_until(
        Request::query("deadline", "from private_items"),
        Instant::now() - Duration::from_millis(1),
    );
    assert_eq!(deadline.error.unwrap().code, "E_TIMEOUT");

    let terminals = collector
        .events()
        .into_iter()
        .filter_map(|event| match event {
            ObservabilityEvent::RequestTerminal(event) => Some(event),
            ObservabilityEvent::SlowQuery(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminals.len(), 3);
    assert_eq!(terminals[0].operation, RequestOperation::Write);
    assert_eq!(terminals[0].terminal, RequestTerminal::Completed);
    assert!(terminals[0].phases.candidate_micros.is_some());
    assert!(terminals[0].phases.commit_micros.is_some());
    assert_eq!(terminals[1].terminal, RequestTerminal::Failed);
    assert_eq!(terminals[2].terminal, RequestTerminal::DeadlineExceeded);
    assert_eq!(terminals[2].resource_limit, Some(ResourceLimit::Deadline));
}

struct PanickingObserver;

impl RequestObserver for PanickingObserver {
    fn observe(&self, _: &ObservabilityEvent) {
        panic!("observer failure")
    }
}

#[test]
fn observer_panics_do_not_change_request_results() {
    let shared = ConcurrentEngine::with_observer(
        Engine::memory(),
        ObserverConfig::default(),
        Arc::new(PanickingObserver),
    )
    .unwrap();
    assert!(shared.execute("create table items (id int)").ok);
}

#[cfg(feature = "tracing")]
#[test]
fn optional_tracing_observer_accepts_the_same_event_contract() {
    let observer: Arc<dyn RequestObserver> = Arc::new(unionid::TracingObserver);
    let shared =
        ConcurrentEngine::with_observer(Engine::memory(), ObserverConfig::default(), observer)
            .unwrap();
    assert!(shared.execute("create table items (id int)").ok);
}
