use unionid::{Engine, format_source};

#[test]
fn formatter_is_idempotent_for_current_examples() {
    for (name, source) in [
        ("tasks", include_str!("../examples/tasks.uid")),
        ("config", include_str!("../examples/config.uid")),
        ("events", include_str!("../examples/events.uid")),
        ("job_queue", include_str!("../examples/job_queue.uid")),
        ("sync", include_str!("../examples/sync_conflicts.uid")),
        ("mutations", include_str!("../examples/task_mutations.uid")),
        ("schema", include_str!("../examples/schema.uid")),
        (
            "schema_migration",
            include_str!("../examples/schema_migration.uid"),
        ),
        (
            "migration_initial",
            include_str!("../examples/migrations/0001_initial.uid"),
        ),
        (
            "migration_priority",
            include_str!("../examples/migrations/0002_add_priority.uid"),
        ),
    ] {
        let formatted = format_source(source).unwrap_or_else(|error| panic!("{name}: {error}"));
        let second =
            format_source(&formatted).unwrap_or_else(|error| panic!("{name} round trip: {error}"));
        assert_eq!(formatted, second, "{name}");
        for comment in source
            .lines()
            .filter_map(|line| line.find('#').map(|offset| line[offset..].trim_end()))
        {
            assert!(
                formatted.contains(comment),
                "{name} lost comment {comment:?}"
            );
        }
    }
}

#[test]
fn formatter_preserves_comments_at_stable_statement_boundaries() {
    let source = "# schema\ntype Task = {id int} # row type\n# query\nfrom tasks | take 1\n  # trailing note\n";
    let formatted = format_source(source).unwrap();
    assert_eq!(
        formatted,
        "# schema\ntype Task =\n  id int\n\n# row type\n# query\nfrom tasks\ntake 1\n\n# trailing note\n"
    );
    assert_eq!(format_source(&formatted).unwrap(), formatted);
    assert_eq!(
        format_source("  # only\n\n# two").unwrap(),
        "# only\n# two\n"
    );
    assert_eq!(format_source("  \n").unwrap(), "");
}

#[test]
fn formatter_covers_statements_stages_patterns_values_and_migrations() {
    for source in [
        "create table legacy (id int, state enum(Pending, Done(text)))\ncreate index legacy (state)",
        "insert tasks $row\nupsert tasks $row",
        "update tasks\nfilter match state\n  Pending => true\n  _ => false\nset score = base + bonus * 2",
        "delete tasks | filter id == $id",
        "explain from tasks | let retryable = attempt -> attempt < 3 | filter retryable attempts | derive score = base + bonus | group {state, owner.id}\n  aggregate\n    rows = count\n    total = sum score\nselect {state, rows}\nsort {-rows, state}\ntake 11..20",
        "from tasks\nderive next = match state\n  Pending {attempt = Some n, ..} => State.Running {attempt = n + 1}\n  Running worker => State.Done (worker, [1, 2])\n  _ => None",
        "migration all_steps\n  parent earlier\n  add type Extra =\n    id int\n  drop type Old\n  add table extras Extra key id\n  drop table old_rows\n  rename table extras to items\n  rename type Extra to Item\n  add field Item.note text = \"\"\n  drop field Item.old\n  change default Item.note to \"new\"\n  drop default Item.note\n  rename field Item.note to label\n  change field Item.score to float using old -> old + 1\n  add variant State.Paused {reason text}\n  drop variant State.Old using old -> State.Done\n  rename variant State.Done to Complete\n  change variant State.Failed to {code int, message text} using old -> {code = 0, message = old.message}\n  add index items.label\n  drop index items.old\n  set key items.id\n  drop key items",
    ] {
        let formatted = format_source(source).unwrap_or_else(|error| panic!("{error}\n{source}"));
        assert_eq!(format_source(&formatted).unwrap(), formatted, "{source}");
    }
}

#[test]
fn formatting_preserves_execution_and_schema_identity() {
    let source = r#"type State = Pending | Done text
type Task = {id int, state State, score int}
table tasks Task
  key id
insert tasks {id = 1, state = Pending, score = 2}
insert tasks {id = 2, state = Done "ok", score = 4}
from tasks | filter state == Pending or score > 3 | derive doubled = score * (1 + 1) | sort id"#;
    let formatted = format_source(source).unwrap();
    let mut original = Engine::memory();
    let mut canonical = Engine::memory();
    let original_response = original.execute(source);
    let canonical_response = canonical.execute(&formatted);
    assert!(original_response.ok, "{}", original_response.message);
    assert!(canonical_response.ok, "{}", canonical_response.message);
    assert_eq!(
        serde_json::to_value(original_response).unwrap(),
        serde_json::to_value(canonical_response).unwrap()
    );
    assert_eq!(original.schema_info(), canonical.schema_info());
}
