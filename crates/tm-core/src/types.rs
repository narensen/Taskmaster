use serde::{Deserialize, Serialize};

pub type PageId = String;
pub type DictKey = String;

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
