//! A compact, machine-readable capability manifest for AI agents and tools.
//!
//! `unionid agent` prints this manifest so an agent can discover the stable
//! command surface, the error contract (`code`/`span`/`constraint`/`hint`), and
//! the documented error-code vocabulary without parsing human help text.

use serde::Serialize;

use crate::{ConstraintKind, Engine, StorageVersions};

pub const AGENT_MANIFEST_VERSION: u32 = 1;

/// Every error code the binary can emit, sorted. Guarded against drift by
/// `error_codes_cover_every_literal_in_source`.
pub const ERROR_CODES: &[&str] = &[
    "E_ARGUMENT",
    "E_ARITH",
    "E_BACKUP",
    "E_BACKUP_AFTER_HEAD",
    "E_BACKUP_ARCHIVE",
    "E_BACKUP_BEFORE_BASELINE",
    "E_BACKUP_CHAIN",
    "E_BACKUP_CHAIN_ACTIVE",
    "E_BACKUP_JOURNAL_FULL",
    "E_BUSY",
    "E_BYTES",
    "E_CANCELLED",
    "E_CELLS",
    "E_CHECKPOINT",
    "E_CODEC",
    "E_CODEC_KEY",
    "E_CODEC_VERSION",
    "E_CONFIG",
    "E_CONNECTION",
    "E_CONSTRAINT",
    "E_CONTRACT_SCHEMA",
    "E_CONTRACT_SCOPE",
    "E_CONTRACT_VERSION",
    "E_CURSOR_CODEC",
    "E_CURSOR_DATABASE",
    "E_CURSOR_INTEGRITY",
    "E_CURSOR_LIMIT",
    "E_CURSOR_QUERY",
    "E_CURSOR_SCHEMA",
    "E_CURSOR_STALE",
    "E_DECIMAL_RANGE",
    "E_DECIMAL_ROUNDING",
    "E_DECIMAL_TYPE",
    "E_DUPLICATE_KEY",
    "E_EVALUATIONS",
    "E_FIELD",
    "E_FORMAT_VERSION",
    "E_HTTP_STATUS",
    "E_IDEMPOTENCY_CAPACITY",
    "E_IDEMPOTENCY_CONFLICT",
    "E_IDEMPOTENCY_DIGEST",
    "E_IDEMPOTENCY_KEY",
    "E_IDEMPOTENCY_LIMIT",
    "E_IDEMPOTENCY_NOT_MUTATION",
    "E_IDEMPOTENCY_PRUNE",
    "E_INCOMPLETE",
    "E_INDEX",
    "E_INDEX_DUPLICATE",
    "E_INDEX_KEY_LIMIT",
    "E_INDEX_PREDICATE",
    "E_INDEX_PREDICATE_CONTRADICTION",
    "E_INDEX_SHAPE",
    "E_INPUT",
    "E_INTERNAL",
    "E_IO",
    "E_KEY",
    "E_LIMIT",
    "E_MAINTENANCE",
    "E_MAINTENANCE_CONFLICT",
    "E_MAINTENANCE_IN_PROGRESS",
    "E_MAINTENANCE_LIMIT",
    "E_MAINTENANCE_REQUIRED",
    "E_MAP_LIMIT",
    "E_MATCH",
    "E_MIGRATION",
    "E_OBSERVER_CONFIG",
    "E_OPERATION_CAPACITY",
    "E_OPERATION_DROPPED",
    "E_OPERATION_ID",
    "E_OPERATION_UNKNOWN",
    "E_OUTPUTS",
    "E_PAGE_ORDER",
    "E_PAGE_SHAPE",
    "E_PARAM_EXTRA",
    "E_PARAM_MISSING",
    "E_PARAM_TYPE",
    "E_PREPARE",
    "E_PROTOCOL",
    "E_PROTOCOL_TYPE",
    "E_PROTOCOL_VERSION",
    "E_QUERY",
    "E_QUERY_BINDING",
    "E_QUERY_CARDINALITY",
    "E_QUERY_CONTRACT",
    "E_QUERY_FILE",
    "E_QUERY_RESPONSE",
    "E_READ_ONLY",
    "E_READ_SNAPSHOT",
    "E_RECEIPTS",
    "E_RELATION_KEY",
    "E_RELATION_LIMIT",
    "E_RELATION_NOT_UNIQUE",
    "E_REQUEST_TOO_LARGE",
    "E_RESPONSE_TOO_LARGE",
    "E_RUNTIME",
    "E_SCALAR_LITERAL",
    "E_SCHEMA",
    "E_SCHEMA_CHANGED",
    "E_SCHEMA_DIFF",
    "E_SERDE",
    "E_SHUTDOWN",
    "E_STEPS",
    "E_STORAGE",
    "E_STORAGE_REOPEN_REQUIRED",
    "E_STORAGE_UPGRADE",
    "E_STORAGE_UPGRADE_REQUIRED",
    "E_STREAM_LIMIT",
    "E_STREAM_SHAPE",
    "E_STREAM_VERSION",
    "E_SYNTAX",
    "E_TABLE",
    "E_TIME",
    "E_TIMEOUT",
    "E_TYPE",
];

#[derive(Debug, Clone, Serialize)]
pub struct AgentManifest {
    pub schema_version: u32,
    pub software_version: &'static str,
    pub protocol_versions: [u32; 2],
    pub stream_protocol_versions: [u32; 1],
    pub current_storage: StorageVersions,
    pub commands: Vec<AgentCommand>,
    pub workflow: Vec<&'static str>,
    pub error_contract: AgentErrorContract,
    pub docs: AgentDocs,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AgentCommand {
    pub name: &'static str,
    pub summary: &'static str,
    pub usage: &'static str,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentErrorContract {
    pub fields: Vec<&'static str>,
    pub codes: &'static [&'static str],
    pub constraint_kinds: Vec<AgentConstraintKind>,
    pub hint_codes: Vec<&'static str>,
    pub exit_classes: Vec<AgentExitClass>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AgentConstraintKind {
    pub kind: &'static str,
    pub hint: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AgentExitClass {
    pub code: u8,
    pub class: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AgentDocs {
    pub catalog: &'static str,
    pub query_reference: &'static str,
    pub schema: &'static str,
}

const COMMANDS: &[AgentCommand] = &[
    AgentCommand {
        name: "run",
        summary: "Execute one atomic script in memory or a local redb database.",
        usage: "unionid run --db <path> --query <source> --format json [--read-only]",
        json: true,
    },
    AgentCommand {
        name: "cli",
        summary: "Run one script locally or against a server, or open a REPL.",
        usage: "unionid cli --memory --query <source> --format json   # or --db <path> / --addr <host:port>",
        json: true,
    },
    AgentCommand {
        name: "schema",
        summary: "Validate, print, or generate Rust bindings from a schema.",
        usage: "unionid schema print --db <path> --format json   # or schema check --file <schema.unid>",
        json: true,
    },
    AgentCommand {
        name: "query",
        summary: "Bind and describe a saved query file without executing it.",
        usage: "unionid query describe --db <path> --file <query.unid>",
        json: true,
    },
    AgentCommand {
        name: "migration",
        summary: "Create, plan, apply, resume, inspect, and abort migrations.",
        usage: "unionid migration plan --db <path> --dir <dir> --format json   # also apply, status, advance, abort",
        json: true,
    },
    AgentCommand {
        name: "project",
        summary: "Check a source project (schema, migrations, queries) without a database.",
        usage: "unionid project check --dir <dir> --format json",
        json: true,
    },
    AgentCommand {
        name: "fmt",
        summary: "Canonicalize generated source, or check it with --check.",
        usage: "unionid fmt --file <source.unid> [--check]",
        json: false,
    },
    AgentCommand {
        name: "check",
        summary: "Verify redb and logical storage integrity.",
        usage: "unionid check --db <path> --format json",
        json: true,
    },
    AgentCommand {
        name: "backup",
        summary: "Create a verified logical backup from a redb database.",
        usage: "unionid backup --db <path> --output <backup.json> --format json",
        json: true,
    },
    AgentCommand {
        name: "restore",
        summary: "Restore a verified backup to a new redb path.",
        usage: "unionid restore --backup <backup.json> --db <new-path> --format json",
        json: true,
    },
    AgentCommand {
        name: "docs",
        summary: "Read version-matched bundled documentation.",
        usage: "unionid docs list --format json   # or docs show <topic>, docs query",
        json: true,
    },
    AgentCommand {
        name: "version",
        summary: "Report software, protocol, storage, codec, and target versions.",
        usage: "unionid version --format json",
        json: true,
    },
    AgentCommand {
        name: "doctor",
        summary: "Diagnose the binary and optionally inspect a database copy.",
        usage: "unionid doctor [--db <path>] --format json",
        json: true,
    },
    AgentCommand {
        name: "init",
        summary: "Create a runnable starter project in a new or empty directory.",
        usage: "unionid init <dir>",
        json: false,
    },
    AgentCommand {
        name: "agent",
        summary: "Print this machine-readable capability manifest.",
        usage: "unionid agent --format json",
        json: true,
    },
];

const WORKFLOW: &[&str] = &[
    "Read the current schema: `unionid schema print --db app.redb --format json`.",
    "Generate source and canonicalize it: `unionid fmt --file query.unid --check`.",
    "Bind without executing: `unionid query describe --db app.redb --file query.unid --format json`.",
    "Inspect a read plan without rows: `unionid run --db app.redb --read-only --query \"explain ...\"`.",
    "Execute, or apply migrations: `unionid run ...` / `unionid migration apply ...`.",
    "On failure branch on `error.code` and `error.constraint`, then follow `error.hint`.",
];

const HINT_CODES: &[&str] = &["E_TABLE", "E_CONSTRAINT", "E_PAGE_ORDER"];

const EXIT_CLASSES: &[AgentExitClass] = &[
    AgentExitClass {
        code: 0,
        class: "success",
    },
    AgentExitClass {
        code: 1,
        class: "internal",
    },
    AgentExitClass {
        code: 2,
        class: "argument_or_config",
    },
    AgentExitClass {
        code: 3,
        class: "input_schema_or_query",
    },
    AgentExitClass {
        code: 4,
        class: "connection_or_busy",
    },
    AgentExitClass {
        code: 5,
        class: "storage_or_uncertain",
    },
    AgentExitClass {
        code: 6,
        class: "integrity",
    },
];

pub fn manifest() -> AgentManifest {
    AgentManifest {
        schema_version: AGENT_MANIFEST_VERSION,
        software_version: env!("CARGO_PKG_VERSION"),
        protocol_versions: [
            crate::protocol::VERSION,
            crate::protocol::PRODUCTION_VERSION,
        ],
        stream_protocol_versions: [crate::stream::VERSION],
        current_storage: Engine::current_storage_versions(),
        commands: COMMANDS.to_vec(),
        workflow: WORKFLOW.to_vec(),
        error_contract: AgentErrorContract {
            fields: vec!["code", "message", "span", "constraint", "hint"],
            codes: ERROR_CODES,
            constraint_kinds: [
                ConstraintKind::Unique,
                ConstraintKind::PartialUnique,
                ConstraintKind::PrimaryKey,
                ConstraintKind::PrimaryKeyMissing,
            ]
            .into_iter()
            .map(|kind| AgentConstraintKind {
                kind: match kind {
                    ConstraintKind::Unique => "unique",
                    ConstraintKind::PartialUnique => "partial_unique",
                    ConstraintKind::PrimaryKey => "primary_key",
                    ConstraintKind::PrimaryKeyMissing => "primary_key_missing",
                },
                hint: kind.default_hint(),
            })
            .collect(),
            hint_codes: HINT_CODES.to_vec(),
            exit_classes: EXIT_CLASSES.to_vec(),
        },
        docs: AgentDocs {
            catalog: "unionid docs list --format json",
            query_reference: "unionid docs query",
            schema: "unionid schema print --db <path> --format json",
        },
    }
}

pub fn render_markdown() -> String {
    let storage = Engine::current_storage_versions();
    let mut output = format!(
        "# unionid agent manifest\n\nschema_version: {}\nsoftware_version: {}\nprotocol_versions: {}, {}\nstream_protocol_versions: {}\nstorage_format: {}\ncodecs (catalog/value/index/migration/receipt/maintenance/journal/backup): {}/{}/{}/{}/{}/{}/{}/{}\n\n## Commands\n\n",
        AGENT_MANIFEST_VERSION,
        env!("CARGO_PKG_VERSION"),
        crate::protocol::VERSION,
        crate::protocol::PRODUCTION_VERSION,
        crate::stream::VERSION,
        storage.format,
        storage.catalog_codec,
        storage.value_codec,
        storage.index_key_codec,
        storage.migration_codec,
        storage.receipt_codec,
        storage.maintenance_codec,
        storage.journal_codec,
        storage.backup_codec,
    );
    for command in COMMANDS {
        output.push_str(&format!(
            "- `{}`{}: {} — `{}`\n",
            command.name,
            if command.json { " (json)" } else { "" },
            command.summary,
            command.usage
        ));
    }
    output.push_str(
        "\n## Error contract\n\nFields: `code`, `message`, `span`, `constraint`, `hint`.\n\n",
    );
    for kind in [
        ConstraintKind::Unique,
        ConstraintKind::PartialUnique,
        ConstraintKind::PrimaryKey,
        ConstraintKind::PrimaryKeyMissing,
    ] {
        let name = match kind {
            ConstraintKind::Unique => "unique",
            ConstraintKind::PartialUnique => "partial_unique",
            ConstraintKind::PrimaryKey => "primary_key",
            ConstraintKind::PrimaryKeyMissing => "primary_key_missing",
        };
        match kind.default_hint() {
            Some(hint) => output.push_str(&format!("- `{name}`: {hint}\n")),
            None => output.push_str(&format!("- `{name}`: no automatic hint\n")),
        }
    }
    output.push_str(&format!(
        "\nHint-bearing codes: {}.\n\nExit classes: {}.\n",
        HINT_CODES.join(", "),
        EXIT_CLASSES
            .iter()
            .map(|class| format!("{}={}", class.code, class.class))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    output.push_str(&format!(
        "\n## Workflow\n\n{}\n\n## Docs\n\n- catalog: `{}`\n- query reference: `{}`\n- schema: `{}`\n",
        WORKFLOW
            .iter()
            .map(|step| format!("- {step}"))
            .collect::<Vec<_>>()
            .join("\n"),
        "unionid docs list --format json",
        "unionid docs query",
        "unionid schema print --db <path> --format json",
    ));
    output.push_str(&format!(
        "\n## Error codes\n\n{} error codes: {}.\n",
        ERROR_CODES.len(),
        ERROR_CODES.join(", ")
    ));
    output
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::ERROR_CODES;

    #[test]
    fn error_codes_cover_every_literal_in_source() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = BTreeSet::new();
        collect_codes(&root, &mut found);
        let catalog = ERROR_CODES.iter().copied().collect::<BTreeSet<_>>();
        let missing = found
            .iter()
            .filter(|code| !catalog.contains(code.as_str()))
            .cloned()
            .collect::<Vec<String>>();
        assert!(
            missing.is_empty(),
            "error codes missing from AGENT ERROR_CODES: {missing:?}"
        );
    }

    fn collect_codes(path: &Path, found: &mut BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_codes(&path, found);
            } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
                let Ok(source) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let bytes = source.as_bytes();
                let mut index = 0;
                while index + 2 < bytes.len() {
                    if bytes[index] == b'"' && bytes[index + 1] == b'E' && bytes[index + 2] == b'_'
                    {
                        let mut end = index + 1;
                        while end < bytes.len()
                            && (bytes[end].is_ascii_uppercase()
                                || bytes[end].is_ascii_digit()
                                || bytes[end] == b'_')
                        {
                            end += 1;
                        }
                        let code = &source[index + 1..end];
                        // Skip bare prefixes ("E_") and format prefixes
                        // ("E_TEST_{n}") that are not complete codes.
                        if code.len() > 2 && !code.ends_with('_') {
                            found.insert(code.to_string());
                        }
                        index = end;
                    } else {
                        index += 1;
                    }
                }
            }
        }
    }
}
