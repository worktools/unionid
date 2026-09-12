use std::time::Duration;

use unionid::asynchronous::http::{self, Config};
use unionid::{ConcurrentEngine, Engine};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let database = std::env::args_os()
        .nth(1)
        .ok_or("usage: cargo run --features http --example http_service -- <database.redb>")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    let engine = ConcurrentEngine::new(Engine::open_redb(database)?);
    let app = http::router(
        engine,
        Config {
            request_timeout: Duration::from_secs(10),
        },
    );

    println!(
        "unionid HTTP service listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}
