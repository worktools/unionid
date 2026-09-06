use unionid::{Engine, QueryResponse, UpsertAction, Value};

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let r = engine.execute(source);
    assert!(r.ok, "{}\n{source}", r.message);
    r
}

fn rows(engine: &mut Engine, source: &str) -> serde_json::Value {
    serde_json::to_value(ok(engine, source).rows).unwrap()
}

#[test]
fn executable_examples() {
    let mut engine = Engine::memory();
    let r = ok(&mut engine, include_str!("../examples/tasks.uid"));
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "title", "owner.email", "state"]
    );
    assert!(r.rows[0]["title"].cmp_eq(&Value::Text("同步目录".into())));
    assert!(engine.schema().contains("type State"));
    assert!(!engine.schema().contains(';'));
    assert_eq!(engine.tables(), ["tasks"]);

    let mut config = Engine::memory();
    let r = ok(&mut config, include_str!("../examples/config.uid"));
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["name", "endpoint.host", "mode"]
    );
    assert!(r.rows[0]["name"].cmp_eq(&Value::Text("worker".into())));
    assert!(r.rows[0]["endpoint.host"].cmp_eq(&Value::Text("worker.internal".into())));

    let mut events = Engine::memory();
    let r = ok(&mut events, include_str!("../examples/events.uid"));
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "event_kind", "event"]
    );
    assert!(r.rows[0]["id"].cmp_eq(&Value::Int(2)));
    assert!(r.rows[0]["event_kind"].cmp_eq(&Value::Text("purchase".into())));

    let mut jobs = Engine::memory();
    let r = ok(&mut jobs, include_str!("../examples/job_queue.uid"));
    assert_eq!(r.rows.len(), 2);
    assert!(r.rows[0]["id"].cmp_eq(&Value::Text("job-c".into())));
    assert!(r.rows[1]["id"].cmp_eq(&Value::Text("job-a".into())));
    assert_eq!(
        r.columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "state_label", "next_attempt", "payload", "state"]
    );
    assert!(
        r.rows[0]["next_attempt"]
            .unwrapped()
            .cmp_eq(&Value::Option(Some(Box::new(Value::Int(2)))))
    );

    let mut sync = Engine::memory();
    let r = ok(&mut sync, include_str!("../examples/sync_conflicts.uid"));
    assert_eq!(r.rows.len(), 3);
    assert!(r.rows[0]["id"].cmp_eq(&Value::Text("docs".into())));
    assert!(r.rows[0]["local_change"].cmp_eq(&Value::Text("modified".into())));
    assert!(r.rows[1]["local_change"].cmp_eq(&Value::Text("added".into())));
    assert!(r.rows[2]["local_change"].cmp_eq(&Value::Text("none".into())));

    let mut mutations = Engine::memory();
    let r = ok(
        &mut mutations,
        include_str!("../examples/task_mutations.uid"),
    );
    assert_eq!(r.rows.len(), 1);
    assert!(r.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(r.rows[0]["attempts"].cmp_eq(&Value::Int(2)));
}

#[test]
fn nested_products_options_lists_and_tuples() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Point = (float, float)\ntype Contact =\n  email text\ntype Row =\n  id int\n  point Point\n  contacts option (list Contact)\ntable places Row\ninsert places\n  id = 1\n  point = (1.5, -2.0)\n  contacts = Some [{email = \"a\"}, {email = \"b\"}]",
    );
    let result = ok(
        &mut e,
        "from places\nfilter contacts == Some [{email = \"a\"}, {email = \"b\"}]\nselect {point}",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.columns[0].ty, "Point");
}

#[test]
fn multiline_variant_payload_and_qualified_constructor() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State =\n  Pending\n  | Running\n      worker text\n      attempt int\ntype Job =\n  state State\ntable jobs Job\ninsert jobs {state = State.Running {worker = \"w\", attempt = 2}}",
    );
    assert_eq!(
        ok(
            &mut e,
            "from jobs | filter state == Running {attempt = 2, worker = \"w\"}"
        )
        .rows
        .len(),
        1
    );
}

#[test]
fn nominal_types_do_not_share_constructor_names() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type A = First | Last\ntype B = First | Last\ntype R =\n  a A\ntable rows R",
    );
    assert!(!e.execute("insert rows {a = B.First}").ok);
    ok(&mut e, "insert rows {a = A.First}");
    assert!(!e.execute("from rows | filter a == B.First").ok);
}

#[test]
fn failed_batch_rolls_back_catalog_rows_and_indexes() {
    let mut e = Engine::memory();
    let r = e.execute("type Task =\n  id int\ntable tasks Task\n  key id\ninsert tasks {id = 1}\ninsert tasks {id = 1}");
    assert!(!r.ok);
    assert_eq!(r.error.unwrap().code, "E_CONSTRAINT");
    assert!(e.tables().is_empty());
    assert!(e.schema().is_empty());
    ok(
        &mut e,
        "type Task =\n  id int\ntable tasks Task\n  key id\ninsert tasks {id = 1}",
    );
    assert!(!e.execute("insert tasks {id = 2}\ninsert tasks {id = 1}").ok);
    assert_eq!(ok(&mut e, "from tasks").rows.len(), 1);
    assert!(ok(&mut e, "from tasks | filter id == 2").rows.is_empty());
}

#[test]
fn schema_revision_tracks_atomic_catalog_changes() {
    let mut e = Engine::memory();
    let empty = e.schema_info();
    assert_eq!(empty.revision, 0);
    assert!(empty.hash.starts_with("sha256:"));

    let created = ok(
        &mut e,
        "type State = Pending | Done\ntype Task =\n  id int\n  state State\ntable tasks Task\n  key id",
    );
    let v1 = created.schema.unwrap();
    assert_eq!(v1.revision, 1, "one atomic script creates one revision");
    assert_ne!(v1.hash, empty.hash);
    assert_eq!(e.schema_info(), v1);

    let inserted = ok(&mut e, "insert tasks {id = 1, state = Pending}");
    assert_eq!(inserted.schema.as_ref(), Some(&v1));
    assert_eq!(e.schema_info(), v1, "row writes do not change the schema");

    let failed = e.execute("type Later = text\ntable tasks Later");
    assert!(!failed.ok);
    assert_eq!(failed.schema.as_ref(), Some(&v1));
    assert_eq!(e.schema_info(), v1, "a failed script publishes no revision");

    let changed = ok(&mut e, "create index tasks (state)");
    let v2 = changed.schema.unwrap();
    assert_eq!(v2.revision, 2);
    assert_ne!(v2.hash, v1.hash);
    assert_eq!(ok(&mut e, "from tasks").schema.as_ref(), Some(&v2));
}

#[test]
fn type_and_table_names_share_one_schema_namespace() {
    let mut type_first = Engine::memory();
    ok(&mut type_first, "type Item = text");
    let error = type_first.execute("create table Item (id int)");
    assert!(!error.ok);
    assert_eq!(error.error.unwrap().code, "E_SCHEMA");

    let mut table_first = Engine::memory();
    ok(&mut table_first, "create table Item (id int)");
    let error = table_first.execute("type Item = text");
    assert!(!error.ok);
    assert_eq!(error.error.unwrap().code, "E_SCHEMA");
}

#[test]
fn tables_sharing_an_adt_use_the_same_nominal_identity() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Pending | Done\ntype Task =\n  id int\n  state State\ntable active Task\ntable archive Task\ninsert active {id = 1, state = Pending}\ninsert archive {id = 2, state = Done}",
    );
    let active = ok(&mut e, "from active");
    let archive = ok(&mut e, "from archive");
    let Value::Named {
        type_id: active_id, ..
    } = &active.rows[0]["state"]
    else {
        panic!("State should preserve its nominal identity")
    };
    let Value::Named {
        type_id: archive_id,
        ..
    } = &archive.rows[0]["state"]
    else {
        panic!("State should preserve its nominal identity")
    };
    assert_eq!(active_id, archive_id);
}

#[test]
fn failure_in_final_query_rolls_back_prior_writes() {
    let mut e = Engine::memory();
    let r =
        e.execute("create table things (id int)\ninsert things {id:1}\nfrom things\nselect typo");
    assert!(!r.ok);
    assert!(e.tables().is_empty());
    assert_eq!(r.error.unwrap().span.unwrap().line, 3);
}

#[test]
fn required_fields_and_nested_errors_are_strict() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Contact =\n  email text\ntype R =\n  id int\n  contact Contact\n  nickname option text\ntable people R",
    );
    for source in [
        "insert people {id = 1, contact = {email = \"x\"}}",
        "insert people {id = 1, contact = {email = 5}, nickname = None}",
        "insert people {id = 1, contact = {email = \"x\"}, nickname = null}",
        "insert people {id = 1, contact = {email = \"x\"}, nickname = None, extra = 2}",
    ] {
        assert!(!e.execute(source).ok, "{source}");
    }
    assert!(
        e.execute("insert people {id = 1, contact = {email = 5}, nickname = None}")
            .message
            .contains("people.contact.email")
    );
    assert_eq!(ok(&mut e, "from people").rows.len(), 0);
}

#[test]
fn defaults_fill_omitted_fields_at_each_record_level() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Mode = Development | Production\ntype Endpoint =\n  host text = \"localhost\"\n  port int = 8080\ntype Config =\n  name text\n  endpoint Endpoint =\n    port = 9000\n  mode Mode = Development\n  tags list text = []\n  owner option text = None\ntable configs Config\n  key name\ninsert configs {name = \"implicit\"}\ninsert configs {name = \"explicit\", endpoint = {}}",
    );

    let implicit = ok(
        &mut e,
        "from configs | filter name == \"implicit\" | select {endpoint.host, endpoint.port, mode, tags, owner}",
    );
    assert!(implicit.rows[0]["endpoint.host"].cmp_eq(&Value::Text("localhost".into())));
    assert!(implicit.rows[0]["endpoint.port"].cmp_eq(&Value::Int(9000)));
    assert!(implicit.rows[0]["tags"].cmp_eq(&Value::List(Vec::new())));
    assert!(implicit.rows[0]["owner"].cmp_eq(&Value::Option(None)));

    let explicit = ok(
        &mut e,
        "from configs | filter name == \"explicit\" | select {endpoint.port}",
    );
    assert!(explicit.rows[0]["endpoint.port"].cmp_eq(&Value::Int(8080)));
}

#[test]
fn variant_record_defaults_are_applied_and_explicit_values_are_checked() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Event = Failed {message text, retryable bool = false} | Done\ntype Row =\n  id int\n  event Event\ntable events Row\ninsert events {id = 1, event = Failed {message = \"network\"}}",
    );
    assert_eq!(
        ok(
            &mut e,
            "from events\nfilter match event\n  Failed {retryable, ..} => retryable == false\n  Done => false"
        )
        .rows
        .len(),
        1
    );
    let error =
        e.execute("insert events {id = 2, event = Failed {message = \"x\", retryable = 1}}");
    assert!(!error.ok);
    assert_eq!(error.error.unwrap().code, "E_TYPE");
}

#[test]
fn invalid_defaults_reject_the_complete_schema_batch() {
    for source in [
        "type Bad =\n  count int = \"many\"",
        "type Bad =\n  owner option text = null",
        "type Bad =\n  state enum(Ready) = Missing",
        "type Nested =\n  enabled bool\ntype Bad =\n  nested Nested = {enabled = 1}",
        "type Nested =\n  enabled bool\ntype Bad =\n  nested Nested = {}",
    ] {
        let mut e = Engine::memory();
        let error = e.execute(source);
        assert!(!error.ok, "accepted {source}");
        assert!(matches!(
            error.error.as_ref().unwrap().code.as_str(),
            "E_TYPE" | "E_FIELD"
        ));
        assert!(error.message.contains("default for field"), "{error:?}");
        assert!(e.schema().is_empty());
    }
}

#[test]
fn errors_are_checked_before_scanning_empty_tables() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "create table users (id int, name text, state enum(A, B(int)))",
    );
    for source in [
        "from users | select typo",
        "from users | filter typo == 1",
        "from users | select name | filter id == 1",
        "from users | sort typo",
        "from users | filter id == \"1\"",
        "from users | filter state > A",
        "from users | sort state",
    ] {
        assert!(!e.execute(source).ok, "{source}");
    }
}

#[test]
fn stage_order_and_projection_paths_are_preserved() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "create table t (n int)\ninsert t {n:1}\ninsert t {n:2}\ninsert t {n:3}",
    );
    assert!(ok(&mut e, "from t | take 1 | filter n > 1").rows.is_empty());
    assert_eq!(ok(&mut e, "from t | filter n > 1 | take 1").rows.len(), 1);
    assert!(ok(&mut e, "from t | sort -n | take 1").rows[0]["n"].cmp_eq(&Value::Int(3)));
    ok(&mut e, include_str!("../examples/tasks.uid"));
    assert_eq!(
        ok(
            &mut e,
            "from tasks | select {owner.email} | filter owner.email == \"alice@example.com\""
        )
        .rows
        .len(),
        1
    );
}

#[test]
fn multi_key_sort_and_inclusive_take_ranges_compose() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Meta =\n  scheduled int\ntype Row =\n  id int\n  priority int\n  meta Meta\ntable rows Row\ninsert rows {id = 1, priority = 2, meta = {scheduled = 5}}\ninsert rows {id = 2, priority = 2, meta = {scheduled = 3}}\ninsert rows {id = 3, priority = 3, meta = {scheduled = 9}}\ninsert rows {id = 4, priority = 2, meta = {scheduled = 3}}\ninsert rows {id = 5, priority = 1, meta = {scheduled = 1}}",
    );
    let result = ok(
        &mut e,
        "from rows\nsort {\n  -priority,\n  meta.scheduled,\n  id,\n}\ntake 2..4\nselect {id}",
    );
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| match &row["id"] {
                Value::Int(value) => *value,
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        [2, 4, 1]
    );
    assert_eq!(
        ok(&mut e, "from rows | sort {id} | take 4..10").rows.len(),
        2
    );
    assert!(ok(&mut e, "from rows | take 6..10").rows.is_empty());
    assert_eq!(
        rows(&mut e, "from rows | sort -id | take 2"),
        rows(&mut e, "from rows | sort {-id} | take 1..2")
    );
}

#[test]
fn sort_keys_and_take_ranges_are_validated_before_scanning() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Ready | Done\ntype Row =\n  id int\n  state State\ntable rows Row",
    );
    for source in [
        "from rows | sort id, -id",
        "from rows | sort {id, id}",
        "from rows | sort {id, missing}",
        "from rows | sort {id, state}",
        "from rows | take 0..1",
        "from rows | take 3..2",
        "from rows | take 1.5",
        "from rows | take 1..2..3",
        "from rows | take 1..18446744073709551616",
    ] {
        let result = e.execute(source);
        assert!(!result.ok, "accepted {source}");
        assert!(result.error.unwrap().span.is_some(), "{source}");
    }
}

#[test]
fn newline_and_inline_pipelines_have_identical_results() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "create table t (n int)\ninsert t {n:1}\ninsert t {n:2}",
    );
    assert_eq!(
        rows(
            &mut e,
            "from t | filter n >= 1 | select {n} | sort -n | take 1"
        ),
        rows(
            &mut e,
            "from t\n# comment\nfilter n >= 1\n\nselect {n}\nsort -n\ntake 1"
        )
    );
}

#[test]
fn boolean_filters_compose_fields_lists_and_length() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Tag = Sync | Local | Remote\ntype Tags = list Tag\ntype Job =\n  id int\n  priority int\n  threshold int\n  archived bool\n  tags Tags\n  label text\ntable jobs Job\ninsert jobs {id = 1, priority = 0, threshold = 10, archived = true, tags = [], label = \"old\"}\ninsert jobs {id = 2, priority = 10, threshold = 10, archived = false, tags = [Sync], label = \"sync\"}\ninsert jobs {id = 3, priority = 0, threshold = 10, archived = false, tags = [Sync, Local], label = \"草稿\"}",
    );

    let result = ok(
        &mut e,
        "from jobs\nfilter archived or priority >= threshold and contains tags Sync\nsort id\nselect {id}",
    );
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| &row["id"])
            .filter_map(|value| match value {
                Value::Int(value) => Some(*value),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [1, 2]
    );

    let grouped = ok(
        &mut e,
        "from jobs\nfilter (archived or priority >= threshold) and contains tags Sync\nfilter contains tags Sync and not archived\nfilter length tags >= 1 and length label == 4\nselect {id}",
    );
    assert_eq!(grouped.rows.len(), 1);
    assert!(grouped.rows[0]["id"].cmp_eq(&Value::Int(2)));

    let block = ok(
        &mut e,
        "from jobs\nfilter\n  (archived or priority >= threshold)\n  and contains tags Sync\n  and not archived\nselect {id}",
    );
    assert_eq!(block.rows.len(), 1);
    assert!(block.rows[0]["id"].cmp_eq(&Value::Int(2)));

    let parenthesized = ok(
        &mut e,
        "from jobs\nfilter (\n  archived\n  or priority >= threshold\n)\nsort id\nselect {id}",
    );
    assert_eq!(parenthesized.rows.len(), 2);
    assert_eq!(
        ok(&mut e, "from jobs | filter 10 <= priority | sort id")
            .rows
            .len(),
        1
    );
    assert!(
        ok(&mut e, "from jobs | filter contains [] 1")
            .rows
            .is_empty()
    );
    assert_eq!(
        ok(&mut e, "from jobs | filter length label == 2")
            .rows
            .len(),
        1
    );
}

#[test]
fn match_conditions_share_boolean_and_collection_expressions() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Ready {urgent bool, attempts int, max_attempts int, tags list text} | Done\ntype Job =\n  id int\n  state State\ntable jobs Job\ninsert jobs {id = 1, state = Ready {urgent = true, attempts = 1, max_attempts = 3, tags = [\"sync\"]}}\ninsert jobs {id = 2, state = Ready {urgent = true, attempts = 3, max_attempts = 3, tags = [\"sync\"]}}\ninsert jobs {id = 3, state = Done}",
    );
    let result = ok(
        &mut e,
        "from jobs\nfilter match state\n  Ready {urgent, attempts, max_attempts, tags} =>\n    urgent\n    and attempts < max_attempts\n    and contains tags \"sync\"\n  Done => false\nselect {id}",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(1)));
}

#[test]
fn boolean_expressions_are_checked_before_scanning_empty_tables() {
    let setup = "type State = Ready {urgent bool, attempts int} | Done\ntype Job =\n  priority int\n  archived bool\n  maybe option bool\n  tags list text\n  state State\ntable jobs Job";
    for (query, code, message) in [
        ("filter priority and archived", "E_TYPE", "must be bool"),
        ("filter contains priority 1", "E_TYPE", "expects a list"),
        ("filter contains tags 1", "E_TYPE", "expected text"),
        ("filter length priority > 0", "E_TYPE", "length expects"),
        ("filter archived or missing", "E_FIELD", "unknown field"),
        ("filter maybe", "E_TYPE", "must be bool"),
        (
            "filter match state\n  Ready {urgent, attempts} => attempts and urgent\n  Done => false",
            "E_TYPE",
            "must be bool",
        ),
    ] {
        let mut e = Engine::memory();
        ok(&mut e, setup);
        let result = e.execute(&format!("from jobs\n{query}"));
        assert!(!result.ok, "accepted {query}");
        let error = result.error.unwrap();
        assert_eq!(error.code, code, "{query}: {error}");
        assert!(error.message.contains(message), "{query}: {error}");
    }

    let mut e = Engine::memory();
    ok(&mut e, setup);
    let too_deep = format!("from jobs | filter {}archived", "not ".repeat(64));
    let error = e.execute(&too_deep).error.unwrap();
    assert_eq!(error.code, "E_LIMIT");
}

#[test]
fn typed_arithmetic_composes_filters_match_conditions_and_derives() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Attempts = int\ntype State = Ready {attempts Attempts, limit Attempts} | Done\ntype Job =\n  id int\n  priority int\n  bonus int\n  ratio float\n  state State\ntable jobs Job\ninsert jobs {id = 1, priority = 4, bonus = 3, ratio = 5.0, state = Ready {attempts = 2, limit = 3}}\ninsert jobs {id = 2, priority = 5, bonus = 1, ratio = 8.0, state = Ready {attempts = 4, limit = 8}}\ninsert jobs {id = 3, priority = 10, bonus = 0, ratio = 2.0, state = Done}",
    );
    let result = ok(
        &mut engine,
        "from jobs\nfilter (\n  priority\n  + bonus * 2\n) >= 10\nfilter ratio / 2.0 < 3.0\nfilter ratio == (2 + 3)\nfilter match state\n  Ready {attempts, limit} => attempts + 1 >= limit\n  Done => false\nderive next_attempt =\n  match state\n    Ready {attempts, ..} => attempts + 1\n    Done => 0\nselect {id, next_attempt}",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(
        result.rows[0]["next_attempt"]
            .unwrapped()
            .cmp_eq(&Value::Int(3))
    );
    assert!(matches!(
        result.rows[0]["next_attempt"],
        Value::Named { .. }
    ));
    assert_eq!(result.columns[1].ty, "Attempts");
}

#[test]
fn arithmetic_precedence_grouping_and_integer_division_are_explicit() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type State = Value {a int, b int} | Empty\ntype Row =\n  id int\n  state State\ntable rows Row\ninsert rows {id = 1, state = Value {a = 5, b = 2}}",
    );
    let result = ok(
        &mut engine,
        "from rows\nderive precedence =\n  match state\n    Value {a, b} => a + b * 3\n    Empty => 0\nderive grouped =\n  match state\n    Value {a, b} => (a + b) * 3\n    Empty => 0\nderive subtracted =\n  match state\n    Value {a, b} => a - b\n    Empty => 0\nderive divided =\n  match state\n    Value {a, b} => a / b\n    Empty => 0\nderive negated =\n  match state\n    Value {a, ..} => -a\n    Empty => 0\nselect {precedence, grouped, subtracted, divided, negated}",
    );
    assert!(result.rows[0]["precedence"].cmp_eq(&Value::Int(11)));
    assert!(result.rows[0]["grouped"].cmp_eq(&Value::Int(21)));
    assert!(result.rows[0]["subtracted"].cmp_eq(&Value::Int(3)));
    assert!(result.rows[0]["divided"].cmp_eq(&Value::Int(2)));
    assert!(result.rows[0]["negated"].cmp_eq(&Value::Int(-5)));
}

#[test]
fn arithmetic_type_and_runtime_failures_are_structured() {
    let setup = "type State = Value {number int, ratio float, label text} | Empty\ntype Row =\n  state State\ntable rows Row";
    for (expression, expected) in [
        ("number + label", "different types"),
        ("number + ratio", "different types"),
        ("label - 1", "expects int or float"),
    ] {
        let mut engine = Engine::memory();
        ok(&mut engine, setup);
        let result = engine.execute(&format!(
            "from rows\nderive output =\n  match state\n    Value {{number, ratio, label}} => {expression}\n    Empty => 0"
        ));
        assert!(!result.ok, "accepted {expression}");
        assert_eq!(result.error.as_ref().unwrap().code, "E_TYPE");
        assert!(result.message.contains(expected), "{}", result.message);
    }

    for (value, expression, expected) in [
        ("1", "number / 0", "division by zero"),
        ("9223372036854775807", "number + 1", "overflow"),
        ("-9223372036854775808", "-number", "overflow"),
    ] {
        let mut engine = Engine::memory();
        ok(
            &mut engine,
            &format!(
                "type State = Value {{number int}} | Empty\ntype Row =\n  state State\ntable rows Row\ninsert rows {{state = Value {{number = {value}}}}}"
            ),
        );
        let result = engine.execute(&format!(
            "from rows\nderive output =\n  match state\n    Value {{number}} => {expression}\n    Empty => 0"
        ));
        assert!(!result.ok, "accepted {expression}");
        assert_eq!(result.error.as_ref().unwrap().code, "E_ARITH");
        assert!(result.message.contains(expected), "{}", result.message);
    }

    for (expression, expected) in [
        ("ratio / 0.0", "division by zero"),
        ("ratio * ratio", "non-finite"),
    ] {
        let mut engine = Engine::memory();
        ok(
            &mut engine,
            "type State = Value {ratio float} | Empty\ntype Row =\n  state State\ntable rows Row\ninsert rows {state = Value {ratio = 1e308}}",
        );
        let result = engine.execute(&format!(
            "from rows\nderive output =\n  match state\n    Value {{ratio}} => {expression}\n    Empty => 0.0"
        ));
        assert!(!result.ok, "accepted {expression}");
        assert_eq!(result.error.as_ref().unwrap().code, "E_ARITH");
        assert!(result.message.contains(expected), "{}", result.message);
    }
}

#[test]
fn boolean_short_circuit_skips_failing_arithmetic() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Row =\n  id int\ntable rows Row\ninsert rows {id = 1}",
    );
    assert!(
        ok(&mut engine, "from rows | filter false and 1 / 0 == 0")
            .rows
            .is_empty()
    );
    assert_eq!(
        ok(&mut engine, "from rows | filter true or 1 / 0 == 0")
            .rows
            .len(),
        1
    );
}

#[test]
fn exact_i64_comparisons_with_and_without_indexes() {
    let values = [
        i64::MIN,
        -1,
        0,
        1,
        9_007_199_254_740_992,
        9_007_199_254_740_993,
        i64::MAX,
    ];
    let mut e = Engine::memory();
    ok(&mut e, "create table t (n int)");
    for n in values {
        ok(&mut e, &format!("insert t {{n:{n}}}"));
    }
    for indexed in [false, true] {
        if indexed {
            ok(&mut e, "create index t (n)");
        }
        for n in values {
            for op in ["==", "!=", ">", ">=", "<", "<="] {
                let expected = values
                    .iter()
                    .filter(|v| match op {
                        "==" => **v == n,
                        "!=" => **v != n,
                        ">" => **v > n,
                        ">=" => **v >= n,
                        "<" => **v < n,
                        _ => **v <= n,
                    })
                    .count();
                assert_eq!(
                    ok(&mut e, &format!("from t | filter n {op} {n}"))
                        .rows
                        .len(),
                    expected,
                    "{op} {n}, indexed={indexed}"
                );
            }
        }
    }
    assert_eq!(ok(&mut e, "from t | filter 1 == n").rows.len(), 1);
    assert!(!e.execute("from t | filter n == 1.0").ok);
}

#[test]
fn float_zero_and_nested_enum_equality_keys_are_consistent() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "create table t (n float, kind enum(A(float)))\ninsert t {n:0.0,kind:A(0.0)}\ninsert t {n:-0.0,kind:A(-0.0)}\ninsert t {n:0.0000000000000001,kind:A(0.0000000000000001)}",
    );
    for indexed in [false, true] {
        if indexed {
            ok(&mut e, "create index t (n)\ncreate index t (kind)");
        }
        assert_eq!(ok(&mut e, "from t | filter n == 0.0").rows.len(), 2);
        assert_eq!(ok(&mut e, "from t | filter n == 0").rows.len(), 2);
        assert_eq!(ok(&mut e, "from t | filter 0 == n").rows.len(), 2);
        assert_eq!(ok(&mut e, "from t | filter kind == A(-0.0)").rows.len(), 2);
        assert_eq!(
            ok(&mut e, "from t | filter n == 0.0000000000000001")
                .rows
                .len(),
            1
        );
    }
    assert!(!e.execute("insert t {n:9007199254740993,kind:A(0.0)}").ok);
}

#[test]
fn indexes_on_nested_records_preserve_results() {
    let mut e = Engine::memory();
    ok(&mut e, include_str!("../examples/tasks.uid"));
    let before = rows(
        &mut e,
        "from tasks | filter owner.email == \"alice@example.com\"",
    );
    ok(&mut e, "create index tasks (owner.email)");
    assert_eq!(
        before,
        rows(
            &mut e,
            "from tasks | filter owner.email == \"alice@example.com\""
        )
    );
    assert!(
        ok(&mut e, "from tasks | filter owner.email == \"absent\"")
            .rows
            .is_empty()
    );
}

#[test]
fn literals_are_not_split_on_pipes_punctuation_or_escaped_quotes() {
    let mut e = Engine::memory();
    let text = "中文 a|b |> c, : # \"quoted\"\nnext";
    let literal = serde_json::to_string(text).unwrap();
    ok(
        &mut e,
        &format!("create table t (s text)\ninsert t {{s:{literal}}}"),
    );
    assert_eq!(
        ok(&mut e, &format!("from t | filter s == {literal}"))
            .rows
            .len(),
        1
    );
}

#[test]
fn reject_invalid_syntax_without_partial_changes() {
    for source in [
        "type R =\n  id int;",
        "type R =\n\tid int",
        "type R =\n  a int\n b int",
        "create table t (id int, id text)",
        "create table t (id int) garbage",
        "type R = {id: int}",
        "create table t (n int)\ninsert t {n:1,n:2}",
        "type S = A | A",
        "type R =\n  n option\ntable t R",
        "type R = {n int",
        "from t |",
        "from t | take -1",
        "create table t (n int)\ninsert t {n:9223372036854775808}",
        "create table t (n float)\ninsert t {n:1e999}",
    ] {
        let mut e = Engine::memory();
        let r = e.execute(source);
        assert!(!r.ok, "accepted {source}");
        assert!(e.tables().is_empty());
        assert!(r.error.unwrap().span.is_some());
    }
}

#[test]
fn nesting_and_source_limits_are_controlled_errors() {
    let mut e = Engine::memory();
    assert_eq!(
        e.execute(&format!("type R =\n  x {}int", "option ".repeat(80)))
            .error
            .unwrap()
            .code,
        "E_LIMIT"
    );
    assert_eq!(
        e.execute(&" ".repeat(unionid::syntax::MAX_SOURCE_BYTES + 1))
            .error
            .unwrap()
            .code,
        "E_LIMIT"
    );
    assert!(!e.execute("type Tree =\n  child Tree").ok);
}

#[test]
fn multiple_queries_and_comments_do_not_require_blank_lines() {
    let mut e = Engine::memory();
    let r = ok(
        &mut e,
        "type R =\n  n int\ntable t R\ninsert t\n  n = 3\nfrom t\nfilter n == 1\n# the next from starts a new query\nfrom t\nfilter n == 3",
    );
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn schema_display_is_reusable_for_named_and_legacy_types() {
    let mut original = Engine::memory();
    ok(
        &mut original,
        "type Flag =\n  Enabled\ntype Pair =\n  Wrapped((int, text))\ntype R =\n  id int\n  flag Flag = Enabled\n  pair Pair\n  note option text = None\ntable typed R\n  key id\ncreate table old (n int = 1, kind enum(A, B(float)) = A)",
    );
    let schema = original.schema();
    assert!(schema.contains("note option text = None"));
    assert!(schema.contains("create table old (n int = 1, kind enum(A, B(float)) = A)"));
    let mut restored = Engine::memory();
    ok(&mut restored, &schema);
    assert_eq!(schema, restored.schema());
    ok(
        &mut restored,
        "insert typed {id = 1, pair = Wrapped((2, \"x\"))}",
    );
    assert_eq!(ok(&mut restored, "from typed").rows.len(), 1);
}

#[test]
fn every_default_literal_form_round_trips_through_schema_text() {
    let mut original = Engine::memory();
    ok(
        &mut original,
        "type Choice = A | B(int) | C {label text}\ntype Defaults =\n  integer int = 1\n  decimal float = 2\n  enabled bool = true\n  title text = \"line\\nnext\"\n  pair (int, text) = (3, \"three\")\n  numbers list int = [1, 2]\n  absent option text = None\n  present option text = Some \"value\"\n  choice Choice = B(4)\n  record_choice Choice = C {label = \"c\"}\n  nested {value int = 5} = {}\ntable defaults Defaults",
    );
    let schema = original.schema();
    let mut restored = Engine::memory();
    ok(&mut restored, &schema);
    assert_eq!(restored.schema(), schema);
    ok(&mut restored, "insert defaults {}");
    let row = ok(&mut restored, "from defaults");
    assert!(row.rows[0]["decimal"].cmp_eq(&Value::Float(2.0)));
    assert!(
        row.rows[0]["present"].cmp_eq(&Value::Option(Some(Box::new(Value::Text("value".into())))))
    );
}

#[test]
fn parser_handles_generated_invalid_inputs_without_panics() {
    let alphabet = b"abcABC019{}[]()\"\\,|:=\n \t#;+-";
    let mut state = 42_u64;
    for _ in 0..1000 {
        let mut source = String::new();
        for _ in 0..80 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            source.push(alphabet[(state >> 32) as usize % alphabet.len()] as char);
        }
        let _ = unionid::syntax::parse(&source);
    }
}

#[test]
fn boolean_literals_do_not_shadow_user_defined_constructors() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Choice = True | False\ntype R =\n  choice Choice\n  active bool\ntable t R\ninsert t {choice = True, active = true}",
    );
    assert_eq!(ok(&mut e, "from t | filter choice == True").rows.len(), 1);
}

#[test]
fn match_filters_sum_variants_and_record_payloads() {
    let mut e = Engine::memory();
    ok(&mut e, include_str!("../examples/tasks.uid"));
    let result = ok(
        &mut e,
        "from tasks\nfilter match state\n  State.Pending => false\n  State.Running {worker, attempt} => worker == \"local\"\n  State.Done {result} => result == \"ok\"\n  State.Failed {retryable, ..} => retryable\nselect {id}\nsort id",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(1)));
}

#[test]
fn match_is_checked_for_exhaustiveness_before_scanning() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Pending | Running {attempt int} | Done\ntype R =\n  state State\ntable tasks R",
    );
    let error = e.execute(
        "from tasks\nfilter match state\n  Pending => false\n  Running {attempt} => attempt > 0",
    );
    assert!(!error.ok);
    assert_eq!(error.error.as_ref().unwrap().code, "E_MATCH");
    assert!(error.message.contains("missing Done"), "{}", error.message);
    assert!(
        ok(
            &mut e,
            "from tasks\nfilter match state\n  Pending => false\n  Running {attempt} => attempt > 0\n  Done => true"
        )
        .rows
        .is_empty()
    );
}

#[test]
fn match_rejects_unreachable_or_invalid_patterns_and_branch_scope() {
    let setup = "type State = Pending | Running {worker text, attempt int}\ntype Other = Pending | Running {worker text, attempt int}\ntype Pair = Pair(int, text) | Empty\ntype R =\n  state State\n  count int\n  pair Pair\ntable tasks R";
    for (query, expected) in [
        ("filter match count\n  _ => true", "must be a sum type"),
        (
            "filter match pair\n  Pair only => true\n  Empty => false",
            "expects 2 payload binding(s), got 1",
        ),
        (
            "filter match state\n  _ => false\n  Pending => true",
            "wildcard match branch must be last",
        ),
        (
            "filter match state\n  Pending => true\n  Pending => false\n  _ => false",
            "matched more than once",
        ),
        (
            "filter match state\n  Other.Pending => true\n  _ => false",
            "belongs to a different type",
        ),
        (
            "filter match state\n  Missing => true\n  _ => false",
            "unknown variant",
        ),
        (
            "filter match state\n  Pending {} => true\n  _ => false",
            "has no record payload",
        ),
        (
            "filter match state\n  Running => true\n  _ => false",
            "expects 1 payload binding(s)",
        ),
        (
            "filter match state\n  Running {attempt} => attempt > 0\n  _ => false",
            "add '..' to ignore",
        ),
        (
            "filter match state\n  Running {missing, ..} => true\n  _ => false",
            "has no payload field",
        ),
        (
            "filter match state\n  Running {attempt, attempt, ..} => true\n  _ => false",
            "bound more than once",
        ),
        (
            "filter match state\n  Running {.., attempt} => true\n  _ => false",
            "'..' must be the last item",
        ),
        (
            "filter match state\n  Pending => attempt > 0\n  _ => false",
            "unknown match binding",
        ),
        (
            "filter match state\n  Running {worker, ..} => worker\n  _ => false",
            "must be bool",
        ),
    ] {
        let mut e = Engine::memory();
        ok(&mut e, setup);
        let result = e.execute(&format!("from tasks\n{query}"));
        assert!(!result.ok, "accepted {query}");
        assert!(
            result.message.contains(expected),
            "{}: {query}",
            result.message
        );
    }
}

#[test]
fn match_conditions_support_named_bools_and_nested_binding_paths() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Enabled = bool\ntype Meta =\n  attempts int\ntype State = Active {enabled Enabled, meta Meta} | Idle\ntype R =\n  state State\ntable rows R\ninsert rows {state = Active {enabled = true, meta = {attempts = 3}}}",
    );
    assert_eq!(
        ok(
            &mut e,
            "from rows\nfilter match state\n  Active {enabled, meta} => enabled\n  Idle => false"
        )
        .rows
        .len(),
        1
    );
    assert_eq!(
        ok(
            &mut e,
            "from rows\nfilter match state\n  Active {enabled, meta} => meta.attempts >= 3\n  _ => false"
        )
        .rows
        .len(),
        1
    );
}

#[test]
fn option_and_positional_payloads_can_be_matched_without_extra_punctuation() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Pair = Pair(int, text) | Empty\ntype R =\n  id int\n  retry_at option int\n  pair Pair\ntable rows R\ninsert rows {id = 1, retry_at = None, pair = Empty}\ninsert rows {id = 2, retry_at = Some 20, pair = Pair(2, \"two\")}\ninsert rows {id = 3, retry_at = Some 30, pair = Pair(3, \"three\")}",
    );
    assert_eq!(
        ok(
            &mut e,
            "from rows\nfilter match retry_at\n  Some at => at >= 20\n  None => false"
        )
        .rows
        .len(),
        2
    );
    let result = ok(
        &mut e,
        "from rows\nfilter match pair\n  Pair number _ => number == 3\n  Empty => false\nselect {id}",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(3)));
    assert!(
        !e.execute("from rows\nfilter match retry_at\n  Some _ => true")
            .ok
    );
}

#[test]
fn derive_match_adds_a_typed_column_for_sum_and_option_values() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Queued {scheduled_at int} | Failed {retry_at option int, message text} | Done\ntype Meta =\n  owner option text\n  state State\ntype Job =\n  id int\n  meta Meta\ntable jobs Job\ninsert jobs {id = 1, meta = {owner = None, state = Queued {scheduled_at = 10}}}\ninsert jobs {id = 2, meta = {owner = Some \"alice\", state = Failed {retry_at = Some 30, message = \"network\"}}}\ninsert jobs {id = 3, meta = {owner = Some \"bob\", state = Done}}",
    );
    let result = ok(
        &mut e,
        "from jobs\nderive status =\n  match meta.state\n    Queued {..} => \"queued\"\n    Failed {..} => \"failed\"\n    Done => \"done\"\nderive retry_at =\n  match meta.state\n    Failed {retry_at = at, ..} => at\n    _ => None\nderive owner_name =\n  match meta.owner\n    Some name => name\n    None => \"unowned\"\nfilter retry_at == Some 30\nselect {id, status, retry_at, owner_name}",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(2)));
    assert!(result.rows[0]["status"].cmp_eq(&Value::Text("failed".into())));
    assert!(result.rows[0]["retry_at"].cmp_eq(&Value::Option(Some(Box::new(Value::Int(30))))));
    assert!(result.rows[0]["owner_name"].cmp_eq(&Value::Text("alice".into())));
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.ty.as_str()))
            .collect::<Vec<_>>(),
        [
            ("id", "int"),
            ("status", "text"),
            ("retry_at", "option int"),
            ("owner_name", "text"),
        ]
    );
}

#[test]
fn derive_match_supports_positional_bindings_and_inline_layout() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Pair = Pair(int, text) | Empty\ntype R =\n  id int\n  pair Pair\ntable rows R\ninsert rows {id = 1, pair = Pair(7, \"seven\")}\ninsert rows {id = 2, pair = Empty}",
    );
    let result = ok(
        &mut e,
        "from rows\nderive number = match pair\n  Pair value _ => value\n  Empty => 0\nsort id\nselect {id, number}",
    );
    assert!(result.rows[0]["number"].cmp_eq(&Value::Int(7)));
    assert!(result.rows[1]["number"].cmp_eq(&Value::Int(0)));
}

#[test]
fn derive_match_is_fully_checked_on_empty_tables() {
    let setup = "type State = A {value int} | B\ntype R =\n  id int\n  optional option int\n  state State\ntable rows R";
    for (query, expected) in [
        (
            "derive id =\n  match state\n    A {value} => value\n    B => 0",
            "already exists",
        ),
        (
            "derive value =\n  match state\n    A {value} => value\n    B => \"wrong\"",
            "expected int",
        ),
        (
            "derive value =\n  match state\n    A {missing, ..} => missing\n    B => 0",
            "no payload field",
        ),
        (
            "derive value =\n  match state\n    A {value} => value",
            "non-exhaustive",
        ),
        (
            "derive value =\n  match optional\n    None => None\n    Some _ => None",
            "cannot infer type",
        ),
        (
            "derive value =\n  match id\n    _ => 0",
            "must be a sum type or option",
        ),
    ] {
        let mut engine = Engine::memory();
        ok(&mut engine, setup);
        let result = engine.execute(&format!("from rows\n{query}"));
        assert!(!result.ok, "accepted {query}");
        assert!(
            result.message.contains(expected),
            "{}: {query}",
            result.message
        );
    }
}

#[test]
fn derive_match_preserves_nominal_result_types() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type First = text\ntype Second = text\ntype Choice = Left(First) | Right(Second)\ntype R =\n  choice Choice\ntable rows R",
    );
    let result = e.execute(
        "from rows\nderive value =\n  match choice\n    Left value => value\n    Right value => value",
    );
    assert!(!result.ok);
    assert_eq!(result.error.unwrap().code, "E_TYPE");
    assert!(result.message.contains("Second"), "{}", result.message);
    assert!(result.message.contains("First"), "{}", result.message);
}

#[test]
fn nested_patterns_destructure_sum_option_and_tuple_values() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type Detail = Network {code int} | Validation text\ntype State = Failed {detail Detail, retry option (int, text)} | Done\ntype Job =\n  id int\n  state State\ntable jobs Job\ninsert jobs {id = 1, state = Failed {detail = Network {code = 503}, retry = Some ((30, \"network\"))}}\ninsert jobs {id = 2, state = Failed {detail = Validation \"email\", retry = None}}\ninsert jobs {id = 3, state = Done}",
    );
    let result = ok(
        &mut e,
        "from jobs\nderive retry_reason =\n  match state\n    Failed {detail = Detail.Network {code}, retry = Some (at, reason)} => reason\n    _ => \"none\"\nfilter match state\n  Failed {retry = Some (at, _), ..} => at >= 20\n  _ => false\nselect {id, retry_reason}",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(result.rows[0]["retry_reason"].cmp_eq(&Value::Text("network".into())));
}

#[test]
fn nested_patterns_are_typed_and_refutable_on_empty_tables() {
    let setup = "type Detail = Network {code int} | Validation text\ntype State = Failed {detail Detail, retry option (int, text)} | Done\ntype Job =\n  state State\ntable jobs Job";
    for (pattern, expected) in [
        (
            "Failed {detail = Network {code}, ..} => code\n    Done => 0",
            "non-exhaustive match; missing Failed",
        ),
        (
            "Failed {retry = Some (at, _, extra), ..} => at\n    _ => 0",
            "tuple pattern",
        ),
        (
            "Failed {detail = Network {code = value}, retry = Some (value, _)} => value\n    _ => 0",
            "binding 'value' is declared more than once",
        ),
        (
            "Failed {retry = Some value} => 1\n    _ => 0",
            "omits detail",
        ),
    ] {
        let mut engine = Engine::memory();
        ok(&mut engine, setup);
        let result = engine.execute(&format!(
            "from jobs\nderive value =\n  match state\n    {pattern}"
        ));
        assert!(!result.ok, "accepted {pattern}");
        assert!(
            result.message.contains(expected),
            "{}: {pattern}",
            result.message
        );
    }
}

#[test]
fn complementary_nested_patterns_are_exhaustive_and_reachable() {
    let setup = "type Change =\n  Added {path text}\n  | Modified {path text, before text, after text}\n  | Deleted {path text, before text}\ntype SyncState =\n  Clean {revision text}\n  | Conflict {local Change, remote Change}\ntype Workspace =\n  id text\n  state SyncState\ntable workspaces Workspace";
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        &format!(
            "{setup}\ninsert workspaces {{id = \"a\", state = Conflict {{local = Added {{path = \"a.txt\"}}, remote = Deleted {{path = \"a.txt\", before = \"old\"}}}}}}\ninsert workspaces {{id = \"b\", state = Conflict {{local = Modified {{path = \"b.txt\", before = \"old\", after = \"new\"}}, remote = Added {{path = \"b.txt\"}}}}}}\ninsert workspaces {{id = \"c\", state = Conflict {{local = Deleted {{path = \"c.txt\", before = \"old\"}}, remote = Added {{path = \"c.txt\"}}}}}}\ninsert workspaces {{id = \"d\", state = Clean {{revision = \"r1\"}}}}"
        ),
    );
    let result = ok(
        &mut engine,
        "from workspaces\nderive local_kind =\n  match state\n    Conflict {local = Added {path}, ..} => \"added\"\n    Conflict {local = Modified {path, ..}, ..} => \"modified\"\n    Conflict {local = Deleted {path, ..}, ..} => \"deleted\"\n    Clean {..} => \"clean\"\nsort id\nselect {id, local_kind}",
    );
    for (row, expected) in result
        .rows
        .iter()
        .zip(["added", "modified", "deleted", "clean"])
    {
        assert!(row["local_kind"].cmp_eq(&Value::Text(expected.into())));
    }

    let mut empty = Engine::memory();
    ok(&mut empty, setup);
    let missing = empty.execute(
        "from workspaces\nderive local_kind =\n  match state\n    Conflict {local = Added {..}, ..} => \"added\"\n    Conflict {local = Modified {..}, ..} => \"modified\"\n    Clean {..} => \"clean\"",
    );
    assert!(!missing.ok);
    assert_eq!(missing.error.as_ref().unwrap().code, "E_MATCH");
    assert!(
        missing.message.contains("missing Conflict"),
        "{}",
        missing.message
    );
    assert!(missing.message.contains("Deleted"), "{}", missing.message);

    let unreachable = empty.execute(
        "from workspaces\nderive local_kind =\n  match state\n    Conflict {local, ..} => \"conflict\"\n    Conflict {local = Added {..}, ..} => \"added\"\n    Clean {..} => \"clean\"",
    );
    assert!(!unreachable.ok);
    assert_eq!(unreachable.error.as_ref().unwrap().code, "E_MATCH");
    assert!(
        unreachable.message.contains("branch 2 is unreachable"),
        "{}",
        unreachable.message
    );
}

#[test]
fn nested_pattern_coverage_preserves_product_correlations() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type PairState = Pair((option int, option int)) | Empty\ntype Row =\n  state PairState\ntable rows Row",
    );
    let exhaustive = ok(
        &mut engine,
        "from rows\nderive category =\n  match state\n    Pair (Some _, _) => \"left\"\n    Pair (None, Some _) => \"right\"\n    Pair (None, None) => \"neither\"\n    Empty => \"empty\"",
    );
    assert!(exhaustive.rows.is_empty());

    let non_exhaustive = engine.execute(
        "from rows\nderive category =\n  match state\n    Pair (Some _, None) => \"left\"\n    Pair (None, Some _) => \"right\"\n    Pair (None, None) => \"neither\"\n    Empty => \"empty\"",
    );
    assert!(!non_exhaustive.ok);
    assert!(
        non_exhaustive.message.contains("Some"),
        "{}",
        non_exhaustive.message
    );
}

#[test]
fn derive_match_constructs_named_adt_values_from_bindings() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Failed {message text, retry_at option int} | Done\ntype Summary =\n  label text\n  retry_at option int = None\ntype Display = Retrying(Summary) | Complete\ntype Job =\n  id int\n  state State\ntable jobs Job\ninsert jobs {id = 1, state = Failed {message = \"network\", retry_at = Some 30}}\ninsert jobs {id = 2, state = Done}",
    );
    let result = ok(
        &mut e,
        "from jobs\nderive retry_at =\n  match state\n    Failed {retry_at = Some at, ..} => Some at\n    _ => None\nderive summary =\n  match state\n    Failed {message, retry_at} => Summary {label = message, retry_at = retry_at}\n    Done => Summary {label = \"done\"}\nderive display =\n  match state\n    Failed {message, retry_at} => Display.Retrying (Summary {label = message, retry_at = retry_at})\n    Done => Complete\nfilter retry_at == Some 30\nfilter summary == {label = \"network\", retry_at = Some 30}\nfilter display == Display.Retrying({label = \"network\", retry_at = Some 30})\nselect {id, retry_at, summary, display}",
    );
    assert_eq!(result.rows.len(), 1);
    assert!(result.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.name.as_str(), column.ty.as_str()))
            .collect::<Vec<_>>(),
        [
            ("id", "int"),
            ("retry_at", "option int"),
            ("summary", "Summary"),
            ("display", "Display"),
        ]
    );
}

#[test]
fn derive_match_constructs_structural_product_and_list_values() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = Failed {message text, retry_at option int} | Done\ntype Job =\n  id int\n  state State\ntable jobs Job\ninsert jobs {id = 1, state = Failed {message = \"network\", retry_at = Some 30}}\ninsert jobs {id = 2, state = Done}",
    );
    let result = ok(
        &mut e,
        "from jobs\nderive detail =\n  match state\n    Failed {message, retry_at = Some at} => {label = message, attempts = [at], pair = (at, message)}\n    _ => {label = \"done\", attempts = [], pair = (0, \"done\")}\nsort id\nselect {id, detail}",
    );
    let Value::Record(first) = result.rows[0]["detail"].unwrapped() else {
        panic!("detail should be a record");
    };
    assert!(first["label"].cmp_eq(&Value::Text("network".into())));
    assert!(first["attempts"].cmp_eq(&Value::List(vec![Value::Int(30)])));
    assert!(first["pair"].cmp_eq(&Value::Tuple(vec![
        Value::Int(30),
        Value::Text("network".into())
    ])));
    let Value::Record(second) = result.rows[1]["detail"].unwrapped() else {
        panic!("detail should be a record");
    };
    assert!(second["attempts"].cmp_eq(&Value::List(Vec::new())));
}

#[test]
fn constructed_match_results_are_checked_on_empty_tables() {
    let setup = "type State = Failed {message text, retry_at option int} | Done\ntype Summary =\n  label text\n  retry_at option int = None\ntype Other =\n  label text\n  retry_at option int = None\ntype Display = Retrying(Summary) | Complete\ntype Job =\n  state State\ntable jobs Job";
    for (query, expected) in [
        (
            "Failed {retry_at, ..} => retry_at\n    Done => Some \"wrong\"",
            "expected int",
        ),
        (
            "Failed {message, ..} => Summary {retry_at = None}\n    Done => Summary {label = \"done\"}",
            "missing required field 'label'",
        ),
        (
            "Failed {message, ..} => Display.Retrying\n    Done => Display.Complete",
            "expects 1 argument(s), got 0",
        ),
        (
            "Failed {message, ..} => Some missing\n    Done => None",
            "unknown match binding 'missing'",
        ),
        ("Failed {..} => {}\n    Done => {}", "cannot infer type"),
        (
            "Failed {message, ..} => Summary {label = message}\n    Done => Other {label = \"done\"}",
            "expected Summary",
        ),
    ] {
        let mut engine = Engine::memory();
        ok(&mut engine, setup);
        let result = engine.execute(&format!(
            "from jobs\nderive value =\n  match state\n    {query}"
        ));
        assert!(!result.ok, "accepted {query}");
        assert!(
            result.message.contains(expected),
            "{}: {query}",
            result.message
        );
    }
}

#[test]
fn match_obeys_pipeline_scope_and_layout_boundaries() {
    let mut e = Engine::memory();
    ok(
        &mut e,
        "type State = A | B {active bool}\ntype R =\n  id int\n  state State\ntable rows R\ninsert rows {id = 1, state = B {active = true}}",
    );
    let query = "from rows | filter match state\n  A => false\n  B {active} => active\nselect {id}\ntake 1\nfrom rows\nfilter match state\n  A => false\n  _ => true";
    assert_eq!(ok(&mut e, query).rows.len(), 1);
    let result = e.execute("from rows\nselect {id}\nfilter match state\n  A => true\n  _ => false");
    assert!(!result.ok);
    assert!(result.message.contains("unknown field 'state'"));
}

#[test]
fn deeply_nested_index_keys_grow_with_the_value_size() {
    let mut a = Value::Float(-0.0);
    let mut b = Value::Float(0.0);
    for _ in 0..48 {
        a = Value::Record([("child".to_string(), a)].into());
        b = Value::Record([("child".to_string(), b)].into());
    }
    assert!(a.cmp_eq(&b));
    let key = a.index_key();
    assert_eq!(key, b.index_key());
    assert!(
        key.len() < 4096,
        "nested ADT keys must not grow exponentially"
    );
    assert_ne!(
        Value::Int(1).index_key(),
        Value::Text("1".into()).index_key()
    );
    assert_ne!(
        Value::List(vec![a]).index_key(),
        Value::Tuple(vec![b]).index_key()
    );
}

#[test]
fn update_and_delete_compose_with_typed_adt_filters_and_indexes() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type State = Pending | Running {attempt int} | Done {result text}\ntype Meta =\n  owner text\ntype Task =\n  id int\n  attempts int\n  state State\n  meta Meta\ntable tasks Task\n  key id\ncreate index tasks (meta.owner)\ninsert tasks {id = 1, attempts = 1, state = Pending, meta = {owner = \"alice\"}}\ninsert tasks {id = 2, attempts = 4, state = Running {attempt = 2}, meta = {owner = \"eve\"}}",
    );
    let updated = ok(
        &mut engine,
        "update tasks\nfilter match state\n  Pending => true\n  _ => false\nset state = Done {result = \"ok\"}\nset attempts = attempts + 1\nset meta.owner = \"bob\"",
    );
    assert_eq!(updated.affected_rows, Some(1));
    let rows = ok(
        &mut engine,
        "from tasks | filter meta.owner == \"bob\" | select {id, attempts, state}",
    );
    assert_eq!(rows.rows.len(), 1);
    assert!(rows.rows[0]["id"].cmp_eq(&Value::Int(1)));
    assert!(rows.rows[0]["attempts"].cmp_eq(&Value::Int(2)));

    let deleted = ok(&mut engine, "delete tasks | filter meta.owner == \"bob\"");
    assert_eq!(deleted.affected_rows, Some(1));
    let remaining = ok(&mut engine, "from tasks");
    assert_eq!(remaining.rows.len(), 1);
    assert!(remaining.rows[0]["id"].cmp_eq(&Value::Int(2)));
}

#[test]
fn update_assignments_are_simultaneous_and_batches_remain_atomic() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "create table pairs (id int, left int, right int)\ninsert pairs {id = 1, left = 10, right = 20}",
    );
    let updated = ok(
        &mut engine,
        "update pairs\nfilter id == 1\nset left = right\nset right = left",
    );
    assert_eq!(updated.affected_rows, Some(1));
    let swapped = ok(&mut engine, "from pairs");
    assert!(swapped.rows[0]["left"].cmp_eq(&Value::Int(20)));
    assert!(swapped.rows[0]["right"].cmp_eq(&Value::Int(10)));

    let failed = engine.execute("update pairs\nset left = left + 1\nfrom pairs | select {missing}");
    assert!(!failed.ok);
    assert!(ok(&mut engine, "from pairs").rows[0]["left"].cmp_eq(&Value::Int(20)));
}

#[test]
fn failed_multi_row_updates_preserve_rows_constraints_and_indexes() {
    let setup = "type Item =\n  id int\n  divisor int\n  value int\ntable items Item\n  key id\ninsert items {id = 1, divisor = 2, value = 5}\ninsert items {id = 2, divisor = 0, value = 6}";
    let mut engine = Engine::memory();
    ok(&mut engine, setup);

    let duplicate = engine.execute("update items\nset id = 1");
    assert!(!duplicate.ok);
    assert_eq!(duplicate.error.unwrap().code, "E_CONSTRAINT");
    assert_eq!(ok(&mut engine, "from items | filter id == 2").rows.len(), 1);

    let arithmetic = engine.execute("update items\nset value = 10 / divisor");
    assert!(!arithmetic.ok);
    assert_eq!(arithmetic.error.unwrap().code, "E_ARITH");
    let unchanged = ok(&mut engine, "from items | sort id");
    assert!(unchanged.rows[0]["value"].cmp_eq(&Value::Int(5)));
    assert!(unchanged.rows[1]["value"].cmp_eq(&Value::Int(6)));
}

#[test]
fn update_and_delete_validate_before_scanning_empty_tables() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Meta =\n  owner text\ntype Row =\n  id int\n  meta Meta\ntable rows Row",
    );
    for (source, expected) in [
        ("update rows\nset missing = 1", "unknown field 'missing'"),
        ("update rows\nset id = \"wrong\"", "expected int"),
        (
            "update rows\nset meta = {owner = \"a\"}\nset meta.owner = \"b\"",
            "overlap",
        ),
        (
            "update rows\nset id = 1\nfilter id == 1",
            "filters must appear before set",
        ),
        ("update rows\nfilter id == 1", "requires at least one set"),
        (
            "delete rows\nfilter missing == 1",
            "unknown field 'missing'",
        ),
    ] {
        let result = engine.execute(source);
        assert!(!result.ok, "accepted {source}");
        assert!(result.message.contains(expected), "{}", result.message);
    }
    let deleted = ok(&mut engine, "delete rows");
    assert_eq!(deleted.affected_rows, Some(0));
}

#[test]
fn upsert_inserts_then_replaces_complete_typed_rows() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "type Endpoint =\n  host text\n  port int = 80\ntype Config =\n  name text\n  endpoint Endpoint\n  mode text = \"development\"\n  owner option text = None\ntable configs Config\n  key name\ncreate index configs (endpoint.host)",
    );

    let inserted = ok(
        &mut engine,
        "upsert configs\n  name = \"worker\"\n  endpoint =\n    host = \"old\"\n  owner = Some \"alice\"",
    );
    assert_eq!(inserted.affected_rows, Some(1));
    assert_eq!(inserted.upsert_action, Some(UpsertAction::Inserted));
    assert_eq!(
        ok(
            &mut engine,
            "from configs | filter endpoint.host == \"old\""
        )
        .rows
        .len(),
        1
    );

    let updated = ok(
        &mut engine,
        "upsert configs {name = \"worker\", endpoint = {host = \"new\"}}",
    );
    assert_eq!(updated.affected_rows, Some(1));
    assert_eq!(updated.upsert_action, Some(UpsertAction::Updated));
    assert!(
        ok(
            &mut engine,
            "from configs | filter endpoint.host == \"old\""
        )
        .rows
        .is_empty()
    );
    let rows = ok(
        &mut engine,
        "from configs | filter endpoint.host == \"new\"",
    );
    assert_eq!(rows.rows.len(), 1);
    assert!(
        rows.rows[0]["owner"]
            .unwrapped()
            .cmp_eq(&Value::Option(None))
    );
    assert!(rows.rows[0]["mode"].cmp_eq(&Value::Text("development".into())));
    assert!(
        rows.rows[0]["endpoint"]
            .field("port")
            .unwrap()
            .cmp_eq(&Value::Int(80))
    );

    let repeated = ok(
        &mut engine,
        "upsert configs {name = \"worker\", endpoint = {host = \"new\"}}",
    );
    assert_eq!(repeated.upsert_action, Some(UpsertAction::Updated));
    assert_eq!(ok(&mut engine, "from configs").rows.len(), 1);
}

#[test]
fn upsert_requires_a_key_and_failed_batches_leave_no_partial_row() {
    let mut engine = Engine::memory();
    ok(&mut engine, "create table unkeyed (id int)");
    let unkeyed = engine.execute("upsert unkeyed {id = 1}");
    assert!(!unkeyed.ok);
    assert_eq!(unkeyed.error.unwrap().code, "E_CONSTRAINT");

    ok(
        &mut engine,
        "type Item =\n  id int\n  value text\ntable items Item\n  key id\ninsert items {id = 1, value = \"one\"}",
    );
    for source in [
        "upsert items {value = \"missing key\"}",
        "upsert items {id = 1, value = 2}",
        "upsert items 1",
    ] {
        let result = engine.execute(source);
        assert!(!result.ok, "accepted {source}");
        assert_eq!(ok(&mut engine, "from items").rows.len(), 1);
        assert!(ok(&mut engine, "from items").rows[0]["value"].cmp_eq(&Value::Text("one".into())));
    }

    let failed =
        engine.execute("upsert items {id = 2, value = \"two\"}\nfrom items | select {missing}");
    assert!(!failed.ok);
    assert!(
        ok(&mut engine, "from items | filter id == 2")
            .rows
            .is_empty()
    );
}
