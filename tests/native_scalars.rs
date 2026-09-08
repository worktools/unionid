use std::collections::BTreeMap;

use serde_json::json;
use unionid::{
    Engine, Value,
    codec::{decode_value, encode_value, encode_value_v2},
    model::{Catalog, Column, ScalarType},
    protocol::{Request, WireValue},
    scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid},
    server::execute_protocol_request,
};

fn vectors() -> Vec<(ScalarType, Value, serde_json::Value, Vec<u8>)> {
    vec![
        (
            ScalarType::Uuid,
            Uuid::from_bytes([
                0xf8, 0x1d, 0x4f, 0xae, 0x7d, 0xec, 0x11, 0xd0, 0xa7, 0x65, 0, 0xa0, 0xc9, 0x1e,
                0x6b, 0xf6,
            ])
            .into(),
            json!({"type":"uuid","value":"f81d4fae-7dec-11d0-a765-00a0c91e6bf6"}),
            vec![
                0xf8, 0x1d, 0x4f, 0xae, 0x7d, 0xec, 0x11, 0xd0, 0xa7, 0x65, 0, 0xa0, 0xc9, 0x1e,
                0x6b, 0xf6,
            ],
        ),
        (
            ScalarType::Date,
            "1970-01-01".parse::<Date>().unwrap().into(),
            json!({"type":"date","value":"1970-01-01"}),
            vec![0; 4],
        ),
        (
            ScalarType::Timestamp,
            "1970-01-01T08:00:00.000001+08:00"
                .parse::<Timestamp>()
                .unwrap()
                .into(),
            json!({"type":"timestamp","value":"1970-01-01T00:00:00.000001Z"}),
            vec![0, 0, 0, 0, 0, 0, 0, 1],
        ),
        (
            ScalarType::Duration,
            Duration::from_microseconds(-1_500_000).into(),
            json!({"type":"duration","microseconds":"-1500000"}),
            vec![255, 255, 255, 255, 255, 233, 28, 160],
        ),
        (
            ScalarType::Decimal {
                precision: 18,
                scale: 2,
            },
            Decimal::parse("19.9", 18, 2).unwrap().into(),
            json!({"type":"decimal","coefficient":"1990","scale":2}),
            vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, 198],
        ),
        (
            ScalarType::Bytes,
            "deadbeef".parse::<Bytes>().unwrap().into(),
            json!({"type":"bytes","base64url":"3q2-7w"}),
            vec![0, 0, 0, 4, 0xde, 0xad, 0xbe, 0xef],
        ),
    ]
}

#[test]
fn native_wire_and_codec_golden_vectors_are_lossless() {
    let catalog = Catalog::default();
    for (ty, value, json, payload) in vectors() {
        let wire = WireValue::from(&value);
        assert_eq!(serde_json::to_value(&wire).unwrap(), json);
        let decoded = Value::try_from(serde_json::from_value::<WireValue>(json).unwrap()).unwrap();
        assert!(decoded.cmp_eq(&value));
        let mut expected = b"UIDV\0\x02".to_vec();
        expected.extend(payload);
        assert_eq!(encode_value_v2(&catalog, &ty, &value).unwrap(), expected);
        let decoded = decode_value(&catalog, &ty, &expected).unwrap();
        assert!(decoded.cmp_eq(&value));
        assert_eq!(decoded.index_key(), value.index_key());
        assert!(encode_value(&catalog, &ty, &value).is_err());
        expected[5] = 1;
        assert!(decode_value(&catalog, &ty, &expected).is_err());
        assert!(catalog.coerce(&value, &ScalarType::Text, "test").is_err());
    }
    let v1 = encode_value(&catalog, &ScalarType::Int, &Value::Int(42)).unwrap();
    let v2 = encode_value_v2(&catalog, &ScalarType::Int, &Value::Int(42)).unwrap();
    assert_eq!(&v1[6..], &v2[6..]);
    assert!(
        decode_value(&catalog, &ScalarType::Int, &v2)
            .unwrap()
            .cmp_eq(&Value::Int(42))
    );
}

#[test]
fn native_codec_and_catalog_validate_empty_container_types() {
    let catalog = Catalog::default();
    for (ty, value) in [
        (
            ScalarType::Option(Box::new(ScalarType::Uuid)),
            Value::Option(None),
        ),
        (
            ScalarType::List(Box::new(ScalarType::Bytes)),
            Value::List(vec![]),
        ),
    ] {
        assert!(encode_value(&catalog, &ty, &value).is_err());
        let bytes = encode_value_v2(&catalog, &ty, &value).unwrap();
        assert!(decode_value(&catalog, &ty, &bytes).unwrap().cmp_eq(&value));
    }
    let invalid = ScalarType::List(Box::new(ScalarType::Decimal {
        precision: 2,
        scale: 3,
    }));
    assert!(encode_value_v2(&catalog, &invalid, &Value::List(vec![])).is_err());
    let mut catalog = catalog;
    assert_eq!(
        catalog.define("Invalid".into(), invalid).unwrap_err().code,
        "E_DECIMAL_TYPE"
    );
    assert!(catalog.types.is_empty());
    catalog.define("Id".into(), ScalarType::Uuid).unwrap();
    let id = catalog.types["Id"].id;
    let ty = catalog
        .resolve(
            ScalarType::Record(vec![Column {
                name: "ids".into(),
                ty: ScalarType::List(Box::new(ScalarType::Ref(id))),
                id: 0,
                default: None,
            }]),
            0,
        )
        .unwrap();
    let value = Value::Record(BTreeMap::from([(
        "ids".into(),
        Value::List(vec![Uuid::from_bytes([0; 16]).into()]),
    )]));
    let expected = catalog.coerce(&value, &ty, "test").unwrap();
    let bytes = encode_value_v2(&catalog, &ty, &value).unwrap();
    assert!(
        decode_value(&catalog, &ty, &bytes)
            .unwrap()
            .cmp_eq(&expected)
    );
}

#[test]
fn wire_rejects_noncanonical_native_spellings() {
    for value in [
        json!({"type":"uuid","value":"F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6"}),
        json!({"type":"date","value":"1900-02-29"}),
        json!({"type":"timestamp","value":"1970-01-01T01:00:00+01:00"}),
        json!({"type":"duration","microseconds":"-0"}),
        json!({"type":"decimal","coefficient":"01","scale":2}),
        json!({"type":"bytes","base64url":"_x"}),
    ] {
        let wire: WireValue = serde_json::from_value(value).unwrap();
        assert_eq!(Value::try_from(wire).unwrap_err().code, "E_PARAM_TYPE");
    }
}

#[test]
fn protocol_versions_echo_and_v1_rejects_nested_parameters_before_mutation() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute("type Row = { id int }\ntable items Row\ninsert items { id = 1 }")
            .ok
    );
    for (_, value, _, _) in vectors() {
        let request = Request::query(
            "native-read",
            "from items | derive native = $value | select native",
        )
        .with_version(2)
        .unwrap();
        let mut request = request;
        request
            .params
            .insert("value".into(), WireValue::from(&value));
        let response = execute_protocol_request(&mut engine, request.clone());
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.version, 2);
        assert!(
            Value::try_from(response.rows[0]["native"].clone())
                .unwrap()
                .cmp_eq(&value)
        );

        request.version = 1;
        request.query = "insert items { id = 2 }\nfrom items | derive native = $value".into();
        request.params.insert(
            "value".into(),
            WireValue::Option {
                value: Some(Box::new(WireValue::List {
                    items: vec![WireValue::from(&value)],
                })),
            },
        );
        let response = execute_protocol_request(&mut engine, request);
        assert_eq!(response.version, 1);
        assert_eq!(response.error.unwrap().code, "E_PROTOCOL_TYPE");
        assert_eq!(engine.execute("from items").rows.len(), 1);
    }
    for version in [1, 2, 99] {
        let mut request = Request::query("error", "from missing");
        request.version = version;
        let response = execute_protocol_request(&mut engine, request);
        assert_eq!(response.version, version);
        assert!(!response.ok);
    }
    let a = Request::query("attempt", "insert items { id = 3 }");
    let b = a.clone().with_version(2).unwrap();
    assert_ne!(a.canonical_digest().unwrap(), b.canonical_digest().unwrap());
}

#[test]
fn protocol_v2_introspection_receipts_retries_and_errors_echo_the_version() {
    let mut engine = Engine::memory();
    assert!(engine.execute("type Row = { id int }\ntable items Row").ok);
    let request = Request::query("write", "insert items { id = 1 }")
        .with_version(2)
        .unwrap()
        .with_idempotency_key("scalar-version-test")
        .unwrap();
    let first = execute_protocol_request(&mut engine, request.clone());
    assert!(first.ok, "{}", first.message);
    assert_eq!(first.version, 2);
    let replay = execute_protocol_request(&mut engine, request.clone());
    assert_eq!(replay.version, 2);
    assert!(replay.idempotency.unwrap().replayed);
    let conflict = execute_protocol_request(&mut engine, request.with_version(1).unwrap());
    assert_eq!(conflict.version, 1);
    assert_eq!(conflict.error.unwrap().code, "E_IDEMPOTENCY_CONFLICT");
    for request in [
        Request::receipt_status("status"),
        Request::introspection("schema", unionid::introspection::IntrospectionKind::Schema),
    ] {
        let response = execute_protocol_request(&mut engine, request.with_version(2).unwrap());
        assert_eq!(response.version, 2);
        assert!(response.ok, "{}", response.message);
    }
    let mut invalid = Request::receipt_status("invalid").with_version(2).unwrap();
    invalid.query = "from items".into();
    let error = execute_protocol_request(&mut engine, invalid);
    assert_eq!(error.version, 2);
    assert_eq!(error.error.unwrap().code, "E_PROTOCOL");
    assert_eq!(engine.execute("from items").rows.len(), 1);
}

#[test]
fn legacy_durable_formats_reject_native_writes_without_changing_state() {
    let path = std::env::temp_dir().join(format!(
        "unionid-native-{}-{}.redb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut engine = Engine::open_redb(path.clone()).unwrap();
        assert!(
            engine
                .execute("type Row = { id int }\ntable items Row\ninsert items { id = 1 }")
                .ok
        );
        let mut request = Request::query(
            "write",
            "insert items { id = 2 }\nfrom items | derive native = $value",
        )
        .with_version(2)
        .unwrap()
        .with_idempotency_key("native-write")
        .unwrap();
        request.params.insert(
            "value".into(),
            WireValue::from(&Value::from(Uuid::from_bytes([0; 16]))),
        );
        let response = execute_protocol_request(&mut engine, request);
        assert_eq!(response.error.unwrap().code, "E_STORAGE_UPGRADE_REQUIRED");
        assert!(engine.check_integrity().is_ok());
        assert_eq!(engine.execute("from items").rows.len(), 1);
        let status = execute_protocol_request(&mut engine, Request::receipt_status("status"));
        assert_eq!(
            serde_json::to_value(status).unwrap()["receipts"]["result"]["count"],
            0
        );
        // Read-only use of a native parameter does not require a durable format change.
        let request = Request::query("read", "from items | derive native = $value")
            .with_version(2)
            .unwrap()
            .with_serde_param("value", &Uuid::from_bytes([0; 16]))
            .unwrap();
        assert!(execute_protocol_request(&mut engine, request).ok);
    }
    let mut engine = Engine::open_redb(path.clone()).unwrap();
    assert_eq!(engine.execute("from items").rows.len(), 1);
    engine.check_integrity().unwrap();
    drop(engine);
    std::fs::remove_file(path).unwrap();
}
