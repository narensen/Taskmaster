//! Git operations the daemon needs. Identity is never written; a repository without a
//! local `user.name` and `user.email` is refused rather than given one.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

/// [`git`], with the output as bytes: a file's content, which need not be text.
pub fn git_bytes(repo: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().context("failed to run git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

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

/// The checked-out branch name, if the project is a git repo and HEAD is not detached.
pub fn current_branch(repo: &Path) -> Option<String> {
    let name = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).ok()?.trim().to_string();
    if name.is_empty() || name == "HEAD" {
        None
    } else {
        Some(name)
    }
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
        ".taskmaster/races/",
        ".taskmaster/dictionary/",
        ".taskmaster/commons/",
        ".taskmaster/evals/",
        ".taskmaster/skills/",
        ".taskmaster/errorography.json",
        ".taskmaster/citations.json",
        ".taskmaster/pairing.json",
        ".taskmaster/devices.json",
        ".taskmaster/config.json",
        ".taskmaster/hosts.json",
        ".taskmaster/taskmaster.db*",
        ".taskmaster/reviews/jobs/",
        ".flow/index.sqlite*",
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
    let mut args = vec!["ls-files".to_string(), "--others".into(), "--exclude-standard".into(), "-z".into()];
    args.extend(ARTIFACTS.iter().map(|pattern| format!("--exclude={pattern}")));
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let untracked = git(repo, &refs)?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        if !changes.iter().any(|c| c.path == path) {
            changes.push(Change { status: 'A', path: path.into() });
        }
    }
    changes.retain(|c| c.status != 'A' || !is_artifact(&c.path));
    Ok(changes)
}

/// Whether a change in `worktree` leaves the file as it already is in `tree`: the same
/// bytes, or deleted where `tree` does not have it either.
pub fn same_as(worktree: &Path, tree: &Path, change: &Change) -> bool {
    let theirs = std::fs::read(tree.join(&change.path));
    if change.status == 'D' {
        return theirs.is_err();
    }
    match (std::fs::read(worktree.join(&change.path)), theirs) {
        (Ok(mine), Ok(theirs)) => mine == theirs,
        _ => false,
    }
}

/// One file updated or deleted in a worktree by [`sync_worktree_excluding`], together with the
/// SHA-256 digest of the bytes written (or `None` for a deletion) so downstream ownership checks
/// can distinguish daemon-synced sibling files from worker edits even if `tree` later moves on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedFile {
    pub path: String,
    pub deleted: bool,
    pub sha256: Option<[u8; 32]>,
}

/// Summary of a [`sync_worktree_excluding`] pass.
#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    /// Relative paths whose on-disk content changed during this sync pass.
    pub updated: Vec<String>,
    /// All non-excluded files represented in `base..HEAD` with their synced content digests.
    pub snapshots: Vec<SyncedFile>,
}

static SYNC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn compile_exclude_globs(globs: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    let mut any = false;
    for glob in globs {
        for alt in tm_core::glob_alternatives(glob) {
            let pat = alt.trim().trim_start_matches("./");
            if pat.is_empty() {
                continue;
            }
            if let Ok(compiled) = globset::Glob::new(pat) {
                builder.add(compiled);
                any = true;
            }
        }
    }
    if any { builder.build().ok() } else { None }
}

fn is_flow_sqlite(path: &str) -> bool {
    let norm = path.trim_start_matches("./");
    norm.starts_with(".flow/index.sqlite")
        || Path::new(norm)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("index.sqlite") && norm.starts_with(".flow/"))
}

fn atomic_write_bytes(src: &Path, dest: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let seq = SYNC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".tm-sync-{}-{seq}.tmp", std::process::id()));
    let write_res = (|| -> anyhow::Result<()> {
        std::fs::write(&tmp, bytes)?;
        if let Ok(meta) = std::fs::metadata(src) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, dest)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_res
}

/// Bring `worktree` up to date with `tree` by atomically copying every file that changed in
/// `tree` since `worktree`'s commit, skipping `.flow/index.sqlite*` and any path matching
/// `exclude_globs` (e.g. an active worker's `owned_glob` plus independent same/later-phase
/// sibling globs). Files whose on-disk bytes already match `tree` are left untouched so
/// timestamps and test caches are preserved.
pub fn sync_worktree_excluding_globs(
    worktree: &Path,
    tree: &Path,
    exclude_globs: &[String],
) -> anyhow::Result<SyncReport> {
    let base = head(worktree)?;
    let exclude = compile_exclude_globs(exclude_globs);
    let changed = git(tree, &["diff", "--no-renames", "--name-status", "-z", &base, "HEAD"])?;
    let mut report = SyncReport::default();
    for change in parse_name_status(&changed) {
        if !safe_rel(&change.path) || is_flow_sqlite(&change.path) {
            continue;
        }
        if exclude.as_ref().is_some_and(|set| set.is_match(&change.path)) {
            continue;
        }
        let dest = worktree.join(&change.path);
        if change.status == 'D' {
            if dest.exists() {
                let _ = std::fs::remove_file(&dest);
                report.updated.push(change.path.clone());
            }
            report.snapshots.push(SyncedFile { path: change.path, deleted: true, sha256: None });
            continue;
        }
        let src = tree.join(&change.path);
        let src_bytes = match std::fs::read(&src) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let digest: [u8; 32] = Sha256::digest(&src_bytes).into();
        let already_same = std::fs::read(&dest).ok().as_deref() == Some(src_bytes.as_slice());
        if !already_same {
            atomic_write_bytes(&src, &dest, &src_bytes)
                .with_context(|| format!("cannot sync {}", change.path))?;
            report.updated.push(change.path.clone());
        }
        report.snapshots.push(SyncedFile { path: change.path, deleted: false, sha256: Some(digest) });
    }
    Ok(report)
}

/// Convenience wrapper around [`sync_worktree_excluding_globs`] for a single optional exclude glob.
pub fn sync_worktree_excluding(
    worktree: &Path,
    tree: &Path,
    exclude_glob: Option<&str>,
) -> anyhow::Result<SyncReport> {
    let globs: Vec<String> = exclude_glob.into_iter().map(str::to_string).collect();
    sync_worktree_excluding_globs(worktree, tree, &globs)
}

/// Bring a worktree kept open for a follow-up up to date with `tree`: every file that
/// changed in `tree` since the worktree's commit is copied over (or removed). The worktree's
/// own edits to other files stay. Only plain file writes; nothing in git moves.
pub fn sync_worktree(worktree: &Path, tree: &Path) -> anyhow::Result<()> {
    sync_worktree_excluding(worktree, tree, None).map(|_| ())
}

/// Reset a reusable worktree cleanly to `tree`'s exact `HEAD` commit and remove any untracked
/// files left by the previous unit, so a warm ACP session can safely build a subsequent unit
/// without inheriting stale edits or stray files.
pub fn reset_worktree(worktree: &Path, tree: &Path) -> anyhow::Result<()> {
    if worktree == tree {
        bail!("cannot reset the main run tree as a reusable worker worktree");
    }
    let target = head(tree)?;
    git(worktree, &["reset", "--hard", &target])?;
    git(worktree, &["clean", "-fd"])?;
    // Drop any derived `.flow/index.sqlite*` left behind so nothing reads a stale index.
    let flow_dir = worktree.join(".flow");
    if flow_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(&flow_dir)
    {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("index.sqlite") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// What running a project's tests or installing its packages leaves in a worktree. A worker
/// with a shell makes these; they are never its work, so they are never merged or counted.
pub const ARTIFACTS: [&str; 15] = [
    "__pycache__/",
    "*.pyc",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".hypothesis/",
    ".tox/",
    ".nox/",
    ".venv/",
    "venv/",
    "*.egg-info/",
    ".coverage",
    "htmlcov/",
    "node_modules/",
    "target/",
];

/// Whether a new file is a build or test leftover (see [`ARTIFACTS`]).
pub fn is_artifact(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let name = parts.last().copied().unwrap_or("");
    dirs.iter().any(|d| {
        ["__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".hypothesis", ".tox", ".nox", ".venv", "venv", "htmlcov", "node_modules", "target"].contains(d)
            || d.ends_with(".egg-info")
    }) || name.ends_with(".pyc")
        || name == ".coverage"
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
    let is_gen = |path: &str| {
        generated.iter().any(|g| if g.ends_with('/') { path.starts_with(g) } else { path == *g })
    };
    if paths.is_empty() || !paths.iter().all(|path| is_gen(path)) {
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
    tm_core::safe_rel(path)
}

fn link_dir(src: &Path, dst: &Path) {
    if !src.is_dir() || dst.exists() || dst.symlink_metadata().is_ok() {
        return;
    }
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(src, dst);
    #[cfg(windows)]
    let _ = std::os::windows::fs::symlink_dir(src, dst);
}

/// Let a checkout use the project's installed packages: they are not in git. Links are
/// made only when the project has the dependency cache and the checkout does not. Windows
/// allows directory links only with Developer Mode on or as an administrator; without it
/// the checkout goes without, as it would with no packages installed.
pub fn borrow_node_modules(project: &Path, tree: &Path) {
    for name in ["node_modules", ".venv", "venv"] {
        link_dir(&project.join(name), &tree.join(name));
    }
    // Monorepos often keep frontend/backend packages one level down (e.g. `web/node_modules`).
    let Ok(entries) = std::fs::read_dir(project) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if s.starts_with('.') || matches!(s, "node_modules" | "target" | "venv" | "dist" | "build") {
            continue;
        }
        let sub_src = entry.path().join("node_modules");
        let sub_tree = tree.join(s);
        if sub_src.is_dir() && sub_tree.is_dir() {
            link_dir(&sub_src, &sub_tree.join("node_modules"));
        }
    }
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

/// Create a new git repository at `path` and make an empty initial commit so later
/// runs have a `HEAD` to branch from. Identity is copied from `from` when that
/// repository (or the process's git config) has one; this never invents a name.
pub fn init_repo(path: &Path, from: Option<&Path>) -> anyhow::Result<()> {
    if path.join(".git").exists() {
        bail!("{} is already a git repository", path.display());
    }
    git(path, &["init", "-q", "-b", "main"])?;
    if let Some(from) = from {
        if let Ok(name) = git(from, &["config", "user.name"]) {
            let name = name.trim();
            if !name.is_empty() {
                git(path, &["config", "user.name", name])?;
            }
        }
        if let Ok(email) = git(from, &["config", "user.email"]) {
            let email = email.trim();
            if !email.is_empty() {
                git(path, &["config", "user.email", email])?;
            }
        }
    }
    git(path, &["commit", "--allow-empty", "-q", "-m", "Start of the project."])?;
    let _ = ensure_exclude(path);
    Ok(())
}

/// A remote address this daemon will store. An `https` address with a user or token in it
/// is refused, so a secret is never written into git config.
pub fn check_remote_url(url: &str) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() || url.len() > 300 || url.chars().any(|c| c.is_whitespace() || c.is_control()) || url.contains(['?', '#', '\\']) {
        return Err("a remote address is one line, up to 300 characters, with no query or spaces".into());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() || host.contains('@') {
            return Err("paste the address without a token in it".into());
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        if rest.is_empty() {
            return Err("that is not a git address".into());
        }
        return Ok(());
    }
    if url.starts_with("git@") && url.contains(':') {
        return Ok(());
    }
    // A folder on this machine. The daemon already reads the disk it is running on.
    if url.starts_with('/') && !url.contains("..") && is_repo(Path::new(url)) {
        return Ok(());
    }
    Err("a remote address starts with https://, ssh://, or git@, or is a folder on this machine".into())
}

pub fn check_remote_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with(['.', '-'])
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("a remote name uses letters, digits, `.`, `_` and `-`".into());
    }
    Ok(())
}

/// A branch name that can be given to `git push` without being read as a flag.
pub fn check_branch_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || name.starts_with('-')
        || name.ends_with(['/', '.'])
        || name.contains("..")
        || name.chars().any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\@".contains(c))
    {
        return Err("that is not a branch name".into());
    }
    Ok(())
}

/// `origin` and the rest, as `(name, url)`.
pub fn remotes(repo: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let text = git(repo, &["remote", "-v"])?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url)) = (parts.next(), parts.next()) else { continue };
        if parts.next() == Some("(push)") || !out.iter().any(|(have, _)| have == name) {
            if let Some(slot) = out.iter_mut().find(|(have, _)| have == name) {
                slot.1 = url.to_string();
            } else {
                out.push((name.to_string(), url.to_string()));
            }
        }
    }
    Ok(out)
}

pub fn dirty(repo: &Path) -> bool {
    git(repo, &["status", "--porcelain"]).ok().is_some_and(|text| !text.trim().is_empty())
}

/// Add `name` at `url`, or leave it when it already points there.
pub fn ensure_remote(repo: &Path, name: &str, url: &str) -> anyhow::Result<()> {
    match remotes(repo)?.into_iter().find(|(have, _)| have == name) {
        Some((_, current)) if current == url => Ok(()),
        Some((_, current)) => bail!("remote `{name}` already points at {current}"),
        None => {
            git(repo, &["remote", "add", name, url])?;
            Ok(())
        }
    }
}

/// Push `branch` to `remote`. `basic` is sent as one request header for this push
/// and is not written into the repository.
pub fn push(repo: &Path, remote: &str, branch: &str, basic: Option<&str>) -> anyhow::Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo);
    if let Some(header) = basic {
        command.arg("-c").arg(format!("http.extraHeader=Authorization: Basic {header}"));
    }
    command.args(["push", "-u", remote, branch]).stdin(std::process::Stdio::null());
    let out = command.output().context("failed to run git")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = match basic {
            Some(secret) => err.replace(secret, "***"),
            None => err.into_owned(),
        };
        bail!("git push failed: {}", err.trim());
    }
    Ok(())
}

/// Clone `url` into `dest`. `basic` is a one-shot request header, not stored.
pub fn clone_repo(url: &str, dest: &Path, basic: Option<&str>) -> anyhow::Result<()> {
    let mut command = Command::new("git");
    if let Some(header) = basic {
        command.arg("-c").arg(format!("http.extraHeader=Authorization: Basic {header}"));
    }
    command.args(["clone", "--quiet", url]).arg(dest).stdin(std::process::Stdio::null());
    let out = command.output().context("failed to run git")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = match basic {
            Some(secret) => err.replace(secret, "***"),
            None => err.into_owned(),
        };
        bail!("git clone failed: {}", err.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_is_added_once_and_a_token_in_the_address_is_refused() {
        let root = std::env::temp_dir().join(format!("tm-git-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&root).status().unwrap();
        git(&root, &["config", "user.name", "Test"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        git(&root, &["commit", "--allow-empty", "-q", "-m", "start"]).unwrap();
        assert!(!dirty(&root));
        std::fs::write(root.join("note.txt"), "hi").unwrap();
        assert!(dirty(&root));
        ensure_remote(&root, "origin", "https://github.com/ada/shop.git").unwrap();
        ensure_remote(&root, "origin", "https://github.com/ada/shop.git").unwrap();
        let err = ensure_remote(&root, "origin", "https://gitlab.com/ada/shop.git").unwrap_err().to_string();
        assert!(err.contains("already points"), "{err}");
        assert!(!err.contains("secret"));
        assert_eq!(remotes(&root).unwrap(), vec![("origin".into(), "https://github.com/ada/shop.git".into())]);
        assert!(check_remote_url("https://user:secret@github.com/ada/shop.git").is_err());
        assert!(check_remote_url("git@github.com:ada/shop.git").is_ok());
        assert!(check_remote_url("ssh://git@github.com/ada/shop.git").is_ok());
        assert!(check_remote_url("http://github.com/ada/shop.git").is_err());
        let local = root.canonicalize().unwrap();
        assert!(check_remote_url(&local.display().to_string()).is_ok());
        assert!(check_remote_url("/tmp").is_err());
        assert!(check_branch_name("-main").is_err());
        assert!(check_remote_name(".origin").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_runs_and_installs_leave_nothing_a_worker_is_charged_with() {
        let root = std::env::temp_dir().join(format!("tm-git-artifacts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&root).status().unwrap();
        git(&root, &["config", "user.name", "Test"]).unwrap();
        git(&root, &["config", "user.email", "test@example.com"]).unwrap();
        git(&root, &["commit", "--allow-empty", "-q", "-m", "start"]).unwrap();
        for (path, body) in [
            ("db/engine.py", "x = 1\n"),
            ("db/__pycache__/engine.cpython-312.pyc", "\0"),
            ("tests/test_engine.py", "def test(): pass\n"),
            (".pytest_cache/v/cache/lastfailed", "{}"),
            (".venv/bin/python", ""),
            ("pkg.egg-info/PKG-INFO", ""),
            ("node_modules/x/index.js", ""),
            (".coverage", ""),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, body).unwrap();
        }
        let mut found: Vec<String> = worktree_changes(&root).unwrap().into_iter().map(|c| c.path).collect();
        found.sort();
        assert_eq!(found, vec!["db/engine.py".to_string(), "tests/test_engine.py".to_string()]);
        assert!(is_artifact("a/b/__pycache__/c.pyc") && is_artifact("target/debug/x") && !is_artifact("src/target.rs"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reset_worktree_brings_reused_worktree_to_clean_head_and_drops_prior_unit_files() {
        let root = std::env::temp_dir().join(format!("tm-git-reset-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let wt = root.join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&repo).status().unwrap();
        git(&repo, &["config", "user.name", "Test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("base.py"), "BASE = 1\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        let h1 = head(&repo).unwrap();
        add_worktree(&repo, &wt, &h1).unwrap();

        // Simulate unit 1 editing a tracked file, writing an untracked file, and leaving a derived .flow sqlite.
        std::fs::write(wt.join("base.py"), "BASE = 999\n").unwrap();
        std::fs::write(wt.join("stray_unit1.py"), "X = 1\n").unwrap();
        std::fs::create_dir_all(wt.join(".flow")).unwrap();
        std::fs::write(wt.join(".flow/index.sqlite"), "stale").unwrap();

        // Meanwhile main tree merges unit 1's real file at a newer commit.
        std::fs::write(repo.join("unit1.py"), "U1 = 1\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-q", "-m", "unit 1"]).unwrap();

        reset_worktree(&wt, &repo).unwrap();
        assert_eq!(head(&wt).unwrap(), head(&repo).unwrap());
        assert_eq!(std::fs::read_to_string(wt.join("base.py")).unwrap(), "BASE = 1\n");
        assert_eq!(std::fs::read_to_string(wt.join("unit1.py")).unwrap(), "U1 = 1\n");
        assert!(!wt.join("stray_unit1.py").exists());
        assert!(!wt.join(".flow/index.sqlite").exists());
        assert!(worktree_changes(&wt).unwrap().is_empty());
        assert!(reset_worktree(&repo, &repo).is_err());

        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tb18_sync_worktree_excluding_preserves_owned_files_atomically() {
        let root = std::env::temp_dir().join(format!("tm-git-sync-excl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let wt = root.join("wt");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&repo).status().unwrap();
        git(&repo, &["config", "user.name", "Test"]).unwrap();
        git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("pkg/__init__.py"), "").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        let h1 = head(&repo).unwrap();
        add_worktree(&repo, &wt, &h1).unwrap();

        // Active worker owns `pkg/cli.py` and `tests/test_cli.py`, and writes local work.
        std::fs::write(wt.join("pkg/cli.py"), "MY_WIP = 42\n").unwrap();

        // Meanwhile sibling lands `pkg/parser.py`, `pkg/cli.py` (hypothetically), and `.flow/index.sqlite`.
        std::fs::write(repo.join("pkg/parser.py"), "def parse(s): return s\n").unwrap();
        std::fs::write(repo.join("pkg/cli.py"), "SHOULD_NOT_OVERWRITE = 1\n").unwrap();
        std::fs::create_dir_all(repo.join(".flow")).unwrap();
        std::fs::write(repo.join(".flow/index.sqlite"), "binary").unwrap();
        git(&repo, &["add", "pkg/parser.py", "pkg/cli.py", "-f", ".flow/index.sqlite"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "sibling landed"]).unwrap();

        let report = sync_worktree_excluding(&wt, &repo, Some("{pkg/cli.py,tests/test_cli.py}")).unwrap();
        assert_eq!(report.updated, vec!["pkg/parser.py".to_string()]);
        assert_eq!(std::fs::read_to_string(wt.join("pkg/parser.py")).unwrap(), "def parse(s): return s\n");
        assert_eq!(std::fs::read_to_string(wt.join("pkg/cli.py")).unwrap(), "MY_WIP = 42\n");
        assert!(!wt.join(".flow/index.sqlite").exists());

        // Second sync with identical bytes is a no-op (no updated paths, snapshots retained).
        let report2 = sync_worktree_excluding(&wt, &repo, Some("{pkg/cli.py,tests/test_cli.py}")).unwrap();
        assert!(report2.updated.is_empty());
        assert_eq!(report2.snapshots.len(), 1);
        assert_eq!(report2.snapshots[0].path, "pkg/parser.py");
        assert!(report2.snapshots[0].sha256.is_some());

        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&root);
    }
}
