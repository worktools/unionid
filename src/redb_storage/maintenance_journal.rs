//! Stream one migration commit directly into its atomic cutover transaction.
//! Only two cursor entries and one encoded record are held by the executor.

use super::*;

fn visit_changes(
    source: &impl ReadableTable<&'static [u8], &'static [u8]>,
    target: &impl ReadableTable<&'static [u8], &'static [u8]>,
    source_generation: GenerationRef,
    target_generation: GenerationRef,
    control: Option<&crate::control::ExecutionControl>,
    mut visit: impl FnMut(&[u8], Option<&[u8]>) -> Result<()>,
) -> Result<()> {
    check_source_control(control)?;
    let (lower, upper) = generation_scan_bounds(source_generation)?;
    let mut source = source
        .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
        .map_err(|error| storage_error("scan journal source generation", error))?;
    let (lower, upper) = generation_scan_bounds(target_generation)?;
    let mut target = target
        .range::<&[u8]>((borrowed_bound(&lower), borrowed_bound(&upper)))
        .map_err(|error| storage_error("scan journal target generation", error))?;
    let read_error = |error| storage_error("read journal generation entry", error);
    let mut before = source.next().transpose().map_err(read_error)?;
    let mut after = target.next().transpose().map_err(read_error)?;
    while before.is_some() || after.is_some() {
        check_source_control(control)?;
        let before_key = before
            .as_ref()
            .map(|(key, _)| logical_generation_key(source_generation, key.value()))
            .transpose()?;
        let after_key = after
            .as_ref()
            .map(|(key, _)| logical_generation_key(target_generation, key.value()))
            .transpose()?;
        let order = match (before_key, after_key) {
            (Some(before), Some(after)) => before.cmp(after),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => break,
        };
        match order {
            std::cmp::Ordering::Less => {
                if let Some(key) = before_key {
                    visit(key, None)?;
                }
                before = source.next().transpose().map_err(read_error)?;
            }
            std::cmp::Ordering::Greater => {
                if let (Some(key), Some((_, value))) = (after_key, after.as_ref()) {
                    visit(key, Some(value.value()))?;
                }
                after = target.next().transpose().map_err(read_error)?;
            }
            std::cmp::Ordering::Equal => {
                if let (Some((_, before_value)), Some((_, after_value)), Some(key)) =
                    (before.as_ref(), after.as_ref(), after_key)
                    && before_value.value() != after_value.value()
                {
                    visit(key, Some(after_value.value()))?;
                }
                before = source.next().transpose().map_err(read_error)?;
                after = target.next().transpose().map_err(read_error)?;
            }
        }
    }
    Ok(())
}

struct Writer<'a> {
    table: redb::Table<'a, &'static [u8], &'static [u8]>,
    current: &'a JournalState,
    sequence: u64,
    ordinal: u64,
    bytes: u64,
    digest: Sha256,
    control: Option<&'a crate::control::ExecutionControl>,
}

impl<'a> Writer<'a> {
    fn new(
        transaction: &'a redb::WriteTransaction,
        current: &'a JournalState,
        sequence: u64,
        control: Option<&'a crate::control::ExecutionControl>,
    ) -> Result<Self> {
        if current
            .last_retained_sequence
            .unwrap_or(current.exported_sequence)
            .checked_add(1)
            != Some(sequence)
        {
            return Err(Error::new(
                "E_BACKUP_CHAIN",
                "cutover journal sequence does not extend its durable head",
            ));
        }
        if current.commit_count >= current.max_commits {
            return Err(Error::new(
                "E_BACKUP_JOURNAL_FULL",
                "backup journal commit capacity exceeded; export the active chain before retrying",
            ));
        }
        let state_table = transaction
            .open_table(BACKUP_CHAIN_STATE)
            .map_err(|error| storage_error("open cutover journal state", error))?;
        let stored = state_table
            .get(JOURNAL_STATE_KEY)
            .map_err(|error| storage_error("read cutover journal state", error))?
            .ok_or_else(|| Error::new("E_STORAGE", "active backup chain state disappeared"))?;
        if decode_journal_state(stored.value())? != *current {
            return Err(Error::new(
                "E_STORAGE",
                "backup chain state changed before cutover",
            ));
        }
        let mut digest = Sha256::new();
        digest.update(JOURNAL_COMMIT_DOMAIN);
        digest.update(current.head_checksum.as_bytes());
        Ok(Self {
            table: transaction
                .open_table(BACKUP_JOURNAL)
                .map_err(|error| storage_error("open cutover journal", error))?,
            current,
            sequence,
            ordinal: 0,
            bytes: 0,
            digest,
            control,
        })
    }

    fn record(&mut self, kind: u8, payload: Vec<u8>) -> Result<()> {
        check_source_control(self.control)?;
        // Reuse the existing codec; this temporary vector contains one record.
        let mut record = Vec::with_capacity(1);
        push_journal_record(&mut record, self.sequence, &mut self.ordinal, kind, payload)?;
        let (key, value) = record
            .pop()
            .ok_or_else(|| Error::new("E_STORAGE", "journal codec emitted no record"))?;
        self.bytes = self
            .bytes
            .checked_add(usize_u64(key.len()))
            .and_then(|bytes| bytes.checked_add(usize_u64(value.len())))
            .ok_or_else(|| Error::new("E_LIMIT", "backup journal byte count overflow"))?;
        if self
            .current
            .expanded_bytes
            .checked_add(self.bytes)
            .is_none_or(|bytes| bytes > self.current.max_bytes)
        {
            return Err(Error::new(
                "E_BACKUP_JOURNAL_FULL",
                "backup journal byte capacity exceeded; export the active chain before retrying",
            ));
        }
        if self
            .table
            .get(key.as_slice())
            .map_err(|error| storage_error("check cutover journal key", error))?
            .is_some()
        {
            return Err(Error::new(
                "E_STORAGE",
                "backup journal key is already occupied",
            ));
        }
        if kind != 25 {
            self.digest.update(&key);
            self.digest.update(&value);
        }
        self.table
            .insert(key.as_slice(), value.as_slice())
            .map_err(|error| storage_error("write cutover journal record", error))?;
        Ok(())
    }

    fn changes(
        &mut self,
        source: &impl ReadableTable<&'static [u8], &'static [u8]>,
        target: &impl ReadableTable<&'static [u8], &'static [u8]>,
        source_generation: GenerationRef,
        target_generation: GenerationRef,
        delete_kind: u8,
    ) -> Result<()> {
        // Canonical codec order requires deletes before writes. Two passes
        // preserve that contract without retaining either set of changes.
        for deleting in [true, false] {
            visit_changes(
                source,
                target,
                source_generation,
                target_generation,
                self.control,
                |key, value| {
                    if value.is_none() != deleting {
                        return Ok(());
                    }
                    let mut payload = Vec::new();
                    push_journal_bytes(&mut payload, key)?;
                    if let Some(value) = value {
                        push_journal_bytes(&mut payload, value)?;
                    }
                    self.record(delete_kind + u8::from(!deleting), payload)
                },
            )?;
        }
        Ok(())
    }

    fn finish(mut self, transaction: &redb::WriteTransaction) -> Result<JournalState> {
        let checksum = format!("sha256:{:x}", self.digest.clone().finalize());
        self.record(25, checksum.as_bytes().to_vec())?;
        let state = JournalState {
            head_checksum: checksum,
            first_retained_sequence: self.current.first_retained_sequence.or(Some(self.sequence)),
            last_retained_sequence: Some(self.sequence),
            commit_count: self
                .current
                .commit_count
                .checked_add(1)
                .ok_or_else(|| Error::new("E_LIMIT", "backup journal commit count exhausted"))?,
            expanded_bytes: self
                .current
                .expanded_bytes
                .checked_add(self.bytes)
                .ok_or_else(|| Error::new("E_LIMIT", "backup journal byte count overflow"))?,
            ..self.current.clone()
        };
        let mut table = transaction
            .open_table(BACKUP_CHAIN_STATE)
            .map_err(|error| storage_error("open cutover journal state", error))?;
        table
            .insert(JOURNAL_STATE_KEY, encode_journal_state(&state)?.as_slice())
            .map_err(|error| storage_error("write cutover journal state", error))?;
        Ok(state)
    }
}

pub(super) fn append(
    database: &RedbDatabase,
    transaction: &redb::WriteTransaction,
    current: &JournalState,
    generations: (GenerationRef, GenerationRef),
    target: &Database,
    migration_position: usize,
    control: Option<&crate::control::ExecutionControl>,
) -> Result<JournalState> {
    check_source_control(control)?;
    let (source_generation, target_generation) = generations;
    let read = database
        .begin_read()
        .map_err(|error| storage_error("read cutover journal generations", error))?;
    let source_catalog = read
        .open_table(match source_generation {
            GenerationRef::Legacy0 => CATALOG,
            GenerationRef::Generated(_) => GENERATION_CATALOG,
        })
        .map_err(|error| storage_error("open journal source catalog", error))?;
    let target_catalog = read
        .open_table(GENERATION_CATALOG)
        .map_err(|error| storage_error("open journal target catalog", error))?;
    let source_rows = read
        .open_table(match source_generation {
            GenerationRef::Legacy0 => ROWS,
            GenerationRef::Generated(_) => GENERATION_ROWS,
        })
        .map_err(|error| storage_error("open journal source rows", error))?;
    let target_rows = read
        .open_table(GENERATION_ROWS)
        .map_err(|error| storage_error("open journal target rows", error))?;
    let mut counts = [0_u64, 0, 1, 0];
    for (position, source, target_table) in [
        (0, &source_catalog, &target_catalog),
        (1, &source_rows, &target_rows),
    ] {
        visit_changes(
            source,
            target_table,
            source_generation,
            target_generation,
            control,
            |_, _| {
                counts[position] = counts[position]
                    .checked_add(1)
                    .ok_or_else(|| Error::new("E_LIMIT", "journal change count overflow"))?;
                Ok(())
            },
        )?;
    }
    let meta = target.durable_meta();
    let mut writer = Writer::new(transaction, current, meta.sequence, control)?;
    let mut begin = Vec::new();
    begin.extend_from_slice(&meta.sequence.to_be_bytes());
    push_journal_bytes(&mut begin, current.head_checksum.as_bytes())?;
    begin.extend_from_slice(&meta.schema_revision.to_be_bytes());
    begin.extend_from_slice(&meta.next_catalog_id.to_be_bytes());
    push_journal_bytes(&mut begin, meta.schema_hash.as_bytes())?;
    for count in counts {
        begin.extend_from_slice(&count.to_be_bytes());
    }
    let layout = {
        let meta = read
            .open_table(META)
            .map_err(|error| storage_error("read cutover journal storage header", error))?;
        read_meta(&meta)?.1
    };
    if layout.has_header() {
        push_journal_bytes(&mut begin, &StorageHeader::encode_layout(layout)?)?;
    }
    writer.record(16, begin)?;
    writer.changes(
        &source_catalog,
        &target_catalog,
        source_generation,
        target_generation,
        17,
    )?;
    writer.changes(
        &source_rows,
        &target_rows,
        source_generation,
        target_generation,
        19,
    )?;
    let entry = target
        .durable_migrations()
        .get(migration_position)
        .ok_or_else(|| Error::new("E_STORAGE", "cutover migration entry disappeared"))?;
    let mut payload = Vec::new();
    push_journal_bytes(
        &mut payload,
        &u64::try_from(migration_position)
            .map_err(|_| Error::new("E_LIMIT", "migration position exhausted"))?
            .to_be_bytes(),
    )?;
    push_journal_bytes(&mut payload, &encode_migration_entry(entry)?)?;
    writer.record(22, payload)?;
    writer.finish(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_migrations_preserve_codec_bytes_and_rollback_partial_appends() {
        for source_generation in [GenerationRef::Legacy0, GenerationRef::Generated(1)] {
            for migration in [
                "migration add_note\n  add field Item.note text = \"migrated\"",
                "migration rename\n  rename table items to renamed_items",
                "migration remove\n  drop table items",
            ] {
                let mut engine = crate::Engine::memory();
                assert!(engine.execute("struct Item {id: int, label: text}\ntable items: Item {key id}\ninsert many items [{id: 1, label: \"one\"}, {id: 2, label: \"two\"}]").ok);
                let previous = engine.database_snapshot().unwrap();
                engine
                    .apply_migrations(&[MigrationFile::parse(migration).unwrap()])
                    .unwrap();
                let next = engine.database_snapshot().unwrap();
                let metadata = next.metadata_only().unwrap();
                let layout = StorageLayout::partial().with_journal().unwrap();
                let previous = PreparedState::new(&previous, &ReceiptMap::new(), layout).unwrap();
                let next = PreparedState::new(&next, &ReceiptMap::new(), layout).unwrap();
                let current = JournalState {
                    version: JOURNAL_CODEC_VERSION,
                    chain_id: "streaming".into(),
                    baseline_sequence: previous.meta.sequence,
                    exported_sequence: previous.meta.sequence,
                    exported_checksum: format!("sha256:{}", "e".repeat(64)),
                    head_checksum: format!("sha256:{}", "e".repeat(64)),
                    first_retained_sequence: None,
                    last_retained_sequence: None,
                    commit_count: 0,
                    expanded_bytes: 0,
                    max_commits: 10,
                    max_bytes: 1_000_000,
                };
                let expected = PreparedDelta::between(&previous, &next)
                    .prepare_journal_append(&current)
                    .unwrap();
                for failure in ["none", "capacity", "cancel"] {
                    let path = std::env::temp_dir().join(format!(
                        "unionid-streaming-journal-{}-{}.redb",
                        std::process::id(),
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_nanos()
                    ));
                    let (store, _, _, _, _) = RedbStore::open(&path).unwrap();
                    let mut current = current.clone();
                    if failure == "capacity" {
                        // Fail at the terminal record after earlier records
                        // have already been inserted into the transaction.
                        current.max_bytes = expected.encoded_bytes - 1;
                    }
                    let target_generation = GenerationRef::Generated(2);
                    let setup = store.database.begin_write().unwrap();
                    for (generation, prepared) in
                        [(source_generation, &previous), (target_generation, &next)]
                    {
                        let mut catalog = setup
                            .open_table(match generation {
                                GenerationRef::Legacy0 => CATALOG,
                                GenerationRef::Generated(_) => GENERATION_CATALOG,
                            })
                            .unwrap();
                        for (key, value) in &prepared.catalog {
                            catalog
                                .insert(
                                    physical_generation_key(generation, key).unwrap().as_slice(),
                                    value.as_slice(),
                                )
                                .unwrap();
                        }
                        let mut rows = setup
                            .open_table(match generation {
                                GenerationRef::Legacy0 => ROWS,
                                GenerationRef::Generated(_) => GENERATION_ROWS,
                            })
                            .unwrap();
                        for (key, value) in &prepared.rows {
                            rows.insert(
                                physical_generation_key(generation, key).unwrap().as_slice(),
                                value.as_slice(),
                            )
                            .unwrap();
                        }
                    }
                    setup
                        .open_table(BACKUP_CHAIN_STATE)
                        .unwrap()
                        .insert(
                            JOURNAL_STATE_KEY,
                            encode_journal_state(&current).unwrap().as_slice(),
                        )
                        .unwrap();
                    setup.open_table(BACKUP_JOURNAL).unwrap();
                    setup.commit().unwrap();
                    let transaction = store.database.begin_write().unwrap();
                    let result = if failure == "cancel" {
                        use std::sync::atomic::{AtomicBool, Ordering};
                        let cancelled = Arc::new(AtomicBool::new(false));
                        let control = crate::control::ExecutionControl::cancellable(
                            Instant::now() + std::time::Duration::from_secs(30),
                            Arc::clone(&cancelled),
                            None,
                        );
                        let mut writer =
                            Writer::new(&transaction, &current, metadata.sequence, Some(&control))
                                .unwrap();
                        let (kind, payload) =
                            decode_journal_record(&expected.records[0].1).unwrap();
                        writer.record(kind, payload.to_vec()).unwrap();
                        assert_eq!(writer.table.len().unwrap(), 1);
                        cancelled.store(true, Ordering::Release);
                        writer
                            .record(25, expected.state.head_checksum.as_bytes().to_vec())
                            .map(|()| current.clone())
                    } else {
                        append(
                            &store.database,
                            &transaction,
                            &current,
                            (source_generation, target_generation),
                            &metadata,
                            0,
                            None,
                        )
                    };
                    if failure != "none" {
                        let expected_code = if failure == "cancel" {
                            "E_CANCELLED"
                        } else {
                            "E_BACKUP_JOURNAL_FULL"
                        };
                        assert_eq!(result.unwrap_err().code, expected_code);
                        drop(transaction);
                    } else {
                        assert_eq!(result.unwrap(), expected.state);
                        transaction.commit().unwrap();
                    }
                    let read = store.database.begin_read().unwrap();
                    let journal = read.open_table(BACKUP_JOURNAL).unwrap();
                    let records = journal
                        .iter()
                        .unwrap()
                        .map(|entry| {
                            let (key, value) = entry.unwrap();
                            (key.value().to_vec(), value.value().to_vec())
                        })
                        .collect::<Vec<_>>();
                    if failure != "none" {
                        assert!(records.is_empty());
                        let state = read.open_table(BACKUP_CHAIN_STATE).unwrap();
                        assert_eq!(
                            decode_journal_state(
                                state.get(JOURNAL_STATE_KEY).unwrap().unwrap().value()
                            )
                            .unwrap(),
                            current
                        );
                    } else {
                        assert_eq!(records, expected.records);
                        validate_journal_records(&journal, &expected.state, store.committed.layout)
                            .unwrap();
                    }
                    drop(journal);
                    drop(read);
                    drop(store);
                    std::fs::remove_file(path).unwrap();
                }
            }
        }
    }
}
