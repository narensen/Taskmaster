//! Retention policy and garbage collection for runs, worktrees, and caches (Item 3.6).

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionPolicy {
    /// Keep at most this many finished runs (0 = unlimited).
    pub max_runs: usize,
    /// Delete finished runs older than this many days (0 = unlimited).
    pub max_age_days: u64,
    /// Prune stale git worktrees (`git worktree prune`).
    pub prune_worktrees: bool,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_runs: 100,
            max_age_days: 30,
            prune_worktrees: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    pub removed_runs: Vec<String>,
    pub kept_runs: usize,
    pub worktrees_pruned: bool,
    pub bytes_freed: u64,
}

/// Remove expired run directories under `.taskmaster/runs/` (never deleting `active_runs`),
/// and prune stale git worktrees when enabled.
pub fn run_gc(
    project: &Path,
    policy: &RetentionPolicy,
    active_runs: &BTreeSet<String>,
) -> anyhow::Result<GcReport> {
    let mut report = GcReport::default();
    let runs_dir = project.join(".taskmaster").join("runs");
    if runs_dir.is_dir() {
        let mut entries: Vec<(String, SystemTime, std::path::PathBuf)> = Vec::new();
        if let Ok(read) = std::fs::read_dir(&runs_dir) {
            for ent in read.flatten() {
                let path = ent.path();
                if !path.is_dir() {
                    continue;
                }
                let id = ent.file_name().to_string_lossy().to_string();
                let mtime = ent
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                entries.push((id, mtime, path));
            }
        }
        // Newest first.
        entries.sort_by_key(|e| std::cmp::Reverse(e.1));
        let now = SystemTime::now();
        let cutoff = if policy.max_age_days > 0 {
            now.checked_sub(Duration::from_secs(policy.max_age_days * 86_400))
        } else {
            None
        };

        for (idx, (id, mtime, path)) in entries.into_iter().enumerate() {
            if active_runs.contains(&id) {
                report.kept_runs += 1;
                continue;
            }
            let over_count = policy.max_runs > 0 && idx >= policy.max_runs;
            let over_age = cutoff.is_some_and(|c| mtime < c);
            if over_count || over_age {
                let bytes = dir_size(&path);
                if std::fs::remove_dir_all(&path).is_ok() {
                    crate::checkpoints::prune_run(project, &id);
                    report.removed_runs.push(id);
                    report.bytes_freed += bytes;
                }
            } else {
                report.kept_runs += 1;
            }
        }
    }
    if policy.prune_worktrees && project.join(".git").exists() {
        report.worktrees_pruned = crate::git::git(project, &["worktree", "prune"]).is_ok();
    }
    Ok(report)
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(read) = std::fs::read_dir(path) else { return 0 };
    for ent in read.flatten() {
        let p = ent.path();
        if p.is_dir() {
            total += dir_size(&p);
        } else if let Ok(m) = ent.metadata() {
            total += m.len();
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_removes_excess_finished_runs_and_preserves_active_runs() {
        let dir = std::env::temp_dir().join(format!("tm-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let runs = dir.join(".taskmaster/runs");
        for name in ["run-1", "run-2", "run-3", "run-active"] {
            let d = runs.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("status.json"), b"{\"phase\":\"done\"}").unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        let policy = RetentionPolicy { max_runs: 2, max_age_days: 0, prune_worktrees: false };
        let active = BTreeSet::from(["run-1".to_string()]);
        let report = run_gc(&dir, &policy, &active).unwrap();
        // run-active and run-3 are the 2 newest; run-1 is active so preserved; run-2 is removed.
        assert!(runs.join("run-1").exists());
        assert!(runs.join("run-active").exists());
        assert_eq!(report.removed_runs, vec!["run-2".to_string()]);
        assert!(report.bytes_freed > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
