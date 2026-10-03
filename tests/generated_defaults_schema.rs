use unionid::portable::{CompatibilityLevel, GeneratorDescription};
use unionid::{Engine, Value};

const BASE: &str =
    "struct Item {id: int, owner: text, created_at: timestamp}\ntable items: Item {key id}";
const GENERATED: &str = "sequence ids {start 5}\nstruct Item {id: int, owner: text, created_at: timestamp}\ntable items: Item {key id, default id = next(ids), default created_at = now()}";

fn ok(engine: &mut Engine, source: &str) {
    let response = engine.execute(source);
    assert!(response.ok, "{source}: {:?}", response.error);
}

#[test]
fn schema_diff_orders_sequence_policy_changes_and_ignores_active_counters() {
    let mut engine = Engine::memory();
    ok(&mut engine, BASE);
    let diff = engine.diff_schema(GENERATED, "generated", None).unwrap();
    assert!(diff.runnable);
    assert_eq!(diff.operations.len(), 3);
    assert!(
        diff.operations[0]
            .description
            .starts_with("add sequence ids")
    );
    ok(&mut engine, &diff.migration_source);
    let row = engine.execute("insert items {owner: \"a\"} | returning id");
    assert!(row.ok);
    assert!(row.rows[0]["id"].cmp_eq(&Value::Int(5)));
    assert!(
        engine
            .diff_schema(GENERATED, "unchanged", None)
            .unwrap()
            .operations
            .is_empty()
    );
    let replacement = GENERATED
        .replace("ids", "replacement")
        .replace("start 5", "start 50");
    let diff = engine.diff_schema(&replacement, "switch", None).unwrap();
    assert!(diff.runnable);
    assert!(
        diff.operations
            .first()
            .unwrap()
            .description
            .starts_with("add sequence replacement")
    );
    assert_eq!(
        diff.operations.last().unwrap().description,
        "drop sequence ids"
    );
    ok(&mut engine, &diff.migration_source);
    let row = engine.execute("insert items {owner: \"b\"} | returning id");
    assert!(row.ok);
    assert!(row.rows[0]["id"].cmp_eq(&Value::Int(50)));
    let diff = engine.diff_schema(BASE, "remove", None).unwrap();
    assert!(diff.runnable);
    assert!(
        diff.operations
            .first()
            .unwrap()
            .description
            .starts_with("drop default")
    );
    assert_eq!(
        diff.operations.last().unwrap().description,
        "drop sequence replacement"
    );
    ok(&mut engine, &diff.migration_source);
}

#[test]
fn schema_diff_does_not_guess_sequence_renames_or_reset_start() {
    let mut engine = Engine::memory();
    ok(&mut engine, GENERATED);
    for target in [
        GENERATED.replace("ids", "renamed"),
        GENERATED.replace("start 5", "start 9"),
    ] {
        let diff = engine.diff_schema(&target, "review", None).unwrap();
        assert!(!diff.runnable);
        assert!(diff.operations.iter().any(
            |operation| operation.requires_input && operation.description.contains("sequence")
        ));
        assert!(
            !diff
                .operations
                .iter()
                .any(|operation| operation.description.starts_with("drop sequence"))
        );
    }
}

#[test]
fn new_table_diff_creates_table_before_its_generation_policies() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct Existing {id: int}\ntable existing: Existing {key id}",
    );
    let target = String::from(
        "sequence ids {start 5}\nstruct Existing {id: int}\nstruct Item {id: int, owner: text, created_at: timestamp}\ntable existing: Existing {key id}\ntable items: Item {key id, default id = next(ids), default created_at = now()}",
    );
    let diff = engine.diff_schema(&target, "new_table", None).unwrap();
    assert!(diff.runnable);
    ok(&mut engine, &diff.migration_source);
    assert!(engine.execute("insert items {owner: \"first\"}").ok);
}

#[test]
fn portable_generation_metadata_is_lossless_validated_and_not_an_adt_default() {
    let mut engine = Engine::memory();
    ok(&mut engine, GENERATED);
    let contract = engine.portable_contract().unwrap();
    let description = contract.description().clone();
    assert_eq!(description.version, 4);
    assert_eq!(description.sequences[0].start, "5");
    assert_eq!(description.tables[0].generated_defaults.len(), 2);
    let json = serde_json::to_value(&description).unwrap();
    assert!(json["sequences"][0].get("next").is_none());
    for version in [1, 2, 3] {
        let mut old = description.clone();
        old.version = version;
        assert_eq!(old.validate().unwrap_err().code, "E_CONTRACT_SCHEMA");
        old.sequences.clear();
        old.tables[0].generated_defaults.clear();
        old.validate().unwrap();
        assert!(contract.validate_description(&old).is_err());
    }
    for corrupt in 0..7 {
        let mut invalid = description.clone();
        match corrupt {
            0 => invalid.sequences[0].start = "05".into(),
            1 => invalid.sequences[0].id = invalid.tables[0].id.clone(),
            2 => invalid.sequences[0].name = "Item".into(),
            3 => invalid.tables[0].generated_defaults[0].field = "owner".into(),
            4 => {
                invalid.tables[0].generated_defaults[0].generator = GeneratorDescription::Next {
                    sequence_id: "9999".into(),
                }
            }
            5 => invalid.tables[0].generated_defaults[0].generator = GeneratorDescription::Now,
            _ => {
                let duplicate = invalid.tables[0].generated_defaults[0].clone();
                invalid.tables[0].generated_defaults.push(duplicate);
            }
        }
        assert_eq!(invalid.validate().unwrap_err().code, "E_CONTRACT_SCHEMA");
    }
    let mut forged = description.clone();
    forged.sequences[0].start = "6".into();
    forged.validate().unwrap();
    assert!(contract.validate_description(&forged).is_err());
    let mut omitted = description.clone();
    omitted.tables[0].generated_defaults.clear();
    omitted.validate().unwrap();
    assert!(contract.validate_description(&omitted).is_err());
    let before = serde_json::to_value(engine.portable_contract().unwrap().description()).unwrap();
    ok(&mut engine, "insert items {owner: \"a\"}");
    assert_eq!(
        serde_json::to_value(engine.portable_contract().unwrap().description()).unwrap(),
        before
    );
    ok(&mut engine, "migration remove {drop default items.id}");
    let next = engine.portable_contract().unwrap();
    let evolution = description
        .compare_same_catalog(next.description())
        .unwrap();
    assert_eq!(
        evolution.client_write.level,
        CompatibilityLevel::Incompatible
    );
    assert_eq!(
        evolution.existing_data.level,
        CompatibilityLevel::Compatible
    );
}

#[test]
fn nominal_generation_does_not_override_reusable_constant_omission_metadata() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "sequence ids {start 1}\ntype Id = int\nstruct Item {id: Id = 99, owner: text}\ntable items: Item {key id, default id = next(ids)}",
    );
    let old = engine.portable_contract().unwrap().into_description();
    old.validate().unwrap();
    ok(&mut engine, "migration remove {drop default items.id}");
    let next = engine.portable_contract().unwrap().into_description();
    let report = old.compare_same_catalog(&next).unwrap();
    assert_eq!(report.client_write.level, CompatibilityLevel::Conditional);
    let response = engine.execute("insert items {owner: \"fallback\"} | returning id");
    assert!(response.ok);
    assert!(response.rows[0]["id"].unwrapped().cmp_eq(&Value::Int(99)));
}

#[test]
fn query_inputs_describe_omission_separately_and_keep_upsert_keys_required() {
    let schema = "sequence ids {start 1}\nstruct Item {id: int, owner: text, created_at: timestamp}\ntable items: Item {key id, default id = next(ids), default created_at = now()}";
    for (query, fields) in [
        ("explain insert items $item", vec!["id", "created_at"]),
        ("explain upsert items $item", vec!["created_at"]),
        (
            "insert items $item | returning id",
            vec!["id", "created_at"],
        ),
        (
            "insert many items $item | returning id",
            vec!["id", "created_at"],
        ),
        ("upsert items $item | returning id", vec!["created_at"]),
        ("upsert many items $item | returning id", vec!["created_at"]),
    ] {
        let description = unionid::query_contract::describe(schema, query).unwrap();
        assert_eq!(description.version, 2);
        assert_eq!(description.parameters[0].omittable_fields, fields);
        let source = unionid::codegen::rust_query(schema, query, "write_item").unwrap();
        assert!(source.contains("pub created_at: Option<unionid::scalars::Timestamp>"));
        assert!(source.contains("serialize_present"));
        if query.starts_with("upsert") || query.starts_with("explain upsert") {
            assert!(source.contains("pub id: i64,"));
        } else {
            assert!(source.contains("pub id: Option<i64>,"));
        }
    }
    let mut old = unionid::query_contract::describe(schema, "insert items $item").unwrap();
    old.version = 1;
    assert!(
        unionid::codegen::rust_query_bundle_from_descriptions(schema, &[("old".into(), old)])
            .is_err()
    );
}
