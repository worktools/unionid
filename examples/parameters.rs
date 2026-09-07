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

    let prepared = database.prepare("from tasks | filter id == $id | select {id, title}")?;
    let result = database.query(
        &prepared,
        BTreeMap::from([("id".into(), Value::Int(9_007_199_254_740_993))]),
    );
    if let Some(error) = result.error {
        return Err(error.into());
    }
    println!("{:#?}", result.typed_rows::<Task>()?);
    Ok(())
}
