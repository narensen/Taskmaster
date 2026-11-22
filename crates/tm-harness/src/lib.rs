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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub use gate::{Decision, PermissionGate, PermissionRequest, ToolKind, WorktreeGate, host_of};
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

/// Send SIGKILL to every process in the group led by `pgid`. Windows has no process
/// groups here: the direct child is ended by `kill_on_drop`, and a tree kill by id could
/// reach a process that reused the id after the child exited.
pub fn kill_group(pgid: u32) {
    if pgid > 1 {
        // SAFETY: killpg only sends a signal; a stale group id fails with ESRCH.
        #[cfg(unix)]
        unsafe {
            libc::killpg(pgid as libc::pid_t, libc::SIGKILL);
        }
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

/// Rules for a worker with no shell (its harness cannot be sandboxed here). Harnesses that
/// end their turn on the first refused call need to know up front which tools do not exist.
pub const WORKER_RULES: &str = "Rules: you have no shell in this run, so do not try to run commands; use \
your file reading and editing tools and the Dictionary tools. Work only inside the current directory. \
If a call is refused, say which one in your reply and carry on without it.\n\nTask: ";

/// Rules for a worker whose shell runs inside the sandbox.
pub const SHELL_WORKER_RULES: &str = "Rules: you have a shell, confined to a sandbox. Only the current directory, \
/tmp and $TASKMASTER_SCRATCH can be written; package indexes can be reached to install what the tests need \
(for example `python3 -m venv $TASKMASTER_SCRATCH/venv`, then its pip). Work only inside the current directory. \
Run the tests for your files before you say you are done, and fix what fails. Leave no server or watcher running \
when you finish. If a call is refused, say which one in your reply and carry on without it.\n\nTask: ";

/// What a worker's shell may write besides its worktree, and the environment it sees there.
/// Only honoured by a harness that runs inside the sandbox ([`Harness::sandboxed_shell`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellScope {
    /// Writable directories: the agent's scratch and the run's package caches.
    pub writable: Vec<PathBuf>,
    /// Set for every command the agent runs, after the daemon's Python variables are removed.
    pub env: Vec<(String, String)>,
}

/// Hosts a worker may fetch from when the package-index setting is on: what `pip`, `npm`
/// and `cargo` need to install, and nothing else.
pub const PACKAGE_INDEXES: [&str; 6] = ["pypi.org", "files.pythonhosted.org", "registry.npmjs.org", "crates.io", "static.crates.io", "index.crates.io"];

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
    /// A sandboxed shell for a builder. `None` keeps the shell off.
    pub shell: Option<ShellScope>,
}

/// Standing instruction added when Code Mode (`run_code`) is active for a read-only agent.
pub const CODE_MODE_READ_RULES: &str = "Code Mode is active: prefer batching file reads, searches, symbol lookups, and diffs in a single `run_code` script (`readMany`, `grep`, `ls`, `symbols`, `diff`) instead of serial tool calls.\n\n";

/// Standing instruction added when Code Mode (`run_code`) is active for a builder.
pub const CODE_MODE_WRITE_RULES: &str = "Code Mode is active: batch reads (`readMany`, `symbols`, `grep`), edits (`applyPatch`, `patch`, `edit`, `write`), checks (`diff`, `check`), and tests (`test()`, `sh`) inside `run_code` scripts instead of individual file or shell tool calls.\n\n";

impl WorkerTask {
    /// The brief with the standing rules for this kind of task in front of it.
    pub fn framed_brief(&self) -> String {
        let rules = if self.read_only {
            REVIEWER_RULES
        } else if self.shell.is_some() {
            SHELL_WORKER_RULES
        } else {
            WORKER_RULES
        };
        if self.code_mode {
            let code = if self.read_only { CODE_MODE_READ_RULES } else { CODE_MODE_WRITE_RULES };
            format!("{code}{rules}{}", self.brief)
        } else {
            format!("{rules}{}", self.brief)
        }
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

/// Rate-limit / usage window reported by a provider CLI (O4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitReport {
    /// Utilization percentage (0.0..=100.0) of the 5-hour rolling window, if reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_5h_pct: Option<f64>,
    /// Reset time of the 5-hour window (ISO-8601 or CLI label), if reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_5h: Option<String>,
    /// Utilization percentage (0.0..=100.0) of the 7-day rolling window, if reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_7d_pct: Option<f64>,
    /// Reset time of the 7-day window (ISO-8601 or CLI label), if reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_7d: Option<String>,
    /// Source label (`"cli"`, `"event"`, `"oauth"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl RateLimitReport {
    pub fn is_empty(&self) -> bool {
        self.window_5h_pct.is_none() && self.window_7d_pct.is_none()
    }
}

/// Normalize a utilization ratio (0..=1) or percentage (0..=100) into 0..=100.
fn normalize_pct(v: f64, is_ratio_hint: bool) -> Option<f64> {
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let pct = if is_ratio_hint && v <= 1.0 { v * 100.0 } else { v };
    Some(pct.clamp(0.0, 100.0))
}

fn read_window(obj: &Value) -> (Option<f64>, Option<String>) {
    let pct = obj
        .get("used_percent")
        .or_else(|| obj.get("usedPercent"))
        .or_else(|| obj.get("percent"))
        .or_else(|| obj.get("pct"))
        .and_then(|v| v.as_f64())
        .and_then(|v| normalize_pct(v, false))
        .or_else(|| {
            obj.get("utilization")
                .and_then(|v| v.as_f64())
                .and_then(|v| normalize_pct(v, true))
        });
    let reset = obj
        .get("resets_at")
        .or_else(|| obj.get("resetsAt"))
        .or_else(|| obj.get("reset_at"))
        .or_else(|| obj.get("reset"))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .filter(|s| !s.trim().is_empty());
    (pct, reset)
}

/// Extract rate-limit / usage window data from a CLI JSON event (Claude, Codex, or ACP).
pub fn parse_rate_limit_value(val: &Value, source: &str) -> Option<RateLimitReport> {
    let container = val
        .get("rate_limit_info")
        .or_else(|| val.get("rate_limits"))
        .or_else(|| val.get("rateLimits"))
        .or_else(|| val.get("rateLimit"))
        .or_else(|| val.get("info").and_then(|i| i.get("rate_limits")))
        .or_else(|| val.get("usage").and_then(|u| u.get("rate_limits")))
        .unwrap_or(val);

    let mut report = RateLimitReport { source: Some(source.to_string()), ..Default::default() };

    if let Some(w5) = container
        .get("five_hour")
        .or_else(|| container.get("fiveHour"))
        .or_else(|| container.get("primary"))
        .or_else(|| container.get("window_5h"))
    {
        let (pct, reset) = read_window(w5);
        report.window_5h_pct = pct;
        report.reset_5h = reset;
    }
    if let Some(w7) = container
        .get("seven_day")
        .or_else(|| container.get("sevenDay"))
        .or_else(|| container.get("weekly"))
        .or_else(|| container.get("secondary"))
        .or_else(|| container.get("window_7d"))
    {
        let (pct, reset) = read_window(w7);
        report.window_7d_pct = pct;
        report.reset_7d = reset;
    }

    // Single-window flat shape (e.g. Claude `rate_limit_info: { rateLimitType: "five_hour", utilization: 0.85, resetsAt: ... }`).
    if report.is_empty() {
        let (pct, reset) = read_window(container);
        if let Some(pct) = pct {
            let kind = container
                .get("rateLimitType")
                .or_else(|| container.get("rate_limit_type"))
                .or_else(|| container.get("window"))
                .and_then(|v| v.as_str())
                .unwrap_or("five_hour");
            if kind.contains("seven") || kind.contains("week") || kind.contains("7d") {
                report.window_7d_pct = Some(pct);
                report.reset_7d = reset;
            } else {
                report.window_5h_pct = Some(pct);
                report.reset_5h = reset;
            }
        }
    }

    if let Some(pct) = container.get("window_5h_pct").and_then(|v| v.as_f64()) {
        report.window_5h_pct = normalize_pct(pct, false);
    }
    if let Some(pct) = container.get("window_7d_pct").and_then(|v| v.as_f64()) {
        report.window_7d_pct = normalize_pct(pct, false);
    }
    if let Some(r) = container.get("reset_5h").and_then(|v| v.as_str()) {
        report.reset_5h = Some(r.to_string());
    }
    if let Some(r) = container.get("reset_7d").and_then(|v| v.as_str()) {
        report.reset_7d = Some(r.to_string());
    }

    (!report.is_empty()).then_some(report)
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
    /// The provider assigned or resumed a session id for this call (H2 / O7).
    SessionStarted { session_id: String },
    /// Rate-limit / usage window reported by the provider CLI (O4).
    RateLimit(RateLimitReport),
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

/// A session kept open after its first turn, so a repair can be sent to the same agent as
/// a follow-up turn: it keeps everything it read and did, and the provider's prompt cache.
#[async_trait]
pub trait Session: Send + Sync {
    /// Send one more turn. Events go to `events` until the turn ends.
    async fn follow_up(&mut self, text: String, events: mpsc::Sender<HarnessEvent>, timeout: Duration) -> anyhow::Result<RunSummary>;

    /// The provider's resumable session id, if this session can be resumed later by id (H2 / O7).
    fn session_id(&self) -> Option<&str> {
        None
    }

    /// Release the in-memory session/process while leaving any on-disk provider session intact
    /// so it can be resumed later by id (O7). Returns the resumable session id, if any.
    fn detach(self: Box<Self>) -> Option<String> {
        self.session_id().map(str::to_string)
    }

    /// End the session and free the agent and everything it started.
    async fn close(self: Box<Self>) -> Freed;
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> &'static str;

    /// [`Harness::run`], keeping the session open afterwards when the harness can. `None`
    /// when it cannot: the agent was released as usual.
    async fn run_kept(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn Session>>) {
        (self.run(task, gate, events).await, None)
    }

    /// Resume a previously recorded provider session by id with a short follow-up prompt
    /// (H2 restart resume and O7 hibernated warm-session resume). Returns an error when the
    /// harness does not support session resume so the caller can fall back to a fresh brief.
    async fn resume_kept(
        &self,
        _session_id: &str,
        _prompt: String,
        _task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        _events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn Session>>) {
        (Err(anyhow::anyhow!("{} does not support resuming a session by id", self.id())), None)
    }

    /// Whether a command this agent runs stays inside an OS sandbox confined to its worktree
    /// and [`ShellScope`]. Only then is it given a shell.
    fn sandboxed_shell(&self) -> bool {
        false
    }

    /// Run one task to completion, streaming events as they happen.
    async fn run(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary>;
}

#[cfg(all(test, unix))]
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
