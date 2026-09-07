use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::db::QueryResponse;
use crate::error::{Error, Result};

pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_IDEMPOTENCY_RECEIPT_BYTES: usize = 1024 * 1024;
pub const MAX_IDEMPOTENCY_RECEIPTS: usize = 10_000;
pub const MAX_IDEMPOTENCY_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const DURABLE_CODEC_OVERHEAD: usize = 6;

pub(crate) type ReceiptMap = BTreeMap<String, IdempotencyReceipt>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyReceipt {
    pub digest: String,
    pub committed_sequence: u64,
    pub completed_at_unix_ms: u64,
    pub response: QueryResponse,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyDurability {
    ProcessLocal,
    Durable,
}

#[derive(Debug, Clone)]
pub struct IdempotentExecution {
    pub response: QueryResponse,
    pub replayed: bool,
    pub digest: String,
    pub committed_sequence: u64,
    pub durability: IdempotencyDurability,
}

pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(Error::new(
            "E_IDEMPOTENCY_KEY",
            format!("idempotency key must contain 1 to {MAX_IDEMPOTENCY_KEY_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(())
}

pub(crate) fn validate_digest(digest: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(invalid_digest());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_digest());
    }
    Ok(())
}

pub(crate) fn encoded_receipt(receipt: &IdempotencyReceipt) -> Result<Vec<u8>> {
    serde_json::to_vec(receipt)
        .map_err(|error| Error::new("E_STORAGE", format!("encode idempotency receipt: {error}")))
}

pub(crate) fn validate_receipts(receipts: &ReceiptMap, sequence: u64) -> Result<usize> {
    if receipts.len() > MAX_IDEMPOTENCY_RECEIPTS {
        return Err(Error::new(
            "E_STORAGE",
            "idempotency receipt count exceeds the supported limit",
        ));
    }
    let mut total = 0usize;
    for (key, receipt) in receipts {
        validate_key(key).map_err(as_storage_error)?;
        validate_digest(&receipt.digest).map_err(as_storage_error)?;
        if !receipt.response.ok || receipt.response.error.is_some() {
            return Err(Error::new(
                "E_STORAGE",
                "idempotency receipt contains a failed response",
            ));
        }
        if receipt.committed_sequence > sequence {
            return Err(Error::new(
                "E_STORAGE",
                "idempotency receipt sequence exceeds the database commit sequence",
            ));
        }
        let encoded = encoded_receipt(receipt)?;
        if encoded.len().saturating_add(DURABLE_CODEC_OVERHEAD) > MAX_IDEMPOTENCY_RECEIPT_BYTES {
            return Err(Error::new(
                "E_STORAGE",
                "idempotency receipt exceeds the supported encoded size",
            ));
        }
        total = total
            .checked_add(encoded.len().saturating_add(DURABLE_CODEC_OVERHEAD))
            .ok_or_else(|| Error::new("E_STORAGE", "idempotency receipt size overflow"))?;
    }
    if total > MAX_IDEMPOTENCY_TOTAL_BYTES {
        return Err(Error::new(
            "E_STORAGE",
            "idempotency receipt total exceeds the supported limit",
        ));
    }
    Ok(total)
}

pub(crate) fn validate_new_receipt(
    receipts: &ReceiptMap,
    receipt: &IdempotencyReceipt,
) -> Result<()> {
    let encoded = encoded_receipt(receipt)?;
    let encoded_len = encoded.len().saturating_add(DURABLE_CODEC_OVERHEAD);
    if encoded_len > MAX_IDEMPOTENCY_RECEIPT_BYTES {
        return Err(Error::new(
            "E_IDEMPOTENCY_LIMIT",
            format!(
                "encoded idempotency receipt is {} bytes; limit is {MAX_IDEMPOTENCY_RECEIPT_BYTES}",
                encoded_len
            ),
        ));
    }
    let total = receipts.values().try_fold(0usize, |total, stored| {
        total
            .checked_add(
                encoded_receipt(stored)?
                    .len()
                    .saturating_add(DURABLE_CODEC_OVERHEAD),
            )
            .ok_or_else(|| Error::new("E_IDEMPOTENCY_CAPACITY", "receipt size overflow"))
    })?;
    if receipts.len() >= MAX_IDEMPOTENCY_RECEIPTS
        || total.saturating_add(encoded_len) > MAX_IDEMPOTENCY_TOTAL_BYTES
    {
        return Err(Error::new(
            "E_IDEMPOTENCY_CAPACITY",
            format!(
                "idempotency receipt capacity exhausted (count {}/{MAX_IDEMPOTENCY_RECEIPTS}, bytes {total}/{MAX_IDEMPOTENCY_TOTAL_BYTES})",
                receipts.len()
            ),
        ));
    }
    Ok(())
}

fn invalid_digest() -> Error {
    Error::new(
        "E_IDEMPOTENCY_DIGEST",
        "idempotency digest must be sha256 followed by 64 lowercase hexadecimal digits",
    )
}

fn as_storage_error(error: Error) -> Error {
    Error::new("E_STORAGE", error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn receipt(message: String) -> IdempotencyReceipt {
        IdempotencyReceipt {
            digest: DIGEST.into(),
            committed_sequence: 1,
            completed_at_unix_ms: 1,
            response: QueryResponse::ok_message(message),
        }
    }

    #[test]
    fn validates_key_and_digest_boundaries() {
        assert!(validate_key("x").is_ok());
        assert!(validate_key(&"x".repeat(MAX_IDEMPOTENCY_KEY_BYTES)).is_ok());
        assert_eq!(validate_key("").unwrap_err().code, "E_IDEMPOTENCY_KEY");
        assert_eq!(
            validate_key(&"x".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1))
                .unwrap_err()
                .code,
            "E_IDEMPOTENCY_KEY"
        );
        assert!(validate_digest(DIGEST).is_ok());
        assert!(validate_digest(&DIGEST.to_uppercase()).is_err());
    }

    #[test]
    fn rejects_an_oversized_receipt_before_commit() {
        let oversized = receipt("x".repeat(MAX_IDEMPOTENCY_RECEIPT_BYTES));
        let error = validate_new_receipt(&ReceiptMap::new(), &oversized).unwrap_err();
        assert_eq!(error.code, "E_IDEMPOTENCY_LIMIT");
    }

    #[test]
    fn rejects_new_receipts_at_count_capacity_but_validates_existing_state() {
        let stored = receipt("ok".into());
        let receipts = (0..MAX_IDEMPOTENCY_RECEIPTS)
            .map(|index| (format!("key-{index}"), stored.clone()))
            .collect::<ReceiptMap>();
        let error = validate_new_receipt(&receipts, &stored).unwrap_err();
        assert_eq!(error.code, "E_IDEMPOTENCY_CAPACITY");
        assert!(validate_receipts(&receipts, 1).is_ok());
    }
}
