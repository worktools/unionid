//! Strict framing for RFC 0028. Public upgrades remain unavailable until the
//! journal transition and backup contracts can preserve this header atomically.
use serde::{Deserialize, Serialize};

use super::{
    JOURNAL_CODEC_VERSION, MAINTENANCE_CODEC_VERSION, MAP_CATALOG_CODEC_VERSION,
    MAP_INDEX_KEY_VERSION, MAP_RECEIPT_CODEC_VERSION, MAP_VALUE_CODEC_VERSION,
    MIGRATION_CODEC_VERSION, PARTIAL_CATALOG_CODEC_VERSION, PRODUCTION_CATALOG_CODEC_VERSION,
    PRODUCTION_INDEX_KEY_VERSION, PRODUCTION_RECEIPT_CODEC_VERSION,
    REFERENCE_CATALOG_CODEC_VERSION,
};
use crate::error::{Error, Result};

const MAGIC: &[u8; 4] = b"UISH";
const VERSION: u16 = 1;
const PREFIX_BYTES: usize = 6;
// A structural bound, derived from the longest supported capability set and
// the largest representable integer fields, rather than a new resource budget.
const MAX_JSON: &str = concat!(
    "{\"physical_format\":4294967295,\"required_capabilities\":[",
    "\"partial_unique_index\",\"typed_map\",\"typed_references\"],",
    "\"codecs\":{\"catalog\":65535,\"value\":65535,\"index_key\":65535,",
    "\"migration\":65535,\"receipt\":65535,\"maintenance\":65535,\"journal\":65535}}"
);
pub(super) const MAX_HEADER_BYTES: usize = PREFIX_BYTES + MAX_JSON.len();

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct StorageHeader {
    physical_format: u32,
    required_capabilities: Vec<String>,
    codecs: Codecs,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Codecs {
    catalog: u16,
    value: u16,
    index_key: u16,
    migration: u16,
    receipt: u16,
    maintenance: u16,
    journal: u16,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new("E_STORAGE", format!("storage_header: {}", message.into()))
}

impl StorageHeader {
    pub(super) fn decode(bytes: &[u8], physical_format: u32) -> Result<Self> {
        if bytes.len() > MAX_HEADER_BYTES {
            return Err(invalid("exceeds the supported header shape"));
        }
        if bytes.len() < PREFIX_BYTES || &bytes[..4] != MAGIC {
            return Err(invalid("invalid framing"));
        }
        if bytes[4..6] != VERSION.to_be_bytes() {
            return Err(invalid("unsupported header codec"));
        }
        let header: Self = serde_json::from_slice(&bytes[PREFIX_BYTES..])
            .map_err(|_| invalid("invalid fields or JSON"))?;
        if header.physical_format != physical_format {
            return Err(invalid("physical format contradicts discriminator"));
        }
        let canonical =
            serde_json::to_vec(&header).map_err(|_| invalid("cannot encode canonical fields"))?;
        if canonical != bytes[PREFIX_BYTES..] {
            return Err(invalid("noncanonical encoding"));
        }
        header.validate_profile()?;
        Ok(header)
    }

    fn validate_profile(&self) -> Result<()> {
        let mut map = false;
        let mut partial = false;
        let mut references = false;
        for capability in &self.required_capabilities {
            match capability.as_str() {
                "typed_map" => map = true,
                "partial_unique_index" => partial = true,
                "typed_references" => references = true,
                _ => return Err(invalid("unsupported required capability")),
            }
        }
        if self
            .required_capabilities
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid("capabilities must be sorted and unique"));
        }
        if (references && !partial) || (partial && !map) {
            return Err(invalid("missing capability dependency"));
        }
        let expected = if references {
            (
                REFERENCE_CATALOG_CODEC_VERSION,
                MAP_VALUE_CODEC_VERSION,
                MAP_INDEX_KEY_VERSION,
                MAP_RECEIPT_CODEC_VERSION,
            )
        } else if partial {
            (
                PARTIAL_CATALOG_CODEC_VERSION,
                MAP_VALUE_CODEC_VERSION,
                MAP_INDEX_KEY_VERSION,
                MAP_RECEIPT_CODEC_VERSION,
            )
        } else if map {
            (
                MAP_CATALOG_CODEC_VERSION,
                MAP_VALUE_CODEC_VERSION,
                MAP_INDEX_KEY_VERSION,
                MAP_RECEIPT_CODEC_VERSION,
            )
        } else {
            (
                PRODUCTION_CATALOG_CODEC_VERSION,
                MAP_VALUE_CODEC_VERSION,
                PRODUCTION_INDEX_KEY_VERSION,
                PRODUCTION_RECEIPT_CODEC_VERSION,
            )
        };
        let codecs = &self.codecs;
        if (
            codecs.catalog,
            codecs.value,
            codecs.index_key,
            codecs.receipt,
        ) != expected
            || codecs.migration != MIGRATION_CODEC_VERSION
            || codecs.maintenance != MAINTENANCE_CODEC_VERSION
            || !matches!(codecs.journal, 0 | JOURNAL_CODEC_VERSION)
        {
            return Err(invalid("unsupported capability/codec profile"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: &str) -> Vec<u8> {
        [MAGIC.as_slice(), &VERSION.to_be_bytes(), json.as_bytes()].concat()
    }

    const BASE: &str = concat!(
        "{\"physical_format\":4294967295,\"required_capabilities\":[],",
        "\"codecs\":{\"catalog\":4,\"value\":3,\"index_key\":3,",
        "\"migration\":1,\"receipt\":2,\"maintenance\":1,\"journal\":0}}"
    );

    #[test]
    fn supported_profiles_have_independent_journal_codecs() {
        for (capabilities, catalog, index, receipt) in [
            ("", 4, 3, 2),
            ("\"typed_map\"", 5, 4, 3),
            ("\"partial_unique_index\",\"typed_map\"", 6, 4, 3),
            (
                "\"partial_unique_index\",\"typed_map\",\"typed_references\"",
                7,
                4,
                3,
            ),
        ] {
            for journal in [0, 1] {
                let json = BASE
                    .replace("[]", &format!("[{capabilities}]"))
                    .replace("\"catalog\":4", &format!("\"catalog\":{catalog}"))
                    .replace("\"index_key\":3", &format!("\"index_key\":{index}"))
                    .replace("\"receipt\":2", &format!("\"receipt\":{receipt}"))
                    .replace("\"journal\":0", &format!("\"journal\":{journal}"));
                let bytes = frame(&json);
                assert!(bytes.len() <= MAX_HEADER_BYTES);
                assert!(StorageHeader::decode(&bytes, u32::MAX).is_ok(), "{json}");
            }
        }
    }

    #[test]
    fn ambiguous_and_unknown_requirements_fail_closed() {
        for json in [
            BASE.replace("[]", "[\"generated_defaults\"]"),
            BASE.replace("[]", "[\"future_feature\"]"),
            BASE.replace("[]", "[\"typed_map\",\"typed_map\"]"),
            BASE.replace("[]", "[\"typed_map\",\"partial_unique_index\"]"),
            BASE.replace("[]", "[\"partial_unique_index\"]"),
            BASE.replace("[]", "[\"typed_references\",\"typed_map\"]"),
            BASE.replace("\"catalog\":4", "\"catalog\":5"),
            BASE.replace("\"journal\":0", "\"journal\":2"),
            BASE.replace("\"value\":3", "\"value\":2"),
            BASE.replace("\"value\":3", "\"value\":3,\"value\":3"),
            BASE.replace("\"value\":3,", ""),
            BASE.replace("\"value\":3", "\"value\":3,\"extra\":0"),
            BASE.replace("\"codecs\":", "\"extra\":0,\"codecs\":"),
            BASE.replace("\"catalog\":4", "\"catalog\":65536"),
            BASE.replace("\"catalog\":4,\"value\":3", "\"value\":3,\"catalog\":4"),
            BASE.replace("\"catalog\":4", "\"catalog\": 4"),
            BASE.replace("\"catalog\":4", "\"catalog\":4.0"),
            format!("{BASE}\n"),
        ] {
            let error = StorageHeader::decode(&frame(&json), u32::MAX).unwrap_err();
            assert_eq!(error.code, "E_STORAGE", "{json}");
        }
        assert!(StorageHeader::decode(&frame(BASE), 10).is_err());
        assert!(StorageHeader::decode(&frame(MAX_JSON), u32::MAX).is_err());
        assert!(StorageHeader::decode(&vec![0; MAX_HEADER_BYTES + 1], 10).is_err());
        for bytes in [
            b"UISH".to_vec(),
            b"UISH\0\x02{}".to_vec(),
            b"OTHER!".to_vec(),
        ] {
            assert!(StorageHeader::decode(&bytes, 10).is_err());
        }
    }
}
