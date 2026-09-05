#![allow(dead_code)]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "unionid-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Server {
    child: Child,
    pub addr: String,
}
impl Server {
    pub fn start(options: &[&str]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_unionid"))
            .args(["server", "--addr", "127.0.0.1:0"])
            .args(options)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            addr: String::new(),
        };
        let stdout = server.child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server startup deadline")
            .unwrap();
        server.addr = line
            .trim()
            .strip_prefix("unionid server listening on ")
            .expect("server readiness message")
            .to_string();
        server
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn wait(child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("CLI failed to exit before deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
