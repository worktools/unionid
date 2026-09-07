mod common;

use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::time::Instant;

use common::{Server, TempDir};
use serde::{Deserialize, Serialize};
use unionid::cli;
use unionid::protocol::Request;
use unionid::server::execute_protocol_request_until;
use unionid::{Engine, PageSpec};

const QUERY: &str = "from tasks\nsort {-priority, id}";
const SETUP: &str = r#"type Task =
  id int
  priority int
  title text
table tasks Task
  key id
insert tasks {id = 1, priority = 3, title = "one"}
insert tasks {id = 2, priority = 3, title = "two"}
insert tasks {id = 3, priority = 2, title = "three"}
insert tasks {id = 4, priority = 1, title = "four"}
insert tasks {id = 5, priority = 1, title = "five"}"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Task {
    id: i64,
    priority: i64,
    title: String,
}

fn expected() -> Vec<Task> {
    vec![
        Task {
            id: 1,
            priority: 3,
            title: "one".into(),
        },
        Task {
            id: 2,
            priority: 3,
            title: "two".into(),
        },
        Task {
            id: 3,
            priority: 2,
            title: "three".into(),
        },
        Task {
            id: 4,
            priority: 1,
            title: "four".into(),
        },
        Task {
            id: 5,
            priority: 1,
            title: "five".into(),
        },
    ]
}

#[test]
fn rust_and_tcp_builders_traverse_typed_pages_across_reopen() {
    let mut embedded = Engine::memory();
    assert!(embedded.execute(SETUP).ok);
    let mut embedded_rows = Vec::new();
    let mut page = PageSpec::forward(2);
    loop {
        let typed = embedded
            .execute_page(QUERY, page)
            .typed_page::<Task>()
            .unwrap();
        embedded_rows.extend(typed.rows);
        let Some(next) = typed.page.next_page() else {
            break;
        };
        page = next;
    }
    assert_eq!(embedded_rows, expected());
    assert_eq!(
        embedded
            .execute(QUERY)
            .typed_page::<Task>()
            .unwrap_err()
            .code,
        "E_PAGE_SHAPE"
    );

    let dir = TempDir::new();
    let database = dir.0.join("pages.redb");
    let database_arg = database.to_string_lossy().into_owned();
    let mut server = Server::start(&["--db", &database_arg]);
    assert!(cli::send_one(&server.addr, SETUP).unwrap().ok);

    let first = cli::send_request(
        &server.addr,
        &Request::query("tcp-page-1", QUERY).with_page(PageSpec::forward(2)),
    )
    .unwrap()
    .typed_page::<Task>()
    .unwrap();
    assert_eq!(first.rows, expected()[..2]);
    let next = first.page.next_page().unwrap();

    server.shutdown();
    let server = Server::start(&["--db", &database_arg]);
    let second = cli::send_request(
        &server.addr,
        &Request::query("tcp-page-2", QUERY).with_page(next),
    )
    .unwrap()
    .typed_page::<Task>()
    .unwrap();
    assert_eq!(second.rows, expected()[2..4]);
    let previous = second.page.previous_page().unwrap();
    let next = second.page.next_page().unwrap();

    let back = cli::send_request(
        &server.addr,
        &Request::query("tcp-page-back", QUERY).with_page(previous),
    )
    .unwrap()
    .typed_page::<Task>()
    .unwrap();
    assert_eq!(back.rows, expected()[..2]);

    let last = cli::send_request(
        &server.addr,
        &Request::query("tcp-page-3", QUERY).with_page(next),
    )
    .unwrap()
    .typed_page::<Task>()
    .unwrap();
    assert_eq!(last.rows, expected()[4..]);
    assert!(last.page.next_page().is_none());

    let mut disconnected = TcpStream::connect(&server.addr).unwrap();
    let request = Request::query("tcp-disconnect", QUERY).with_page(PageSpec::forward(1));
    serde_json::to_writer(&mut disconnected, &request).unwrap();
    disconnected.write_all(b"\n").unwrap();
    disconnected.shutdown(Shutdown::Both).unwrap();
    drop(disconnected);

    let healthy = cli::send_request(
        &server.addr,
        &Request::query("tcp-after-disconnect", QUERY).with_page(PageSpec::forward(1)),
    )
    .unwrap();
    assert!(healthy.ok, "{}", healthy.message);
}

#[test]
fn adapter_owned_deadline_returns_a_stable_error_without_a_cursor() {
    let mut engine = Engine::memory();
    assert!(engine.execute(SETUP).ok);
    let response = execute_protocol_request_until(
        &mut engine,
        Request::query("expired-page", QUERY).with_page(PageSpec::forward(2)),
        Instant::now() - std::time::Duration::from_secs(1),
    );
    assert!(!response.ok);
    assert_eq!(response.error.unwrap().code, "E_TIMEOUT");
    assert!(response.page.is_none());

    let healthy = execute_protocol_request_until(
        &mut engine,
        Request::query("after-timeout", QUERY).with_page(PageSpec::forward(2)),
        Instant::now() + std::time::Duration::from_secs(1),
    );
    assert!(healthy.ok, "{}", healthy.message);
}
