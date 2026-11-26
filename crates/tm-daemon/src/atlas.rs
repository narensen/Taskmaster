//! Living Codebase Atlas: structural index of modules, languages, import hubs, git churn
//! hotspots, test-to-source links, and detected conventions.
//!
//! Cached per git HEAD commit at `.taskmaster/atlas.json` so planners and the web Knowledge
//! Hub read rich architectural intuition in milliseconds without scanning on every call.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

const MAX_FILES: usize = 3000;
const MAX_FILE_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LanguageStat {
    pub language: String,
    pub files: usize,
    pub lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleCluster {
    pub name: String,
    pub files: usize,
    pub lines: usize,
    pub exports: usize,
    pub tests: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HubFile {
    pub path: String,
    pub imports_in: usize,
    pub exports: usize,
    pub lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChurnFile {
    pub path: String,
    pub touches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestLink {
    pub module: String,
    pub test_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Atlas {
    pub head: Option<String>,
    pub generated_at: String,
    pub total_files: usize,
    pub total_lines: usize,
    pub languages: Vec<LanguageStat>,
    pub modules: Vec<ModuleCluster>,
    pub hubs: Vec<HubFile>,
    pub hotspots: Vec<ChurnFile>,
    pub test_map: Vec<TestLink>,
    pub conventions: Vec<String>,
}

fn cache_path(project: &Path) -> PathBuf {
    project.join(".taskmaster").join("atlas.json")
}

fn git_head(project: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(project)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

fn git_hotspots(project: &Path) -> Vec<ChurnFile> {
    let out = match Command::new("git")
        .args(["log", "-n", "40", "--name-only", "--pretty=format:"])
        .current_dir(project)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let p = line.trim();
        if p.is_empty() || p.starts_with(".taskmaster/") {
            continue;
        }
        if project.join(p).is_file() {
            *counts.entry(p.to_string()).or_insert(0) += 1;
        }
    }
    let mut list: Vec<ChurnFile> = counts
        .into_iter()
        .map(|(path, touches)| ChurnFile { path, touches })
        .collect();
    list.sort_by(|a, b| b.touches.cmp(&a.touches).then_with(|| a.path.cmp(&b.path)));
    list.truncate(12);
    list
}

fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".taskmaster"
            | ".turbo"
            | ".cache"
            | "coverage"
    )
}

fn language_of(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str())?.to_ascii_lowercase().as_str() {
        "ts" | "tsx" | "mts" | "cts" => Some("TypeScript"),
        "js" | "jsx" | "mjs" | "cjs" => Some("JavaScript"),
        "rs" => Some("Rust"),
        "py" => Some("Python"),
        "go" => Some("Go"),
        "css" | "scss" => Some("CSS"),
        "html" => Some("HTML"),
        "json" => Some("JSON"),
        "toml" | "yaml" | "yml" => Some("Config"),
        "md" => Some("Markdown"),
        "sh" => Some("Shell"),
        _ => None,
    }
}

fn is_code_lang(lang: &str) -> bool {
    matches!(lang, "TypeScript" | "JavaScript" | "Rust" | "Python" | "Go")
}

fn is_test_path(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    lower.contains(".test.")
        || lower.contains(".spec.")
        || lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.ends_with("/tests.rs")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.contains("/test_")
        || lower.starts_with("test_")
        || lower.contains("/e2e/")
}

fn count_exports(lang: &str, text: &str) -> usize {
    let mut n = 0;
    for line in text.lines() {
        let t = line.trim_start();
        let hit = match lang {
            "TypeScript" | "JavaScript" => t.starts_with("export "),
            "Rust" => {
                t.starts_with("pub fn ")
                    || t.starts_with("pub async fn ")
                    || t.starts_with("pub struct ")
                    || t.starts_with("pub enum ")
                    || t.starts_with("pub trait ")
                    || t.starts_with("pub type ")
            }
            "Python" => t.starts_with("def ") && !t.starts_with("def _") || t.starts_with("class "),
            "Go" => t.starts_with("func ") || t.starts_with("type "),
            _ => false,
        };
        if hit {
            n += 1;
        }
    }
    n
}

fn extract_import_stems(text: &str) -> Vec<String> {
    let mut stems = Vec::new();
    for line in text.lines().take(120) {
        let t = line.trim();
        if (t.starts_with("import ") || t.starts_with("export ")) && t.contains("from ") {
            if let Some(quoted) = t.split("from ").nth(1) {
                let cleaned = quoted
                    .trim()
                    .trim_end_matches(';')
                    .trim_matches(|c| c == '\'' || c == '"');
                if cleaned.starts_with('.')
                    && let Some(stem) = Path::new(cleaned).file_stem().and_then(|s| s.to_str())
                {
                    stems.push(stem.to_string());
                }
            }
        } else if t.starts_with("use crate::") {
            let rest = t.trim_start_matches("use crate::");
            if let Some(seg) = rest.split("::").next() {
                let seg = seg.trim_end_matches(';').trim();
                if !seg.is_empty() && !seg.starts_with('{') {
                    stems.push(seg.to_string());
                }
            }
        } else if let Some(rest) = t.strip_prefix("from ")
            && let Some((mod_path, _)) = rest.split_once(" import ")
        {
            let mod_path = mod_path.trim().trim_start_matches('.');
            if let Some(stem) = mod_path.rsplit('.').next().filter(|s| !s.is_empty()) {
                stems.push(stem.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("import ")
            && !t.contains("from ")
            && !t.contains('{')
        {
            for part in rest.split(',') {
                let mod_path = part.split(" as ").next().unwrap_or("").trim();
                if let Some(stem) = mod_path.rsplit('.').next().filter(|s| !s.is_empty()) {
                    stems.push(stem.to_string());
                }
            }
        }
    }
    stems
}

fn module_bucket(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() >= 3 && (parts[0] == "crates" || parts[0] == "packages" || parts[0] == "apps") {
        format!("{}/{}", parts[0], parts[1])
    } else if parts.len() >= 2 {
        parts[0].to_string()
    } else {
        "root".to_string()
    }
}

fn detect_conventions(project: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    if project.join("Cargo.toml").is_file() {
        notes.push("Rust Cargo workspace".into());
    }
    if project.join("package.json").is_file() || project.join("web/package.json").is_file() {
        notes.push("Node / npm frontend".into());
    }
    if project.join("tsconfig.json").is_file() || project.join("web/tsconfig.json").is_file() {
        notes.push("TypeScript project".into());
    }
    if project.join("pyproject.toml").is_file() || project.join("requirements.txt").is_file() {
        notes.push("Python project".into());
    }
    if project.join("playwright.config.ts").is_file() || project.join("web/playwright.config.ts").is_file() {
        notes.push("Playwright e2e suite".into());
    }
    if project.join("AGENTS.md").is_file() {
        notes.push("Repository rules in AGENTS.md".into());
    }
    if project.join("taskmaster.policy.toml").is_file() || project.join(".taskmaster/policy.json").is_file() {
        notes.push("Policy-as-Code guardrails enabled".into());
    }
    notes
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_FILES {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut list: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    list.sort();
    for path in list {
        if out.len() >= MAX_FILES {
            break;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !skip_dir(name) {
                walk_files(&path, out);
            }
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Build the atlas freshly from disk.
pub fn build(project: &Path) -> Atlas {
    let mut files = Vec::new();
    walk_files(project, &mut files);

    let mut total_files = 0usize;
    let mut total_lines = 0usize;
    let mut langs: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut mods: BTreeMap<String, ModuleCluster> = BTreeMap::new();
    let mut test_buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();

    // Per-file stats for hub ranking
    let mut file_meta: Vec<(String, String, usize, usize)> = Vec::new(); // (rel, stem, exports, lines)
    let mut stem_imports: HashMap<String, usize> = HashMap::new();

    for full in files {
        let Ok(rel_path) = full.strip_prefix(project) else { continue };
        let rel = rel_path.to_string_lossy().replace('\\', "/");
        let Some(lang) = language_of(&full) else { continue };
        let meta = match std::fs::metadata(&full) {
            Ok(m) if m.len() <= MAX_FILE_BYTES => m,
            _ => continue,
        };
        let _ = meta;
        let Ok(text) = std::fs::read_to_string(&full) else { continue };
        let lines = text.lines().count();
        total_files += 1;
        total_lines += lines;

        let entry = langs.entry(lang.to_string()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += lines;

        let is_test = is_test_path(&rel);
        let exports = if is_code_lang(lang) { count_exports(lang, &text) } else { 0 };
        let bucket = module_bucket(&rel);
        let cluster = mods.entry(bucket.clone()).or_insert_with(|| ModuleCluster {
            name: bucket.clone(),
            files: 0,
            lines: 0,
            exports: 0,
            tests: 0,
        });
        cluster.files += 1;
        cluster.lines += lines;
        cluster.exports += exports;
        if is_test {
            cluster.tests += 1;
            let list = test_buckets.entry(bucket).or_default();
            if list.len() < 8 {
                list.push(rel.clone());
            }
        }

        if is_code_lang(lang) && !is_test {
            for stem in extract_import_stems(&text) {
                *stem_imports.entry(stem).or_insert(0) += 1;
            }
            let stem = full.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            file_meta.push((rel, stem, exports, lines));
        }
    }

    let mut languages: Vec<LanguageStat> = langs
        .into_iter()
        .map(|(language, (files, lines))| LanguageStat { language, files, lines })
        .collect();
    languages.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.language.cmp(&b.language)));

    let mut modules: Vec<ModuleCluster> = mods.into_values().collect();
    modules.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.name.cmp(&b.name)));
    modules.truncate(24);

    let mut hubs: Vec<HubFile> = file_meta
        .into_iter()
        .map(|(path, stem, exports, lines)| {
            let imports_in = stem_imports.get(&stem).copied().unwrap_or(0);
            HubFile { path, imports_in, exports, lines }
        })
        .filter(|h| h.imports_in > 0 || h.exports >= 3)
        .collect();
    hubs.sort_by(|a, b| {
        (b.imports_in * 3 + b.exports)
            .cmp(&(a.imports_in * 3 + a.exports))
            .then_with(|| a.path.cmp(&b.path))
    });
    hubs.truncate(15);

    let test_map: Vec<TestLink> = test_buckets
        .into_iter()
        .map(|(module, test_files)| TestLink { module, test_files })
        .collect();

    Atlas {
        head: git_head(project),
        generated_at: crate::time::rfc3339_now(),
        total_files,
        total_lines,
        languages,
        modules,
        hubs,
        hotspots: git_hotspots(project),
        test_map,
        conventions: detect_conventions(project),
    }
}

/// Return the cached atlas if it matches the current git HEAD, otherwise rebuild and cache it.
pub fn build_or_load(project: &Path, force_refresh: bool) -> Atlas {
    let head = git_head(project);
    let path = cache_path(project);
    if !force_refresh
        && head.is_some()
        && let Ok(raw) = std::fs::read_to_string(&path)
        && let Ok(cached) = serde_json::from_str::<Atlas>(&raw)
        && cached.head == head
    {
        return cached;
    }
    let atlas = build(project);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&atlas) {
        let _ = std::fs::write(&path, text);
    }
    atlas
}

impl Atlas {
    /// Render a compact (~1.5–2 KB) architectural summary for brownfield planners covering
    /// languages, conventions, module clusters, import hubs, churn hotspots, and test mapping.
    pub fn render_for_planner(&self, max_chars: usize) -> String {
        if self.total_files == 0 || max_chars < 120 {
            return String::new();
        }
        let mut lines: Vec<String> = Vec::new();
        let langs: Vec<String> = self
            .languages
            .iter()
            .take(5)
            .map(|l| format!("{} ({}f/{}L)", l.language, l.files, l.lines))
            .collect();
        lines.push(format!(
            "## Codebase Atlas ({} files, {} lines{})",
            self.total_files,
            self.total_lines,
            if langs.is_empty() { String::new() } else { format!("; {}", langs.join(", ")) }
        ));
        if !self.conventions.is_empty() {
            lines.push(format!("Conventions: {}", self.conventions.join(", ")));
        }
        if !self.modules.is_empty() {
            let mods: Vec<String> = self
                .modules
                .iter()
                .take(8)
                .map(|m| format!("{} ({}f/{}L, {} exp, {} tests)", m.name, m.files, m.lines, m.exports, m.tests))
                .collect();
            lines.push(format!("Modules: {}", mods.join("; ")));
        }
        if !self.hubs.is_empty() {
            let hubs: Vec<String> = self
                .hubs
                .iter()
                .take(8)
                .map(|h| format!("{} ({} in, {} exp, {}L)", h.path, h.imports_in, h.exports, h.lines))
                .collect();
            lines.push(format!("Import hubs: {}", hubs.join("; ")));
        }
        if !self.hotspots.is_empty() {
            let hot: Vec<String> = self
                .hotspots
                .iter()
                .take(6)
                .map(|c| format!("{} ({}x)", c.path, c.touches))
                .collect();
            lines.push(format!("Recent hotspots: {}", hot.join(", ")));
        }
        if !self.test_map.is_empty() {
            let tmap: Vec<String> = self
                .test_map
                .iter()
                .take(6)
                .map(|t| {
                    let files: Vec<&str> = t.test_files.iter().take(4).map(String::as_str).collect();
                    let extra = t.test_files.len().saturating_sub(files.len());
                    if extra > 0 {
                        format!("{}: {} (+{extra})", t.module, files.join(", "))
                    } else {
                        format!("{}: {}", t.module, files.join(", "))
                    }
                })
                .collect();
            lines.push(format!("Test map: {}", tmap.join("; ")));
        }

        clip_lines(&lines.join("\n"), max_chars)
    }
}

/// Trim newline-separated prompt text to at most `max_chars` bytes on line boundaries.
/// If even the first line exceeds `max_chars`, clips it safely on a UTF-8 character boundary.
/// Shared by [`Atlas::render_for_planner`] and [`crate::scout`] so bounded prompt sections
/// never return empty on an oversized first line or panic on multibyte UTF-8.
pub(crate) fn clip_lines(text: &str, max_chars: usize) -> String {
    if max_chars == 0 || text.is_empty() {
        return String::new();
    }
    if text.len() <= max_chars {
        return if text.ends_with('\n') { text.to_string() } else { format!("{text}\n") };
    }
    let mut out = String::new();
    for line in text.lines() {
        if out.len() + line.len() + 1 > max_chars {
            if out.is_empty() {
                let mut end = max_chars.saturating_sub(1);
                while end > 0 && !line.is_char_boundary(end) {
                    end -= 1;
                }
                out.push_str(&line[..end]);
                out.push('\n');
            }
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atlas_indexes_modules_languages_hubs_and_tests() {
        let dir = std::env::temp_dir().join(format!("tm-atlas-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/auth")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        std::fs::write(
            dir.join("src/auth/session.ts"),
            "export interface Session { id: string }\nexport function verify() { return true; }\nexport const TTL = 60;\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/index.ts"),
            "import { verify } from './auth/session';\nexport function main() { return verify(); }\n",
        )
        .unwrap();
        std::fs::write(dir.join("tests/auth.spec.ts"), "test('ok', () => {});\n").unwrap();

        let atlas = build(&dir);
        assert!(atlas.total_files >= 3, "{atlas:?}");
        assert!(atlas.languages.iter().any(|l| l.language == "TypeScript" && l.files == 3));
        assert!(atlas.hubs.iter().any(|h| h.path == "src/auth/session.ts" && h.imports_in >= 1));
        assert!(atlas.test_map.iter().any(|t| t.test_files.iter().any(|f| f.contains("auth.spec.ts"))));
        assert!(atlas.conventions.iter().any(|c| c.contains("Node")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tb18_atlas_render_for_planner_is_compact_and_covers_hubs_and_tests() {
        let dir = std::env::temp_dir().join(format!("tm-atlas-planner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"demo\"\n").unwrap();
        std::fs::write(
            dir.join("pkg/storage.py"),
            "class Storage:\n    pass\ndef open_db(): pass\ndef close_db(): pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pkg/cli.py"),
            "from pkg.storage import Storage, open_db\ndef main(): return open_db()\n",
        )
        .unwrap();
        std::fs::write(dir.join("tests/test_storage.py"), "def test_open(): pass\n").unwrap();

        let atlas = build(&dir);
        let rendered = atlas.render_for_planner(2_000);
        assert!(rendered.contains("## Codebase Atlas"), "{rendered}");
        assert!(rendered.contains("Python"), "{rendered}");
        assert!(rendered.contains("Import hubs:"), "{rendered}");
        assert!(rendered.contains("pkg/storage.py"), "{rendered}");
        assert!(rendered.contains("Test map:"), "{rendered}");
        assert!(rendered.contains("tests/test_storage.py"), "{rendered}");
        assert!(rendered.len() <= 2_000, "rendered len {} exceeded cap", rendered.len());

        let tight = atlas.render_for_planner(180);
        assert!(tight.len() <= 180, "tight len {} exceeded 180:\n{tight}", tight.len());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tb18_clip_lines_handles_multibyte_utf8_and_oversized_first_line() {
        let long_utf8 = "—".repeat(40);
        let clipped = clip_lines(&long_utf8, 16);
        assert!(!clipped.is_empty(), "oversized first line must not collapse to empty");
        assert!(clipped.len() <= 16, "clipped bytes {} exceeded 16", clipped.len());
        assert!(clipped.ends_with('\n'));
        assert_eq!(clip_lines("line1\nline2", 64), "line1\nline2\n");
    }
}
