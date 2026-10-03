use unionid::{
    formatter::format_source,
    query::{GeneratedDefault, Statement},
    syntax::{InputStatus, input_status, parse},
};

#[test]
fn generated_declarations_preserve_names_order_and_signed_sequence_limits() {
    let source = r#"sequence account_ids {start -9223372036854775808}
struct Account {
  id: int
  public_id: uuid
  created_at: timestamp
}
table accounts: Account {
  default created_at = now()
  key id
  default id = next(account_ids)
  default public_id = uuid_v7()
}"#;
    let statements = parse(source).unwrap();
    assert!(
        matches!(&statements[0].statement, Statement::CreateSequence {name, start}
        if name == "account_ids" && *start == i64::MIN)
    );
    let Statement::TypedTable { key, defaults, .. } = &statements[2].statement else {
        panic!("expected typed table");
    };
    assert_eq!(key.as_deref(), Some("id"));
    assert_eq!(
        defaults
            .iter()
            .map(|value| value.field.as_str())
            .collect::<Vec<_>>(),
        vec!["created_at", "id", "public_id"]
    );
    assert_eq!(defaults[0].generator, GeneratedDefault::Now);
    assert_eq!(
        defaults[1].generator,
        GeneratedDefault::Next("account_ids".into())
    );
    assert_eq!(defaults[2].generator, GeneratedDefault::UuidV7);
    let formatted = format_source(source).unwrap();
    assert!(!formatted.contains(';'));
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    let reparsed = parse(&formatted).unwrap();
    let Statement::TypedTable {
        defaults: after, ..
    } = &reparsed[2].statement
    else {
        panic!("expected typed table after formatting");
    };
    assert_eq!(defaults, after);
}

#[test]
fn braces_and_layout_have_one_canonical_generator_syntax() {
    for source in [
        "sequence ids {}\ntable items: Item {key id, default id = next(ids), default time = now()}",
        "sequence ids\n  start 1\ntable items: Item\n  key id\n  default id = next(ids)\n  default time = now()",
    ] {
        assert_eq!(
            format_source(source).unwrap(),
            "sequence ids {start 1}\n\ntable items: Item {\n  key id\n  default id = next(ids)\n  default time = now()\n}\n"
        );
    }
    let spaced = parse("sequence ids {start - 9223372036854775808}").unwrap();
    assert!(matches!(
        spaced[0].statement,
        Statement::CreateSequence {
            start: i64::MIN,
            ..
        }
    ));
    for start in [i64::MIN, -1, 0, 1, i64::MAX] {
        let source = format!("sequence ids {{start {start}}}");
        assert!(matches!(&parse(&source).unwrap()[0].statement,
            Statement::CreateSequence {start: actual, ..} if *actual == start));
    }
}

#[test]
fn generator_syntax_rejects_ambiguous_or_unsupported_forms() {
    for source in [
        "sequence ids {start 9223372036854775808}",
        "sequence ids {start -9223372036854775809}",
        "sequence ids {start 1.0}",
        "sequence ids {start $initial}",
        "sequence ids {start 1 start 2}",
        "sequence ids {cycle true}",
        "table items: Item {key id, key other}",
        "table items: Item {default id = next(ids), default id = next(ids)}",
        "table items: Item {default nested.id = next(ids)}",
        "table items: Item {default id = next($ids)}",
        "table items: Item {default id = next(ids, other)}",
        "table items: Item {default id = next(ids + 1)}",
        "table items: Item {default time = now(1)}",
        "table items: Item {default uid = uuid_v7(1)}",
        "table items: Item {default time = clock()}",
        "table items: Item {default time = now}",
        "table items: Item {default id = 1}",
        "table items: Item {key id default time = now()}",
    ] {
        assert!(parse(source).is_err(), "unexpectedly accepted {source}");
    }
    assert!(matches!(
        input_status("table items: Item {\n default id = next("),
        InputStatus::Incomplete(_)
    ));
    assert!(matches!(
        input_status("sequence ids {start 1}"),
        InputStatus::Complete
    ));
}

#[test]
fn generated_migrations_format_and_reparse_with_existing_constant_defaults() {
    for source in [
        "migration generated {\nadd sequence ids {start - 4}\nadd struct Item {id: int, created_at: timestamp}\nadd table items: Item {key id, default id = next(ids), default created_at = now()}\nrename sequence ids to item_ids\nchange default items.id to next(item_ids)\ndrop default items.created_at\ndrop default items.id\ndrop sequence item_ids\n}",
        "migration plain {\nadd sequence ids\nadd struct Item {id: int}\nadd table items Item key id\nchange default Item.id to 1\nchange default items.id to next(ids)\n}",
    ] {
        let formatted = format_source(source).unwrap();
        assert_eq!(format_source(&formatted).unwrap(), formatted);
        assert!(!formatted.contains(';'));
    }
    for source in [
        "migration bad {change default items.id to next(ids, extra)}",
        "migration bad {change default items.created_at to now(1)}",
        "migration bad {add sequence ids {start 1.5}}",
    ] {
        assert!(parse(source).is_err(), "{source}");
    }
}
