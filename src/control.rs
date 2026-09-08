use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::{Error, Result};

#[derive(Clone)]
pub(crate) struct ExecutionControl {
    deadline: Option<Instant>,
    cancelled: Option<Arc<AtomicBool>>,
    shutdown: Option<Arc<AtomicBool>>,
}

impl ExecutionControl {
    pub(crate) fn deadline(deadline: Instant) -> Self {
        Self {
            deadline: Some(deadline),
            cancelled: None,
            shutdown: None,
        }
    }

    pub(crate) fn cancellable(
        deadline: Instant,
        cancelled: Arc<AtomicBool>,
        shutdown: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            deadline: Some(deadline),
            cancelled: Some(cancelled),
            shutdown,
        }
    }

    pub(crate) fn checkpoint(&self) -> Result<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::Acquire))
        {
            return Err(Error::new("E_CANCELLED", "read operation cancelled"));
        }
        if self
            .shutdown
            .as_ref()
            .is_some_and(|signal| signal.load(Ordering::Acquire))
        {
            return Err(Error::new("E_SHUTDOWN", "server is shutting down"));
        }
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::new(
                "E_TIMEOUT",
                "request execution deadline exceeded",
            ));
        }
        Ok(())
    }

    pub(crate) fn remaining(&self) -> Option<std::time::Duration> {
        self.deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
    }
}
