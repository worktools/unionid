use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unionid::{Engine, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Job {
    id: i64,
    state: State,
    meta: Meta,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum State {
    Idle,
    Label(String),
    Pair(i64, String),
    Running { worker: String, attempt: i64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Meta {
    checkpoint: Option<(i64, String)>,
    tags: Vec<String>,
}

fn job(id: i64, state: State, checkpoint: Option<(i64, &str)>) -> Job {
    Job {
        id,
        state,
        meta: Meta {
            checkpoint: checkpoint.map(|(number, label)| (number, label.into())),
            tags: vec!["typed".into(), format!("job-{id}")],
        },
    }
}

#[test]
fn native_rust_adts_round_trip_through_prepared_dml_and_queries() {
    let mut engine = Engine::memory();
    let setup = engine.execute(
        r#"type State =
  Idle
  | Label text
  | Pair(int, text)
  | Running {worker text, attempt int}

type Meta =
  checkpoint option (int, text)
  tags list text

type Job =
  id int
  state State
  meta Meta

table jobs Job
  key id"#,
    );
    assert!(setup.ok, "{}", setup.message);

    let values = vec![
        job(1, State::Idle, None),
        job(2, State::Label("queued".into()), Some((10, "a"))),
        job(3, State::Pair(7, "pair".into()), None),
        job(
            4,
            State::Running {
                worker: "local".into(),
                attempt: 2,
            },
            Some((20, "b")),
        ),
    ];

    let bulk = engine.prepare("insert many jobs $rows\nreturning").unwrap();
    let inserted = engine.execute_prepared(
        &bulk,
        BTreeMap::from([("rows".into(), Value::from_serde(&values).unwrap())]),
    );
    assert!(inserted.ok, "{}", inserted.message);
    assert_eq!(inserted.typed_rows::<Job>().unwrap(), values);

    let query = engine
        .prepare("from jobs | filter id >= $minimum | sort id")
        .unwrap();
    let selected = engine.execute_prepared(
        &query,
        BTreeMap::from([("minimum".into(), Value::from_serde(&2_i64).unwrap())]),
    );
    assert!(selected.ok, "{}", selected.message);
    assert_eq!(selected.typed_rows::<Job>().unwrap(), values[1..]);

    let upsert = engine.prepare("upsert jobs $row\nreturning").unwrap();
    let replacement = job(
        4,
        State::Running {
            worker: "remote".into(),
            attempt: 3,
        },
        None,
    );
    let updated = engine.execute_prepared(
        &upsert,
        BTreeMap::from([("row".into(), Value::from_serde(&replacement).unwrap())]),
    );
    assert!(updated.ok, "{}", updated.message);
    assert_eq!(updated.typed_rows::<Job>().unwrap(), [replacement]);
}

#[test]
fn serde_adapter_reports_unsupported_values_and_typed_row_errors() {
    let integer = Value::from_serde(&u64::MAX).unwrap_err();
    assert_eq!(integer.code, "E_SERDE");
    assert!(integer.message.contains("exceeds unionid int range"));

    let float = Value::from_serde(&f64::NAN).unwrap_err();
    assert_eq!(float.code, "E_SERDE");
    assert!(float.message.contains("non-finite"));

    let keyed = BTreeMap::from([(1_i64, "value")]);
    let key = Value::from_serde(&keyed).unwrap_err();
    assert_eq!(key.code, "E_SERDE");
    assert!(key.message.contains("text keys"));

    let mut engine = Engine::memory();
    assert!(
        engine
            .execute("create table values (id int, label text)\ninsert values {id = 1, label = \"x\"}\nfrom values")
            .ok
    );
    let response = engine.execute("from values");
    let error = response.typed_rows::<BTreeMap<String, i64>>().unwrap_err();
    assert_eq!(error.code, "E_SERDE");
    assert!(error.message.contains("decode result row 1"));
}
