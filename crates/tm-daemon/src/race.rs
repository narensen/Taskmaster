//! O5: race one request across providers.
//!
//! A race starts the same prompt as two or three runs, one per provider, each on its own
//! branch and held there (none merges on its own). The person compares them, picks one, and
//! only that one is merged; the others' branches and worktrees are removed and the runs are
//! archived. Racing multiplies spend, so each run keeps its budget cap and the race is only
//! started when the person asks for it.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::store::Records;

pub const MIN: usize = 2;
pub const MAX: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entrant {
    pub run: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// `races/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Race {
    pub id: String,
    pub prompt: String,
    pub created: String,
    pub runs: Vec<Entrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub winner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picked_at: Option<String>,
}

/// The run id for one entrant: the race and the provider, as a valid key.
pub fn run_id(race: &str, provider: &str) -> String {
    let slug: String = provider.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    format!("{race}-{slug}")
}

/// `git diff --shortstat` of what a held run built, as `(files, added, removed)`.
pub fn diff_size(project: &Path, base: Option<&str>, branch: Option<&str>) -> Option<(u64, u64, u64)> {
    let (base, branch) = (base?, branch?);
    let out = std::process::Command::new("git").args(["diff", "--shortstat", &format!("{base}..{branch}")]).current_dir(project).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let number_before = |word: &str| {
        text.split(',').find(|part| part.contains(word)).and_then(|part| part.split_whitespace().next()).and_then(|n| n.parse::<u64>().ok()).unwrap_or(0)
    };
    Some((number_before("file"), number_before("insertion"), number_before("deletion")))
}

/// Merge the winner's branch into the project as a finished run's merge would be, and
/// record it in the run's status so its Undo works.
pub fn merge_winner(project: &Path, records: &Records, live: &crate::live::Live, run: &str) -> anyhow::Result<String> {
    let meta = records.meta(run)?.ok_or_else(|| anyhow::anyhow!("run `{run}` has no record"))?;
    let branch = meta.branch.ok_or_else(|| anyhow::anyhow!("run `{run}` built in place, so there is nothing to merge"))?;
    if !crate::git::branch_exists(project, &branch) {
        anyhow::bail!("branch `{branch}` is gone");
    }
    let _project = crate::engine::PROJECT.lock().unwrap();
    let before = crate::git::git(project, &["rev-parse", "--verify", "HEAD"])?.trim().to_string();
    let tip = crate::git::git(project, &["rev-parse", "--verify", &format!("{branch}^{{commit}}")])?.trim().to_string();
    if !crate::git::merge_branch(project, &branch, &[tm_core::docs::ARCHITECTURE_FILE, tm_core::docs::CHANGES_FILE, ".flow/"]) {
        anyhow::bail!("`{branch}` does not merge cleanly into the project; merge it yourself: git merge {branch}");
    }
    if project.join(tm_flow::FLOW_DIR).is_dir() {
        if let Err(err) = tm_flow::build(project) {
            eprintln!("taskmaster: flow refresh after race merge failed: {err:#}");
        } else if crate::git::git(project, &["add", "-A", "--", tm_flow::FLOW_DIR]).is_ok() {
            let staged = crate::git::git(project, &["diff", "--cached", "--name-only", "--", tm_flow::FLOW_DIR]).unwrap_or_default();
            if !staged.trim().is_empty() {
                let _ = crate::git::git(project, &["commit", "-q", "-m", "taskmaster: update .flow"]);
            }
        }
    }
    let after = crate::git::git(project, &["rev-parse", "--verify", "HEAD"])?.trim().to_string();
    let base = crate::git::git(project, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default().trim().to_string();
    let commit = crate::git::git(project, &["rev-parse", "--short=8", "HEAD"]).unwrap_or_default().trim().to_string();
    let _ = live.set_tip(run, &tip);
    let mut status = records.status(run)?.unwrap_or_else(|| json!({}));
    status["branch"] = json!(branch);
    status["merged"] = json!(true);
    status["base"] = json!(base);
    status["merge_commit"] = json!(commit);
    status["merged_from"] = json!(before);
    status["merged_head"] = json!(after);
    records.save_status(run, &status)?;
    let _ = crate::git::git(project, &["branch", "-D", &branch]);
    Ok(commit)
}

/// Remove a losing run's branch and worktrees. Its records stay, archived.
pub fn clear_loser(project: &Path, records: &Records, run: &str) {
    crate::settling::remove_leftover_worktrees(project, run);
    if let Ok(Some(meta)) = records.meta(run)
        && let Some(branch) = meta.branch
        && crate::git::branch_exists(project, &branch)
    {
        let _project = crate::engine::PROJECT.lock().unwrap();
        let _ = crate::git::git(project, &["branch", "-D", &branch]);
    }
    let mut flags = records.flags(run).ok().flatten().unwrap_or_default();
    flags.archived = true;
    flags.settled = true;
    flags.settled_by = Some("taskmaster".into());
    flags.settled_reason = Some("another run won the race".into());
    flags.settled_at = Some(crate::time::rfc3339_now());
    let _ = records.save_flags(run, &flags);
}

/// What the compare view shows for one entrant.
pub fn compare_row(entrant: &Entrant, view: &Value, diff: Option<(u64, u64, u64)>) -> Value {
    let pages = view["pages"].as_array().cloned().unwrap_or_default();
    let passed = pages.iter().filter(|p| p["state"] == "passed").count();
    json!({
        "run": entrant.run,
        "provider": entrant.provider,
        "model": entrant.model,
        "phase": view["phase"],
        "active": view["active"],
        "error": view["error"],
        "pages": pages.len(),
        "passed": passed,
        "review_rounds": view["review_rounds"],
        "cost_usd": view["spent"]["cost_usd"],
        "tokens": view["spent"]["tokens"],
        "branch": view["branch"],
        "merged": view["merged"],
        "diff": diff.map(|(files, added, removed)| json!({ "files": files, "added": added, "removed": removed })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrants_get_run_ids_from_the_race_and_provider() {
        assert_eq!(run_id("race-1a2b", "claude"), "race-1a2b-claude");
        assert_eq!(run_id("race-1a2b", "Open Code"), "race-1a2b-open-code");
        assert!(tm_core::valid_key(&run_id("race-1a2b", "codex")));
    }
}
