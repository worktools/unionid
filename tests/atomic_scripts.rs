mod common;

use std::collections::BTreeMap;

use common::TempDir;
use unionid::script::{MAX_SCRIPT_STATEMENTS, StatementKind};
use unionid::{Engine, QueryResponse, Value, format_source, input_status};

const SETUP: &str = "struct Account {id: int, balance: int, version: int}\ntable accounts: Account {key id}\ncreate index accounts (balance)\ninsert many accounts [{id: 1, balance: 100, version: 0}, {id: 2, balance: 0, version: 0}]";
const TRANSFER: &str = "update accounts\nfilter id == $sender && balance >= $amount\nset balance = balance - $amount\nexpect affected == 1\nupdate accounts\nfilter id == $recipient\nset balance = balance + $amount\nexpect affected == 1";

fn ok(engine: &mut Engine, source: &str) -> QueryResponse {
    let response = engine.execute(source);
    assert!(response.ok, "{}", response.message);
    response
}

fn params(sender: i64, recipient: i64, amount: i64) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("sender".into(), Value::Int(sender)),
        ("recipient".into(), Value::Int(recipient)),
        ("amount".into(), Value::Int(amount)),
    ])
}

fn rows(engine: &mut Engine) -> serde_json::Value {
    serde_json::to_value(ok(engine, "from accounts | sort id").rows).unwrap()
}

#[test]
fn transfers_guard_each_mutation_and_return_ordered_counts_in_memory_and_redb() {
    let temp = TempDir::new();
    for mut engine in [
        Engine::memory(),
        Engine::open_redb(temp.0.join("accounts.redb")).unwrap(),
    ] {
        ok(&mut engine, SETUP);
        let before = rows(&mut engine);
        let sequence = engine.backup_journal_status().ok().map(|s| s.head_sequence);
        let prepared = engine.prepare(TRANSFER).unwrap();
        for (sender, recipient, amount, index) in [(2, 1, 50, 2), (1, 99, 50, 4)] {
            let failed = engine.execute_prepared(&prepared, params(sender, recipient, amount));
            assert!(!failed.ok);
            let error = failed.error.unwrap();
            assert_eq!(error.code, "E_EXPECTATION");
            assert_eq!(error.statement_index, Some(index));
            assert!(error.span.is_some());
            assert!(failed.rows.is_empty() && failed.statements.is_empty());
            assert_eq!(rows(&mut engine), before);
            assert_eq!(
                engine.backup_journal_status().ok().map(|s| s.head_sequence),
                sequence
            );
            assert_eq!(
                ok(&mut engine, "from accounts | filter balance == 100")
                    .rows
                    .len(),
                1
            );
        }
        let success = engine.execute_prepared(&prepared, params(1, 2, 50));
        assert!(success.ok, "{}", success.message);
        assert_eq!(success.affected_rows, Some(1));
        assert_eq!(success.statements.len(), 4);
        assert_eq!(success.statements[0].kind, StatementKind::Update);
        assert_eq!(success.statements[0].affected_rows, Some(1));
        assert_eq!(success.statements[1].kind, StatementKind::Expect);
        assert_eq!(success.statements[1].affected_rows, None);
        assert_eq!(success.statements[3].index, 4);
        assert_eq!(
            ok(&mut engine, "from accounts | filter balance == 50")
                .rows
                .len(),
            2
        );
        if sequence.is_some() {
            engine.check_integrity().unwrap();
        }
    }
    let mut reopened = Engine::open_redb(temp.0.join("accounts.redb")).unwrap();
    assert_eq!(
        ok(&mut reopened, "from accounts | filter balance == 50")
            .rows
            .len(),
        2
    );
    reopened.check_integrity().unwrap();
}

#[test]
fn returning_and_final_query_keep_the_existing_output_contract() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let returning = ok(
        &mut engine,
        "update accounts | filter id == 1 | set balance = 90 | returning {id, balance}\nexpect affected == 1",
    );
    assert!(matches!(returning.rows[0]["balance"], Value::Int(90)));
    assert_eq!(returning.affected_rows, Some(1));
    let final_query = ok(
        &mut engine,
        "update accounts | filter id == 1 | set balance = 80\nexpect affected == 1\nfrom accounts | filter id == 1 | select {balance}",
    );
    assert!(matches!(final_query.rows[0]["balance"], Value::Int(80)));
    assert_eq!(final_query.affected_rows, None);
    assert_eq!(
        final_query.statements.last().unwrap().kind,
        StatementKind::Query
    );
    let unguarded = ok(&mut engine, "delete accounts | filter id == 99");
    assert_eq!(unguarded.affected_rows, Some(0));
}

#[test]
fn single_statement_outputs_and_receipts_omit_redundant_summaries() {
    use unionid::{ProtocolRequest, backup};
    let temp = TempDir::new();
    let db = temp.0.join("single.redb");
    let archive = temp.0.join("single.json");
    let restored = temp.0.join("single-restored.redb");
    let source =
        "update accounts | filter id == $id | set version = version + 1 | returning {id, version}";
    let bound = BTreeMap::from([("id".into(), Value::Int(1))]);
    let digest = ProtocolRequest::query("single", source)
        .with_serde_param("id", &1_i64)
        .unwrap()
        .canonical_digest()
        .unwrap();
    let mut saved = None;
    for mut engine in [Engine::memory(), Engine::open_redb(&db).unwrap()] {
        ok(&mut engine, SETUP);
        let query = ok(
            &mut engine,
            "from accounts | filter id == 1 | select {balance}",
        );
        assert!(matches!(query.rows[0]["balance"], Value::Int(100)));
        assert!(query.statements.is_empty());
        assert!(
            serde_json::to_value(&query)
                .unwrap()
                .get("statements")
                .is_none()
        );
        let prepared = engine.prepare(source).unwrap();
        let response = engine.execute_prepared(&prepared, bound.clone());
        assert!(response.ok, "{}", response.message);
        assert_eq!(response.affected_rows, Some(1));
        assert!(matches!(response.rows[0]["version"], Value::Int(1)));
        assert!(response.statements.is_empty());
        let first = engine
            .execute_idempotent_with_params("single", &digest, source, bound.clone(), None)
            .unwrap();
        assert!(!first.replayed);
        assert!(matches!(first.response.rows[0]["version"], Value::Int(2)));
        let expected = serde_json::to_value(&first.response).unwrap();
        assert!(expected.get("statements").is_none());
        let replay = engine
            .execute_idempotent_with_params("single", &digest, source, bound.clone(), None)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(serde_json::to_value(replay.response).unwrap(), expected);
        let failure = engine.execute("update accounts | set balance = balance / 0");
        assert_eq!(failure.error.unwrap().statement_index, Some(1));
        saved = Some(expected);
    }
    backup::create(&db, &archive).unwrap();
    backup::restore(&archive, &restored).unwrap();
    for path in [&db, &restored] {
        let mut engine = Engine::open_redb(path).unwrap();
        engine.check_integrity().unwrap();
        let replay = engine
            .execute_idempotent_with_params("single", &digest, source, bound.clone(), None)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            serde_json::to_value(replay.response).unwrap(),
            *saved.as_ref().unwrap()
        );
        assert!(matches!(
            ok(&mut engine, "from accounts | filter id == 1").rows[0]["version"],
            Value::Int(2)
        ));
    }
}

#[test]
fn context_and_script_limits_fail_before_any_candidate_effect() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let before = rows(&mut engine);
    for source in [
        "expect affected == 1",
        "update accounts | set balance = 0\nfrom accounts\nexpect affected == 2",
        "update accounts | set balance = 0\nexpect affected == 2\nexpect affected == 2",
    ] {
        let response = engine.execute(source);
        assert_eq!(response.error.unwrap().code, "E_EXPECTATION_CONTEXT");
        assert_eq!(rows(&mut engine), before);
        assert!(engine.prepare(source).is_err());
    }
    let source = format!(
        "update accounts | set balance = 0\n{}",
        "from accounts\n".repeat(MAX_SCRIPT_STATEMENTS)
    );
    assert_eq!(engine.execute(&source).error.unwrap().code, "E_LIMIT");
    assert_eq!(rows(&mut engine), before);
    assert!(engine.prepare(&source).is_err());
}

#[test]
fn guard_syntax_is_bounded_and_formatting_and_continuation_agree() {
    for op in ["==", "!=", "<", "<=", ">", ">="] {
        let source = format!("delete accounts | filter id == 0\nexpect affected {op} 1");
        let formatted = format_source(&source).unwrap();
        assert_eq!(format_source(&formatted).unwrap(), formatted);
        assert!(matches!(
            input_status(&formatted),
            unionid::InputStatus::Complete
        ));
    }
    assert!(matches!(
        input_status("expect affected =="),
        unionid::InputStatus::Incomplete(_)
    ));
    for source in [
        "expect affected == -1",
        "expect affected == 1.5",
        "expect affected == $count",
        "expect rows == 1",
        "expect affected = 1",
        "expect affected == 18446744073709551616",
    ] {
        assert!(unionid::syntax::parse(source).is_err(), "{source}");
    }
}

#[test]
fn guarded_static_operations_include_the_guard_in_the_digest() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let first = engine
        .describe_query("delete accounts | filter id == $id\nexpect affected == 1")
        .unwrap();
    let second = engine
        .describe_query("delete accounts | filter id == $id\nexpect affected >= 1")
        .unwrap();
    assert_ne!(first.query_digest, second.query_digest);
    assert!(engine.describe_query(TRANSFER).is_err());
}

#[test]
fn later_errors_and_batch_guards_roll_back_schema_indexes_and_row_ids() {
    let temp = TempDir::new();
    let db = temp.0.join("rollback.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    ok(&mut engine, SETUP);
    let before = rows(&mut engine);
    let schema = engine.introspection().schema;
    let response =
        engine.execute("insert accounts {id: 3, balance: 20, version: 0}\nexpect affected == 2");
    assert_eq!(response.error.unwrap().statement_index, Some(2));
    let response = engine.execute("update accounts | filter id == 1 | set balance = 0\nexpect affected == 1\nupdate accounts | set balance = balance / 0");
    let error = response.error.unwrap();
    assert_eq!(error.code, "E_ARITH");
    assert_eq!(error.statement_index, Some(3));
    assert_eq!(rows(&mut engine), before);
    assert_eq!(engine.introspection().schema, schema);
    let response = ok(
        &mut engine,
        "insert many accounts [{id: 3, balance: 20, version: 0}, {id: 4, balance: 20, version: 0}]\nexpect affected == 2",
    );
    assert_eq!(response.affected_rows, Some(2));
    engine.check_integrity().unwrap();
}

#[test]
fn durable_receipts_replay_the_exact_summaries_after_restart_and_backup() {
    use unionid::{ProtocolRequest, backup};
    let temp = TempDir::new();
    let db = temp.0.join("receipts.redb");
    let archive = temp.0.join("backup.json");
    let restored = temp.0.join("restored.redb");
    let request = ProtocolRequest::query("transfer", TRANSFER)
        .with_serde_param("sender", &1_i64)
        .unwrap()
        .with_serde_param("recipient", &2_i64)
        .unwrap()
        .with_serde_param("amount", &50_i64)
        .unwrap();
    let digest = request.canonical_digest().unwrap();
    let response;
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        ok(&mut engine, SETUP);
        // A guard failure must leave the key available for a corrected retry.
        let error = engine
            .execute_idempotent_with_params("transfer", &digest, TRANSFER, params(2, 1, 50), None)
            .unwrap_err();
        assert_eq!(error.code, "E_EXPECTATION");
        let success = engine
            .execute_idempotent_with_params("transfer", &digest, TRANSFER, params(1, 2, 50), None)
            .unwrap();
        response = serde_json::to_value(success.response).unwrap();
    }
    backup::create(&db, &archive).unwrap();
    backup::restore(&archive, &restored).unwrap();
    for path in [&db, &restored] {
        let mut engine = Engine::open_redb(path).unwrap();
        let replay = engine
            .execute_idempotent_with_params("transfer", &digest, TRANSFER, params(1, 2, 50), None)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(serde_json::to_value(replay.response).unwrap(), response);
        let changed = ProtocolRequest::query("transfer", TRANSFER.replace("== 1", ">= 1"))
            .with_serde_param("sender", &1_i64)
            .unwrap()
            .with_serde_param("recipient", &2_i64)
            .unwrap()
            .with_serde_param("amount", &50_i64)
            .unwrap()
            .canonical_digest()
            .unwrap();
        assert_eq!(
            engine
                .execute_idempotent_with_params(
                    "transfer",
                    &changed,
                    TRANSFER,
                    params(1, 2, 50),
                    None
                )
                .unwrap_err()
                .code,
            "E_IDEMPOTENCY_CONFLICT"
        );
        engine.check_integrity().unwrap();
    }
}

#[test]
fn legacy_responses_and_errors_round_trip_without_new_empty_fields() {
    let response = QueryResponse::ok_message("ok");
    let json = serde_json::to_value(&response).unwrap();
    assert!(json.get("statements").is_none());
    let decoded: QueryResponse = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), json);
    let error = unionid::Error::new("E_TABLE", "missing table");
    let json = serde_json::to_value(&error).unwrap();
    assert!(json.get("statement_index").is_none());
    assert_eq!(
        serde_json::from_value::<unionid::Error>(json).unwrap(),
        error
    );
}

#[test]
fn cli_and_tcp_share_guard_failures_and_success_summaries() {
    use unionid::{ProtocolRequest, cli};
    let failed_source = format!("{SETUP}\n{TRANSFER}");
    // Use literals here so both local CLI and protocol exercise real parsing.
    let failed_source = failed_source
        .replace("$sender", "2")
        .replace("$recipient", "1")
        .replace("$amount", "50");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_unionid"))
        .args(["run", "--query", &failed_source, "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let response: QueryResponse = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.error.unwrap().statement_index, Some(6));
    assert!(response.statements.is_empty());
    let server = common::Server::start(&[]);
    assert!(cli::send_one(&server.addr, SETUP).unwrap().ok);
    for version in [
        unionid::protocol::VERSION,
        unionid::protocol::PRODUCTION_VERSION,
    ] {
        let mut single = ProtocolRequest::query(
            "single-query",
            "from accounts | filter id == 1 | select {balance}",
        );
        single.version = version;
        let response = cli::send_request(&server.addr, &single).unwrap();
        assert!(response.ok);
        assert_eq!(response.rows.len(), 1);
        assert!(response.statements.is_empty());
        assert!(
            serde_json::to_value(response)
                .unwrap()
                .get("statements")
                .is_none()
        );
        let query = TRANSFER
            .replace("$sender", "2")
            .replace("$recipient", "1")
            .replace("$amount", "50");
        let mut request = ProtocolRequest::query("failed-transfer", query);
        request.version = version;
        let failed = cli::send_request(&server.addr, &request).unwrap();
        assert_eq!(failed.error.unwrap().code, "E_EXPECTATION");
        assert!(failed.statements.is_empty());
    }
    let query = TRANSFER
        .replace("$sender", "1")
        .replace("$recipient", "2")
        .replace("$amount", "50");
    let response =
        cli::send_request(&server.addr, &ProtocolRequest::query("transfer", query)).unwrap();
    assert!(response.ok, "{}", response.message);
    assert_eq!(response.statements.len(), 4);
    assert_eq!(
        cli::send_one(&server.addr, "from accounts | filter balance == 50")
            .unwrap()
            .rows
            .len(),
        2
    );
}

#[test]
fn competing_version_claims_have_exactly_one_winner() {
    use std::sync::{Arc, Barrier};
    use unionid::ConcurrentEngine;
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    let shared = ConcurrentEngine::new(engine);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = (0..2).map(|_| {
        let shared = shared.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            shared.execute("update accounts | filter id == 1 && version == 0 | set version = version + 1\nexpect affected == 1")
        })
    }).collect();
    barrier.wait();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.ok).count(), 1);
    let error = results.into_iter().find(|r| !r.ok).unwrap().error.unwrap();
    assert_eq!(error.code, "E_EXPECTATION");
    assert_eq!(error.statement_index, Some(2));
    assert_eq!(
        shared
            .execute("from accounts | filter id == 1 && version == 1")
            .rows
            .len(),
        1
    );
}

#[test]
fn journal_recovery_preserves_guarded_effects_and_receipt_summaries() {
    use unionid::backup::incremental::{self, ArchiveLimits};
    let temp = TempDir::new();
    let db = temp.0.join("journal.redb");
    let repository = temp.0.join("archive");
    let restored = temp.0.join("restored.redb");
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        ok(&mut engine, SETUP);
    }
    incremental::init(&db, &repository, Default::default()).unwrap();
    let digest = unionid::ProtocolRequest::query(
        "claim",
        "update accounts | filter id == 1 | set version = 1\nexpect affected == 1",
    )
    .canonical_digest()
    .unwrap();
    let response;
    let sequence;
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        let executed = engine
            .execute_idempotent_with_params(
                "claim",
                &digest,
                "update accounts | filter id == 1 | set version = 1\nexpect affected == 1",
                BTreeMap::new(),
                None,
            )
            .unwrap();
        sequence = executed.committed_sequence;
        response = serde_json::to_value(executed.response).unwrap();
        let failed = engine.execute("delete accounts | filter id == 1\nexpect affected == 2");
        assert_eq!(failed.error.unwrap().code, "E_EXPECTATION");
        assert_eq!(
            engine.backup_journal_status().unwrap().head_sequence,
            sequence
        );
    }
    incremental::export(&db, &repository, Default::default()).unwrap();
    incremental::restore(&repository, &restored, sequence, ArchiveLimits::default()).unwrap();
    let mut engine = Engine::open_redb(&restored).unwrap();
    engine.check_integrity().unwrap();
    let replay = engine
        .execute_idempotent_with_params(
            "claim",
            &digest,
            "not parsed on replay",
            BTreeMap::new(),
            None,
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(serde_json::to_value(replay.response).unwrap(), response);
    assert_eq!(
        ok(
            &mut engine,
            "from accounts | filter id == 1 && version == 1"
        )
        .rows
        .len(),
        1
    );
}

#[test]
fn failed_candidate_preserves_the_complete_durable_logical_state() {
    use unionid::backup;
    let temp = TempDir::new();
    let db = temp.0.join("identity.redb");
    let before = temp.0.join("before.json");
    let after = temp.0.join("after.json");
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        ok(&mut engine, SETUP);
    }
    backup::create(&db, &before).unwrap();
    {
        let mut engine = Engine::open_redb(&db).unwrap();
        let response = engine.execute("struct Extra {id: int}\ntable extras: Extra {key id}\ninsert accounts {id: 3, balance: 20, version: 0}\nexpect affected == 2");
        assert_eq!(response.error.unwrap().statement_index, Some(4));
        engine.check_integrity().unwrap();
    }
    backup::create(&db, &after).unwrap();
    let load =
        |path| serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap();
    let before = load(before);
    let after = load(after);
    assert_eq!(before["database"], after["database"]);
    assert_eq!(before["receipts"], after["receipts"]);
}

#[test]
fn all_guard_comparisons_use_actual_counts_and_preserve_upsert_actions() {
    let mut engine = Engine::memory();
    ok(&mut engine, SETUP);
    for (op, expected, success) in [
        ("==", 0, true),
        ("!=", 0, false),
        ("<", 1, true),
        ("<=", 0, true),
        (">", 0, false),
        (">=", 0, true),
    ] {
        let response = engine.execute(&format!(
            "delete accounts | filter id == 99\nexpect affected {op} {expected}"
        ));
        assert_eq!(response.ok, success, "{op}");
    }
    let response = ok(
        &mut engine,
        "upsert accounts {id: 1, balance: 100, version: 0}\n# Comments and blank lines preserve adjacency.\n\nexpect affected == 1",
    );
    assert_eq!(response.upsert_action, Some(unionid::UpsertAction::Updated));
    assert_eq!(response.affected_rows, Some(1));
}

#[test]
fn guarded_receipt_capacity_failure_is_precommit_and_does_not_occupy_the_key() {
    let temp = TempDir::new();
    let db = temp.0.join("budget.redb");
    let mut engine = Engine::open_redb(&db).unwrap();
    ok(
        &mut engine,
        "struct Item {id: int, note: text}\ntable items: Item {key id}\ninsert items {id: 1, note: \"original\"}",
    );
    let sequence = engine.backup_journal_status().unwrap().head_sequence;
    let source =
        "update items | filter id == 1 | set note = $note | returning\nexpect affected == 1";
    let note = "x".repeat(unionid::idempotency::MAX_IDEMPOTENCY_RECEIPT_BYTES + 100);
    let digest = unionid::ProtocolRequest::query("budget", source)
        .with_serde_param("note", &note)
        .unwrap()
        .canonical_digest()
        .unwrap();
    let error = engine
        .execute_idempotent_with_params(
            "budget",
            &digest,
            source,
            BTreeMap::from([("note".into(), Value::Text(note))]),
            None,
        )
        .unwrap_err();
    assert_eq!(error.code, "E_IDEMPOTENCY_LIMIT");
    assert_eq!(
        engine.backup_journal_status().unwrap().head_sequence,
        sequence
    );
    assert_eq!(
        ok(&mut engine, "from items | filter note == \"original\"")
            .rows
            .len(),
        1
    );
    let digest = unionid::ProtocolRequest::query("budget", source)
        .with_serde_param("note", &"saved")
        .unwrap()
        .canonical_digest()
        .unwrap();
    let success = engine
        .execute_idempotent_with_params(
            "budget",
            &digest,
            source,
            BTreeMap::from([("note".into(), Value::Text("saved".into()))]),
            None,
        )
        .unwrap();
    assert!(!success.replayed);
    assert_eq!(success.response.statements.len(), 2);
    engine.check_integrity().unwrap();
}

#[test]
fn a_trailing_guard_does_not_bypass_read_only_or_v1_returning_boundaries() {
    let mut engine = Engine::memory();
    ok(
        &mut engine,
        "struct Native {id: int, token: date}\ntable natives: Native {key id}\ninsert natives {id: 1, token: @2026-10-01}",
    );
    let request = unionid::ProtocolRequest::query(
        "v1",
        "update natives | filter id == 1 | set id = 2 | returning\nexpect affected == 1",
    );
    let response = unionid::server::execute_protocol_request(&mut engine, request);
    assert_eq!(response.error.unwrap().code, "E_PROTOCOL_TYPE");
    assert_eq!(
        ok(&mut engine, "from natives | filter id == 1").rows.len(),
        1
    );
    let mut read_only = engine.with_read_only(true);
    let response = read_only.execute("delete natives | filter id == 1\nexpect affected == 1");
    assert_eq!(response.error.unwrap().code, "E_READ_ONLY");
    assert_eq!(
        ok(&mut read_only, "from natives | filter id == 1")
            .rows
            .len(),
        1
    );
}

#[test]
fn legacy_wal_and_snapshot_reject_guards_without_persisting_unreplayable_scripts() {
    let temp = TempDir::new();
    for snapshot_enabled in [false, true] {
        let wal = temp.0.join(format!("legacy-{snapshot_enabled}.wal"));
        let snapshot = snapshot_enabled.then(|| temp.0.join("legacy.snapshot"));
        let before_rows;
        {
            let mut engine = Engine::open(
                Some(wal.clone()),
                snapshot.clone(),
                usize::from(snapshot_enabled),
            )
            .unwrap();
            ok(&mut engine, SETUP);
            before_rows = rows(&mut engine);
            let before_wal = std::fs::read(&wal).unwrap();
            let before_snapshot = snapshot.as_ref().map(|p| std::fs::read(p).unwrap());
            let failed = engine.execute(
                "update accounts | filter id == 1 | set balance = 0\nexpect affected == 1",
            );
            let error = failed.error.unwrap();
            assert_eq!(error.code, "E_CONFIG");
            assert_eq!(error.statement_index, Some(2));
            assert!(error.hint.unwrap().contains("--db"));
            assert_eq!(rows(&mut engine), before_rows);
            assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
            assert_eq!(
                snapshot.as_ref().map(|p| std::fs::read(p).unwrap()),
                before_snapshot
            );
        }
        let mut reopened =
            Engine::open(Some(wal), snapshot, usize::from(snapshot_enabled)).unwrap();
        assert_eq!(rows(&mut reopened), before_rows);
    }
}

#[test]
fn a_lost_tcp_response_can_retry_the_guarded_receipt_without_repeating_effects() {
    use std::io::Write;
    use std::net::TcpStream;
    use unionid::{ProtocolRequest, cli};
    let temp = TempDir::new();
    let db = temp.0.join("delivery.redb");
    let server = common::Server::start(&["--db", db.to_str().unwrap()]);
    assert!(cli::send_one(&server.addr, SETUP).unwrap().ok);
    let request = ProtocolRequest::query("lost-response", "update accounts | filter id == 1 | set version = version + 1\nreturning {id, version}\nexpect affected == 1").with_idempotency_key("claim-delivery").unwrap();
    let mut connection = TcpStream::connect(&server.addr).unwrap();
    connection
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    connection.write_all(b"\n").unwrap();
    connection.flush().unwrap();
    // Wait for the committed effect on a separate connection, without reading
    // any of the mutation response, then deliberately discard that response.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if cli::send_one(
            &server.addr,
            "from accounts | filter id == 1 && version == 1",
        )
        .unwrap()
        .rows
        .len()
            == 1
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(connection);
    let replay = cli::send_request(&server.addr, &request).unwrap();
    assert!(replay.ok, "{}", replay.message);
    assert!(replay.idempotency.unwrap().replayed);
    assert_eq!(replay.statements.len(), 2);
    assert_eq!(replay.statements[0].affected_rows, Some(1));
    assert_eq!(
        cli::send_one(
            &server.addr,
            "from accounts | filter id == 1 && version == 1"
        )
        .unwrap()
        .rows
        .len(),
        1
    );
}

#[test]
fn the_new_request_limit_does_not_prevent_replaying_older_large_wal_records() {
    let temp = TempDir::new();
    let path = temp.0.join("old-large.wal");
    let legacy = unionid::wal::Wal::new(&path).unwrap();
    legacy.append(1, SETUP).unwrap();
    let old_source = "update accounts | filter id == 1 | set version = version + 1\n"
        .repeat(MAX_SCRIPT_STATEMENTS + 1);
    legacy.append(2, &old_source).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut engine = Engine::open(Some(path.clone()), None, 0).unwrap();
    assert_eq!(
        ok(
            &mut engine,
            &format!(
                "from accounts | filter id == 1 && version == {}",
                MAX_SCRIPT_STATEMENTS + 1
            )
        )
        .rows
        .len(),
        1
    );
    assert_eq!(engine.execute(&old_source).error.unwrap().code, "E_LIMIT");
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}
