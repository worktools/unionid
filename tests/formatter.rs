use unionid::{Engine, format_source};

#[test]
fn formatter_is_idempotent_for_current_examples() {
    for (name, source) in [
        ("tasks", include_str!("../examples/tasks.uid")),
        ("config", include_str!("../examples/config.uid")),
        ("events", include_str!("../examples/events.uid")),
        ("job_queue", include_str!("../examples/job_queue.uid")),
        ("sync", include_str!("../examples/sync_conflicts.uid")),
        (
            "content_metadata",
            include_str!("../examples/content_metadata.uid"),
        ),
        ("mutations", include_str!("../examples/task_mutations.uid")),
        (
            "recursive_tree",
            include_str!("../examples/recursive_tree.uid"),
        ),
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
        "# schema\ntype Task = {\n  id int,\n}\n\n# row type\n# query\nfrom tasks\ntake 1\n\n# trailing note\n"
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
        "create table legacy (id int, state enum(Pending, Done(text)))\ncreate index legacy (state)\ncreate unique index legacy (id)",
        "insert tasks $row | returning\nupsert tasks $row\nreturning id, state",
        "insert many tasks [{id = 2, state = Pending}, {id = 1, state = Done}] | returning {id, state}\ninsert many tasks $rows\nreturning id",
        "upsert many tasks [{id = 2, state = Pending}, {id = 1, state = Done}] | returning {id, state}\nupsert many tasks $rows\nreturning id",
        "update tasks\nfilter match state\n  Pending => true\n  _ => false\nsort {-priority, id}\ntake 1\nset score = base + bonus * 2",
        "update tasks\nset state = match state\n  Pending {attempt} => Running {attempt = attempt + 1}\n  current => current",
        "delete tasks | filter id == $id | returning {id, state}",
        "from tasks | sort {-priority, id} | page 20 after \"u1.payload.mac\"",
        "explain from tasks | let retryable = attempt -> attempt < 3 | filter retryable attempts | derive score = base + bonus | group {state, owner.id}\n  aggregate\n    rows = count\n    total = sum score\nselect {state, rows}\nsort {-rows, state}\ntake 11..20",
        "from tasks\nderive next = match state\n  Pending {attempt = Some n, ..} => State.Running {attempt = n + 1}\n  Running worker => State.Done (worker, [1, 2])\n  _ => None",
        "migration all_steps\n  parent earlier\n  add type Extra =\n    id int\n  drop type Old\n  add table extras Extra key id\n  drop table old_rows\n  rename table extras to items\n  rename type Extra to Item\n  add field Item.note text = \"\"\n  drop field Item.old\n  change default Item.note to \"new\"\n  drop default Item.note\n  rename field Item.note to label\n  change field Item.score to float using old -> old + 1\n  add variant State.Paused {reason text}\n  drop variant State.Old using old -> State.Done\n  rename variant State.Done to Complete\n  change variant State.Failed to {code int, message text} using old -> {code = 0, message = old.message}\n  add index items.label\n  add unique index items.id\n  drop index items.old\n  set key items.id\n  drop key items",
    ] {
        let formatted = format_source(source).unwrap_or_else(|error| panic!("{error}\n{source}"));
        assert_eq!(format_source(&formatted).unwrap(), formatted, "{source}");
    }
}

#[test]
fn formatter_uses_structured_record_and_returning_layout() {
    let source = "insert tasks {id = 1, state = Pending} | returning {id, state}\ninsert many tasks [{id = 2, state = Pending}, {id = 3, state = Done}] | returning {id}\nupdate tasks | set state = Done | returning {id, state}\ndelete tasks\nreturning";
    let formatted = format_source(source).unwrap();
    assert_eq!(
        formatted,
        "insert tasks {\n  id = 1,\n  state = Pending,\n}\nreturning {id, state}\n\ninsert many tasks [\n  {id = 2, state = Pending},\n  {id = 3, state = Done},\n]\nreturning id\n\nupdate tasks\nset state = Done\nreturning {id, state}\n\ndelete tasks\nreturning\n"
    );
    assert_eq!(format_source(&formatted).unwrap(), formatted);
}

#[test]
fn formatter_preserves_boolean_match_results_and_update_precedence() {
    let source = "from jobs | derive retryable = match state\n  Pending {retry_at} => is_some retry_at and true\n  Running {attempt} => attempt < $limit or false\nupdate jobs | set ready = id > 0 and (not ready or contains tags \"active\")";
    let formatted = format_source(source).unwrap();
    assert_eq!(
        formatted,
        "from jobs\nderive retryable = match state {\n  Pending {retry_at} => is_some retry_at and true,\n  Running {attempt} => attempt < $limit or false,\n}\n\nupdate jobs\nset ready = id > 0 and (not ready or contains tags \"active\")\n"
    );
    assert_eq!(format_source(&formatted).unwrap(), formatted);
}

#[test]
fn formatter_wraps_long_boolean_expressions_at_structural_boundaries() {
    let source = "from jobs | derive retryable = any history (attempt -> attempt.outcome == Rejected {code = 503} and any attempt.checkpoints (checkpoint -> checkpoint >= 3) and is_some attempt.note)";
    let formatted = format_source(source).unwrap();
    assert_eq!(
        formatted,
        "from jobs\nderive retryable = (\n  any history (\n    attempt ->\n      attempt.outcome == Rejected {code = 503}\n      and any attempt.checkpoints (checkpoint -> checkpoint >= 3)\n      and is_some attempt.note\n  )\n)\n"
    );
    assert_eq!(format_source(&formatted).unwrap(), formatted);
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

#[test]
fn structured_query_syntax_matches_legacy_layout_and_executes() {
    let legacy = r#"type State =
  Pending
  | Running
    attempt int
  | Done
type Task =
  id int
  priority int
  state State
table tasks Task
  key id
insert tasks
  id = 1
  priority = 10
  state = Running {attempt = 2}
from tasks
filter match state
  Pending => true
  Running {attempt} => attempt < 3
  Done => false
derive label = match state
  Pending => "pending"
  Running {..} => "running"
  Done => "done"
group label
  aggregate
    rows = count
    total = sum priority"#;
    let structured = r#"type State =
  Pending
  | Running {
      attempt int,
    }
  | Done
type Task = {
  id int,
  priority int,
  state State,
}
table tasks Task
  key id
insert tasks {
  id = 1,
  priority = 10,
  state = Running {attempt = 2},
}
from tasks
filter (
  match state {
    Pending => true,
    Running {attempt} => attempt < 3,
    Done => false,
  }
)
derive label = match state {
  Pending => "pending",
  Running {..} => "running",
  Done => "done",
}
group label (
  aggregate {
    rows = count,
    total = sum priority,
  }
)"#;

    let canonical = format_source(structured).unwrap();
    assert_eq!(format_source(legacy).unwrap(), canonical);
    assert_eq!(format_source(&canonical).unwrap(), canonical);

    let mut legacy_engine = Engine::memory();
    let mut structured_engine = Engine::memory();
    let legacy_response = legacy_engine.execute(legacy);
    let structured_response = structured_engine.execute(structured);
    assert!(legacy_response.ok, "{}", legacy_response.message);
    assert!(structured_response.ok, "{}", structured_response.message);
    assert_eq!(
        serde_json::to_value(legacy_response).unwrap(),
        serde_json::to_value(structured_response).unwrap()
    );
    assert_eq!(legacy_engine.schema_info(), structured_engine.schema_info());
}
