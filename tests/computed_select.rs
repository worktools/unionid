mod common;
use common::TempDir;
use std::collections::BTreeMap;
use unionid::{Engine, Value, format_source};

const SETUP: &str = r#"type State = Pending | Running {attempt int}
type Row = {id int, score int, state State}
table rows Row
  key id
insert many rows [
  {id = 1, score = 3, state = Pending},
  {id = 2, score = 6, state = Running {attempt = 2}},
]"#;
const COMPUTED: &str = r#"from rows
select {
  id,
  original = score,
  score = score + $bonus,
  doubled = score * 2,
  state = match state {
    Pending => Running {attempt = 0},
    current => current,
  },
}
sort id"#;
const EXPANDED: &str = r#"from rows
derive original = score
derive score = score + $bonus
derive doubled = score * 2
derive state = match state {
  Pending => Running {attempt = 0},
  current => current,
}
select {id, original, score, doubled, state}
sort id"#;

#[test]
fn computed_select_matches_expansion_and_survives_durable_use() {
    let dir = TempDir::new();
    let path = dir.0.join("computed.redb");
    let mut disk = Engine::open_redb(&path).unwrap();
    let mut memory = Engine::memory();
    for engine in [&mut memory, &mut disk] {
        assert!(engine.execute(SETUP).ok);
    }
    drop(disk);
    let mut disk = Engine::open_redb(&path).unwrap();
    let mut baseline = None;
    for engine in [&mut memory, &mut disk] {
        let schema = engine.schema_info();
        for source in [COMPUTED, EXPANDED] {
            let prepared = engine.prepare(source).unwrap();
            let result = engine
                .execute_prepared(&prepared, BTreeMap::from([("bonus".into(), Value::Int(2))]));
            assert!(result.ok, "{}", result.message);
            assert_eq!(
                result
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>(),
                ["id", "original", "score", "doubled", "state"]
            );
            assert!(result.rows[0]["original"].cmp_eq(&Value::Int(3)));
            assert!(result.rows[0]["score"].cmp_eq(&Value::Int(5)));
            assert!(result.rows[0]["doubled"].cmp_eq(&Value::Int(10)));
            assert_eq!(
                result.rows[0]["state"].source_text(),
                "Running {attempt = 0}"
            );
            let json = serde_json::to_value(result).unwrap();
            if let Some(previous) = &baseline {
                assert_eq!(previous, &json);
            } else {
                baseline = Some(json);
            }
        }
        assert_eq!(engine.schema_info(), schema);
        assert!(engine.execute("from rows").rows[0]["score"].cmp_eq(&Value::Int(3)));
    }
}

#[test]
fn field_sets_preserve_order_scope_and_type_replacement() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let source = "from rows | derive {score = score + 1, label = score > 3, state = match state {Pending => 0, Running {attempt} => attempt}}";
    let result = engine.execute(source);
    assert!(result.ok, "{}", result.message);
    assert_eq!(
        result
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "score", "state", "label"]
    );
    assert!(result.rows[1]["state"].cmp_eq(&Value::Int(2)));
    let formatted = format_source(source).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(engine.execute(&formatted)).unwrap()
    );
    let duplicate_stages = "from rows | derive score = score + 1 | derive score = score * 2";
    let formatted = format_source(duplicate_stages).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert_eq!(
        serde_json::to_value(engine.execute(duplicate_stages)).unwrap(),
        serde_json::to_value(engine.execute(&formatted)).unwrap()
    );
}

#[test]
fn field_sets_reject_invalid_empty_table_queries_and_incomplete_source() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(SETUP.split("insert many").next().unwrap())
            .ok
    );
    for source in [
        "from rows | derive {a = 1, a = 2}",
        "from rows | select {a = 1, a = 2}",
        "from rows | derive {a = b, b = 2}",
        "from rows | select {a = missing}",
        "from rows | select {x = match state {Pending => 1}}",
        "from rows | select {x = 1} | filter score > 0",
        "from rows | select {a = 1 b = 2}",
        "from rows | derive {}",
        "from rows | select {}",
    ] {
        assert!(!engine.execute(source).ok, "accepted {source}");
    }
    for source in [
        "from rows | derive {",
        "from rows | select {a =",
        "from rows | select {a = 1,",
    ] {
        assert!(
            matches!(
                unionid::input_status(source),
                unionid::InputStatus::Incomplete(_)
            ),
            "{source}"
        );
    }
    for source in [COMPUTED, EXPANDED] {
        let formatted = format_source(source).unwrap();
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }
    assert_eq!(
        format_source(COMPUTED).unwrap(),
        format_source(EXPANDED).unwrap()
    );
}

#[test]
fn computed_pagination_keeps_original_primary_key_and_checks_shape() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let result =
        engine.execute("from rows | select {id, score = score + 1} | sort {score, id} | page 1");
    assert!(result.ok, "{}", result.message);
    let cursor = result.page.unwrap().next_cursor.unwrap();
    let next = engine.execute(&format!(
        "from rows | select {{id, score = score + 1}} | sort {{score, id}} | page 1 after {}",
        serde_json::to_string(&cursor).unwrap()
    ));
    assert!(next.ok, "{}", next.message);
    assert!(next.rows[0]["id"].cmp_eq(&Value::Int(2)));
    for source in [
        "from rows | derive id = 1 | sort id | page 1",
        "from rows | select {id = 1, score} | sort id | page 1",
        "from rows | derive id = match state {Pending => 0, Running {attempt} => attempt} | sort id | page 1",
        "from rows | sort id | select {id, score = score + 1} | page 1",
    ] {
        assert!(!engine.execute(source).ok, "accepted {source}");
    }
}

#[test]
fn computed_select_uses_the_same_tcp_parameter_and_nominal_wire_types() {
    let server = common::Server::start(&[]);
    assert!(unionid::cli::send_one(&server.addr, SETUP).unwrap().ok);
    let mut expected = None;
    for source in [COMPUTED, EXPANDED] {
        let request = unionid::ProtocolRequest::query("computed", source)
            .with_serde_param("bonus", &2_i64)
            .unwrap();
        let result = unionid::cli::send_request(&server.addr, &request).unwrap();
        assert!(result.ok, "{}", result.message);
        assert!(matches!(
            result.rows[0]["state"],
            unionid::WireValue::Named { .. }
        ));
        let json = serde_json::to_value(result).unwrap();
        if let Some(previous) = &expected {
            assert_eq!(previous, &json);
        } else {
            expected = Some(json);
        }
    }
}

#[test]
fn paging_applies_early_projections_before_later_derives() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let result = engine.execute("from rows | select {id, score = score + 1} | derive doubled = score * 2 | sort id | select {score, doubled} | page 1");
    assert!(result.ok, "{}", result.message);
    assert_eq!(result.rows[0].len(), 2);
    assert!(result.rows[0]["doubled"].cmp_eq(&Value::Int(8)));
    assert!(result.rows[0]["score"].cmp_eq(&Value::Int(4)));
    assert!(result.page.unwrap().next_cursor.is_some());
}
