use serde::Deserialize;
use std::process::Command;
mod common;
use common::TempDir;
use unionid::portable::{CompatibilityLevel, DESCRIPTION_VERSION, TypeShape};
use unionid::{Engine, WireValue};

#[derive(Deserialize)]
struct VectorFile {
    version: u32,
    schema: String,
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    name: String,
    type_name: String,
    value: WireValue,
    expected_ok: bool,
    error_code: Option<String>,
}

#[test]
fn portable_description_is_versioned_lossless_and_database_local() {
    let vectors: VectorFile =
        serde_json::from_str(include_str!("fixtures/portable/v1.json")).unwrap();
    let description = unionid::portable::describe(&vectors.schema).unwrap();
    description.validate().unwrap();
    assert_eq!(description.version, DESCRIPTION_VERSION);
    assert_eq!(description.id_scope.kind, "database_local_catalog");
    assert!(!description.id_scope.globally_stable);
    assert!(
        description
            .types
            .iter()
            .all(|ty| ty.id.parse::<u64>().is_ok())
    );

    let nested = description
        .types
        .iter()
        .find(|ty| ty.name == "Nested")
        .unwrap();
    assert!(matches!(
        nested.shape,
        TypeShape::Option { ref item }
            if matches!(item.as_ref(), TypeShape::Option { .. })
    ));
    let money = description
        .types
        .iter()
        .find(|ty| ty.name == "Money")
        .unwrap();
    assert!(matches!(
        money.shape,
        TypeShape::Decimal {
            precision: 38,
            scale: 9,
            ..
        }
    ));
    let envelope = description
        .types
        .iter()
        .find(|ty| ty.name == "Envelope")
        .unwrap();
    let TypeShape::Record { fields } = &envelope.shape else {
        panic!("Envelope must be a record")
    };
    let tags = fields.iter().find(|field| field.name == "tags").unwrap();
    assert!(tags.input_omittable);
    assert!(matches!(tags.default, Some(WireValue::List { ref items }) if items.is_empty()));

    let encoded = serde_json::to_string(&description).unwrap();
    let decoded = serde_json::from_str(&encoded).unwrap();
    assert_eq!(description, decoded);

    let mut unsupported = description.clone();
    unsupported.version += 1;
    assert_eq!(
        unsupported.validate().unwrap_err().code,
        "E_CONTRACT_VERSION"
    );
    let mut global = description.clone();
    global.id_scope.globally_stable = true;
    assert_eq!(global.validate().unwrap_err().code, "E_CONTRACT_SCOPE");
    let mut duplicate = description.clone();
    duplicate.types[1].id = duplicate.types[0].id.clone();
    assert_eq!(duplicate.validate().unwrap_err().code, "E_CONTRACT_SCHEMA");

    let keyed =
        unionid::portable::describe("type Item = {id int, title text}\ntable items Item\n  key id")
            .unwrap();
    let mut mismatched_key_name = keyed.clone();
    mismatched_key_name.tables[0]
        .primary_key
        .as_mut()
        .unwrap()
        .field = "title".into();
    assert_eq!(
        mismatched_key_name.validate().unwrap_err().code,
        "E_CONTRACT_SCHEMA"
    );

    let mut mismatched_key_path = keyed;
    let item = mismatched_key_path
        .types
        .iter()
        .find(|ty| ty.name == "Item")
        .unwrap();
    let TypeShape::Record { fields } = &item.shape else {
        panic!("Item must be a record")
    };
    let title_id = fields[1].id.clone();
    mismatched_key_path.tables[0]
        .primary_key
        .as_mut()
        .unwrap()
        .field_path[0] = title_id;
    assert_eq!(
        mismatched_key_path.validate().unwrap_err().code,
        "E_CONTRACT_SCHEMA"
    );
}

#[test]
fn rust_reference_implementation_runs_common_portable_vectors() {
    let vectors: VectorFile =
        serde_json::from_str(include_str!("fixtures/portable/v1.json")).unwrap();
    assert_eq!(vectors.version, DESCRIPTION_VERSION);
    let contract = unionid::PortableContract::from_source(&vectors.schema).unwrap();
    for vector in vectors.vectors {
        let result = contract.validate_type(&vector.type_name, vector.value);
        assert_eq!(
            result.is_ok(),
            vector.expected_ok,
            "vector '{}' returned {result:?}",
            vector.name
        );
        if let Some(code) = vector.error_code {
            assert_eq!(result.unwrap_err().code, code, "vector '{}'", vector.name);
        } else if vector.name == "nested_some_none" {
            let WireValue::Named { value, .. } = result.unwrap() else {
                panic!("named type identity must be restored")
            };
            let WireValue::Option { value: Some(inner) } = *value else {
                panic!("outer option must remain Some")
            };
            assert!(matches!(*inner, WireValue::Option { value: None }));
        } else if vector.name == "defaulted_empty_collection" {
            let WireValue::Named { value, .. } = result.unwrap() else {
                panic!("named type identity must be restored")
            };
            let WireValue::Record { fields } = *value else {
                panic!("Envelope must normalize to a record")
            };
            assert!(matches!(
                fields.get("tags"),
                Some(WireValue::List { items }) if items.is_empty()
            ));
        }
    }
}

#[test]
fn engine_description_preserves_live_catalog_identity() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute("type UserId = text\ntype User = {id UserId}\ntable users User\n  key id")
            .ok
    );
    let contract = engine.portable_contract().unwrap();
    assert_eq!(
        contract.description().schema.revision,
        engine.schema_info().revision.to_string()
    );
    assert_eq!(
        contract.description().schema.hash,
        engine.schema_info().hash
    );
    let user_id = contract
        .description()
        .types
        .iter()
        .find(|ty| ty.name == "UserId")
        .unwrap();
    let user = contract
        .description()
        .types
        .iter()
        .find(|ty| ty.name == "User")
        .unwrap();
    let TypeShape::Record { fields } = &user.shape else {
        panic!("User must be a record")
    };
    assert!(matches!(
        fields[0].shape,
        TypeShape::Ref {
            ref type_id,
            ref name
        } if type_id == &user_id.id && name == "UserId"
    ));
}

#[test]
fn cli_describes_schema_files_and_live_databases() {
    let dir = TempDir::new();
    let schema = dir.0.join("schema.uid");
    let source = "type Item = {id int, tags list text = []}\ntable items Item\n  key id";
    std::fs::write(&schema, source).unwrap();
    let file_output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["schema", "describe", "--file", schema.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(file_output.status.success());
    let from_file: unionid::SchemaDescription =
        serde_json::from_slice(&file_output.stdout).unwrap();

    let db = dir.0.join("app.redb");
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        assert!(engine.execute(source).ok);
    }
    let destination = dir.0.join("contract.json");
    let db_output = Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args([
            "schema",
            "describe",
            "--db",
            db.to_str().unwrap(),
            "--output",
            destination.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(db_output.status.success());
    let from_db: unionid::SchemaDescription =
        serde_json::from_slice(&std::fs::read(destination).unwrap()).unwrap();
    assert_eq!(from_file.types, from_db.types);
    assert_eq!(from_file.tables, from_db.tables);
}

#[test]
fn evolution_reports_four_directions_for_additive_changes() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "type State = Pending | Running\ntype Task = {id int, state State}\ntable tasks Task\n  key id",
            )
            .ok
    );
    let baseline = engine.portable_contract().unwrap().into_description();
    assert!(
        engine
            .execute(
                "migration additive\n  add variant State.Complete\n  add field Task.priority int = 0",
            )
            .ok
    );
    let candidate = engine.portable_contract().unwrap().into_description();
    let report = baseline.compare_same_catalog(&candidate).unwrap();
    assert_eq!(report.existing_data.level, CompatibilityLevel::Compatible);
    assert_eq!(report.query.level, CompatibilityLevel::Conditional);
    assert_eq!(report.client_read.level, CompatibilityLevel::Conditional);
    assert_eq!(report.client_write.level, CompatibilityLevel::Compatible);
    assert!(
        report
            .query
            .findings
            .iter()
            .any(|finding| finding.code == "variant_added")
    );
    assert!(
        report
            .client_read
            .findings
            .iter()
            .any(|finding| finding.code == "field_added")
    );
}

#[test]
fn evolution_uses_stable_ids_and_reports_renames_separately() {
    let mut engine = Engine::memory();
    assert!(
        engine
            .execute(
                "type State = Pending | Failed {message text}\ntype Task = {id int, state State}\ntable tasks Task\n  key id",
            )
            .ok
    );
    let baseline = engine.portable_contract().unwrap().into_description();
    assert!(
        engine
            .execute(
                "migration rename\n  rename variant State.Failed to Rejected\n  rename field Task.id to task_id",
            )
            .ok
    );
    let candidate = engine.portable_contract().unwrap().into_description();
    let report = baseline.compare_same_catalog(&candidate).unwrap();
    assert_eq!(report.existing_data.level, CompatibilityLevel::Compatible);
    assert_eq!(report.query.level, CompatibilityLevel::Incompatible);
    assert_eq!(report.client_read.level, CompatibilityLevel::Incompatible);
    assert_eq!(report.client_write.level, CompatibilityLevel::Incompatible);
    assert!(
        report
            .query
            .findings
            .iter()
            .any(|finding| finding.code == "variant_renamed")
    );
    assert!(
        report
            .query
            .findings
            .iter()
            .any(|finding| finding.code == "field_renamed")
    );
}

#[test]
fn evolution_reports_retained_field_reordering() {
    let baseline = unionid::portable::describe(
        "type Item = {id int, title text, active bool}\ntable items Item\n  key id",
    )
    .unwrap();
    let mut candidate = baseline.clone();
    let item = candidate
        .types
        .iter_mut()
        .find(|ty| ty.name == "Item")
        .unwrap();
    let TypeShape::Record { fields } = &mut item.shape else {
        panic!("Item must be a record")
    };
    fields.swap(0, 1);
    candidate.schema.hash = format!("sha256:{}", "1".repeat(64));
    candidate.validate().unwrap();
    let report = baseline.compare_same_catalog(&candidate).unwrap();
    assert_eq!(report.existing_data.level, CompatibilityLevel::Compatible);
    assert_eq!(report.query.level, CompatibilityLevel::Conditional);
    assert_eq!(report.client_read.level, CompatibilityLevel::Conditional);
    assert_eq!(report.client_write.level, CompatibilityLevel::Compatible);
    assert!(
        report
            .query
            .findings
            .iter()
            .any(|finding| finding.code == "field_order_changed")
    );
}

#[test]
fn evolution_reports_default_changes_for_old_writers() {
    let baseline = unionid::portable::describe(
        "type Item = {id int, priority int = 0}\ntable items Item\n  key id",
    )
    .unwrap();
    let mut candidate = baseline.clone();
    let item = candidate
        .types
        .iter_mut()
        .find(|ty| ty.name == "Item")
        .unwrap();
    let TypeShape::Record { fields } = &mut item.shape else {
        panic!("Item must be a record")
    };
    let priority = fields
        .iter_mut()
        .find(|field| field.name == "priority")
        .unwrap();
    priority.default = Some(WireValue::Int { value: "1".into() });
    candidate.schema.hash = format!("sha256:{}", "1".repeat(64));
    candidate.validate().unwrap();
    let report = baseline.compare_same_catalog(&candidate).unwrap();
    assert_eq!(report.existing_data.level, CompatibilityLevel::Compatible);
    assert_eq!(report.client_write.level, CompatibilityLevel::Conditional);
    assert!(
        report
            .client_write
            .findings
            .iter()
            .any(|finding| finding.code == "field_default_changed")
    );

    let item = candidate
        .types
        .iter_mut()
        .find(|ty| ty.name == "Item")
        .unwrap();
    let TypeShape::Record { fields } = &mut item.shape else {
        panic!("Item must be a record")
    };
    let priority = fields
        .iter_mut()
        .find(|field| field.name == "priority")
        .unwrap();
    priority.default = None;
    priority.input_omittable = false;
    candidate.validate().unwrap();
    let report = baseline.compare_same_catalog(&candidate).unwrap();
    assert_eq!(report.client_write.level, CompatibilityLevel::Incompatible);
    assert!(
        report
            .client_write
            .findings
            .iter()
            .any(|finding| finding.code == "field_default_removed")
    );
}
