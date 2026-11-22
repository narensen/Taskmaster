//! PR CI loop and review-comment sync (Item 4.1).
//!
//! Reads CI check status and PR review comments for a run's pull request, attributes
//! failures and inline review comments to the owning plan page by its glob, and turns
//! them into follow-up notes addressed to `page:<id>` (or `master` for shared files).

use std::path::Path;

use serde::{Deserialize, Serialize};
use tm_core::Plan;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCheck {
    pub name: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrReviewComment {
    pub id: String,
    pub author: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedNote {
    pub target: String,
    pub source: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrSyncReport {
    pub checks: Vec<CiCheck>,
    pub failing_checks: usize,
    pub comments_synced: usize,
    pub queued_notes: Vec<AttributedNote>,
}

/// Find which page of `plan` owns `rel_path`, returning `page:<id>` or `"master"`.
pub fn owner_target(plan: &Plan, rel_path: &str) -> String {
    let clean = rel_path.trim().trim_start_matches("./");
    for page in &plan.pages {
        if let Ok(glob) = globset::Glob::new(&page.owned_glob)
            && glob.compile_matcher().is_match(clean)
        {
            return format!("page:{}", page.id);
        }
    }
    "master".to_string()
}

/// Extract file paths referenced in a CI failure log and attribute each failing check
/// plus each PR review comment to the owning page.
pub fn attribute_pr_feedback(
    plan: &Plan,
    checks: &[CiCheck],
    comments: &[PrReviewComment],
) -> Vec<AttributedNote> {
    let mut notes = Vec::new();
    for check in checks {
        let lower = check.state.to_ascii_lowercase();
        if !matches!(lower.as_str(), "fail" | "failed" | "failure" | "error") {
            continue;
        }
        let excerpt = check.log_excerpt.as_deref().unwrap_or("").trim();
        let target = find_path_in_log(excerpt)
            .map(|p| owner_target(plan, &p))
            .unwrap_or_else(|| "master".to_string());
        let text = if excerpt.is_empty() {
            format!("CI check `{}` failed. Inspect and fix the failure.", check.name)
        } else {
            format!("CI check `{}` failed:\n{}", check.name, excerpt.chars().take(1200).collect::<String>())
        };
        notes.push(AttributedNote {
            target,
            source: format!("ci:{}", check.name),
            text,
        });
    }
    for comment in comments {
        let target = owner_target(plan, &comment.path);
        let loc = match comment.line {
            Some(l) => format!("{}:{l}", comment.path),
            None => comment.path.clone(),
        };
        notes.push(AttributedNote {
            target,
            source: format!("pr_comment:{}", comment.id),
            text: format!("PR review comment from @{} on `{loc}`:\n{}", comment.author, comment.body.trim()),
        });
    }
    notes
}

fn find_path_in_log(log: &str) -> Option<String> {
    for token in log.split_whitespace() {
        let clean = token
            .trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '(' | ')' | ',' | ';'));
        let path_part = clean.split(':').next().unwrap_or(clean);
        let path_part = path_part.split('(').next().unwrap_or(path_part);
        if path_part.contains('/')
            && (path_part.ends_with(".ts")
                || path_part.ends_with(".tsx")
                || path_part.ends_with(".js")
                || path_part.ends_with(".rs")
                || path_part.ends_with(".py")
                || path_part.ends_with(".go"))
        {
            return Some(path_part.trim_start_matches("./").to_string());
        }
    }
    None
}

/// Query `gh pr checks --json name,state,link` for the current branch if `gh` is installed.
pub fn fetch_gh_checks(project: &Path, pr_ref: Option<&str>) -> Vec<CiCheck> {
    let mut args = vec!["pr", "checks"];
    if let Some(r) = pr_ref.filter(|s| !s.is_empty()) {
        args.push(r);
    }
    args.extend(["--json", "name,state,link"]);
    let Ok(out) = std::process::Command::new("gh")
        .args(&args)
        .current_dir(project)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return Vec::new();
    };
    items
        .into_iter()
        .map(|v| CiCheck {
            name: v["name"].as_str().unwrap_or("check").to_string(),
            state: v["state"].as_str().unwrap_or("unknown").to_string(),
            url: v["link"].as_str().map(str::to_string),
            log_excerpt: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::PagePlan;

    #[test]
    fn attributes_ci_failures_and_pr_comments_to_owning_pages() {
        let plan = Plan {
            pages: vec![
                PagePlan { id: "cart".into(), owned_glob: "src/cart/**".into(), ..Default::default() },
                PagePlan { id: "auth".into(), owned_glob: "src/auth/**".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let checks = vec![
            CiCheck {
                name: "typecheck".into(),
                state: "FAILURE".into(),
                url: None,
                log_excerpt: Some("src/cart/total.ts(14,3): error TS2322: Type mismatch".into()),
            },
            CiCheck {
                name: "lint".into(),
                state: "SUCCESS".into(),
                url: None,
                log_excerpt: None,
            },
        ];
        let comments = vec![PrReviewComment {
            id: "c1".into(),
            author: "reviewer".into(),
            path: "src/auth/login.ts".into(),
            line: Some(42),
            body: "Check token expiry before redirecting.".into(),
        }];

        let notes = attribute_pr_feedback(&plan, &checks, &comments);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].target, "page:cart");
        assert!(notes[0].text.contains("TS2322"));
        assert_eq!(notes[1].target, "page:auth");
        assert!(notes[1].text.contains("src/auth/login.ts:42"));
    }
}
