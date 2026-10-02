use serde::{Deserialize, Serialize};

use super::{IdempotencyPruneOptions, IdempotencyPruneResult, MAX_IDEMPOTENCY_PRUNE_RECEIPTS};
use crate::{Error, Result};

/// Explicit runtime policy, never implicitly enabled or stored in a backup.
/// Age is measured from the receipt's UTC completion timestamp. A removed key
/// can execute again; the window must exceed all supported retry windows.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptRetentionPolicy {
    pub min_age_seconds: u64,
    pub max_receipts: usize,
}

impl ReceiptRetentionPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.min_age_seconds == 0 || self.min_age_seconds.checked_mul(1_000).is_none() {
            return Err(Error::new(
                "E_CONFIG",
                "receipt retention requires a positive window representable in milliseconds",
            ));
        }
        if self.max_receipts == 0 || self.max_receipts > MAX_IDEMPOTENCY_PRUNE_RECEIPTS {
            return Err(Error::new(
                "E_CONFIG",
                format!(
                    "receipt retention max_receipts must be between 1 and {MAX_IDEMPOTENCY_PRUNE_RECEIPTS}"
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn selection_at(
        &self,
        now_unix_ms: u64,
    ) -> Result<(Option<u64>, IdempotencyPruneOptions)> {
        self.validate()?;
        let cutoff = now_unix_ms.checked_sub(self.min_age_seconds * 1_000);
        Ok((
            cutoff,
            IdempotencyPruneOptions {
                // Explicit zero selects nothing when the window extends before the
                // epoch. None would remove the time restriction in the prune API.
                completed_before_unix_ms: Some(cutoff.unwrap_or(0)),
                committed_through_sequence: None,
                max_receipts: self.max_receipts,
            },
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptRetentionResult {
    pub schema_version: u32,
    pub policy: ReceiptRetentionPolicy,
    pub as_of_unix_ms: u64,
    pub cutoff_unix_ms: Option<u64>,
    #[serde(flatten)]
    pub pruning: IdempotencyPruneResult,
    pub key_reuse_warning: String,
}

impl ReceiptRetentionResult {
    pub(crate) fn new(
        policy: ReceiptRetentionPolicy,
        as_of_unix_ms: u64,
        cutoff_unix_ms: Option<u64>,
        pruning: IdempotencyPruneResult,
    ) -> Self {
        Self {
            schema_version: 1,
            policy,
            as_of_unix_ms,
            cutoff_unix_ms,
            pruning,
            key_reuse_warning: "A pruned key can execute again; retain receipts longer than every supported retry window.".into(),
        }
    }
}
