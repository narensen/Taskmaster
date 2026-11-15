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

use crate::languages;
use crate::ownership::Owners;

const SCRIPTS: [&str; 6] = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
const EXTENSIONS: [&str; 26] = [
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "rs", "py", "go", "java", "kt", "kts", "c", "h", "cc", "cpp",
    "cxx", "hh", "hpp", "hxx", "rb", "php", "cs", "css", "scss", "sass",
];
const SKIP_DIRS: [&str; 10] =
    ["node_modules", ".next", ".git", "dist", ".taskmaster", "target", "vendor", "__pycache__", ".venv", "venv"];

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
        let parsed: BTreeMap<String, String> =
            sources.iter().map(|(file, source)| (file.clone(), strip_for(file, source))).collect();
        let rust = languages::rust_modules(&parsed);
        let csharp = languages::csharp_namespaces(&parsed);
        for (file, source) in &parsed {
            if file == "go.mod" || file.ends_with("/go.mod") {
                continue;
            }
            let mut targets = BTreeSet::new();
            if is_javascript(file) {
                for capture in IMPORT.captures_iter(source) {
                    let spec = capture.iter().skip(1).flatten().next().map(|m| m.as_str());
                    if let Some(target) = spec.and_then(|s| resolve(file, s, alias_root, &parsed)) {
                        targets.insert(target);
                    }
                }
            }
            targets.extend(languages::connections(file, source, &parsed, &rust, &csharp));
            if !targets.is_empty() {
                edges.insert(file.clone(), targets);
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
            } else if kind.is_file() && is_loaded(&name, &path) {
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

fn is_javascript(file: &str) -> bool {
    matches!(extension_of(file), Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs"))
}

fn extension_of(path: &str) -> Option<&str> {
    path.rsplit_once('.').map(|(_, ext)| ext)
}

fn is_loaded(name: &str, path: &Path) -> bool {
    name == "go.mod" || path.extension().is_some_and(|e| EXTENSIONS.iter().any(|x| e == *x))
}

fn strip_for(file: &str, source: &str) -> String {
    match extension_of(file) {
        Some("py" | "rb") => strip_comments(source, true, file.ends_with(".py")),
        Some("php") => strip_comments(source, true, false),
        _ => strip_comments(source, false, false),
    }
}

/// Blank out comments so a commented-out import is not counted. String contents are kept,
/// because import specifiers are strings. `hash` treats `#` as a comment. `triple` keeps
/// Python docstrings intact.
fn strip_comments(source: &str, hash: bool, triple: bool) -> String {
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
            if triple && i + 2 < bytes.len() && bytes[i] == q && bytes[i + 1] == q && bytes[i + 2] == q {
                quote = None;
                i += 3;
                out.push_str("   ");
                continue;
            }
            if c == q && !(triple && bytes.get(i + 1) == Some(&q)) {
                quote = None;
            }
        } else if triple && (source[i..].starts_with("\"\"\"") || source[i..].starts_with("'''")) {
            quote = Some(bytes[i]);
            i += 2;
            out.push_str("  ");
        } else if matches!(c, b'"' | b'\'' | b'`') {
            quote = Some(c);
        } else if hash && c == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
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
    candidates.extend(SCRIPTS.iter().map(|e| format!("{base}.{e}")));
    candidates.extend(SCRIPTS.iter().map(|e| format!("{base}/index.{e}")));
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

    #[test]
    fn connects_rust_python_go_and_the_other_languages() {
        let src = sources(&[
            ("src/lib.rs", "mod foo;\npub mod bar;\n// mod hidden;"),
            ("src/foo.rs", "use super::bar::Item;\nmod baz;\ninclude!(\"./foo/extra.rs\");"),
            ("src/foo/baz.rs", ""),
            ("src/foo/extra.rs", ""),
            ("src/bar.rs", "use crate::foo::baz::Thing;"),
            ("src/hidden.rs", ""),
            ("pkg/a.py", "from . import b\nfrom .c import d\nimport pkg.sub.e"),
            ("pkg/b.py", ""),
            ("pkg/c.py", ""),
            ("pkg/sub/e.py", ""),
            ("go.mod", "module example.com/app\n\ngo 1.22\n"),
            ("main.go", "import \"example.com/app/util\"\n"),
            ("util/util.go", "package util\n"),
            ("src/com/acme/A.java", "import com.acme.B;"),
            ("src/com/acme/B.java", ""),
            ("src/a.c", "#include \"b.h\"\n#include <stdio.h>\n"),
            ("src/b.h", ""),
            ("lib/a.rb", "require_relative \"b\"\nrequire \"c\""),
            ("lib/b.rb", ""),
            ("lib/c.rb", ""),
            ("src/a.php", "require 'b.php';\nuse App\\Model;"),
            ("src/b.php", ""),
            ("src/App/Model.php", ""),
            ("src/A.cs", "namespace App.One;\nusing App.Two;"),
            ("src/B.cs", "namespace App.Two;"),
            ("web/a.css", "@import \"./b.css\";"),
            ("web/b.css", ""),
        ]);
        let graph = ImportGraph::build(&src, "");
        assert_eq!(imports(&graph, "src/lib.rs"), vec!["src/bar.rs", "src/foo.rs"]);
        assert_eq!(imports(&graph, "src/foo.rs"), vec!["src/bar.rs", "src/foo/baz.rs", "src/foo/extra.rs"]);
        assert_eq!(imports(&graph, "src/bar.rs"), vec!["src/foo/baz.rs"]);
        assert!(graph.edges.get("src/hidden.rs").is_none());
        assert_eq!(imports(&graph, "pkg/a.py"), vec!["pkg/b.py", "pkg/c.py", "pkg/sub/e.py"]);
        assert_eq!(imports(&graph, "main.go"), vec!["util/util.go"]);
        assert_eq!(imports(&graph, "src/com/acme/A.java"), vec!["src/com/acme/B.java"]);
        assert_eq!(imports(&graph, "src/a.c"), vec!["src/b.h"]);
        assert_eq!(imports(&graph, "lib/a.rb"), vec!["lib/b.rb", "lib/c.rb"]);
        assert_eq!(imports(&graph, "src/a.php"), vec!["src/App/Model.php", "src/b.php"]);
        assert_eq!(imports(&graph, "src/A.cs"), vec!["src/B.cs"]);
        assert_eq!(imports(&graph, "web/a.css"), vec!["web/b.css"]);
    }
}
