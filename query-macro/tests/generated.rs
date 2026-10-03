unionid_query::queries! {
    schema "tests/generated.unid"
    query add_item {
        insert items $item
        returning {id, owner, state, detail, public_id, created_at}
    }
    query add_batch {
        insert many items $items
        returning {id, public_id, created_at}
    }
    query replace_item {
        upsert items $item
        returning {id, owner, public_id}
    }
}

fn exercise(engine: &mut unionid::Engine) {
    let response = engine.execute(include_str!("generated.unid"));
    assert!(response.ok, "{}", response.message);
    let first = add_item::add_item(
        engine,
        add_item::AddItemParams {
            item: add_item::AddItemParamsItem {
                id: None,
                owner: "first".into(),
                state: State::Pending,
                detail: Detail { note: None },
                public_id: None,
                created_at: None,
            },
        },
    )
    .unwrap();
    assert_eq!(first.rows.id, Id(1));
    assert_eq!(first.rows.state, State::Pending);
    assert_eq!(first.rows.detail.note, None);
    let explicit_uuid = "018f29bd-93a4-7000-8000-000000000001".parse().unwrap();
    let explicit_time = "2020-01-01T00:00:00Z".parse().unwrap();
    let explicit = add_item::add_item(
        engine,
        add_item::AddItemParams {
            item: add_item::AddItemParamsItem {
                id: Some(Id(90)),
                owner: "explicit".into(),
                state: State::Done,
                detail: Detail {
                    note: Some("kept".into()),
                },
                public_id: Some(explicit_uuid),
                created_at: Some(explicit_time),
            },
        },
    )
    .unwrap();
    assert_eq!(explicit.rows.id, Id(90));
    assert_eq!(explicit.rows.public_id, explicit_uuid);
    assert_eq!(explicit.rows.created_at, explicit_time);
    assert_eq!(explicit.rows.detail.note.as_deref(), Some("kept"));
    let batch = add_batch::add_batch(
        engine,
        add_batch::AddBatchParams {
            items: ["second", "third"]
                .into_iter()
                .map(|owner| add_batch::AddBatchParamsItems {
                    id: None,
                    owner: owner.into(),
                    state: State::Pending,
                    detail: Detail { note: None },
                    public_id: None,
                    created_at: None,
                })
                .collect(),
        },
    )
    .unwrap();
    assert_eq!(batch.rows[0].id, Id(2));
    assert_eq!(batch.rows[1].id, Id(3));
    assert_eq!(batch.rows[0].created_at, batch.rows[1].created_at);
    assert_ne!(batch.rows[0].public_id, batch.rows[1].public_id);
    let replaced = replace_item::replace_item(
        engine,
        replace_item::ReplaceItemParams {
            item: replace_item::ReplaceItemParamsItem {
                id: Id(1),
                owner: "replaced".into(),
                state: State::Done,
                detail: Detail { note: None },
                public_id: None,
                created_at: None,
            },
        },
    )
    .unwrap();
    assert_eq!(replaced.rows.id, Id(1));
    assert_eq!(replaced.rows.owner, "replaced");
}

#[test]
fn generated_inputs_preserve_nominal_ids_options_and_complete_outputs() {
    exercise(&mut unionid::Engine::memory());
}

#[test]
fn generated_macro_inputs_survive_native_redb_reopen() {
    let directory = std::env::temp_dir().join(format!(
        "unionid-generated-macro-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("data.redb");
    {
        let mut engine = unionid::Engine::open_redb(&path).unwrap();
        engine.upgrade_storage(14).unwrap();
        engine
            .install_storage_capabilities(&["generated_defaults".into()], None)
            .unwrap();
        exercise(&mut engine);
    }
    {
        let mut engine = unionid::Engine::open_redb(&path).unwrap();
        engine.check_integrity().unwrap();
        let response = engine.execute("insert items {owner: \"after restart\", state: Pending, detail: {note: None}} | returning id");
        assert!(response.ok, "{}", response.message);
        assert!(
            response.rows[0]["id"]
                .unwrapped()
                .cmp_eq(&unionid::Value::Int(4))
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
