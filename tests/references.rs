use std::collections::BTreeMap;
use unionid::{Engine, QueryResponse, Value, error::ConstraintKind};

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{source}: {}", response.message);
    response
}

fn rejected(engine: &mut Engine, source: &str, kind: ConstraintKind) {
    let response = engine.execute(source);
    assert!(!response.ok, "accepted {source}");
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_CONSTRAINT");
    assert_eq!(error.constraint, Some(kind));
    assert!(error.hint.is_some());
}

fn rows(engine: &mut Engine, table: &str) -> serde_json::Value {
    serde_json::to_value(ok(engine, &format!("from {table} | sort id")).rows).unwrap()
}

const ORDERS: &str = "struct Order {id: int, code: text}\nstruct Line {id: int, order_id: int}\ntable orders: Order {key id}\ntable lines: Line {key id}\ncreate reference lines (order_id) references orders (id)\ninsert orders {id: 1, code: \"first\"}";

#[test]
fn orphan_batches_and_restricted_targets_leave_rows_and_schema_unchanged() {
    let mut engine = Engine::memory();
    ok(&mut engine, ORDERS);
    let schema = engine.schema_info();
    let before = rows(&mut engine, "lines");
    rejected(
        &mut engine,
        "insert many lines [{id: 1, order_id: 1}, {id: 2, order_id: 999}]",
        ConstraintKind::ReferenceMissing,
    );
    assert_eq!(rows(&mut engine, "lines"), before);
    assert_eq!(engine.schema_info(), schema);
    ok(&mut engine, "insert lines {id: 1, order_id: 1}");
    rejected(
        &mut engine,
        "delete orders | filter id == 1",
        ConstraintKind::ReferenceRestricted,
    );
    rejected(
        &mut engine,
        "update orders | filter id == 1 | set id = 2",
        ConstraintKind::ReferenceRestricted,
    );
    ok(&mut engine, "delete lines\ndelete orders");
    assert_eq!(rows(&mut engine, "orders"), serde_json::json!([]));
}

#[test]
fn optional_assignees_and_prepared_writes_share_constraints() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct User {id: int}\nstruct Task {id: int, assignee: Option<int>}\ntable users: User {key id}\ntable tasks: Task {key id}\ncreate reference tasks (assignee) references users (id)\ninsert tasks {id: 1, assignee: None}\ninsert users {id: 7}",
    );
    let prepared = engine
        .prepare(
            "update tasks | filter id == 1 | set assignee = $assignee | returning {id, assignee}",
        )
        .unwrap();
    let failed = engine.execute_prepared(
        &prepared,
        BTreeMap::from([(
            "assignee".into(),
            Value::Option(Some(Box::new(Value::Int(99)))),
        )]),
    );
    assert_eq!(
        failed.error.unwrap().constraint,
        Some(ConstraintKind::ReferenceMissing)
    );
    let updated = engine.execute_prepared(
        &prepared,
        BTreeMap::from([(
            "assignee".into(),
            Value::Option(Some(Box::new(Value::Int(7)))),
        )]),
    );
    assert!(updated.ok, "{}", updated.message);
    rejected(
        &mut engine,
        "delete users",
        ConstraintKind::ReferenceRestricted,
    );
    ok(
        &mut engine,
        "update tasks | set assignee = None\ndelete users",
    );
}

#[test]
fn batch_self_references_use_final_state_and_script_failure_rolls_back() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct Node {id: int, parent: Option<int>}\ntable nodes: Node {key id}\ncreate reference nodes (parent) references nodes (id)",
    );
    ok(
        &mut engine,
        "insert many nodes [{id: 2, parent: Some(1)}, {id: 1, parent: Some(2)}]",
    );
    let before = rows(&mut engine, "nodes");
    rejected(
        &mut engine,
        "delete nodes | filter id == 1",
        ConstraintKind::ReferenceRestricted,
    );
    assert_eq!(rows(&mut engine, "nodes"), before);
    rejected(
        &mut engine,
        "insert nodes {id: 3, parent: None}\ninsert nodes {id: 4, parent: Some(999)}",
        ConstraintKind::ReferenceMissing,
    );
    assert_eq!(rows(&mut engine, "nodes"), before);
    ok(&mut engine, "delete nodes");
    assert_eq!(rows(&mut engine, "nodes"), serde_json::json!([]));
}

#[test]
fn composite_adt_unique_target_and_exact_none_are_real_values() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "enum Sku { Local(int), Remote(text) }\nstruct Product {tenant: int, sku: Option<Sku>}\nstruct Line {id: int, tenant: int, sku: Option<Sku>}\ntable products: Product {}\ntable lines: Line {key id}\ncreate unique index products (-tenant, sku)\ncreate reference lines (tenant, sku) references products (tenant, sku)",
    );
    rejected(
        &mut engine,
        "insert lines {id: 1, tenant: 3, sku: None}",
        ConstraintKind::ReferenceMissing,
    );
    ok(
        &mut engine,
        "insert many products [{tenant: 3, sku: None}, {tenant: 3, sku: Some(Local(2))}]",
    );
    ok(
        &mut engine,
        "insert many lines [{id: 1, tenant: 3, sku: None}, {id: 2, tenant: 3, sku: Some(Local(2))}]",
    );
    rejected(
        &mut engine,
        "upsert lines {id: 2, tenant: 4, sku: Some(Local(2))}",
        ConstraintKind::ReferenceMissing,
    );
    rejected(
        &mut engine,
        "delete products | filter sku == None",
        ConstraintKind::ReferenceRestricted,
    );
    ok(
        &mut engine,
        "drop reference lines (tenant, sku) references products (tenant, sku)\ndelete products",
    );
}

#[test]
fn adding_reference_checks_existing_rows_without_publishing_a_failed_schema() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct Parent {id: int}\nstruct Child {id: int, parent: int}\ntable parents: Parent {key id}\ntable children: Child {key id}\ninsert children {id: 1, parent: 99}",
    );
    let before = engine.schema_info();
    rejected(
        &mut engine,
        "create reference children (parent) references parents (id)",
        ConstraintKind::ReferenceMissing,
    );
    assert_eq!(engine.schema_info(), before);
    ok(
        &mut engine,
        "insert parents {id: 99}\ncreate reference children (parent) references parents (id)",
    );
    let source = engine.schema();
    assert!(source.contains("create reference children (parent) references parents (id)"));
    let mut reproduced = Engine::memory();
    ok(&mut reproduced, &source);
    assert_eq!(engine.schema_info().hash, reproduced.schema_info().hash);
    rejected(
        &mut reproduced,
        "insert children {id: 1, parent: 99}",
        ConstraintKind::ReferenceMissing,
    );
}

#[test]
fn replacing_a_unique_target_is_restricted_but_non_key_updates_are_allowed() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct User {id: int, email: text, label: text}\nstruct Session {id: int, email: text}\ntable users: User {key id}\ntable sessions: Session {key id}\ncreate unique index users (email)\ncreate reference sessions (email) references users (email)\ninsert users {id: 1, email: \"a\", label: \"old\"}\ninsert sessions {id: 2, email: \"a\"}",
    );
    rejected(
        &mut engine,
        "upsert users {id: 1, email: \"b\", label: \"new\"}",
        ConstraintKind::ReferenceRestricted,
    );
    ok(
        &mut engine,
        "upsert users {id: 1, email: \"a\", label: \"new\"}",
    );
    let before = rows(&mut engine, "users");
    rejected(
        &mut engine,
        "upsert many users [{id: 3, email: \"c\", label: \"new\"}, {id: 1, email: \"b\", label: \"new\"}]",
        ConstraintKind::ReferenceRestricted,
    );
    assert_eq!(rows(&mut engine, "users"), before);
}

#[test]
fn migration_renames_preserve_reference_dependencies() {
    let mut engine = Engine::memory();
    ok(&mut engine, ORDERS);
    ok(&mut engine, "insert lines {id: 1, order_id: 1}");
    ok(
        &mut engine,
        "migration rename_links {\nrename table orders to purchases\nrename field Order.id to order_key\nrename field Line.order_id to purchase_key\n}",
    );
    assert!(
        engine
            .schema()
            .contains("create reference lines (purchase_key) references purchases (order_key)")
    );
    rejected(
        &mut engine,
        "insert lines {id: 2, purchase_key: 99}",
        ConstraintKind::ReferenceMissing,
    );
    rejected(
        &mut engine,
        "delete purchases",
        ConstraintKind::ReferenceRestricted,
    );
    ok(
        &mut engine,
        "migration remove_link {\ndrop reference lines (purchase_key) references purchases (order_key)\n}",
    );
    ok(&mut engine, "delete purchases");
}

#[test]
fn failed_migration_reference_addition_rolls_back_all_schema_changes() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct Parent {id: int}\nstruct Child {id: int, parent: int}\ntable parents: Parent {key id}\ntable children: Child {key id}\ninsert children {id: 1, parent: 99}",
    );
    let schema = engine.schema_info();
    let source = engine.schema();
    rejected(
        &mut engine,
        "migration invalid_reference {\nrename table parents to renamed\nadd reference children (parent) references renamed (id)\n}",
        ConstraintKind::ReferenceMissing,
    );
    assert_eq!(engine.schema_info(), schema);
    assert_eq!(engine.schema(), source);
    ok(
        &mut engine,
        "insert parents {id: 99}\nmigration valid_reference {\nadd reference children (parent) references parents (id)\n}",
    );
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
}

#[test]
fn migration_cannot_remove_bound_keys_or_fields_even_with_equivalent_unique() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct Parent {id: int, email: text}\nstruct Child {id: int, parent: text}\ntable parents: Parent {key id}\ntable children: Child {key id}\ncreate unique index parents (email)\ncreate unique index parents (-email)\ncreate reference children (parent) references parents (email)",
    );
    let before = engine.schema_info();
    for statement in [
        "drop index parents (email)",
        "drop table parents",
        "drop table children",
        "drop field Child.parent",
        "change field Child.parent to int using old -> 1",
    ] {
        let response = engine.execute(&format!("migration invalid {{\n{statement}\n}}"));
        assert!(!response.ok, "accepted {statement}");
        assert_eq!(response.error.unwrap().code, "E_MIGRATION", "{statement}");
        assert_eq!(engine.schema_info(), before);
    }
    ok(
        &mut engine,
        "migration remove_unbound_index {\ndrop index parents (-email)\n}",
    );
    ok(
        &mut engine,
        "migration remove_binding {\ndrop reference children (parent) references parents (email)\ndrop index parents (email)\n}",
    );
}

#[test]
fn memory_migration_runner_keeps_reference_failures_out_of_the_ledger() {
    use unionid::migration::MigrationFile;
    let initial = MigrationFile::parse("migration initial {\nadd struct Parent {id: int}\nadd struct Child {id: int, parent: int}\nadd table parents: Parent {key id}\nadd table children: Child {key id}\n}").unwrap();
    let link = MigrationFile::parse("migration link {\nparent initial\nadd reference children (parent) references parents (id)\n}").unwrap();
    let mut engine = Engine::memory();
    engine
        .apply_migrations(std::slice::from_ref(&initial))
        .unwrap();
    ok(&mut engine, "insert children {id: 1, parent: 7}");
    let before = engine.schema_info();
    let files = [initial, link];
    assert_eq!(
        engine.apply_migrations(&files).unwrap_err().code,
        "E_CONSTRAINT"
    );
    assert_eq!(engine.schema_info(), before);
    assert_eq!(engine.introspection().migration_count, 1);
    ok(&mut engine, "insert parents {id: 7}");
    engine.apply_migrations(&files).unwrap();
    assert_eq!(engine.introspection().migration_count, 2);
    rejected(
        &mut engine,
        "delete parents",
        ConstraintKind::ReferenceRestricted,
    );
}
