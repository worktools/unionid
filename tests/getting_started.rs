mod common;

use common::{Server, TempDir};
use std::process::Command;
use unionid::{Engine, QueryResponse};

const SETUP: &str = include_str!("../examples/getting-started/01_setup.uid");
const BEFORE: &str = include_str!("../examples/getting-started/02_running.uid");
const UPDATE: &str = include_str!("../examples/getting-started/03_update.uid");
const REOPEN: &str = include_str!("../examples/getting-started/04_reopen.uid");

fn success(response: QueryResponse) -> QueryResponse {
    assert!(response.error.is_none(), "{:?}", response.error);
    response
}

fn run_cli(args: &[&str]) -> QueryResponse {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn typed_result(response: &QueryResponse) -> serde_json::Value {
    serde_json::to_value((&response.columns, &response.rows, &response.schema)).unwrap()
}

#[test]
fn getting_started_is_consistent_across_embedded_local_and_tcp_paths() {
    let mut memory = Engine::memory();
    success(memory.execute(SETUP));
    assert_eq!(success(memory.execute(BEFORE)).rows.len(), 1);
    assert_eq!(success(memory.execute(UPDATE)).affected_rows, Some(1));
    let embedded = success(memory.execute(REOPEN));
    assert_eq!(embedded.rows.len(), 2);

    let dir = TempDir::new();
    let db = dir.0.join("local.redb");
    let db_arg = db.to_str().unwrap();
    let setup = "examples/getting-started/01_setup.uid";
    let before = "examples/getting-started/02_running.uid";
    let update = "examples/getting-started/03_update.uid";
    let reopen = "examples/getting-started/04_reopen.uid";
    success(run_cli(&[
        "run", "--db", db_arg, "--file", setup, "--format", "json",
    ]));
    assert_eq!(
        success(run_cli(&[
            "run", "--db", db_arg, "--file", before, "--format", "json"
        ]))
        .rows
        .len(),
        1
    );
    assert_eq!(
        success(run_cli(&[
            "run", "--db", db_arg, "--file", update, "--format", "json"
        ]))
        .affected_rows,
        Some(1)
    );
    let local = success(run_cli(&[
        "run", "--db", db_arg, "--file", reopen, "--format", "json",
    ]));
    let check = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["check", "--db", db_arg])
        .status()
        .unwrap();
    assert!(check.success());

    let tcp_db = dir.0.join("tcp.redb");
    let mut server = Server::start(&["--db", tcp_db.to_str().unwrap()]);
    let base = |file: &str| {
        run_cli(&[
            "cli",
            "--addr",
            &server.addr,
            "--file",
            file,
            "--format",
            "json",
            "--no-history",
        ])
    };
    success(base(setup));
    success(base(update));
    let remote = success(base(reopen));
    server.shutdown();

    assert_eq!(typed_result(&embedded), typed_result(&local));
    assert_eq!(typed_result(&remote), typed_result(&local));
}
