//! Thin wrappers over the `git` command.

use std::path::Path;
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

/// Resolve a revision to a full commit hash. Refuses anything that could be read as an option.
pub fn resolve(repo: &Path, rev: &str) -> anyhow::Result<String> {
    if rev.starts_with('-') {
        bail!("invalid revision `{rev}`");
    }
    let commit = format!("{rev}^{{commit}}");
    Ok(git(repo, &["rev-parse", "--verify", "--quiet", &commit])
        .with_context(|| format!("`{rev}` is not a commit in this repository"))?
        .trim()
        .to_string())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// `A` added, `M` modified, `D` deleted.
    pub status: char,
    pub path: String,
}

pub fn changed_files(repo: &Path, base: &str, head: &str) -> anyhow::Result<Vec<Change>> {
    let out = git(repo, &["diff", "--no-renames", "--name-status", "-z", base, head])?;
    Ok(parse_name_status(&out))
}

/// Staged and unstaged edits, plus untracked files, against `HEAD`.
pub fn working_tree_changes(repo: &Path) -> anyhow::Result<Vec<Change>> {
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

/// Copy uncommitted work into a detached checkout without touching the user's tree.
pub fn apply_working_tree(repo: &Path, worktree: &Path) -> anyhow::Result<()> {
    let patch = git(repo, &["diff", "--binary", "HEAD"])?;
    if !patch.trim().is_empty() {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["apply", "--binary", "--whitespace=nowarn"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to start git apply")?;
        use std::io::Write;
        child.stdin.take().context("git apply stdin")?.write_all(patch.as_bytes())?;
        let out = child.wait_with_output().context("git apply")?;
        if !out.status.success() {
            bail!("could not apply uncommitted changes: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    let untracked = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        let from = repo.join(path);
        if !from.is_file() {
            continue;
        }
        let to = worktree.join(path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &to).with_context(|| format!("failed to copy untracked file {path}"))?;
    }
    Ok(())
}

/// Diff of the working tree. Untracked files are appended as their contents.
pub fn working_tree_diff(repo: &Path, paths: &[String]) -> anyhow::Result<String> {
    let mut args = vec!["diff", "--no-renames", "HEAD", "--"];
    args.extend(paths.iter().map(String::as_str));
    let mut out = git(repo, &args)?;
    let untracked = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in untracked.split('\0').filter(|p| !p.is_empty()) {
        if !paths.is_empty() && !paths.iter().any(|p| p == path) {
            continue;
        }
        out.push_str(&format!("\n# untracked {path}\n"));
        if let Ok(text) = std::fs::read_to_string(repo.join(path)) {
            let clipped: String = text.chars().take(8_000).collect();
            out.push_str(&clipped);
            if text.chars().count() > 8_000 {
                out.push_str("\n# …\n");
            }
        }
    }
    Ok(out)
}

pub fn merge_base(repo: &Path, a: &str, b: &str) -> anyhow::Result<String> {
    Ok(git(repo, &["merge-base", a, b])?.trim().to_string())
}

/// The branch a request is most likely based on: the remote default, then `main` or `master`.
pub fn default_base(repo: &Path) -> anyhow::Result<String> {
    if let Ok(link) = git(repo, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"]) {
        let name = link.trim().trim_start_matches("refs/remotes/");
        if let Ok(commit) = resolve(repo, name) {
            return Ok(commit);
        }
    }
    for name in ["origin/main", "origin/master", "main", "master", "HEAD"] {
        if let Ok(commit) = resolve(repo, name) {
            return Ok(commit);
        }
    }
    bail!("could not find a default branch")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub date: String,
    pub author: String,
    pub subject: String,
    pub files: Vec<String>,
}

/// Commits in `base..head`, newest first.
pub fn log(repo: &Path, base: &str, head: &str) -> anyhow::Result<Vec<Commit>> {
    let range = format!("{base}..{head}");
    let format = "--format=%x1e%h%x1f%cI%x1f%an%x1f%s";
    let out = git(repo, &["log", "--no-renames", "--name-only", format, &range])?;
    Ok(parse_log(&out))
}

fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\x1e')
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let mut lines = record.lines();
            let mut fields = lines.next()?.split('\x1f');
            Some(Commit {
                hash: fields.next()?.to_string(),
                date: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                subject: fields.next()?.to_string(),
                files: lines.filter(|l| !l.trim().is_empty()).map(str::to_string).collect(),
            })
        })
        .collect()
}

pub fn diff(repo: &Path, base: &str, head: &str, paths: &[String]) -> anyhow::Result<String> {
    let mut args = vec!["diff", "--no-renames", base, head, "--"];
    args.extend(paths.iter().map(String::as_str));
    git(repo, &args)
}

/// Every tracked file at `rev`.
pub fn tracked_files(repo: &Path, rev: &str) -> anyhow::Result<Vec<String>> {
    let out = git(repo, &["ls-tree", "-r", "--name-only", "-z", rev])?;
    Ok(out.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
}

/// Check `head` out into its own directory, detached, so the review never touches the
/// user's working tree.
pub fn add_worktree(repo: &Path, path: &Path, head: &str) -> anyhow::Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "add", "--detach", "--quiet", path, head]).map(|_| ())
}

pub fn remove_worktree(repo: &Path, path: &Path) -> anyhow::Result<()> {
    let path = path.to_str().context("worktree path is not valid UTF-8")?;
    git(repo, &["worktree", "remove", "--force", path]).map(|_| ())
}

#[cfg(test)]
pub(crate) mod testrepo {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway repository. Each call to `commit` writes the given files and commits them.
    pub struct TestRepo(pub PathBuf);

    impl TestRepo {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("tm-review-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q", "-b", "main"]).unwrap();
            TestRepo(dir)
        }

        pub fn commit(&self, message: &str, files: &[(&str, Option<&str>)]) -> String {
            for (path, content) in files {
                let full = self.0.join(path);
                match content {
                    Some(text) => {
                        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                        std::fs::write(full, text).unwrap();
                    }
                    None => std::fs::remove_file(full).unwrap(),
                }
            }
            git(&self.0, &["add", "-A"]).unwrap();
            let identity = ["-c", "user.name=Test", "-c", "user.email=test@example.invalid"];
            let mut args = identity.to_vec();
            args.extend(["commit", "-q", "-m", message]);
            git(&self.0, &args).unwrap();
            resolve(&self.0, "HEAD").unwrap()
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testrepo::TestRepo;
    use super::*;

    #[test]
    fn reads_changes_log_and_diff_between_two_commits() {
        let repo = TestRepo::new("git");
        let base = repo.commit("first", &[("a.ts", Some("one\n")), ("gone.ts", Some("x\n"))]);
        repo.commit("second", &[("a.ts", Some("two\n")), ("dir/b c.ts", Some("new\n"))]);
        let head = repo.commit("third", &[("gone.ts", None)]);

        let changes = changed_files(&repo.0, &base, &head).unwrap();
        let summary: Vec<(char, &str)> = changes.iter().map(|c| (c.status, c.path.as_str())).collect();
        assert_eq!(summary, vec![('M', "a.ts"), ('A', "dir/b c.ts"), ('D', "gone.ts")]);

        let commits = log(&repo.0, &base, &head).unwrap();
        assert_eq!(commits.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["third", "second"]);
        assert_eq!(commits[1].files, ["a.ts", "dir/b c.ts"]);
        assert_eq!(commits[0].author, "Test");

        let patch = diff(&repo.0, &base, &head, &["a.ts".into()]).unwrap();
        assert!(patch.contains("-one") && patch.contains("+two") && !patch.contains("gone.ts"));
        assert_eq!(tracked_files(&repo.0, &head).unwrap(), ["a.ts", "dir/b c.ts"]);
    }

    #[test]
    fn a_worktree_is_a_separate_checkout_of_the_commit() {
        let repo = TestRepo::new("worktree");
        let first = repo.commit("first", &[("a.ts", Some("one\n"))]);
        repo.commit("second", &[("a.ts", Some("two\n"))]);
        let tree = repo.0.with_extension("tree");
        add_worktree(&repo.0, &tree, &first).unwrap();
        assert_eq!(std::fs::read_to_string(tree.join("a.ts")).unwrap(), "one\n");
        assert_eq!(std::fs::read_to_string(repo.0.join("a.ts")).unwrap(), "two\n");
        remove_worktree(&repo.0, &tree).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn refuses_revisions_that_are_not_commits() {
        let repo = TestRepo::new("resolve");
        repo.commit("first", &[("a.ts", Some("one\n"))]);
        assert!(resolve(&repo.0, "--upload-pack=x").is_err());
        assert!(resolve(&repo.0, "no-such-branch").is_err());
        assert_eq!(resolve(&repo.0, "HEAD").unwrap().len(), 40);
    }

    #[test]
    fn uncommitted_work_is_copied_into_a_detached_checkout() {
        let repo = TestRepo::new("dirty");
        let head = repo.commit("first", &[("a.txt", Some("one\n"))]);
        std::fs::write(repo.0.join("a.txt"), "two\n").unwrap();
        std::fs::write(repo.0.join("new.txt"), "fresh\n").unwrap();
        let changes = working_tree_changes(&repo.0).unwrap();
        assert_eq!(
            changes,
            vec![
                Change { status: 'M', path: "a.txt".into() },
                Change { status: 'A', path: "new.txt".into() }
            ]
        );
        let tree = repo.0.with_extension("tree");
        add_worktree(&repo.0, &tree, &head).unwrap();
        apply_working_tree(&repo.0, &tree).unwrap();
        assert_eq!(std::fs::read_to_string(tree.join("a.txt")).unwrap(), "two\n");
        assert_eq!(std::fs::read_to_string(tree.join("new.txt")).unwrap(), "fresh\n");
        assert_eq!(std::fs::read_to_string(repo.0.join("a.txt")).unwrap(), "two\n");
        let diff = working_tree_diff(&repo.0, &["a.txt".into()]).unwrap();
        assert!(diff.contains("+two"));
        remove_worktree(&repo.0, &tree).unwrap();
    }
}
