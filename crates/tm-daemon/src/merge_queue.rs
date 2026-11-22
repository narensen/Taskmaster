//! Serialized merge queue with semantic conflict verification when base has moved (Item 3.4).
//!
//! Concurrent runs sharing one repository acquire the per-project merge lock one at a time.
//! If the base branch `HEAD` moved since the run started (`run_base`), the queue merges the
//! run branch into a temporary detached worktree, invokes the caller's check callback on the
//! combined tree, and only advances the base branch when the combined result passes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use tokio::sync::Mutex as AsyncMutex;

static PROJECT_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeQueueOutcome {
    /// Merged cleanly into the base branch (returning the new HEAD commit SHA).
    Merged { commit: String, rechecked_on_moved_base: bool },
    /// Refused because combining with the moved base branch failed git merge or checks.
    Conflict { reason: String },
}

/// Obtain the serialized async merge lock for `project`.
pub fn lock_for(project: &Path) -> Arc<AsyncMutex<()>> {
    let key = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
    let mut map = PROJECT_LOCKS.lock().unwrap();
    map.entry(key).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
}

/// Serialize a merge of `run_branch` into `base_branch` for `project`.
///
/// When `base_branch`'s current commit differs from `started_base`, the combined tree is
/// materialized in a temporary git worktree and validated by `check_combined(&temp_tree)`.
/// If `check_combined` returns `Err(msg)`, `base_branch` is left completely untouched.
pub async fn merge_serialized<F>(
    project: &Path,
    base_branch: &str,
    started_base: &str,
    run_branch: &str,
    check_combined: F,
) -> anyhow::Result<MergeQueueOutcome>
where
    F: Fn(&Path) -> Result<(), String>,
{
    let lock = lock_for(project);
    let _guard = lock.lock().await;

    let current_head = crate::git::git(project, &["rev-parse", base_branch])?.trim().to_string();
    let base_moved = !started_base.is_empty() && current_head != started_base.trim();

    if base_moved {
        // Test the combined merge inside a temporary detached worktree first.
        let tmp = std::env::temp_dir().join(format!(
            "tm-mergeq-{}-{}",
            std::process::id(),
            crate::secret::random_hex(4).unwrap_or_else(|_| "0000".into())
        ));
        let tmp_str = tmp.to_string_lossy().to_string();
        crate::git::git(project, &["worktree", "add", "--detach", &tmp_str, &current_head])?;

        let outcome = (|| -> Result<(), String> {
            if let Err(err) = crate::git::git(&tmp, &["merge", "--no-ff", "-m", "merge queue check", run_branch]) {
                let _ = crate::git::git(&tmp, &["merge", "--abort"]);
                return Err(format!("git merge conflict against moved base {base_branch}: {err}"));
            }
            check_combined(&tmp)
        })();

        let _ = crate::git::git(project, &["worktree", "remove", "--force", &tmp_str]);
        let _ = std::fs::remove_dir_all(&tmp);

        if let Err(reason) = outcome {
            return Ok(MergeQueueOutcome::Conflict { reason });
        }
    }

    // Now perform the actual merge on the base branch.
    crate::git::git(project, &["checkout", base_branch])?;
    if let Err(err) = crate::git::git(project, &["merge", "--no-ff", "-m", &format!("Merge {run_branch}"), run_branch]) {
        let _ = crate::git::git(project, &["merge", "--abort"]);
        return Ok(MergeQueueOutcome::Conflict {
            reason: format!("merge failed: {err}"),
        });
    }
    let commit = crate::git::git(project, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok(MergeQueueOutcome::Merged { commit, rechecked_on_moved_base: base_moved })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_runs_that_pass_alone_but_break_together_do_not_both_merge() {
        let dir = std::env::temp_dir().join(format!("tm-mergeq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::git::git(&dir, &["init", "-b", "main"]).unwrap();
        crate::git::git(&dir, &["config", "user.email", "t@example.com"]).unwrap();
        crate::git::git(&dir, &["config", "user.name", "Test"]).unwrap();
        std::fs::write(dir.join("a.txt"), "ok\n").unwrap();
        std::fs::write(dir.join("b.txt"), "ok\n").unwrap();
        crate::git::git(&dir, &["add", "."]).unwrap();
        crate::git::git(&dir, &["commit", "-m", "base"]).unwrap();
        let base = crate::git::git(&dir, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        // Branch 1 sets a.txt = "x"
        crate::git::git(&dir, &["checkout", "-b", "run-1", &base]).unwrap();
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        crate::git::git(&dir, &["commit", "-am", "run 1"]).unwrap();

        // Branch 2 sets b.txt = "x" (non-overlapping files, so git merge succeeds cleanly!)
        crate::git::git(&dir, &["checkout", "-b", "run-2", &base]).unwrap();
        std::fs::write(dir.join("b.txt"), "x\n").unwrap();
        crate::git::git(&dir, &["commit", "-am", "run 2"]).unwrap();
        crate::git::git(&dir, &["checkout", "main"]).unwrap();

        // Semantic invariant: a.txt and b.txt must not BOTH be "x" at the same time.
        let check = |tree: &Path| -> Result<(), String> {
            let a = std::fs::read_to_string(tree.join("a.txt")).unwrap_or_default();
            let b = std::fs::read_to_string(tree.join("b.txt")).unwrap_or_default();
            if a.trim() == "x" && b.trim() == "x" {
                Err("semantic conflict: a.txt and b.txt cannot both be x".into())
            } else {
                Ok(())
            }
        };

        // Run 1 merges cleanly (base has not moved).
        let first = merge_serialized(&dir, "main", &base, "run-1", check).await.unwrap();
        assert!(matches!(first, MergeQueueOutcome::Merged { rechecked_on_moved_base: false, .. }));

        // Run 2 detects moved base, checks combined tree, catches semantic conflict, and refuses!
        let second = merge_serialized(&dir, "main", &base, "run-2", check).await.unwrap();
        assert!(
            matches!(second, MergeQueueOutcome::Conflict { ref reason } if reason.contains("semantic conflict")),
            "{second:?}"
        );
        // Main still has b.txt == "ok" (untouched by failed run-2).
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap().trim(), "ok");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
