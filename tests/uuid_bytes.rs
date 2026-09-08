mod common;

use common::TempDir;
use serde::{Deserialize, Serialize};
use unionid::{
    Engine, QueryAccessKind, Value, backup, format_source,
    protocol::Request,
    scalars::{Bytes, Uuid},
    server::execute_protocol_request,
};

const SETUP: &str = r#"type Artifact = {
  id uuid,
  digest bytes,
  payload bytes,
}

table artifacts Artifact
  key id

create unique index artifacts (digest)

insert many artifacts [
  {
    id = uuid "00000000-0000-0000-0000-000000000002",
    digest = bytes "00ff",
    payload = bytes "1000ff20",
  },
  {
    id = uuid "00000000-0000-0000-0000-000000000001",
    digest = bytes "0001",
    payload = bytes "00",
  },
]"#;

#[test]
fn uuid_and_bytes_literals_types_queries_and_formatting_are_native() {
    let mut engine = Engine::memory();
    let setup = format_source(SETUP).unwrap();
    assert_eq!(format_source(&setup).unwrap(), setup);
    let response = engine.execute(&setup);
    assert!(response.ok, "{}", response.message);

    let response = engine.execute(
        r#"from artifacts
filter contains payload bytes "00ff"
derive octets = length payload
sort id
select {id, digest, octets}"#,
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
    assert_eq!(
        response.rows[0]["id"].source_text(),
        "uuid \"00000000-0000-0000-0000-000000000002\""
    );
    assert!(response.rows[0]["octets"].cmp_eq(&Value::Int(4)));

    let grouped = engine.execute(
        r#"from artifacts
group digest (
  aggregate {rows = count}
)
sort digest"#,
    );
    assert!(grouped.ok, "{}", grouped.message);
    assert_eq!(grouped.rows.len(), 2);
    let extrema = engine.execute("from artifacts | aggregate {first = min id, last = max id}");
    assert!(extrema.ok, "{}", extrema.message);
    assert_eq!(
        extrema.rows[0]["first"].source_text(),
        "Some (uuid \"00000000-0000-0000-0000-000000000001\")"
    );

    let explain = engine.execute("explain from artifacts | filter digest == bytes \"00ff\"");
    assert!(explain.ok, "{}", explain.message);
    assert_eq!(
        explain.plan.unwrap().access.kind,
        QueryAccessKind::SecondaryIndexLookup
    );

    let empty = engine.execute(
        r#"from artifacts
filter contains payload bytes ""
sort id"#,
    );
    assert!(empty.ok, "{}", empty.message);
    assert_eq!(empty.rows.len(), 2);
}

#[test]
fn indexed_and_scanned_bytes_queries_have_identical_results() {
    let schema = r#"type Blob = {id int, digest bytes, payload bytes}
table blobs Blob
  key id
insert many blobs [
  {id = 1, digest = bytes "00ff", payload = bytes "0102"},
  {id = 2, digest = bytes "0001", payload = bytes "0304"},
  {id = 3, digest = bytes "00ff10", payload = bytes "0506"},
]"#;
    let query = r#"from blobs
filter digest == bytes "00ff"
sort id
select {id, digest, payload}"#;

    let mut scanned = Engine::memory();
    assert!(scanned.execute(schema).ok);
    let scanned_response = scanned.execute(query);
    assert!(scanned_response.ok, "{}", scanned_response.message);
    assert_eq!(
        scanned
            .execute(&format!("explain {query}"))
            .plan
            .unwrap()
            .access
            .kind,
        QueryAccessKind::OrderedScan
    );

    let mut indexed = Engine::memory();
    assert!(indexed.execute(schema).ok);
    assert!(indexed.execute("create index blobs (digest)").ok);
    let indexed_response = indexed.execute(query);
    assert!(indexed_response.ok, "{}", indexed_response.message);
    assert_eq!(
        indexed
            .execute(&format!("explain {query}"))
            .plan
            .unwrap()
            .access
            .kind,
        QueryAccessKind::SecondaryIndexLookup
    );
    let scanned_columns = scanned_response
        .columns
        .iter()
        .map(|column| (&column.name, &column.ty))
        .collect::<Vec<_>>();
    let indexed_columns = indexed_response
        .columns
        .iter()
        .map(|column| (&column.name, &column.ty))
        .collect::<Vec<_>>();
    assert_eq!(indexed_columns, scanned_columns);
    let source_rows = |response: &unionid::QueryResponse| {
        response
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|(name, value)| (name.clone(), value.source_text()))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        source_rows(&indexed_response),
        source_rows(&scanned_response)
    );
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NativeArtifact {
    id: Uuid,
    digest: Bytes,
    payload: Bytes,
}

#[test]
fn source_rust_wire_redb_and_backup_round_trip_uuid_bytes() {
    let dir = TempDir::new();
    let db = dir.0.join("uuid-bytes.redb");
    let archive = dir.0.join("uuid-bytes.backup.json");
    let restored = dir.0.join("uuid-bytes-restored.redb");
    let row = NativeArtifact {
        id: "f81d4fae-7dec-11d0-a765-00a0c91e6bf6".parse().unwrap(),
        digest: "0000ff".parse().unwrap(),
        payload: "deadbeef".parse().unwrap(),
    };
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        assert!(
            engine
                .execute(
                    "type Artifact = {id uuid, digest bytes, payload bytes}\ntable artifacts Artifact\n  key id\ncreate unique index artifacts (digest)",
                )
                .ok
        );
        let request = Request::query("native-insert", "insert artifacts $row\nreturning")
            .with_version(2)
            .unwrap()
            .with_serde_param("row", &row)
            .unwrap();
        let response = execute_protocol_request(&mut engine, request);
        assert!(response.ok, "{}", response.message);
        let decoded = response.typed_rows::<NativeArtifact>().unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], row);
        engine.check_integrity().unwrap();
    }
    {
        let mut reopened = Engine::open_redb(&db).unwrap();
        let response = execute_protocol_request(
            &mut reopened,
            Request::query("native-read", "from artifacts")
                .with_version(2)
                .unwrap(),
        );
        let decoded = response.typed_rows::<NativeArtifact>().unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0], row);
    }
    backup::create(&db, &archive).unwrap();
    backup::restore(&archive, &restored).unwrap();
    let mut restored = Engine::open_redb(&restored).unwrap();
    let response = execute_protocol_request(
        &mut restored,
        Request::query("native-restored", "from artifacts")
            .with_version(2)
            .unwrap(),
    );
    assert_eq!(response.typed_rows::<NativeArtifact>().unwrap(), [row]);
}

#[test]
fn uuid_primary_key_supports_page_upsert_update_and_delete() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let first = engine.execute("from artifacts | sort id | page 1");
    assert!(first.ok, "{}", first.message);
    let cursor = first.page.unwrap().next_cursor.unwrap();
    let second = engine.execute(&format!(
        "from artifacts | sort id | page 1 after {}",
        serde_json::to_string(&cursor).unwrap()
    ));
    assert!(second.ok, "{}", second.message);
    assert_eq!(second.rows.len(), 1);

    let upsert = engine.execute(
        r#"upsert artifacts {
  id = uuid "00000000-0000-0000-0000-000000000002",
  digest = bytes "00ff",
  payload = bytes "abcd",
}
returning"#,
    );
    assert!(upsert.ok, "{}", upsert.message);
    assert_eq!(upsert.rows[0]["payload"].source_text(), "bytes \"abcd\"");
    let update = engine.execute(
        r#"update artifacts
filter id == uuid "00000000-0000-0000-0000-000000000002"
set id = uuid "00000000-0000-0000-0000-000000000003""#,
    );
    assert!(update.ok, "{}", update.message);
    let delete = engine.execute(
        r#"delete artifacts
filter id == uuid "00000000-0000-0000-0000-000000000003""#,
    );
    assert!(delete.ok, "{}", delete.message);
}

#[test]
fn indexed_bytes_limit_is_enforced_for_creation_and_writes() {
    let within = "00".repeat(unionid::scalars::MAX_INDEXED_BYTES);
    let over = "00".repeat(unionid::scalars::MAX_INDEXED_BYTES + 1);

    let mut create = Engine::memory();
    assert!(
        create
            .execute("type Blob = {id int, value bytes}\ntable blobs Blob\n  key id")
            .ok
    );
    assert!(
        create
            .execute(&format!(
                "insert blobs {{id = 1, value = bytes \"{over}\"}}"
            ))
            .ok
    );
    let rejected = create.execute("create index blobs (value)");
    assert_eq!(rejected.error.unwrap().code, "E_INDEX_KEY_LIMIT");

    let mut write = Engine::memory();
    assert!(write.execute("type Blob = {id int, value bytes}\ntable blobs Blob\n  key id\ncreate index blobs (value)").ok);
    assert!(
        write
            .execute(&format!(
                "insert blobs {{id = 1, value = bytes \"{within}\"}}"
            ))
            .ok
    );
    let rejected = write.execute(&format!(
        "insert blobs {{id = 2, value = bytes \"{over}\"}}"
    ));
    assert_eq!(rejected.error.unwrap().code, "E_INDEX_KEY_LIMIT");
    assert_eq!(write.execute("from blobs").rows.len(), 1);
}

#[test]
fn invalid_uuid_and_bytes_source_forms_fail_closed() {
    let mut engine = Engine::memory();
    let canonical = engine.execute(
        "type Good = {id uuid}\ntable good Good\ninsert good {id = uuid \"F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6\"}\nfrom good",
    );
    assert!(canonical.ok, "{}", canonical.message);
    assert_eq!(
        canonical.rows[0]["id"].source_text(),
        "uuid \"f81d4fae-7dec-11d0-a765-00a0c91e6bf6\""
    );
    for source in [
        "type Bad = {id uuid, raw bytes}\ntable bad Bad\ninsert bad {id = uuid \"not-a-uuid\", raw = bytes \"00\"}",
        "type Bad = {id uuid, raw bytes}\ntable bad Bad\ninsert bad {id = uuid \"00000000-0000-0000-0000-000000000000\", raw = bytes \"ABC0\"}",
        "type Bad = {id uuid, raw bytes}\ntable bad Bad\ninsert bad {id = uuid \"00000000-0000-0000-0000-000000000000\", raw = bytes \"0\"}",
    ] {
        let response = engine.execute(source);
        assert!(!response.ok, "accepted {source}");
        assert!(engine.execute("from bad").error.is_some());
    }
}

#[test]
fn migration_parse_functions_convert_text_atomically() {
    let mut engine = Engine::memory();
    let response = engine.execute(
        r#"type Legacy = {id int, external_id text, digest text}
table records Legacy
  key id
insert records {
  id = 1,
  external_id = "F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6",
  digest = "00ff",
}
migration native_identifiers
  change field Legacy.external_id to uuid using old -> uuid_parse old
  change field Legacy.digest to bytes using old -> bytes_parse_hex old
from records"#,
    );
    assert!(response.ok, "{}", response.message);
    assert_eq!(
        response.rows[0]["external_id"].source_text(),
        "uuid \"f81d4fae-7dec-11d0-a765-00a0c91e6bf6\""
    );
    assert_eq!(response.rows[0]["digest"].source_text(), "bytes \"00ff\"");

    let mut invalid = Engine::memory();
    assert!(
        invalid
            .execute(
                "type Legacy = {id int, external_id text}\ntable records Legacy\n  key id\ninsert records {id = 1, external_id = \"invalid\"}",
            )
            .ok
    );
    let before = invalid.schema_info();
    let failed = invalid.execute(
        "migration invalid_uuid\n  change field Legacy.external_id to uuid using old -> uuid_parse old",
    );
    assert_eq!(failed.error.unwrap().code, "E_SCALAR_LITERAL");
    assert_eq!(invalid.schema_info(), before);
    assert_eq!(
        invalid.execute("from records").rows[0]["external_id"].source_text(),
        "\"invalid\""
    );
}
