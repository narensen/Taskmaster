use serde::{Deserialize, Serialize};

pub type PageId = String;
pub type DictKey = String;

/// Dictionary keys and page ids become directory names, so they stay one path segment.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictTier {
    Local,
    Commons,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Schema,
    Routes,
    ComponentApi,
    Pattern,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryStats {
    pub reuses: u64,
    pub pass_rate: f32,
    /// Successful uses of this Commons entry.
    #[serde(default)]
    pub passes: u64,
    /// Uses that failed their page.
    #[serde(default)]
    pub fails: u64,
}

/// One immutable Dictionary entry. A change is published as a new entry with a new hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DictionaryEntry {
    pub key: DictKey,
    pub tier: DictTier,
    pub kind: EntryKind,
    /// One line, shown in the index every worker sees.
    pub summary: String,
    pub content: String,
    /// Content address.
    pub hash: String,
    /// Stack the entry was verified against, e.g. `next@15,react@19`.
    pub stack: String,
    /// Commons entries only.
    pub stats: Option<EntryStats>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractFile {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PagePlan {
    pub id: PageId,
    pub route: String,
    /// The only files this page's worker may change, e.g. `app/dashboard/**`.
    pub owned_glob: String,
    pub cites: Vec<DictKey>,
    pub brief: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub tokens: u64,
    pub usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Real source files that workers import; the compiler enforces them.
    pub contracts: Vec<ContractFile>,
    pub pages: Vec<PagePlan>,
    pub estimate: Estimate,
}

/// A worker's objection to a Dictionary entry, raised instead of editing around it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flag {
    pub page: PageId,
    pub key: DictKey,
    pub issue: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagResolution {
    /// The entry stands; the worker must conform to it.
    Conform,
    /// The Taskmaster published a successor entry.
    Amended,
}

/// One connection to review: a Dictionary entry and the pages that consume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewScope {
    pub key: DictKey,
    pub consumers: Vec<PageId>,
}

/// A reviewer's claim that something is broken. Reviewers never edit; a finding is routed
/// to the page's owning worker as a repair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub page: PageId,
    /// The connection the reviewer was looking at, if any.
    pub key: Option<DictKey>,
    pub file: String,
    pub line: u32,
    /// The concrete failure: what input or state leads to what wrong result.
    pub failure: String,
}

impl Finding {
    /// A finding without a location and a concrete failure is dropped: acting on a vague
    /// one costs a repair round for nothing.
    pub fn is_actionable(&self) -> bool {
        !self.file.trim().is_empty() && self.line > 0 && self.failure.trim().len() >= 10
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    ContractsLocked,
    /// The master repaired a shared file the run wrote, after the project's checks failed on it.
    SharedRepaired,
    PageBuilt,
    PageRepaired,
    EntryAmended,
    ReviewFinding,
    VerifyFailed,
}

/// One line of the change log: what changed, where, and who changed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeEntry {
    /// RFC 3339 timestamp, supplied by the executor.
    pub at: String,
    pub kind: ChangeKind,
    pub page: Option<PageId>,
    pub commit: Option<String>,
    /// Model that made the change.
    pub model: Option<String>,
    pub files: Vec<String>,
    /// Dictionary keys the change touches or depends on.
    pub keys: Vec<DictKey>,
    pub summary: String,
}
