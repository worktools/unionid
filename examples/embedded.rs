use unionid::Engine;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut database = Engine::memory();
    let result = database.execute(include_str!("tasks.uid"));
    if let Some(error) = result.error {
        return Err(error.into());
    }
    println!("{}", serde_json::to_string_pretty(&result.rows)?);
    Ok(())
}
