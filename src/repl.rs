use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use serde::{Deserialize, Serialize};

use crate::Introspection;

const HISTORY_VERSION: u32 = 1;
const MAX_HISTORY_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 10_000;

pub(crate) const KEYWORDS: &[&str] = &[
    "add",
    "aggregate",
    "all",
    "and",
    "any",
    "as",
    "by",
    "change",
    "contains",
    "count",
    "create",
    "default",
    "delete",
    "derive",
    "drop",
    "else",
    "explain",
    "false",
    "filter",
    "float",
    "from",
    "group",
    "index",
    "insert",
    "int",
    "key",
    "length",
    "let",
    "limit",
    "match",
    "max",
    "migration",
    "min",
    "not",
    "option",
    "or",
    "parent",
    "rename",
    "returning",
    "select",
    "set",
    "sort",
    "sum",
    "table",
    "take",
    "text",
    "to",
    "true",
    "tuple",
    "type",
    "update",
    "upsert",
    "using",
    "variant",
];

pub(crate) const META_COMMANDS: &[&str] =
    &[".help", ".quit", ".schema", ".storage", ".tables", ".types"];

#[derive(Debug, Clone, Default)]
pub struct HistoryOptions {
    pub disabled: bool,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryEntry {
    version: u32,
    source: String,
}

pub(crate) struct HistoryStore {
    path: Option<PathBuf>,
}

pub(crate) struct LoadedHistory {
    pub entries: Vec<String>,
    pub warning: Option<String>,
    pub store: HistoryStore,
}

impl HistoryStore {
    pub fn load(options: &HistoryOptions) -> LoadedHistory {
        let path = if options.disabled {
            None
        } else {
            options.path.clone().or_else(default_history_path)
        };
        let Some(path) = path else {
            return LoadedHistory {
                entries: Vec::new(),
                warning: None,
                store: Self { path: None },
            };
        };
        if !path.exists() {
            return LoadedHistory {
                entries: Vec::new(),
                warning: None,
                store: Self { path: Some(path) },
            };
        }
        let result = load_entries(&path);
        match result {
            Ok(entries) => LoadedHistory {
                entries,
                warning: None,
                store: Self { path: Some(path) },
            },
            Err(error) => LoadedHistory {
                entries: Vec::new(),
                warning: Some(format!(
                    "history disabled for this session; '{}' was not changed: {error}",
                    path.display()
                )),
                store: Self { path: None },
            },
        }
    }

    pub fn append(&mut self, source: &str) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !safe_to_persist(source) {
            return Ok(());
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("create history directory '{}': {error}", parent.display())
            })?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|error| format!("open history '{}': {error}", path.display()))?;
        let mut encoded = serde_json::to_vec(&HistoryEntry {
            version: HISTORY_VERSION,
            source: source.trim_end().to_string(),
        })
        .map_err(|error| format!("encode history '{}': {error}", path.display()))?;
        encoded.push(b'\n');
        file.write_all(&encoded)
            .and_then(|()| file.flush())
            .map_err(|error| format!("write history '{}': {error}", path.display()))
    }

    pub fn disable(&mut self) {
        self.path = None;
    }
}

fn default_history_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(".unionid").join("history.jsonl"))
}

fn load_entries(path: &Path) -> Result<Vec<String>, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("read metadata: {error}"))?;
    if metadata.len() > MAX_HISTORY_BYTES {
        return Err(format!("file exceeds {MAX_HISTORY_BYTES} byte limit"));
    }
    let file = std::fs::File::open(path).map_err(|error| format!("open: {error}"))?;
    let mut entries = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        if index >= MAX_HISTORY_ENTRIES {
            return Err(format!("file exceeds {MAX_HISTORY_ENTRIES} entry limit"));
        }
        let line = line.map_err(|error| format!("read line {}: {error}", index + 1))?;
        let entry: HistoryEntry = serde_json::from_str(&line)
            .map_err(|error| format!("invalid line {}: {error}", index + 1))?;
        if entry.version != HISTORY_VERSION {
            return Err(format!(
                "unsupported version {} on line {}",
                entry.version,
                index + 1
            ));
        }
        if entry.source.len() > crate::syntax::MAX_SOURCE_BYTES {
            return Err(format!("entry on line {} exceeds source limit", index + 1));
        }
        if !safe_to_persist(&entry.source) {
            return Err(format!(
                "entry on line {} violates the safe-history policy",
                index + 1
            ));
        }
        entries.push(entry.source);
    }
    Ok(entries)
}

pub(crate) fn safe_to_persist(source: &str) -> bool {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('.') {
        return matches!(
            trimmed,
            ".help" | ".schema" | ".storage" | ".tables" | ".types"
        );
    }
    if trimmed
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|word| {
            matches!(
                word.to_ascii_lowercase().as_str(),
                "create"
                    | "delete"
                    | "insert"
                    | "migration"
                    | "table"
                    | "type"
                    | "update"
                    | "upsert"
            )
        })
    {
        return false;
    }
    !contains_literal(trimmed)
}

fn contains_literal(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => return true,
            b'#' => return true,
            b'0'..=b'9' => {
                let previous = index.checked_sub(1).and_then(|i| bytes.get(i)).copied();
                if previous.is_none_or(|byte| !byte.is_ascii_alphanumeric() && byte != b'_') {
                    return true;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    false
}

#[derive(Default)]
pub(crate) struct CompletionHelper {
    candidates: Vec<String>,
}

impl CompletionHelper {
    pub fn new(introspection: Option<&Introspection>) -> Self {
        let mut helper = Self::default();
        helper.set_catalog(introspection);
        helper
    }

    pub fn set_catalog(&mut self, introspection: Option<&Introspection>) {
        let mut candidates = KEYWORDS
            .iter()
            .chain(META_COMMANDS)
            .map(|candidate| (*candidate).to_string())
            .collect::<BTreeSet<_>>();
        if let Some(introspection) = introspection {
            candidates.extend(introspection.tables.iter().cloned());
            candidates.extend(introspection.types.iter().cloned());
            candidates.extend(introspection.fields.iter().cloned());
        }
        self.candidates = candidates.into_iter().collect();
    }
}

impl Completer for CompletionHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let prefix = &line[..pos];
        let start = prefix
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                (!character.is_ascii_alphanumeric() && character != '_' && character != '.')
                    .then_some(index + character.len_utf8())
            })
            .unwrap_or(0);
        let fragment = &prefix[start..];
        let pairs = self
            .candidates
            .iter()
            .filter(|candidate| candidate.starts_with(fragment))
            .map(|candidate| Pair {
                display: candidate.clone(),
                replacement: candidate.clone(),
            })
            .collect();
        Ok((start, pairs))
    }
}

impl Hinter for CompletionHelper {
    type Hint = String;
}

impl Highlighter for CompletionHelper {}
impl Validator for CompletionHelper {}
impl Helper for CompletionHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_history_excludes_writes_and_literals() {
        assert!(safe_to_persist("from tasks | select {id, title}"));
        assert!(safe_to_persist(".schema"));
        assert!(!safe_to_persist("from tasks | filter id == 42"));
        assert!(!safe_to_persist("from tasks | filter title == \"secret\""));
        assert!(!safe_to_persist("insert tasks {id = 1}"));
        assert!(!safe_to_persist("update tasks\nset title = \"secret\""));
        assert!(!safe_to_persist("from tasks\ninsert tasks {id = 1}"));
        assert!(!safe_to_persist("from tasks # private note"));
    }

    #[test]
    fn configured_history_round_trips_safe_entries_and_skips_sensitive_ones() {
        let path = std::env::temp_dir().join(format!(
            "unionid-history-{}-{}.jsonl",
            std::process::id(),
            std::thread::current().name().unwrap_or("round-trip")
        ));
        let _ = std::fs::remove_file(&path);
        let options = HistoryOptions {
            disabled: false,
            path: Some(path.clone()),
        };
        let mut loaded = HistoryStore::load(&options);
        assert!(loaded.warning.is_none());
        loaded.store.append("from tasks | select id").unwrap();
        loaded.store.append("from tasks | filter id == 42").unwrap();
        let loaded = HistoryStore::load(&options);
        assert_eq!(loaded.entries, ["from tasks | select id"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_history_is_left_untouched_and_disabled() {
        let path = std::env::temp_dir().join(format!(
            "unionid-history-corrupt-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(&path, b"not-json\n").unwrap();
        let options = HistoryOptions {
            disabled: false,
            path: Some(path.clone()),
        };
        let mut loaded = HistoryStore::load(&options);
        assert!(loaded.warning.as_deref().unwrap().contains("not changed"));
        loaded.store.append("from tasks").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"not-json\n");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn disabled_history_never_creates_the_configured_path() {
        let path = std::env::temp_dir().join(format!(
            "unionid-history-disabled-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let options = HistoryOptions {
            disabled: true,
            path: Some(path.clone()),
        };
        let mut loaded = HistoryStore::load(&options);
        loaded.store.append("from tasks").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn completion_combines_keywords_meta_commands_and_catalog_names() {
        let mut engine = crate::Engine::memory();
        assert!(
            engine
                .execute("type Task =\n  id int\n  title text\ntable tasks Task\n  key id")
                .ok
        );
        let helper = CompletionHelper::new(Some(&engine.introspection()));
        for expected in [
            "from",
            "returning",
            ".schema",
            "Task",
            "tasks",
            "id",
            "title",
        ] {
            assert!(
                helper
                    .candidates
                    .iter()
                    .any(|candidate| candidate == expected)
            );
        }
        let empty = CompletionHelper::new(Some(&crate::Engine::memory().introspection()));
        assert!(empty.candidates.iter().any(|candidate| candidate == "from"));
        assert!(
            empty
                .candidates
                .iter()
                .any(|candidate| candidate == ".tables")
        );

        let history = rustyline::history::DefaultHistory::new();
        let context = Context::new(&history);
        let (start, matches) = helper.complete("from ta", 7, &context).unwrap();
        assert_eq!(start, 5);
        assert_eq!(
            matches
                .into_iter()
                .map(|candidate| candidate.replacement)
                .collect::<Vec<_>>(),
            ["table", "take", "tasks"]
        );
    }
}
