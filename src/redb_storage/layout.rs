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
pub(super) struct StorageLayout {
    pub(super) format: u32,
    pub(super) catalog: u16,
    pub(super) value: u16,
    pub(super) index: u16,
    pub(super) migration: u16,
    pub(super) receipt: u16,
    pub(super) maintenance: u16,
    pub(super) journal: u16,
    pub(super) capabilities: u8,
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
    pub(super) const fn for_format(format: u32) -> Option<Self> {
        if format == 0 || format > LEGACY_LAYOUTS.len() as u32 {
            None
        } else {
            Some(LEGACY_LAYOUTS[(format - 1) as usize])
        }
    }

    pub(super) const fn legacy(format: u32) -> Self {
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

    pub(super) const fn production() -> Self {
        LEGACY_LAYOUTS[(PRODUCTION_STORAGE_FORMAT_VERSION - 1) as usize]
    }
    pub(super) const fn partial() -> Self {
        LEGACY_LAYOUTS[(PARTIAL_STORAGE_FORMAT_VERSION - 1) as usize]
    }

    pub(super) const fn supports_production_scalars(self) -> bool {
        self.capabilities & SCALARS != 0
    }
    pub(super) const fn supports_maps(self) -> bool {
        self.capabilities & MAPS != 0
    }
    pub(super) const fn supports_partial_indexes(self) -> bool {
        self.capabilities & PARTIAL != 0
    }
    pub(super) const fn supports_references(self) -> bool {
        self.capabilities & REFERENCES != 0
    }
    pub(super) const fn supports_cursor_identity(self) -> bool {
        self.capabilities & CURSOR != 0
    }
    pub(super) const fn supports_bounded_reads(self) -> bool {
        self.capabilities & BOUNDED != 0
    }
    pub(super) const fn supports_generation_envelope(self) -> bool {
        self.capabilities & GENERATIONS != 0
    }
    pub(super) const fn supports_journal(self) -> bool {
        self.journal != 0
    }

    /// Legacy formats encode journaling in the adjacent format number. Keep the
    /// translation here rather than selecting a format at every feature callsite.
    pub(super) const fn with_journal(self) -> Option<Self> {
        if !self.supports_generation_envelope() {
            None
        } else if self.supports_journal() {
            Some(self)
        } else {
            Self::for_format(self.format + 1)
        }
    }

    pub(super) const fn backup_format(self) -> u32 {
        if self.supports_references() {
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
