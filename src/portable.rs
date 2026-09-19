//! Versioned, machine-readable ADT descriptions for generated clients.
//!
//! Descriptions expose catalog IDs as decimal strings because they are scoped
//! to one database lineage and may exceed lossless JSON-number ranges. Runtime
//! validation delegates to the same catalog coercion used by query binding.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::db::{Database, IndexKind, SchemaInfo};
use crate::error::{Error, Result};
use crate::model::{Catalog, Column, EnumType, ScalarType};
use crate::protocol::WireValue;

pub const DESCRIPTION_VERSION: u32 = 2;

/// Version 1 descriptions predate partial unique index predicates and remain
/// readable; version 2 adds an optional normalized predicate per index.
const SUPPORTED_DESCRIPTION_VERSIONS: &[u32] = &[1, DESCRIPTION_VERSION];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemaDescription {
    pub version: u32,
    pub id_scope: IdScope,
    pub schema: PortableSchemaIdentity,
    pub types: Vec<TypeDescription>,
    pub tables: Vec<TableDescription>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortableSchemaIdentity {
    pub revision: String,
    pub hash: String,
}

impl From<SchemaInfo> for PortableSchemaIdentity {
    fn from(schema: SchemaInfo) -> Self {
        Self {
            revision: schema.revision.to_string(),
            hash: schema.hash,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdScope {
    pub kind: String,
    pub encoding: String,
    pub globally_stable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeDescription {
    pub id: String,
    pub name: String,
    pub shape: TypeShape,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypeShape {
    Int {
        min: String,
        max: String,
    },
    Float {
        bits: u8,
        finite_only: bool,
    },
    Bool,
    Text {
        encoding: String,
    },
    Uuid {
        bits: u8,
    },
    Date {
        unit: String,
        min_days: String,
        max_days: String,
    },
    Timestamp {
        unit: String,
        epoch: String,
        normalized: String,
        min_microseconds: String,
        max_microseconds: String,
    },
    Duration {
        unit: String,
        min: String,
        max: String,
    },
    Decimal {
        precision: u8,
        scale: u8,
        coefficient_encoding: String,
    },
    Bytes {
        wire_encoding: String,
        max_bytes: usize,
    },
    Sum {
        variants: Vec<VariantDescription>,
    },
    Record {
        fields: Vec<FieldDescription>,
    },
    Tuple {
        items: Vec<TypeShape>,
    },
    Option {
        item: Box<TypeShape>,
    },
    List {
        item: Box<TypeShape>,
        max_items: usize,
    },
    Map {
        key: Box<TypeShape>,
        value: Box<TypeShape>,
        max_entries: usize,
        max_key_bytes: usize,
    },
    Ref {
        type_id: String,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldDescription {
    pub id: String,
    pub name: String,
    pub shape: TypeShape,
    pub input_omittable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<WireValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VariantDescription {
    pub id: String,
    pub name: String,
    pub payload: Vec<TypeShape>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableDescription {
    pub id: String,
    pub name: String,
    pub row: TypeShape,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_key: Option<KeyDescription>,
    pub indexes: Vec<IndexDescription>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyDescription {
    pub field: String,
    pub field_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexDescription {
    pub id: String,
    pub unique: bool,
    pub components: Vec<IndexComponentDescription>,
    /// Canonical normalized partial predicate; omitted for whole-table indexes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicate: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexComponentDescription {
    pub field: String,
    pub field_path: Vec<String>,
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityLevel {
    Compatible,
    Conditional,
    Incompatible,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatibilityFinding {
    pub code: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatibilityAxis {
    pub level: CompatibilityLevel,
    pub findings: Vec<CompatibilityFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvolutionReport {
    pub baseline: PortableSchemaIdentity,
    pub candidate: PortableSchemaIdentity,
    pub existing_data: CompatibilityAxis,
    pub query: CompatibilityAxis,
    pub client_read: CompatibilityAxis,
    pub client_write: CompatibilityAxis,
}

impl SchemaDescription {
    /// Validate a deserialized description before a generator relies on it.
    pub fn validate(&self) -> Result<()> {
        if !SUPPORTED_DESCRIPTION_VERSIONS.contains(&self.version) {
            return Err(Error::new(
                "E_CONTRACT_VERSION",
                format!(
                    "portable description version {} is unsupported; expected {DESCRIPTION_VERSION}",
                    self.version
                ),
            ));
        }
        if self.id_scope.kind != "database_local_catalog"
            || self.id_scope.encoding != "unsigned_64_bit_decimal_string"
            || self.id_scope.globally_stable
        {
            return Err(Error::new(
                "E_CONTRACT_SCOPE",
                "portable description must use database-local unsigned decimal catalog IDs",
            ));
        }
        parse_contract_id(&self.schema.revision, "schema.revision", true)?;
        let Some(hash) = self.schema.hash.strip_prefix("sha256:") else {
            return Err(Error::new(
                "E_CONTRACT_SCHEMA",
                "schema hash must use the sha256:<hex> form",
            ));
        };
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(Error::new(
                "E_CONTRACT_SCHEMA",
                "schema hash must contain exactly 64 hexadecimal digits",
            ));
        }

        let mut ids = BTreeSet::new();
        let mut type_names = BTreeSet::new();
        let mut type_definitions = BTreeMap::new();
        for ty in &self.types {
            let id = insert_contract_id(&mut ids, &ty.id, &format!("type {}", ty.name))?;
            if !type_names.insert(ty.name.as_str()) {
                return Err(Error::new(
                    "E_CONTRACT_SCHEMA",
                    format!("duplicate portable type name '{}'", ty.name),
                ));
            }
            type_definitions.insert(id, (ty.name.as_str(), &ty.shape));
        }
        for ty in &self.types {
            validate_shape(
                &ty.shape,
                &type_definitions,
                &mut ids,
                &format!("type {}", ty.name),
                0,
            )?;
        }

        let mut table_names = BTreeSet::new();
        for table in &self.tables {
            insert_contract_id(&mut ids, &table.id, &format!("table {}", table.name))?;
            if !table_names.insert(table.name.as_str()) {
                return Err(Error::new(
                    "E_CONTRACT_SCHEMA",
                    format!("duplicate portable table name '{}'", table.name),
                ));
            }
            validate_shape(
                &table.row,
                &type_definitions,
                &mut ids,
                &format!("table {}.row", table.name),
                0,
            )?;
            if let Some(key) = &table.primary_key {
                let field = validate_field_path(
                    &table.row,
                    &key.field_path,
                    &type_definitions,
                    &format!("table {}.key", table.name),
                )?;
                if field != key.field {
                    return Err(Error::new(
                        "E_CONTRACT_SCHEMA",
                        format!(
                            "table {} key name '{}' does not match field-ID path '{field}'",
                            table.name, key.field
                        ),
                    ));
                }
            }
            for index in &table.indexes {
                insert_contract_id(&mut ids, &index.id, &format!("table {}.index", table.name))?;
                if index.components.is_empty() {
                    return Err(Error::new(
                        "E_CONTRACT_SCHEMA",
                        format!("table {} index {} has no components", table.name, index.id),
                    ));
                }
                if let Some(predicate) = &index.predicate {
                    if self.version < 2 {
                        return Err(Error::new(
                            "E_CONTRACT_SCHEMA",
                            format!(
                                "table {} index {} carries a predicate, which requires description version 2",
                                table.name, index.id
                            ),
                        ));
                    }
                    if !index.unique {
                        return Err(Error::new(
                            "E_CONTRACT_SCHEMA",
                            format!(
                                "table {} index {} carries a predicate but is not unique",
                                table.name, index.id
                            ),
                        ));
                    }
                    if predicate.is_empty() || predicate.trim() != predicate {
                        return Err(Error::new(
                            "E_CONTRACT_SCHEMA",
                            format!(
                                "table {} index {} predicate must be a non-empty canonical string",
                                table.name, index.id
                            ),
                        ));
                    }
                }
                for component in &index.components {
                    let field = validate_field_path(
                        &table.row,
                        &component.field_path,
                        &type_definitions,
                        &format!("table {}.index {}", table.name, index.id),
                    )?;
                    if field != component.field {
                        return Err(Error::new(
                            "E_CONTRACT_SCHEMA",
                            format!(
                                "table {} index {} name '{}' does not match field-ID path '{field}'",
                                table.name, index.id, component.field
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Compare two snapshots known by the caller to belong to the same catalog lineage.
    ///
    /// Catalog IDs are database-local, so comparing independently created databases
    /// through this method is invalid even when their source text is identical.
    pub fn compare_same_catalog(&self, candidate: &Self) -> Result<EvolutionReport> {
        self.validate()?;
        candidate.validate()?;
        let mut report = EvolutionReport::new(self.schema.clone(), candidate.schema.clone());
        let candidate_types = candidate
            .types
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect::<BTreeMap<_, _>>();
        for baseline_type in &self.types {
            let path = format!("type#{}", baseline_type.id);
            let Some(candidate_type) = candidate_types.get(baseline_type.id.as_str()) else {
                report.break_all(
                    "type_removed",
                    &path,
                    format!("type '{}' was removed", baseline_type.name),
                );
                continue;
            };
            if baseline_type.name != candidate_type.name {
                report.break_names(
                    "type_renamed",
                    &path,
                    format!(
                        "type '{}' was renamed to '{}'",
                        baseline_type.name, candidate_type.name
                    ),
                );
            }
            compare_shape(
                &baseline_type.shape,
                &candidate_type.shape,
                &format!("type {}", candidate_type.name),
                &mut report,
            );
        }

        compare_tables(self, candidate, &mut report);
        Ok(report)
    }
}

impl CompatibilityAxis {
    fn compatible() -> Self {
        Self {
            level: CompatibilityLevel::Compatible,
            findings: Vec::new(),
        }
    }

    fn add(
        &mut self,
        level: CompatibilityLevel,
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        if severity(level) > severity(self.level) {
            self.level = level;
        }
        self.findings.push(CompatibilityFinding {
            code: code.into(),
            path: path.into(),
            message: message.into(),
        });
    }
}

impl EvolutionReport {
    fn new(baseline: PortableSchemaIdentity, candidate: PortableSchemaIdentity) -> Self {
        Self {
            baseline,
            candidate,
            existing_data: CompatibilityAxis::compatible(),
            query: CompatibilityAxis::compatible(),
            client_read: CompatibilityAxis::compatible(),
            client_write: CompatibilityAxis::compatible(),
        }
    }

    fn break_all(&mut self, code: &str, path: &str, message: String) {
        for axis in [
            &mut self.existing_data,
            &mut self.query,
            &mut self.client_read,
            &mut self.client_write,
        ] {
            axis.add(
                CompatibilityLevel::Incompatible,
                code,
                path,
                message.clone(),
            );
        }
    }

    fn break_names(&mut self, code: &str, path: &str, message: String) {
        for axis in [
            &mut self.query,
            &mut self.client_read,
            &mut self.client_write,
        ] {
            axis.add(
                CompatibilityLevel::Incompatible,
                code,
                path,
                message.clone(),
            );
        }
    }
}

fn severity(level: CompatibilityLevel) -> u8 {
    match level {
        CompatibilityLevel::Compatible => 0,
        CompatibilityLevel::Conditional => 1,
        CompatibilityLevel::Incompatible => 2,
    }
}

fn parse_contract_id(value: &str, path: &str, allow_zero: bool) -> Result<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(Error::new(
            "E_CONTRACT_SCHEMA",
            format!("{path} must be a canonical unsigned decimal string"),
        ));
    }
    let id = value.parse::<u64>().map_err(|_| {
        Error::new(
            "E_CONTRACT_SCHEMA",
            format!("{path} exceeds the unsigned 64-bit range"),
        )
    })?;
    if !allow_zero && id == 0 {
        return Err(Error::new(
            "E_CONTRACT_SCHEMA",
            format!("{path} must be positive"),
        ));
    }
    Ok(id)
}

fn insert_contract_id(ids: &mut BTreeSet<u64>, value: &str, path: &str) -> Result<u64> {
    let id = parse_contract_id(value, path, false)?;
    if !ids.insert(id) {
        return Err(Error::new(
            "E_CONTRACT_SCHEMA",
            format!("{path} repeats catalog ID {id}"),
        ));
    }
    Ok(id)
}

fn validate_field_path(
    root: &TypeShape,
    path: &[String],
    types: &BTreeMap<u64, (&str, &TypeShape)>,
    owner: &str,
) -> Result<String> {
    if path.is_empty() {
        return Err(Error::new(
            "E_CONTRACT_SCHEMA",
            format!("{owner} has an empty field-ID path"),
        ));
    }
    let mut shape = root;
    let mut names = Vec::with_capacity(path.len());
    for (depth, value) in path.iter().enumerate() {
        let id = parse_contract_id(value, owner, false)?;
        for _ in 0..crate::model::MAX_DEPTH {
            let TypeShape::Ref { type_id, .. } = shape else {
                break;
            };
            let reference = parse_contract_id(type_id, owner, false)?;
            shape = types
                .get(&reference)
                .map(|(_, shape)| *shape)
                .ok_or_else(|| {
                    Error::new(
                        "E_CONTRACT_SCHEMA",
                        format!("{owner} references unknown type ID {reference}"),
                    )
                })?;
        }
        let TypeShape::Record { fields } = shape else {
            return Err(Error::new(
                "E_CONTRACT_SCHEMA",
                format!("{owner} traverses a non-record at component {depth}"),
            ));
        };
        let field = fields
            .iter()
            .find(|field| parse_contract_id(&field.id, owner, false).ok() == Some(id))
            .ok_or_else(|| {
                Error::new(
                    "E_CONTRACT_SCHEMA",
                    format!("{owner} references field ID {id} outside its row path"),
                )
            })?;
        names.push(field.name.as_str());
        shape = &field.shape;
    }
    Ok(names.join("."))
}

fn validate_shape(
    shape: &TypeShape,
    types: &BTreeMap<u64, (&str, &TypeShape)>,
    ids: &mut BTreeSet<u64>,
    path: &str,
    depth: usize,
) -> Result<()> {
    if depth >= crate::model::MAX_DEPTH {
        return Err(Error::new(
            "E_LIMIT",
            format!("{path} exceeds the portable type depth limit"),
        ));
    }
    match shape {
        TypeShape::Int { min, max }
            if min == &i64::MIN.to_string() && max == &i64::MAX.to_string() => {}
        TypeShape::Float {
            bits: 64,
            finite_only: true,
        }
        | TypeShape::Bool => {}
        TypeShape::Text { encoding } if encoding == "utf-8" => {}
        TypeShape::Uuid { bits: 128 } => {}
        TypeShape::Date {
            unit,
            min_days,
            max_days,
        } if unit == "day"
            && min_days == &crate::scalars::Date::MIN_DAYS.to_string()
            && max_days == &crate::scalars::Date::MAX_DAYS.to_string() => {}
        TypeShape::Timestamp {
            unit,
            epoch,
            normalized,
            min_microseconds,
            max_microseconds,
        } if unit == "microsecond"
            && epoch == "unix"
            && normalized == "utc"
            && min_microseconds == &timestamp_min_microseconds().to_string()
            && max_microseconds == &timestamp_max_microseconds().to_string() => {}
        TypeShape::Duration { unit, min, max }
            if unit == "microsecond"
                && min == &i64::MIN.to_string()
                && max == &i64::MAX.to_string() => {}
        TypeShape::Decimal {
            precision,
            scale,
            coefficient_encoding,
        } if coefficient_encoding == "signed_base_10_string" => {
            crate::scalars::validate_decimal_type(*precision, *scale)?;
        }
        TypeShape::Bytes {
            wire_encoding,
            max_bytes,
        } if wire_encoding == "base64url_no_padding" && *max_bytes == crate::scalars::MAX_BYTES => {
        }
        TypeShape::Sum { variants } => {
            let mut names = BTreeSet::new();
            for variant in variants {
                insert_contract_id(
                    ids,
                    &variant.id,
                    &format!("{path}.variant {}", variant.name),
                )?;
                if !names.insert(variant.name.as_str()) {
                    return Err(Error::new(
                        "E_CONTRACT_SCHEMA",
                        format!("{path} repeats variant '{}'", variant.name),
                    ));
                }
                for (index, payload) in variant.payload.iter().enumerate() {
                    validate_shape(
                        payload,
                        types,
                        ids,
                        &format!("{path}.{}.payload[{index}]", variant.name),
                        depth + 1,
                    )?;
                }
            }
        }
        TypeShape::Record { fields } => {
            let mut names = BTreeSet::new();
            for field in fields {
                insert_contract_id(ids, &field.id, &format!("{path}.field {}", field.name))?;
                if !names.insert(field.name.as_str()) {
                    return Err(Error::new(
                        "E_CONTRACT_SCHEMA",
                        format!("{path} repeats field '{}'", field.name),
                    ));
                }
                if field.input_omittable != field.default.is_some() {
                    return Err(Error::new(
                        "E_CONTRACT_SCHEMA",
                        format!(
                            "{path}.{} must mark input_omittable exactly when a default exists",
                            field.name
                        ),
                    ));
                }
                validate_shape(
                    &field.shape,
                    types,
                    ids,
                    &format!("{path}.{}", field.name),
                    depth + 1,
                )?;
            }
        }
        TypeShape::Tuple { items } => {
            for (index, item) in items.iter().enumerate() {
                validate_shape(item, types, ids, &format!("{path}[{index}]"), depth + 1)?;
            }
        }
        TypeShape::Option { item } => {
            validate_shape(item, types, ids, &format!("{path}.item"), depth + 1)?;
        }
        TypeShape::List { item, max_items } if *max_items == crate::codec::MAX_COLLECTION_ITEMS => {
            validate_shape(item, types, ids, &format!("{path}.item"), depth + 1)?;
        }
        TypeShape::Map {
            key,
            value,
            max_entries,
            max_key_bytes,
        } if **key
            == TypeShape::Text {
                encoding: "utf-8".into(),
            }
            && *max_entries == crate::model::MAX_MAP_ENTRIES
            && *max_key_bytes == crate::model::MAX_MAP_KEY_BYTES =>
        {
            validate_shape(value, types, ids, &format!("{path}.value"), depth + 1)?;
        }
        TypeShape::Ref { type_id, name } => {
            let id = parse_contract_id(type_id, path, false)?;
            if types.get(&id).map(|(name, _)| *name) != Some(name.as_str()) {
                return Err(Error::new(
                    "E_CONTRACT_SCHEMA",
                    format!("{path} references unknown or mismatched type {type_id} '{name}'"),
                ));
            }
        }
        _ => {
            return Err(Error::new(
                "E_CONTRACT_SCHEMA",
                format!("{path} has noncanonical scalar or container constraints"),
            ));
        }
    }
    Ok(())
}

fn compare_shape(
    baseline: &TypeShape,
    candidate: &TypeShape,
    path: &str,
    report: &mut EvolutionReport,
) {
    match (baseline, candidate) {
        (TypeShape::Record { fields: left }, TypeShape::Record { fields: right }) => {
            compare_fields(left, right, path, report);
        }
        (TypeShape::Sum { variants: left }, TypeShape::Sum { variants: right }) => {
            compare_variants(left, right, path, report);
        }
        (TypeShape::Tuple { items: left }, TypeShape::Tuple { items: right })
            if left.len() == right.len() =>
        {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                compare_shape(left, right, &format!("{path}[{index}]"), report);
            }
        }
        (TypeShape::Option { item: left }, TypeShape::Option { item: right })
        | (TypeShape::List { item: left, .. }, TypeShape::List { item: right, .. }) => {
            compare_shape(left, right, &format!("{path}.item"), report);
        }
        (TypeShape::Map { value: left, .. }, TypeShape::Map { value: right, .. }) => {
            compare_shape(left, right, &format!("{path}.value"), report);
        }
        (TypeShape::Ref { type_id: left, .. }, TypeShape::Ref { type_id: right, .. })
            if left == right => {}
        _ if baseline == candidate => {}
        _ => report.break_all(
            "type_shape_changed",
            path,
            "the value shape or scalar constraint changed".into(),
        ),
    }
}

fn compare_fields(
    baseline: &[FieldDescription],
    candidate: &[FieldDescription],
    path: &str,
    report: &mut EvolutionReport,
) {
    let right = candidate
        .iter()
        .map(|field| (field.id.as_str(), field))
        .collect::<BTreeMap<_, _>>();
    for field in baseline {
        let field_path = format!("{path}.field#{}", field.id);
        let Some(next) = right.get(field.id.as_str()) else {
            report.break_all(
                "field_removed",
                &field_path,
                format!("field '{}' was removed", field.name),
            );
            continue;
        };
        if field.name != next.name {
            report.break_names(
                "field_renamed",
                &field_path,
                format!("field '{}' was renamed to '{}'", field.name, next.name),
            );
        }
        compare_shape(&field.shape, &next.shape, &field_path, report);
        compare_field_default(field, next, &field_path, report);
    }
    let left_order = baseline.iter().map(|field| &field.id).collect::<Vec<_>>();
    let right_order = candidate
        .iter()
        .filter(|field| left_order.contains(&&field.id))
        .map(|field| &field.id)
        .collect::<Vec<_>>();
    let retained_left = left_order
        .into_iter()
        .filter(|id| right_order.contains(id))
        .collect::<Vec<_>>();
    if retained_left != right_order {
        report.query.add(
            CompatibilityLevel::Conditional,
            "field_order_changed",
            path,
            "record field order changed; complete-row column order requires regeneration",
        );
        report.client_read.add(
            CompatibilityLevel::Conditional,
            "field_order_changed",
            path,
            "record field order changed; positional host mappings require review",
        );
    }
    let left_ids = baseline
        .iter()
        .map(|field| field.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for field in candidate
        .iter()
        .filter(|field| !left_ids.contains(field.id.as_str()))
    {
        let field_path = format!("{path}.field#{}", field.id);
        let message = format!("field '{}' was added", field.name);
        report.query.add(
            CompatibilityLevel::Conditional,
            "field_added",
            &field_path,
            format!("{message}; explicit projections remain valid but complete-row results change"),
        );
        report.client_read.add(
            CompatibilityLevel::Conditional,
            "field_added",
            &field_path,
            format!("{message}; older complete-row clients may reject the new field"),
        );
        if !field.input_omittable {
            report.existing_data.add(
                CompatibilityLevel::Incompatible,
                "required_field_added",
                &field_path,
                format!("{message} without a default"),
            );
            report.client_write.add(
                CompatibilityLevel::Incompatible,
                "required_field_added",
                &field_path,
                format!("{message} without a default; older writers omit it"),
            );
        }
    }
}

fn compare_field_default(
    baseline: &FieldDescription,
    candidate: &FieldDescription,
    path: &str,
    report: &mut EvolutionReport,
) {
    match (&baseline.default, &candidate.default) {
        (Some(_), None) => report.client_write.add(
            CompatibilityLevel::Incompatible,
            "field_default_removed",
            path,
            format!(
                "field '{}' no longer has a default; older writers may omit it",
                candidate.name
            ),
        ),
        (Some(left), Some(right)) if left != right => report.client_write.add(
            CompatibilityLevel::Conditional,
            "field_default_changed",
            path,
            format!(
                "field '{}' changed its default; omitted values now normalize differently",
                candidate.name
            ),
        ),
        _ => {}
    }
}

fn compare_variants(
    baseline: &[VariantDescription],
    candidate: &[VariantDescription],
    path: &str,
    report: &mut EvolutionReport,
) {
    let right = candidate
        .iter()
        .map(|variant| (variant.id.as_str(), variant))
        .collect::<BTreeMap<_, _>>();
    for variant in baseline {
        let variant_path = format!("{path}.variant#{}", variant.id);
        let Some(next) = right.get(variant.id.as_str()) else {
            report.break_all(
                "variant_removed",
                &variant_path,
                format!("variant '{}' was removed", variant.name),
            );
            continue;
        };
        if variant.name != next.name {
            report.break_names(
                "variant_renamed",
                &variant_path,
                format!("variant '{}' was renamed to '{}'", variant.name, next.name),
            );
        }
        if variant.payload.len() != next.payload.len() {
            report.break_all(
                "variant_payload_changed",
                &variant_path,
                format!("variant '{}' changed payload arity", next.name),
            );
        } else {
            for (index, (left, right)) in variant.payload.iter().zip(&next.payload).enumerate() {
                compare_shape(
                    left,
                    right,
                    &format!("{variant_path}.payload[{index}]"),
                    report,
                );
            }
        }
    }
    let left_ids = baseline
        .iter()
        .map(|variant| variant.id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for variant in candidate
        .iter()
        .filter(|variant| !left_ids.contains(variant.id.as_str()))
    {
        let variant_path = format!("{path}.variant#{}", variant.id);
        report.query.add(
            CompatibilityLevel::Conditional,
            "variant_added",
            &variant_path,
            format!(
                "variant '{}' was added; exhaustive matches require regeneration",
                variant.name
            ),
        );
        report.client_read.add(
            CompatibilityLevel::Conditional,
            "variant_added",
            &variant_path,
            format!(
                "variant '{}' may be returned to a client that cannot decode it",
                variant.name
            ),
        );
    }
}

fn compare_tables(
    baseline: &SchemaDescription,
    candidate: &SchemaDescription,
    report: &mut EvolutionReport,
) {
    let right = candidate
        .tables
        .iter()
        .map(|table| (table.id.as_str(), table))
        .collect::<BTreeMap<_, _>>();
    for table in &baseline.tables {
        let path = format!("table#{}", table.id);
        let Some(next) = right.get(table.id.as_str()) else {
            report.break_all(
                "table_removed",
                &path,
                format!("table '{}' was removed", table.name),
            );
            continue;
        };
        if table.name != next.name {
            report.break_names(
                "table_renamed",
                &path,
                format!("table '{}' was renamed to '{}'", table.name, next.name),
            );
        }
        compare_shape(&table.row, &next.row, &format!("{path}.row"), report);
        if table.primary_key != next.primary_key {
            report.query.add(
                CompatibilityLevel::Conditional,
                "primary_key_changed",
                &path,
                "the primary key changed; lookup and pagination contracts require regeneration",
            );
            report.client_write.add(
                CompatibilityLevel::Conditional,
                "primary_key_changed",
                &path,
                "the primary key changed; upsert behavior must be reviewed",
            );
        }
        compare_unique_constraints(&table.indexes, &next.indexes, &path, report);
    }
}

fn compare_unique_constraints(
    baseline: &[IndexDescription],
    candidate: &[IndexDescription],
    table_path: &str,
    report: &mut EvolutionReport,
) {
    let baseline = baseline
        .iter()
        .map(|index| (index.id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    for index in candidate.iter().filter(|index| index.unique) {
        match baseline.get(index.id.as_str()) {
            None => report.client_write.add(
                CompatibilityLevel::Incompatible,
                "unique_index_added",
                format!("{table_path}.index#{}", index.id),
                "a new unique constraint can reject writes accepted by the baseline",
            ),
            Some(previous) if !previous.unique => report.client_write.add(
                CompatibilityLevel::Incompatible,
                "unique_index_tightened",
                format!("{table_path}.index#{}", index.id),
                "an ordinary index became unique and can reject writes accepted by the baseline",
            ),
            _ => {}
        }
    }
}

/// A description paired with the existing catalog validator.
#[derive(Debug, Clone)]
pub struct PortableContract {
    description: SchemaDescription,
    catalog: Catalog,
}

impl PortableContract {
    pub fn from_source(source: &str) -> Result<Self> {
        Self::from_database(&crate::schema::parse(source)?)
    }

    pub fn description(&self) -> &SchemaDescription {
        &self.description
    }

    pub fn into_description(self) -> SchemaDescription {
        self.description
    }

    /// Validate and normalize a value with the same rules as prepared binding.
    pub fn validate_type(&self, type_name: &str, value: WireValue) -> Result<WireValue> {
        let definition = self.catalog.types.get(type_name).ok_or_else(|| {
            Error::new(
                "E_TYPE",
                format!("portable contract has no type '{type_name}'"),
            )
        })?;
        let value = crate::Value::try_from(value)?;
        let normalized = self.catalog.coerce(
            &value,
            &ScalarType::Ref(definition.id),
            &format!("type {type_name}"),
        )?;
        Ok(WireValue::from(&normalized))
    }

    pub(crate) fn from_database(database: &Database) -> Result<Self> {
        Ok(Self {
            description: describe_database(database)?,
            catalog: database.catalog.clone(),
        })
    }
}

pub fn describe(source: &str) -> Result<SchemaDescription> {
    PortableContract::from_source(source).map(PortableContract::into_description)
}

fn describe_database(database: &Database) -> Result<SchemaDescription> {
    let catalog = &database.catalog;
    let mut types = catalog.types.values().collect::<Vec<_>>();
    types.sort_by_key(|definition| definition.id);
    let types = types
        .into_iter()
        .map(|definition| {
            Ok(TypeDescription {
                id: definition.id.to_string(),
                name: definition.name.clone(),
                shape: describe_type(catalog, &definition.ty)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut indexes = BTreeMap::<u64, Vec<IndexDescription>>::new();
    for (_, definition) in database.schema_indexes() {
        indexes
            .entry(definition.table_id)
            .or_default()
            .push(IndexDescription {
                id: definition.id.to_string(),
                unique: definition.kind == IndexKind::Unique,
                components: definition
                    .effective_components()
                    .into_iter()
                    .map(|component| IndexComponentDescription {
                        field: component.column,
                        field_path: component
                            .field_path
                            .into_iter()
                            .map(|id| id.to_string())
                            .collect(),
                        descending: component.descending,
                    })
                    .collect(),
                predicate: definition.display_predicate(),
            });
    }
    for definitions in indexes.values_mut() {
        definitions.sort_by_key(|definition| definition.id.parse::<u64>().unwrap_or(u64::MAX));
    }

    let mut tables = database.schema_tables();
    tables.sort_by_key(|table| table.id);
    let tables = tables
        .into_iter()
        .map(|table| {
            let row = match table.row_type {
                Some(type_id) => describe_type(catalog, &ScalarType::Ref(type_id))?,
                None => TypeShape::Record {
                    fields: describe_fields(catalog, &table.schema)?,
                },
            };
            Ok(TableDescription {
                id: table.id.to_string(),
                name: table.name.clone(),
                row,
                primary_key: table
                    .primary_key
                    .as_ref()
                    .map(|field| {
                        Ok(KeyDescription {
                            field: field.clone(),
                            field_path: catalog
                                .field_path_ids(&table.schema, field)?
                                .into_iter()
                                .map(|id| id.to_string())
                                .collect(),
                        })
                    })
                    .transpose()?,
                indexes: indexes.remove(&table.id).unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let description = SchemaDescription {
        version: DESCRIPTION_VERSION,
        id_scope: IdScope {
            kind: "database_local_catalog".into(),
            encoding: "unsigned_64_bit_decimal_string".into(),
            globally_stable: false,
        },
        schema: database.schema_info().into(),
        types,
        tables,
    };
    description.validate()?;
    Ok(description)
}

pub(crate) fn describe_type(catalog: &Catalog, ty: &ScalarType) -> Result<TypeShape> {
    Ok(match ty {
        ScalarType::Int => TypeShape::Int {
            min: i64::MIN.to_string(),
            max: i64::MAX.to_string(),
        },
        ScalarType::Float => TypeShape::Float {
            bits: 64,
            finite_only: true,
        },
        ScalarType::Bool => TypeShape::Bool,
        ScalarType::Text => TypeShape::Text {
            encoding: "utf-8".into(),
        },
        ScalarType::Uuid => TypeShape::Uuid { bits: 128 },
        ScalarType::Date => TypeShape::Date {
            unit: "day".into(),
            min_days: crate::scalars::Date::MIN_DAYS.to_string(),
            max_days: crate::scalars::Date::MAX_DAYS.to_string(),
        },
        ScalarType::Timestamp => TypeShape::Timestamp {
            unit: "microsecond".into(),
            epoch: "unix".into(),
            normalized: "utc".into(),
            min_microseconds: timestamp_min_microseconds().to_string(),
            max_microseconds: timestamp_max_microseconds().to_string(),
        },
        ScalarType::Duration => TypeShape::Duration {
            unit: "microsecond".into(),
            min: i64::MIN.to_string(),
            max: i64::MAX.to_string(),
        },
        ScalarType::Decimal { precision, scale } => TypeShape::Decimal {
            precision: *precision,
            scale: *scale,
            coefficient_encoding: "signed_base_10_string".into(),
        },
        ScalarType::Bytes => TypeShape::Bytes {
            wire_encoding: "base64url_no_padding".into(),
            max_bytes: crate::scalars::MAX_BYTES,
        },
        ScalarType::Enum(sum) => describe_sum(catalog, sum)?,
        ScalarType::Record(fields) => TypeShape::Record {
            fields: describe_fields(catalog, fields)?,
        },
        ScalarType::Tuple(items) => TypeShape::Tuple {
            items: items
                .iter()
                .map(|item| describe_type(catalog, item))
                .collect::<Result<_>>()?,
        },
        ScalarType::Option(item) => TypeShape::Option {
            item: Box::new(describe_type(catalog, item)?),
        },
        ScalarType::List(item) => TypeShape::List {
            item: Box::new(describe_type(catalog, item)?),
            max_items: crate::codec::MAX_COLLECTION_ITEMS,
        },
        ScalarType::Map(value) => TypeShape::Map {
            key: Box::new(TypeShape::Text {
                encoding: "utf-8".into(),
            }),
            value: Box::new(describe_type(catalog, value)?),
            max_entries: crate::model::MAX_MAP_ENTRIES,
            max_key_bytes: crate::model::MAX_MAP_KEY_BYTES,
        },
        ScalarType::Ref(type_id) => {
            let definition = catalog.definition(*type_id)?;
            TypeShape::Ref {
                type_id: type_id.to_string(),
                name: definition.name.clone(),
            }
        }
        ScalarType::Named(name) => {
            return Err(Error::new(
                "E_SCHEMA",
                format!("unresolved type '{name}' in portable description"),
            ));
        }
    })
}

fn timestamp_min_microseconds() -> i64 {
    i64::from(crate::scalars::Date::MIN_DAYS) * 86_400_000_000
}

fn timestamp_max_microseconds() -> i64 {
    (i64::from(crate::scalars::Date::MAX_DAYS) + 1) * 86_400_000_000 - 1
}

fn describe_sum(catalog: &Catalog, sum: &EnumType) -> Result<TypeShape> {
    Ok(TypeShape::Sum {
        variants: sum
            .variants
            .iter()
            .map(|variant| {
                Ok(VariantDescription {
                    id: variant.id.to_string(),
                    name: variant.name.clone(),
                    payload: variant
                        .args
                        .iter()
                        .map(|argument| describe_type(catalog, argument))
                        .collect::<Result<_>>()?,
                })
            })
            .collect::<Result<_>>()?,
    })
}

fn describe_fields(catalog: &Catalog, fields: &[Column]) -> Result<Vec<FieldDescription>> {
    fields
        .iter()
        .map(|field| {
            Ok(FieldDescription {
                id: field.id.to_string(),
                name: field.name.clone(),
                shape: describe_type(catalog, &field.ty)?,
                input_omittable: field.default.is_some(),
                default: field.default.as_ref().map(WireValue::from),
            })
        })
        .collect()
}
