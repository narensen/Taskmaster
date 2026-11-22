//! Centralized daemon egress policy enforcement and audit log (Item 3.2).
//!
//! Every daemon-side network request checks the project's [`crate::policy::PolicyConfig`]
//! allow/deny host rules and records `{at, run, agent, method, host, status, bytes, allowed, reason}`
//! into `.taskmaster/egress.jsonl`, served to Settings via `GET /api/egress`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

static EGRESS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressRecord {
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub method: String,
    pub host: String,
    pub status: u16,
    pub bytes: u64,
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

pub fn log_path(project: &Path) -> PathBuf {
    project.join(".taskmaster").join("egress.jsonl")
}

/// Evaluate whether `host` is permitted by the project's [`crate::policy::Policy`].
pub fn check_host_allowed(project: &Path, host: &str) -> Result<(), String> {
    let policy = crate::policy::Policy::load(project);
    let host_l = host.trim().to_ascii_lowercase();
    if host_l.is_empty() {
        return Err("empty host".into());
    }
    for denied in &policy.denied_fetch_hosts {
        let d = denied.trim().to_ascii_lowercase();
        if !d.is_empty() && (host_l == d || host_l.ends_with(&format!(".{d}"))) {
            return Err(format!("domain `{host}` is denied by policy"));
        }
    }
    if !policy.allowed_fetch_hosts.is_empty() {
        let matched = policy.allowed_fetch_hosts.iter().any(|allowed| {
            let a = allowed.trim().to_ascii_lowercase();
            !a.is_empty() && (host_l == a || host_l.ends_with(&format!(".{a}")))
        });
        if !matched {
            return Err(format!("domain `{host}` is not in policy allowed_fetch_hosts"));
        }
    }
    Ok(())
}

/// Append an egress audit record to `.taskmaster/egress.jsonl`.
pub fn record(project: &Path, entry: &EgressRecord) -> anyhow::Result<()> {
    let path = log_path(project);
    let _guard = EGRESS_LOCK.lock().unwrap();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(entry)?;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Enforce policy and log the decision. Returns `Err(reason)` if blocked.
pub fn audit_request(
    project: &Path,
    run: Option<&str>,
    agent: Option<&str>,
    method: &str,
    host: &str,
    status: u16,
    bytes: u64,
) -> Result<(), String> {
    match check_host_allowed(project, host) {
        Ok(()) => {
            let _ = record(
                project,
                &EgressRecord {
                    at: crate::time::rfc3339_now(),
                    run: run.map(str::to_string),
                    agent: agent.map(str::to_string),
                    method: method.to_string(),
                    host: host.to_string(),
                    status,
                    bytes,
                    allowed: true,
                    reason: None,
                },
            );
            Ok(())
        }
        Err(reason) => {
            let _ = record(
                project,
                &EgressRecord {
                    at: crate::time::rfc3339_now(),
                    run: run.map(str::to_string),
                    agent: agent.map(str::to_string),
                    method: method.to_string(),
                    host: host.to_string(),
                    status: 0,
                    bytes: 0,
                    allowed: false,
                    reason: Some(reason.clone()),
                },
            );
            Err(reason)
        }
    }
}

/// Read the most recent `limit` egress audit entries (newest first).
pub fn recent(project: &Path, limit: usize) -> Vec<EgressRecord> {
    let Ok(text) = std::fs::read_to_string(log_path(project)) else { return Vec::new() };
    let mut entries: Vec<EgressRecord> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<EgressRecord>(line).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_denied_domains_and_records_audit_entries() {
        let dir = std::env::temp_dir().join(format!("tm-egress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("taskmaster.policy.toml"),
            "denied_fetch_hosts = [\"evil.example\"]\n",
        )
        .unwrap();

        assert!(audit_request(&dir, Some("run-1"), Some("planner"), "GET", "docs.rs", 200, 4096).is_ok());
        let denied = audit_request(&dir, Some("run-1"), Some("worker"), "GET", "sub.evil.example", 0, 0);
        assert!(denied.is_err());

        let log = recent(&dir, 10);
        assert_eq!(log.len(), 2);
        assert!(!log[0].allowed);
        assert_eq!(log[0].host, "sub.evil.example");
        assert!(log[1].allowed);
        assert_eq!(log[1].bytes, 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
