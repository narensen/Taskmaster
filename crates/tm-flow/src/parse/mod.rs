//! Language detection and AST/fallback parsing for `.flow`.

pub mod fallback;
#[cfg(feature = "python")]
pub mod python;
#[cfg(feature = "rust")]
pub mod rust;
#[cfg(feature = "typescript")]
pub mod typescript;

use std::collections::HashMap;
use std::path::Path;

use crate::model::{
    EdgeKind, Node, NodeKind, NodeStatus, body_hash, file_node_id, git_blob_sha,
    normalize_rel_path, sig_hash, symbol_node_id,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Jsx,
    Rust,
    Fallback(&'static str),
}

impl Language {
    pub fn name(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::JavaScript => "javascript",
            Language::Jsx => "jsx",
            Language::Rust => "rust",
            Language::Fallback(name) => name,
        }
    }
}

/// Detect a supported source language from a relative file path.
pub fn detect_language(rel_path: &str) -> Option<Language> {
    let ext = Path::new(rel_path)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "py" | "pyi" => Some(Language::Python),
        "ts" | "mts" | "cts" => Some(Language::TypeScript),
        "tsx" => Some(Language::Tsx),
        "js" | "mjs" | "cjs" => Some(Language::JavaScript),
        "jsx" => Some(Language::Jsx),
        "rs" => Some(Language::Rust),
        "go" => Some(Language::Fallback("go")),
        "java" => Some(Language::Fallback("java")),
        "kt" | "kts" => Some(Language::Fallback("kotlin")),
        "rb" => Some(Language::Fallback("ruby")),
        "c" | "h" => Some(Language::Fallback("c")),
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" => Some(Language::Fallback("cpp")),
        "php" => Some(Language::Fallback("php")),
        "cs" => Some(Language::Fallback("csharp")),
        _ => None,
    }
}

/// An unresolved import extracted from a single file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    /// Module specifier or path (`tinydb.btree`, `./utils`, `crate::store`).
    pub module: String,
    /// Number of leading dots for Python relative imports (`0` for non-relative).
    pub relative_dots: usize,
    /// Imported symbol bindings as `(remote_name, local_alias)`.
    pub names: Vec<(String, String)>,
    /// Optional module-level alias (`import foo as bar`, `import * as ns from './m'`).
    pub module_alias: Option<String>,
    /// True for wildcard imports (`from x import *`, `use foo::*`, `export * from './m'`).
    pub wildcard: bool,
}

/// An unresolved inheritance or implementation relation extracted from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRelation {
    pub from_id: String,
    pub target: String,
    pub kind: EdgeKind,
}

/// An unresolved call site extracted from a function, method, or module body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCall {
    pub from_id: String,
    /// Receiver expression (`self`, `this`, `storage`, `BTree`, `crate::mod`), if any.
    pub receiver: Option<String>,
    /// Target symbol/method name.
    pub callee: String,
    /// Enclosing class/struct/trait qualified name when inside a method.
    pub enclosing_type: Option<String>,
    /// Inferred receiver type name from local annotations or constructors, if known.
    pub receiver_type: Option<String>,
}

/// Intermediate AST extraction for a single file before cross-file resolution.
#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub path: String,
    pub lang: String,
    pub blob: String,
    pub file_id: String,
    pub nodes: Vec<Node>,
    pub imports: Vec<RawImport>,
    pub relations: Vec<RawRelation>,
    pub calls: Vec<RawCall>,
}

/// Parse one source file into its `ParsedFile` representation.
/// Returns `None` if the extension is not a recognized source language.
pub fn parse_file(rel_path: &str, source: &str) -> Option<ParsedFile> {
    let lang = detect_language(rel_path)?;
    let path = normalize_rel_path(rel_path);
    let blob = git_blob_sha(source.as_bytes());
    let parsed = match lang {
        #[cfg(feature = "python")]
        Language::Python => python::parse(&path, source, &blob),
        #[cfg(not(feature = "python"))]
        Language::Python => fallback::parse(&path, "python", source, &blob),

        #[cfg(feature = "typescript")]
        Language::TypeScript => typescript::parse(&path, "typescript", typescript::Dialect::Ts, source, &blob),
        #[cfg(not(feature = "typescript"))]
        Language::TypeScript => fallback::parse(&path, "typescript", source, &blob),

        #[cfg(feature = "typescript")]
        Language::Tsx => typescript::parse(&path, "tsx", typescript::Dialect::Tsx, source, &blob),
        #[cfg(not(feature = "typescript"))]
        Language::Tsx => fallback::parse(&path, "tsx", source, &blob),

        #[cfg(feature = "typescript")]
        Language::JavaScript => typescript::parse(&path, "javascript", typescript::Dialect::Js, source, &blob),
        #[cfg(not(feature = "typescript"))]
        Language::JavaScript => fallback::parse(&path, "javascript", source, &blob),

        #[cfg(feature = "typescript")]
        Language::Jsx => typescript::parse(&path, "jsx", typescript::Dialect::Js, source, &blob),
        #[cfg(not(feature = "typescript"))]
        Language::Jsx => fallback::parse(&path, "jsx", source, &blob),

        #[cfg(feature = "rust")]
        Language::Rust => rust::parse(&path, source, &blob),
        #[cfg(not(feature = "rust"))]
        Language::Rust => fallback::parse(&path, "rust", source, &blob),

        Language::Fallback(name) => fallback::parse(&path, name, source, &blob),
    };
    Some(parsed)
}

/// Tracks duplicate qualified names in source order to assign stable `~n` suffixes.
#[derive(Default)]
pub(crate) struct NameCounter {
    counts: HashMap<String, usize>,
}

impl NameCounter {
    pub(crate) fn next_id(&mut self, lang: &str, path: &str, qualified: &str) -> String {
        let count = self.counts.entry(qualified.to_string()).or_insert(0);
        *count += 1;
        symbol_node_id(lang, path, qualified, *count)
    }
}

/// Build the synthetic `file` node for a parsed file.
pub(crate) fn make_file_node(lang: &str, path: &str, source: &str, doc: Option<String>) -> Node {
    let lines = source.lines().count().max(1) as u32;
    let id = file_node_id(lang, path);
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();
    Node {
        id,
        kind: NodeKind::File,
        name,
        path: path.to_string(),
        parent: None,
        span: [1, lines],
        sig: path.to_string(),
        sig_hash: sig_hash(path),
        body_hash: body_hash(source),
        exported: true,
        doc,
        owner: None,
        status: NodeStatus::Built,
    }
}

/// Collapse a multi-line header signature into one tidy line, capped at 220 chars.
pub(crate) fn clean_signature(raw: &str) -> String {
    let joined: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::with_capacity(joined.len().min(220));
    let mut prev_space = false;
    for ch in joined.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
        if out.len() >= 220 {
            break;
        }
    }
    out.trim().to_string()
}

/// Extract the first meaningful line from a doc comment or docstring, capped at 160 chars.
pub(crate) fn clean_doc_line(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line
            .trim()
            .trim_start_matches("/**")
            .trim_start_matches("/*!")
            .trim_start_matches("/*")
            .trim_start_matches("///")
            .trim_start_matches("//!")
            .trim_start_matches("//")
            .trim_start_matches('*')
            .trim_end_matches("*/")
            .trim();
        if !trimmed.is_empty() && !trimmed.starts_with('@') {
            let line: String = trimmed.chars().take(160).collect();
            return Some(line);
        }
    }
    None
}

/// Strip leading/trailing Python string quotes (`"""`, `'''`, `"`, `'`, optional `r`/`u`/`f` prefix).
#[cfg(feature = "python")]
pub(crate) fn strip_python_string(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let s = trimmed.trim_start_matches(['r', 'R', 'u', 'U', 'b', 'B', 'f', 'F']);
    let inner = if let Some(rest) = s.strip_prefix("\"\"\"").and_then(|x| x.strip_suffix("\"\"\"")) {
        rest
    } else if let Some(rest) = s.strip_prefix("'''").and_then(|x| x.strip_suffix("'''")) {
        rest
    } else if let Some(rest) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        rest
    } else {
        s.strip_prefix('\'').and_then(|x| x.strip_suffix('\''))?
    };
    clean_doc_line(inner)
}
