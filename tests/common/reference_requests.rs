use serde::{Deserialize, Serialize};
use unionid::error::ConstraintKind;
use unionid::protocol::PRODUCTION_VERSION;
use unionid::{ProtocolRequest, ProtocolResponse};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum AccountId {
    Local(i64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    id: i64,
    account: Option<AccountId>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct Account {
    id: AccountId,
}

pub enum Expected {
    Success,
    Failure(ConstraintKind, usize),
    Sessions(Vec<Session>),
    Accounts(Vec<Account>),
    Plan(
        unionid::QueryOperation,
        Vec<unionid::QueryReferenceCheckKind>,
    ),
}

impl Expected {
    pub fn check(self, request: &ProtocolRequest, response: ProtocolResponse) {
        assert_eq!(response.request_id, request.request_id);
        assert_eq!(response.version, PRODUCTION_VERSION);
        match self {
            Self::Failure(kind, statement) => {
                assert!(!response.ok, "{}", request.request_id);
                let error = response.error.unwrap();
                assert_eq!(error.code, "E_CONSTRAINT");
                assert_eq!(error.constraint, Some(kind));
                assert_eq!(error.hint.as_deref(), kind.default_hint());
                assert_eq!(error.statement_index, Some(statement));
                assert!(error.span.is_some());
            }
            expected => {
                assert!(response.ok, "{}: {}", request.request_id, response.message);
                match expected {
                    Self::Plan(operation, checks) => {
                        assert!(response.rows.is_empty());
                        assert!(response.affected_rows.is_none());
                        let plan = response.mutation_plan.unwrap();
                        assert_eq!(plan.operation, operation);
                        assert_eq!(
                            plan.reference_checks
                                .iter()
                                .map(|check| check.kind)
                                .collect::<Vec<_>>(),
                            checks
                        );
                    }
                    Self::Sessions(rows) => {
                        assert_eq!(response.typed_rows::<Session>().unwrap(), rows)
                    }
                    Self::Accounts(rows) => {
                        assert_eq!(response.typed_rows::<Account>().unwrap(), rows)
                    }
                    _ => {}
                }
            }
        }
    }
}

pub fn steps() -> Vec<(ProtocolRequest, Expected)> {
    let request = |id: &str, query: &str| {
        ProtocolRequest::query(id, query)
            .with_version(PRODUCTION_VERSION)
            .unwrap()
    };
    let row = Session {
        id: 1,
        account: Some(AccountId::Local(7)),
    };
    let missing = Session {
        id: 1,
        account: Some(AccountId::Local(999)),
    };
    let insert = request("insert", "insert sessions $session\nreturning")
        .with_serde_param("session", &row)
        .unwrap()
        .with_idempotency_key("session-1")
        .unwrap();
    vec![
        (
            request(
                "setup",
                "enum AccountId {Local(int)}\nstruct Account {id: AccountId}\nstruct Session {id: int, account: Option<AccountId>}\ntable accounts: Account {}\ntable sessions: Session {key id}\ncreate unique index accounts (id)\ncreate reference sessions (account) references accounts (id)\ninsert accounts {id: Local(7)}",
            ),
            Expected::Success,
        ),
        (
            request(
                "plan-missing",
                "explain insert sessions $session\nreturning",
            )
            .with_serde_param("session", &missing)
            .unwrap(),
            Expected::Plan(
                unionid::QueryOperation::Insert,
                vec![unionid::QueryReferenceCheckKind::TargetExists],
            ),
        ),
        (
            request("missing", "insert sessions $session\nreturning")
                .with_serde_param("session", &missing)
                .unwrap()
                .with_idempotency_key("session-1")
                .unwrap(),
            Expected::Failure(ConstraintKind::ReferenceMissing, 1),
        ),
        (insert.clone(), Expected::Sessions(vec![row.clone()])),
        (
            request("plan-restrict", "explain delete accounts"),
            Expected::Plan(
                unionid::QueryOperation::Delete,
                vec![unionid::QueryReferenceCheckKind::Restrict],
            ),
        ),
        (
            request("restricted", "delete accounts"),
            Expected::Failure(ConstraintKind::ReferenceRestricted, 1),
        ),
        (
            request(
                "rollback",
                "insert accounts {id: Local(8)}\ninsert sessions {id: 3, account: Some(Local(999))}",
            ),
            Expected::Failure(ConstraintKind::ReferenceMissing, 2),
        ),
        (
            request("accounts-after-rollback", "from accounts | sort id"),
            Expected::Accounts(vec![Account {
                id: AccountId::Local(7),
            }]),
        ),
        (
            ProtocolRequest {
                request_id: "replay".into(),
                ..insert
            },
            Expected::Sessions(vec![row.clone()]),
        ),
        (
            request(
                "unassigned",
                "insert sessions {id: 2, account: None}\nreturning",
            ),
            Expected::Sessions(vec![Session {
                id: 2,
                account: None,
            }]),
        ),
        (
            request(
                "unlink",
                "update sessions | filter id == 1 | set account = None | returning",
            ),
            Expected::Sessions(vec![Session {
                id: 1,
                account: None,
            }]),
        ),
        (
            request("remove", "delete accounts\nreturning"),
            Expected::Accounts(vec![Account {
                id: AccountId::Local(7),
            }]),
        ),
        (
            request("final-sessions", "from sessions | sort id"),
            Expected::Sessions(vec![
                Session {
                    id: 1,
                    account: None,
                },
                Session {
                    id: 2,
                    account: None,
                },
            ]),
        ),
        (
            request("final-accounts", "from accounts"),
            Expected::Accounts(vec![]),
        ),
    ]
}
