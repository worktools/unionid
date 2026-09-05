mod common;
use common::{Server, TempDir, wait};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use unionid::{Engine, QueryResponse, cli};

#[test]
fn local_cli_executes_file_and_reports_errors_with_nonzero_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--file", "examples/tasks.uid", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.rows.len(), 1);
    let output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--query", "from absent", "--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.error.unwrap().code, "E_TABLE");
}

#[test]
fn cli_eof_exits_and_query_does_not_wait_for_stdin() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--memory"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait(&mut child);
    assert!(child.wait().unwrap().success());
    let mut child = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--query", "create table t (id int)"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    wait(&mut child);
    assert!(child.wait().unwrap().success());
}

#[test]
fn multiline_stdin_is_a_single_atomic_batch() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--memory", "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_bytes!("../examples/tasks.uid"))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn tcp_and_embedded_engine_have_identical_typed_results() {
    let server = Server::start(&[]);
    let script = include_str!("../examples/tasks.uid");
    let remote = cli::send_one(&server.addr, script).unwrap();
    let local = Engine::memory().execute(script);
    assert!(remote.ok, "{}", remote.message);
    assert_eq!(
        serde_json::to_value(remote).unwrap(),
        serde_json::to_value(local).unwrap()
    );
    let error = cli::send_one(&server.addr, "from tasks | select typo").unwrap();
    assert!(!error.ok);
}

#[test]
fn legacy_line_protocol_and_cli_work_against_the_same_engine() {
    let server = Server::start(&[]);
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream
        .write_all(b"create table users (id int, name text)\n")
        .unwrap();
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).unwrap();
    assert!(serde_json::from_str::<QueryResponse>(&response).unwrap().ok);
    for query in [
        "insert users {id:1,name:\"alice\"}",
        "from users | filter id = 1 | select name",
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["cli", "--addr", &server.addr, "--query", query])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    let result = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["cli", "--addr", &server.addr, "--query", "from absent"])
        .output()
        .unwrap();
    assert!(!result.status.success());
}

#[test]
fn server_restart_recovers_typed_data_and_index() {
    let dir = TempDir::new();
    let wal = dir.0.join("db.wal");
    let snapshot = dir.0.join("db.snapshot");
    let args = [
        "--wal-path",
        wal.to_str().unwrap(),
        "--snapshot-path",
        snapshot.to_str().unwrap(),
        "--snapshot-every",
        "1",
    ];
    {
        let server = Server::start(&args);
        let response = cli::send_one(&server.addr, include_str!("../examples/tasks.uid")).unwrap();
        assert!(response.ok);
    }
    let server = Server::start(&args);
    let response = cli::send_one(&server.addr, "from tasks | filter id == 1").unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 1);
}

#[test]
fn concurrent_requests_are_serialized_and_invalid_json_is_reported() {
    let server = Server::start(&[]);
    assert!(
        cli::send_one(&server.addr, "type R =\n  id int\ntable t R\n  key id")
            .unwrap()
            .ok
    );
    let threads = (0..16)
        .map(|id| {
            let addr = server.addr.clone();
            std::thread::spawn(move || {
                assert!(
                    cli::send_one(&addr, &format!("insert t {{id = {id}}}"))
                        .unwrap()
                        .ok
                )
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(
        cli::send_one(&server.addr, "from t").unwrap().rows.len(),
        16
    );
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    stream.write_all(b"{\"query\":7}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<QueryResponse>(&line)
            .unwrap()
            .error
            .unwrap()
            .code,
        "E_PROTOCOL"
    );
}
