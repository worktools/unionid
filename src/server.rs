use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::db::{Database, QueryResponse};
use crate::query::parse_statement;
use crate::snapshot::SnapshotStore;
use crate::wal::Wal;

pub fn run_server(
    addr: &str,
    wal_path: Option<PathBuf>,
    snapshot_path: Option<PathBuf>,
    snapshot_every: usize,
) -> Result<(), String> {
    let wal = if let Some(path) = wal_path {
        Some(Arc::new(Wal::new(path)?))
    } else {
        None
    };

    let snapshot = if let Some(path) = snapshot_path {
        Some(Arc::new(SnapshotStore::new(path)?))
    } else {
        None
    };

    let mut boot_db = if let Some(snapshot) = &snapshot {
        match snapshot.load()? {
            Some(db) => {
                println!("snapshot loaded from {}", snapshot.path().display());
                db
            }
            None => Database::default(),
        }
    } else {
        Database::default()
    };

    if let Some(wal) = &wal {
        let recovered = wal.replay_into(&mut boot_db)?;
        println!(
            "wal replay completed: {} statement(s) from {}",
            recovered,
            wal.path().display()
        );
    }

    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr} failed: {e}"))?;
    let shared_db = Arc::new(Mutex::new(boot_db));
    let mutation_count = Arc::new(AtomicUsize::new(0));

    println!("unionid server listening on {addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let db = Arc::clone(&shared_db);
                let wal = wal.clone();
                let snapshot = snapshot.clone();
                let mutation_count = mutation_count.clone();
                thread::spawn(move || {
                    if let Err(err) =
                        handle_client(stream, db, wal, snapshot, snapshot_every, mutation_count)
                    {
                        eprintln!("client error: {err}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }

    Ok(())
}

fn handle_client(
    stream: TcpStream,
    db: Arc<Mutex<Database>>,
    wal: Option<Arc<Wal>>,
    snapshot: Option<Arc<SnapshotStore>>,
    snapshot_every: usize,
    mutation_count: Arc<AtomicUsize>,
) -> Result<(), String> {
    let peer = stream
        .peer_addr()
        .map(|x| x.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("clone stream: {e}"))?;
    let mut reader = BufReader::new(stream);

    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("read line: {e}"))?;

        if n == 0 {
            break;
        }

        let request = line.trim();
        if request.is_empty() {
            continue;
        }

        let response =
            if request.eq_ignore_ascii_case("quit") || request.eq_ignore_ascii_case("exit") {
                QueryResponse::ok_message("bye")
            } else {
                match parse_statement(request) {
                    Ok(stmt) => {
                        let is_mutating = stmt.is_mutating();
                        let mut guard = db
                            .lock()
                            .map_err(|_| "database lock poisoned".to_string())?;
                        let response = guard.execute(stmt);

                        if is_mutating && response.ok {
                            if let Some(wal) = &wal {
                                wal.append(request)
                                    .map_err(|e| format!("wal append failed: {e}"))?;
                            }

                            if snapshot_every > 0 {
                                let current = mutation_count.fetch_add(1, Ordering::SeqCst) + 1;
                                if current % snapshot_every == 0 {
                                    if let Some(snapshot) = &snapshot {
                                        snapshot.save(&guard)?;
                                        println!(
                                            "snapshot saved to {} after {} mutation(s)",
                                            snapshot.path().display(),
                                            current
                                        );

                                        if let Some(wal) = &wal {
                                            wal.truncate()?;
                                            println!("wal truncated after snapshot");
                                        }
                                    }
                                }
                            }
                        }

                        response
                    }
                    Err(e) => QueryResponse::err(e),
                }
            };

        let json =
            serde_json::to_string(&response).map_err(|e| format!("serialize response: {e}"))?;
        writer
            .write_all(json.as_bytes())
            .map_err(|e| format!("write response: {e}"))?;
        writer
            .write_all(b"\n")
            .map_err(|e| format!("write newline: {e}"))?;
        writer.flush().map_err(|e| format!("flush response: {e}"))?;

        if request.eq_ignore_ascii_case("quit") || request.eq_ignore_ascii_case("exit") {
            break;
        }
    }

    println!("client disconnected: {peer}");
    Ok(())
}
