//! Preflight / Doctor: sub-second readiness checks before a run spends planning tokens.
//!
//! Checks git health, installed provider CLIs for configured roles, Code Mode (`node`),
//! Linux sandbox (`bwrap`), headless Chrome for UI audits, project dependency folders,
//! and active Policy-as-Code guardrails.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tm_harness::HARNESSES;

use crate::policy::Policy;
use crate::settings::Settings;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: String,
    pub label: String,
    pub category: String,
    pub status: CheckStatus,
    pub detail: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub summary: String,
    pub checks: Vec<DoctorCheck>,
    pub checked_at: String,
}

fn on_path(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run fast preflight readiness checks for `project`.
pub fn run(project: &Path) -> DoctorReport {
    let mut checks = Vec::new();

    // 1. Git repository & cleanliness
    let is_git = project.join(".git").exists();
    if !is_git {
        checks.push(DoctorCheck {
            id: "git".into(),
            label: "Git repository".into(),
            category: "workspace".into(),
            status: CheckStatus::Fail,
            detail: "Project folder is not a git repository.".into(),
            fix: Some("Run `git init` and make an initial commit so Taskmaster can isolate worktrees.".into()),
        });
    } else {
        let has_head = crate::git::has_head(project);
        let dirty = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(project)
            .output()
            .ok()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        checks.push(DoctorCheck {
            id: "git".into(),
            label: "Git repository".into(),
            category: "workspace".into(),
            status: if !has_head || dirty {
                CheckStatus::Warn
            } else {
                CheckStatus::Ok
            },
            detail: if !has_head {
                "Repository has no commits yet; parallel multi-run builds require an initial commit.".into()
            } else if dirty {
                "Working tree has uncommitted changes; runs still isolate safely on their own branches.".into()
            } else {
                "Clean git repository with HEAD ready for parallel worktrees.".into()
            },
            fix: if !has_head {
                Some("Create an initial commit (`git add -A && git commit -m 'init'`).".into())
            } else {
                None
            },
        });
    }

    // 2. Configured role providers
    let saved_roles = Settings::saved_roles(project);
    let mut chosen: Vec<String> = saved_roles
        .values()
        .filter_map(|v| v.as_str().map(String::from))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    chosen.sort();
    if chosen.is_empty() {
        checks.push(DoctorCheck {
            id: "providers".into(),
            label: "AI providers".into(),
            category: "providers".into(),
            status: CheckStatus::Warn,
            detail: "No default providers chosen yet.".into(),
            fix: Some("Choose Master and Worker providers in Settings or the New Run composer.".into()),
        });
    } else {
        let mut missing = Vec::new();
        for id in &chosen {
            if HARNESSES.contains(&id.as_str()) {
                let bin = match id.as_str() {
                    "agy" => "antigravity",
                    "cursor" => "cursor-agent",
                    other => other,
                };
                if tm_harness::sandbox::resolve_binary(bin).is_err() && !on_path(bin) {
                    missing.push(format!("{id} (`{bin}`)"));
                }
            }
        }
        checks.push(DoctorCheck {
            id: "providers".into(),
            label: "AI providers".into(),
            category: "providers".into(),
            status: if missing.is_empty() { CheckStatus::Ok } else { CheckStatus::Fail },
            detail: if missing.is_empty() {
                format!("Configured providers ready: {}.", chosen.join(", "))
            } else {
                format!("Missing CLI binary for: {}.", missing.join(", "))
            },
            fix: if missing.is_empty() {
                None
            } else {
                Some("Open Providers to install or sign in to the missing CLI.".into())
            },
        });
    }

    // 3. Code Mode runtime (Node.js)
    let has_node = on_path("node");
    checks.push(DoctorCheck {
        id: "node".into(),
        label: "Code Mode runtime".into(),
        category: "runtime".into(),
        status: if has_node { CheckStatus::Ok } else { CheckStatus::Warn },
        detail: if has_node {
            "Node.js available for warm Code Mode sandboxes (`run_code`).".into()
        } else {
            "Node.js not found; workers will fall back to individual tool turns.".into()
        },
        fix: if has_node {
            None
        } else {
            Some("Install Node.js 20+ via Providers → Runtimes to unlock batched Code Mode.".into())
        },
    });

    // 4. Sandbox availability
    let sandbox_ok = tm_harness::sandbox::bwrap_usable();
    checks.push(DoctorCheck {
        id: "sandbox".into(),
        label: "Worker sandbox".into(),
        category: "security".into(),
        status: if sandbox_ok { CheckStatus::Ok } else { CheckStatus::Warn },
        detail: if sandbox_ok {
            "OS sandbox available; builders get isolated worktree shells.".into()
        } else {
            "Bubblewrap sandbox unavailable on this host; builders run without a shell.".into()
        },
        fix: if sandbox_ok {
            None
        } else {
            Some("Install `bubblewrap` (`sudo apt install bubblewrap`) for sandboxed worker shells.".into())
        },
    });

    // 5. Headless Chrome for UI audits
    let has_chrome = crate::runtime::chrome().is_some();
    let is_web_project = project.join("package.json").is_file() || project.join("web/package.json").is_file();
    if is_web_project {
        checks.push(DoctorCheck {
            id: "chrome".into(),
            label: "Browser visual audit".into(),
            category: "runtime".into(),
            status: if has_chrome { CheckStatus::Ok } else { CheckStatus::Warn },
            detail: if has_chrome {
                "Headless Chrome available for live UI checks and screenshots.".into()
            } else {
                "Chrome/Chromium not found; web runs will skip browser visual checks.".into()
            },
            fix: if has_chrome {
                None
            } else {
                Some("Install Chromium or Google Chrome to enable browser checks.".into())
            },
        });
    }

    // 6. Project dependencies
    if project.join("package.json").is_file() && !project.join("node_modules").is_dir() {
        checks.push(DoctorCheck {
            id: "node_modules".into(),
            label: "Node dependencies".into(),
            category: "workspace".into(),
            status: CheckStatus::Warn,
            detail: "`package.json` exists, but `node_modules/` is missing.".into(),
            fix: Some("Run `npm install` so TypeScript and unit test checks run immediately.".into()),
        });
    }

    // 7. Policy-as-Code status
    let policy = Policy::load(project);
    let policy_source = Policy::source(project);
    checks.push(DoctorCheck {
        id: "policy".into(),
        label: "Policy-as-Code".into(),
        category: "security".into(),
        status: CheckStatus::Ok,
        detail: match policy_source {
            Some(src) => format!(
                "Loaded from `{src}` ({} denied, {} protected globs{}).",
                policy.denied_globs.len(),
                policy.protected_globs.len(),
                policy
                    .max_run_cost_usd
                    .map(|c| format!(", ${c:.2}/run cap"))
                    .unwrap_or_default()
            ),
            None => format!(
                "Default guardrails active ({} denied secret globs).",
                policy.denied_globs.len()
            ),
        },
        fix: None,
    });

    let fails = checks.iter().filter(|c| c.status == CheckStatus::Fail).count();
    let warns = checks.iter().filter(|c| c.status == CheckStatus::Warn).count();
    let ok = fails == 0;
    let summary = if fails > 0 {
        format!("{fails} blocker{} to fix before starting a run", if fails == 1 { "" } else { "s" })
    } else if warns > 0 {
        format!("Ready with {warns} recommendation{}", if warns == 1 { "" } else { "s" })
    } else {
        "All systems ready".into()
    };

    DoctorReport {
        ok,
        summary,
        checks,
        checked_at: crate::time::rfc3339_now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_missing_git_as_blocker_and_policy_as_ok() {
        let dir = std::env::temp_dir().join(format!("tm-doctor-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let report = run(&dir);
        assert!(!report.ok);
        assert!(report.checks.iter().any(|c| c.id == "git" && c.status == CheckStatus::Fail));
        assert!(report.checks.iter().any(|c| c.id == "policy" && c.status == CheckStatus::Ok));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
