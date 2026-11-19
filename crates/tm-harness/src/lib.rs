//! Harness adapters: run a coding agent headless inside one directory and report what it did.
//!
//! Two shapes of harness exist. Claude Code is a one-way event stream whose permissions are
//! fixed by launch flags. Antigravity speaks the Agent Client Protocol, where the agent asks
//! before shell commands and the daemon answers through a [`PermissionGate`]; its file access
//! is confined by an OS sandbox, because it does not ask before reading or writing files.

pub mod acp;
pub mod claude;
pub mod codex;
pub mod gate;
pub mod providers;
pub mod pty;
pub mod sandbox;
pub mod screen;
pub mod setup;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

pub use gate::{Decision, PermissionGate, PermissionRequest, ToolKind, WorktreeGate};
pub use providers::{ModelChoice, ProviderReport, provider_report, provider_reports, require_model};

/// Kills a spawned agent's whole process group when dropped, so a cancelled, timed-out or
/// finished run leaves no grandchildren (language servers, shells, browsers) behind.
pub struct GroupGuard(u32);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// What releasing a finished agent freed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Freed {
    /// Live processes in the agent's group when it was released: the agent and anything it started.
    pub processes: usize,
    /// Processes of the group still running after the kill. Zero means nothing lingers.
    pub lingering: usize,
}

impl GroupGuard {
    /// Free the agent and everything it started: kill the whole group, reap the leader, and
    /// check that no process of the group is still running.
    pub async fn release(mut self, child: &mut tokio::process::Child) -> Freed {
        let pgid = std::mem::take(&mut self.0);
        let processes = group_members(pgid);
        kill_group(pgid);
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        let mut lingering = group_members(pgid);
        for _ in 0..20 {
            if lingering == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            lingering = group_members(pgid);
        }
        Freed { processes, lingering }
    }
}

/// A finished turn's summary with what releasing its agent freed.
pub fn released(result: anyhow::Result<RunSummary>, freed: Freed) -> anyhow::Result<RunSummary> {
    result.map(|summary| RunSummary { freed: Some(freed), ..summary })
}

/// Running (not zombie) processes whose process group is `pgid`, read from `/proc`.
pub fn group_members(pgid: u32) -> usize {
    if pgid <= 1 {
        return 0;
    }
    let Ok(dir) = std::fs::read_dir("/proc") else { return 0 };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter(|stat| {
            // `pid (comm) state ppid pgrp ...`; comm may hold spaces, so split after the last `)`.
            let rest = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
            let fields: Vec<&str> = rest.split_whitespace().collect();
            fields.len() > 2 && fields[0] != "Z" && fields[2].parse::<u32>().ok() == Some(pgid)
        })
        .count()
}

/// Send SIGKILL to every process in the group led by `pgid`.
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: killpg only sends a signal; a stale group id fails with ESRCH.
        #[cfg(unix)]
        unsafe {
            libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = pgid;
    }
}

/// Spawn `cmd` as the leader of its own process group. Keep the guard as long as the child
/// should live: dropping it kills the whole group.
pub fn spawn_grouped(cmd: &mut tokio::process::Command) -> std::io::Result<(tokio::process::Child, GroupGuard)> {
    #[cfg(unix)]
    cmd.process_group(0).kill_on_drop(true);
    #[cfg(windows)]
    cmd.kill_on_drop(true);
    for name in parent_session_vars() {
        cmd.env_remove(name);
    }
    let child = cmd.spawn()?;
    let guard = GroupGuard(child.id().unwrap_or(0));
    Ok((child, guard))
}

/// Whether an environment variable belongs to a Claude Code session this process was
/// started from. An agent must not inherit them: they tie it to that session (a nested
/// session keeps no transcript) and carry its messaging token.
pub fn is_parent_session_var(name: &str) -> bool {
    name.starts_with("CLAUDE_CODE_") || matches!(name, "CLAUDECODE" | "CLAUDE_PID" | "CLAUDE_AGENT_SDK_VERSION" | "CLAUDE_EFFORT")
}

/// The names to remove from a child's environment, from this process's own.
pub fn parent_session_vars() -> Vec<String> {
    std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).filter(|k| is_parent_session_var(k)).collect()
}

/// Every harness Taskmaster can drive, by the name used on the command line.
pub const HARNESSES: [&str; 5] = ["claude", "agy", "grok", "codex", "opencode"];

/// Build the adapter for a harness named in [`HARNESSES`].
pub fn harness(name: &str) -> anyhow::Result<Arc<dyn Harness>> {
    Ok(match name {
        "claude" => Arc::new(claude::ClaudeCode::default()),
        "agy" => Arc::new(acp::AcpHarness::antigravity()?),
        "grok" => Arc::new(acp::AcpHarness::grok()?),
        "opencode" => Arc::new(acp::AcpHarness::opencode()?),
        "codex" => Arc::new(codex::Codex::default()),
        other => anyhow::bail!("unknown harness `{other}`: use one of {}", HARNESSES.join(", ")),
    })
}

/// Directories under the home directory that harnesses keep their own state in. Reads
/// there are allowed, because an agent consults its own session files.
pub const HARNESS_STATE_DIRS: [&str; 3] = [".gemini", ".grok", ".opencode"];

/// Rules every worker is told first. Harnesses that end their turn on the first refused call
/// need to know up front which tools do not exist.
pub const WORKER_RULES: &str = "Rules: you have no shell. Never try to run commands; use only your \
file reading and editing tools and the Dictionary tools. Work only inside the current directory. \
If any call is refused, stop and state what was refused.\n\nTask: ";

/// Rules for agents that only read: reviewers and verifiers.
pub const REVIEWER_RULES: &str = "Rules: you can only read files in the current directory. You have \
no shell and cannot edit, create or delete anything; do not try. Answer from what you read.\n\n\
Task: ";

/// The Dictionary MCP server a worker may read from, with its run-scoped token.
#[derive(Debug, Clone)]
pub struct DictionaryAccess {
    /// MCP server name as the harness will see it.
    pub name: String,
    pub url: String,
    pub bearer_token: String,
}

#[derive(Debug, Clone)]
pub struct WorkerTask {
    pub brief: String,
    /// The worker's worktree. It is the only place the worker may read or write.
    pub cwd: PathBuf,
    pub model: Option<String>,
    /// Reasoning effort (`low`, `medium`, `high`) for harnesses that take one. `None` keeps the default.
    pub effort: Option<String>,
    pub dictionary: Option<DictionaryAccess>,
    pub timeout: Duration,
    /// Give up on the agent when it sends nothing for this long while no tool of its is
    /// running: the provider has stalled. `None` waits for the whole `timeout`.
    pub stall: Option<Duration>,
    /// Reviewers and verifiers: the worktree is mounted read-only and edit tools are removed.
    pub read_only: bool,
    /// Use the harness's native code mode where it has one (Claude Code, interactive).
    pub code_mode: bool,
}

impl WorkerTask {
    /// The brief with the standing rules for this kind of task in front of it.
    pub fn framed_brief(&self) -> String {
        let rules = if self.read_only { REVIEWER_RULES } else { WORKER_RULES };
        format!("{rules}{}", self.brief)
    }
}

/// The agent went silent while waiting on its model, so it was stopped. A fresh agent
/// usually gets an answer at once: the provider queued this request, not the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stalled(pub Duration);

impl std::fmt::Display for Stalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the agent sent nothing for {}s while waiting on its model", self.0.as_secs())
    }
}

impl std::error::Error for Stalled {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    Text { text: String },
    Thought { text: String },
    ToolCall { id: String, name: String, input: Value },
    /// Output a harness attached to a tool call. `diff` is set only when the harness sent one.
    /// An empty `text` with a `diff` is still a result. This is never a substitute for a thought.
    ToolResult { id: String, text: String, diff: Option<String> },
    PermissionDecided { tool: String, allowed: bool },
}

/// Keep a tool result small enough for the transcript. The cut is marked, and nothing is added.
pub(crate) fn clip_result(text: &str) -> String {
    const MAX: usize = 16_000;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunSummary {
    pub ok: bool,
    pub usage: Usage,
    /// Only some harnesses report a price.
    pub cost_usd: Option<f64>,
    /// Tool calls the gate refused.
    pub denied: u32,
    /// What was freed when the agent was released after its turn. `None` before release.
    pub freed: Option<Freed>,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> &'static str;

    /// Run one task to completion, streaming events as they happen.
    async fn run(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary>;
}

#[cfg(test)]
mod group_tests {
    use super::*;

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn dropping_the_guard_kills_a_grandchild() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 300 & echo $!; wait"]).stdout(std::process::Stdio::piped());
        let (mut child, guard) = spawn_grouped(&mut cmd).unwrap();
        let mut line = String::new();
        let out = child.stdout.take().unwrap();
        tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(out), &mut line).await.unwrap();
        let sleeper: i32 = line.trim().parse().unwrap();
        assert!(alive(sleeper));
        drop(guard);
        let _ = child.wait().await;
        for _ in 0..50 {
            if !alive(sleeper) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the grandchild survived the group kill");
    }

    #[tokio::test]
    async fn release_frees_the_agent_and_what_it_started() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 300 & sleep 300 & echo go; wait"]).stdout(std::process::Stdio::piped());
        let (mut child, guard) = spawn_grouped(&mut cmd).unwrap();
        let mut line = String::new();
        let out = child.stdout.take().unwrap();
        tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(out), &mut line).await.unwrap();
        let freed = guard.release(&mut child).await;
        assert_eq!(freed, Freed { processes: 3, lingering: 0 });
    }
}
