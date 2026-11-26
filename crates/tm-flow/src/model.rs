//! Core data model, stable IDs, hashes, and deterministic shard serialization for `.flow`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use sha2::{Digest as _, Sha256};

/// Root directory name for the knowledge graph inside a project tree.
pub const FLOW_DIR: &str = ".flow";

/// Current `.flow` on-disk schema version. Bumping this forces a clean rebuild.
pub const SCHEMA_VERSION: u32 = 1;

/// Builder version stamped into `.flow/meta.json`.
pub const BUILDER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Required contents of `.flow/.gitignore` so derived SQLite state is never committed.
pub const FLOW_GITIGNORE_CONTENT: &str = "index.sqlite\nindex.sqlite-wal\nindex.sqlite-shm\n";

/// True when `path` refers to `.flow` itself or any file inside `.flow/`.
pub fn is_flow_path(path: &str) -> bool {
    tm_core::is_flow_path(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    File,
    Module,
    Class,
    Function,
    Method,
    Type,
    Const,
    Unit,
    Contract,
}

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Module => "module",
            NodeKind::Class => "class",
            NodeKind::Function => "function",
            NodeKind::Method => "method",
            NodeKind::Type => "type",
            NodeKind::Const => "const",
            NodeKind::Unit => "unit",
            NodeKind::Contract => "contract",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file" => Some(NodeKind::File),
            "module" => Some(NodeKind::Module),
            "class" => Some(NodeKind::Class),
            "function" => Some(NodeKind::Function),
            "method" => Some(NodeKind::Method),
            "type" => Some(NodeKind::Type),
            "const" => Some(NodeKind::Const),
            "unit" => Some(NodeKind::Unit),
            "contract" => Some(NodeKind::Contract),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Imports,
    Calls,
    Inherits,
    Implements,
    Owns,
    ContractOf,
    Tests,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Imports => "imports",
            EdgeKind::Calls => "calls",
            EdgeKind::Inherits => "inherits",
            EdgeKind::Implements => "implements",
            EdgeKind::Owns => "owns",
            EdgeKind::ContractOf => "contract_of",
            EdgeKind::Tests => "tests",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "imports" => Some(EdgeKind::Imports),
            "calls" => Some(EdgeKind::Calls),
            "inherits" => Some(EdgeKind::Inherits),
            "implements" => Some(EdgeKind::Implements),
            "owns" => Some(EdgeKind::Owns),
            "contract_of" => Some(EdgeKind::ContractOf),
            "tests" => Some(EdgeKind::Tests),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Exact,
    #[default]
    Import,
    Name,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::Import => "import",
            Confidence::Name => "name",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "exact" => Some(Confidence::Exact),
            "import" => Some(Confidence::Import),
            "name" => Some(Confidence::Name),
            _ => None,
        }
    }

    /// High-confidence edges (`exact` or `import`) used in unit maps.
    pub fn is_high(self) -> bool {
        matches!(self, Confidence::Exact | Confidence::Import)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    #[default]
    Built,
    Planned,
    Contract,
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStatus::Built => "built",
            NodeStatus::Planned => "planned",
            NodeStatus::Contract => "contract",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "built" => Some(NodeStatus::Built),
            "planned" => Some(NodeStatus::Planned),
            "contract" => Some(NodeStatus::Contract),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub span: [u32; 2],
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sig: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sig_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body_hash: String,
    #[serde(default)]
    pub exported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub status: NodeStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    #[serde(default)]
    pub conf: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    pub path: String,
    pub lang: String,
    pub blob: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl Shard {
    /// Sort nodes and edges into canonical deterministic order and deduplicate edges.
    pub fn canonicalize(&mut self) {
        self.nodes
            .sort_by(|a, b| a.span.cmp(&b.span).then_with(|| a.id.cmp(&b.id)));
        self.edges.sort();
        self.edges.dedup();
    }

    /// Format deterministically with one JSON object per node/edge line for readable git diffs.
    pub fn to_deterministic_json(&self) -> anyhow::Result<String> {
        let mut sorted = self.clone();
        sorted.canonicalize();
        let mut out = String::new();
        out.push_str("{\n");
        let _ = writeln!(out, "  \"path\": {},", serde_json::to_string(&sorted.path)?);
        let _ = writeln!(out, "  \"lang\": {},", serde_json::to_string(&sorted.lang)?);
        let _ = writeln!(out, "  \"blob\": {},", serde_json::to_string(&sorted.blob)?);
        if let Some(owner) = &sorted.owner {
            let _ = writeln!(out, "  \"owner\": {},", serde_json::to_string(owner)?);
        }
        out.push_str("  \"nodes\": [");
        if !sorted.nodes.is_empty() {
            out.push('\n');
            for (i, node) in sorted.nodes.iter().enumerate() {
                out.push_str("    ");
                out.push_str(&serde_json::to_string(node)?);
                if i + 1 < sorted.nodes.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str("  ],\n");
        } else {
            out.push_str("],\n");
        }
        out.push_str("  \"edges\": [");
        if !sorted.edges.is_empty() {
            out.push('\n');
            for (i, edge) in sorted.edges.iter().enumerate() {
                out.push_str("    ");
                out.push_str(&serde_json::to_string(edge)?);
                if i + 1 < sorted.edges.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str("  ]\n");
        } else {
            out.push_str("]\n");
        }
        out.push_str("}\n");
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
    pub builder_version: String,
    pub head: String,
    pub tree: String,
    pub languages: BTreeMap<String, usize>,
    pub file_count: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub moves: BTreeMap<String, String>,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            builder_version: BUILDER_VERSION.to_string(),
            head: String::new(),
            tree: String::new(),
            languages: BTreeMap::new(),
            file_count: 0,
            moves: BTreeMap::new(),
        }
    }
}

impl Meta {
    pub fn to_deterministic_json(&self) -> anyhow::Result<String> {
        let mut s = serde_json::to_string_pretty(self)?;
        s.push('\n');
        Ok(s)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitSeed {
    pub id: String,
    pub owned_glob: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub is_ui: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub route: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSeed {
    #[serde(default)]
    pub units: Vec<UnitSeed>,
    #[serde(default)]
    pub contracts: Vec<String>,
    #[serde(default)]
    pub contract_tests: Vec<String>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

impl PlanSeed {
    pub fn canonicalize(&mut self) {
        self.units.sort_by(|a, b| a.id.cmp(&b.id));
        self.contracts.sort();
        self.contracts.dedup();
        self.contract_tests.sort();
        self.contract_tests.dedup();
        self.nodes
            .sort_by(|a, b| a.id.cmp(&b.id).then_with(|| a.span.cmp(&b.span)));
        self.nodes.dedup_by(|a, b| a.id == b.id);
        self.edges.sort();
        self.edges.dedup();
    }

    pub fn to_deterministic_json(&self) -> anyhow::Result<String> {
        let mut sorted = self.clone();
        sorted.canonicalize();
        let mut s = serde_json::to_string_pretty(&sorted)?;
        s.push('\n');
        Ok(s)
    }
}

/// Short language prefix used inside stable node IDs (`<lang>:<path>#<qualified.name>`).
pub fn lang_prefix(lang: &str) -> &'static str {
    match lang {
        "python" | "py" => "py",
        "typescript" | "tsx" | "ts" => "ts",
        "javascript" | "jsx" | "js" => "js",
        "rust" | "rs" => "rs",
        "go" => "go",
        "java" => "java",
        "kotlin" | "kt" => "kt",
        "ruby" | "rb" => "rb",
        "c" => "c",
        "cpp" => "cpp",
        "php" => "php",
        "csharp" | "cs" => "cs",
        _ => "src",
    }
}

/// Normalize a repo-relative path to POSIX forward slashes without a leading `./`.
pub fn normalize_rel_path(path: &str) -> String {
    let p = path.replace('\\', "/");
    p.trim_start_matches("./").to_string()
}

/// Stable ID for a file node: `<lang>:<path>`.
pub fn file_node_id(lang: &str, path: &str) -> String {
    format!("{}:{}", lang_prefix(lang), normalize_rel_path(path))
}

/// Stable ID for a symbol node: `<lang>:<path>#<qualified.name>` with `~<n>` for duplicates
/// (1-indexed occurrence; occurrence `<= 1` omits the suffix).
pub fn symbol_node_id(lang: &str, path: &str, qualified: &str, occurrence: usize) -> String {
    let base = format!("{}:{}#{}", lang_prefix(lang), normalize_rel_path(path), qualified);
    if occurrence <= 1 {
        base
    } else {
        format!("{base}~{occurrence}")
    }
}

/// Stable ID for a synthetic unit node: `unit:<id>`.
pub fn unit_node_id(unit_id: &str) -> String {
    format!("unit:{unit_id}")
}

/// Exact Git blob SHA-1 for `bytes` (`sha1("blob <len>\0<bytes>")`).
pub fn git_blob_sha(bytes: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

/// Deterministic short SHA-256 hash (16 hex chars) of a whitespace-normalized signature.
pub fn sig_hash(sig: &str) -> String {
    short_sha256(&normalize_ws(sig))
}

/// Deterministic short SHA-256 hash (16 hex chars) of a whitespace-normalized body.
pub fn body_hash(body: &str) -> String {
    short_sha256(&normalize_ws(body))
}

fn normalize_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let mut prev_space = false;
        for ch in trimmed.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(ch);
                prev_space = false;
            }
        }
    }
    out
}

fn short_sha256(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    hex_encode(&digest[..8])
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_ids_and_duplicates() {
        assert_eq!(file_node_id("python", "tinydb/executor.py"), "py:tinydb/executor.py");
        assert_eq!(
            symbol_node_id("python", "./tinydb/executor.py", "Executor.execute", 1),
            "py:tinydb/executor.py#Executor.execute"
        );
        assert_eq!(
            symbol_node_id("python", "tinydb/executor.py", "Executor.execute", 2),
            "py:tinydb/executor.py#Executor.execute~2"
        );
        assert_eq!(
            symbol_node_id("typescript", "src/app.tsx", "render", 3),
            "ts:src/app.tsx#render~3"
        );
        assert_eq!(symbol_node_id("rust", "src/lib.rs", "Store.get", 1), "rs:src/lib.rs#Store.get");
        assert_eq!(unit_node_id("executor"), "unit:executor");
    }

    #[test]
    fn git_blob_sha_matches_git_hash_object() {
        // Known git blob SHA for "hello\n" is ce013625030ba8dba906f756967f9e9ca394464a
        assert_eq!(
            git_blob_sha(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        // Empty blob is e69de29bb2d1d6434b8b29ae775ad8c2e48c5391
        assert_eq!(
            git_blob_sha(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn hashes_normalize_whitespace_and_detect_real_changes() {
        assert_eq!(
            sig_hash("def  foo( x: int ) -> str"),
            sig_hash("def foo( x: int ) -> str\n")
        );
        assert_ne!(
            sig_hash("def foo(x: int) -> str"),
            sig_hash("def foo(x: str) -> str")
        );
        assert_eq!(
            body_hash("def f():\n    return 1\n\n"),
            body_hash("def f():\n  return 1")
        );
        assert_ne!(body_hash("return 1"), body_hash("return 2"));
    }

    #[test]
    fn shard_serialization_is_sorted_and_deterministic() {
        let mut shard = Shard {
            path: "tinydb/executor.py".into(),
            lang: "python".into(),
            blob: git_blob_sha(b"class Executor:\n    pass\n"),
            owner: Some("executor".into()),
            nodes: vec![
                Node {
                    id: "py:tinydb/executor.py#Executor.run".into(),
                    kind: NodeKind::Method,
                    name: "run".into(),
                    path: "tinydb/executor.py".into(),
                    parent: Some("py:tinydb/executor.py#Executor".into()),
                    span: [10, 20],
                    sig: "def run(self)".into(),
                    sig_hash: sig_hash("def run(self)"),
                    body_hash: body_hash("def run(self): pass"),
                    exported: true,
                    doc: Some("Run query.".into()),
                    owner: Some("executor".into()),
                    status: NodeStatus::Built,
                },
                Node {
                    id: "py:tinydb/executor.py#Executor".into(),
                    kind: NodeKind::Class,
                    name: "Executor".into(),
                    path: "tinydb/executor.py".into(),
                    parent: None,
                    span: [1, 20],
                    sig: "class Executor".into(),
                    sig_hash: sig_hash("class Executor"),
                    body_hash: body_hash("class Executor: pass"),
                    exported: true,
                    doc: None,
                    owner: Some("executor".into()),
                    status: NodeStatus::Built,
                },
            ],
            edges: vec![
                Edge {
                    from: "py:tinydb/executor.py#Executor.run".into(),
                    to: "py:tinydb/btree.py#BTree.insert".into(),
                    kind: EdgeKind::Calls,
                    conf: Confidence::Import,
                },
                Edge {
                    from: "py:tinydb/executor.py".into(),
                    to: "py:tinydb/btree.py".into(),
                    kind: EdgeKind::Imports,
                    conf: Confidence::Import,
                },
                Edge {
                    from: "py:tinydb/executor.py".into(),
                    to: "py:tinydb/btree.py".into(),
                    kind: EdgeKind::Imports,
                    conf: Confidence::Import,
                },
            ],
        };
        let json1 = shard.to_deterministic_json().unwrap();
        shard.nodes.reverse();
        shard.edges.reverse();
        let json2 = shard.to_deterministic_json().unwrap();
        assert_eq!(json1, json2);

        let parsed: Shard = serde_json::from_str(&json1).unwrap();
        assert_eq!(parsed.nodes[0].name, "Executor");
        assert_eq!(parsed.nodes[1].name, "run");
        assert_eq!(parsed.edges.len(), 2, "duplicate edges must be deduplicated");
        assert_eq!(parsed.edges[0].kind, EdgeKind::Imports);
    }

    #[test]
    fn flow_path_detection() {
        assert!(is_flow_path(".flow"));
        assert!(is_flow_path(".flow/meta.json"));
        assert!(is_flow_path("./.flow/files/a.py.json"));
        assert!(is_flow_path(".flow\\index.sqlite"));
        assert!(!is_flow_path("src/.flow_helper.rs"));
        assert!(!is_flow_path("flow/a.py"));
    }
}
