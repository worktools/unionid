use std::path::PathBuf;

use unionid::Engine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "unionid-getting-started-{}.redb",
                std::process::id()
            ))
        });
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    {
        let mut engine = Engine::open_redb(&path)?;
        require_ok(engine.execute(include_str!("getting-started/01_setup.uid")))?;
        require_ok(engine.execute(include_str!("getting-started/02_running.uid")))?;
        require_ok(engine.execute(include_str!("getting-started/03_update.uid")))?;
    }
    let mut reopened = Engine::open_redb(&path)?;
    let response = require_ok(reopened.execute(include_str!("getting-started/04_reopen.uid")))?;
    println!("{}", serde_json::to_string_pretty(&response.rows)?);
    Ok(())
}

fn require_ok(response: unionid::QueryResponse) -> Result<unionid::QueryResponse, unionid::Error> {
    match response.error {
        Some(error) => Err(error),
        None => Ok(response),
    }
}
