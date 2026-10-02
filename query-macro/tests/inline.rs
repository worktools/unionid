use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct CallerOwnedModel {
    label: String,
}

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
        expect affected == 1
    }

    query due_before {
        from tasks
        filter due_on < @2026-10-01
        select {id, due_on}
    }

    query running_for_worker {
        from tasks
        filter match state {
            Pending => false
            Running {worker} => worker == $worker
            Done {..} => false
        }
        select {id, state}
    }

    query normalize_text {
        from tasks
        filter id == $id
        derive label = concat (substring (lower (trim title)) 0 $end) "!"
        derive matched = starts_with label $prefix
        select {label, matched}
    }
}

#[test]
fn inline_text_functions_infer_typed_parameters_and_preserve_range_errors() {
    let mut engine = unionid::Engine::memory();
    assert!(engine.execute(include_str!("schema.unid")).ok);
    assert!(engine.execute("insert tasks {id: 1, title: \"　Aé🦀　\", state: Pending, priority: 1, due_on: @2026-09-17}").ok);
    let rows = normalize_text::normalize_text(
        &mut engine,
        normalize_text::NormalizeTextParams {
            id: 1,
            end: 2,
            prefix: "a".into(),
        },
    )
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "aé!");
    assert!(rows[0].matched);
    let error = normalize_text::normalize_text(
        &mut engine,
        normalize_text::NormalizeTextParams {
            id: 1,
            end: 4,
            prefix: "a".into(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "E_TEXT_RANGE");
}

#[test]
fn inline_queries_generate_shared_typed_bindings() {
    let caller_model = CallerOwnedModel {
        label: "caller import remains usable".into(),
    };
    assert_eq!(caller_model.label, "caller import remains usable");

    let mut engine = unionid::Engine::memory();
    let schema = include_str!("schema.unid");
    let created = engine.execute(schema);
    assert!(created.ok, "{}", created.message);
    let inserted = engine.execute(
        r#"insert tasks {id: 1, title: "macro", state: Pending, priority: 4, due_on: @2026-09-17}"#,
    );
    assert!(inserted.ok, "{}", inserted.message);
    let inserted = engine.execute(
        r#"insert tasks {id: 2, title: "worker", state: Running {worker: "alpha"}, priority: 2, due_on: @2026-10-17}"#,
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
    let missed = reprioritize_task::reprioritize_task(
        &mut engine,
        reprioritize_task::ReprioritizeTaskParams {
            id: 99,
            priority: 0,
        },
    )
    .unwrap_err();
    assert_eq!(missed.code, "E_EXPECTATION");
    assert_eq!(missed.statement_index, Some(2));
    let due = due_before::due_before(&mut engine, due_before::DueBeforeParams).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, 1);
    let running = running_for_worker::running_for_worker(
        &mut engine,
        running_for_worker::RunningForWorkerParams {
            worker: "alpha".into(),
        },
    )
    .unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].id, 2);
    assert_eq!(
        running[0].state,
        State::Running {
            worker: "alpha".into()
        }
    );
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

#[test]
fn inline_queries_execute_against_redb() {
    let directory = std::env::temp_dir().join(format!(
        "unionid-query-macro-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("inline.redb");
    {
        let mut engine = unionid::Engine::open_redb(&database).unwrap();
        assert!(engine.execute(include_str!("schema.unid")).ok);
        let inserted = engine.execute(
            r#"insert tasks {id: 7, title: "redb", state: Pending, priority: 5, due_on: @2026-09-18}"#,
        );
        assert!(inserted.ok, "{}", inserted.message);
        let rows = find_pending::find_pending(
            &mut engine,
            find_pending::FindPendingParams { min_priority: 5 },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 7);
        assert_eq!(rows[0].state, State::Pending);
    }
    {
        let mut reopened = unionid::Engine::open_redb(&database).unwrap();
        let rows = due_before::due_before(&mut reopened, due_before::DueBeforeParams).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 7);
    }
    std::fs::remove_dir_all(directory).unwrap();
}
