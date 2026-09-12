include!(env!("UNIONID_GENERATED_QUERY"));

mod create {
    include!(env!("UNIONID_GENERATED_MUTATION"));
}

#[allow(dead_code)]
mod classify_v1 {
    include!(env!("UNIONID_GENERATED_QUERY_V1"));
}

#[allow(dead_code)]
mod classify_v2 {
    include!(env!("UNIONID_GENERATED_QUERY_V2"));
}

#[allow(dead_code)]
mod bundle {
    include!(env!("UNIONID_GENERATED_QUERY_BUNDLE"));
}

/// Execute generated v1/v2 query bindings against original and migrated catalogs.
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
    let classified = classify_v1::classify_task_v1(
        &mut engine,
        classify_v1::ClassifyTaskV1Params { id: 7 },
    )?
    .ok_or("v1 generated query did not return the inserted row")?;
    assert_eq!(classified.status, "running");

    let shared_task = bundle::Task {
        id: 9,
        title: "shared model".into(),
        state: bundle::State::Pending,
    };
    let bundled_insert = bundle::create_task::create_task(
        &mut engine,
        bundle::create_task::CreateTaskParams {
            task: shared_task.clone(),
        },
    )?;
    assert_eq!(bundled_insert.rows.state, shared_task.state);
    let bundled_row = bundle::find_task::find_task(
        &mut engine,
        bundle::find_task::FindTaskParams { id: shared_task.id },
    )?
    .ok_or("bundled query did not return the inserted row")?;
    assert_eq!(bundled_row.state, shared_task.state);
    assert_eq!(bundled_row.title, shared_task.title);

    let changed = engine.execute("type Extra = text");
    if !changed.ok {
        return Err(changed.message.into());
    }
    let error = find_task(&mut engine, FindTaskParams { id: 7 }).unwrap_err();
    assert_eq!(error.code, "E_SCHEMA_CHANGED");

    let evolved_path = std::env::var("UNIONID_EVOLVED_DB")?;
    let mut evolved = unionid::Engine::open_redb(evolved_path)?;
    let migrated = classify_v2::classify_task_v2(
        &mut evolved,
        classify_v2::ClassifyTaskV2Params { id: 6 },
    )?
    .ok_or("v2 generated query did not return the migrated v1 row")?;
    assert_eq!(migrated.status, "running");
    assert_eq!(migrated.priority, 0);
    let inserted = evolved.execute(
        "insert tasks {id = 8, title = \"done\", state = Complete, priority = 3}",
    );
    if !inserted.ok {
        return Err(inserted.message.into());
    }
    let old_error = classify_v1::classify_task_v1(
        &mut evolved,
        classify_v1::ClassifyTaskV1Params { id: 8 },
    )
    .unwrap_err();
    assert_eq!(old_error.code, "E_SCHEMA_CHANGED");
    let current = classify_v2::classify_task_v2(
        &mut evolved,
        classify_v2::ClassifyTaskV2Params { id: 8 },
    )?
    .ok_or("v2 generated query did not return the inserted row")?;
    assert_eq!(current.status, "complete");
    assert_eq!(current.priority, 3);
    assert_ne!(classify_v1::CLASSIFY_TASK_V1_DIGEST, classify_v2::CLASSIFY_TASK_V2_DIGEST);
    Ok(())
}
