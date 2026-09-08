mod common;

use common::TempDir;
use serde::{Deserialize, Serialize};
use unionid::{
    Engine, Value, backup, format_source,
    protocol::Request,
    scalars::{Date, Duration, Timestamp},
    server::execute_protocol_request,
};

const SETUP: &str = r#"type Event = {
  id int,
  day date,
  occurred_at timestamp,
  retry_after duration,
}

table events Event
  key id

create index events (occurred_at)

insert many events [
  {id = 1, day = @1970-01-01, occurred_at = @1970-01-01T08:00:00.000001+08:00, retry_after = 1500milliseconds},
  {id = 2, day = @2026-09-08, occurred_at = @2026-09-08T00:00:00Z, retry_after = 1day},
]"#;

#[test]
fn temporal_literals_queries_arithmetic_and_sum_are_typed() {
    let mut engine = Engine::memory();
    let formatted = format_source(SETUP).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert!(engine.execute(&formatted).ok);

    let result = engine.execute(
        r#"from events
derive {deadline = occurred_at + retry_after, elapsed = occurred_at - @1970-01-01T00:00:00Z, negative = -retry_after}
sort occurred_at
select {id, day, occurred_at, deadline, elapsed, negative}"#,
    );
    assert!(result.ok, "{}", result.message);
    assert_eq!(
        result.rows[0]["occurred_at"].source_text(),
        "@1970-01-01T00:00:00.000001Z"
    );
    assert_eq!(
        result.rows[0]["deadline"].source_text(),
        "@1970-01-01T00:00:01.500001Z"
    );
    assert_eq!(result.rows[0]["elapsed"].source_text(), "1microsecond");
    assert_eq!(
        result.rows[0]["negative"].source_text(),
        "-1500milliseconds"
    );

    let aggregate = engine.execute("from events | aggregate {retry_total = sum retry_after, first = min day, last = max occurred_at}");
    assert!(aggregate.ok, "{}", aggregate.message);
    assert_eq!(
        aggregate.rows[0]["retry_total"].source_text(),
        "86401500milliseconds"
    );
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NativeEvent {
    id: i64,
    day: Date,
    occurred_at: Timestamp,
    retry_after: Duration,
}

#[test]
fn source_rust_wire_redb_backup_and_cursor_preserve_temporal_values() {
    let dir = TempDir::new();
    let db = dir.0.join("temporal.redb");
    let archive = dir.0.join("temporal.backup.json");
    let restored = dir.0.join("temporal-restored.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    assert!(engine.execute("type Event = {id int, day date, occurred_at timestamp, retry_after duration}\ntable events Event\n  key id\ncreate index events (occurred_at)").ok);
    let row = NativeEvent {
        id: 1,
        day: "2026-09-08".parse().unwrap(),
        occurred_at: "2026-09-08T08:00:00+08:00".parse().unwrap(),
        retry_after: "30seconds".parse().unwrap(),
    };
    let response = execute_protocol_request(
        &mut engine,
        Request::query("insert", "insert events $row\nreturning")
            .with_version(2)
            .unwrap()
            .with_serde_param("row", &row)
            .unwrap(),
    );
    let inserted = response.typed_rows::<NativeEvent>().unwrap();
    assert_eq!(inserted.len(), 1);
    assert_eq!(inserted[0], row);
    let page = engine.execute("from events | sort {occurred_at, id} | page 1");
    assert!(page.ok, "{}", page.message);
    drop(engine);
    backup::create(&db, &archive).unwrap();
    backup::restore(&archive, &restored).unwrap();
    let mut restored = Engine::open_redb(&restored).unwrap();
    let response = execute_protocol_request(
        &mut restored,
        Request::query("read", "from events")
            .with_version(2)
            .unwrap(),
    );
    assert_eq!(response.typed_rows::<NativeEvent>().unwrap(), [row]);
}

#[test]
fn temporal_parse_migrations_are_exact_and_atomic() {
    let mut engine = Engine::memory();
    let response = engine.execute(
        r#"type Legacy = {id int, day text, at text, delay text}
table records Legacy
  key id
insert records {id = 1, day = "2026-09-08", at = "2026-09-08T08:00:00+08:00", delay = "30seconds"}
migration temporal
  change field Legacy.day to date using old -> date_parse old
  change field Legacy.at to timestamp using old -> timestamp_parse old
  change field Legacy.delay to duration using old -> duration_parse old
from records"#,
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.rows[0]["at"].source_text(),
        "@2026-09-08T00:00:00Z"
    );

    let failed = engine
        .execute("update records | set delay = 9223372036854775807microseconds + 1microsecond");
    assert_eq!(failed.error.unwrap().code, "E_ARITH");
    assert_eq!(
        engine.execute("from records").rows[0]["delay"].source_text(),
        "30seconds"
    );
}

#[test]
fn invalid_temporal_source_is_rejected_without_implicit_calendar_or_zone_rules() {
    let mut engine = Engine::memory();
    for source in [
        "type Bad = {v date}\ntable bad Bad\ninsert bad {v = @2026-02-30}",
        "type Bad = {v timestamp}\ntable bad Bad\ninsert bad {v = @2026-09-08T12:00:00}",
        "type Bad = {v timestamp}\ntable bad Bad\ninsert bad {v = @2026-09-08T12:00:60Z}",
        "type Bad = {v duration}\ntable bad Bad\ninsert bad {v = 1month}",
        "type Bad = {v duration}\ntable bad Bad\ninsert bad {v = 1.5seconds}",
    ] {
        let response = engine.execute(source);
        assert!(!response.ok, "accepted {source}");
        assert!(engine.execute("from bad").error.is_some());
    }
    let wrong = engine
        .execute("type Bad = {v date}\ntable bad Bad\ninsert bad {v = @2026-09-08T00:00:00Z}");
    assert_eq!(wrong.error.unwrap().code, "E_TYPE");
    assert!(!matches!(
        Value::Date("2026-09-08".parse().unwrap()),
        Value::Timestamp(_)
    ));
}
