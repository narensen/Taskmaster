//! Project worktree configuration (`taskmaster.yaml`, O1).
//!
//! Committed at the project root so every worktree gets deterministic setup, shared
//! dependency links, copied ignored files (like `.env`), and custom preview/test/archive
//! commands. Executable commands are trusted per SHA-256 hash so a cloned repository or
//! edited `setup` command always asks the person before running.

use std::fs;
use std::path::{Component, Path};
use std::process::Command;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CONFIG_FILE: &str = "taskmaster.yaml";
const TRUST_FILE: &str = "trusted_yaml.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedProjectConfig {
    pub config: ProjectConfig,
    pub raw: String,
    /// SHA-256 hex digest of the canonical command fields (`setup`, `preview`, `test`, `archive`).
    pub hash: String,
    pub has_commands: bool,
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TrustRecord {
    approved_hashes: Vec<String>,
}

impl ProjectConfig {
    pub fn has_commands(&self) -> bool {
        self.setup.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self.preview.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self.test.as_deref().is_some_and(|s| !s.trim().is_empty())
            || self.archive.as_deref().is_some_and(|s| !s.trim().is_empty())
    }

    pub fn command_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(cmd) = self.setup.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("setup: `{cmd}`"));
        }
        if let Some(cmd) = self.preview.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("preview: `{cmd}`"));
        }
        if let Some(cmd) = self.test.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("test: `{cmd}`"));
        }
        if let Some(cmd) = self.archive.as_deref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("archive: `{cmd}`"));
        }
        parts.join(", ")
    }

    /// Canonical SHA-256 digest of all executable command fields.
    pub fn commands_hash(&self) -> String {
        let canonical = serde_json::json!({
            "setup": self.setup.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            "preview": self.preview.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            "test": self.test.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            "archive": self.archive.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        });
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string().as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// Parse the flat YAML subset used by `taskmaster.yaml`: scalar keys (`setup`, `preview`,
/// `test`, `archive`) and string list keys (`shared`, `copy`, in flow `[a, b]` or block `- a` form).
pub fn parse_yaml(text: &str) -> anyhow::Result<ProjectConfig> {
    let mut cfg = ProjectConfig::default();
    let mut current_list: Option<&'static str> = None;

    for (line_no, raw_line) in text.lines().enumerate() {
        let stripped = strip_yaml_comment(raw_line);
        let trimmed = stripped.trim();
        if trimmed.is_empty() || trimmed == "---" || trimmed == "..." {
            continue;
        }
        if let Some(item) = trimmed.strip_prefix("- ") {
            let val = unquote_scalar(item.trim());
            if val.is_empty() {
                continue;
            }
            match current_list {
                Some("shared") => cfg.shared.push(val),
                Some("copy") => cfg.copy.push(val),
                _ => bail!("taskmaster.yaml:{}: unexpected list item outside `shared` or `copy`", line_no + 1),
            }
            continue;
        }
        current_list = None;
        let Some((key, rest)) = trimmed.split_once(':') else {
            bail!("taskmaster.yaml:{}: expected `key: value`", line_no + 1);
        };
        let key = key.trim();
        let val = rest.trim();
        match key {
            "setup" => cfg.setup = non_empty(unquote_scalar(val)),
            "preview" => cfg.preview = non_empty(unquote_scalar(val)),
            "test" => cfg.test = non_empty(unquote_scalar(val)),
            "archive" => cfg.archive = non_empty(unquote_scalar(val)),
            "shared" => {
                if val.is_empty() {
                    current_list = Some("shared");
                } else {
                    cfg.shared = parse_flow_list(val)?;
                }
            }
            "copy" => {
                if val.is_empty() {
                    current_list = Some("copy");
                } else {
                    cfg.copy = parse_flow_list(val)?;
                }
            }
            _ => {
                // Ignore unknown keys for forward compatibility.
            }
        }
    }
    Ok(cfg)
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_space = true;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '#' if !in_single && !in_double && prev_space => return &line[..idx],
            _ => {}
        }
        prev_space = ch.is_whitespace();
    }
    line
}

fn unquote_scalar(val: &str) -> String {
    let v = val.trim();
    if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

fn parse_flow_list(val: &str) -> anyhow::Result<Vec<String>> {
    let v = val.trim();
    if v == "[]" {
        return Ok(Vec::new());
    }
    if let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Ok(inner
            .split(',')
            .map(unquote_scalar)
            .filter(|s| !s.is_empty())
            .collect())
    } else {
        let single = unquote_scalar(v);
        if single.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(vec![single])
        }
    }
}

/// Load `taskmaster.yaml` from `project` root (returns `Ok(None)` if absent).
pub fn load(project: &Path) -> anyhow::Result<Option<LoadedProjectConfig>> {
    let path = project.join(CONFIG_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let config = parse_yaml(&raw)?;
    let hash = config.commands_hash();
    let has_commands = config.has_commands();
    let trusted = !has_commands || is_trusted(project, &hash);
    Ok(Some(LoadedProjectConfig {
        config,
        raw,
        hash,
        has_commands,
        trusted,
    }))
}

fn trust_path(project: &Path) -> std::path::PathBuf {
    project.join(".taskmaster").join(TRUST_FILE)
}

pub fn is_trusted(project: &Path, hash: &str) -> bool {
    let Ok(raw) = fs::read_to_string(trust_path(project)) else {
        return false;
    };
    let Ok(rec) = serde_json::from_str::<TrustRecord>(&raw) else {
        return false;
    };
    rec.approved_hashes.iter().any(|h| h == hash)
}

pub fn trust(project: &Path, hash: &str) -> anyhow::Result<()> {
    let path = trust_path(project);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut rec: TrustRecord = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if !rec.approved_hashes.iter().any(|h| h == hash) {
        rec.approved_hashes.push(hash.to_string());
    }
    fs::write(&path, serde_json::to_string_pretty(&rec)?)?;
    Ok(())
}

fn is_safe_entry(rel: &str) -> bool {
    let rel = rel.trim();
    if rel.is_empty() || rel.starts_with('/') || rel.starts_with('\\') || rel == ".git" || rel.starts_with(".git/") {
        return false;
    }
    let p = Path::new(rel);
    !p.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}

/// Apply `shared` links (generalizing `borrow_node_modules`) and `copy` ignored files into `worktree`.
pub fn apply_shared_and_copy(project: &Path, worktree: &Path, cfg: Option<&ProjectConfig>) {
    crate::git::borrow_node_modules(project, worktree);
    let Some(cfg) = cfg else {
        return;
    };
    for rel in &cfg.shared {
        if !is_safe_entry(rel) {
            continue;
        }
        let src = project.join(rel);
        let dest = worktree.join(rel);
        if !src.exists() || dest.exists() || dest.symlink_metadata().is_ok() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&src, &dest);
        #[cfg(windows)]
        if src.is_dir() {
            let _ = std::os::windows::fs::symlink_dir(&src, &dest);
        } else {
            let _ = std::os::windows::fs::symlink_file(&src, &dest);
        }
    }
    for rel in &cfg.copy {
        if !is_safe_entry(rel) {
            continue;
        }
        let src = project.join(rel);
        let dest = worktree.join(rel);
        if !src.exists() {
            continue;
        }
        if src.is_file() {
            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::copy(&src, &dest);
        } else if src.is_dir() {
            let _ = copy_dir_recursive(&src, &dest);
        }
    }
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}

/// Execute `setup` synchronously in `worktree` (`wait-for-setup` semantics) and return `(ok, combined_output)`.
pub fn run_setup(worktree: &Path, setup_cmd: &str) -> anyhow::Result<(bool, String)> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(setup_cmd)
        .current_dir(worktree)
        .output()
        .with_context(|| format!("failed to start taskmaster.yaml setup `{setup_cmd}`"))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    Ok((out.status.success(), combined))
}

/// Run the optional `archive` hook before removing a worktree (best effort).
pub fn run_archive(worktree: &Path, archive_cmd: &str) {
    let cmd = archive_cmd.trim();
    if cmd.is_empty() || !worktree.is_dir() {
        return;
    }
    let _ = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .status();
}

/// Generate a sensible starter `taskmaster.yaml` by inspecting `project`.
pub fn generate_from_project(project: &Path) -> String {
    let mut setup = String::new();
    let mut preview = String::new();
    let mut test = String::new();
    let mut shared: Vec<&str> = Vec::new();
    let mut copy: Vec<&str> = Vec::new();

    if project.join("package.json").is_file() {
        let pkg: serde_json::Value = fs::read_to_string(project.join("package.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let scripts = pkg.get("scripts").and_then(|v| v.as_object());
        let runner = if project.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if project.join("yarn.lock").is_file() {
            "yarn"
        } else if project.join("bun.lockb").is_file() || project.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        if runner == "npm" && project.join("package-lock.json").is_file() {
            setup = "npm ci".into();
        } else {
            setup = format!("{runner} install");
        }
        if scripts.is_some_and(|s| s.contains_key("dev")) {
            preview = format!("{runner} run dev -- --port $PORT");
        } else if scripts.is_some_and(|s| s.contains_key("start")) {
            preview = format!("{runner} start");
        }
        if scripts.and_then(|s| s.get("test")).and_then(|v| v.as_str()).is_some_and(|s| !s.contains("no test specified")) {
            test = format!("{runner} test");
        }
        shared.push("node_modules");
    } else if project.join("Cargo.toml").is_file() {
        test = "cargo test".into();
    } else if project.join("pyproject.toml").is_file() || project.join("requirements.txt").is_file() {
        test = "pytest".into();
        shared.push(".venv");
    }

    for env_file in [".env", ".env.local"] {
        if project.join(env_file).is_file() {
            copy.push(env_file);
        }
    }
    if project.join(".cache").is_dir() && !shared.contains(&".cache") {
        shared.push(".cache");
    }

    let mut out = String::from("# Taskmaster worktree configuration\n");
    if !setup.is_empty() {
        out.push_str(&format!("setup: {setup}\n"));
    } else {
        out.push_str("# setup: npm ci\n");
    }
    if !preview.is_empty() {
        out.push_str(&format!("preview: {preview}\n"));
    } else {
        out.push_str("# preview: npm run dev -- --port $PORT\n");
    }
    if !test.is_empty() {
        out.push_str(&format!("test: {test}\n"));
    } else {
        out.push_str("# test: npm test\n");
    }
    out.push_str(&format!("shared: [{}]\n", shared.join(", ")));
    out.push_str(&format!("copy: [{}]\n", copy.join(", ")));
    out.push_str("archive: \"\"\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flow_and_block_yaml_and_hashes_commands() {
        let yaml = r#"
setup: npm ci # install deps
preview: "npm run dev -- --port $PORT"
test: npm test
shared: [node_modules, .venv, .cache]
copy:
  - .env
  - .env.local
archive: ""
"#;
        let cfg = parse_yaml(yaml).unwrap();
        assert_eq!(cfg.setup.as_deref(), Some("npm ci"));
        assert_eq!(cfg.preview.as_deref(), Some("npm run dev -- --port $PORT"));
        assert_eq!(cfg.test.as_deref(), Some("npm test"));
        assert_eq!(cfg.shared, vec!["node_modules", ".venv", ".cache"]);
        assert_eq!(cfg.copy, vec![".env", ".env.local"]);
        assert_eq!(cfg.archive, None);
        assert!(cfg.has_commands());

        let mut cfg2 = cfg.clone();
        cfg2.setup = Some("touch .ready".into());
        assert_ne!(cfg.commands_hash(), cfg2.commands_hash());
    }
}
