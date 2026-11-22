//! Compact, deterministic repository map and symbol extractor.
//!
//! Built once in Rust (zero LLM tokens), cached by git tree hash, and placed inside the
//! shared prompt prefix so the planner and every parallel worker skip redundant `ls`/`grep`
//! discovery turns while hitting the provider's prompt cache.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Maximum byte budget for the prompt-embedded repo map (~1.5k tokens).
const MAX_MAP_BYTES: usize = 6_000;
/// Maximum symbols extracted per file in the compact map.
const MAX_SYMBOLS_PER_FILE: usize = 12;
/// Maximum routes listed in the compact map header.
const MAX_ROUTES: usize = 24;
/// Maximum file size scanned for symbols/routes (avoid minified/generated blobs).
const MAX_SCAN_BYTES: u64 = 128 * 1024;
/// Maximum cached repo maps retained in memory.
const MAX_CACHE_ENTRIES: usize = 64;

const IGNORED_DIRS: [&str; 10] = [
    ".git/",
    ".taskmaster/",
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    ".next/",
    ".venv/",
    "venv/",
    "__pycache__/",
];

const CODE_EXTS: [&str; 14] = [
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".go", ".java", ".kt", ".rb", ".c", ".cpp",
];

static MAP_CACHE: OnceLock<Mutex<HashMap<(PathBuf, String), String>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<(PathBuf, String), String>> {
    MAP_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Deterministic fingerprint for `tree`: `HEAD^{tree}` plus a hash of any uncommitted paths'
/// statuses, sizes, and mtimes. Returns `None` when git cannot inspect `tree`.
fn tree_cache_key(tree: &Path) -> Option<(PathBuf, String)> {
    let head_tree = std::process::Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["rev-parse", "--verify", "HEAD^{tree}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "no-head".to_string());

    let status_out = std::process::Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .ok()?;
    if !status_out.status.success() {
        return None;
    }

    let status = String::from_utf8_lossy(&status_out.stdout);
    let canonical = tree.canonicalize().unwrap_or_else(|_| tree.to_path_buf());
    if status.trim().is_empty() && head_tree != "no-head" {
        return Some((canonical, head_tree));
    }

    let mut hasher = DefaultHasher::new();
    head_tree.hash(&mut hasher);
    for line in status.lines() {
        line.hash(&mut hasher);
        if let Some(rel) = line.get(3..).map(str::trim) {
            let full = tree.join(rel);
            if let Ok(meta) = std::fs::metadata(&full) {
                meta.len().hash(&mut hasher);
                if let Ok(modified) = meta.modified()
                    && let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH)
                {
                    dur.as_nanos().hash(&mut hasher);
                }
            }
        }
    }
    Some((canonical, format!("{head_tree}:{:016x}", hasher.finish())))
}

fn tracked_files(tree: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|p| !p.is_empty() && !IGNORED_DIRS.iter().any(|dir| p.starts_with(dir) || p.contains(&format!("/{dir}"))))
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    files
}

fn is_code_file(path: &str) -> bool {
    CODE_EXTS.iter().any(|ext| path.ends_with(ext))
}

/// Extract top-level symbol declarations from one file's source text.
pub fn file_symbols(path: &str, text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let is_rust = path.ends_with(".rs");
    let is_py = path.ends_with(".py");
    let is_go = path.ends_with(".go");
    for (idx, raw) in text.lines().enumerate() {
        if out.len() >= limit {
            break;
        }
        // Only inspect top-level or lightly indented declarations so nested helpers don't bloat the map.
        let indent = raw.len() - raw.trim_start().len();
        if indent > 4 {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') || line.starts_with("/*") || line.starts_with('*') {
            continue;
        }
        let matched = if is_rust {
            line.starts_with("pub fn ")
                || line.starts_with("pub async fn ")
                || line.starts_with("pub struct ")
                || line.starts_with("pub enum ")
                || line.starts_with("pub trait ")
                || line.starts_with("pub type ")
                || line.starts_with("pub const ")
                || (indent == 0 && (line.starts_with("fn ") || line.starts_with("async fn ") || line.starts_with("struct ") || line.starts_with("enum ")))
        } else if is_py {
            indent == 0 && (line.starts_with("def ") || line.starts_with("async def ") || line.starts_with("class "))
        } else if is_go {
            indent == 0 && (line.starts_with("func ") || line.starts_with("type "))
        } else {
            // JS/TS and similar
            line.starts_with("export function ")
                || line.starts_with("export async function ")
                || line.starts_with("export default function ")
                || line.starts_with("export class ")
                || line.starts_with("export interface ")
                || line.starts_with("export type ")
                || line.starts_with("export enum ")
                || line.starts_with("export const ")
                || (indent == 0
                    && (line.starts_with("function ")
                        || line.starts_with("async function ")
                        || line.starts_with("class ")
                        || line.starts_with("interface ")))
        };
        if matched {
            let sig = line.split('{').next().unwrap_or(line).trim();
            let sig: String = sig.chars().take(100).collect();
            out.push(format!("L{}: {sig}", idx + 1));
        }
    }
    out
}

/// Extract quoted route literals from common web/HTTP frameworks (Axum, Express/Hono, FastAPI/Flask, React Router).
pub fn file_routes(path: &str, text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        if out.len() >= limit {
            break;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        let candidate = extract_after_call(line, ".route(")
            .or_else(|| extract_after_call(line, ".get("))
            .or_else(|| extract_after_call(line, ".post("))
            .or_else(|| extract_after_call(line, ".put("))
            .or_else(|| extract_after_call(line, ".delete("))
            .or_else(|| extract_after_call(line, ".patch("))
            .or_else(|| extract_after_call(line, "path="));
        if let Some(route) = candidate
            && route.starts_with('/')
            && route.len() <= 80
            && !route.contains(' ')
        {
            let entry = format!("{route} ({path})");
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out
}

fn extract_after_call<'a>(line: &'a str, needle: &str) -> Option<&'a str> {
    let rest = line.split_once(needle)?.1.trim_start();
    let quote = rest.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let after = &rest[1..];
    let end = after.find(quote)?;
    Some(&after[..end])
}

/// Summarize manifest scripts (package.json, Cargo.toml, pyproject.toml) so agents skip reading them just for commands.
fn extract_scripts(tree: &Path, files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for rel in files {
        let name = Path::new(rel).file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "package.json" {
            if let Ok(text) = std::fs::read_to_string(tree.join(rel))
                && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
                && let Some(scripts) = val["scripts"].as_object()
                && !scripts.is_empty()
            {
                let mut keys: Vec<&str> = scripts.keys().map(String::as_str).collect();
                keys.sort();
                let list = keys.into_iter().take(10).collect::<Vec<_>>().join(", ");
                out.push(format!("{rel}: npm scripts [{list}]"));
            }
        } else if rel == "Cargo.toml"
            && let Ok(text) = std::fs::read_to_string(tree.join(rel))
        {
            if text.contains("[workspace]") {
                out.push("Cargo.toml: cargo workspace (cargo test / cargo check / cargo clippy)".to_string());
            } else {
                out.push("Cargo.toml: rust crate (cargo test / cargo check)".to_string());
            }
        } else if rel == "pyproject.toml" {
            out.push("pyproject.toml: python project (pytest)".to_string());
        }
        if out.len() >= 6 {
            break;
        }
    }
    out
}

/// Extract `path:line signature` entries across files matching `glob` (used by `run_code`'s `symbols()`).
pub fn extract_symbols(tree: &Path, glob: Option<&str>) -> anyhow::Result<Vec<String>> {
    let mut files = tracked_files(tree);
    if let Some(pattern) = glob.filter(|g| !g.is_empty()) {
        let matcher = globset::Glob::new(pattern)?.compile_matcher();
        files.retain(|f| matcher.is_match(f));
    }
    let mut out = Vec::new();
    for file in files {
        if !is_code_file(&file) {
            continue;
        }
        let full = tree.join(&file);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if meta.len() > MAX_SCAN_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else { continue };
        for sym in file_symbols(&file, &text, 40) {
            out.push(format!("{file}:{sym}"));
            if out.len() >= 300 {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// Render a deterministic, size-capped repo map for prompt injection, cached by git tree hash.
/// Empty for blank repos.
pub fn render(tree: &Path) -> String {
    let cache_key = tree_cache_key(tree);
    if let Some(key) = &cache_key
        && let Some(hit) = cache().lock().unwrap().get(key).cloned()
    {
        return hit;
    }
    let rendered = render_uncached(tree);
    if let Some(key) = cache_key {
        let mut guard = cache().lock().unwrap();
        if guard.len() >= MAX_CACHE_ENTRIES {
            guard.clear();
        }
        guard.insert(key, rendered.clone());
    }
    rendered
}

fn render_uncached(tree: &Path) -> String {
    let files = tracked_files(tree);
    if files.is_empty() {
        return String::new();
    }
    let mut by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in &files {
        let dir = Path::new(file)
            .parent()
            .and_then(|p| p.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(".")
            .to_string();
        by_dir.entry(dir).or_default().push(file.clone());
    }

    let mut out = format!("Repository map ({} files):\n", files.len());
    let scripts = extract_scripts(tree, &files);
    if !scripts.is_empty() {
        out.push_str("Scripts:\n");
        for line in scripts {
            let entry = format!("  - {line}\n");
            if out.len() + entry.len() <= MAX_MAP_BYTES {
                out.push_str(&entry);
            }
        }
    }

    let mut routes: Vec<String> = Vec::new();
    let mut file_syms: HashMap<&str, Vec<String>> = HashMap::new();
    for file in &files {
        if !is_code_file(file) {
            continue;
        }
        let full = tree.join(file);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if meta.len() > MAX_SCAN_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else { continue };
        let syms = file_symbols(file, &text, MAX_SYMBOLS_PER_FILE);
        if !syms.is_empty() {
            file_syms.insert(file.as_str(), syms);
        }
        if routes.len() < MAX_ROUTES {
            for r in file_routes(file, &text, MAX_ROUTES - routes.len()) {
                if !routes.contains(&r) {
                    routes.push(r);
                }
            }
        }
    }

    if !routes.is_empty() {
        out.push_str("Routes:\n");
        for route in &routes {
            let entry = format!("  - {route}\n");
            if out.len() + entry.len() <= MAX_MAP_BYTES / 3 {
                out.push_str(&entry);
            }
        }
    }

    for (dir, list) in &by_dir {
        if out.len() >= MAX_MAP_BYTES {
            out.push_str("... (truncated)\n");
            break;
        }
        let header = format!("- {dir}/\n");
        if out.len() + header.len() > MAX_MAP_BYTES {
            break;
        }
        out.push_str(&header);
        for file in list {
            let name = Path::new(file).file_name().and_then(|n| n.to_str()).unwrap_or(file);
            let mut entry = format!("  - {name}");
            if let Some(syms) = file_syms.get(file.as_str()) {
                let compact: Vec<&str> = syms
                    .iter()
                    .map(|s| s.split_once(": ").map_or(s.as_str(), |(_, rest)| rest))
                    .take(6)
                    .collect();
                entry.push_str(&format!("  [{}]", compact.join(" | ")));
            }
            entry.push('\n');
            if out.len() + entry.len() > MAX_MAP_BYTES {
                out.push_str("  ... (more files)\n");
                return out;
            }
            out.push_str(&entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_deterministic_map_with_symbols_scripts_and_routes() {
        let dir = std::env::temp_dir().join(format!("tm-repomap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(&dir).args(["config", "user.email", "t@e.com"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(&dir).args(["config", "user.name", "T"]).status().unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"vite build","test":"vitest"}}"#).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub struct App {}\npub fn start() {\n    router.route(\"/api/health\", get(ok));\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/ui.ts"), "export interface Props { id: string }\nexport function View() {}\n").unwrap();
        std::process::Command::new("git").arg("-C").arg(&dir).args(["add", "-A"]).status().unwrap();
        std::process::Command::new("git").arg("-C").arg(&dir).args(["commit", "-q", "-m", "init"]).status().unwrap();

        let map = render(&dir);
        assert!(map.contains("Repository map (3 files):"), "{map}");
        assert!(map.contains("npm scripts [build, test]"), "{map}");
        assert!(map.contains("/api/health (src/lib.rs)"), "{map}");
        assert!(map.contains("lib.rs"));
        assert!(map.contains("pub struct App"));
        assert!(map.contains("export function View()"));

        // Second render hits the git-tree-hash cache identically.
        let key_before = tree_cache_key(&dir).unwrap();
        assert_eq!(map, render(&dir));
        assert!(cache().lock().unwrap().contains_key(&key_before));

        // Mutating a file invalidates the cache key and reflects the new symbol.
        std::fs::write(dir.join("src/lib.rs"), "pub struct App {}\npub fn start() {}\npub fn added_fn() {}\n").unwrap();
        let updated = render(&dir);
        assert!(updated.contains("pub fn added_fn()"), "{updated}");
        assert_ne!(tree_cache_key(&dir).unwrap(), key_before);

        let syms = extract_symbols(&dir, Some("src/*.rs")).unwrap();
        assert!(syms.iter().any(|s| s.contains("src/lib.rs:L1: pub struct App")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
