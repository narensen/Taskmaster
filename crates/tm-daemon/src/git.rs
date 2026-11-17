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
        ".taskmaster/config.json",
        ".taskmaster/taskmaster.db*",
        ".taskmaster/reviews/jobs/",
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

/// Every file committed in the worktree's index, relative to its root.
pub fn tracked_files(repo: &Path) -> anyhow::Result<Vec<String>> {
    Ok(git(repo, &["ls-files", "-z"])?.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
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

/// Everything changed in a scratch worktree as one diff, new files included. Stages the
/// changes, so call it only on a worktree that is about to be thrown away.
pub fn worktree_diff(repo: &Path) -> anyhow::Result<String> {
    git(repo, &["add", "-A"])?;
    git(repo, &["diff", "--cached", "--no-color", "--no-ext-diff", "HEAD"])
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

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

/// Check `branch` out at `path`, creating it at `base` when it does not exist yet.
pub fn open_branch(repo: &Path, path: &Path, branch: &str, base: &str) -> anyhow::Result<()> {
    if base.starts_with('-') || branch.starts_with('-') {
        bail!("invalid revision");
    }
    // A checkout that was deleted from disk still holds its branch until it is pruned.
    let _ = git(repo, &["worktree", "prune"]);
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    if branch_exists(repo, branch) {
        git(repo, &["worktree", "add", "--quiet", path, branch]).map(|_| ())
    } else {
        git(repo, &["worktree", "add", "--quiet", "-b", branch, path, base]).map(|_| ())
    }
}

/// Bring `branch` into whatever the project has checked out. Fast-forward when the
/// project has not moved, a merge commit when it has. `false` when git refused, which
/// leaves the project exactly as it was.
pub fn merge_branch(repo: &Path, branch: &str, generated: &[&str]) -> bool {
    if git(repo, &["merge", "--ff-only", "--quiet", branch]).is_ok() {
        return true;
    }
    let message = format!("taskmaster: merge {branch}");
    if git(repo, &["merge", "--no-edit", "--quiet", "-m", &message, branch]).is_ok() {
        return true;
    }
    if settle_generated(repo, generated) {
        return true;
    }
    let _ = git(repo, &["merge", "--abort"]);
    false
}

/// Two runs that finish side by side both rewrite the generated documents, and git sees
/// that as a conflict. When those are the only conflicts, the incoming copy is taken:
/// it describes the run being merged, which is what the files are for.
fn settle_generated(repo: &Path, generated: &[&str]) -> bool {
    let Ok(unmerged) = git(repo, &["diff", "--name-only", "--diff-filter=U", "-z"]) else { return false };
    let paths: Vec<&str> = unmerged.split('\0').filter(|path| !path.is_empty()).collect();
    if paths.is_empty() || !paths.iter().all(|path| generated.contains(path)) {
        return false;
    }
    let mut take = vec!["checkout", "--theirs", "--"];
    take.extend(&paths);
    let mut add = vec!["add", "--"];
    add.extend(&paths);
    git(repo, &take).is_ok() && git(repo, &add).is_ok() && git(repo, &["commit", "-q", "--no-edit"]).is_ok()
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

/// The directory every worktree of `project` is made in: `<temp>/tm-daemon-<hash>-`
/// followed by the run and the label.
fn worktree_prefix(project: &Path) -> String {
    let dir = worktree_dir(project, "", "");
    dir.file_name().map(|n| n.to_string_lossy().trim_end_matches('-').to_string() + "-").unwrap_or_default()
}

/// If `path` is inside one of this project's run worktrees, the worktree's directory and
/// the path inside it. Agents see their worktree as the whole project, so the paths they
/// report are absolute paths in it, and mean the same file in the project.
pub fn in_worktree(project: &Path, path: &Path) -> Option<(PathBuf, PathBuf)> {
    let prefix = worktree_prefix(project);
    let temp = std::env::temp_dir();
    // The temp directory may itself be reached through a link (`/tmp` on a Mac).
    for base in [Some(temp.clone()), temp.canonicalize().ok()].into_iter().flatten() {
        let Ok(rest) = path.strip_prefix(&base) else { continue };
        let mut parts = rest.components();
        let Some(first) = parts.next() else { continue };
        let name = first.as_os_str().to_string_lossy();
        if !name.starts_with(&prefix) {
            continue;
        }
        let inner: PathBuf = parts.collect();
        if inner.as_os_str().is_empty() || !inner.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
            return None;
        }
        return Some((base.join(first.as_os_str()), inner));
    }
    None
}

/// `text` with every absolute worktree path of this project cut back to a path in the
/// project, for output an agent or a tool produced inside a worktree.
pub fn unroot_worktrees(project: &Path, text: &str) -> String {
    let prefix = worktree_prefix(project);
    let temp = std::env::temp_dir();
    let mut bases = vec![temp.display().to_string()];
    if let Ok(real) = temp.canonicalize() {
        bases.push(real.display().to_string());
    }
    let mut out = text.to_string();
    for base in bases {
        let needle = format!("{}/{prefix}", base.trim_end_matches('/'));
        while let Some(at) = out.find(&needle) {
            let after = at + needle.len();
            let end = out[after..].find('/').map(|i| after + i + 1).unwrap_or(out.len());
            out.replace_range(at..end, "");
        }
    }
    out
}
