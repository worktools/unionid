//! Request-local allocation; committed sequence counters belong to Database.
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{Database, QueryResponse};
use crate::error::{Error, Result};
use crate::model::{BoundGeneratedDefault, Column, ScalarType, Value};
use crate::query::{GeneratedDefault, TableDefault};
use crate::scalars::{Timestamp, Uuid};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sequence {
    pub id: u64,
    pub name: String,
    pub start: i64,
    pub next: Option<i64>,
}

impl Sequence {
    fn allocate(&mut self) -> Result<i64> {
        let value = self.next.ok_or_else(|| {
            Error::new("E_ARITH", format!("sequence '{}' is exhausted", self.name))
        })?;
        self.next = value.checked_add(1);
        Ok(value)
    }
}

#[derive(Debug, Default, Clone)]
pub(super) struct GenerationContext {
    sample: Option<Timestamp>,
}

impl GenerationContext {
    fn timestamp(&mut self) -> Result<Timestamp> {
        if let Some(sample) = self.sample {
            return Ok(sample);
        }
        let sample = timestamp_from_system_time(SystemTime::now())?;
        self.sample = Some(sample);
        Ok(sample)
    }

    fn uuid(&mut self) -> Result<Uuid> {
        let timestamp = self.timestamp()?;
        let mut random = [0_u8; 10];
        getrandom::fill(&mut random)
            .map_err(|_| Error::new("E_GENERATION", "UUID entropy source failed"))?;
        uuid_v7(timestamp, random)
    }
}

fn timestamp_from_system_time(now: SystemTime) -> Result<Timestamp> {
    let nanos = match now.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).map_err(|_| generation_error())?,
        Err(error) => {
            -i128::try_from(error.duration().as_nanos()).map_err(|_| generation_error())?
        }
    };
    let micros = i64::try_from(nanos.div_euclid(1_000)).map_err(|_| generation_error())?;
    Timestamp::from_epoch_microseconds(micros).map_err(|_| generation_error())
}

fn generation_error() -> Error {
    Error::new(
        "E_GENERATION",
        "server clock is outside the supported timestamp range",
    )
}

fn uuid_v7(timestamp: Timestamp, random: [u8; 10]) -> Result<Uuid> {
    let millis = u64::try_from(timestamp.epoch_microseconds().div_euclid(1_000)).map_err(|_| {
        Error::new(
            "E_GENERATION",
            "UUID v7 requires a nonnegative Unix timestamp",
        )
    })?;
    if millis >= (1_u64 << 48) {
        return Err(Error::new(
            "E_GENERATION",
            "UUID v7 timestamp exceeds 48-bit Unix milliseconds",
        ));
    }
    let mut bytes = [0_u8; 16];
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (random[0] & 0x0f);
    bytes[7] = random[1];
    bytes[8] = 0x80 | (random[2] & 0x3f);
    bytes[9..].copy_from_slice(&random[3..]);
    Ok(Uuid::from_bytes(bytes))
}

impl Database {
    pub(super) fn create_sequence(&mut self, name: String, start: i64) -> Result<QueryResponse> {
        if self.objects.contains_key(&name)
            || self.catalog.types.contains_key(&name)
            || self.sequences.contains_key(&name)
        {
            return Err(Error::new(
                "E_SCHEMA",
                format!("name '{name}' is already in use"),
            ));
        }
        let id = self.catalog.allocate()?;
        self.sequences.insert(
            name.clone(),
            Sequence {
                id,
                name: name.clone(),
                start,
                next: Some(start),
            },
        );
        Ok(QueryResponse::ok_message(format!(
            "sequence '{name}' created"
        )))
    }

    pub(super) fn bind_generated_defaults(
        &self,
        columns: &[Column],
        defaults: &[TableDefault],
    ) -> Result<BTreeMap<u64, BoundGeneratedDefault>> {
        let mut generators = BTreeMap::new();
        for default in defaults {
            let column = columns
                .iter()
                .find(|column| column.name == default.field)
                .ok_or_else(|| {
                    Error::new(
                        "E_FIELD",
                        format!("unknown generated-default field '{}'", default.field),
                    )
                })?;
            let (generator, expected) = match &default.generator {
                GeneratedDefault::Next(name) => {
                    let sequence = self.sequences.get(name).ok_or_else(|| {
                        Error::new("E_SCHEMA", format!("unknown sequence '{name}'"))
                    })?;
                    (BoundGeneratedDefault::Next(sequence.id), ScalarType::Int)
                }
                GeneratedDefault::UuidV7 => (BoundGeneratedDefault::UuidV7, ScalarType::Uuid),
                GeneratedDefault::Now => (BoundGeneratedDefault::Now, ScalarType::Timestamp),
            };
            if !matches!(
                (self.catalog.underlying(&column.ty)?, &expected),
                (ScalarType::Int, ScalarType::Int)
                    | (ScalarType::Uuid, ScalarType::Uuid)
                    | (ScalarType::Timestamp, ScalarType::Timestamp)
            ) {
                return Err(Error::new(
                    "E_TYPE",
                    format!(
                        "generated default for '{}' requires {}",
                        column.name,
                        self.catalog.describe(&expected)
                    ),
                ));
            }
            if generators.insert(column.id, generator).is_some() {
                return Err(Error::new(
                    "E_SCHEMA",
                    format!("duplicate generated default for '{}'", column.name),
                ));
            }
        }
        Ok(generators)
    }

    pub(super) fn materialize_generated_row(
        &mut self,
        name: &str,
        values: &Value,
        operation: &str,
    ) -> Result<BTreeMap<String, Value>> {
        let mut fields = self.coerce_row(name, values, operation)?;
        if self.table(name)?.generated_defaults.is_empty() {
            return Ok(fields);
        }
        let columns = self.table(name)?.schema.clone();
        let defaults = self.table(name)?.generated_defaults.clone();
        for column in columns {
            if fields.contains_key(&column.name) {
                continue;
            }
            if let Some(generator) = defaults.get(&column.id) {
                let value = self.generate_default(generator)?;
                let value =
                    self.catalog
                        .coerce(&value, &column.ty, &format!("{name}.{}", column.name))?;
                fields.insert(column.name, value);
            }
        }
        Ok(fields)
    }

    pub(crate) fn validate_generated_state(&self) -> Result<()> {
        for (name, sequence) in &self.sequences {
            if name != &sequence.name
                || self.objects.contains_key(name)
                || self.catalog.types.contains_key(name)
            {
                return Err(Error::new(
                    "E_STORAGE",
                    "sequence name conflicts with its catalog binding",
                ));
            }
            if sequence.next.is_some_and(|next| next < sequence.start) {
                return Err(Error::new(
                    "E_STORAGE",
                    "sequence counter precedes its declared start",
                ));
            }
        }
        for table in self.schema_tables() {
            for (field_id, generator) in &table.generated_defaults {
                let column = table
                    .schema
                    .iter()
                    .find(|column| column.id == *field_id)
                    .ok_or_else(|| {
                        Error::new(
                            "E_STORAGE",
                            "generated default references an unknown table field",
                        )
                    })?;
                let expected = match generator {
                    BoundGeneratedDefault::Next(id) => {
                        if !self.sequences.values().any(|sequence| sequence.id == *id) {
                            return Err(Error::new(
                                "E_STORAGE",
                                "generated default references an unknown sequence",
                            ));
                        }
                        ScalarType::Int
                    }
                    BoundGeneratedDefault::UuidV7 => ScalarType::Uuid,
                    BoundGeneratedDefault::Now => ScalarType::Timestamp,
                };
                if !matches!(
                    (self.catalog.underlying(&column.ty)?, expected),
                    (ScalarType::Int, ScalarType::Int)
                        | (ScalarType::Uuid, ScalarType::Uuid)
                        | (ScalarType::Timestamp, ScalarType::Timestamp)
                ) {
                    return Err(Error::new(
                        "E_STORAGE",
                        "generated default result type disagrees with its field",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn begin_generation_request(&mut self) {
        self.generation_context = GenerationContext::default();
    }

    pub(crate) fn generated_input_shape(
        &self,
        table: &str,
        upsert: bool,
    ) -> Result<Option<(crate::portable::TypeShape, Vec<String>)>> {
        let table = self.table(table)?;
        let fields = table
            .schema
            .iter()
            .filter(|field| {
                table.generated_defaults.contains_key(&field.id)
                    && (!upsert || table.primary_key.as_deref() != Some(field.name.as_str()))
            })
            .map(|field| field.name.clone())
            .collect::<Vec<_>>();
        if fields.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            crate::portable::describe_type(
                &self.catalog,
                &ScalarType::Record(table.schema.clone()),
            )?,
            fields,
        )))
    }

    pub(crate) fn schema_sequences(&self) -> Vec<&Sequence> {
        let mut sequences = self.sequences.values().collect::<Vec<_>>();
        sequences.sort_by_key(|sequence| sequence.id);
        sequences
    }

    pub(crate) fn generated_default_source(&self, generator: &BoundGeneratedDefault) -> String {
        match generator {
            BoundGeneratedDefault::Next(id) => {
                let name = self
                    .sequences
                    .values()
                    .find(|sequence| sequence.id == *id)
                    .map(|sequence| sequence.name.as_str())
                    .unwrap_or("unknown_sequence");
                format!("next({name})")
            }
            BoundGeneratedDefault::UuidV7 => "uuid_v7()".into(),
            BoundGeneratedDefault::Now => "now()".into(),
        }
    }

    pub(crate) fn has_generated_defaults(&self) -> bool {
        !self.sequences.is_empty()
            || self
                .schema_tables()
                .iter()
                .any(|table| !table.generated_defaults.is_empty())
    }

    pub(crate) fn ensure_legacy_generation(&self) -> Result<()> {
        if self.has_generated_defaults() {
            return Err(Error::new(
                "E_STORAGE_UPGRADE_REQUIRED",
                "generated defaults require native redb storage; legacy WAL and snapshots cannot preserve the capability contract",
            ));
        }
        Ok(())
    }

    pub(super) fn generate_default(&mut self, generator: &BoundGeneratedDefault) -> Result<Value> {
        match generator {
            BoundGeneratedDefault::Next(id) => {
                let sequence = self
                    .sequences
                    .values_mut()
                    .find(|sequence| sequence.id == *id)
                    .ok_or_else(|| {
                        Error::new(
                            "E_SCHEMA",
                            "generated default references an unknown sequence",
                        )
                    })?;
                let before = sequence.next;
                let value = sequence.allocate()?;
                let after = sequence.next;
                self.pending_writes
                    .sequence_counters
                    .entry(*id)
                    .and_modify(|change| change.1 = after)
                    .or_insert((before, after));
                Ok(Value::Int(value))
            }
            BoundGeneratedDefault::UuidV7 => self.generation_context.uuid().map(Value::Uuid),
            BoundGeneratedDefault::Now => self.generation_context.timestamp().map(Value::Timestamp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_rounds_toward_the_previous_microsecond_across_the_epoch() {
        use std::time::Duration;
        for (nanos, expected) in [(1, -1), (999, -1), (1_000, -1), (1_001, -2)] {
            assert_eq!(
                timestamp_from_system_time(UNIX_EPOCH - Duration::from_nanos(nanos))
                    .unwrap()
                    .epoch_microseconds(),
                expected
            );
        }
        assert_eq!(
            timestamp_from_system_time(UNIX_EPOCH + Duration::from_nanos(999))
                .unwrap()
                .epoch_microseconds(),
            0
        );
        assert_eq!(
            timestamp_from_system_time(UNIX_EPOCH + Duration::from_secs(u64::MAX / 2))
                .unwrap_err()
                .code,
            "E_GENERATION"
        );
    }

    #[test]
    fn durable_catalog_round_trip_preserves_counters_and_default_bindings() {
        let mut engine = crate::Engine::memory();
        assert!(engine.execute("sequence ids {start 1}\nstruct Item {id: int, owner: text}\ntable items: Item {key id, default id = next(ids)}\ninsert items {owner: \"first\"}").ok);
        let before = engine.database_snapshot().unwrap();
        let mut candidate = before.clone();
        let statement = crate::syntax::parse("insert items {owner: \"second\"} | returning id")
            .unwrap()
            .remove(0)
            .statement;
        let response = candidate.execute(statement).unwrap();
        assert!(response.rows[0]["id"].cmp_eq(&Value::Int(2)));
        let writes = candidate.take_write_set();
        let sequence = candidate.sequences.get("ids").unwrap();
        assert_eq!(
            writes.sequence_counters.get(&sequence.id),
            Some(&(Some(2), Some(3)))
        );
        let entries = candidate.durable_catalog_entries();
        let rows = candidate
            .durable_rows_with_codec(crate::codec::MAP_VALUE_CODEC_VERSION)
            .unwrap();
        let mut restored = Database::from_durable(
            candidate.durable_meta(),
            entries.clone(),
            rows.clone(),
            candidate.migration_history.clone(),
        )
        .unwrap();
        assert_eq!(restored.schema_info(), candidate.schema_info());
        assert_eq!(restored.sequences["ids"].next, Some(3));
        let statement = crate::syntax::parse("insert items {owner: \"third\"} | returning id")
            .unwrap()
            .remove(0)
            .statement;
        let response = restored.execute(statement).unwrap();
        assert!(response.rows[0]["id"].cmp_eq(&Value::Int(3)));
        let mut corrupt = entries.clone();
        for entry in &mut corrupt {
            if let super::super::DurableCatalogEntry::Sequence(sequence) = entry {
                sequence.next = Some(0);
            }
        }
        assert_eq!(
            Database::from_durable(candidate.durable_meta(), corrupt, rows.clone(), vec![])
                .unwrap_err()
                .code,
            "E_STORAGE"
        );
        let no_sequences = entries
            .into_iter()
            .filter(|entry| !matches!(entry, super::super::DurableCatalogEntry::Sequence(_)))
            .collect();
        assert_eq!(
            Database::from_durable(candidate.durable_meta(), no_sequences, rows, vec![])
                .unwrap_err()
                .code,
            "E_STORAGE"
        );
    }

    #[test]
    fn sequence_allocates_maximum_once_without_wrap_or_reset() {
        let mut sequence = Sequence {
            id: 1,
            name: "last".into(),
            start: i64::MAX,
            next: Some(i64::MAX),
        };
        assert_eq!(sequence.allocate().unwrap(), i64::MAX);
        assert!(sequence.next.is_none());
        assert_eq!(sequence.allocate().unwrap_err().code, "E_ARITH");
    }

    #[test]
    fn uuid_v7_has_exact_timestamp_version_variant_and_independent_random_bits() {
        let timestamp =
            Timestamp::from_epoch_microseconds(0x010203040506_i64 * 1_000 + 999).unwrap();
        let uuid = uuid_v7(timestamp, [255; 10]).unwrap();
        assert_eq!(
            uuid.as_bytes(),
            &[
                1, 2, 3, 4, 5, 6, 0x7f, 0xff, 0xbf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
            ]
        );
        let zero = uuid_v7(timestamp, [0; 10]).unwrap();
        assert_eq!(
            zero.as_bytes(),
            &[1, 2, 3, 4, 5, 6, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            uuid_v7(Timestamp::from_epoch_microseconds(-1).unwrap(), [0; 10])
                .unwrap_err()
                .code,
            "E_GENERATION"
        );
    }
}
