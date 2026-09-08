//! Versioned, authenticated cursor values for bounded keyset pages.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::db::SchemaInfo;
use crate::error::{Error, Result};
use crate::protocol::WireValue;
use crate::query::PageDirection;

pub const MAX_PAGE_LIMIT: usize = 1_000;
pub const MAX_CURSOR_BYTES: usize = 8_192;
pub const MAX_CURSOR_PAYLOAD_BYTES: usize = 6_144;
pub const MAX_CURSOR_SORT_KEYS: usize = 16;

const LEGACY_CURSOR_PREFIX: &str = "u1";
const PRODUCTION_CURSOR_PREFIX: &str = "u2";
const LEGACY_CURSOR_CODEC_VERSION: u32 = 1;
const PRODUCTION_CURSOR_CODEC_VERSION: u32 = 2;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CursorIdentity {
    instance_id: [u8; 16],
    secret: [u8; 32],
}

impl std::fmt::Debug for CursorIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CursorIdentity")
            .field("instance_id", &URL_SAFE_NO_PAD.encode(self.instance_id))
            .field("secret", &"[redacted]")
            .finish()
    }
}

impl CursorIdentity {
    pub(crate) fn generate() -> Result<Self> {
        let mut instance_id = [0_u8; 16];
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut instance_id).map_err(|error| {
            Error::new("E_STORAGE", format!("generate database identity: {error}"))
        })?;
        getrandom::fill(&mut secret)
            .map_err(|error| Error::new("E_STORAGE", format!("generate cursor secret: {error}")))?;
        Ok(Self {
            instance_id,
            secret,
        })
    }

    pub(crate) fn from_bytes(instance_id: [u8; 16], secret: [u8; 32]) -> Self {
        Self {
            instance_id,
            secret,
        }
    }

    pub(crate) fn instance_id(&self) -> &[u8; 16] {
        &self.instance_id
    }

    pub(crate) fn secret(&self) -> &[u8; 32] {
        &self.secret
    }
}

impl Default for CursorIdentity {
    fn default() -> Self {
        Self::generate().expect("operating system random source is required for cursor identity")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorKey {
    pub field_path: Vec<u64>,
    pub column: String,
    pub descending: bool,
    pub value: WireValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorPayload {
    codec: u32,
    database: String,
    schema_revision: String,
    schema_hash: String,
    snapshot_sequence: String,
    plan_digest: String,
    direction: PageDirection,
    limit: usize,
    keys: Vec<CursorKey>,
}

#[derive(Debug, Clone)]
pub(crate) struct DecodedCursor {
    pub keys: Vec<CursorKey>,
}

pub(crate) struct CursorExpectation<'a> {
    pub schema: &'a SchemaInfo,
    pub snapshot_sequence: u64,
    pub plan_digest: &'a str,
    pub direction: PageDirection,
    pub limit: usize,
}

pub(crate) fn encode(
    identity: &CursorIdentity,
    schema: &SchemaInfo,
    snapshot_sequence: u64,
    plan_digest: &str,
    direction: PageDirection,
    limit: usize,
    keys: Vec<CursorKey>,
) -> Result<String> {
    if keys.len() > MAX_CURSOR_SORT_KEYS {
        return Err(Error::new(
            "E_CURSOR_LIMIT",
            format!("cursor sort key count exceeds {MAX_CURSOR_SORT_KEYS}"),
        ));
    }
    let production = keys.iter().any(|key| key.value.requires_v2());
    let (prefix, codec) = if production {
        (PRODUCTION_CURSOR_PREFIX, PRODUCTION_CURSOR_CODEC_VERSION)
    } else {
        (LEGACY_CURSOR_PREFIX, LEGACY_CURSOR_CODEC_VERSION)
    };
    let payload = CursorPayload {
        codec,
        database: URL_SAFE_NO_PAD.encode(identity.instance_id),
        schema_revision: schema.revision.to_string(),
        schema_hash: schema.hash.clone(),
        snapshot_sequence: snapshot_sequence.to_string(),
        plan_digest: plan_digest.to_owned(),
        direction,
        limit,
        keys,
    };
    let payload = serde_json::to_vec(&payload)
        .map_err(|error| Error::new("E_CURSOR_CODEC", format!("encode cursor: {error}")))?;
    if payload.len() > MAX_CURSOR_PAYLOAD_BYTES {
        return Err(Error::new(
            "E_CURSOR_LIMIT",
            format!("cursor payload exceeds {MAX_CURSOR_PAYLOAD_BYTES} bytes"),
        ));
    }
    let signature = signature(&identity.secret, &payload)?;
    let token = format!(
        "{prefix}.{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(signature)
    );
    if token.len() > MAX_CURSOR_BYTES {
        return Err(Error::new(
            "E_CURSOR_LIMIT",
            format!("cursor exceeds {MAX_CURSOR_BYTES} bytes"),
        ));
    }
    Ok(token)
}

pub(crate) fn decode(
    identity: &CursorIdentity,
    token: &str,
    expected: CursorExpectation<'_>,
) -> Result<DecodedCursor> {
    if token.len() > MAX_CURSOR_BYTES {
        return Err(Error::new(
            "E_CURSOR_LIMIT",
            format!("cursor exceeds {MAX_CURSOR_BYTES} bytes"),
        ));
    }
    let mut parts = token.split('.');
    let prefix = parts.next();
    let payload_text = parts.next();
    let signature_text = parts.next();
    let expected_codec = match prefix {
        Some(LEGACY_CURSOR_PREFIX) => LEGACY_CURSOR_CODEC_VERSION,
        Some(PRODUCTION_CURSOR_PREFIX) => PRODUCTION_CURSOR_CODEC_VERSION,
        _ => {
            return Err(Error::new("E_CURSOR_CODEC", "unsupported cursor prefix"));
        }
    };
    if payload_text.is_none() || signature_text.is_none() || parts.next().is_some() {
        return Err(Error::new("E_CURSOR_CODEC", "invalid cursor syntax"));
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload_text.unwrap())
        .map_err(|_| Error::new("E_CURSOR_CODEC", "cursor payload is not valid base64url"))?;
    if payload.len() > MAX_CURSOR_PAYLOAD_BYTES {
        return Err(Error::new(
            "E_CURSOR_LIMIT",
            format!("cursor payload exceeds {MAX_CURSOR_PAYLOAD_BYTES} bytes"),
        ));
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature_text.unwrap())
        .map_err(|_| Error::new("E_CURSOR_CODEC", "cursor signature is not valid base64url"))?;
    verify_signature(&identity.secret, &payload, &signature)?;

    let payload: CursorPayload = serde_json::from_slice(&payload)
        .map_err(|_| Error::new("E_CURSOR_CODEC", "cursor payload is invalid"))?;
    if payload.codec != expected_codec {
        return Err(Error::new(
            "E_CURSOR_CODEC",
            format!(
                "cursor prefix and codec version {} do not match",
                payload.codec
            ),
        ));
    }
    if payload.keys.len() > MAX_CURSOR_SORT_KEYS {
        return Err(Error::new(
            "E_CURSOR_LIMIT",
            format!("cursor sort key count exceeds {MAX_CURSOR_SORT_KEYS}"),
        ));
    }
    let production = payload.keys.iter().any(|key| key.value.requires_v2());
    if production != (expected_codec == PRODUCTION_CURSOR_CODEC_VERSION) {
        return Err(Error::new(
            "E_CURSOR_CODEC",
            "cursor prefix does not match its typed boundary vocabulary",
        ));
    }
    let database = URL_SAFE_NO_PAD
        .decode(&payload.database)
        .map_err(|_| Error::new("E_CURSOR_CODEC", "cursor database identity is invalid"))?;
    if database.as_slice() != identity.instance_id {
        return Err(Error::new(
            "E_CURSOR_DATABASE",
            "cursor belongs to another database instance",
        ));
    }
    let schema_revision = parse_u64(&payload.schema_revision, "schema revision")?;
    let cursor_schema = SchemaInfo {
        revision: schema_revision,
        hash: payload.schema_hash,
    };
    if &cursor_schema != expected.schema {
        return Err(Error::new(
            "E_CURSOR_SCHEMA",
            "cursor schema identity no longer matches the database",
        ));
    }
    if payload.plan_digest != expected.plan_digest
        || payload.direction != expected.direction
        || payload.limit != expected.limit
    {
        return Err(Error::new(
            "E_CURSOR_QUERY",
            "cursor does not match the bound query, parameters, direction, or page limit",
        ));
    }
    let snapshot_sequence = parse_u64(&payload.snapshot_sequence, "snapshot sequence")?;
    if snapshot_sequence != expected.snapshot_sequence {
        return Err(Error::new(
            "E_CURSOR_STALE",
            "database commit sequence changed after the cursor was created",
        ));
    }
    Ok(DecodedCursor { keys: payload.keys })
}

fn signature(secret: &[u8; 32], payload: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| Error::new("E_CURSOR_CODEC", "invalid cursor secret"))?;
    mac.update(payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn verify_signature(secret: &[u8; 32], payload: &[u8], signature: &[u8]) -> Result<()> {
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| Error::new("E_CURSOR_CODEC", "invalid cursor secret"))?;
    mac.update(payload);
    mac.verify_slice(signature)
        .map_err(|_| Error::new("E_CURSOR_INTEGRITY", "cursor integrity verification failed"))
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse()
        .map_err(|_| Error::new("E_CURSOR_CODEC", format!("cursor {name} is invalid")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> CursorIdentity {
        CursorIdentity::from_bytes([7; 16], [9; 32])
    }

    fn schema() -> SchemaInfo {
        SchemaInfo {
            revision: 3,
            hash: "sha256:test".into(),
        }
    }

    #[test]
    fn cursor_round_trip_and_tamper_are_bounded() {
        let token = encode(
            &identity(),
            &schema(),
            8,
            "sha256:plan",
            PageDirection::Forward,
            20,
            vec![CursorKey {
                field_path: vec![1],
                column: "id".into(),
                descending: false,
                value: WireValue::Int { value: "42".into() },
            }],
        )
        .unwrap();
        let decoded = decode(
            &identity(),
            &token,
            CursorExpectation {
                schema: &schema(),
                snapshot_sequence: 8,
                plan_digest: "sha256:plan",
                direction: PageDirection::Forward,
                limit: 20,
            },
        )
        .unwrap();
        assert_eq!(decoded.keys.len(), 1);

        let mut tampered = token.into_bytes();
        let position = tampered.iter().position(|byte| *byte != b'.').unwrap() + 3;
        tampered[position] = if tampered[position] == b'A' {
            b'B'
        } else {
            b'A'
        };
        let error = decode(
            &identity(),
            std::str::from_utf8(&tampered).unwrap(),
            CursorExpectation {
                schema: &schema(),
                snapshot_sequence: 8,
                plan_digest: "sha256:plan",
                direction: PageDirection::Forward,
                limit: 20,
            },
        )
        .unwrap_err();
        assert!(matches!(
            error.code.as_str(),
            "E_CURSOR_CODEC" | "E_CURSOR_INTEGRITY"
        ));
    }

    #[test]
    fn cursor_prefix_tracks_the_typed_boundary_vocabulary() {
        let legacy = encode(
            &identity(),
            &schema(),
            8,
            "sha256:legacy",
            PageDirection::Forward,
            20,
            vec![CursorKey {
                field_path: vec![1],
                column: "id".into(),
                descending: false,
                value: WireValue::Int { value: "42".into() },
            }],
        )
        .unwrap();
        assert!(legacy.starts_with("u1."));

        let production = encode(
            &identity(),
            &schema(),
            8,
            "sha256:production",
            PageDirection::Forward,
            20,
            vec![CursorKey {
                field_path: vec![2],
                column: "uuid".into(),
                descending: false,
                value: WireValue::Uuid {
                    value: "00000000-0000-0000-0000-000000000001".into(),
                },
            }],
        )
        .unwrap();
        assert!(production.starts_with("u2."));
        assert_eq!(
            decode(
                &identity(),
                &production,
                CursorExpectation {
                    schema: &schema(),
                    snapshot_sequence: 8,
                    plan_digest: "sha256:production",
                    direction: PageDirection::Forward,
                    limit: 20,
                },
            )
            .unwrap()
            .keys
            .len(),
            1
        );

        let wrong_prefix = production.replacen("u2.", "u1.", 1);
        assert_eq!(
            decode(
                &identity(),
                &wrong_prefix,
                CursorExpectation {
                    schema: &schema(),
                    snapshot_sequence: 8,
                    plan_digest: "sha256:production",
                    direction: PageDirection::Forward,
                    limit: 20,
                },
            )
            .unwrap_err()
            .code,
            "E_CURSOR_CODEC"
        );
    }
}
