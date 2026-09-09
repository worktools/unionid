//! Durable summaries and on-demand ADT content; no network or subscription runtime.
use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use unionid::{Engine, Error, QueryResponse, Result, Value};

const SCHEMA: &str = r#"
type Attachment = {name text, location text}
type Content =
  Plain text
  | Document {paragraphs list text, attachment option Attachment}
type Summary = {id int, owner text, revision int, title text}
type Detail = {id int, owner text, revision int, content Content}
table summaries Summary
  key id
table details Detail
  key id
create index summaries (owner, -revision, id)
"#;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Attachment {
    name: String,
    location: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Content {
    Plain(String),
    Document {
        paragraphs: Vec<String>,
        attachment: Option<Attachment>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Summary {
    id: i64,
    owner: String,
    revision: i64,
    title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Detail {
    id: i64,
    owner: String,
    revision: i64,
    content: Content,
}

fn checked(response: QueryResponse) -> Result<QueryResponse> {
    if response.ok {
        Ok(response)
    } else {
        Err(response
            .error
            .unwrap_or_else(|| Error::new("E_EXAMPLE", response.message)))
    }
}

// Called by the application's single writer after authorization. Revision assignment,
// optimistic concurrency and idempotency belong to that command layer, not this helper.
fn save(engine: &mut Engine, summary: &Summary, content: Content) -> Result<()> {
    let detail = Detail {
        id: summary.id,
        owner: summary.owner.clone(),
        revision: summary.revision,
        content,
    };
    // One request is one transaction: never publish the hot summary between these writes.
    checked(engine.execute_with_params(
        "upsert summaries $summary\nupsert details $detail",
        BTreeMap::from([
            ("summary".into(), Value::from_serde(summary)?),
            ("detail".into(), Value::from_serde(&detail)?),
        ]),
    ))?;
    Ok(())
}

fn summaries(engine: &mut Engine, authenticated_owner: &str) -> Result<Vec<Summary>> {
    let query = engine
        .prepare("from summaries | filter owner == $owner | sort {-revision, id} | take 12")?;
    checked(engine.execute_prepared(
        &query,
        BTreeMap::from([("owner".into(), Value::from_serde(&authenticated_owner)?)]),
    ))?
    .typed_rows()
}

// The caller supplies the owner from its authenticated session, not a client parameter.
// Content and actual revision come from the same row/read snapshot. No historical
// revision is promised: the caller compares the result with its expected revision.
fn fetch(engine: &mut Engine, authenticated_owner: &str, id: i64) -> Result<Option<Detail>> {
    let query =
        engine.prepare("from details | filter id == $id | filter owner == $owner | take 1")?;
    let rows: Vec<Detail> = checked(engine.execute_prepared(
        &query,
        BTreeMap::from([
            ("id".into(), Value::from_serde(&id)?),
            ("owner".into(), Value::from_serde(&authenticated_owner)?),
        ]),
    ))?
    .typed_rows()?;
    Ok(rows.into_iter().next())
}

fn initial_summary() -> Summary {
    Summary {
        id: 1,
        owner: "alice".into(),
        revision: 1,
        title: "Release notes".into(),
    }
}

fn document() -> Content {
    Content::Document {
        paragraphs: vec!["Typed content stays out of the hot summary.".into()],
        attachment: Some(Attachment {
            name: "notes.txt".into(),
            location: "objects/notes.txt".into(),
        }),
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("usage: cargo run --example cold_content -- <new-database-path>")?,
    );
    if path.try_exists()? {
        return Err("use a fresh path; this example does not modify existing databases".into());
    }
    let mut engine = Engine::open_redb(&path)?;
    checked(engine.execute(SCHEMA))?;
    let mut summary = initial_summary();
    save(&mut engine, &summary, document())?;
    assert_eq!(summaries(&mut engine, "alice")?, [summary.clone()]);
    assert!(fetch(&mut engine, "bob", summary.id)?.is_none());

    summary.revision = 2;
    save(
        &mut engine,
        &summary,
        Content::Plain("Updated content".into()),
    )?;
    // Simulate rebuilding runtime hot state after a successful write and reopen.
    drop(engine);
    let mut engine = Engine::open_redb(&path)?;
    assert_eq!(summaries(&mut engine, "alice")?, [summary]);
    let detail = fetch(&mut engine, "alice", 1)?.ok_or("missing persisted detail")?;
    assert_eq!(detail.revision, 2);
    assert_eq!(detail.content, Content::Plain("Updated content".into()));
    engine.check_integrity()?;
    println!("cold-content: atomic summaries, typed fetch, owner filtering and reopen passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_content_is_separate_from_bounded_owner_summaries() {
        let mut engine = Engine::memory();
        checked(engine.execute(SCHEMA)).unwrap();
        for id in 1..=20 {
            let mut summary = initial_summary();
            summary.id = id;
            summary.revision = id;
            save(&mut engine, &summary, document()).unwrap();
        }
        let hot = summaries(&mut engine, "alice").unwrap();
        assert_eq!(hot.len(), 12);
        assert_eq!(hot.first().unwrap().id, 20);
        assert_eq!(hot.last().unwrap().id, 9);
        assert!(summaries(&mut engine, "bob").unwrap().is_empty());
        assert!(fetch(&mut engine, "bob", 20).unwrap().is_none());
        let cold = fetch(&mut engine, "alice", 20).unwrap().unwrap();
        assert_eq!(cold.revision, 20);
        assert_eq!(cold.content, document());
    }

    #[test]
    fn later_constraint_failure_rolls_back_summary_and_content() {
        let mut engine = Engine::memory();
        checked(engine.execute(SCHEMA)).unwrap();
        let summary = initial_summary();
        save(&mut engine, &summary, document()).unwrap();
        let rejected = engine.execute(
            r#"update summaries | filter id == 1 | set revision = 2
update details | filter id == 1 | set revision = 2
insert summaries {id = 1, owner = "alice", revision = 3, title = "duplicate"}"#,
        );
        assert!(!rejected.ok);
        assert_eq!(summaries(&mut engine, "alice").unwrap(), [summary]);
        assert_eq!(fetch(&mut engine, "alice", 1).unwrap().unwrap().revision, 1);
    }
}
