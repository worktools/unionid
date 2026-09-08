use std::fmt::Debug;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value as Json, json};
use unionid::{
    Value,
    scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid},
};

fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: T, payload: Json) {
    assert_eq!(serde_json::to_value(&value).unwrap(), payload);
    assert_eq!(serde_json::from_value::<T>(payload).unwrap(), value);
}

#[test]
fn canonical_serde_payloads_preserve_exact_values() {
    golden(
        "F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6"
            .parse::<Uuid>()
            .unwrap(),
        json!("f81d4fae-7dec-11d0-a765-00a0c91e6bf6"),
    );
    golden("1970-01-01".parse::<Date>().unwrap(), json!("1970-01-01"));
    golden(
        "1970-01-01T08:00:00.000001+08:00"
            .parse::<Timestamp>()
            .unwrap(),
        json!("1970-01-01T00:00:00.000001Z"),
    );
    golden(
        "-1500milliseconds".parse::<Duration>().unwrap(),
        json!("-1500000"),
    );
    golden(
        Decimal::parse("19.9", 18, 2).unwrap(),
        json!({"coefficient":"1990","scale":2}),
    );
    golden("deadbeef".parse::<Bytes>().unwrap(), json!("3q2-7w"));
    golden(
        "89504e470d0a1a0a".parse::<Bytes>().unwrap(),
        json!("iVBORw0KGgo"),
    );
    for value in [i64::MIN, i64::MAX, -1, 0, 1] {
        golden(Duration::from_microseconds(value), json!(value.to_string()));
    }
    for source in [
        "99999999999999999999999999999999999999",
        "-99999999999999999999999999999999999999",
    ] {
        golden(
            Decimal::parse(source, 38, 0).unwrap(),
            json!({"coefficient":source,"scale":0}),
        );
    }
    golden(
        Decimal::parse("-0.00", 4, 2).unwrap(),
        json!({"coefficient":"0","scale":2}),
    );
    golden(
        Decimal::new(1, 38, 38).unwrap(),
        json!({"coefficient":"1","scale":38}),
    );
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum Event {
    Pending,
    Completed {
        at: Timestamp,
        cost: Decimal,
        attachments: Vec<Bytes>,
    },
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Task {
    id: Uuid,
    due: Option<Date>,
    retry: (Duration, Option<Duration>),
    events: Vec<Event>,
}

#[test]
fn application_adts_round_trip_without_a_json_number_precision_boundary() {
    let task = Task {
        id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
        due: Some("2026-09-08".parse().unwrap()),
        retry: (Duration::from_microseconds(i64::MAX), None),
        events: vec![
            Event::Pending,
            Event::Completed {
                at: "2026-09-08T00:00:00Z".parse().unwrap(),
                cost: Decimal::new(99999999999999999999999999999999999999, 38, 2).unwrap(),
                attachments: vec!["".parse().unwrap(), "00ff".parse().unwrap()],
            },
        ],
    };
    let encoded = serde_json::to_string(&task).unwrap();
    assert_eq!(serde_json::from_str::<Task>(&encoded).unwrap(), task);
    let json: Json = serde_json::from_str(&encoded).unwrap();
    assert_eq!(json["retry"][0], "9223372036854775807");
    assert_eq!(
        json["events"][1]["Completed"]["cost"]["coefficient"],
        "99999999999999999999999999999999999999"
    );
    // Nested application ADTs preserve native identity in the database model.
    let value = Value::from_serde(&task).unwrap();
    assert_eq!(value.to_serde::<Task>().unwrap(), task);
}

fn rejected<T: DeserializeOwned>(values: &[Json]) {
    for value in values {
        assert!(
            serde_json::from_value::<T>(value.clone()).is_err(),
            "accepted {value}"
        );
    }
}

#[test]
fn deserialization_validates_canonical_form_and_scalar_invariants() {
    rejected::<Uuid>(&[
        json!("F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6"),
        json!("invalid"),
        json!(vec![0; 16]),
    ]);
    rejected::<Date>(&[
        json!("1900-02-29"),
        json!("0000-01-01"),
        json!("2026-9-08"),
        json!(0),
    ]);
    rejected::<Timestamp>(&[
        json!("1970-01-01T01:00:00+01:00"),
        json!("1970-01-01t00:00:00z"),
        json!("1970-01-01T00:00:00.0Z"),
        json!("1970-01-01T00:00:00.0000001Z"),
        json!("9999-12-31T23:59:60Z"),
        json!("0000-01-01T00:00:00Z"),
    ]);
    rejected::<Duration>(&[
        json!("1second"),
        json!("+1"),
        json!("01"),
        json!("-0"),
        json!("1.0"),
        json!("9223372036854775808"),
        json!("-9223372036854775809"),
        json!(1),
        json!(null),
    ]);
    rejected::<Decimal>(&[
        json!("1.20"),
        json!({"coefficient":1,"scale":0}),
        json!({"coefficient":"+1","scale":0}),
        json!({"coefficient":"01","scale":0}),
        json!({"coefficient":"-0","scale":0}),
        json!({"coefficient":"1e2","scale":0}),
        json!({"coefficient":"1","scale":39}),
        json!({"coefficient":"1","scale":-1}),
        json!({"coefficient":"100000000000000000000000000000000000000","scale":0}),
        json!({"coefficient":"-170141183460469231731687303715884105728","scale":0}),
        json!({"coefficient":"1","scale":0,"precision":1}),
        json!({"coefficient":"1"}),
    ]);
    assert!(
        serde_json::from_str::<Decimal>(r#"{"coefficient":"1","coefficient":"2","scale":0}"#)
            .is_err()
    );
    rejected::<Bytes>(&[
        json!("/w"),
        json!("_w=="),
        json!("_x"),
        json!("__9"),
        json!("_w\n"),
        json!([255]),
    ]);
}

#[test]
fn bytes_base64_is_canonical_and_bounded() {
    for length in 0..=256 {
        let bytes = Bytes::new((0..length).map(|n| n as u8).collect()).unwrap();
        let encoded = bytes.to_base64url();
        assert_eq!(Bytes::from_base64url(&encoded).unwrap(), bytes);
        golden(bytes, json!(encoded));
    }
    let bytes = Bytes::new(vec![255; unionid::scalars::MAX_BYTES]).unwrap();
    let mut encoded = bytes.to_base64url();
    assert_eq!(Bytes::from_base64url(&encoded).unwrap(), bytes);
    // MAX_BYTES % 3 = 1; append two alphabet characters to exceed the bound.
    encoded.push_str("AA");
    assert!(Bytes::from_base64url(&encoded).is_err());
}

fn native_roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, marker: &str) {
    let native = Value::from_serde(value).unwrap();
    assert!(native.requires_protocol_v2());
    let kind = marker.rsplit("::").next().unwrap();
    assert_eq!(
        serde_json::to_value(&native).unwrap()["kind"]
            .as_str()
            .unwrap()
            .to_lowercase(),
        kind
    );
    assert_eq!(&native.to_serde::<T>().unwrap(), value);
}

#[test]
fn scalar_markers_are_distinct_and_ordinary_newtypes_keep_working() {
    native_roundtrip(&Uuid::from_bytes([0; 16]), "unionid::scalar::v1::uuid");
    native_roundtrip(
        &Date::from_epoch_days(0).unwrap(),
        "unionid::scalar::v1::date",
    );
    native_roundtrip(
        &Timestamp::from_epoch_microseconds(0).unwrap(),
        "unionid::scalar::v1::timestamp",
    );
    native_roundtrip(
        &Duration::from_microseconds(0),
        "unionid::scalar::v1::duration",
    );
    native_roundtrip(
        &Decimal::new(0, 1, 0).unwrap(),
        "unionid::scalar::v1::decimal",
    );
    native_roundtrip(&Bytes::new(vec![]).unwrap(), "unionid::scalar::v1::bytes");

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Label(String);
    let label = Label("application text".into());
    let value = Value::from_serde(&label).unwrap();
    assert!(matches!(value, Value::Text(_)));
    assert_eq!(value.to_serde::<Label>().unwrap(), label);
}
