use unionid::{Engine, Value};

const SCHEMA: &str = "struct User {id: int, email: Option<text>}\ntable users: User {key id}";

#[test]
fn ordinary_unique_none_is_one_value_and_text_is_exact() {
    let mut engine = Engine::memory();
    let response = engine.execute(&format!("{SCHEMA}\ncreate unique index users (email)"));
    assert!(response.ok, "{}", response.message);
    assert!(engine.execute("insert users {id: 1, email: None}").ok);
    let response = engine.execute("insert users {id: 2, email: None}");
    assert_eq!(response.error.unwrap().code, "E_CONSTRAINT");
    for (id, email) in [(3, "A@x.com"), (4, "a@x.com"), (5, " a@x.com")] {
        let response = engine.execute(&format!(
            "insert users {{id: {id}, email: Some({email:?})}}"
        ));
        assert!(response.ok, "{}", response.message);
    }
    assert_eq!(engine.execute("from users").rows.len(), 4);
}

#[test]
fn optional_email_example_rejects_duplicate_present_values_atomically() {
    let mut engine = Engine::memory();
    let response = engine.execute(include_str!("../examples/account_email.unid"));
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.rows.len(), 3);
    assert!(matches!(response.rows[0]["email"], Value::Option(None)));
    assert!(matches!(response.rows[1]["email"], Value::Option(None)));
    let before = engine.execute("from users | sort id").rows;
    let response = engine.execute(
        "insert many users [{id: 4, email: None}, {id: 5, email: Some(\"alice@example.com\")}] ",
    );
    assert_eq!(response.error.unwrap().code, "E_CONSTRAINT");
    let after = engine.execute("from users | sort id").rows;
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}
