//! Opt-in maintenance scheduling; no policy is stored in database state.
use super::*;
use crate::ReceiptRetentionPolicy;
use std::sync::TryLockError;
use std::thread::JoinHandle;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptRetentionSchedule {
    pub policy: ReceiptRetentionPolicy,
    pub interval_seconds: u64,
}

impl ReceiptRetentionSchedule {
    pub fn validate(&self) -> Result<(), Error> {
        self.policy.validate()?;
        if !(1..=86_400).contains(&self.interval_seconds) {
            return Err(Error::new(
                "E_CONFIG",
                "receipt retention interval must be 1..=86400 seconds",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptRetentionState {
    Idle,
    Busy,
    Maintenance,
    ClockError,
    Applied,
    Failed,
}

/// Fixed-size, value-free last-pass information. No receipt boundaries or keys.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptRetentionStatus {
    pub schema_version: u32,
    pub state: ReceiptRetentionState,
    pub as_of_unix_ms: Option<u64>,
    pub deleted_count: usize,
    pub deleted_encoded_bytes: usize,
}

impl ReceiptRetentionStatus {
    fn new(state: ReceiptRetentionState) -> Self {
        Self {
            schema_version: 1,
            state,
            as_of_unix_ms: None,
            deleted_count: 0,
            deleted_encoded_bytes: 0,
        }
    }
}

/// Dropping or stopping the handle wakes and joins the worker. An in-flight
/// transaction finishes under the ordinary Engine commit-outcome contract.
pub struct ReceiptRetentionWorker {
    wake: Arc<(Mutex<bool>, Condvar)>,
    status: Arc<Mutex<ReceiptRetentionStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl ReceiptRetentionWorker {
    pub fn status(&self) -> ReceiptRetentionStatus {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn stop(mut self) -> Result<(), Error> {
        self.join()
    }

    fn join(&mut self) -> Result<(), Error> {
        *self
            .wake
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.wake.1.notify_all();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| Error::new("E_STORAGE", "receipt retention worker panicked"))?;
        }
        Ok(())
    }
}

impl Drop for ReceiptRetentionWorker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

#[derive(Default)]
struct ClockGuard {
    high_water: Option<u64>,
}
impl ClockGuard {
    fn observe(&mut self, now: Result<u64, Error>) -> Option<u64> {
        let now = now.ok()?;
        if self.high_water.is_some_and(|previous| now < previous) {
            return None;
        }
        self.high_water = Some(now);
        Some(now)
    }
}

impl ConcurrentEngine {
    /// Start exactly one explicitly configured worker for this shared engine.
    /// The first pass runs after one interval. HTTP/embedded callers retain the
    /// handle and inspect status; TCP also supplies its shutdown signal.
    pub fn start_receipt_retention(
        &self,
        schedule: ReceiptRetentionSchedule,
        shutdown: Arc<AtomicBool>,
    ) -> Result<ReceiptRetentionWorker, Error> {
        schedule.validate()?;
        self.with_exclusive(|engine| engine.validate_receipt_retention_service())?;
        if self
            .inner
            .retention_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::new(
                "E_CONFIG",
                "receipt retention worker is already running",
            ));
        }
        let wake = Arc::new((Mutex::new(false), Condvar::new()));
        let status = Arc::new(Mutex::new(ReceiptRetentionStatus::new(
            ReceiptRetentionState::Idle,
        )));
        let engine = self.clone();
        let worker_wake = Arc::clone(&wake);
        let worker_status = Arc::clone(&status);
        let thread = std::thread::Builder::new()
            .name("unionid-retention".into())
            .spawn(move || {
                struct Running(ConcurrentEngine);
                impl Drop for Running {
                    fn drop(&mut self) {
                        self.0
                            .inner
                            .retention_running
                            .store(false, Ordering::Release);
                    }
                }
                let _running = Running(engine.clone());
                let mut clock = ClockGuard::default();
                // Observe startup time too, so a rollback before the first pass is detected.
                clock.observe(crate::engine::unix_time_ms());
                let interval = Duration::from_secs(schedule.interval_seconds);
                loop {
                    let until = Instant::now() + interval;
                    let mut stopped = worker_wake
                        .0
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    while !*stopped && !shutdown.load(Ordering::Acquire) && Instant::now() < until {
                        let wait = until
                            .saturating_duration_since(Instant::now())
                            .min(CONNECTION_POLL);
                        stopped = worker_wake
                            .1
                            .wait_timeout(stopped, wait)
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .0;
                    }
                    if *stopped || shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    drop(stopped);
                    let result = engine.retention_pass(schedule.policy, &mut clock, &shutdown);
                    let mut status = worker_status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if result.state != status.state
                        && matches!(
                            result.state,
                            ReceiptRetentionState::Failed
                                | ReceiptRetentionState::Maintenance
                                | ReceiptRetentionState::ClockError
                        )
                    {
                        eprintln!("unionid receipt retention: {:?}", result.state);
                    }
                    *status = result;
                }
            })
            .map_err(|_| {
                self.inner.retention_running.store(false, Ordering::Release);
                Error::new("E_CONFIG", "unable to start receipt retention worker")
            })?;
        Ok(ReceiptRetentionWorker {
            wake,
            status,
            thread: Some(thread),
        })
    }

    fn retention_pass(
        &self,
        policy: ReceiptRetentionPolicy,
        clock: &mut ClockGuard,
        shutdown: &AtomicBool,
    ) -> ReceiptRetentionStatus {
        let mut engine = match self.inner.engine.try_lock() {
            Ok(engine) => engine,
            Err(TryLockError::WouldBlock) => {
                return ReceiptRetentionStatus::new(ReceiptRetentionState::Busy);
            }
            Err(TryLockError::Poisoned(_)) => {
                return ReceiptRetentionStatus::new(ReceiptRetentionState::Failed);
            }
        };
        if shutdown.load(Ordering::Acquire) {
            return ReceiptRetentionStatus::new(ReceiptRetentionState::Idle);
        }
        let Some(now) = clock.observe(crate::engine::unix_time_ms()) else {
            return ReceiptRetentionStatus::new(ReceiptRetentionState::ClockError);
        };
        self.inner.active_writes.fetch_add(1, Ordering::AcqRel);
        struct Active<'a>(&'a AtomicUsize);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _active = Active(&self.inner.active_writes);
        self.inner.write_admissions.fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let result = engine.apply_idempotency_retention_at(policy, now);
        self.inner.metrics.record(
            MetricOperation::Maintenance,
            started.elapsed(),
            result.as_ref().err().map(|_| "E_MAINTENANCE"),
        );
        if engine.idempotency_receipt_count() != self.inner.metrics.receipt_count() {
            self.inner
                .metrics
                .update_receipts(engine.idempotency_status().ok().as_ref());
        }
        let mut status = ReceiptRetentionStatus::new(match &result {
            Ok(result) if result.pruning.selected_count > 0 => ReceiptRetentionState::Applied,
            Ok(_) => ReceiptRetentionState::Idle,
            Err(error) if error.code == "E_MAINTENANCE_REQUIRED" => {
                ReceiptRetentionState::Maintenance
            }
            Err(_) => ReceiptRetentionState::Failed,
        });
        status.as_of_unix_ms = Some(now);
        if let Ok(result) = result {
            status.deleted_count = result.pruning.selected_count;
            status.deleted_encoded_bytes = result.pruning.selected_encoded_bytes;
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(interval_seconds: u64) -> ReceiptRetentionSchedule {
        ReceiptRetentionSchedule {
            policy: ReceiptRetentionPolicy {
                min_age_seconds: 1,
                max_receipts: 1,
            },
            interval_seconds,
        }
    }

    #[test]
    fn clock_guard_protects_regressions_until_high_water_is_reached() {
        let mut clock = ClockGuard::default();
        assert_eq!(clock.observe(Ok(1000)), Some(1000));
        assert_eq!(clock.observe(Ok(999)), None);
        assert_eq!(clock.observe(Err(Error::new("E_TIME", "invalid"))), None);
        assert_eq!(clock.observe(Ok(900)), None);
        assert_eq!(clock.observe(Ok(1000)), Some(1000));
        assert_eq!(clock.observe(Ok(1001)), Some(1001));
    }

    #[test]
    fn retention_busy_skips_without_queueing_and_shutdown_skips_execution() {
        let engine = ConcurrentEngine::new(Engine::memory());
        let shutdown = AtomicBool::new(false);
        let mut clock = ClockGuard::default();
        let before = engine.stats();
        let lock = engine.inner.engine.lock().unwrap();
        assert_eq!(
            engine
                .retention_pass(schedule(1).policy, &mut clock, &shutdown)
                .state,
            ReceiptRetentionState::Busy
        );
        assert_eq!(engine.stats().queued_writes, 0);
        assert_eq!(engine.stats().write_admissions, before.write_admissions);
        drop(lock);
        shutdown.store(true, Ordering::Release);
        assert_eq!(
            engine
                .retention_pass(schedule(1).policy, &mut clock, &shutdown)
                .state,
            ReceiptRetentionState::Idle
        );
        assert_eq!(engine.stats().write_admissions, before.write_admissions);
        shutdown.store(false, Ordering::Release);
        clock.high_water = Some(u64::MAX);
        assert_eq!(
            engine
                .retention_pass(schedule(1).policy, &mut clock, &shutdown)
                .state,
            ReceiptRetentionState::ClockError
        );
        assert_eq!(engine.stats().write_admissions, before.write_admissions);
    }

    #[test]
    fn worker_is_unique_and_stop_wakes_long_interval_without_waiting() {
        let engine = ConcurrentEngine::new(Engine::memory());
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = engine
            .start_receipt_retention(schedule(86400), shutdown.clone())
            .unwrap();
        assert_eq!(worker.status().state, ReceiptRetentionState::Idle);
        assert!(
            engine
                .start_receipt_retention(schedule(1), shutdown.clone())
                .is_err()
        );
        let started = Instant::now();
        worker.stop().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!engine.inner.retention_running.load(Ordering::Acquire));
        let worker = engine
            .start_receipt_retention(schedule(86400), shutdown)
            .unwrap();
        drop(worker);
        assert!(!engine.inner.retention_running.load(Ordering::Acquire));
        assert_eq!(engine.stats().active_writes, 0);
    }

    #[test]
    fn configuration_validation_precedes_starting_worker() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let engine = ConcurrentEngine::new(Engine::memory());
        for interval in [0, 86401] {
            assert!(
                engine
                    .start_receipt_retention(schedule(interval), shutdown.clone())
                    .is_err()
            );
            assert!(!engine.inner.retention_running.load(Ordering::Acquire));
        }
        let engine = ConcurrentEngine::new(Engine::memory().with_read_only(true));
        assert!(
            engine
                .start_receipt_retention(schedule(1), shutdown)
                .is_err()
        );
        assert!(!engine.inner.retention_running.load(Ordering::Acquire));
    }

    #[test]
    fn worker_prunes_in_bounded_passes_and_retained_key_still_replays() {
        let mut engine = Engine::memory();
        assert!(engine.execute("struct Counter {id: int, value: int}\ntable counters Counter\n  key id\ninsert counters {id: 1, value: 0}").ok);
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let query = "update counters | set value = value + 1 | returning {value}";
        for key in ["first", "second"] {
            engine
                .execute_idempotent_with_params(key, digest, query, BTreeMap::new(), None)
                .unwrap();
        }
        let engine = ConcurrentEngine::new(engine);
        let worker = engine
            .start_receipt_retention(schedule(1), Arc::new(AtomicBool::new(false)))
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while worker.status().state != ReceiptRetentionState::Applied {
            assert!(Instant::now() < until, "worker never applied retention");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(worker.status().deleted_count, 1);
        worker.stop().unwrap();
        engine.with_exclusive(|engine| {
            assert_eq!(engine.idempotency_status().unwrap().count, 1);
            assert!(
                engine
                    .execute_idempotent_with_params("second", digest, query, BTreeMap::new(), None)
                    .unwrap()
                    .replayed
            );
            assert!(
                !engine
                    .execute_idempotent_with_params("first", digest, query, BTreeMap::new(), None)
                    .unwrap()
                    .replayed
            );
        });
    }
}
