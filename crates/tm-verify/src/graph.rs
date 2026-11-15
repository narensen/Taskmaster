//! The import graph: which file imports which, read from the code itself.
//!
//! The plan says what each page is supposed to depend on. This says what it actually
//! depends on, which is how a page importing another page's files gets caught.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use tm_core::PageId;

use crate::ownership::Owners;

const EXTENSIONS: [&str; 4] = ["ts", "tsx", "js", "jsx"];
const SKIP_DIRS: [&str; 5] = ["node_modules", ".next", ".git", "dist", ".taskmaster"];

/// `import x from "y"`, `export * from "y"`, `import "y"`, `import("y")`, `require("y")`.
static IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:import|export)\s[^;'"]*?from\s*['"]([^'"]+)['"]|import\s*['"]([^'"]+)['"]|(?:import|require)\s*\(\s*['"]([^'"]+)['"]\s*\)"#,
    )
    .expect("import pattern is valid")
});

#[derive(Debug, Default, PartialEq)]
pub struct ImportGraph {
    /// Importing file to the project files it imports. Package imports are not included.
    pub edges: BTreeMap<String, BTreeSet<String>>,
}

impl ImportGraph {
    /// Build the graph from `path -> source`. Paths are relative to the project root and
    /// use `/`. `alias_root` is what `@/` resolves to (`""` for the project root).
    pub fn build(sources: &BTreeMap<String, String>, alias_root: &str) -> Self {
        let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (file, source) in sources {
            for capture in IMPORT.captures_iter(&strip_comments(source)) {
                let spec = capture.iter().skip(1).flatten().next().map(|m| m.as_str());
                let target = spec.and_then(|s| resolve(file, s, alias_root, sources));
                if let Some(target) = target {
                    edges.entry(file.clone()).or_default().insert(target);
                }
            }
        }
        ImportGraph { edges }
    }

    /// Imports that cross from one page into another, grouped by the importing page. The
    /// text is what the page's worker is given to repair.
    pub fn cross_page_imports(&self, owners: &Owners) -> BTreeMap<PageId, String> {
        let mut out: BTreeMap<PageId, String> = BTreeMap::new();
        for (from, targets) in &self.edges {
            let Some(from_page) = owners.owner(from) else { continue };
            for to in targets {
                match owners.owner(to) {
                    Some(to_page) if to_page != from_page => {
                        let _ = writeln!(
                            out.entry(from_page.clone()).or_default(),
                            "{from} imports {to}, which belongs to page `{to_page}`. Pages may \
                             share code only through contracts."
                        );
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Markdown for the architecture file: what each page really imports from shared code.
    pub fn render(&self, owners: &Owners) -> String {
        let mut by_page: BTreeMap<&PageId, BTreeSet<&str>> = BTreeMap::new();
        for (from, targets) in &self.edges {
            let Some(page) = owners.owner(from) else { continue };
            let shared = targets.iter().filter(|t| owners.owner(t).is_none()).map(String::as_str);
            by_page.entry(page).or_default().extend(shared);
        }
        let mut out = String::from(
            "\n## Actual imports\n\nRead from the code. Each page and the shared files it imports.\n\n",
        );
        for (page, files) in &by_page {
            let list: Vec<String> = files.iter().map(|f| format!("`{f}`")).collect();
            let list = if list.is_empty() { "nothing shared".to_string() } else { list.join(", ") };
            let _ = writeln!(out, "- {page}: {list}");
        }
        if by_page.is_empty() {
            out.push_str("- no page files yet\n");
        }
        out
    }
}

/// Read every source file under `root`, skipping build output and dependencies.
pub fn load_sources(root: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // `file_type` does not follow symlinks, so a link cannot lead the walk elsewhere.
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    walk(root, &path, out)?;
                }
            } else if kind.is_file()
                && path.extension().is_some_and(|e| EXTENSIONS.iter().any(|x| e == *x))
            {
                let relative = path.strip_prefix(root)?.to_string_lossy().replace('\\', "/");
                out.insert(relative, std::fs::read_to_string(&path)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

/// Blank out `//` and `/* */` comments so a commented-out import is not counted. String
/// contents are kept, because import specifiers are strings.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                out.push_str("  ");
                continue;
            }
            if c == q {
                quote = None;
            }
        } else if matches!(c, b'"' | b'\'' | b'`') {
            quote = Some(c);
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            out.push(' ');
            continue;
        }
        // Copy whole characters so non-ASCII text survives.
        let len = source[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&source[i..i + len]);
        i += len;
    }
    out
}

/// Turn an import specifier into a project file, or `None` for a package or a missing file.
fn resolve(
    from: &str,
    spec: &str,
    alias_root: &str,
    sources: &BTreeMap<String, String>,
) -> Option<String> {
    let base = if let Some(rest) = spec.strip_prefix("@/") {
        if alias_root.is_empty() { rest.to_string() } else { format!("{alias_root}/{rest}") }
    } else if spec.starts_with("./") || spec.starts_with("../") {
        let dir = from.rsplit_once('/').map_or("", |(dir, _)| dir);
        format!("{dir}/{spec}")
    } else {
        return None;
    };
    let base = normalize(&base)?;
    let mut candidates = vec![base.clone()];
    candidates.extend(EXTENSIONS.iter().map(|e| format!("{base}.{e}")));
    candidates.extend(EXTENSIONS.iter().map(|e| format!("{base}/index.{e}")));
    candidates.into_iter().find(|c| sources.contains_key(c))
}

/// Resolve `.` and `..`. `None` if the path climbs out of the project.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::test_plan;

    fn sources(files: &[(&str, &str)]) -> BTreeMap<String, String> {
        files.iter().map(|(p, s)| (p.to_string(), s.to_string())).collect()
    }

    fn imports<'a>(graph: &'a ImportGraph, file: &str) -> Vec<&'a str> {
        graph.edges.get(file).map(|t| t.iter().map(String::as_str).collect()).unwrap_or_default()
    }

    #[test]
    fn resolves_every_import_form() {
        let src = sources(&[
            (
                "app/a/page.tsx",
                "import { User } from \"@/contracts/types\";\n\
                 import {\n  List,\n  Row\n} from './parts/list';\n\
                 import '../globals';\n\
                 export * from \"./helpers\";\n\
                 const lazy = import('./lazy');\n\
                 const old = require(\"./legacy\");\n\
                 import React from 'react';",
            ),
            ("contracts/types.ts", ""),
            ("app/a/parts/list.tsx", ""),
            ("app/globals.ts", ""),
            ("app/a/helpers/index.ts", ""),
            ("app/a/lazy.tsx", ""),
            ("app/a/legacy.js", ""),
        ]);
        let graph = ImportGraph::build(&src, "");
        assert_eq!(
            imports(&graph, "app/a/page.tsx"),
            vec![
                "app/a/helpers/index.ts",
                "app/a/lazy.tsx",
                "app/a/legacy.js",
                "app/a/parts/list.tsx",
                "app/globals.ts",
                "contracts/types.ts"
            ]
        );
    }

    #[test]
    fn ignores_commented_out_imports_packages_and_missing_files() {
        let src = sources(&[
            (
                "app/a/page.tsx",
                "// import x from '../b/secret';\n\
                 /* import y from '../b/secret'; */\n\
                 import z from 'lodash';\n\
                 import w from './missing';\n\
                 const url = 'https://example.com/a'; // not a comment start inside the string\n\
                 import real from './real';",
            ),
            ("app/b/secret.ts", ""),
            ("app/a/real.ts", ""),
        ]);
        let graph = ImportGraph::build(&src, "");
        assert_eq!(imports(&graph, "app/a/page.tsx"), vec!["app/a/real.ts"]);
    }

    #[test]
    fn honours_the_alias_root_and_refuses_to_leave_the_project() {
        let src = sources(&[
            ("src/app/a/page.tsx", "import t from '@/lib/t'; import o from '../../../../etc/passwd';"),
            ("src/lib/t.ts", ""),
        ]);
        let graph = ImportGraph::build(&src, "src");
        assert_eq!(imports(&graph, "src/app/a/page.tsx"), vec!["src/lib/t.ts"]);
    }

    #[test]
    fn reports_a_page_importing_another_page() {
        let src = sources(&[
            ("app/a/page.tsx", "import { fmt } from '../b/format'; import { T } from '@/contracts/types';"),
            ("app/b/format.ts", "import { T } from '@/contracts/types';"),
            ("app/b/page.tsx", "import { fmt } from './format';"),
            ("contracts/types.ts", ""),
        ]);
        let owners = Owners::from_plan(&test_plan(&[("a", "app/a/**"), ("b", "app/b/**")])).unwrap();
        let graph = ImportGraph::build(&src, "");
        let violations = graph.cross_page_imports(&owners);
        // `b` importing its own file and both pages importing a contract are fine.
        assert_eq!(violations.keys().collect::<Vec<_>>(), vec!["a"]);
        assert!(violations["a"].contains("app/a/page.tsx imports app/b/format.ts"));
        assert!(violations["a"].contains("page `b`"));
    }

    #[test]
    fn renders_what_each_page_really_imports() {
        let src = sources(&[
            ("app/a/page.tsx", "import { T } from '@/contracts/types'; import './local';"),
            ("app/a/local.ts", ""),
            ("contracts/types.ts", ""),
        ]);
        let owners = Owners::from_plan(&test_plan(&[("a", "app/a/**")])).unwrap();
        let doc = ImportGraph::build(&src, "").render(&owners);
        assert!(doc.contains("## Actual imports"));
        // The page's own files are not listed; only what it takes from shared code.
        assert!(doc.contains("- a: `contracts/types.ts`"));
        assert!(!doc.contains("local.ts"));
    }

    #[test]
    fn loads_sources_from_disk_and_skips_dependencies() {
        let root = std::env::temp_dir().join(format!("tm-verify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["app/a", "node_modules/pkg", ".next"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("app/a/page.tsx"), "import x from './x';").unwrap();
        std::fs::write(root.join("app/a/x.ts"), "").unwrap();
        std::fs::write(root.join("app/a/notes.md"), "").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::write(root.join(".next/build.js"), "").unwrap();
        let loaded = load_sources(&root).unwrap();
        assert_eq!(loaded.keys().collect::<Vec<_>>(), vec!["app/a/page.tsx", "app/a/x.ts"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
