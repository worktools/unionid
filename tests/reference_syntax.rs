use unionid::{
    format_source,
    query::{SchemaMigration, Statement},
    syntax,
};

#[test]
fn reference_paths_preserve_pairing_through_multiline_formatting() {
    let source = "create reference reservations (\n  tenant\n  product.sku\n) references inventory (tenant, sku)\ndrop reference reservations (tenant, product.sku) references inventory (tenant, sku)";
    let formatted = format_source(source).unwrap();
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert!(!formatted.contains(';'));
    let parsed = syntax::parse(&formatted).unwrap();
    let Statement::CreateReference(created) = &parsed[0].statement else {
        panic!("expected reference creation");
    };
    let Statement::DropReference(dropped) = &parsed[1].statement else {
        panic!("expected reference removal");
    };
    assert_eq!(created, dropped);
    assert_eq!(created.table, "reservations");
    assert_eq!(created.fields, ["tenant", "product.sku"]);
    assert_eq!(created.target_table, "inventory");
    assert_eq!(created.target_fields, ["tenant", "sku"]);
    assert!(
        parsed
            .iter()
            .all(|located| located.statement.changes_schema())
    );
}

#[test]
fn migration_reference_shapes_share_the_standalone_grammar() {
    let source = "migration relationships {\nadd reference lines (order.id) references orders (id)\ndrop reference lines (order.id) references orders (id)\n}";
    let canonical = format_source(source).unwrap();
    assert_eq!(format_source(&canonical).unwrap(), canonical);
    let parsed = syntax::parse(&canonical).unwrap();
    let Statement::Migration { steps, .. } = &parsed[0].statement else {
        panic!("expected migration");
    };
    let SchemaMigration::AddReference(added) = &steps[0] else {
        panic!("expected added reference");
    };
    let SchemaMigration::DropReference(dropped) = &steps[1] else {
        panic!("expected dropped reference");
    };
    assert_eq!(added, dropped);
    assert_eq!(
        unionid::migration::describe_step(&steps[0]),
        (
            "add reference lines (order.id) references orders (id)".into(),
            false,
        )
    );
    assert!(unionid::migration::describe_step(&steps[1]).1);
}

#[test]
fn references_reject_ambiguous_or_non_path_shapes() {
    for source in [
        "create reference lines () references orders (id)",
        "create reference lines (id) references orders ()",
        "create reference lines (a, b) references orders (id)",
        "create reference lines (a, a) references orders (id, other)",
        "create reference lines (a, b) references orders (id, id)",
        "create reference lines (-id) references orders (id)",
        "create reference lines (id) references orders (-id)",
        "create reference lines (id + 1) references orders (id)",
        "create reference lines (id) references orders (id) if active",
        "create reference lines (id) references orders (id);",
    ] {
        assert!(syntax::parse(source).is_err(), "accepted {source}");
    }
    let fields = (0..17)
        .map(|n| format!("f{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        syntax::parse(&format!(
            "create reference lines ({fields}) references orders ({fields})"
        ))
        .is_err()
    );
}
