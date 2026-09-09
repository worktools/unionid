use crate::error::{Error, Result};
use crate::model::{Catalog, MAX_DEPTH, ScalarType, Value};

pub(crate) const INDEX_KEY_CODEC_VERSION: u16 = 3;
pub(crate) const MAX_COMPLETE_INDEX_KEY_BYTES: usize = 64 * 1024;
const INDEX_MAGIC: &[u8; 4] = b"UIDI";

#[derive(Clone, Copy)]
pub(crate) struct Component<'a> {
    pub(crate) ty: &'a ScalarType,
    pub(crate) value: &'a Value,
    pub(crate) descending: bool,
}

pub(crate) fn encode_tuple(catalog: &Catalog, components: &[Component<'_>]) -> Result<Vec<u8>> {
    if components.is_empty() || components.len() > crate::query::MAX_INDEX_COMPONENTS {
        return Err(Error::new(
            "E_INDEX_SHAPE",
            format!(
                "an index requires 1 to {} fields",
                crate::query::MAX_INDEX_COMPONENTS
            ),
        ));
    }
    let mut output = Vec::new();
    for component in components {
        let start = output.len();
        encode_value(catalog, component.ty, component.value, &mut output, 0)?;
        if component.descending {
            for byte in &mut output[start..] {
                *byte = !*byte;
            }
        }
    }
    Ok(output)
}

pub(crate) fn encode_complete(
    catalog: &Catalog,
    index_id: u64,
    components: &[Component<'_>],
    row_id: u64,
) -> Result<Vec<u8>> {
    let tuple = encode_tuple(catalog, components)?;
    let component_count = u8::try_from(components.len())
        .map_err(|_| Error::new("E_INDEX_SHAPE", "too many index fields"))?;
    let mut output = Vec::with_capacity(15 + tuple.len() + 8);
    output.extend_from_slice(INDEX_MAGIC);
    output.extend_from_slice(&INDEX_KEY_CODEC_VERSION.to_be_bytes());
    output.extend_from_slice(&index_id.to_be_bytes());
    output.push(component_count);
    output.extend_from_slice(&tuple);
    output.extend_from_slice(&row_id.to_be_bytes());
    if output.len() > MAX_COMPLETE_INDEX_KEY_BYTES {
        return Err(Error::new(
            "E_INDEX_KEY_LIMIT",
            format!(
                "encoded secondary index key is {} bytes; limit is {MAX_COMPLETE_INDEX_KEY_BYTES}",
                output.len()
            ),
        ));
    }
    Ok(output)
}

pub(crate) fn validate_complete(key: &[u8]) -> Result<()> {
    if key.len() < 23 || key.len() > MAX_COMPLETE_INDEX_KEY_BYTES || &key[..4] != INDEX_MAGIC {
        return Err(Error::new("E_STORAGE", "invalid secondary index key"));
    }
    let version = u16::from_be_bytes(key[4..6].try_into().unwrap());
    if version != INDEX_KEY_CODEC_VERSION {
        return Err(Error::new(
            "E_STORAGE",
            format!("unsupported secondary index key version {version}"),
        ));
    }
    let count = key[14] as usize;
    if count == 0 || count > crate::query::MAX_INDEX_COMPONENTS {
        return Err(Error::new(
            "E_STORAGE",
            "invalid secondary index component count",
        ));
    }
    Ok(())
}

fn encode_value(
    catalog: &Catalog,
    ty: &ScalarType,
    value: &Value,
    output: &mut Vec<u8>,
    depth: usize,
) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(Error::new(
            "E_LIMIT",
            "secondary index value is too deeply nested",
        ));
    }
    let mismatch = || {
        Error::new(
            "E_TYPE",
            format!(
                "index value does not match bound type '{}'",
                catalog.describe(ty)
            ),
        )
    };
    match ty {
        ScalarType::Int => match value {
            Value::Int(value) => {
                output.extend_from_slice(&((*value as u64) ^ (1_u64 << 63)).to_be_bytes())
            }
            _ => return Err(mismatch()),
        },
        ScalarType::Float => match value {
            Value::Float(value) if value.is_finite() => {
                let bits = if *value == 0.0 {
                    0.0_f64.to_bits()
                } else {
                    value.to_bits()
                };
                let ordered = if bits & (1_u64 << 63) == 0 {
                    bits ^ (1_u64 << 63)
                } else {
                    !bits
                };
                output.extend_from_slice(&ordered.to_be_bytes());
            }
            Value::Float(_) => {
                return Err(Error::new(
                    "E_TYPE",
                    "non-finite float has no typed index order",
                ));
            }
            _ => return Err(mismatch()),
        },
        ScalarType::Bool => match value {
            Value::Bool(value) => output.push(u8::from(*value)),
            _ => return Err(mismatch()),
        },
        ScalarType::Text => match value {
            Value::Text(value) => encode_escaped(value.as_bytes(), output),
            _ => return Err(mismatch()),
        },
        ScalarType::Uuid => match value {
            Value::Uuid(value) => output.extend_from_slice(value.as_bytes()),
            _ => return Err(mismatch()),
        },
        ScalarType::Date => match value {
            Value::Date(value) => output
                .extend_from_slice(&((value.epoch_days() as u32) ^ (1_u32 << 31)).to_be_bytes()),
            _ => return Err(mismatch()),
        },
        ScalarType::Timestamp => match value {
            Value::Timestamp(value) => output.extend_from_slice(
                &((value.epoch_microseconds() as u64) ^ (1_u64 << 63)).to_be_bytes(),
            ),
            _ => return Err(mismatch()),
        },
        ScalarType::Duration => match value {
            Value::Duration(value) => output
                .extend_from_slice(&((value.microseconds() as u64) ^ (1_u64 << 63)).to_be_bytes()),
            _ => return Err(mismatch()),
        },
        ScalarType::Decimal { scale, .. } => match value {
            Value::Decimal(value) if value.scale() == *scale => output.extend_from_slice(
                &((value.coefficient() as u128) ^ (1_u128 << 127)).to_be_bytes(),
            ),
            _ => return Err(mismatch()),
        },
        ScalarType::Bytes => match value {
            Value::Bytes(value) => encode_escaped(value.as_slice(), output),
            _ => return Err(mismatch()),
        },
        ScalarType::Ref(id) => match value {
            Value::Named { type_id, value } if type_id == id => encode_value(
                catalog,
                &catalog.definition(*id)?.ty,
                value,
                output,
                depth + 1,
            )?,
            _ => return Err(mismatch()),
        },
        ScalarType::Enum(sum) => match value {
            Value::Enum(value) => {
                let variant = sum
                    .variants
                    .iter()
                    .find(|variant| variant.id != 0 && variant.id == value.id)
                    .ok_or_else(mismatch)?;
                if value.args.len() != variant.args.len() {
                    return Err(mismatch());
                }
                output.extend_from_slice(&value.id.to_be_bytes());
                for (ty, value) in variant.args.iter().zip(&value.args) {
                    encode_value(catalog, ty, value, output, depth + 1)?;
                }
            }
            _ => return Err(mismatch()),
        },
        ScalarType::Record(columns) => match value {
            Value::Record(values) if values.len() == columns.len() => {
                let mut columns = columns.iter().collect::<Vec<_>>();
                columns.sort_by_key(|column| column.id);
                for column in columns {
                    encode_value(
                        catalog,
                        &column.ty,
                        values.get(&column.name).ok_or_else(mismatch)?,
                        output,
                        depth + 1,
                    )?;
                }
            }
            _ => return Err(mismatch()),
        },
        ScalarType::Tuple(types) => match value {
            Value::Tuple(values) if values.len() == types.len() => {
                for (ty, value) in types.iter().zip(values) {
                    encode_value(catalog, ty, value, output, depth + 1)?;
                }
            }
            _ => return Err(mismatch()),
        },
        ScalarType::Option(inner) => match value {
            Value::Option(None) => output.push(0),
            Value::Option(Some(value)) => {
                output.push(1);
                encode_value(catalog, inner, value, output, depth + 1)?;
            }
            _ => return Err(mismatch()),
        },
        ScalarType::List(inner) => match value {
            Value::List(values) => {
                for value in values {
                    output.push(1);
                    encode_value(catalog, inner, value, output, depth + 1)?;
                }
                output.push(0);
            }
            _ => return Err(mismatch()),
        },
        ScalarType::Named(name) => {
            return Err(Error::new(
                "E_SCHEMA",
                format!("unresolved type '{name}' cannot be indexed"),
            ));
        }
    }
    Ok(())
}

fn encode_escaped(bytes: &[u8], output: &mut Vec<u8>) {
    for byte in bytes {
        if *byte == 0 {
            output.extend_from_slice(&[0, 0xff]);
        } else {
            output.push(*byte);
        }
    }
    output.extend_from_slice(&[0, 0]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Column, EnumType, EnumValue, EnumVariantDef};

    fn assert_order(ty: ScalarType, values: Vec<Value>) {
        let catalog = Catalog::default();
        let encoded = values
            .iter()
            .map(|value| {
                encode_tuple(
                    &catalog,
                    &[Component {
                        ty: &ty,
                        value,
                        descending: false,
                    }],
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        for pair in values.windows(2).zip(encoded.windows(2)) {
            let ([left, right], [left_bytes, right_bytes]) = pair else {
                unreachable!()
            };
            assert!(catalog.cmp_typed(&ty, left, right).unwrap().is_lt());
            assert!(left_bytes < right_bytes);
        }
    }

    #[test]
    fn primitive_and_container_bytes_follow_typed_order() {
        assert_order(
            ScalarType::Int,
            vec![
                Value::Int(i64::MIN),
                Value::Int(-1),
                Value::Int(0),
                Value::Int(i64::MAX),
            ],
        );
        assert_order(
            ScalarType::Text,
            vec![
                Value::Text("".into()),
                Value::Text("a".into()),
                Value::Text("a\0".into()),
            ],
        );
        assert_order(
            ScalarType::Option(Box::new(ScalarType::Int)),
            vec![
                Value::Option(None),
                Value::Option(Some(Box::new(Value::Int(-1)))),
                Value::Option(Some(Box::new(Value::Int(2)))),
            ],
        );
        assert_order(
            ScalarType::List(Box::new(ScalarType::Int)),
            vec![
                Value::List(vec![]),
                Value::List(vec![Value::Int(1)]),
                Value::List(vec![Value::Int(1), Value::Int(0)]),
                Value::List(vec![Value::Int(2)]),
            ],
        );
    }

    #[test]
    fn descending_complements_one_complete_component() {
        let catalog = Catalog::default();
        let ty = ScalarType::Text;
        let low = Value::Text("a".into());
        let high = Value::Text("b".into());
        let encode = |value, descending| {
            encode_tuple(
                &catalog,
                &[Component {
                    ty: &ty,
                    value,
                    descending,
                }],
            )
            .unwrap()
        };
        assert!(encode(&low, false) < encode(&high, false));
        assert!(encode(&low, true) > encode(&high, true));
    }

    #[test]
    fn complete_key_limit_accepts_exact_boundary_and_rejects_next_byte() {
        let catalog = Catalog::default();
        let ty = ScalarType::Text;
        let exact = Value::Text("x".repeat(MAX_COMPLETE_INDEX_KEY_BYTES - 25));
        let oversized = Value::Text("x".repeat(MAX_COMPLETE_INDEX_KEY_BYTES - 24));
        let component = |value| Component {
            ty: &ty,
            value,
            descending: false,
        };
        assert_eq!(
            encode_complete(&catalog, 1, &[component(&exact)], 1)
                .unwrap()
                .len(),
            MAX_COMPLETE_INDEX_KEY_BYTES
        );
        let error = encode_complete(&catalog, 1, &[component(&oversized)], 1).unwrap_err();
        assert_eq!(error.code, "E_INDEX_KEY_LIMIT");
    }

    #[test]
    fn named_sum_and_product_bytes_follow_catalog_stable_ids() {
        let mut catalog = Catalog::default();
        catalog
            .define(
                "State".into(),
                ScalarType::Enum(EnumType {
                    variants: vec![
                        EnumVariantDef {
                            name: "Pending".into(),
                            args: vec![],
                            id: 0,
                        },
                        EnumVariantDef {
                            name: "Running".into(),
                            args: vec![ScalarType::Int],
                            id: 0,
                        },
                    ],
                }),
            )
            .unwrap();
        catalog
            .define(
                "Meta".into(),
                ScalarType::Record(vec![
                    Column {
                        name: "owner".into(),
                        ty: ScalarType::Text,
                        default: None,
                        id: 0,
                    },
                    Column {
                        name: "attempt".into(),
                        ty: ScalarType::Int,
                        default: None,
                        id: 0,
                    },
                ]),
            )
            .unwrap();

        let state = ScalarType::Ref(catalog.types["State"].id);
        let pending = catalog
            .coerce(
                &Value::Enum(EnumValue {
                    variant: "Pending".into(),
                    args: vec![],
                    id: 0,
                }),
                &state,
                "state",
            )
            .unwrap();
        let running = catalog
            .coerce(
                &Value::Enum(EnumValue {
                    variant: "Running".into(),
                    args: vec![Value::Int(1)],
                    id: 0,
                }),
                &state,
                "state",
            )
            .unwrap();
        let encode = |ty: &ScalarType, value: &Value| {
            encode_tuple(
                &catalog,
                &[Component {
                    ty,
                    value,
                    descending: false,
                }],
            )
            .unwrap()
        };
        assert!(
            catalog
                .cmp_typed(&state, &pending, &running)
                .unwrap()
                .is_lt()
        );
        assert!(encode(&state, &pending) < encode(&state, &running));

        let meta = ScalarType::Ref(catalog.types["Meta"].id);
        let record = |attempt| {
            catalog
                .coerce(
                    &Value::Record(std::collections::BTreeMap::from([
                        ("attempt".into(), Value::Int(attempt)),
                        ("owner".into(), Value::Text("acme".into())),
                    ])),
                    &meta,
                    "meta",
                )
                .unwrap()
        };
        let low = record(1);
        let high = record(2);
        assert!(catalog.cmp_typed(&meta, &low, &high).unwrap().is_lt());
        assert!(encode(&meta, &low) < encode(&meta, &high));
    }
}
