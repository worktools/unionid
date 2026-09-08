use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unionid::{
    Engine, Value,
    protocol::Request,
    scalars::{Bytes, Date, Decimal, Duration, Timestamp, Uuid},
    server::execute_protocol_request,
};

#[derive(Debug, Serialize, Deserialize)]
struct Task {
    id: i64,
    title: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Content {
    id: Uuid,
    digest: Bytes,
    preview: Bytes,
    published_on: Date,
    created_at: Timestamp,
    retry_after: Duration,
    price: Decimal,
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

    let setup = database.execute(
        "type Content = {id uuid, digest bytes, preview bytes, published_on date, created_at timestamp, retry_after duration, price decimal 18 2}\n\
         table content Content\n  key id\n\
         create unique index content (digest)",
    );
    if let Some(error) = setup.error {
        return Err(error.into());
    }
    let content = Content {
        id: "018f67a4-2f44-7aa3-8f2b-14f7a2f66210".parse()?,
        digest: "00ff10".parse()?,
        preview: "89504e470d0a1a0a".parse()?,
        published_on: "2026-09-08".parse()?,
        created_at: "2026-09-08T09:30:15.123456+08:00".parse()?,
        retry_after: "30seconds".parse()?,
        price: Decimal::parse("19.9", 18, 2)?,
    };
    let request = Request::query("content-insert", "insert content $row\nreturning")
        .with_version(2)?
        .with_serde_param("row", &content)?;
    let response = execute_protocol_request(&mut database, request);
    if let Some(error) = response.error {
        return Err(error.into());
    }
    assert_eq!(response.typed_rows::<Content>()?, [content]);
    Ok(())
}
