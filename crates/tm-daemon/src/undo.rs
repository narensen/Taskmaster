//! Taking a merged run back out of the project.
//!
//! An undo is one new commit on the branch the run was merged into. It puts every file the
//! run changed back as it was before the merge. Commits made since are kept, and so is the
//! history: nothing is reset or rewritten. When a file the run changed was edited after the
//! merge, or has uncommitted edits, the undo is refused and the project is left alone.

use std::path::Path;

use serde_json::json;

use crate::git;
use crate::store::Records;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undo {
    /// The undo commit, short, and how many files it put back.
    Done { commit: String, files: usize },
    /// Why the run cannot be undone here, as a sentence for the person.
    Refused(String),
}

pub fn undo_run(project: &Path, records: &Records, run: &str) -> anyhow::Result<Undo> {
    let _project = crate::engine::PROJECT.lock().unwrap();
    let refuse = |why: &str| Ok(Undo::Refused(why.to_string()));
    let Some(mut status) = records.status(run)? else { return refuse("This run has not finished, so there is nothing to undo.") };
    if let Some(commit) = status["undone"]["commit"].as_str() {
        return Ok(Undo::Refused(format!("This run was already undone, in commit {commit}.")));
    }
    if status["merged"].as_bool() != Some(true) {
        return refuse("This run was not merged into the project, so there is nothing to undo. What it built is on its branch.");
    }
    let (Some(from), Some(head)) = (status["merged_from"].as_str(), status["merged_head"].as_str()) else {
        let commit = status["merge_commit"].as_str().unwrap_or("its merge");
        return Ok(Undo::Refused(format!("This run was merged before Taskmaster kept what an undo needs. Revert it with git: git revert {commit}")));
    };
    let (from, head) = (from.to_string(), head.to_string());
    let base = status["base"].as_str().unwrap_or_default().to_string();
    let current = git::git(project, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default().trim().to_string();
    if !base.is_empty() && current != base {
        return Ok(Undo::Refused(format!("The project has `{current}` checked out. Switch to `{base}`, where this run was merged, to undo it.")));
    }
    let listed = git::git(project, &["diff", "--name-only", "-z", &head, &from])?;
    let files: Vec<&str> = listed.split('\0').filter(|f| !f.is_empty()).collect();
    if files.is_empty() {
        return refuse("The merge changed no files, so there is nothing to undo.");
    }
    let mut asked = vec!["status", "--porcelain", "--"];
    asked.extend(files.iter().copied());
    if !git::git(project, &asked)?.trim().is_empty() {
        return refuse("Files this run changed have uncommitted edits. Commit or stash them, then undo the run.");
    }
    let patch = std::env::temp_dir().join(format!("tm-undo-{}-{run}.patch", std::process::id()));
    let output = format!("--output={}", patch.display());
    git::git(project, &["diff", "--binary", &output, &head, &from])?;
    let patch_arg = patch.display().to_string();
    let applied = if git::git(project, &["apply", "--check", "--index", &patch_arg]).is_err() {
        None
    } else {
        Some(git::git(project, &["apply", "--index", &patch_arg]))
    };
    let _ = std::fs::remove_file(&patch);
    match applied {
        None => {
            let short: String = head.chars().take(8).collect();
            return Ok(Undo::Refused(format!(
                "Files this run changed were edited after it was merged, so it cannot be undone cleanly. Revert it with git and settle the conflicts: git revert {short}"
            )));
        }
        Some(result) => {
            result?;
        }
    }
    git::git(project, &["commit", "-q", "-m", &format!("taskmaster: undo run {run}")])?;
    // If undoing the run removed `.flow/meta.json`, also remove the derived SQLite cache left on disk.
    let flow_dir = project.join(tm_flow::FLOW_DIR);
    if flow_dir.is_dir() && !flow_dir.join("meta.json").is_file() {
        for name in ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"] {
            let _ = std::fs::remove_file(flow_dir.join(name));
        }
        let _ = std::fs::remove_dir(&flow_dir);
    }
    let commit = git::git(project, &["rev-parse", "--short=8", "HEAD"])?.trim().to_string();
    status["undone"] = json!({ "commit": commit, "at": crate::time::rfc3339_now(), "files": files.len() });
    records.save_status(run, &status)?;
    // The pages it built are gone again, so the next run must not skip them as fresh.
    if let Some(plan) = tm_core::Run::replay(1, records.events(run)?).plan {
        let mut citations = records.citations()?;
        for page in &plan.pages {
            citations.pages.remove(&page.id);
        }
        records.save_citations(&citations)?;
    }
    Ok(Undo::Done { commit, files: files.len() })
}
