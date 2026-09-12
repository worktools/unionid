mod common;

use common::Server;
use serde::{Deserialize, Serialize};
use unionid::{ProtocolRequest, TcpClient};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Item {
    id: i64,
    label: String,
}

#[test]
fn client_runs_typed_queries_and_mutations() {
    let server = Server::start(&[]);
    let mut client = TcpClient::connect(&server.addr).unwrap();

    let setup = client
        .query("setup", "create table items (id int, label text)")
        .unwrap();
    assert!(setup.ok, "{}", setup.message);

    let insert = client
        .query("insert", "insert items { id = 1, label = \"a\" }")
        .unwrap();
    assert!(insert.ok, "{}", insert.message);
    assert_eq!(insert.affected_rows, Some(1));

    let read = client.query("read", "from items | sort id").unwrap();
    assert!(read.ok, "{}", read.message);
    assert_eq!(
        read.typed_rows::<Item>().unwrap(),
        [Item {
            id: 1,
            label: "a".into(),
        }]
    );
}

#[test]
fn client_replays_idempotent_mutations_across_reconnect() {
    let server = Server::start(&[]);
    let request = ProtocolRequest::query("mut", "insert items { id = 1, label = \"a\" }")
        .with_idempotency_key("client-key")
        .unwrap();

    let mut first = TcpClient::connect(&server.addr).unwrap();
    let setup = first
        .query("setup", "create table items (id int, label text)")
        .unwrap();
    assert!(setup.ok, "{}", setup.message);
    let insert = first.request(&request).unwrap();
    assert!(insert.ok, "{}", insert.message);
    assert_eq!(
        insert.idempotency.as_ref().map(|meta| meta.replayed),
        Some(false)
    );
    drop(first);

    let mut second = TcpClient::connect(&server.addr).unwrap();
    let replay = second.request(&request).unwrap();
    assert!(replay.ok, "{}", replay.message);
    assert_eq!(
        replay.idempotency.as_ref().map(|meta| meta.replayed),
        Some(true)
    );

    // Exactly one row exists after the replay.
    let rows = second.query("read", "from items").unwrap();
    assert!(rows.ok, "{}", rows.message);
    assert_eq!(rows.typed_rows::<Item>().unwrap().len(), 1);
}

#[test]
fn client_retry_requires_an_idempotency_key() {
    let server = Server::start(&[]);
    let mut client = TcpClient::connect(&server.addr).unwrap();
    let request = ProtocolRequest::query("plain", "from items");
    let error = client
        .request_retrying(&server.addr, &request, 3)
        .unwrap_err();
    assert_eq!(error.code, "E_CONFIG");
}
