//! Line-prefix fallback symbol extractor (matching `tm-daemon/src/lsp.rs::extract_symbols`).
//!
//! Used for languages without an enabled tree-sitter grammar (Go, Java, Kotlin, Ruby, C/C++, etc.)
//! or when a language's Cargo feature is disabled. Extracts top-level declarations with no call edges.

use std::path::Path;

use crate::model::{Node, NodeKind, NodeStatus, body_hash, file_node_id, sig_hash};
use crate::parse::{NameCounter, ParsedFile, clean_signature, make_file_node};

pub fn parse(path: &str, lang: &str, source: &str, blob: &str) -> ParsedFile {
    let file_id = file_node_id(lang, path);
    let mut nodes = vec![make_file_node(lang, path, source, None)];
    let mut counter = NameCounter::default();

    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    for (idx, raw) in source.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') || line.starts_with("/*") {
            continue;
        }
        let line_no = idx as u32 + 1;
        let extracted: Option<(String, NodeKind, bool, String)> = match ext.as_str() {
            "ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs" => {
                let (exported, rest) = if let Some(r) = line.strip_prefix("export default ") {
                    (true, r)
                } else if let Some(r) = line.strip_prefix("export ") {
                    (true, r)
                } else {
                    (false, line)
                };
                let rest = rest.strip_prefix("async ").unwrap_or(rest);
                match_prefixes(
                    rest,
                    &[
                        ("function ", NodeKind::Function),
                        ("interface ", NodeKind::Type),
                        ("type ", NodeKind::Type),
                        ("class ", NodeKind::Class),
                        ("enum ", NodeKind::Type),
                        ("const ", NodeKind::Const),
                    ],
                )
                .map(|(name, kind)| (name, kind, exported, clean_signature(line.trim_end_matches('{'))))
            }
            "rs" => {
                let (exported, rest) = if let Some(r) = line.strip_prefix("pub(crate) ") {
                    (true, r)
                } else if let Some(r) = line.strip_prefix("pub ") {
                    (true, r)
                } else {
                    (false, line)
                };
                let rest = rest.strip_prefix("async ").unwrap_or(rest);
                match_prefixes(
                    rest,
                    &[
                        ("fn ", NodeKind::Function),
                        ("struct ", NodeKind::Type),
                        ("enum ", NodeKind::Type),
                        ("trait ", NodeKind::Type),
                        ("type ", NodeKind::Type),
                        ("const ", NodeKind::Const),
                    ],
                )
                .map(|(name, kind)| (name, kind, exported, clean_signature(line.trim_end_matches('{'))))
            }
            "py" | "pyi" if !raw.starts_with(' ') && !raw.starts_with('\t') => {
                let rest = line.strip_prefix("async ").unwrap_or(line);
                match_prefixes(rest, &[("def ", NodeKind::Function), ("class ", NodeKind::Class)])
                    .map(|(name, kind)| {
                        let exported = !name.starts_with('_');
                        (name, kind, exported, clean_signature(line.trim_end_matches(':')))
                    })
            }
            "go" if !raw.starts_with(' ') && !raw.starts_with('\t') => {
                if let Some(after) = line.strip_prefix("func ") {
                    let after_recv = if let Some(rest) = after.strip_prefix('(') {
                        rest.split_once(')').map(|(_, r)| r.trim()).unwrap_or(after)
                    } else {
                        after
                    };
                    leading_ident(after_recv).map(|name| {
                        let exported = name.chars().next().is_some_and(char::is_uppercase);
                        (name.to_string(), NodeKind::Function, exported, clean_signature(line.trim_end_matches('{')))
                    })
                } else {
                    match_prefixes(line, &[("type ", NodeKind::Type), ("const ", NodeKind::Const)])
                        .map(|(name, kind)| {
                            let exported = name.chars().next().is_some_and(char::is_uppercase);
                            (name, kind, exported, clean_signature(line.trim_end_matches('{')))
                        })
                }
            }
            "rb" if !raw.starts_with(' ') && !raw.starts_with('\t') => {
                match_prefixes(
                    line,
                    &[("def ", NodeKind::Function), ("class ", NodeKind::Class), ("module ", NodeKind::Module)],
                )
                .map(|(name, kind)| (name, kind, true, clean_signature(line)))
            }
            _ => {
                // Generic top-level heuristic for Java/Kotlin/C/C++/PHP/C#
                let rest = line
                    .strip_prefix("public ")
                    .or_else(|| line.strip_prefix("protected "))
                    .or_else(|| line.strip_prefix("private "))
                    .unwrap_or(line);
                let rest = rest.strip_prefix("static ").unwrap_or(rest);
                match_prefixes(
                    rest,
                    &[
                        ("class ", NodeKind::Class),
                        ("interface ", NodeKind::Type),
                        ("struct ", NodeKind::Type),
                        ("enum ", NodeKind::Type),
                        ("fn ", NodeKind::Function),
                        ("fun ", NodeKind::Function),
                        ("function ", NodeKind::Function),
                    ],
                )
                .map(|(name, kind)| (name, kind, !line.starts_with("private "), clean_signature(line.trim_end_matches('{'))))
            }
        };

        if let Some((name, kind, exported, sig)) = extracted {
            let id = counter.next_id(lang, path, &name);
            nodes.push(Node {
                id,
                kind,
                name,
                path: path.to_string(),
                parent: None,
                span: [line_no, line_no],
                sig_hash: sig_hash(&sig),
                body_hash: body_hash(raw),
                sig,
                exported,
                doc: None,
                owner: None,
                status: NodeStatus::Built,
            });
        }
    }

    ParsedFile {
        path: path.to_string(),
        lang: lang.to_string(),
        blob: blob.to_string(),
        file_id,
        nodes,
        imports: Vec::new(),
        relations: Vec::new(),
        calls: Vec::new(),
    }
}

fn match_prefixes(text: &str, prefixes: &[(&str, NodeKind)]) -> Option<(String, NodeKind)> {
    for &(prefix, kind) in prefixes {
        if let Some(after) = text.strip_prefix(prefix)
            && let Some(ident) = leading_ident(after)
        {
            return Some((ident.to_string(), kind));
        }
    }
    None
}

fn leading_ident(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(trimmed.len());
    if end == 0 { None } else { Some(&trimmed[..end]) }
}
