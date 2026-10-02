unionid_query::queries! {
    schema "tests/references.unid"

    query add_session {
        insert sessions $session
        returning {id, account}
    }

    query remove_account {
        delete accounts
        filter id == $id
        returning {id}
    }

    query list_sessions {
        from sessions
        sort id
        select {id, account}
    }
}

fn exercise(engine: &mut unionid::Engine) {
    let created = engine.execute(include_str!("references.unid"));
    assert!(created.ok, "{}", created.message);
    let error = add_session::add_session(
        engine,
        add_session::AddSessionParams {
            session: Session {
                id: 1,
                account: Some(AccountId::Local(7)),
            },
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert!(
        list_sessions::list_sessions(engine, list_sessions::ListSessionsParams)
            .unwrap()
            .is_empty()
    );
    assert!(engine.execute("insert accounts {id: Local(7)}").ok);
    let inserted = add_session::add_session(
        engine,
        add_session::AddSessionParams {
            session: Session {
                id: 1,
                account: Some(AccountId::Local(7)),
            },
        },
    )
    .unwrap();
    assert_eq!(inserted.affected_rows, 1);
    assert_eq!(inserted.rows.account, Some(AccountId::Local(7)));
    add_session::add_session(
        engine,
        add_session::AddSessionParams {
            session: Session {
                id: 2,
                account: None,
            },
        },
    )
    .unwrap();
    let error = remove_account::remove_account(
        engine,
        remove_account::RemoveAccountParams {
            id: AccountId::Local(7),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert_eq!(
        list_sessions::list_sessions(engine, list_sessions::ListSessionsParams)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn inline_typed_adt_references_enforce_missing_and_restrict_in_memory() {
    let mut engine = unionid::Engine::memory();
    exercise(&mut engine);
    assert!(
        engine
            .execute("drop reference sessions (account) references accounts (id)")
            .ok
    );
    let error =
        list_sessions::list_sessions(&mut engine, list_sessions::ListSessionsParams).unwrap_err();
    assert_eq!(error.code, "E_SCHEMA_CHANGED");
}

#[test]
fn inline_typed_adt_references_survive_redb_reopen() {
    let directory = std::env::temp_dir().join(format!(
        "unionid-reference-macro-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("references.redb");
    {
        let mut engine = unionid::Engine::open_redb(&database).unwrap();
        engine.upgrade_storage(12).unwrap();
        exercise(&mut engine);
    }
    {
        let mut engine = unionid::Engine::open_redb(&database).unwrap();
        let rows =
            list_sessions::list_sessions(&mut engine, list_sessions::ListSessionsParams).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].account, Some(AccountId::Local(7)));
        assert_eq!(rows[1].account, None);
        let error = remove_account::remove_account(
            &mut engine,
            remove_account::RemoveAccountParams {
                id: AccountId::Local(7),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "E_CONSTRAINT");
    }
    std::fs::remove_dir_all(directory).unwrap();
}
