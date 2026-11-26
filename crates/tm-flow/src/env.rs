//! Verified project test/runtime environment facts stored at `.flow/env.json`.
//!
//! Recorded after the first successful test/verify run so later phases and runs reuse
//! the already-created test command and virtualenv instead of rediscovering them.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::model::{FLOW_DIR, FLOW_GITIGNORE_CONTENT};

pub const ENV_FILE: &str = "env.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvFacts {
    pub runner: String,
    pub test_cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venv: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_cmd: Option<String>,
}

impl EnvFacts {
    /// One-line hint placed into the shared prefix at run start or phase boundaries.
    pub fn shared_prefix_line(&self) -> String {
        if self.test_cmd.trim().is_empty() {
            return String::new();
        }
        match (&self.venv, &self.python) {
            (Some(venv), Some(python)) if !venv.is_empty() && !python.is_empty() => format!(
                "Verified test environment: run tests with `{}` (runner: {}); the venv already exists at `{}` (python: `{}`) — reuse it instead of creating a new venv.",
                self.test_cmd, self.runner, venv, python
            ),
            (Some(venv), _) if !venv.is_empty() => format!(
                "Verified test environment: run tests with `{}` (runner: {}); the venv already exists at `{}` — reuse it instead of creating a new venv.",
                self.test_cmd, self.runner, venv
            ),
            _ => format!(
                "Verified test environment: run tests with `{}` (runner: {}).",
                self.test_cmd, self.runner
            ),
        }
    }
}

pub fn env_path(tree: &Path) -> PathBuf {
    tree.join(FLOW_DIR).join(ENV_FILE)
}

/// Read `.flow/env.json` if present and valid.
pub fn read_env(tree: &Path) -> anyhow::Result<Option<EnvFacts>> {
    let path = env_path(tree);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let facts: EnvFacts = serde_json::from_str(&raw)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    Ok(Some(facts))
}

/// Write `.flow/env.json` deterministically and ensure `.flow/.gitignore` exists.
pub fn write_env(tree: &Path, facts: &EnvFacts) -> anyhow::Result<bool> {
    let flow_dir = tree.join(FLOW_DIR);
    std::fs::create_dir_all(&flow_dir)?;
    let gi = flow_dir.join(".gitignore");
    if std::fs::read_to_string(&gi).ok().as_deref() != Some(FLOW_GITIGNORE_CONTENT) {
        std::fs::write(&gi, FLOW_GITIGNORE_CONTENT)?;
    }
    let path = env_path(tree);
    let mut rendered = serde_json::to_string_pretty(facts)?;
    rendered.push('\n');
    if std::fs::read_to_string(&path).ok().as_deref() == Some(rendered.as_str()) {
        return Ok(false);
    }
    std::fs::write(&path, rendered).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_facts_roundtrip_and_prefix_line() {
        let dir = std::env::temp_dir().join(format!("tm-flow-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(read_env(&dir).unwrap(), None);

        let facts = EnvFacts {
            runner: "pytest".into(),
            test_cmd: "/tmp/venv/bin/python -m pytest -q".into(),
            venv: Some("/tmp/venv".into()),
            python: Some("/tmp/venv/bin/python".into()),
            install_cmd: Some("/tmp/venv/bin/python -m pip install -q -e . pytest".into()),
        };
        assert!(write_env(&dir, &facts).unwrap());
        assert!(!write_env(&dir, &facts).unwrap(), "unchanged write should report false");
        assert_eq!(read_env(&dir).unwrap(), Some(facts.clone()));
        assert!(dir.join(".flow/.gitignore").is_file());

        let line = facts.shared_prefix_line();
        assert!(line.contains("/tmp/venv/bin/python -m pytest -q"), "{line}");
        assert!(line.contains("/tmp/venv"), "{line}");
        assert!(line.contains("reuse it"), "{line}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
