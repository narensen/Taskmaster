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
    let mut parts = out.split('\0').filter(|p| !p.is_empty());
    let mut changes = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        changes.push(Change { status: status.chars().next().unwrap_or('M'), path: path.into() });
    }
    Ok(changes)
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
}
