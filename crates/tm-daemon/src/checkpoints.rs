//! Hidden git checkpoints for rewinding a run turn without polluting branch history (A2).
//!
//! Before each turn starts, Taskmaster captures the checkout (including untracked files)
//! using a temporary `GIT_INDEX_FILE` (`git add -A`, `write-tree`, `commit-tree`) and
//! writes the commit to `refs/taskmaster/checkpoints/<run>/turn/<n>`. Restoring a turn
//! updates the working tree to match that snapshot without making a branch commit, and
//! removing a run prunes all of its hidden refs.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use serde::Serialize;

use crate::git;

#[derive(Debug, Clone, Serialize)]
pub struct CheckpointInfo {
    pub turn: usize,
    pub commit: String,
}

pub fn turn_ref(run: &str, turn: usize) -> String {
    format!("refs/taskmaster/checkpoints/{run}/turn/{turn}")
}

pub fn branch_ref(run: &str, turn: usize) -> String {
    format!("refs/taskmaster/checkpoints/{run}/branch/{turn}")
}

fn ref_prefix(run: &str) -> String {
    format!("refs/taskmaster/checkpoints/{run}/")
}

fn git_with_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_bytes_with_env(repo: &Path, args: &[&str], envs: &[(&str, &str)]) -> anyhow::Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// True when a checkpoint ref already exists for `(run, turn)`.
pub fn exists(repo: &Path, run: &str, turn: usize) -> bool {
    if !git::is_repo(repo) {
        return false;
    }
    git::git(repo, &["rev-parse", "--verify", "--quiet", &turn_ref(run, turn)]).is_ok()
}

/// Capture the checkout (tracked + untracked non-ignored files) into a hidden checkpoint ref
/// using a temporary `GIT_INDEX_FILE`. Never writes a commit onto the user's branch.
pub fn capture(repo: &Path, run: &str, turn: usize) -> anyhow::Result<Option<String>> {
    if !git::is_repo(repo) {
        return Ok(None);
    }
    let _ = git::ensure_exclude(repo);
    let rand = crate::secret::random_hex(4).unwrap_or_else(|_| "0".into());
    let tmp_idx = std::env::temp_dir().join(format!(
        "tm-ckpt-{}-{}-{}-{rand}.idx",
        std::process::id(),
        run.replace('/', "_"),
        turn
    ));
    let _ = std::fs::remove_file(&tmp_idx);
    let idx_str = tmp_idx.to_string_lossy().into_owned();
    let idx_env = [("GIT_INDEX_FILE", idx_str.as_str())];

    let tree_res = (|| -> anyhow::Result<String> {
        git_with_env(repo, &["add", "-A"], &idx_env)?;
        let tree = git_with_env(repo, &["write-tree"], &idx_env)?;
        Ok(tree.trim().to_string())
    })();
    let _ = std::fs::remove_file(&tmp_idx);
    let tree = tree_res?;

    let msg = format!("taskmaster checkpoint {run} turn {turn}");
    let mut args = vec!["commit-tree", &tree];
    let head_sha = git::head(repo).ok();
    if let Some(ref parent) = head_sha {
        args.push("-p");
        args.push(parent);
    }
    args.push("-m");
    args.push(&msg);

    // Supply fallback identity solely for the hidden checkpoint commit so capture works
    // even if global git user.name/email isn't configured in a test/temp repo.
    let commit = git_with_env(
        repo,
        &args,
        &[
            ("GIT_AUTHOR_NAME", "Taskmaster Checkpoint"),
            ("GIT_AUTHOR_EMAIL", "checkpoint@taskmaster.local"),
            ("GIT_COMMITTER_NAME", "Taskmaster Checkpoint"),
            ("GIT_COMMITTER_EMAIL", "checkpoint@taskmaster.local"),
        ],
    )?
    .trim()
    .to_string();

    let r = turn_ref(run, turn);
    git::git(repo, &["update-ref", &r, &commit])?;
    Ok(Some(commit))
}

/// Also record a run branch's commit SHA before `turn` so branched runs restore cleanly.
pub fn capture_branch(repo: &Path, run: &str, turn: usize, branch: &str) -> anyhow::Result<Option<String>> {
    if !git::is_repo(repo) {
        return Ok(None);
    }
    let full = format!("refs/heads/{branch}");
    let Ok(sha) = git::git(repo, &["rev-parse", "--verify", "--quiet", &full]) else {
        return Ok(None);
    };
    let sha = sha.trim().to_string();
    if sha.is_empty() {
        return Ok(None);
    }
    let r = branch_ref(run, turn);
    git::git(repo, &["update-ref", &r, &sha])?;
    Ok(Some(sha))
}

/// Restore the working tree of `repo` to match `refs/taskmaster/checkpoints/<run>/turn/<turn>`.
/// Removes non-ignored files created after the checkpoint and restores modified/deleted files,
/// while leaving `HEAD` and branch commits untouched.
pub fn restore(repo: &Path, run: &str, turn: usize) -> anyhow::Result<String> {
    if !git::is_repo(repo) {
        bail!("project is not a git repository");
    }
    let r = turn_ref(run, turn);
    let commit = git::git(repo, &["rev-parse", "--verify", "--quiet", &r])
        .with_context(|| format!("no checkpoint saved for turn {turn}"))?
        .trim()
        .to_string();

    // 1. Collect files present in the checkpoint tree.
    let ckpt_bytes = git::git_bytes(repo, &["ls-tree", "-r", "--name-only", "-z", &commit])?;
    let ckpt_files: HashSet<String> = ckpt_bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();

    // 2. Collect current tracked + untracked (non-ignored) files in the checkout.
    let cur_bytes = git::git_bytes(repo, &["ls-files", "--cached", "--others", "--exclude-standard", "-z"])?;
    for raw in cur_bytes.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        let rel = String::from_utf8_lossy(raw);
        if rel == ".git" || rel.starts_with(".git/") || rel == ".taskmaster" || rel.starts_with(".taskmaster/") {
            continue;
        }
        if !ckpt_files.contains(rel.as_ref()) {
            let full = repo.join(rel.as_ref());
            if full.is_file() || full.is_symlink() {
                let _ = std::fs::remove_file(&full);
                prune_empty_parents(repo, full.parent());
            }
        }
    }

    // 3. Extract all files from the checkpoint tree into the working tree using a temp index,
    //    so the real git index stays aligned with HEAD.
    if !ckpt_files.is_empty() {
        let rand = crate::secret::random_hex(4).unwrap_or_else(|_| "0".into());
        let tmp_idx = std::env::temp_dir().join(format!(
            "tm-restore-{}-{}-{}-{rand}.idx",
            std::process::id(),
            run.replace('/', "_"),
            turn
        ));
        let _ = std::fs::remove_file(&tmp_idx);
        let idx_str = tmp_idx.to_string_lossy().into_owned();
        let idx_env = [("GIT_INDEX_FILE", idx_str.as_str())];
        let res = (|| -> anyhow::Result<()> {
            git_with_env(repo, &["read-tree", &commit], &idx_env)?;
            git_bytes_with_env(repo, &["checkout-index", "-a", "-f"], &idx_env)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&tmp_idx);
        res?;
    }

    Ok(commit)
}

/// Restore a run's branch ref to its checkpoint before `turn`, if one was captured.
pub fn restore_branch(repo: &Path, run: &str, turn: usize, branch: &str) -> anyhow::Result<bool> {
    if !git::is_repo(repo) {
        return Ok(false);
    }
    let r = branch_ref(run, turn);
    let Ok(sha) = git::git(repo, &["rev-parse", "--verify", "--quiet", &r]) else {
        return Ok(false);
    };
    let sha = sha.trim();
    if sha.is_empty() {
        return Ok(false);
    }
    let full = format!("refs/heads/{branch}");
    git::git(repo, &["update-ref", &full, sha])?;
    Ok(true)
}

fn prune_empty_parents(root: &Path, mut dir: Option<&Path>) {
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        let is_empty = std::fs::read_dir(d).ok().and_then(|mut it| it.next()).is_none();
        if !is_empty {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// List all turn checkpoints currently saved for `run`, sorted by turn ascending.
pub fn list(repo: &Path, run: &str) -> Vec<CheckpointInfo> {
    if !git::is_repo(repo) {
        return Vec::new();
    }
    let prefix = format!("refs/taskmaster/checkpoints/{run}/turn/");
    let Ok(out) = git::git(
        repo,
        &["for-each-ref", "--format=%(refname) %(objectname)", &prefix],
    ) else {
        return Vec::new();
    };
    let mut items: Vec<CheckpointInfo> = out
        .lines()
        .filter_map(|line| {
            let (r, sha) = line.split_once(' ')?;
            let turn: usize = r.strip_prefix(&prefix)?.parse().ok()?;
            Some(CheckpointInfo { turn, commit: sha.trim().to_string() })
        })
        .collect();
    items.sort_by_key(|c| c.turn);
    items
}

/// Delete checkpoint refs for `run` whose turn number is `>= from_turn`.
pub fn prune_from(repo: &Path, run: &str, from_turn: usize) {
    if !git::is_repo(repo) {
        return;
    }
    let prefix = ref_prefix(run);
    let Ok(out) = git::git(repo, &["for-each-ref", "--format=%(refname)", &prefix]) else {
        return;
    };
    for r in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let turn_num = r
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<usize>().ok());
        if let Some(n) = turn_num
            && n >= from_turn
        {
            let _ = git::git(repo, &["update-ref", "-d", r]);
        }
    }
}

/// Delete all hidden checkpoint refs belonging to `run` (called when a run is removed).
pub fn prune_run(repo: &Path, run: &str) {
    if !git::is_repo(repo) {
        return;
    }
    let prefix = ref_prefix(run);
    let Ok(out) = git::git(repo, &["for-each-ref", "--format=%(refname)", &prefix]) else {
        return;
    };
    for r in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let _ = git::git(repo, &["update-ref", "-d", r]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tm-ckpt-test-{name}-{}-{}",
            std::process::id(),
            crate::secret::random_hex(3).unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git::git(&dir, &["init", "-q"]).unwrap();
        git::git(&dir, &["config", "user.name", "Tester"]).unwrap();
        git::git(&dir, &["config", "user.email", "t@example.com"]).unwrap();
        std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
        git::git(&dir, &["add", "a.txt"]).unwrap();
        git::git(&dir, &["commit", "-q", "-m", "init"]).unwrap();
        dir
    }

    #[test]
    fn captures_untracked_files_restores_cleanly_and_prunes() {
        let repo = temp_repo("roundtrip");
        let head_before = git::head(&repo).unwrap();
        // Add an untracked file before turn 2.
        std::fs::write(repo.join("untracked.txt"), "keep-me\n").unwrap();
        let c2 = capture(&repo, "r1", 2).unwrap().expect("checkpoint created");
        assert!(!c2.is_empty());
        // Branch HEAD must not move!
        assert_eq!(git::head(&repo).unwrap(), head_before);

        // Turn 2 mutates a.txt, deletes untracked.txt, and adds extra.txt.
        std::fs::write(repo.join("a.txt"), "v2-bad\n").unwrap();
        std::fs::remove_file(repo.join("untracked.txt")).unwrap();
        std::fs::write(repo.join("extra.txt"), "remove-me\n").unwrap();

        restore(&repo, "r1", 2).unwrap();
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "v1\n");
        assert_eq!(std::fs::read_to_string(repo.join("untracked.txt")).unwrap(), "keep-me\n");
        assert!(!repo.join("extra.txt").exists(), "post-checkpoint file should be removed");
        assert_eq!(git::head(&repo).unwrap(), head_before);

        assert_eq!(list(&repo, "r1").len(), 1);
        prune_run(&repo, "r1");
        assert!(list(&repo, "r1").is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
