use unionid::{ConcurrentEngine, Engine};

fn main() {
    let engine = ConcurrentEngine::new(Engine::memory());
    let _ = engine.execute("create table health (id int)");
    let _ = engine.execute("from health");

    // The application decides where and how to expose this text. Put any HTTP
    // route behind the same authentication and network boundary as admin APIs.
    print!("{}", engine.metrics_snapshot().prometheus_text());
}
