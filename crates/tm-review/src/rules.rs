//! Review rules written by the project: one project-wide file plus one per directory.
//!
//! A directory's `REVIEW.md` applies to every file below it, so a rule can be scoped to the
//! part of the codebase it is about.

use std::collections::BTreeSet;
use std::path::Path;

use crate::{NESTED_RULES, ROOT_RULES};

/// Rules are instructions to the reviewer, so their total size is capped.
const MAX_RULES_BYTES: usize = 24_000;

#[derive(Debug, Clone, PartialEq)]
pub struct RuleFile {
    pub path: String,
    pub text: String,
}

/// The rule files that apply to `files`: the project-wide file first, then each directory
/// file from the outermost directory inwards. Read from `root`, the review checkout.
pub fn rules_for(root: &Path, files: &[String]) -> Vec<RuleFile> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for file in files {
        let mut dir = file.as_str();
        while let Some((parent, _)) = dir.rsplit_once('/') {
            paths.insert(format!("{parent}/{NESTED_RULES}"));
            dir = parent;
        }
    }
    let mut ordered = vec![ROOT_RULES.to_string(), NESTED_RULES.to_string()];
    // Outermost directory first, so the most specific rule is read last.
    let mut nested: Vec<String> = paths.into_iter().collect();
    nested.sort_by_key(|p| (p.matches('/').count(), p.clone()));
    ordered.extend(nested);

    let mut out = Vec::new();
    let mut budget = MAX_RULES_BYTES;
    for path in ordered {
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else { continue };
        let text = text.trim();
        if text.is_empty() || text.len() > budget {
            continue;
        }
        budget -= text.len();
        out.push(RuleFile { path, text: text.to_string() });
    }
    out
}

pub fn render(rules: &[RuleFile]) -> String {
    let mut out = String::from("# Review rules\n\nWritten by this project. A change that breaks one is a finding.\n");
    for rule in rules {
        let scope = match rule.path.rsplit_once('/') {
            Some((dir, _)) if rule.path != ROOT_RULES => format!("files under `{dir}/`"),
            _ => "the whole project".to_string(),
        };
        out.push_str(&format!("\n## `{}` (applies to {scope})\n\n{}\n", rule.path, rule.text));
    }
    if rules.is_empty() {
        out.push_str("\nThis project has no review rules.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("tm-rules-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, text) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, text).unwrap();
        }
        root
    }

    #[test]
    fn collects_project_rules_then_directory_rules_outermost_first() {
        let root = tree("nested", &[
            (".taskmaster/REVIEW.md", "All dates are UTC."),
            ("src/REVIEW.md", "No default exports."),
            ("src/Api/REVIEW.md", "Every handler checks the session."),
            ("src/ui/REVIEW.md", "Not relevant to the change."),
            ("docs/REVIEW.md", "Not relevant either."),
        ]);
        let rules = rules_for(&root, &["src/Api/users/list.ts".into()]);
        let paths: Vec<&str> = rules.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, [".taskmaster/REVIEW.md", "src/REVIEW.md", "src/Api/REVIEW.md"]);
        let doc = render(&rules);
        assert!(doc.contains("applies to the whole project"));
        assert!(doc.contains("`src/Api/REVIEW.md` (applies to files under `src/Api/`)"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_rules_and_oversized_rules_are_handled() {
        let big = "x".repeat(MAX_RULES_BYTES + 1);
        let root = tree("big", &[("REVIEW.md", &big), ("a/REVIEW.md", "Small rule.")]);
        let rules = rules_for(&root, &["a/b.ts".into()]);
        assert_eq!(rules.len(), 1, "the oversized file is skipped, the small one kept");
        assert!(render(&[]).contains("no review rules"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
