//! Git operations the daemon needs. Identity is never written; a repository without a
//! local `user.name` and `user.email` is refused rather than given one.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

pub fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn is_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Refuse to commit into a repository that has no author. This does not set one.
pub fn require_identity(repo: &Path) -> anyhow::Result<()> {
    let ok = |key: &str| git(repo, &["config", key]).ok().is_some_and(|v| !v.trim().is_empty());
    if ok("user.name") && ok("user.email") {
        return Ok(());
    }
    bail!(
        "this repository has no git user.name and user.email; set them locally (`git config user.name` \
         and `git config user.email`) before running the daemon"
    )
}

pub fn head(repo: &Path) -> anyhow::Result<String> {
    Ok(git(repo, &["rev-parse", "HEAD"])?.trim().to_string())
}

pub fn has_head(repo: &Path) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok()
}

/// Keep the daemon's own run state and dictionary out of `git status`. Local exclude only;
/// the project's committed gitignore is left alone. Architecture and change docs stay visible.
pub fn ensure_exclude(repo: &Path) -> anyhow::Result<()> {
    let path = repo.join(".git/info/exclude");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    let mut changed = false;
    for line in [
        ".taskmaster/runs/",
        ".taskmaster/dictionary/",
        ".taskmaster/commons/",
        ".taskmaster/evals/",
        ".taskmaster/errorography.json",
        ".taskmaster/citations.json",
        ".taskmaster/pairing.json",
        ".taskmaster/devices.json",
    ] {
        if !next.lines().any(|have| have == line) {
            if !changed {
                next.push_str("# taskmaster\n");
                changed = true;
            }
            next.push_str(line);
            next.push('\n');
        }
    }
    if changed {
        std::fs::write(path, next)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// `A` added, `M` modified, `D` deleted.
    pub status: char,
    pub path: String,
}

/// Staged and unstaged edits plus untracked files, against the worktree's `HEAD`.
pub fn worktree_changes(repo: &Path) -> anyhow::Result<Vec<Change>> {
    let mut changes = parse_name_status(&git(repo, &["diff", "--no-renames", "--name-status", "-z", "HEAD"])?);
    let untracked = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        if !changes.iter().any(|c| c.path == path) {
            changes.push(Change { status: 'A', path: path.into() });
        }
    }
    Ok(changes)
}

fn parse_name_status(out: &str) -> Vec<Change> {
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    let mut changes = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        changes.push(Change { status: status.chars().next().unwrap_or('M'), path: path.into() });
    }
    changes
}

pub fn add_worktree(repo: &Path, path: &Path, head: &str) -> anyhow::Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    if head.starts_with('-') {
        bail!("invalid revision");
    }
    git(repo, &["worktree", "add", "--detach", "--quiet", path, head]).map(|_| ())
}

pub fn remove_worktree(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let Some(path) = path.to_str() else { return Ok(()) };
    git(repo, &["worktree", "remove", "--force", path]).map(|_| ())
}

/// Stage `paths` and commit them. `Ok(None)` when there is nothing new to commit.
pub fn commit_paths(repo: &Path, message: &str, paths: &[String]) -> anyhow::Result<Option<String>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let mut add = vec!["add".to_string(), "--".to_string()];
    add.extend(paths.iter().cloned());
    let add_ref: Vec<&str> = add.iter().map(String::as_str).collect();
    git(repo, &add_ref)?;
    let staged = git(repo, &["diff", "--cached", "--name-only"])?;
    if staged.trim().is_empty() {
        return Ok(None);
    }
    git(repo, &["commit", "-q", "-m", message])?;
    Ok(Some(head(repo)?))
}

/// A relative path that cannot climb out of the project.
pub fn safe_rel(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute()
        && path.components().all(|c| matches!(c, std::path::Component::Normal(_)))
        && !path.as_os_str().is_empty()
}

pub fn worktree_dir(project: &Path, run_id: &str, label: &str) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    project.hash(&mut hasher);
    let slug: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("tm-daemon-{:x}-{}-{}", hasher.finish(), run_id, slug))
}
