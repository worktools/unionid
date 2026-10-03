//! Compatibility registry for existing formats. Capabilities and journaling are
//! explicit here, but remain inferred from legacy format numbers on disk.
use super::*;

const SCALARS: u8 = 1;
const MAPS: u8 = 1 << 1;
const PARTIAL: u8 = 1 << 2;
const REFERENCES: u8 = 1 << 3;
const BOUNDED: u8 = 1 << 4;
const GENERATIONS: u8 = 1 << 5;
const CURSOR: u8 = 1 << 6;

// Catalog, value, index-key and receipt codecs; migration codec stays version 1.
const LEGACY_CODECS: [u16; 4] = [
    CATALOG_CODEC_VERSION,
    VALUE_CODEC_VERSION,
    INDEX_KEY_VERSION,
    RECEIPT_CODEC_VERSION,
];
const SCALAR_CODECS: [u16; 4] = [
    SCALAR_CATALOG_CODEC_VERSION,
    PRODUCTION_VALUE_CODEC_VERSION,
    SCALAR_INDEX_KEY_VERSION,
    PRODUCTION_RECEIPT_CODEC_VERSION,
];
const ORDERED_CODECS: [u16; 4] = [
    PRODUCTION_CATALOG_CODEC_VERSION,
    PRODUCTION_VALUE_CODEC_VERSION,
    PRODUCTION_INDEX_KEY_VERSION,
    PRODUCTION_RECEIPT_CODEC_VERSION,
];
const MAP_CODECS: [u16; 4] = [
    MAP_CATALOG_CODEC_VERSION,
    MAP_VALUE_CODEC_VERSION,
    MAP_INDEX_KEY_VERSION,
    MAP_RECEIPT_CODEC_VERSION,
];
const PARTIAL_CODECS: [u16; 4] = [
    PARTIAL_CATALOG_CODEC_VERSION,
    MAP_VALUE_CODEC_VERSION,
    MAP_INDEX_KEY_VERSION,
    MAP_RECEIPT_CODEC_VERSION,
];
const REFERENCE_CODECS: [u16; 4] = [
    REFERENCE_CATALOG_CODEC_VERSION,
    MAP_VALUE_CODEC_VERSION,
    MAP_INDEX_KEY_VERSION,
    MAP_RECEIPT_CODEC_VERSION,
];

const ORDERED: u8 = SCALARS | CURSOR | BOUNDED;
const GENERATED: u8 = ORDERED | GENERATIONS;
const WITH_MAPS: u8 = GENERATED | MAPS;
const WITH_PARTIAL: u8 = WITH_MAPS | PARTIAL;
const WITH_REFERENCES: u8 = WITH_PARTIAL | REFERENCES;

const LEGACY_LAYOUTS: [StorageLayout; 13] = [
    layout(1, LEGACY_CODECS, 0, false),
    layout(2, LEGACY_CODECS, 0, false),
    layout(3, LEGACY_CODECS, CURSOR, false),
    layout(4, SCALAR_CODECS, SCALARS | CURSOR, false),
    layout(5, ORDERED_CODECS, ORDERED, false),
    layout(6, ORDERED_CODECS, GENERATED, false),
    layout(7, ORDERED_CODECS, GENERATED, true),
    layout(8, MAP_CODECS, WITH_MAPS, false),
    layout(9, MAP_CODECS, WITH_MAPS, true),
    layout(10, PARTIAL_CODECS, WITH_PARTIAL, false),
    layout(11, PARTIAL_CODECS, WITH_PARTIAL, true),
    layout(12, REFERENCE_CODECS, WITH_REFERENCES, false),
    layout(13, REFERENCE_CODECS, WITH_REFERENCES, true),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StorageLayout {
    pub(crate) format: u32,
    pub(crate) catalog: u16,
    pub(crate) value: u16,
    pub(crate) index: u16,
    pub(crate) migration: u16,
    pub(crate) receipt: u16,
    pub(crate) maintenance: u16,
    pub(crate) journal: u16,
    pub(crate) capabilities: u8,
}

const fn layout(format: u32, codecs: [u16; 4], capabilities: u8, journal: bool) -> StorageLayout {
    StorageLayout {
        format,
        catalog: codecs[0],
        value: codecs[1],
        index: codecs[2],
        migration: MIGRATION_CODEC_VERSION,
        receipt: codecs[3],
        maintenance: if capabilities & GENERATIONS != 0 {
            MAINTENANCE_CODEC_VERSION
        } else {
            0
        },
        journal: if journal { JOURNAL_CODEC_VERSION } else { 0 },
        capabilities,
    }
}

impl StorageLayout {
    pub(crate) fn from_header_profile(
        map: bool,
        partial: bool,
        references: bool,
        journal: bool,
    ) -> Self {
        let (codecs, capabilities) = if references {
            (REFERENCE_CODECS, WITH_REFERENCES)
        } else if partial {
            (PARTIAL_CODECS, WITH_PARTIAL)
        } else if map {
            (MAP_CODECS, WITH_MAPS)
        } else {
            (
                [
                    PRODUCTION_CATALOG_CODEC_VERSION,
                    MAP_VALUE_CODEC_VERSION,
                    PRODUCTION_INDEX_KEY_VERSION,
                    PRODUCTION_RECEIPT_CODEC_VERSION,
                ],
                GENERATED,
            )
        };
        layout(
            CAPABILITY_STORAGE_FORMAT_VERSION,
            codecs,
            capabilities,
            journal,
        )
    }

    pub(crate) const fn has_header(self) -> bool {
        self.format == CAPABILITY_STORAGE_FORMAT_VERSION
    }

    pub(crate) fn to_header_layout(self) -> Result<Self> {
        if !matches!(self.format, 10..=14) {
            return Err(Error::new(
                "E_STORAGE_UPGRADE",
                "capability storage requires format 10, 11, 12 or 13",
            ));
        }
        Ok(Self {
            format: CAPABILITY_STORAGE_FORMAT_VERSION,
            ..self
        })
    }

    pub(crate) fn required_capabilities(self) -> Vec<String> {
        let mut result = Vec::new();
        if self.supports_partial_indexes() {
            result.push("partial_unique_index".into());
        }
        if self.supports_maps() {
            result.push("typed_map".into());
        }
        if self.supports_references() {
            result.push("typed_references".into());
        }
        result
    }

    pub(crate) fn install_capabilities(self, requested: &[String]) -> Result<Self> {
        if !self.has_header() {
            return Err(Error::new(
                "E_STORAGE_UPGRADE",
                "upgrade to capability storage format 14 before installing capabilities",
            ));
        }
        let mut maps = self.supports_maps();
        let mut partial = self.supports_partial_indexes();
        let mut references = self.supports_references();
        for capability in requested {
            match capability.as_str() {
                "typed_map" => maps = true,
                "partial_unique_index" => {
                    maps = true;
                    partial = true;
                }
                "typed_references" => {
                    maps = true;
                    partial = true;
                    references = true;
                }
                _ => {
                    return Err(Error::new(
                        "E_STORAGE_UPGRADE",
                        format!("unsupported required storage capability '{capability}'"),
                    ));
                }
            }
        }
        Ok(Self::from_header_profile(
            maps,
            partial,
            references,
            self.supports_journal(),
        ))
    }

    pub(crate) fn validate_required_state(
        self,
        database: &Database,
        receipts: &ReceiptMap,
    ) -> Result<()> {
        if database.has_generated_defaults() {
            return Err(Error::new(
                "E_STORAGE",
                "generated-default codecs are not enabled in this development candidate",
            ));
        }
        if !self.has_header() {
            return Ok(());
        }
        let retained_maps = receipts.values().any(|receipt| {
            receipt
                .response
                .rows
                .iter()
                .any(|row| row.values().any(value_has_map))
        });
        if (!self.supports_maps() && (database.requires_map_storage() || retained_maps))
            || (!self.supports_partial_indexes() && database.requires_partial_index_storage())
            || (!self.supports_references() && database.has_references())
        {
            return Err(Error::new(
                "E_STORAGE",
                "storage header omits capabilities required by catalog or retained receipts",
            ));
        }
        Ok(())
    }
    pub(crate) const fn for_format(format: u32) -> Option<Self> {
        if format == 0 || format > LEGACY_LAYOUTS.len() as u32 {
            None
        } else {
            Some(LEGACY_LAYOUTS[(format - 1) as usize])
        }
    }

    pub(crate) const fn legacy(format: u32) -> Self {
        layout(
            format,
            LEGACY_CODECS,
            if format == CURSOR_STORAGE_FORMAT_VERSION {
                CURSOR
            } else {
                0
            },
            false,
        )
    }

    pub(crate) const fn production() -> Self {
        LEGACY_LAYOUTS[(PRODUCTION_STORAGE_FORMAT_VERSION - 1) as usize]
    }
    pub(crate) const fn partial() -> Self {
        LEGACY_LAYOUTS[(PARTIAL_STORAGE_FORMAT_VERSION - 1) as usize]
    }

    pub(crate) const fn supports_production_scalars(self) -> bool {
        self.capabilities & SCALARS != 0
    }
    pub(crate) const fn supports_maps(self) -> bool {
        self.capabilities & MAPS != 0
    }
    pub(crate) const fn supports_partial_indexes(self) -> bool {
        self.capabilities & PARTIAL != 0
    }
    pub(crate) const fn supports_references(self) -> bool {
        self.capabilities & REFERENCES != 0
    }
    pub(crate) const fn supports_cursor_identity(self) -> bool {
        self.capabilities & CURSOR != 0
    }
    pub(crate) const fn supports_bounded_reads(self) -> bool {
        self.capabilities & BOUNDED != 0
    }
    pub(crate) const fn supports_generation_envelope(self) -> bool {
        self.capabilities & GENERATIONS != 0
    }
    pub(crate) const fn supports_journal(self) -> bool {
        self.journal != 0
    }

    /// Legacy formats encode journaling in the adjacent format number. Keep the
    /// translation here rather than selecting a format at every feature callsite.
    pub(crate) const fn with_journal(self) -> Option<Self> {
        if !self.supports_generation_envelope() {
            None
        } else if self.supports_journal() {
            Some(self)
        } else if self.has_header() {
            Some(Self {
                journal: JOURNAL_CODEC_VERSION,
                ..self
            })
        } else {
            Self::for_format(self.format + 1)
        }
    }

    /// Released format <=11 archives retain their exact header bytes. Reference
    /// formats are unreleased and can declare their already-required capabilities.
    pub(crate) fn archive_required_capabilities(self) -> Vec<String> {
        if self.format <= PARTIAL_JOURNAL_STORAGE_FORMAT_VERSION {
            return Vec::new();
        }
        self.required_capabilities()
    }

    pub(crate) const fn backup_format(self) -> u32 {
        if self.has_header() {
            crate::backup::CAPABILITY_BACKUP_FORMAT_VERSION
        } else if self.supports_references() {
            crate::backup::REFERENCE_BACKUP_FORMAT_VERSION
        } else if self.supports_partial_indexes() {
            crate::backup::PARTIAL_BACKUP_FORMAT_VERSION
        } else if self.supports_maps() {
            crate::backup::MAP_BACKUP_FORMAT_VERSION
        } else {
            crate::backup::PRODUCTION_BACKUP_FORMAT_VERSION
        }
    }
}

fn value_has_map(value: &crate::model::Value) -> bool {
    use crate::model::Value;
    match value {
        Value::Map(_) => true,
        Value::Named { value, .. } | Value::Option(Some(value)) => value_has_map(value),
        Value::Record(fields) => fields.values().any(value_has_map),
        Value::Enum(sum) => sum.args.iter().any(value_has_map),
        Value::Tuple(items) | Value::List(items) => items.iter().any(value_has_map),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_codec_profiles_and_unknown_formats_are_preserved() {
        // Historical contract values, independent of the registry's constants.
        let expected = [
            (2, 1, 1, 1, 0, 0),
            (2, 1, 1, 1, 0, 0),
            (2, 1, 1, 1, 0, 0),
            (3, 2, 2, 2, 0, 0),
            (4, 2, 3, 2, 0, 0),
            (4, 2, 3, 2, 1, 0),
            (4, 2, 3, 2, 1, 1),
            (5, 3, 4, 3, 1, 0),
            (5, 3, 4, 3, 1, 1),
            (6, 3, 4, 3, 1, 0),
            (6, 3, 4, 3, 1, 1),
            (7, 3, 4, 3, 1, 0),
            (7, 3, 4, 3, 1, 1),
        ];
        for (index, codecs) in expected.into_iter().enumerate() {
            let actual = StorageLayout::for_format(index as u32 + 1).unwrap();
            assert_eq!(
                (
                    actual.catalog,
                    actual.value,
                    actual.index,
                    actual.receipt,
                    actual.maintenance,
                    actual.journal
                ),
                codecs
            );
            assert_eq!(actual.migration, 1);
            let format = index as u32 + 1;
            assert_eq!(actual.supports_production_scalars(), format >= 4);
            assert_eq!(actual.supports_cursor_identity(), format >= 3);
            assert_eq!(actual.supports_bounded_reads(), format >= 5);
            assert_eq!(actual.supports_generation_envelope(), format >= 6);
            assert_eq!(actual.supports_maps(), format >= 8);
            assert_eq!(actual.supports_partial_indexes(), format >= 10);
            assert_eq!(actual.supports_references(), format >= 12);
        }
        for unknown in [0, 14, u32::MAX] {
            assert!(StorageLayout::for_format(unknown).is_none());
        }
    }

    #[test]
    fn journal_pairs_preserve_capabilities_and_non_journal_codecs() {
        for format in 1..=5 {
            assert!(
                StorageLayout::for_format(format)
                    .unwrap()
                    .with_journal()
                    .is_none()
            );
        }
        for format in [6, 8, 10, 12] {
            let source = StorageLayout::for_format(format).unwrap();
            let journal = source.with_journal().unwrap();
            assert_eq!(journal.format, format + 1);
            assert_eq!(journal.capabilities, source.capabilities);
            assert_eq!(
                (
                    journal.catalog,
                    journal.value,
                    journal.index,
                    journal.migration,
                    journal.receipt,
                    journal.maintenance
                ),
                (
                    source.catalog,
                    source.value,
                    source.index,
                    source.migration,
                    source.receipt,
                    source.maintenance
                )
            );
            assert!(journal.supports_journal());
            assert_eq!(journal.backup_format(), source.backup_format());
            assert_eq!(journal.with_journal(), Some(journal));
        }
    }
}
