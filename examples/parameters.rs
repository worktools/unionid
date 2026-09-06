use std::collections::BTreeMap;

use unionid::{Engine, Value};

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

    let row = Value::Record(BTreeMap::from([
        ("id".into(), Value::Int(9_007_199_254_740_993)),
        (
            "title".into(),
            Value::Text("quoted \"text\"\nwith | pipe".into()),
        ),
    ]));
    let inserted =
        database.execute_with_params("insert tasks $row", BTreeMap::from([("row".into(), row)]));
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
    println!("{}", serde_json::to_string_pretty(&result.rows)?);
    Ok(())
}
