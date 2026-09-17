unionid_query::queries! {
    schema "tests/schema.unid"

    query find_pending {
        from tasks
        filter state == Pending and priority >= $min_priority
        sort {-priority, id}
        select {id, title, state}
        take 20
    }

    query reprioritize_task {
        update tasks
        filter id == $id
        set priority = $priority
        returning {id, priority, state}
    }

    query due_before {
        from tasks
        filter due_on < @2026-10-01
        select {id, due_on}
    }
}

#[test]
fn inline_queries_generate_shared_typed_bindings() {
    let mut engine = unionid::Engine::memory();
    let schema = include_str!("schema.unid");
    let created = engine.execute(schema);
    assert!(created.ok, "{}", created.message);
    let inserted = engine.execute(
        r#"insert tasks {id: 1, title: "macro", state: Pending, priority: 4, due_on: @2026-09-17}"#,
    );
    assert!(inserted.ok, "{}", inserted.message);

    let rows = find_pending::find_pending(
        &mut engine,
        find_pending::FindPendingParams { min_priority: 3 },
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, 1);
    assert_eq!(rows[0].state, State::Pending);

    let changed = reprioritize_task::reprioritize_task(
        &mut engine,
        reprioritize_task::ReprioritizeTaskParams { id: 1, priority: 9 },
    )
    .unwrap();
    assert_eq!(changed.affected_rows, 1);
    assert_eq!(changed.rows.len(), 1);
    assert_eq!(changed.rows[0].state, State::Pending);
    assert_eq!(changed.rows[0].priority, 9);
    let due = due_before::due_before(&mut engine, due_before::DueBeforeParams).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, 1);
    assert!(find_pending::FIND_PENDING_DIGEST.starts_with("sha256:"));
    let described =
        unionid::query_contract::describe(schema, find_pending::FIND_PENDING_SOURCE).unwrap();
    assert_eq!(
        described.canonical_source,
        find_pending::FIND_PENDING_SOURCE
    );
    assert_eq!(described.query_digest, find_pending::FIND_PENDING_DIGEST);
}

#[test]
fn generated_query_rejects_runtime_schema_drift() {
    let mut engine = unionid::Engine::memory();
    assert!(engine.execute(include_str!("schema.unid")).ok);
    assert!(engine.execute("type Extra = text").ok);
    let error = find_pending::find_pending(
        &mut engine,
        find_pending::FindPendingParams { min_priority: 0 },
    )
    .unwrap_err();
    assert_eq!(error.code, "E_SCHEMA_CHANGED");
}
