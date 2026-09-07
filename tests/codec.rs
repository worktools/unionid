use unionid::codec::{MAX_VALUE_BYTES, VALUE_CODEC_VERSION, decode_value, encode_value};
use unionid::model::{
    Catalog, Column, EnumType, EnumValue, EnumVariantDef, MAX_DEPTH, ScalarType, Value,
};

fn column(name: &str, ty: ScalarType, default: Option<Value>) -> Column {
    Column {
        name: name.into(),
        ty,
        default,
        id: 0,
    }
}

fn record(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Record(
        fields
            .into_iter()
            .map(|(name, value)| (name.into(), value))
            .collect(),
    )
}

fn variant(name: &str, args: Vec<Value>) -> Value {
    Value::Enum(EnumValue {
        variant: name.into(),
        args,
        id: 0,
    })
}

fn task_catalog() -> (Catalog, ScalarType) {
    let mut catalog = Catalog::default();
    catalog
        .define(
            "Contact".into(),
            ScalarType::Record(vec![
                column("email", ScalarType::Text, None),
                column(
                    "nickname",
                    ScalarType::Option(Box::new(ScalarType::Text)),
                    Some(variant("None", Vec::new())),
                ),
            ]),
        )
        .unwrap();
    catalog
        .define(
            "State".into(),
            ScalarType::Enum(EnumType {
                variants: vec![
                    EnumVariantDef {
                        name: "Pending".into(),
                        args: Vec::new(),
                        id: 0,
                    },
                    EnumVariantDef {
                        name: "Running".into(),
                        args: vec![ScalarType::Record(vec![
                            column("worker", ScalarType::Text, None),
                            column("attempt", ScalarType::Int, Some(Value::Int(1))),
                        ])],
                        id: 0,
                    },
                ],
            }),
        )
        .unwrap();
    catalog
        .define(
            "Task".into(),
            ScalarType::Record(vec![
                column("id", ScalarType::Int, None),
                column("owner", ScalarType::Named("Contact".into()), None),
                column(
                    "position",
                    ScalarType::Tuple(vec![ScalarType::Float, ScalarType::Float]),
                    None,
                ),
                column(
                    "aliases",
                    ScalarType::Option(Box::new(ScalarType::List(Box::new(ScalarType::Text)))),
                    Some(variant("None", Vec::new())),
                ),
                column("state", ScalarType::Named("State".into()), None),
            ]),
        )
        .unwrap();
    let task_id = catalog.types["Task"].id;
    (catalog, ScalarType::Ref(task_id))
}

#[test]
fn every_adt_shape_round_trips_with_defaults_and_nominal_ids() {
    let (catalog, ty) = task_catalog();
    let input = record([
        ("id", Value::Int(i64::MAX)),
        (
            "owner",
            record([("email", Value::Text("alice@example.com".into()))]),
        ),
        (
            "position",
            Value::Tuple(vec![Value::Float(-0.0), Value::Float(3.5)]),
        ),
        (
            "state",
            variant(
                "Running",
                vec![record([("worker", Value::Text("local".into()))])],
            ),
        ),
    ]);
    let expected = catalog.coerce(&input, &ty, "value").unwrap();
    let encoded = encode_value(&catalog, &ty, &input).unwrap();
    let decoded = decode_value(&catalog, &ty, &encoded).unwrap();
    assert!(decoded.cmp_eq(&expected));
    assert_eq!(encode_value(&catalog, &ty, &decoded).unwrap(), encoded);
    for name in ["Task", "owner", "nickname", "Running", "worker"] {
        assert!(
            !encoded
                .windows(name.len())
                .any(|window| window == name.as_bytes())
        );
    }
}

#[test]
fn field_reorder_and_rename_preserve_bytes_and_decode_to_current_names() {
    let mut catalog = Catalog::default();
    catalog
        .define(
            "Pair".into(),
            ScalarType::Record(vec![
                column("left", ScalarType::Int, None),
                column("right", ScalarType::Bool, None),
            ]),
        )
        .unwrap();
    let id = catalog.types["Pair"].id;
    let ty = ScalarType::Ref(id);
    let bytes = encode_value(
        &catalog,
        &ty,
        &record([("left", Value::Int(7)), ("right", Value::Bool(true))]),
    )
    .unwrap();

    let mut definition = catalog.types.remove("Pair").unwrap();
    definition.name = "RenamedPair".into();
    let ScalarType::Record(fields) = &mut definition.ty else {
        unreachable!()
    };
    fields[0].name = "value".into();
    fields.reverse();
    catalog.types.insert(definition.name.clone(), definition);

    let decoded = decode_value(&catalog, &ty, &bytes).unwrap();
    let Value::Named { value, .. } = &decoded else {
        panic!("named type identity was lost")
    };
    let Value::Record(fields) = value.as_ref() else {
        panic!("record value was lost")
    };
    assert!(fields["value"].cmp_eq(&Value::Int(7)));
    assert!(fields["right"].cmp_eq(&Value::Bool(true)));
    assert_eq!(encode_value(&catalog, &ty, &decoded).unwrap(), bytes);
}

#[test]
fn nominally_distinct_types_have_distinct_encodings() {
    let mut catalog = Catalog::default();
    for name in ["First", "Second"] {
        catalog
            .define(
                name.into(),
                ScalarType::Enum(EnumType {
                    variants: vec![EnumVariantDef {
                        name: "Same".into(),
                        args: Vec::new(),
                        id: 0,
                    }],
                }),
            )
            .unwrap();
    }
    let first = ScalarType::Ref(catalog.types["First"].id);
    let second = ScalarType::Ref(catalog.types["Second"].id);
    let value = variant("Same", Vec::new());
    let first_bytes = encode_value(&catalog, &first, &value).unwrap();
    let second_bytes = encode_value(&catalog, &second, &value).unwrap();
    assert_ne!(first_bytes, second_bytes);
    assert!(decode_value(&catalog, &second, &first_bytes).is_err());
}

#[test]
fn primitive_format_has_a_stable_versioned_golden_value() {
    assert_eq!(VALUE_CODEC_VERSION, 1);
    let bytes = encode_value(&Catalog::default(), &ScalarType::Int, &Value::Int(-1)).unwrap();
    assert_eq!(
        bytes,
        [
            b'U', b'I', b'D', b'V', 0, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
        ]
    );
    assert_eq!(
        encode_value(&Catalog::default(), &ScalarType::Float, &Value::Float(-0.0)).unwrap(),
        encode_value(&Catalog::default(), &ScalarType::Float, &Value::Float(0.0)).unwrap()
    );
}

#[test]
fn malformed_versions_lengths_tags_and_trailing_bytes_fail_closed() {
    let catalog = Catalog::default();
    let good = encode_value(&catalog, &ScalarType::Bool, &Value::Bool(true)).unwrap();
    let mut bad_magic = good.clone();
    bad_magic[0] = b'X';
    assert_eq!(
        decode_value(&catalog, &ScalarType::Bool, &bad_magic)
            .unwrap_err()
            .code,
        "E_CODEC"
    );

    let mut bad_version = good.clone();
    bad_version[5] = 2;
    assert!(
        decode_value(&catalog, &ScalarType::Bool, &bad_version)
            .unwrap_err()
            .message
            .contains("version 2")
    );
    assert!(decode_value(&catalog, &ScalarType::Bool, &good[..good.len() - 1]).is_err());

    let mut bad_tag = good.clone();
    *bad_tag.last_mut().unwrap() = 3;
    assert!(
        decode_value(&catalog, &ScalarType::Bool, &bad_tag)
            .unwrap_err()
            .message
            .contains("bool tag")
    );

    let mut trailing = good;
    trailing.push(0);
    assert!(
        decode_value(&catalog, &ScalarType::Bool, &trailing)
            .unwrap_err()
            .message
            .contains("trailing")
    );

    let mut invalid_utf8 =
        encode_value(&catalog, &ScalarType::Text, &Value::Text("x".into())).unwrap();
    *invalid_utf8.last_mut().unwrap() = 0xff;
    assert!(
        decode_value(&catalog, &ScalarType::Text, &invalid_utf8)
            .unwrap_err()
            .message
            .contains("UTF-8")
    );

    let mut excessive_list = encode_value(
        &catalog,
        &ScalarType::List(Box::new(ScalarType::Bool)),
        &Value::List(Vec::new()),
    )
    .unwrap();
    excessive_list[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(
        decode_value(
            &catalog,
            &ScalarType::List(Box::new(ScalarType::Bool)),
            &excessive_list,
        )
        .unwrap_err()
        .message
        .contains("item limit")
    );
}

#[test]
fn unknown_stable_field_ids_fail_closed() {
    let mut catalog = Catalog::default();
    catalog
        .define(
            "Flags".into(),
            ScalarType::Record(vec![column("enabled", ScalarType::Bool, None)]),
        )
        .unwrap();
    let ty = ScalarType::Ref(catalog.types["Flags"].id);
    let mut bytes = encode_value(&catalog, &ty, &record([("enabled", Value::Bool(true))])).unwrap();
    let field_id_offset = 4 + 2 + 8 + 4;
    bytes[field_id_offset..field_id_offset + 8].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(
        decode_value(&catalog, &ty, &bytes)
            .unwrap_err()
            .message
            .contains("unknown field ID")
    );
}

#[test]
fn corrupt_nested_values_report_the_typed_field_path() {
    let mut catalog = Catalog::default();
    catalog
        .define(
            "Flags".into(),
            ScalarType::Record(vec![column("enabled", ScalarType::Bool, None)]),
        )
        .unwrap();
    let ty = ScalarType::Ref(catalog.types["Flags"].id);
    let mut bytes = encode_value(&catalog, &ty, &record([("enabled", Value::Bool(true))])).unwrap();
    *bytes.last_mut().unwrap() = 9;
    let error = decode_value(&catalog, &ty, &bytes).unwrap_err();
    assert_eq!(error.code, "E_CODEC");
    assert!(error.message.contains("value.enabled"), "{error}");
}

#[test]
fn encoded_and_input_size_limits_are_enforced() {
    let oversized = Value::Text("x".repeat(MAX_VALUE_BYTES));
    let error = encode_value(&Catalog::default(), &ScalarType::Text, &oversized).unwrap_err();
    assert_eq!(error.code, "E_CODEC");
    let bytes = vec![0; MAX_VALUE_BYTES + 1];
    assert_eq!(
        decode_value(&Catalog::default(), &ScalarType::Text, &bytes)
            .unwrap_err()
            .code,
        "E_CODEC"
    );
}

#[test]
fn recursive_named_values_round_trip_and_obey_the_depth_limit() {
    let mut catalog = Catalog::default();
    catalog
        .define(
            "Chain".into(),
            ScalarType::Enum(EnumType {
                variants: vec![
                    EnumVariantDef {
                        name: "Next".into(),
                        args: vec![ScalarType::Named("Chain".into())],
                        id: 0,
                    },
                    EnumVariantDef {
                        name: "End".into(),
                        args: Vec::new(),
                        id: 0,
                    },
                ],
            }),
        )
        .unwrap();
    let definition = &catalog.types["Chain"];
    let ScalarType::Enum(sum) = &definition.ty else {
        unreachable!()
    };
    assert_eq!(
        sum.variants
            .iter()
            .map(|variant| variant.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(definition.id, 3);
    let ty = ScalarType::Ref(catalog.types["Chain"].id);
    let mut input = variant("End", Vec::new());
    for _ in 0..4 {
        input = variant("Next", vec![input]);
    }
    let expected = catalog.coerce(&input, &ty, "value").unwrap();
    let encoded = encode_value(&catalog, &ty, &input).unwrap();
    let decoded = decode_value(&catalog, &ty, &encoded).unwrap();
    assert!(decoded.cmp_eq(&expected));
    assert_eq!(encode_value(&catalog, &ty, &decoded).unwrap(), encoded);

    let mut too_deep = variant("End", Vec::new());
    for _ in 0..MAX_DEPTH {
        too_deep = variant("Next", vec![too_deep]);
    }
    let error = encode_value(&catalog, &ty, &too_deep).unwrap_err();
    assert_eq!(error.code, "E_LIMIT");
}
