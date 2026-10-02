use serde::{Deserialize, Serialize};

pub(crate) const CAPACITY_WARNING: &str = "Idempotency receipt capacity was at least 80% used at this commit; inspect receipts status and preview explicit pruning before confirming it.";
pub(crate) const CAPACITY_HINT: &str = "run `unionid receipts status --db <path> --format json`; preview `unionid receipts prune --db <path> --before-unix-ms <retry-safe-cutoff> --max-receipts 1000 --format json`, then add --confirm only after checking your retry window; a pruned key can execute again";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptCapacityState {
    Normal,
    Warning,
    Full,
}

/// Value-free capacity snapshot. Nonzero remaining bytes do not guarantee that
/// the next individual receipt fits. Metrics snapshots remain weakly consistent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptCapacity {
    pub version: u32,
    pub state: ReceiptCapacityState,
    pub warning_percent: u8,
    pub count: usize,
    pub encoded_bytes: usize,
    pub max_count: usize,
    pub max_encoded_bytes: usize,
    pub remaining_count: usize,
    pub remaining_encoded_bytes: usize,
}

impl ReceiptCapacity {
    pub fn from_usage(
        count: usize,
        encoded_bytes: usize,
        max_count: usize,
        max_encoded_bytes: usize,
    ) -> Self {
        let state = if count >= max_count || encoded_bytes >= max_encoded_bytes {
            ReceiptCapacityState::Full
        } else if (count as u128) * 100 >= (max_count as u128) * 80
            || (encoded_bytes as u128) * 100 >= (max_encoded_bytes as u128) * 80
        {
            ReceiptCapacityState::Warning
        } else {
            ReceiptCapacityState::Normal
        };
        Self {
            version: 1,
            state,
            warning_percent: 80,
            count,
            encoded_bytes,
            max_count,
            max_encoded_bytes,
            remaining_count: max_count.saturating_sub(count),
            remaining_encoded_bytes: max_encoded_bytes.saturating_sub(encoded_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn either_budget_warns_at_exact_integer_threshold_and_full_saturates() {
        assert_eq!(
            ReceiptCapacity::from_usage(79, 79, 100, 100).state,
            ReceiptCapacityState::Normal
        );
        assert_eq!(
            ReceiptCapacity::from_usage(80, 0, 100, 100).state,
            ReceiptCapacityState::Warning
        );
        assert_eq!(
            ReceiptCapacity::from_usage(0, 80, 100, 100).state,
            ReceiptCapacityState::Warning
        );
        assert_eq!(
            ReceiptCapacity::from_usage(80, 0, 101, 100).state,
            ReceiptCapacityState::Normal
        );
        assert_eq!(
            ReceiptCapacity::from_usage(81, 0, 101, 100).state,
            ReceiptCapacityState::Warning
        );
        let full = ReceiptCapacity::from_usage(101, 150, 100, 100);
        assert_eq!(full.state, ReceiptCapacityState::Full);
        assert_eq!((full.remaining_count, full.remaining_encoded_bytes), (0, 0));
        assert_eq!(
            ReceiptCapacity::from_usage(usize::MAX - 1, 0, usize::MAX, usize::MAX).state,
            ReceiptCapacityState::Warning
        );
    }
}
