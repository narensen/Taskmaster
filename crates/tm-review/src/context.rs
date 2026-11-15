//! The context files for a review of arbitrary code, where no plan exists: the
//! architecture is read from the code, and the change log from git.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use tm_verify::ImportGraph;

use crate::git::{Change, Commit};

/// Files per reviewer. More than this and a reviewer skims.
const MAX_FILES_PER_SCOPE: usize = 10;
const MAX_DEPENDENTS: usize = 15;

/// What one reviewer looks at: some changed files and the files that import them.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub name: String,
    pub files: Vec<String>,
    /// Unchanged files that import a changed one, and so may be broken by the change.
    pub dependents: Vec<String>,
}

fn module_of(path: &str) -> &str {
    path.split_once('/').map_or("(root)", |(top, _)| top)
}

/// Group the changed files by top-level directory, a few files per reviewer.
pub fn scopes(changes: &[Change], graph: &ImportGraph) -> Vec<Scope> {
    let mut importers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (from, targets) in &graph.edges {
        for to in targets {
            importers.entry(to).or_default().insert(from);
        }
    }
    let mut by_module: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for change in changes.iter().filter(|c| c.status != 'D') {
        by_module.entry(module_of(&change.path)).or_default().push(&change.path);
    }
    let mut out = Vec::new();
    for (module, files) in by_module {
        let parts = files.len().div_ceil(MAX_FILES_PER_SCOPE);
        for (i, chunk) in files.chunks(MAX_FILES_PER_SCOPE).enumerate() {
            let own: BTreeSet<&str> = chunk.iter().copied().collect();
            let dependents: BTreeSet<&str> = chunk
                .iter()
                .filter_map(|f| importers.get(f))
                .flatten()
                .copied()
                .filter(|f| !own.contains(f))
                .collect();
            let name = if parts > 1 { format!("{module} ({}/{parts})", i + 1) } else { module.into() };
            out.push(Scope {
                name,
                files: chunk.iter().map(|f| f.to_string()).collect(),
                dependents: dependents.into_iter().take(MAX_DEPENDENTS).map(String::from).collect(),
            });
        }
    }
    out
}

/// How the repository fits together, read from the code: its top-level modules, which
/// modules import which, and the files most other files depend on.
pub fn render_architecture(tracked: &[String], graph: &ImportGraph, existing: Option<&str>) -> String {
    let mut out = String::from("# Architecture\n\nGenerated for this review from the code at the reviewed commit.\n");
    if let Some(doc) = existing.map(str::trim).filter(|d| !d.is_empty()) {
        let _ = write!(out, "\n## The project's own description\n\n{doc}\n");
    }

    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for file in tracked {
        *counts.entry(module_of(file)).or_default() += 1;
    }
    let mut uses: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut importer_count: BTreeMap<&str, usize> = BTreeMap::new();
    for (from, targets) in &graph.edges {
        for to in targets {
            *importer_count.entry(to).or_default() += 1;
            if module_of(from) != module_of(to) {
                uses.entry(module_of(from)).or_default().insert(module_of(to));
            }
        }
    }

    out.push_str("\n## Modules\n\n| Module | Files | Imports from |\n|---|---|---|\n");
    for (module, count) in &counts {
        let from = uses.get(module).map(|m| m.iter().copied().collect::<Vec<_>>().join(", "));
        let _ = writeln!(out, "| `{module}` | {count} | {} |", from.unwrap_or_else(|| "-".into()));
    }

    out.push_str("\n## Most depended-on files\n\nA change to one of these reaches the most code.\n\n");
    let mut hubs: Vec<(&str, usize)> = importer_count.into_iter().collect();
    hubs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (file, count) in hubs.iter().take(10) {
        let _ = writeln!(out, "- `{file}` is imported by {count} file(s)");
    }
    if hubs.is_empty() {
        out.push_str("- none found\n");
    }
    out.push_str(
        "\nImports are read from TypeScript and JavaScript files only. For other languages the \
         module table is complete but the import columns are empty.\n",
    );
    out
}

/// What changed in the range under review, newest first.
pub fn render_changes(base: &str, head: &str, changes: &[Change], commits: &[Commit]) -> String {
    let short = |hash: &str| hash.chars().take(10).collect::<String>();
    let mut out = format!(
        "# Changes under review\n\nFrom `{}` to `{}`: {} file(s) in {} commit(s).\n\n## Files\n\n",
        short(base),
        short(head),
        changes.len(),
        commits.len()
    );
    for change in changes {
        let what = match change.status {
            'A' => "added",
            'D' => "deleted",
            _ => "modified",
        };
        let _ = writeln!(out, "- {what}: `{}`", change.path);
    }
    out.push_str("\n## Commits (newest first)\n");
    for commit in commits {
        let _ = write!(out, "\n### `{}` {}\n\n{} · {}\n\n", commit.hash, commit.subject, commit.date, commit.author);
        for file in &commit.files {
            let _ = writeln!(out, "- `{file}`");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> ImportGraph {
        ImportGraph {
            edges: edges
                .iter()
                .map(|(from, to)| (from.to_string(), to.iter().map(|t| t.to_string()).collect()))
                .collect(),
        }
    }

    fn change(status: char, path: &str) -> Change {
        Change { status, path: path.into() }
    }

    #[test]
    fn scopes_group_by_module_and_pull_in_importers() {
        let g = graph(&[
            ("web/page.tsx", &["lib/money.ts"]),
            ("web/cart.tsx", &["lib/money.ts", "lib/tax.ts"]),
            ("lib/tax.ts", &["lib/money.ts"]),
        ]);
        let changes = [change('M', "lib/money.ts"), change('M', "lib/tax.ts"), change('D', "lib/old.ts"), change('A', "README.md")];
        let scopes = scopes(&changes, &g);
        assert_eq!(scopes.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["(root)", "lib"]);
        let lib = &scopes[1];
        // The deleted file is not reviewed, and a changed file is not its own dependent.
        assert_eq!(lib.files, ["lib/money.ts", "lib/tax.ts"]);
        assert_eq!(lib.dependents, ["web/cart.tsx", "web/page.tsx"]);
    }

    #[test]
    fn a_large_module_is_split_between_reviewers() {
        let changes: Vec<Change> = (0..23).map(|i| change('M', &format!("src/f{i:02}.ts"))).collect();
        let scopes = scopes(&changes, &ImportGraph::default());
        assert_eq!(scopes.iter().map(|s| s.files.len()).collect::<Vec<_>>(), [10, 10, 3]);
        assert_eq!(scopes[2].name, "src (3/3)");
    }

    #[test]
    fn architecture_shows_modules_and_hubs() {
        let tracked: Vec<String> = ["web/page.tsx", "web/cart.tsx", "lib/money.ts", "go.mod"].map(String::from).to_vec();
        let g = graph(&[("web/page.tsx", &["lib/money.ts"]), ("web/cart.tsx", &["lib/money.ts"])]);
        let doc = render_architecture(&tracked, &g, Some("Money is integer cents."));
        assert!(doc.contains("Money is integer cents."));
        assert!(doc.contains("| `web` | 2 | lib |"));
        assert!(doc.contains("| `(root)` | 1 | - |"));
        assert!(doc.contains("- `lib/money.ts` is imported by 2 file(s)"));
    }

    #[test]
    fn changes_list_files_and_commits() {
        let commits = [Commit {
            hash: "abc1234".into(),
            date: "2026-11-15T10:00:00Z".into(),
            author: "Test".into(),
            subject: "Round totals".into(),
            files: vec!["lib/money.ts".into()],
        }];
        let doc = render_changes(&"a".repeat(40), &"b".repeat(40), &[change('M', "lib/money.ts"), change('D', "x.ts")], &commits);
        assert!(doc.contains("From `aaaaaaaaaa` to `bbbbbbbbbb`: 2 file(s) in 1 commit(s)."));
        assert!(doc.contains("- modified: `lib/money.ts`") && doc.contains("- deleted: `x.ts`"));
        assert!(doc.contains("### `abc1234` Round totals"));
    }
}
