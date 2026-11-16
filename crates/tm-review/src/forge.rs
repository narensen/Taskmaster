//! A review target on a local checkout or on a Git host.
//!
//! GitHub, GitLab, Bitbucket, Gitea, Forgejo, Codeberg, Azure DevOps and SourceHut are
//! recognised from the remote URL. A pull or merge request is fetched by the ref that host
//! publishes (`refs/pull/N/head`, `refs/merge-requests/N/head`, and the others). The base is
//! taken from `gh` or `glab` when that tool is logged in, and otherwise from the merge-base
//! with the default branch.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use serde_json::Value;

use crate::git;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forge {
    GitHub,
    GitLab,
    Bitbucket,
    /// Gitea, Forgejo and Codeberg publish the same pull refs as GitHub.
    Gitea,
    AzureDevOps,
    SourceHut,
}

impl Forge {
    pub fn name(self) -> &'static str {
        match self {
            Forge::GitHub => "GitHub",
            Forge::GitLab => "GitLab",
            Forge::Bitbucket => "Bitbucket",
            Forge::Gitea => "Gitea",
            Forge::AzureDevOps => "Azure DevOps",
            Forge::SourceHut => "SourceHut",
        }
    }

    /// The remote ref that points at the request, when the host has one.
    pub fn head_ref(self, number: u64) -> Option<String> {
        match self {
            Forge::GitHub | Forge::Gitea => Some(format!("refs/pull/{number}/head")),
            Forge::GitLab => Some(format!("refs/merge-requests/{number}/head")),
            Forge::Bitbucket => Some(format!("refs/pull-requests/{number}/from")),
            Forge::AzureDevOps => Some(format!("refs/pull/{number}/merge")),
            Forge::SourceHut => None,
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Forge::GitHub => "github",
            Forge::GitLab => "gitlab",
            Forge::Bitbucket => "bitbucket",
            Forge::Gitea => "gitea",
            Forge::AzureDevOps => "azure",
            Forge::SourceHut => "sourcehut",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub forge: Forge,
    pub host: String,
    /// Owner, or a GitLab group path such as `group/subgroup`.
    pub owner: String,
    pub repo: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub remote: Remote,
    pub number: u64,
}

/// Where a review reads from, after any fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub repo: PathBuf,
    pub base: String,
    pub head: String,
    pub dirty: bool,
    pub describe: String,
}

pub fn is_remote(spec: &str) -> bool {
    let spec = spec.trim();
    spec.contains("://") || spec.starts_with("git@")
}

/// Open a local path, a repository URL, or a request URL.
pub fn open(
    spec: &str,
    pr: Option<u64>,
    base: Option<&str>,
    head: Option<&str>,
    dirty: bool,
) -> anyhow::Result<Opened> {
    if spec.starts_with('-') {
        bail!("invalid repository `{spec}`");
    }
    if dirty && (pr.is_some() || base.is_some() || head.is_some() || is_remote(spec)) {
        bail!("`--dirty` reviews the local working tree, so it cannot be combined with a remote, --pr, --base or --head");
    }
    if is_remote(spec) {
        return open_remote(spec, pr, base, head);
    }
    let repo = PathBuf::from(spec);
    if !repo.exists() {
        bail!("no such directory: {}", repo.display());
    }
    if let Some(number) = pr {
        let url = git::git(&repo, &["remote", "get-url", "origin"])
            .context("this checkout has no origin remote to take the request from")?;
        let remote = parse_remote(&url).with_context(|| format!("origin `{url}` is not a recognised Git host"))?;
        return open_request(&repo, &Request { remote, number }, base, head);
    }
    if dirty {
        let head = git::resolve(&repo, "HEAD")?;
        return Ok(Opened {
            repo,
            base: head.clone(),
            head,
            dirty: true,
            describe: "uncommitted changes".into(),
        });
    }
    Ok(Opened {
        repo: repo.clone(),
        base: git::resolve(&repo, base.unwrap_or("HEAD~1"))?,
        head: git::resolve(&repo, head.unwrap_or("HEAD"))?,
        dirty: false,
        describe: "local commits".into(),
    })
}

fn open_remote(spec: &str, pr: Option<u64>, base: Option<&str>, head: Option<&str>) -> anyhow::Result<Opened> {
    let request = parse_request(spec);
    let remote = request.as_ref().map(|r| r.remote.clone()).or_else(|| parse_remote(spec));
    let Some(remote) = remote else { bail!("`{spec}` is not a recognised Git host URL") };
    if remote.forge == Forge::SourceHut && (pr.is_some() || request.is_some()) {
        bail!("SourceHut has no pull-request refs; review a clone with --base and --head");
    }
    let repo = ensure_cloned(&remote, spec)?;
    let number = pr.or_else(|| request.as_ref().map(|r| r.number));
    if let Some(number) = number {
        return open_request(&repo, &Request { remote, number }, base, head);
    }
    let _ = git::git(&repo, &["fetch", "--quiet", "origin"]);
    Ok(Opened {
        repo: repo.clone(),
        base: git::resolve(&repo, base.unwrap_or("HEAD~1"))?,
        head: git::resolve(&repo, head.unwrap_or("HEAD"))?,
        dirty: false,
        describe: format!("{} {}/{}", remote.forge.name(), remote.owner, remote.repo),
    })
}

fn open_request(repo: &Path, request: &Request, base: Option<&str>, head: Option<&str>) -> anyhow::Result<Opened> {
    let fetched = fetch_request(repo, "origin", request)?;
    let head = match head {
        Some(rev) => git::resolve(repo, rev)?,
        None => fetched,
    };
    let base = if let Some(rev) = base {
        git::resolve(repo, rev)?
    } else if let Some((cli_base, _)) = forge_cli_base(request) {
        if git::resolve(repo, &cli_base).is_err() {
            let _ = git::git(repo, &["fetch", "--quiet", "origin", &cli_base]);
        }
        git::resolve(repo, &cli_base).unwrap_or(cli_base)
    } else {
        let branch = git::default_base(repo)?;
        git::merge_base(repo, &head, &branch)?
    };
    let describe = format!(
        "{} {}/{}#{} ({})",
        request.remote.forge.name(),
        request.remote.owner,
        request.remote.repo,
        request.number,
        request.remote.forge.head_ref(request.number).unwrap_or_default()
    );
    Ok(Opened { repo: repo.to_path_buf(), base, head, dirty: false, describe })
}

/// Fetch the request head into `refs/taskmaster/...` and return its commit.
pub fn fetch_request(repo: &Path, remote: &str, request: &Request) -> anyhow::Result<String> {
    let Some(spec) = request.remote.forge.head_ref(request.number) else {
        bail!("{} does not publish request refs", request.remote.forge.name());
    };
    let local = format!("refs/taskmaster/{}/{}", request.remote.forge.slug(), request.number);
    git::git(repo, &["fetch", "--quiet", remote, &format!("{spec}:{local}")]).with_context(|| {
        format!(
            "could not fetch {} request {} from {remote}. Pass --base and --head if this host does not publish that ref",
            request.remote.forge.name(),
            request.number
        )
    })?;
    git::resolve(repo, &local)
}

fn forge_cli_base(request: &Request) -> Option<(String, String)> {
    match request.remote.forge {
        Forge::GitHub => github_range(request),
        Forge::GitLab => gitlab_range(request),
        _ => None,
    }
}

fn github_range(request: &Request) -> Option<(String, String)> {
    let slug = format!("{}/{}", request.remote.owner, request.remote.repo);
    let out = Command::new("gh")
        .args(["pr", "view", &request.number.to_string(), "--repo", &slug, "--json", "baseRefOid,headRefOid"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some((value["baseRefOid"].as_str()?.to_string(), value["headRefOid"].as_str()?.to_string()))
}

fn gitlab_range(request: &Request) -> Option<(String, String)> {
    let slug = format!("{}/{}", request.remote.owner, request.remote.repo);
    let out = Command::new("glab")
        .args(["mr", "view", &request.number.to_string(), "-R", &slug, "--output", "json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&out.stdout).ok()?;
    let refs = &value["diff_refs"];
    Some((refs["base_sha"].as_str()?.to_string(), refs["head_sha"].as_str()?.to_string()))
}

fn ensure_cloned(remote: &Remote, spec: &str) -> anyhow::Result<PathBuf> {
    let dir = cache_dir(remote)?;
    if dir.join(".git").is_dir() {
        return Ok(dir);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let url = clone_url(spec, remote);
    let status = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(&dir)
        .status()
        .context("failed to start git clone")?;
    if !status.success() {
        bail!("could not clone {url}");
    }
    Ok(dir)
}

fn cache_dir(remote: &Remote) -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let owner = remote.owner.replace('/', "__");
    Ok(PathBuf::from(home).join(".taskmaster/forges").join(&remote.host).join(owner).join(&remote.repo))
}

fn clone_url(spec: &str, remote: &Remote) -> String {
    if let Some(request) = parse_request(spec) {
        let _ = request;
    }
    if (spec.starts_with("git@") || spec.contains("://"))
        && let Some(repo_url) = repository_url(spec) {
            return repo_url;
        }
    format!("https://{}/{}/{}.git", remote.host, remote.owner, remote.repo)
}

/// The repository URL with any request path removed.
fn repository_url(spec: &str) -> Option<String> {
    let remote = parse_remote(spec).or_else(|| parse_request(spec).map(|r| r.remote))?;
    if remote.forge == Forge::AzureDevOps {
        let host = if remote.host == "ssh.dev.azure.com" { "dev.azure.com" } else { remote.host.as_str() };
        let path = if host.ends_with(".visualstudio.com") {
            remote.owner.split('/').nth(1).unwrap_or(remote.owner.as_str()).to_string()
        } else {
            remote.owner.clone()
        };
        return Some(format!("https://{host}/{path}/_git/{}.git", remote.repo));
    }
    if spec.starts_with("git@") && !spec.contains("://") {
        return Some(format!("git@{}:{}/{}.git", remote.host, remote.owner, remote.repo));
    }
    Some(format!("https://{}/{}/{}.git", remote.host, remote.owner, remote.repo))
}

pub fn parse_remote(input: &str) -> Option<Remote> {
    parse_request(input).map(|r| r.remote).or_else(|| split_host_path(input).and_then(|(host, path)| remote_from(host, path)))
}

pub fn parse_request(input: &str) -> Option<Request> {
    let (host, path) = split_host_path(input)?;
    let (forge, owner, repo, number) = parts(host, path)?;
    let number = number?;
    Some(Request { remote: Remote { forge, host: host.to_string(), owner, repo }, number })
}

fn split_host_path(input: &str) -> Option<(&str, &str)> {
    let url = input.trim().trim_end_matches('/');
    if let Some(rest) = url.strip_prefix("git@")
        && !rest.contains("://") {
            return rest.split_once(':').map(|(host, path)| (host.trim_end_matches(':'), path.trim_start_matches('/')));
        }
    let url = url.strip_prefix("ssh://").or_else(|| url.strip_prefix("git://")).unwrap_or(url);
    let url = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let url = match url.split_once('@') {
        Some((user, rest)) if !user.contains('/') => rest,
        _ => url,
    };
    let (host, path) = url.split_once('/')?;
    if host.is_empty() || path.is_empty() || host.starts_with('.') {
        return None;
    }
    Some((host, path.trim_start_matches('/')))
}

fn remote_from(host: &str, path: &str) -> Option<Remote> {
    let (forge, owner, repo, _) = parts(host, path)?;
    Some(Remote { forge, host: host.to_string(), owner, repo })
}

fn parts(host: &str, path: &str) -> Option<(Forge, String, String, Option<u64>)> {
    let path = path.trim_matches('/');
    if host == "dev.azure.com" || host == "ssh.dev.azure.com" || host.ends_with(".visualstudio.com") {
        return azure(host, path);
    }
    if host == "git.sr.ht" {
        let (owner, repo) = sourcehut(path)?;
        return Some((Forge::SourceHut, owner, repo, None));
    }
    let forge = forge_of(host, path);
    match forge {
        Forge::GitLab => gitlab(path).map(|(o, r, n)| (Forge::GitLab, o, r, n)),
        Forge::Bitbucket => marked(path, &["pull-requests"]).map(|(o, r, n)| (Forge::Bitbucket, o, r, n)),
        Forge::Gitea => marked(path, &["pulls", "pull"]).map(|(o, r, n)| (Forge::Gitea, o, r, n)),
        Forge::GitHub => marked(path, &["pull", "pulls"]).map(|(o, r, n)| (Forge::GitHub, o, r, n)),
        Forge::AzureDevOps | Forge::SourceHut => None,
    }
}

fn forge_of(host: &str, path: &str) -> Forge {
    if host == "github.com" || host.starts_with("github.") || host.contains(".github.") {
        Forge::GitHub
    } else if host == "gitlab.com" || host.contains("gitlab") {
        Forge::GitLab
    } else if host == "bitbucket.org" || host.contains("bitbucket") {
        Forge::Bitbucket
    } else if host == "codeberg.org" || host == "gitea.com" || host.contains("gitea") || host.contains("forgejo") {
        Forge::Gitea
    } else if path.contains("/-/merge_requests/") {
        Forge::GitLab
    } else if path.contains("/pull-requests/") {
        Forge::Bitbucket
    } else if path.contains("/pulls/") {
        Forge::Gitea
    } else {
        Forge::GitHub
    }
}

fn gitlab(path: &str) -> Option<(String, String, Option<u64>)> {
    let (repo_path, rest) = path.split_once("/-/").unwrap_or((path, ""));
    let repo_path = strip_git(repo_path);
    let (owner, repo) = repo_path.rsplit_once('/')?;
    let number = rest.strip_prefix("merge_requests/").and_then(|n| n.split(['/', '?']).next()).and_then(|n| n.parse().ok());
    Some((owner.to_string(), repo.to_string(), number))
}

fn marked(path: &str, markers: &[&str]) -> Option<(String, String, Option<u64>)> {
    let path = strip_git(path);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    let owner = segs[0].to_string();
    let repo = strip_git(segs[1]).to_string();
    if repo.is_empty() {
        return None;
    }
    let number = if segs.len() >= 4 && markers.contains(&segs[2]) { segs[3].parse().ok() } else { None };
    Some((owner, repo, number))
}

fn azure(host: &str, path: &str) -> Option<(Forge, String, String, Option<u64>)> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if host == "ssh.dev.azure.com" {
        // v3/org/project/repo
        if segs.len() >= 4 && segs[0] == "v3" {
            return Some((
                Forge::AzureDevOps,
                format!("{}/{}", segs[1], segs[2]),
                strip_git(segs[3]).to_string(),
                None,
            ));
        }
        return None;
    }
    if host.ends_with(".visualstudio.com") {
        let org = host.split('.').next()?.to_string();
        let git_at = segs.iter().position(|s| *s == "_git")?;
        let project = segs.first()?.to_string();
        let repo = strip_git(segs.get(git_at + 1)?).to_string();
        let number = pullrequest_number(&segs, git_at + 2);
        return Some((Forge::AzureDevOps, format!("{org}/{project}"), repo, number));
    }
    // dev.azure.com/org/project/_git/repo/pullrequest/12
    let git_at = segs.iter().position(|s| *s == "_git")?;
    if git_at < 2 {
        return None;
    }
    let owner = format!("{}/{}", segs[0], segs[1]);
    let repo = strip_git(segs.get(git_at + 1)?).to_string();
    let number = pullrequest_number(&segs, git_at + 2);
    Some((Forge::AzureDevOps, owner, repo, number))
}

fn pullrequest_number(segs: &[&str], at: usize) -> Option<u64> {
    if segs.get(at) == Some(&"pullrequest") { segs.get(at + 1).and_then(|n| n.parse().ok()) } else { None }
}

fn sourcehut(path: &str) -> Option<(String, String)> {
    let segs: Vec<&str> = strip_git(path).split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    Some((segs[0].to_string(), segs[1].to_string()))
}

fn strip_git(path: &str) -> &str {
    path.strip_suffix(".git").unwrap_or(path).trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testrepo::TestRepo;

    #[test]
    fn recognises_each_host_and_its_request_url() {
        let cases = [
            ("git@github.com:acme/widgets.git", Forge::GitHub, "acme", "widgets", None),
            ("https://github.com/acme/widgets", Forge::GitHub, "acme", "widgets", None),
            ("https://github.com/acme/widgets/pull/12/files", Forge::GitHub, "acme", "widgets", Some(12)),
            ("ssh://git@github.com/acme/widgets.git", Forge::GitHub, "acme", "widgets", None),
            ("https://github.example.com/acme/widgets/pull/2", Forge::GitHub, "acme", "widgets", Some(2)),
            ("git@gitlab.com:acme/sub/widgets.git", Forge::GitLab, "acme/sub", "widgets", None),
            ("https://gitlab.com/acme/sub/widgets/-/merge_requests/4", Forge::GitLab, "acme/sub", "widgets", Some(4)),
            ("https://gitlab.example.com/acme/widgets/-/merge_requests/9", Forge::GitLab, "acme", "widgets", Some(9)),
            ("https://bitbucket.org/acme/widgets/pull-requests/3", Forge::Bitbucket, "acme", "widgets", Some(3)),
            ("git@bitbucket.org:acme/widgets.git", Forge::Bitbucket, "acme", "widgets", None),
            ("https://codeberg.org/acme/widgets/pulls/8", Forge::Gitea, "acme", "widgets", Some(8)),
            ("https://gitea.example.com/acme/widgets/pulls/1", Forge::Gitea, "acme", "widgets", Some(1)),
            ("https://dev.azure.com/org/project/_git/widgets/pullrequest/15", Forge::AzureDevOps, "org/project", "widgets", Some(15)),
            ("git@ssh.dev.azure.com:v3/org/project/widgets", Forge::AzureDevOps, "org/project", "widgets", None),
            ("https://org.visualstudio.com/project/_git/widgets", Forge::AzureDevOps, "org/project", "widgets", None),
            ("https://git.sr.ht/~acme/widgets", Forge::SourceHut, "~acme", "widgets", None),
        ];
        for (url, forge, owner, repo, number) in cases {
            let remote = parse_remote(url).unwrap_or_else(|| panic!("remote {url}"));
            assert_eq!((remote.forge, remote.owner.as_str(), remote.repo.as_str()), (forge, owner, repo), "{url}");
            assert_eq!(parse_request(url).map(|r| r.number), number, "{url}");
        }
        assert_eq!(Forge::GitHub.head_ref(12).as_deref(), Some("refs/pull/12/head"));
        assert_eq!(Forge::GitLab.head_ref(4).as_deref(), Some("refs/merge-requests/4/head"));
        assert_eq!(Forge::Bitbucket.head_ref(3).as_deref(), Some("refs/pull-requests/3/from"));
        assert_eq!(Forge::Gitea.head_ref(8).as_deref(), Some("refs/pull/8/head"));
        assert_eq!(Forge::AzureDevOps.head_ref(15).as_deref(), Some("refs/pull/15/merge"));
        assert_eq!(Forge::SourceHut.head_ref(1), None);
    }

    #[test]
    fn fetches_a_request_ref_and_finds_its_merge_base() {
        let upstream = TestRepo::new("forge-up");
        let base = upstream.commit("base", &[("a.txt", Some("a\n"))]);
        let head = upstream.commit("change", &[("a.txt", Some("b\n"))]);
        git::git(&upstream.0, &["update-ref", "refs/pull/4/head", &head]).unwrap();
        git::git(&upstream.0, &["update-ref", "refs/heads/main", &base]).unwrap();

        let dest = upstream.0.with_extension("clone");
        let _ = std::fs::remove_dir_all(&dest);
        let status = Command::new("git").args(["clone", "--quiet"]).arg(&upstream.0).arg(&dest).status().unwrap();
        assert!(status.success());
        let request = parse_request("https://github.com/acme/widgets/pull/4").unwrap();
        let fetched = fetch_request(&dest, "origin", &request).unwrap();
        assert_eq!(fetched, head);
        assert_eq!(git::merge_base(&dest, &fetched, "origin/main").unwrap(), base);
        std::fs::remove_dir_all(&dest).unwrap();
    }

    #[test]
    fn a_local_checkout_opens_as_commits_or_uncommitted_work() {
        let repo = TestRepo::new("forge-local");
        repo.commit("first", &[("a.txt", Some("1\n"))]);
        let second = repo.commit("second", &[("a.txt", Some("2\n"))]);
        let opened = open(&repo.0.to_string_lossy(), None, Some("HEAD~1"), Some("HEAD"), false).unwrap();
        assert_eq!(opened.head, second);
        assert_eq!(opened.base, git::resolve(&repo.0, "HEAD~1").unwrap());
        assert!(!opened.dirty);
        std::fs::write(repo.0.join("a.txt"), "3\n").unwrap();
        let opened = open(&repo.0.to_string_lossy(), None, None, None, true).unwrap();
        assert!(opened.dirty);
        assert_eq!(opened.head, second);
        assert!(open(&repo.0.to_string_lossy(), Some(1), None, None, true).is_err());
    }
}
