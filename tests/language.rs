use unionid::{Engine, QueryResponse, Value};

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

    let mut events = Engine::memory();
    let r = ok(&mut events, include_str!("../examples/events.uid"));
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "event"]
    );
    assert!(r.rows[0]["id"].cmp_eq(&Value::Int(2)));
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
        "type Flag =\n  Enabled\ntype Pair =\n  Wrapped((int, text))\ntype R =\n  id int\n  flag Flag\n  pair Pair\n  note option text\ntable typed R\n  key id\ncreate table old (n int, kind enum(A, B(float)))",
    );
    let schema = original.schema();
    assert!(schema.contains("note option text"));
    let mut restored = Engine::memory();
    ok(&mut restored, &schema);
    assert_eq!(schema, restored.schema());
    ok(
        &mut restored,
        "insert typed {id = 1, flag = Enabled, pair = Wrapped((2, \"x\")), note = None}",
    );
    assert_eq!(ok(&mut restored, "from typed").rows.len(), 1);
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
            "filter match pair\n  Pair => true\n  Empty => false",
            "positional match patterns are not implemented yet",
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
            "requires a record pattern",
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
