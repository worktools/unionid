use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unionid::{Engine, Value};

#[derive(Debug, Serialize, Deserialize)]
struct Task {
    id: i64,
    title: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut database = Engine::memory();
    let setup = database.execute(
        r#"type Task =
  id int
  title text
table tasks Task
  key id"#,
    );
    if let Some(error) = setup.error {
        return Err(error.into());
    }

    let row = Task {
        id: 9_007_199_254_740_993,
        title: "quoted \"text\"\nwith | pipe".into(),
    };
    let insert = database.prepare("insert tasks $row\nreturning")?;
    let inserted = database.execute_prepared(
        &insert,
        BTreeMap::from([("row".into(), Value::from_serde(&row)?)]),
    );
    if let Some(error) = inserted.error {
        return Err(error.into());
    }

    let incoming = vec![
        Task {
            id: 9_007_199_254_740_993,
            title: "updated".into(),
        },
        Task {
            id: 2,
            title: "second".into(),
        },
    ];
    let bulk_upsert = database.prepare("upsert many tasks $rows\nreturning")?;
    let upserted = database.execute_prepared(
        &bulk_upsert,
        BTreeMap::from([("rows".into(), Value::from_serde(&incoming)?)]),
    );
    if let Some(error) = upserted.error {
        return Err(error.into());
    }
    println!("actions: {:?}", upserted.upsert_actions);

    let prepared = database.prepare("from tasks | sort id | select {id, title}")?;
    let result = database.query(&prepared, BTreeMap::new());
    if let Some(error) = result.error {
        return Err(error.into());
    }
    println!("{:#?}", result.typed_rows::<Task>()?);
    Ok(())
}
