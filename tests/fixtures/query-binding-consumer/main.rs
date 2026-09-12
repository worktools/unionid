include!(env!("UNIONID_GENERATED_QUERY"));

mod create {
    include!(env!("UNIONID_GENERATED_MUTATION"));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = unionid::Engine::memory();
    let schema = std::fs::read_to_string(std::env::var("UNIONID_SCHEMA")?)?;
    let setup = engine.execute(&schema);
    if !setup.ok {
        return Err(setup.message.into());
    }
    let inserted = create::create_task(
        &mut engine,
        create::CreateTaskParams {
            task: create::Task {
                id: 7,
                title: "typed".into(),
                state: create::State::Running { attempt: 2 },
            },
        },
    )?;
    assert_eq!(inserted.affected_rows, 1);
    assert_eq!(inserted.rows.id, 7);
    assert!(create::CREATE_TASK_DIGEST.starts_with("sha256:"));
    let row = find_task(&mut engine, FindTaskParams { id: 7 })?
        .ok_or("generated query did not return the inserted row")?;
    assert_eq!(row.id, 7);
    assert_eq!(row.title, "typed");
    assert_eq!(row.state, State::Running { attempt: 2 });

    let changed = engine.execute("type Extra = text");
    if !changed.ok {
        return Err(changed.message.into());
    }
    let error = find_task(&mut engine, FindTaskParams { id: 7 }).unwrap_err();
    assert_eq!(error.code, "E_SCHEMA_CHANGED");
    Ok(())
}
