use serde::{Deserialize, Serialize};

pub const BACKUP_JOURNAL_STATUS_VERSION: u16 = 1;
pub const DEFAULT_JOURNAL_MAX_COMMITS: u64 = 10_000;
pub const DEFAULT_JOURNAL_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const HARD_JOURNAL_MAX_COMMITS: u64 = 100_000;
pub const HARD_JOURNAL_MAX_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJournalConfig {
    pub chain_id: String,
    pub baseline_sequence: u64,
    pub baseline_checksum: String,
    #[serde(default = "default_max_commits")]
    pub max_commits: u64,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
}

impl BackupJournalConfig {
    pub fn new(
        chain_id: impl Into<String>,
        baseline_sequence: u64,
        baseline_checksum: impl Into<String>,
    ) -> Self {
        Self {
            chain_id: chain_id.into(),
            baseline_sequence,
            baseline_checksum: baseline_checksum.into(),
            max_commits: DEFAULT_JOURNAL_MAX_COMMITS,
            max_bytes: DEFAULT_JOURNAL_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupJournalState {
    Disabled,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupJournalStatus {
    pub version: u16,
    pub storage_format: u32,
    pub state: BackupJournalState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported_checksum: Option<String>,
    pub head_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_retained_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_retained_sequence: Option<u64>,
    pub commit_count: u64,
    pub expanded_bytes: u64,
    pub max_commits: u64,
    pub max_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_checksum: Option<String>,
}

const fn default_max_commits() -> u64 {
    DEFAULT_JOURNAL_MAX_COMMITS
}

const fn default_max_bytes() -> u64 {
    DEFAULT_JOURNAL_MAX_BYTES
}
