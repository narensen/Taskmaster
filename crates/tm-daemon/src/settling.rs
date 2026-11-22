//! Server-owned run flags (A8): Pin, Settle and Archive live with the run, so every device
//! agrees, and the daemon settles a run on its own when there is nothing left to do with it.
//!
//! A run settles itself once it was merged (or its pull request was) and has been quiet for
//! a while, or after some days with no activity at all, unless it is set to Never. Settling
//! stops its preview and removes its leftover worktrees. A new turn on a run the daemon
//! settled takes it out of Settled again.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunFlags {
    #[serde(skip_serializing_if = "is_false")]
    pub pinned: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub settled: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub archived: bool,
    /// The daemon never settles this run on its own.
    #[serde(skip_serializing_if = "is_false")]
    pub never_settle: bool,
    /// `you` or `taskmaster`, and why, when the run is settled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<String>,
    /// The pull request the run opened, so a merge there settles it too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
}

impl RunFlags {
    pub fn is_empty(&self) -> bool {
        *self == RunFlags::default()
    }
}

/// When the daemon settles runs on its own, from the settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SettleSettings {
    /// Minutes a merged run (or one whose pull request merged) stays out before it settles.
    pub after_merge_mins: u32,
    /// Days with no activity before any finished run settles. 0 turns it off.
    pub idle_days: u32,
}

impl Default for SettleSettings {
    fn default() -> Self {
        SettleSettings { after_merge_mins: 60, idle_days: 7 }
    }
}

/// What the daemon knows about a finished run when it decides.
pub struct Seen<'a> {
    pub merged: bool,
    pub pr_merged: bool,
    /// The run's last activity, RFC 3339.
    pub last_at: Option<&'a str>,
}

/// Why the run should settle now, or `None`. Active runs and runs waiting on a person are
/// never passed here.
pub fn auto_reason(flags: &RunFlags, seen: &Seen, now: u64, rules: SettleSettings) -> Option<String> {
    if flags.settled || flags.never_settle {
        return None;
    }
    let quiet_mins = seen.last_at.and_then(crate::time::parse_rfc3339).map(|at| (now.saturating_sub(at) / 60) as i64);
    let quiet_long_enough = |mins: u32| quiet_mins.is_none_or(|q| q >= i64::from(mins));
    if seen.merged && quiet_long_enough(rules.after_merge_mins) {
        return Some("merged".into());
    }
    if seen.pr_merged && quiet_long_enough(rules.after_merge_mins) {
        return Some("its pull request merged".into());
    }
    if rules.idle_days > 0 && quiet_mins.is_some_and(|q| q >= i64::from(rules.idle_days) * 24 * 60) {
        return Some(format!("no activity for {} days", rules.idle_days));
    }
    None
}

/// Remove the git worktrees a run left in the temp directory (its pages', its checks', its
/// preview's), and prune what git still lists. Returns how many were removed.
pub fn remove_leftover_worktrees(project: &Path, run: &str) -> usize {
    let prefix = crate::git::worktree_dir(project, run, "").display().to_string();
    let Ok(out) = Command::new("git").args(["worktree", "list", "--porcelain"]).current_dir(project).output() else { return 0 };
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut removed = 0;
    for path in listed.lines().filter_map(|line| line.strip_prefix("worktree ")) {
        if path.starts_with(&prefix) {
            let tree = Path::new(path);
            if crate::git::remove_worktree(project, tree).is_ok() {
                removed += 1;
            }
            let _ = std::fs::remove_dir_all(tree);
        }
    }
    let _ = Command::new("git").args(["worktree", "prune"]).current_dir(project).output();
    removed
}

/// Whether the pull request at `url` is merged, asked of `gh`. Any failure is "no".
pub fn pr_merged(project: &Path, url: &str) -> bool {
    Command::new("gh")
        .args(["pr", "view", url, "--json", "state", "-q", ".state"])
        .current_dir(project)
        .output()
        .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "MERGED")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::parse_rfc3339 as parse;

    #[test]
    fn a_run_settles_after_a_quiet_merge_or_long_idle_unless_never() {
        let rules = SettleSettings { after_merge_mins: 60, idle_days: 7 };
        let now = parse("2026-11-23T12:00:00Z").unwrap();
        let flags = RunFlags::default();
        let merged_recently = Seen { merged: true, pr_merged: false, last_at: Some("2026-11-23T11:30:00Z") };
        assert_eq!(auto_reason(&flags, &merged_recently, now, rules), None, "still within the hour after the merge");
        let merged_quiet = Seen { merged: true, pr_merged: false, last_at: Some("2026-11-23T10:30:00Z") };
        assert_eq!(auto_reason(&flags, &merged_quiet, now, rules).as_deref(), Some("merged"));
        let pr = Seen { merged: false, pr_merged: true, last_at: Some("2026-11-23T10:30:00Z") };
        assert_eq!(auto_reason(&flags, &pr, now, rules).as_deref(), Some("its pull request merged"));
        let idle = Seen { merged: false, pr_merged: false, last_at: Some("2026-11-15T11:00:00Z") };
        assert_eq!(auto_reason(&flags, &idle, now, rules).as_deref(), Some("no activity for 7 days"));
        let fresh = Seen { merged: false, pr_merged: false, last_at: Some("2026-11-22T11:00:00Z") };
        assert_eq!(auto_reason(&flags, &fresh, now, rules), None);
        let never = RunFlags { never_settle: true, ..RunFlags::default() };
        assert_eq!(auto_reason(&never, &merged_quiet, now, rules), None);
        assert_eq!(auto_reason(&never, &idle, now, rules), None);
        assert_eq!(auto_reason(&flags, &idle, now, SettleSettings { idle_days: 0, ..rules }), None, "0 days is off");
    }
}
