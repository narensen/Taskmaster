//! Start a run from a GitHub or GitLab issue URL (Item 4.2).
//!
//! Parses issue URLs, fetches issue details via HTTPS API (or `gh` CLI when signed in),
//! formats a structured run prompt, and generates the PR closing footer (`Closes #N`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueHost {
    GitHub,
    GitLab,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRef {
    pub host: IssueHost,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueDetails {
    pub issue: IssueRef,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Parse a GitHub (`https://github.com/owner/repo/issues/123`) or GitLab
/// (`https://gitlab.com/group/project/-/issues/123`) issue URL.
pub fn parse_issue_url(raw: &str) -> anyhow::Result<IssueRef> {
    let url = raw.trim();
    let without_scheme = url
        .strip_prefix("https://")
        .ok_or_else(|| anyhow::anyhow!("issue URL must start with https://"))?;
    let clean = without_scheme.split(['?', '#']).next().unwrap_or(without_scheme).trim_end_matches('/');
    let parts: Vec<&str> = clean.split('/').collect();
    if parts.len() >= 5 && parts[0].eq_ignore_ascii_case("github.com") && parts[3] == "issues" {
        let number: u64 = parts[4].parse().map_err(|_| anyhow::anyhow!("invalid GitHub issue number"))?;
        return Ok(IssueRef {
            host: IssueHost::GitHub,
            owner: parts[1].to_string(),
            repo: parts[2].to_string(),
            number,
            url: url.to_string(),
        });
    }
    // GitLab: gitlab.com/<group...>/<project>/-/issues/<number>
    if parts.len() >= 6
        && parts[0].to_ascii_lowercase().contains("gitlab")
        && let Some(dash_idx) = parts.iter().position(|p| *p == "-")
        && parts.get(dash_idx + 1) == Some(&"issues")
        && let Some(num_s) = parts.get(dash_idx + 2)
        && dash_idx >= 3
    {
        let number: u64 = num_s.parse().map_err(|_| anyhow::anyhow!("invalid GitLab issue number"))?;
        let owner = parts[1..dash_idx - 1].join("/");
        let repo = parts[dash_idx - 1].to_string();
        return Ok(IssueRef {
            host: IssueHost::GitLab,
            owner,
            repo,
            number,
            url: url.to_string(),
        });
    }
    anyhow::bail!("unsupported issue URL (expected a GitHub or GitLab issue link)")
}

/// Format an [`IssueDetails`] into a clean Taskmaster run prompt.
pub fn issue_to_prompt(details: &IssueDetails) -> String {
    let mut out = format!(
        "Resolve issue #{}: {}\nSource: {}\n",
        details.issue.number, details.title.trim(), details.issue.url
    );
    if !details.labels.is_empty() {
        out.push_str(&format!("Labels: {}\n", details.labels.join(", ")));
    }
    if !details.body.trim().is_empty() {
        out.push_str("\nIssue description:\n");
        out.push_str(details.body.trim());
        out.push('\n');
    }
    out.push_str("\nAdd a regression test that reproduces the issue before verifying the fix.\n");
    out
}

/// PR body line linking back to the originating issue.
pub fn closing_link(issue: &IssueRef) -> String {
    format!("Closes #{} ({})", issue.number, issue.url)
}

/// Fetch issue details from the public GitHub/GitLab API.
pub async fn fetch_issue(raw_url: &str) -> anyhow::Result<IssueDetails> {
    let issue = parse_issue_url(raw_url)?;
    let api_url = match issue.host {
        IssueHost::GitHub => format!(
            "https://api.github.com/repos/{}/{}/issues/{}",
            issue.owner, issue.repo, issue.number
        ),
        IssueHost::GitLab => {
            let encoded = format!("{}%2F{}", issue.owner.replace('/', "%2F"), issue.repo);
            format!("https://gitlab.com/api/v4/projects/{encoded}/issues/{}", issue.number)
        }
    };
    let val: serde_json::Value = tokio::task::spawn_blocking(move || -> anyhow::Result<serde_json::Value> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            .build();
        let resp = agent
            .get(&api_url)
            .set("User-Agent", "taskmaster-daemon")
            .call()
            .map_err(|err| match err {
                ureq::Error::Status(code, _) => anyhow::anyhow!("could not fetch issue (HTTP {code})"),
                ureq::Error::Transport(t) => anyhow::anyhow!("could not fetch issue: {t}"),
            })?;
        let val: serde_json::Value = resp.into_json()?;
        Ok(val)
    })
    .await??;
    let title = val["title"].as_str().unwrap_or("Untitled issue").to_string();
    let body = val["body"]
        .as_str()
        .or_else(|| val["description"].as_str())
        .unwrap_or("")
        .to_string();
    let labels = val["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.as_str().or_else(|| l["name"].as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(IssueDetails { issue, title, body, labels })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_and_gitlab_issue_urls_and_formats_prompt_and_closing_link() {
        let gh = parse_issue_url("https://github.com/acme/widget/issues/42#issuecomment-1").unwrap();
        assert_eq!(gh.host, IssueHost::GitHub);
        assert_eq!((gh.owner.as_str(), gh.repo.as_str(), gh.number), ("acme", "widget", 42));
        assert_eq!(closing_link(&gh), "Closes #42 (https://github.com/acme/widget/issues/42#issuecomment-1)");

        let gl = parse_issue_url("https://gitlab.com/org/sub/app/-/issues/19").unwrap();
        assert_eq!(gl.host, IssueHost::GitLab);
        assert_eq!((gl.owner.as_str(), gl.repo.as_str(), gl.number), ("org/sub", "app", 19));

        let prompt = issue_to_prompt(&IssueDetails {
            issue: gh,
            title: "Fix cart rounding bug".into(),
            body: "Total is off by 1 cent on tax.".into(),
            labels: vec!["bug".into()],
        });
        assert!(prompt.contains("Resolve issue #42: Fix cart rounding bug"));
        assert!(prompt.contains("Labels: bug"));
        assert!(prompt.contains("regression test"));
    }
}
