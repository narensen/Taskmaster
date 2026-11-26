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
    /// Convert any paths under `base` into portable POSIX relative paths so `.flow/env.json`
    /// never commits machine-specific absolute paths.
    pub fn relativize(&self, base: &Path) -> Self {
        let base_str = base
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        let canon_base = std::fs::canonicalize(base)
            .ok()
            .map(|p| p.to_string_lossy().trim_end_matches(['/', '\\']).to_string());
        let canon_base_path = std::fs::canonicalize(base).ok();

        let rel_path = |raw: &str| -> String {
            let p = Path::new(raw);
            if !p.is_absolute() {
                return raw.replace('\\', "/");
            }
            if let Ok(stripped) = p.strip_prefix(base) {
                return stripped.to_string_lossy().replace('\\', "/");
            }
            if let (Ok(canon_p), Some(canon_b)) = (std::fs::canonicalize(p), &canon_base_path)
                && let Ok(stripped) = canon_p.strip_prefix(canon_b)
            {
                return stripped.to_string_lossy().replace('\\', "/");
            }
            for prefix in std::iter::once(base_str.as_str()).chain(canon_base.as_deref()) {
                if !prefix.is_empty()
                    && let Some(rest) = raw.strip_prefix(prefix)
                    && let Some(rest) = rest.strip_prefix('/').or_else(|| rest.strip_prefix('\\'))
                {
                    return rest.replace('\\', "/");
                }
            }
            raw.to_string()
        };

        let rel_cmd = |cmd: &str| -> String {
            let mut out = cmd.to_string();
            for prefix in std::iter::once(base_str.as_str()).chain(canon_base.as_deref()) {
                if !prefix.is_empty() {
                    let slash = format!("{prefix}/");
                    if out.contains(&slash) {
                        out = out.replace(&slash, "");
                    }
                    let bslash = format!("{prefix}\\");
                    if out.contains(&bslash) {
                        out = out.replace(&bslash, "");
                    }
                }
            }
            out
        };

        Self {
            runner: self.runner.clone(),
            test_cmd: rel_cmd(&self.test_cmd),
            venv: self.venv.as_deref().map(rel_path),
            python: self.python.as_deref().map(rel_path),
            install_cmd: self.install_cmd.as_deref().map(rel_cmd),
        }
    }

    /// Resolve relative paths against `base` at runtime (leaving already-absolute paths intact).
    pub fn resolve(&self, base: &Path) -> Self {
        let resolve_one = |raw: &str| -> String {
            let p = Path::new(raw);
            if p.is_absolute() || raw.is_empty() {
                raw.to_string()
            } else {
                base.join(p).display().to_string()
            }
        };
        let resolved_venv = self.venv.as_deref().map(resolve_one);
        let resolved_python = self.python.as_deref().map(resolve_one);
        let mut resolved_test_cmd = self.test_cmd.clone();
        let mut resolved_install_cmd = self.install_cmd.clone();

        // Replace `venv` prefix before `python` (since `python` typically lives inside `venv`)
        // and guard against double-replacement when already resolved.
        if let (Some(raw_venv), Some(abs_venv)) = (&self.venv, &resolved_venv)
            && raw_venv != abs_venv
            && !raw_venv.is_empty()
        {
            if !resolved_test_cmd.contains(abs_venv) {
                resolved_test_cmd = resolved_test_cmd.replace(raw_venv, abs_venv);
            }
            if let Some(ref mut ic) = resolved_install_cmd
                && !ic.contains(abs_venv)
            {
                *ic = ic.replace(raw_venv, abs_venv);
            }
        }
        if let (Some(raw_py), Some(abs_py)) = (&self.python, &resolved_python)
            && raw_py != abs_py
            && !raw_py.is_empty()
        {
            if !resolved_test_cmd.contains(abs_py) {
                resolved_test_cmd = resolved_test_cmd.replace(raw_py, abs_py);
            }
            if let Some(ref mut ic) = resolved_install_cmd
                && !ic.contains(abs_py)
            {
                *ic = ic.replace(raw_py, abs_py);
            }
        }

        Self {
            runner: self.runner.clone(),
            test_cmd: resolved_test_cmd,
            venv: resolved_venv,
            python: resolved_python,
            install_cmd: resolved_install_cmd,
        }
    }

    /// One-line hint placed into the shared prefix at run start or phase boundaries.
    pub fn shared_prefix_line(&self) -> String {
        if self.test_cmd.trim().is_empty() {
            return String::new();
        }
        match (&self.venv, &self.python) {
            (Some(venv), Some(python)) if !venv.is_empty() && !python.is_empty() => format!(
                "Verified test environment: run tests with `{}` (runner: {}); the shared venv already exists at `{}` (python: `{}`, env `$TASKMASTER_SHARED_VENV`) — reuse it and do NOT create a new venv.",
                self.test_cmd, self.runner, venv, python
            ),
            (Some(venv), _) if !venv.is_empty() => format!(
                "Verified test environment: run tests with `{}` (runner: {}); the shared venv already exists at `{}` (env `$TASKMASTER_SHARED_VENV`) — reuse it and do NOT create a new venv.",
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
        assert!(line.contains("do NOT create a new venv"), "{line}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_facts_relativize_and_resolve_roundtrip_without_absolute_paths() {
        let base = std::env::temp_dir().join(format!("tm-flow-env-rel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let venv = base.join(".taskmaster/runs/current/venv");
        std::fs::create_dir_all(venv.join("bin")).unwrap();

        let abs = EnvFacts {
            runner: "pytest".into(),
            test_cmd: format!("{}/bin/python -m pytest -q", venv.display()),
            venv: Some(venv.display().to_string()),
            python: Some(format!("{}/bin/python", venv.display())),
            install_cmd: Some(format!("{}/bin/python -m pip install -q pytest", venv.display())),
        };
        let rel = abs.relativize(&base);
        assert_eq!(rel.venv.as_deref(), Some(".taskmaster/runs/current/venv"));
        assert_eq!(rel.python.as_deref(), Some(".taskmaster/runs/current/venv/bin/python"));
        assert_eq!(rel.test_cmd, ".taskmaster/runs/current/venv/bin/python -m pytest -q");
        assert_eq!(
            rel.install_cmd.as_deref(),
            Some(".taskmaster/runs/current/venv/bin/python -m pip install -q pytest")
        );
        assert!(!Path::new(rel.venv.as_deref().unwrap()).is_absolute());
        assert!(!Path::new(rel.python.as_deref().unwrap()).is_absolute());
        assert!(!rel.test_cmd.starts_with('/'));

        let resolved = rel.resolve(&base);
        assert_eq!(resolved, abs, "resolve must restore exact absolute paths without double prefixing");

        let _ = std::fs::remove_dir_all(&base);
    }
}
