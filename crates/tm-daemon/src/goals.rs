//! Run goals (A4): keep driving follow-up Agent turns until a condition holds, up to a
//! turn cap or budget limit.
//!
//! Supported goal conditions:
//! - Preview probes: `"the preview opens"`, `"preview loads"`, or a localhost URL
//! - Explicit or backticked commands: ``"`cargo test` is green"``, `"npm test"`, `"sh: ./check.sh"`
//! - Project test suite (default): `"all tests pass"`, `"tests are green"`, etc.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::preview::Previews;
use crate::time::rfc3339_now;

pub const DEFAULT_MAX_TURNS: u32 = 5;
pub const MAX_ALLOWED_TURNS: u32 = 20;

/// Stored on `RunRequest`: the goal to keep verifying after each turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalSpec {
    pub condition: String,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
}

fn default_max_turns() -> u32 {
    DEFAULT_MAX_TURNS
}

impl GoalSpec {
    pub fn new(condition: impl Into<String>, max_turns: Option<u32>) -> Option<Self> {
        let condition = condition.into().trim().to_string();
        if condition.is_empty() {
            return None;
        }
        let max_turns = max_turns.unwrap_or(DEFAULT_MAX_TURNS).clamp(1, MAX_ALLOWED_TURNS);
        Some(GoalSpec { condition, max_turns })
    }
}

/// Persisted progress for a run's goal (`runs/<id>/goal.json`) and surfaced in the run header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalProgress {
    pub condition: String,
    pub max_turns: u32,
    pub attempts: u32,
    /// `"active"`, `"met"`, or `"stopped"`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
}

impl GoalProgress {
    pub fn initial(spec: &GoalSpec) -> Self {
        GoalProgress {
            condition: spec.condition.clone(),
            max_turns: spec.max_turns,
            attempts: 0,
            state: "active".into(),
            last_check: None,
            checked_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GoalCheck {
    pub met: bool,
    pub detail: String,
}

/// Extract a leading `/goal <condition>` directive from a prompt, returning the cleaned
/// prompt and optional `GoalSpec`.
pub fn extract_goal(prompt: &str) -> (String, Option<GoalSpec>) {
    let trimmed = prompt.trim();
    let Some(first_line) = trimmed.lines().next() else {
        return (prompt.to_string(), None);
    };
    let rest_after_cmd = first_line
        .trim_start()
        .strip_prefix("/goal ")
        .or_else(|| first_line.trim_start().strip_prefix("/goal\t"));
    let Some(raw_cond) = rest_after_cmd else {
        return (prompt.to_string(), None);
    };
    let Some(spec) = GoalSpec::new(raw_cond, None) else {
        return (prompt.to_string(), None);
    };
    let remaining: String = trimmed
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    let cleaned = if remaining.is_empty() {
        format!("Achieve this goal: {}", spec.condition)
    } else {
        remaining
    };
    (cleaned, Some(spec))
}

#[derive(Debug, PartialEq, Eq)]
enum ConditionKind {
    PreviewUrl(String),
    PreviewRun,
    Command(String),
    ProjectTests,
}

fn classify(condition: &str) -> ConditionKind {
    let trimmed = condition.trim();
    let lower = trimmed.to_ascii_lowercase();

    // 1. Explicit localhost URL probe.
    for token in trimmed.split_whitespace() {
        let cleaned = token.trim_matches(|c: char| matches!(c, '`' | '\'' | '"' | ',' | '.' | '(' | ')'));
        if cleaned.starts_with("http://127.0.0.1") || cleaned.starts_with("http://localhost") {
            return ConditionKind::PreviewUrl(cleaned.to_string());
        }
    }

    // 2. Preview opens / loads / is ready.
    if lower.contains("preview")
        && (lower.contains("open")
            || lower.contains("load")
            || lower.contains("ready")
            || lower.contains("200")
            || lower.contains("running")
            || lower.contains("up"))
    {
        return ConditionKind::PreviewRun;
    }

    // 3. Explicit backticked command (e.g. "`cargo test` is green").
    if let Some(start) = trimmed.find('`')
        && let Some(end) = trimmed[start + 1..].find('`')
    {
        let cmd = trimmed[start + 1..start + 1 + end].trim();
        if !cmd.is_empty() {
            return ConditionKind::Command(cmd.to_string());
        }
    }

    // 4. Prefixed command (`cmd: ...`, `sh: ...`, `run: ...`).
    for prefix in ["cmd:", "sh:", "run:"] {
        if let Some(rest) = lower.strip_prefix(prefix)
            && !rest.trim().is_empty()
        {
            let orig = trimmed[prefix.len()..].trim();
            return ConditionKind::Command(orig.to_string());
        }
    }

    // 5. Raw shell command starting with a known tool.
    for tool in [
        "cargo ",
        "npm ",
        "pnpm ",
        "yarn ",
        "pytest",
        "python ",
        "python3 ",
        "node ",
        "go test",
        "./",
        "sh ",
        "bash ",
    ] {
        if lower.starts_with(tool) {
            return ConditionKind::Command(trimmed.to_string());
        }
    }

    // 6. Default: run project test suite.
    ConditionKind::ProjectTests
}

/// Evaluate `goal` against `project` (or a detached worktree of `branch` when `branch` is
/// unmerged). Always cleans up any temporary worktree it creates.
pub async fn check_goal(
    project: &Path,
    branch: Option<&str>,
    merged: bool,
    run_id: &str,
    goal: &GoalSpec,
    previews: &Previews,
) -> GoalCheck {
    match classify(&goal.condition) {
        ConditionKind::PreviewUrl(url) => probe_http_url(&url).await,
        ConditionKind::PreviewRun => check_preview(previews, run_id, branch.filter(|_| !merged)).await,
        ConditionKind::Command(cmd) => {
            let (dir, guard) = checkout_for_check(project, branch, merged, run_id);
            let res = run_shell_command(&dir, &cmd, Duration::from_secs(120)).await;
            drop(guard);
            res
        }
        ConditionKind::ProjectTests => {
            let (dir, guard) = checkout_for_check(project, branch, merged, run_id);
            let res = run_project_tests(project, &dir).await;
            drop(guard);
            res
        }
    }
}

struct TempWorktree {
    project: PathBuf,
    path: PathBuf,
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .current_dir(&self.project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn checkout_for_check(project: &Path, branch: Option<&str>, merged: bool, run_id: &str) -> (PathBuf, Option<TempWorktree>) {
    if merged {
        return (project.to_path_buf(), None);
    }
    let Some(branch) = branch.filter(|b| !b.is_empty()) else {
        return (project.to_path_buf(), None);
    };
    let nonce = crate::secret::random_hex(4).unwrap_or_else(|_| "chk".into());
    let path = std::env::temp_dir().join(format!("tm-goal-{run_id}-{nonce}"));
    let ok = std::process::Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg(branch)
        .current_dir(project)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        (path.clone(), Some(TempWorktree { project: project.to_path_buf(), path }))
    } else {
        (project.to_path_buf(), None)
    }
}

async fn check_preview(previews: &Previews, run_id: &str, branch: Option<&str>) -> GoalCheck {
    let mut status = match previews.start(run_id, branch).await {
        Ok(status) => status,
        Err(err) => return GoalCheck { met: false, detail: format!("the preview did not start: {err:#}") },
    };
    // A dev server that installs its packages first may take minutes; past that, the
    // preview's own watcher has already failed it or fallen back to static files.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while status.state == "starting" && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match previews.status(run_id) {
            Some(current) => status = current,
            None => break,
        }
    }
    match status.state {
        "running" => probe_http_url(&status.url).await,
        state => GoalCheck {
            met: false,
            detail: status.reason.unwrap_or_else(|| format!("the preview is {state}")),
        },
    }
}

async fn probe_http_url(url: &str) -> GoalCheck {
    let stripped = url
        .strip_prefix("http://")
        .unwrap_or(url);
    let (host_port, path) = match stripped.split_once('/') {
        Some((hp, rest)) => (hp, format!("/{rest}")),
        None => (stripped, "/".to_string()),
    };
    let addr = if host_port.contains(':') {
        host_port.to_string()
    } else {
        format!("{host_port}:80")
    };
    let connect = tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(&addr)).await;
    let Ok(Ok(mut stream)) = connect else {
        return GoalCheck {
            met: false,
            detail: format!("could not connect to {url}"),
        };
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return GoalCheck { met: false, detail: format!("failed writing HTTP request to {url}") };
    }
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    let status: Option<u16> = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok());
    match status {
        Some(code) if (200..400).contains(&code) => GoalCheck {
            met: true,
            detail: format!("preview responded HTTP {code} at {url}"),
        },
        Some(code) => GoalCheck {
            met: false,
            detail: format!("preview responded HTTP {code} at {url}"),
        },
        None => GoalCheck {
            met: false,
            detail: format!("no HTTP response from {url}"),
        },
    }
}

async fn run_shell_command(dir: &Path, command: &str, timeout: Duration) -> GoalCheck {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::runtime::clean_env(&mut cmd);
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            return GoalCheck {
                met: false,
                detail: format!("failed to run `{command}`: {err}"),
            };
        }
        Err(_) => {
            return GoalCheck {
                met: false,
                detail: format!("`{command}` timed out after {}s", timeout.as_secs()),
            };
        }
    };
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let tail: String = combined
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .chars()
        .take(800)
        .collect();
    if output.status.success() {
        GoalCheck {
            met: true,
            detail: if tail.is_empty() { format!("`{command}` passed") } else { tail },
        }
    } else {
        GoalCheck {
            met: false,
            detail: if tail.is_empty() {
                format!("`{command}` exited with {}", output.status)
            } else {
                format!("`{command}` failed:\n{tail}")
            },
        }
    }
}

async fn run_project_tests(project: &Path, dir: &Path) -> GoalCheck {
    // 1. The `test` command from taskmaster.yaml (O1), once the person trusted that file.
    //    The trust record lives in the project, not in a throwaway checkout.
    if let Ok(Some(loaded)) = crate::project_config::load(dir)
        && (loaded.trusted || crate::project_config::is_trusted(project, &loaded.hash))
        && let Some(test_cmd) = loaded.config.test.as_deref().map(str::trim).filter(|cmd| !cmd.is_empty())
    {
        return run_shell_command(dir, test_cmd, Duration::from_secs(120)).await;
    }

    // 2. Taskmaster's standard runtime verification steps (package.json test, pytest, cargo, etc.).
    let venv = dir.join(".venv");
    let steps = crate::runtime::steps(dir, &venv);
    if !steps.is_empty() {
        let (timings, failure) = crate::runtime::run_steps(dir, &steps, Duration::from_secs(120)).await;
        return match failure {
            None => GoalCheck {
                met: true,
                detail: format!("all {} verification step(s) passed", timings.len()),
            },
            Some(fail) => {
                let tail: String = fail.output.trim().chars().take(800).collect();
                GoalCheck {
                    met: false,
                    detail: format!("{} failed:\n{tail}", fail.step),
                }
            }
        };
    }

    // 3. Fallback: conventional ./test.sh if present.
    if dir.join("test.sh").is_file() {
        return run_shell_command(dir, "sh ./test.sh", Duration::from_secs(120)).await;
    }

    GoalCheck {
        met: true,
        detail: "no project test runner detected; run finished".into(),
    }
}

/// Build the Agent-turn follow-up prompt when a goal check failed.
pub fn follow_up_prompt(goal: &GoalSpec, attempt: u32, check: &GoalCheck) -> String {
    format!(
        "Goal not yet met (attempt {attempt}/{}): {}\n\nVerification failure:\n{}\n\nFix the failure so the goal passes.",
        goal.max_turns, goal.condition, check.detail
    )
}

/// Build a `GoalProgress` snapshot after a check.
pub fn next_progress(prev: &GoalProgress, check: &GoalCheck) -> GoalProgress {
    let attempts = prev.attempts + 1;
    let state = if check.met {
        "met".to_string()
    } else if attempts >= prev.max_turns {
        "stopped".to_string()
    } else {
        "active".to_string()
    };
    GoalProgress {
        condition: prev.condition.clone(),
        max_turns: prev.max_turns,
        attempts,
        state,
        last_check: Some(check.detail.clone()),
        checked_at: Some(rfc3339_now()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_slash_goal_directive() {
        let (clean, goal) = extract_goal("/goal all tests pass\nFix the login bug");
        assert_eq!(clean, "Fix the login bug");
        assert_eq!(goal.unwrap().condition, "all tests pass");

        let (clean2, goal2) = extract_goal("/goal `cargo test` is green");
        assert_eq!(clean2, "Achieve this goal: `cargo test` is green");
        assert_eq!(goal2.unwrap().condition, "`cargo test` is green");

        let (clean3, goal3) = extract_goal("Just build a landing page");
        assert_eq!(clean3, "Just build a landing page");
        assert!(goal3.is_none());
    }

    #[test]
    fn classifies_goal_conditions() {
        assert_eq!(classify("all tests pass"), ConditionKind::ProjectTests);
        assert_eq!(classify("the preview opens"), ConditionKind::PreviewRun);
        assert_eq!(classify("`cargo test` is green"), ConditionKind::Command("cargo test".into()));
        assert_eq!(classify("http://127.0.0.1:4100/health"), ConditionKind::PreviewUrl("http://127.0.0.1:4100/health".into()));
    }
}
