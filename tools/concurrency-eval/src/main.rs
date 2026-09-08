use std::error::Error;
use std::process::{Command, ExitCode};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use unionid::Engine;
use unionid::server::ConcurrentEngine;

type AnyResult<T> = Result<T, Box<dyn Error>>;

#[derive(Debug, Serialize, Deserialize)]
struct Measurement {
    mode: String,
    rows: usize,
    readers: usize,
    queries_per_reader: usize,
    elapsed_micros: u128,
    queries_per_second: f64,
    peak_rss_bytes: u64,
    peak_active_reads: usize,
}

#[derive(Serialize)]
struct Report {
    rows: usize,
    readers: usize,
    queries_per_reader: usize,
    serialized: Measurement,
    snapshots: Measurement,
    throughput_ratio: f64,
    peak_rss_ratio: f64,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("concurrency evaluation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> AnyResult<()> {
    let args = std::env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, mode, rows, readers, queries] if mode == "measure" => {
            measure(rows.parse()?, readers.parse()?, queries.parse()?, false)
        }
        [_, mode, rows, readers, queries] if mode == "measure-snapshots" => {
            measure(rows.parse()?, readers.parse()?, queries.parse()?, true)
        }
        [_, rows, readers, queries] => evaluate(rows.parse()?, readers.parse()?, queries.parse()?),
        _ => Err("usage: unionid-concurrency-eval <rows> <readers> <queries-per-reader>".into()),
    }
}

fn evaluate(rows: usize, readers: usize, queries: usize) -> AnyResult<()> {
    validate(rows, readers, queries)?;
    let executable = std::env::current_exe()?;
    let serialized = child(&executable, "measure", rows, readers, queries)?;
    let snapshots = child(&executable, "measure-snapshots", rows, readers, queries)?;
    let report = Report {
        rows,
        readers,
        queries_per_reader: queries,
        throughput_ratio: snapshots.queries_per_second / serialized.queries_per_second,
        peak_rss_ratio: snapshots.peak_rss_bytes as f64 / serialized.peak_rss_bytes as f64,
        serialized,
        snapshots,
    };
    serde_json::to_writer_pretty(std::io::stdout(), &report)?;
    println!();
    Ok(())
}

fn child(
    executable: &std::path::Path,
    mode: &str,
    rows: usize,
    readers: usize,
    queries: usize,
) -> AnyResult<Measurement> {
    let output = Command::new(executable)
        .args([
            mode,
            &rows.to_string(),
            &readers.to_string(),
            &queries.to_string(),
        ])
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn measure(rows: usize, readers: usize, queries: usize, snapshots: bool) -> AnyResult<()> {
    validate(rows, readers, queries)?;
    let engine = populated(rows)?;
    let barrier = Arc::new(Barrier::new(readers + 1));
    let started = Instant::now();
    let (handles, peak_active_reads) = if snapshots {
        let engine = ConcurrentEngine::new(engine);
        let handles = (0..readers)
            .map(|_| {
                let engine = engine.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..queries {
                        let response = engine.execute(query());
                        assert!(response.ok, "{}", response.message);
                        assert_eq!(response.rows.len(), 1);
                    }
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().map_err(|_| "snapshot reader panicked")?;
        }
        let peak = engine.stats().peak_active_reads;
        (Vec::new(), peak)
    } else {
        let engine = Arc::new(Mutex::new(engine));
        let handles = (0..readers)
            .map(|_| {
                let engine = Arc::clone(&engine);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..queries {
                        let response = engine.lock().unwrap().execute(query());
                        assert!(response.ok, "{}", response.message);
                        assert_eq!(response.rows.len(), 1);
                    }
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        (handles, 1)
    };
    for handle in handles {
        handle.join().map_err(|_| "serialized reader panicked")?;
    }
    let elapsed = started.elapsed();
    let total = readers * queries;
    let report = Measurement {
        mode: if snapshots { "snapshots" } else { "serialized" }.into(),
        rows,
        readers,
        queries_per_reader: queries,
        elapsed_micros: elapsed.as_micros(),
        queries_per_second: total as f64 / elapsed.as_secs_f64(),
        peak_rss_bytes: peak_rss_bytes()?,
        peak_active_reads,
    };
    serde_json::to_writer(std::io::stdout(), &report)?;
    Ok(())
}

fn populated(rows: usize) -> AnyResult<Engine> {
    let mut engine = Engine::memory();
    let response = engine.execute("create table samples (id int, category int, value int)");
    if !response.ok {
        return Err(response.message.into());
    }
    for start in (0..rows).step_by(2_000) {
        let end = (start + 2_000).min(rows);
        let values = (start..end)
            .map(|id| format!("{{id: {id}, category: {}, value: {}}}", id % 31, id % 997))
            .collect::<Vec<_>>()
            .join(",");
        let response = engine.execute(&format!("insert many samples [{values}]"));
        if !response.ok {
            return Err(response.message.into());
        }
    }
    Ok(engine)
}

fn query() -> &'static str {
    "from samples | derive score = value + category | sort -score | aggregate {total = sum score, maximum = max score}"
}

fn validate(rows: usize, readers: usize, queries: usize) -> AnyResult<()> {
    if rows == 0 || rows > 100_000 {
        return Err("rows must be between 1 and 100000".into());
    }
    if readers == 0 || readers > 8 {
        return Err("readers must be between 1 and 8".into());
    }
    if queries == 0 || queries > 100 {
        return Err("queries-per-reader must be between 1 and 100".into());
    }
    Ok(())
}

fn peak_rss_bytes() -> AnyResult<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the provided structure on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: getrusage returned success.
    let peak = u64::try_from(unsafe { usage.assume_init() }.ru_maxrss)?;
    Ok(if cfg!(target_os = "macos") {
        peak
    } else {
        peak.saturating_mul(1024)
    })
}
