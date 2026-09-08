use unionid::scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid};

#[test]
fn rfc_scalar_logical_golden_vectors() {
    let uuid: Uuid = "F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6".parse().unwrap();
    assert_eq!(uuid.to_string(), "f81d4fae-7dec-11d0-a765-00a0c91e6bf6");
    assert_eq!(
        *uuid.as_bytes(),
        [
            0xf8, 0x1d, 0x4f, 0xae, 0x7d, 0xec, 0x11, 0xd0, 0xa7, 0x65, 0x00, 0xa0, 0xc9, 0x1e,
            0x6b, 0xf6
        ]
    );
    assert_eq!("1970-01-01".parse::<Date>().unwrap().epoch_days(), 0);
    let instant: Timestamp = "1970-01-01T08:00:00.000001+08:00".parse().unwrap();
    assert_eq!(instant.epoch_microseconds(), 1);
    assert_eq!(instant.to_string(), "1970-01-01T00:00:00.000001Z");
    let duration: Duration = "-1500milliseconds".parse().unwrap();
    assert_eq!(duration.microseconds(), -1_500_000);
    assert_eq!(duration.to_string(), "-1500milliseconds");
    let decimal = Decimal::parse("19.9", 18, 2).unwrap();
    assert_eq!(decimal.coefficient(), 1990);
    assert_eq!(decimal.scale(), 2);
    assert_eq!(decimal.to_string(), "19.90");
    let bytes: Bytes = "deadbeef".parse().unwrap();
    assert_eq!(bytes.as_slice(), &[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(bytes.to_string(), "deadbeef");
}

#[test]
fn date_calendar_boundaries_and_epoch_vectors() {
    for (source, days) in [
        ("0001-01-01", -719162),
        ("9999-12-31", 2932896),
        ("1969-12-31", -1),
        ("2000-02-29", 11016),
        ("1900-03-01", -25508),
    ] {
        let date: Date = source.parse().unwrap();
        assert_eq!(date.epoch_days(), days);
        assert_eq!(Date::from_epoch_days(days).unwrap().to_string(), source);
    }
    for source in [
        "0000-01-01",
        "10000-01-01",
        "1900-02-29",
        "2001-02-29",
        "2026-04-31",
        "2026-00-01",
        "2026-01-00",
        " 2026-01-01",
        "２０２６-01-01",
    ] {
        assert!(source.parse::<Date>().is_err(), "{source}");
    }
    assert!(Date::from_epoch_days(Date::MIN_DAYS - 1).is_err());
    assert!(Date::from_epoch_days(Date::MAX_DAYS + 1).is_err());
    // Exercise every century's leap rule and the inverse conversion across all years.
    for year in 1..=9999 {
        for month in 1..=12 {
            let source = format!("{year:04}-{month:02}-01");
            let date: Date = source.parse().unwrap();
            assert_eq!(date.to_string(), source);
        }
    }
}

#[test]
fn timestamps_normalize_offsets_without_losing_precision() {
    for (source, expected) in [
        ("1969-12-31T23:59:59.999999Z", -1),
        ("1970-01-01T00:00:00.000001000Z", 1),
        ("1969-12-31T23:00:00-01:00", 0),
        ("1970-01-01t00:00:00z", 0),
    ] {
        let value: Timestamp = source.parse().unwrap();
        assert_eq!(value.epoch_microseconds(), expected);
        assert_eq!(value.to_string().parse::<Timestamp>().unwrap(), value);
    }
    for source in [
        "1970-01-01T00:00:60Z",
        "1970-01-01T00:00:00",
        "1970-01-01T00:00:00-00:00",
        "1970-01-01T00:00:00.0000001Z",
        "1970-01-01T00:00:00.Z",
        "1970-01-01T24:00:00Z",
        "1970-01-01T00:00:00+24:00",
        "0001-01-01T00:00:00+00:01",
        "9999-12-31T23:59:59-00:01",
    ] {
        assert!(source.parse::<Timestamp>().is_err(), "{source}");
    }
    for source in ["0001-01-01T00:00:00Z", "9999-12-31T23:59:59.999999Z"] {
        assert_eq!(source.parse::<Timestamp>().unwrap().to_string(), source);
    }
    assert!(Timestamp::from_epoch_microseconds(i64::MIN).is_err());
    assert!(Timestamp::from_epoch_microseconds(i64::MAX).is_err());
}

#[test]
fn duration_limits_units_and_canonical_form() {
    for value in [i64::MIN, i64::MAX, -1, 0, 1, DAY, 1_500_000] {
        let value = Duration::from_microseconds(value);
        assert_eq!(value.to_string().parse::<Duration>().unwrap(), value);
    }
    const DAY: i64 = 86_400_000_000;
    assert_eq!("24hours".parse::<Duration>().unwrap().to_string(), "1day");
    assert_eq!(
        "0seconds".parse::<Duration>().unwrap().to_string(),
        "0microseconds"
    );
    for source in [
        "1month",
        "1year",
        "1.5seconds",
        "+1second",
        "1 seconds",
        "9223372036854775807seconds",
        "",
        "-",
        "1",
    ] {
        assert!(source.parse::<Duration>().is_err(), "{source}");
    }
}

#[test]
fn decimal_rescaling_is_exact_and_precision_is_bounded() {
    let largest = "99999999999999999999999999999999999999";
    assert_eq!(Decimal::parse(largest, 38, 0).unwrap().to_string(), largest);
    assert!(Decimal::parse(&format!("1{largest}"), 38, 0).is_err());
    assert!(Decimal::new(i128::MIN, 38, 0).is_err());
    let value = Decimal::parse("-0.00", 4, 2).unwrap();
    assert_eq!(value.to_string(), "0.00");
    assert_eq!(Decimal::parse("19.900", 4, 2).unwrap().to_string(), "19.90");
    assert!(Decimal::parse("19.901", 4, 2).is_err());
    let value = Decimal::parse("1.20", 4, 2).unwrap();
    assert_eq!(value.rescale(3, 1).unwrap().to_string(), "1.2");
    assert_eq!(value.rescale(6, 4).unwrap().to_string(), "1.2000");
    assert!(value.rescale(3, 0).is_err());
    assert!(value.rescale(2, 2).is_err());
    let inferred = Decimal::infer("0.0001").unwrap();
    assert_eq!((inferred.precision(), inferred.scale()), (4, 4));
    for source in ["+1", "1e2", "NaN", "1_000", ".1", "1.", "--1", ""] {
        assert!(Decimal::parse(source, 10, 2).is_err(), "{source}");
    }
    for (p, s) in [(0, 0), (39, 0), (1, 2)] {
        assert!(Decimal::new(0, p, s).is_err());
    }
}

#[test]
fn identifiers_and_bytes_reject_ambiguous_or_oversized_input() {
    for source in [
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
    ] {
        assert_eq!(source.parse::<Uuid>().unwrap().to_string(), source);
    }
    for source in [
        "00000000000000000000000000000000",
        "{00000000-0000-0000-0000-000000000000}",
        "00000000_0000-0000-0000-000000000000",
    ] {
        assert!(source.parse::<Uuid>().is_err());
    }
    assert!("".parse::<Bytes>().unwrap().as_slice().is_empty());
    for source in ["f", "0xff", "ff ff", "gg", "é"] {
        assert!(source.parse::<Bytes>().is_err());
    }
    assert!(Bytes::new(vec![0; unionid::scalars::MAX_BYTES]).is_ok());
    assert!(Bytes::new(vec![0; unionid::scalars::MAX_BYTES + 1]).is_err());
}
