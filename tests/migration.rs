use unionid::migration::{ApplyDecision, MigrationEntry, validate_history, validate_next};

fn entry(id: &str, parent: Option<&str>, revision: u64) -> MigrationEntry {
    MigrationEntry {
        id: id.into(),
        parent: parent.map(Into::into),
        checksum: format!("checksum-{id}"),
        schema_revision: revision,
        schema_hash: format!("sha256:{id}"),
    }
}

#[test]
fn linear_history_can_be_loaded_in_any_order() {
    let first = entry("v1", None, 1);
    let second = entry("v2", Some("v1"), 2);
    assert_eq!(
        validate_history(&[second.clone(), first.clone()]).unwrap(),
        Some("v2")
    );
    assert_eq!(
        validate_next(&[first], &second).unwrap(),
        ApplyDecision::Apply
    );
}

#[test]
fn migration_history_rejects_forks_and_cycles() {
    let fork = [
        entry("v1", None, 1),
        entry("left", Some("v1"), 2),
        entry("right", Some("v1"), 2),
    ];
    let error = validate_history(&fork).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("forks"));

    let cycle = [entry("v1", Some("v2"), 1), entry("v2", Some("v1"), 2)];
    let error = validate_history(&cycle).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("cycle"));
}

#[test]
fn applying_a_migration_is_idempotent_but_immutable() {
    let first = entry("v1", None, 1);
    assert_eq!(
        validate_next(std::slice::from_ref(&first), &first).unwrap(),
        ApplyDecision::AlreadyApplied
    );
    let mut changed = first.clone();
    changed.checksum = "edited".into();
    assert!(validate_next(&[first], &changed).is_err());
}

#[test]
fn next_migration_must_extend_the_current_head() {
    let history = [entry("v1", None, 1), entry("v2", Some("v1"), 2)];
    let error = validate_next(&history, &entry("v3", Some("v1"), 3)).unwrap_err();
    assert_eq!(error.code, "E_MIGRATION");
    assert!(error.message.contains("current head"));
}
