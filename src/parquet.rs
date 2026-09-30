//! Bounded, local-only inspection of one Parquet file.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

use arrow_array::{
    Array, BinaryArray, BooleanArray, Date32Array, Date64Array, Decimal128Array,
    DurationMicrosecondArray, DurationMillisecondArray, DurationNanosecondArray,
    DurationSecondArray, FixedSizeBinaryArray, FixedSizeListArray, Float32Array, Float64Array,
    Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray, LargeListArray,
    LargeStringArray, ListArray, MapArray, StringArray, StructArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow_schema::{DataType, Field, TimeUnit};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::model::Value;
use crate::scalars::{Bytes, Date, Decimal, Duration, Timestamp};

pub const DEFAULT_PREVIEW_ROWS: usize = 20;
pub const MAX_PREVIEW_ROWS: usize = 1_000;
const MAX_PREVIEW_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ParquetColumn {
    pub name: String,
    pub r#type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParquetInspection {
    pub version: u32,
    pub path: String,
    pub rows_total: u64,
    pub row_groups: usize,
    pub columns: Vec<ParquetColumn>,
    pub preview_rows: Vec<BTreeMap<String, Value>>,
    pub preview_truncated: bool,
}

pub fn inspect(path: &Path, limit: usize) -> Result<ParquetInspection> {
    if limit > MAX_PREVIEW_ROWS {
        return Err(Error::new(
            "E_LIMIT",
            format!("Parquet preview limit exceeds {MAX_PREVIEW_ROWS} rows"),
        )
        .with_hint("use a limit between 0 and 1000"));
    }
    let file = File::open(path).map_err(|error| {
        Error::new(
            "E_PARQUET_IO",
            format!("open '{}': {error}", path.display()),
        )
        .with_hint("check that the local path exists and is readable")
    })?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(|error| {
        Error::new(
            "E_PARQUET_FORMAT",
            format!("read Parquet metadata from '{}': {error}", path.display()),
        )
        .with_hint("verify that the path is a complete local Parquet file")
    })?;
    let rows_total =
        u64::try_from(builder.metadata().file_metadata().num_rows()).map_err(|_| {
            Error::new(
                "E_PARQUET_FORMAT",
                "Parquet metadata contains a negative row count",
            )
        })?;
    let row_groups = builder.metadata().num_row_groups();
    let schema = builder.schema().clone();
    let columns = schema
        .fields()
        .iter()
        .map(|field| {
            Ok(ParquetColumn {
                name: field.name().clone(),
                r#type: unionid_type(field, field.name())?,
                nullable: field.is_nullable(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut preview_rows = Vec::with_capacity(limit.min(rows_total as usize));
    let mut preview_bytes = 0_usize;
    if limit > 0 {
        let reader = builder
            .with_batch_size(limit.min(1_024))
            .with_limit(limit)
            .build()
            .map_err(|error| parquet_error(path, "build reader", error))?;
        for batch in reader {
            let batch = batch.map_err(|error| parquet_error(path, "decode rows", error))?;
            for row_index in 0..batch.num_rows() {
                let mut row = BTreeMap::new();
                for (column_index, field) in schema.fields().iter().enumerate() {
                    row.insert(
                        field.name().clone(),
                        value_at(
                            batch.column(column_index).as_ref(),
                            field,
                            row_index,
                            field.name(),
                        )?,
                    );
                }
                preview_bytes = preview_bytes
                    .checked_add(
                        serde_json::to_vec(&row)
                            .map_err(|error| {
                                Error::new(
                                    "E_PARQUET_VALUE",
                                    format!("measure preview row: {error}"),
                                )
                            })?
                            .len(),
                    )
                    .ok_or_else(|| Error::new("E_LIMIT", "Parquet preview size overflow"))?;
                if preview_bytes > MAX_PREVIEW_BYTES {
                    return Err(Error::new(
                        "E_LIMIT",
                        "Parquet preview exceeds the 64 MiB working-memory limit",
                    )
                    .with_hint("request fewer preview rows"));
                }
                preview_rows.push(row);
            }
        }
    }
    Ok(ParquetInspection {
        version: 1,
        path: path.display().to_string(),
        rows_total,
        row_groups,
        columns,
        preview_truncated: rows_total > preview_rows.len() as u64,
        preview_rows,
    })
}

fn parquet_error(path: &Path, action: &str, error: impl std::fmt::Display) -> Error {
    Error::new(
        "E_PARQUET_FORMAT",
        format!("{action} from '{}': {error}", path.display()),
    )
}

fn unionid_type(field: &Field, path: &str) -> Result<String> {
    let inner = match field.data_type() {
        DataType::Boolean => "bool".into(),
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => "int".into(),
        DataType::Float32 | DataType::Float64 => "float".into(),
        DataType::Utf8 | DataType::LargeUtf8 => "text".into(),
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => "bytes".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Timestamp(_, timezone) if utc_timezone(timezone.as_deref()) => "timestamp".into(),
        DataType::Duration(_) => "duration".into(),
        DataType::Decimal128(precision, scale) if *scale >= 0 && *scale <= *precision as i8 => {
            format!("Decimal<{precision}, {scale}>")
        }
        DataType::Struct(fields) => {
            let members = fields
                .iter()
                .map(|child| {
                    let child_path = format!("{path}.{}", child.name());
                    Ok(format!(
                        "{} {}",
                        child.name(),
                        unionid_type(child, &child_path)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            format!("{{ {} }}", members.join(", "))
        }
        DataType::List(item) | DataType::LargeList(item) | DataType::FixedSizeList(item, _) => {
            format!("List<{}>", unionid_type(item, &format!("{path}[]"))?)
        }
        DataType::Map(entries, _) => {
            let DataType::Struct(fields) = entries.data_type() else {
                return Err(unsupported(
                    path,
                    entries.data_type(),
                    "map entries must be a struct",
                ));
            };
            if fields.len() != 2
                || !matches!(fields[0].data_type(), DataType::Utf8 | DataType::LargeUtf8)
            {
                return Err(unsupported(
                    path,
                    entries.data_type(),
                    "map keys must be text",
                ));
            }
            format!(
                "Map<text, {}>",
                unionid_type(&fields[1], &format!("{path}{{}}"))?
            )
        }
        DataType::Timestamp(_, _) => {
            return Err(unsupported(
                path,
                field.data_type(),
                "timestamp requires UTC timezone metadata",
            ));
        }
        other => return Err(unsupported(path, other, "no lossless UnionID mapping")),
    };
    Ok(if field.is_nullable() {
        format!("Option<{inner}>")
    } else {
        inner
    })
}

fn unsupported(path: &str, ty: &DataType, reason: &str) -> Error {
    Error::new(
        "E_PARQUET_TYPE",
        format!("Parquet field '{path}' uses unsupported type {ty:?}: {reason}"),
    )
    .with_hint("project or convert this field to a supported Parquet logical type")
}

fn utc_timezone(timezone: Option<&str>) -> bool {
    matches!(timezone, Some("UTC" | "Etc/UTC" | "+00:00" | "Z"))
}

fn value_at(array: &dyn Array, field: &Field, index: usize, path: &str) -> Result<Value> {
    if array.is_null(index) {
        return if field.is_nullable() {
            Ok(Value::Option(None))
        } else {
            Err(Error::new(
                "E_PARQUET_VALUE",
                format!("non-nullable Parquet field '{path}' contains null"),
            ))
        };
    }
    let value = non_null_value(array, field.data_type(), index, path)?;
    Ok(if field.is_nullable() {
        Value::Option(Some(Box::new(value)))
    } else {
        value
    })
}

macro_rules! primitive {
    ($array:expr, $index:expr, $path:expr, $ty:ty, $map:expr) => {{
        let values = $array.as_any().downcast_ref::<$ty>().ok_or_else(|| {
            Error::new(
                "E_PARQUET_FORMAT",
                format!("array type mismatch at '{}'", $path),
            )
        })?;
        $map(values.value($index))
    }};
}

fn non_null_value(array: &dyn Array, ty: &DataType, index: usize, path: &str) -> Result<Value> {
    let value = match ty {
        DataType::Boolean => primitive!(array, index, path, BooleanArray, Value::Bool),
        DataType::Int8 => primitive!(array, index, path, Int8Array, |v: i8| Value::Int(v.into())),
        DataType::Int16 => primitive!(array, index, path, Int16Array, |v: i16| Value::Int(
            v.into()
        )),
        DataType::Int32 => primitive!(array, index, path, Int32Array, |v: i32| Value::Int(
            v.into()
        )),
        DataType::Int64 => primitive!(array, index, path, Int64Array, Value::Int),
        DataType::UInt8 => primitive!(array, index, path, UInt8Array, |v: u8| Value::Int(v.into())),
        DataType::UInt16 => primitive!(array, index, path, UInt16Array, |v: u16| Value::Int(
            v.into()
        )),
        DataType::UInt32 => primitive!(array, index, path, UInt32Array, |v: u32| Value::Int(
            v.into()
        )),
        DataType::UInt64 => primitive!(array, index, path, UInt64Array, |v: u64| {
            i64::try_from(v).map(Value::Int).map_err(|_| {
                Error::new(
                    "E_PARQUET_VALUE",
                    format!("unsigned integer at '{path}' exceeds the UnionID int range"),
                )
            })
        })?,
        DataType::Float32 => primitive!(array, index, path, Float32Array, |v: f32| Value::Float(
            v.into()
        )),
        DataType::Float64 => primitive!(array, index, path, Float64Array, Value::Float),
        DataType::Utf8 => primitive!(array, index, path, StringArray, |v: &str| Value::Text(
            v.into()
        )),
        DataType::LargeUtf8 => {
            primitive!(array, index, path, LargeStringArray, |v: &str| Value::Text(
                v.into()
            ))
        }
        DataType::Binary => primitive!(array, index, path, BinaryArray, |v: &[u8]| Bytes::new(
            v.to_vec()
        )
        .map(Value::Bytes))?,
        DataType::LargeBinary => {
            primitive!(array, index, path, LargeBinaryArray, |v: &[u8]| Bytes::new(
                v.to_vec()
            )
            .map(Value::Bytes))?
        }
        DataType::FixedSizeBinary(_) => {
            primitive!(array, index, path, FixedSizeBinaryArray, |v: &[u8]| {
                Bytes::new(v.to_vec()).map(Value::Bytes)
            })?
        }
        DataType::Date32 => primitive!(array, index, path, Date32Array, |v: i32| {
            Date::from_epoch_days(v).map(Value::Date)
        })?,
        DataType::Date64 => primitive!(array, index, path, Date64Array, |v: i64| date64(v, path))?,
        DataType::Timestamp(unit, timezone) if utc_timezone(timezone.as_deref()) => {
            match unit {
                TimeUnit::Second => primitive!(
                    array,
                    index,
                    path,
                    TimestampSecondArray,
                    |v: i64| timestamp(v, 1_000_000, path)
                )?,
                TimeUnit::Millisecond => {
                    primitive!(array, index, path, TimestampMillisecondArray, |v: i64| {
                        timestamp(v, 1_000, path)
                    })?
                }
                TimeUnit::Microsecond => {
                    primitive!(array, index, path, TimestampMicrosecondArray, |v: i64| {
                        Timestamp::from_epoch_microseconds(v).map(Value::Timestamp)
                    })?
                }
                TimeUnit::Nanosecond => {
                    primitive!(array, index, path, TimestampNanosecondArray, |v: i64| {
                        if v % 1_000 != 0 {
                            Err(Error::new(
                                "E_PARQUET_VALUE",
                                format!("timestamp at '{path}' exceeds microsecond precision"),
                            ))
                        } else {
                            Timestamp::from_epoch_microseconds(v / 1_000).map(Value::Timestamp)
                        }
                    })?
                }
            }
        }
        DataType::Duration(unit) => match unit {
            TimeUnit::Second => {
                primitive!(array, index, path, DurationSecondArray, |v: i64| duration(
                    v, 1_000_000, path
                ))?
            }
            TimeUnit::Millisecond => {
                primitive!(array, index, path, DurationMillisecondArray, |v: i64| {
                    duration(v, 1_000, path)
                })?
            }
            TimeUnit::Microsecond => {
                primitive!(array, index, path, DurationMicrosecondArray, |v: i64| Ok::<
                    _,
                    Error,
                >(
                    Value::Duration(Duration::from_microseconds(v))
                ))?
            }
            TimeUnit::Nanosecond => {
                primitive!(array, index, path, DurationNanosecondArray, |v: i64| {
                    if v % 1_000 != 0 {
                        Err(Error::new(
                            "E_PARQUET_VALUE",
                            format!("duration at '{path}' exceeds microsecond precision"),
                        ))
                    } else {
                        Ok(Value::Duration(Duration::from_microseconds(v / 1_000)))
                    }
                })?
            }
        },
        DataType::Decimal128(precision, scale) if *scale >= 0 => {
            primitive!(array, index, path, Decimal128Array, |v: i128| {
                Decimal::new(v, *precision, *scale as u8).map(Value::Decimal)
            })?
        }
        DataType::Struct(fields) => {
            let values = array
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| {
                    Error::new(
                        "E_PARQUET_FORMAT",
                        format!("array type mismatch at '{path}'"),
                    )
                })?;
            let mut record = BTreeMap::new();
            for (column, child) in values.columns().iter().zip(fields) {
                let child_path = format!("{path}.{}", child.name());
                record.insert(
                    child.name().clone(),
                    value_at(column.as_ref(), child, index, &child_path)?,
                );
            }
            Value::Record(record)
        }
        DataType::List(item) => list_value::<ListArray>(array, item, index, path)?,
        DataType::LargeList(item) => list_value::<LargeListArray>(array, item, index, path)?,
        DataType::FixedSizeList(item, _) => {
            list_value::<FixedSizeListArray>(array, item, index, path)?
        }
        DataType::Map(entries, _) => map_value(array, entries, index, path)?,
        other => return Err(unsupported(path, other, "no lossless UnionID mapping")),
    };
    Ok(value)
}

trait ListValues: Array {
    fn list_value(&self, index: usize) -> arrow_array::ArrayRef;
}

impl ListValues for ListArray {
    fn list_value(&self, index: usize) -> arrow_array::ArrayRef {
        self.value(index)
    }
}
impl ListValues for LargeListArray {
    fn list_value(&self, index: usize) -> arrow_array::ArrayRef {
        self.value(index)
    }
}
impl ListValues for FixedSizeListArray {
    fn list_value(&self, index: usize) -> arrow_array::ArrayRef {
        self.value(index)
    }
}

fn list_value<T: ListValues + 'static>(
    array: &dyn Array,
    item: &Field,
    index: usize,
    path: &str,
) -> Result<Value> {
    let list = array.as_any().downcast_ref::<T>().ok_or_else(|| {
        Error::new(
            "E_PARQUET_FORMAT",
            format!("array type mismatch at '{path}'"),
        )
    })?;
    let values = list.list_value(index);
    let items = (0..values.len())
        .map(|item_index| value_at(values.as_ref(), item, item_index, &format!("{path}[]")))
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::List(items))
}

fn map_value(array: &dyn Array, entries: &Field, index: usize, path: &str) -> Result<Value> {
    let map = array.as_any().downcast_ref::<MapArray>().ok_or_else(|| {
        Error::new(
            "E_PARQUET_FORMAT",
            format!("array type mismatch at '{path}'"),
        )
    })?;
    let DataType::Struct(fields) = entries.data_type() else {
        return Err(unsupported(
            path,
            entries.data_type(),
            "map entries must be a struct",
        ));
    };
    let values = map.value(index);
    let mut output = BTreeMap::new();
    for row in 0..values.len() {
        let key = string_key(values.column(0).as_ref(), fields[0].data_type(), row, path)?;
        let value = value_at(
            values.column(1).as_ref(),
            &fields[1],
            row,
            &format!("{path}{{}}"),
        )?;
        if output.insert(key, value).is_some() {
            return Err(Error::new(
                "E_PARQUET_VALUE",
                format!("map at '{path}' contains duplicate text keys"),
            ));
        }
    }
    Ok(Value::Map(output))
}

fn string_key(array: &dyn Array, ty: &DataType, index: usize, path: &str) -> Result<String> {
    if array.is_null(index) {
        return Err(Error::new(
            "E_PARQUET_VALUE",
            format!("map at '{path}' contains a null key"),
        ));
    }
    match ty {
        DataType::Utf8 => Ok(array
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("validated string array")
            .value(index)
            .into()),
        DataType::LargeUtf8 => Ok(array
            .as_any()
            .downcast_ref::<LargeStringArray>()
            .expect("validated string array")
            .value(index)
            .into()),
        other => Err(unsupported(path, other, "map keys must be text")),
    }
}

fn timestamp(value: i64, multiplier: i64, path: &str) -> Result<Value> {
    value
        .checked_mul(multiplier)
        .ok_or_else(|| {
            Error::new(
                "E_PARQUET_VALUE",
                format!("timestamp at '{path}' overflows microseconds"),
            )
        })
        .and_then(Timestamp::from_epoch_microseconds)
        .map(Value::Timestamp)
}

fn duration(value: i64, multiplier: i64, path: &str) -> Result<Value> {
    value
        .checked_mul(multiplier)
        .ok_or_else(|| {
            Error::new(
                "E_PARQUET_VALUE",
                format!("duration at '{path}' overflows microseconds"),
            )
        })
        .map(|value| Value::Duration(Duration::from_microseconds(value)))
}

fn date64(value: i64, path: &str) -> Result<Value> {
    const DAY_MILLIS: i64 = 86_400_000;
    if value % DAY_MILLIS != 0 {
        return Err(Error::new(
            "E_PARQUET_VALUE",
            format!("date at '{path}' is not midnight UTC"),
        ));
    }
    let days = i32::try_from(value / DAY_MILLIS).map_err(|_| {
        Error::new(
            "E_PARQUET_VALUE",
            format!("date at '{path}' exceeds the UnionID date range"),
        )
    })?;
    Date::from_epoch_days(days).map(Value::Date)
}
