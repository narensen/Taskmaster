//! Claude Code adapter: `claude -p --output-format stream-json`.
//!
//! The stream is one-way, so confinement is set by launch flags and by the OS instead of
//! the gate. Where bubblewrap can run, Claude Code runs inside the same sandbox as the ACP
//! harnesses: the filesystem is read-only except the worktree, Claude Code's own state
//! (`~/.claude` and `~/.claude.json`) and, for a builder with a shell, its scratch and
//! cache directories. A builder there gets the Bash tool, so it can run its tests.
//! Without the sandbox the Bash tool is removed, as before. Edits are auto-accepted only
//! inside the working directory, and any other call that would need approval is refused
//! because nobody is there to approve it.
//!
//! In Code Mode (`code_mode`), a builder's own Write, Edit and Bash tools are withheld, so
//! its edits, commands and test runs go through Taskmaster's `run_code`, one script for
//! many steps. Reading stays with its own tools.

use std::sync::Arc;

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::sandbox::Sandbox;
use crate::{Harness, HarnessEvent, PermissionGate, RunSummary, Usage, WorkerTask};

#[derive(Clone)]
pub struct ClaudeCode {
    pub binary: String,
    /// The sandbox it runs in, when bubblewrap can run here. `None` runs it unconfined, with
    /// no shell.
    pub sandbox: Option<Sandbox>,
}

impl Default for ClaudeCode {
    fn default() -> Self {
        let sandbox = Sandbox::with_state_dirs(&[".claude", ".claude.json"]).ok().filter(|_| crate::sandbox::bwrap_usable());
        ClaudeCode { binary: "claude".into(), sandbox }
    }
}

/// Claude Code's tools that change files or run commands. In Code Mode `run_code` does all of it.
const WRITING_TOOLS: [&str; 5] = ["Write", "Edit", "MultiEdit", "NotebookEdit", "Bash"];

impl ClaudeCode {
    fn run_code_tool(task: &WorkerTask) -> String {
        match &task.dictionary {
            Some(dict) => crate::run_code_tool_name("claude", &dict.name),
            None => "run_code".to_string(),
        }
    }

    /// The arguments for one stateless task: permissions, tools, model, effort and the Dictionary.
    pub fn args(&self, task: &WorkerTask) -> Vec<String> {
        self.args_with(task, &task.framed_brief_for(&Self::run_code_tool(task)), false, None)
    }

    /// Arguments for one turn, optionally keeping the session on disk (`persist`) or resuming an
    /// earlier Claude Code session (`resume`) so repair turns reuse the provider prompt cache.
    pub fn args_with(&self, task: &WorkerTask, prompt: &str, persist: bool, resume: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = vec!["-p".into(), prompt.to_string()];
        if let Some(session_id) = resume {
            args.extend(["--resume".into(), session_id.to_string()]);
        }
        args.extend(["--output-format", "stream-json", "--verbose", "--strict-mcp-config"].map(String::from));
        if !persist {
            args.push("--no-session-persistence".into());
        }
        let full_access = !task.read_only && task.permission_mode == Some(crate::PermissionMode::FullAccess);
        let shell = ((self.sandbox.is_some() && task.shell.is_some()) || full_access) && !task.read_only;
        let mut denied: Vec<&str> = if task.read_only {
            // With no edit tools and nobody to approve a prompt, the session can only read.
            vec!["Bash", "Edit", "Write", "MultiEdit", "NotebookEdit"]
        } else if task.code_mode {
            WRITING_TOOLS.to_vec()
        } else if shell {
            Vec::new()
        } else {
            vec!["Bash"]
        };
        denied.dedup();
        // The stream cannot stop to ask, so Supervised and Auto-accept edits are honoured
        // through `run_code` (Code Mode), which goes through the gate. With Claude Code's
        // own tools they stay at `acceptEdits`: `default` would refuse every edit.
        let permission = match task.permission_mode {
            _ if task.read_only => "default",
            Some(crate::PermissionMode::FullAccess) => "bypassPermissions",
            _ => "acceptEdits",
        };
        args.extend(["--permission-mode".into(), permission.into()]);
        if !denied.is_empty() {
            args.push("--disallowedTools".into());
            args.extend(denied.iter().map(|t| t.to_string()));
        }
        let mut allowed: Vec<String> = Vec::new();
        if shell && !task.code_mode {
            allowed.push("Bash".into());
        }
        if let Some(model) = &task.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &task.effort {
            args.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(dict) = &task.dictionary {
            let config = json!({ "mcpServers": { &dict.name: {
                "type": "http",
                "url": dict.url,
                "headers": { "Authorization": format!("Bearer {}", dict.bearer_token) },
            }}});
            args.extend(["--mcp-config".into(), config.to_string()]);
            allowed.push(format!("mcp__{}", dict.name));
        }
        if !allowed.is_empty() {
            args.push("--allowedTools".into());
            args.extend(allowed);
        }
        args
    }

    fn command_with(&self, task: &WorkerTask, prompt: &str, persist: bool, resume: Option<&str>) -> anyhow::Result<Command> {
        let program = crate::sandbox::resolve_binary(&self.binary).with_context(|| format!("`{}` is not installed", self.binary))?;
        let args = self.args_with(task, prompt, persist, resume);
        Ok(match &self.sandbox {
            Some(sandbox) => {
                let mut cmd = Command::new("bwrap");
                cmd.args(sandbox.bwrap_args_with(&program, &args, &task.cwd, task.read_only, task.shell.as_ref()));
                cmd
            }
            None => {
                let mut cmd = Command::new(program);
                cmd.args(&args).current_dir(&task.cwd);
                cmd
            }
        })
    }

    async fn execute(
        &self,
        task: &WorkerTask,
        prompt: &str,
        persist: bool,
        resume: Option<&str>,
        events: mpsc::Sender<HarnessEvent>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<(RunSummary, Option<String>)> {
        let mut cmd = self.command_with(task, prompt, persist, resume)?;
        let (proc, mut io) = crate::child::start(&mut cmd, false, task.hold.as_ref())
            .await
            .with_context(|| format!("failed to start `{}`", self.binary))?;
        let mut stream = Stream { session_id: resume.map(str::to_string), emitted: false, tools: Default::default() };
        if let Some(id) = resume {
            let _ = events.send(HarnessEvent::SessionStarted { session_id: id.to_string() }).await;
            stream.emitted = true;
        }
        let result = match tokio::time::timeout(timeout, stream.read(&mut io, &events, task.stall)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("claude timed out after {:?}", timeout)),
        };
        let freed = proc.release().await;
        match result {
            Ok((mut summary, session_id)) => {
                summary.freed = Some(freed);
                Ok((summary, session_id))
            }
            Err(err) => Err(err),
        }
    }
}

/// What a turn's stream has said so far: its session, whether that was announced,
/// and any tool calls still awaiting their results.
#[derive(Default)]
struct Stream {
    session_id: Option<String>,
    emitted: bool,
    tools: std::collections::BTreeSet<String>,
}

impl Stream {
    /// Read the stream to its result line, stopping with [`crate::Stalled`] if the model
    /// sends nothing for `stall` while no tool call is in flight.
    async fn read(
        &mut self,
        io: &mut crate::child::AgentIo,
        events: &mpsc::Sender<HarnessEvent>,
        stall: Option<std::time::Duration>,
    ) -> anyhow::Result<(RunSummary, Option<String>)> {
        loop {
            let next = io.next_line();
            let line = match stall.filter(|_| self.tools.is_empty()) {
                Some(limit) => match tokio::time::timeout(limit, next).await {
                    Ok(line) => line?,
                    Err(_) => return Err(crate::Stalled(limit).into()),
                },
                None => next.await?,
            };
            let Some(line) = line else {
                break;
            };
            if let Some(summary) = self.line(&line, Some(events)).await {
                return Ok((summary, self.session_id.clone()));
            }
        }
        bail!("claude exited without a result event")
    }

    /// Take in one line, sending its events when there is somewhere to send them (a replay of
    /// lines an earlier daemon already sent has not). Returns the summary at the result line.
    async fn line(&mut self, line: &str, events: Option<&mpsc::Sender<HarnessEvent>>) -> Option<RunSummary> {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return None };
        let mut out = Vec::new();
        if let Some(id) = msg["session_id"].as_str().filter(|s| !s.is_empty()) {
            let changed = self.session_id.as_deref() != Some(id);
            self.session_id = Some(id.to_string());
            if !self.emitted || changed {
                out.push(HarnessEvent::SessionStarted { session_id: id.to_string() });
                self.emitted = true;
            }
        }
        if (msg["type"].as_str() == Some("rate_limit_event") || msg.get("rate_limit_info").is_some() || msg.get("rate_limits").is_some())
            && let Some(report) = crate::parse_rate_limit_value(&msg, "claude-cli")
        {
            out.push(HarnessEvent::RateLimit(report));
        }
        let summary = match msg["type"].as_str() {
            Some("assistant") => {
                out.extend(content_events(&msg["message"]["content"]));
                None
            }
            Some("user") => {
                out.extend(tool_results(&msg["message"]["content"]));
                None
            }
            Some("result") => {
                // Claude Code refuses on its own and reports it here, not as it happens.
                out.extend(denials(&msg));
                Some(summary(&msg))
            }
            _ => None,
        };
        for event in &out {
            match event {
                HarnessEvent::ToolCall { id, .. } if !id.is_empty() => {
                    self.tools.insert(id.clone());
                }
                HarnessEvent::ToolResult { id, .. } => {
                    self.tools.remove(id);
                }
                _ => {}
            }
        }
        if let Some(events) = events {
            for event in out {
                let _ = events.send(event).await;
            }
        }
        summary
    }
}

/// A persisted Claude Code session resumed with `--resume <session_id>` for cached repair turns.
struct ClaudeSession {
    harness: ClaudeCode,
    task: WorkerTask,
    session_id: String,
    cleaned: bool,
}

impl ClaudeSession {
    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        if let Some(home) = std::env::var_os("HOME") {
            cleanup_claude_session(&std::path::PathBuf::from(home), &self.session_id);
        }
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Remove a persisted Claude Code session JSONL file (and optional session dir) under
/// `<home>/.claude/projects/*/<session_id>.jsonl`.
fn cleanup_claude_session(home: &std::path::Path, session_id: &str) -> usize {
    if session_id.len() < 4 || !session_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return 0;
    }
    let projects = home.join(".claude/projects");
    let Ok(entries) = std::fs::read_dir(&projects) else { return 0 };
    let target_file = format!("{session_id}.jsonl");
    let mut removed = 0usize;
    for dir in entries.flatten() {
        let path = dir.path();
        if !path.is_dir() {
            continue;
        }
        let file = path.join(&target_file);
        if file.is_file() && std::fs::remove_file(&file).is_ok() {
            removed += 1;
        }
        let sub = path.join(session_id);
        if sub.is_dir() && std::fs::remove_dir_all(&sub).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[async_trait]
impl crate::Session for ClaudeSession {
    async fn follow_up(
        &mut self,
        text: String,
        events: mpsc::Sender<HarnessEvent>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<RunSummary> {
        let (summary, next_id) = self.harness.execute(&self.task, &text, true, Some(&self.session_id), events, timeout).await?;
        if let Some(id) = next_id {
            self.session_id = id;
        }
        Ok(summary)
    }

    fn session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    fn detach(mut self: Box<Self>) -> Option<String> {
        // Keep the persisted JSONL session file on disk so a later repair or restart can resume it.
        self.cleaned = true;
        Some(self.session_id.clone())
    }

    async fn close(mut self: Box<Self>) -> crate::Freed {
        self.cleanup();
        crate::Freed::default()
    }
}

#[async_trait]
impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn sandboxed_shell(&self) -> bool {
        self.sandbox.is_some()
    }

    async fn run_kept(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        let prompt = task.framed_brief_for(&Self::run_code_tool(&task));
        match self.execute(&task, &prompt, true, None, events, task.timeout).await {
            Ok((summary, Some(session_id))) if summary.ok => {
                let kept: Box<dyn crate::Session> = Box::new(ClaudeSession { harness: self.clone(), task, session_id, cleaned: false });
                (Ok(summary), Some(kept))
            }
            Ok((summary, Some(session_id))) => {
                if let Some(home) = std::env::var_os("HOME") {
                    cleanup_claude_session(&std::path::PathBuf::from(home), &session_id);
                }
                (Ok(summary), None)
            }
            Ok((summary, None)) => (Ok(summary), None),
            Err(err) => (Err(err), None),
        }
    }

    async fn resume_kept(
        &self,
        session_id: &str,
        prompt: String,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        match self.execute(&task, &prompt, true, Some(session_id), events, task.timeout).await {
            Ok((summary, next_id)) if summary.ok => {
                let id = next_id.unwrap_or_else(|| session_id.to_string());
                let kept: Box<dyn crate::Session> =
                    Box::new(ClaudeSession { harness: self.clone(), task, session_id: id, cleaned: false });
                (Ok(summary), Some(kept))
            }
            Ok((summary, _)) => (Ok(summary), None),
            Err(err) => (Err(err), None),
        }
    }

    async fn reattach(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
        attached: crate::child::Attached,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        // The stream is one-way: what the last daemon read gives the session, and the result
        // when the turn ended before this daemon took over.
        let mut stream = Stream::default();
        let mut ended = None;
        for line in attached.outputs() {
            if let Some(summary) = stream.line(line, None).await {
                ended = Some(summary);
            }
        }
        let crate::child::Attached { mut io, proc, keep, .. } = attached;
        let result = match ended {
            Some(summary) => Ok((summary, stream.session_id.clone())),
            None => match tokio::time::timeout(task.timeout, stream.read(&mut io, &events, task.stall)).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("claude timed out after {:?}", task.timeout)),
            },
        };
        drop(io);
        let freed = proc.release().await;
        match result {
            Ok((mut summary, session_id)) => {
                summary.freed = Some(freed);
                let kept: Option<Box<dyn crate::Session>> = match session_id {
                    Some(session_id) if keep && summary.ok => Some(Box::new(ClaudeSession { harness: self.clone(), task, session_id, cleaned: false })),
                    Some(session_id) if keep => {
                        if let Some(home) = std::env::var_os("HOME") {
                            cleanup_claude_session(&std::path::PathBuf::from(home), &session_id);
                        }
                        None
                    }
                    _ => None,
                };
                (Ok(summary), kept)
            }
            Err(err) => (Err(err), None),
        }
    }

    async fn run(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let prompt = task.framed_brief_for(&Self::run_code_tool(&task));
        self.execute(&task, &prompt, false, None, events, task.timeout).await.map(|(summary, _)| summary)
    }
}

/// The calls Claude Code refused in a turn, as the refusals the other harnesses report.
fn denials(result: &Value) -> Vec<HarnessEvent> {
    let Some(list) = result["permission_denials"].as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for (i, denial) in list.iter().enumerate() {
        let tool = denial["tool_name"].as_str().unwrap_or("tool").to_string();
        let id = denial["tool_use_id"].as_str().map(str::to_string).unwrap_or_else(|| format!("denied-{i}"));
        out.push(HarnessEvent::ToolCall { id, name: tool.clone(), input: denial["tool_input"].clone() });
        out.push(HarnessEvent::PermissionDecided { tool, allowed: false });
    }
    out
}

fn content_events(content: &Value) -> Vec<HarnessEvent> {
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    blocks
        .iter()
        .filter_map(|block| {
            let text = |key: &str| block[key].as_str().unwrap_or_default().to_string();
            match block["type"].as_str()? {
                "text" => Some(HarnessEvent::Text { text: text("text") }),
                "thinking" => Some(HarnessEvent::Thought { text: text("thinking") }),
                "tool_use" => Some(HarnessEvent::ToolCall {
                    id: text("id"),
                    name: text("name"),
                    input: block["input"].clone(),
                }),
                "tool_result" => tool_result_event(block),
                _ => None,
            }
        })
        .collect()
}

/// A `tool_result` block. Its content is a string or a list of text blocks. Other user
/// text is left alone, so a prompt echo does not become a step.
fn tool_results(content: &Value) -> Vec<HarnessEvent> {
    let Some(blocks) = content.as_array() else { return Vec::new() };
    blocks.iter().filter(|block| block["type"] == "tool_result").filter_map(tool_result_event).collect()
}

fn tool_result_event(block: &Value) -> Option<HarnessEvent> {
    let id = block["tool_use_id"].as_str().unwrap_or_default().to_string();
    let text = result_text(&block["content"]);
    if text.is_empty() {
        return None;
    }
    Some(HarnessEvent::ToolResult { id, text: crate::clip_result(&text), diff: None })
}

fn result_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .map(|blocks| {
            blocks.iter().filter_map(|block| block["text"].as_str()).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default()
}

fn summary(result: &Value) -> RunSummary {
    let usage = &result["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let denied = result["permission_denials"].as_array().map_or(0, |d| d.len() as u32);
    RunSummary {
        ok: !result["is_error"].as_bool().unwrap_or(true),
        usage: Usage {
            input: count("input_tokens"),
            output: count("output_tokens"),
            cache_read: count("cache_read_input_tokens"),
            cache_write: count("cache_creation_input_tokens"),
        },
        cost_usd: result["total_cost_usd"].as_f64(),
        denied,
        freed: None,
        requests: result["num_turns"].as_u64().map(|n| n as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(read_only: bool, shell: bool, code_mode: bool) -> WorkerTask {
        WorkerTask {
            brief: "build".into(),
            cwd: "/w".into(),
            model: Some("claude-sonnet-5-5".into()),
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(1),
            stall: None,
            read_only,
            code_mode,
            shell: shell.then(crate::ShellScope::default),
            permission_mode: None,
            hold: None,
        }
    }

    fn sandboxed() -> ClaudeCode {
        ClaudeCode { binary: "claude".into(), sandbox: Some(Sandbox { home: "/home/u".into(), state_dirs: vec!["/home/u/.claude".into()] }) }
    }

    fn after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        let Some(at) = args.iter().position(|a| a == flag) else { return Vec::new() };
        args[at + 1..].iter().take_while(|a| !a.starts_with("--")).map(String::as_str).collect()
    }

    #[test]
    fn a_sandboxed_builder_gets_bash_and_nothing_else_does() {
        let args = sandboxed().args(&task(false, true, false));
        assert!(after(&args, "--allowedTools").contains(&"Bash"), "{args:?}");
        assert!(!after(&args, "--disallowedTools").contains(&"Bash"));
        // No sandbox, or no shell for this task: Bash stays out, as before.
        let bare = ClaudeCode { binary: "claude".into(), sandbox: None }.args(&task(false, true, false));
        assert!(after(&bare, "--disallowedTools").contains(&"Bash") && !after(&bare, "--allowedTools").contains(&"Bash"));
        let no_shell = sandboxed().args(&task(false, false, false));
        assert!(after(&no_shell, "--disallowedTools").contains(&"Bash"));
        // A reviewer only reads, sandbox or not.
        let reader = sandboxed().args(&task(true, true, false));
        assert_eq!(after(&reader, "--disallowedTools"), ["Bash", "Edit", "Write", "MultiEdit", "NotebookEdit"]);
        assert!(after(&reader, "--allowedTools").is_empty());
    }

    #[test]
    fn the_run_permission_mode_maps_onto_claude_flags() {
        use crate::PermissionMode::*;
        let with = |mode, read_only| {
            let mut t = task(read_only, false, false);
            t.permission_mode = Some(mode);
            ClaudeCode { binary: "claude".into(), sandbox: None }.args(&t)
        };
        // Claude Code cannot stop to ask, so the asking modes keep its edits working.
        for mode in [Supervised, AutoEdits, Auto] {
            assert_eq!(after(&with(mode, false), "--permission-mode"), ["acceptEdits"], "{mode:?}");
        }
        // Full access lifts the prompts and gives Bash even without the sandbox.
        let full = with(FullAccess, false);
        assert_eq!(after(&full, "--permission-mode"), ["bypassPermissions"]);
        assert!(!after(&full, "--disallowedTools").contains(&"Bash"), "{full:?}");
        // A reader stays a reader whatever the run's mode.
        let reader = with(FullAccess, true);
        assert_eq!(after(&reader, "--permission-mode"), ["default"]);
        assert!(after(&reader, "--disallowedTools").contains(&"Bash"));
    }

    #[test]
    fn code_mode_hands_writes_commands_and_tests_to_run_code() {
        let args = sandboxed().args(&task(false, true, true));
        let denied = after(&args, "--disallowedTools");
        for tool in WRITING_TOOLS {
            assert!(denied.contains(&tool), "{tool} should go through run_code: {args:?}");
        }
        assert!(!after(&args, "--allowedTools").contains(&"Bash"));
    }

    #[test]
    fn claude_runs_inside_the_sandbox_when_it_has_one() {
        let harness = sandboxed();
        let args = harness.sandbox.as_ref().unwrap().bwrap_args_with(std::path::Path::new("/bin/claude"), &harness.args(&task(false, true, false)), std::path::Path::new("/w"), false, Some(&crate::ShellScope::default()));
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..3], ["--ro-bind", "/", "/"]);
        assert!(args.windows(3).any(|w| w == ["--bind", "/home/u/.claude", "/home/u/.claude"]));
        assert!(args.windows(3).any(|w| w == ["--bind", "/w", "/w"]));
    }

    #[test]
    fn refusals_in_the_result_become_refusal_events() {
        let result = json!({ "permission_denials": [{ "tool_name": "Bash", "tool_use_id": "t9", "tool_input": { "command": "curl x" } }] });
        let events = denials(&result);
        assert!(matches!(&events[0], HarnessEvent::ToolCall { name, .. } if name == "Bash"));
        assert_eq!(events[1], HarnessEvent::PermissionDecided { tool: "Bash".into(), allowed: false });
    }

    #[test]
    fn maps_assistant_content_blocks() {
        let content = json!([
            { "type": "thinking", "thinking": "plan" },
            { "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "a.ts" } },
            { "type": "text", "text": "done" },
            { "type": "server_tool_use" }
        ]);
        let events = content_events(&content);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], HarnessEvent::Thought { text: "plan".into() });
        assert_eq!(events[2], HarnessEvent::Text { text: "done".into() });
    }

    #[test]
    fn reads_usage_and_cost_from_the_result_event() {
        // Shape recorded from Claude Code 2.1.325.
        let result = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "total_cost_usd": 0.0033,
            "usage": { "input_tokens": 8, "output_tokens": 463,
                       "cache_read_input_tokens": 69148, "cache_creation_input_tokens": 11406 },
            "permission_denials": [{}]
        });
        let s = summary(&result);
        assert!(s.ok);
        assert_eq!(s.usage.total(), 8 + 463 + 69148 + 11406);
        assert_eq!(s.cost_usd, Some(0.0033));
        assert_eq!(s.denied, 1);
    }

    #[test]
    fn warm_session_omits_no_persistence_and_resumes_by_session_id() {
        let harness = sandboxed();
        let t = task(false, true, true);
        let stateless = harness.args(&t);
        assert!(stateless.contains(&"--no-session-persistence".to_string()));
        assert!(!stateless.contains(&"--resume".to_string()));

        let kept = harness.args_with(&t, "initial", true, None);
        assert!(!kept.contains(&"--no-session-persistence".to_string()));
        assert!(!kept.contains(&"--resume".to_string()));

        let resumed = harness.args_with(&t, "fix it", true, Some("sess-42"));
        assert!(!resumed.contains(&"--no-session-persistence".to_string()));
        assert_eq!(after(&resumed, "--resume"), ["sess-42"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_handed_to_a_new_daemon_carries_on_without_repeating_what_was_sent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tm-claude-reattach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join("go");
        let started = dir.join("started");
        // A stand-in for `claude -p`: it says one thing, waits, says another and ends the turn.
        let script = format!(
            "#!/bin/sh\necho x >> '{started}'\n\
             echo '{{\"type\":\"system\",\"session_id\":\"sess-h3\"}}'\n\
             echo '{{\"type\":\"assistant\",\"session_id\":\"sess-h3\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"before\"}}]}}}}'\n\
             while [ ! -f '{go}' ]; do sleep 0.05; done\n\
             echo '{{\"type\":\"assistant\",\"session_id\":\"sess-h3\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"after\"}}]}}}}'\n\
             echo '{{\"type\":\"result\",\"is_error\":false,\"session_id\":\"sess-h3\",\"total_cost_usd\":0.25,\"usage\":{{\"input_tokens\":3,\"output_tokens\":4}}}}'\n",
            started = started.display(),
            go = go.display()
        );
        let bin = dir.join("claude");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let harness = ClaudeCode { binary: bin.display().to_string(), sandbox: None };
        let holder = crate::hold::in_process(&dir.join("hold")).unwrap();
        let spec = crate::hold::HoldSpec { holder: holder.clone(), key: "p/r/page:a".into(), meta: json!({}) };
        let mut first = task(false, false, false);
        first.cwd = dir.clone();
        first.timeout = std::time::Duration::from_secs(20);
        first.hold = Some(spec);
        let gate: Arc<dyn PermissionGate> = Arc::new(crate::gate::WorktreeGate::new(&dir, vec![]));
        let (tx, mut rx) = mpsc::channel(32);
        let running = tokio::spawn({
            let (harness, gate, first) = (harness.clone(), gate.clone(), first.clone());
            async move { harness.run(first, gate, tx).await }
        });
        assert_eq!(rx.recv().await, Some(HarnessEvent::SessionStarted { session_id: "sess-h3".into() }));
        assert_eq!(rx.recv().await, Some(HarnessEvent::Text { text: "before".into() }));
        // The daemon hands off and goes away mid-turn.
        assert_eq!(holder.hand_off(std::time::Duration::from_secs(60), std::time::Duration::from_millis(50)).await.unwrap(), 1);
        running.abort();
        let _ = running.await;

        // The next daemon takes the agent over; the agent goes on.
        let next = crate::hold::Holder::new(holder.socket(), None);
        let spec = crate::hold::HoldSpec { holder: next, key: "p/r/page:a".into(), meta: json!({}) };
        let attached = crate::child::attach(&spec, false).await.unwrap();
        std::fs::write(&go, "").unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let mut again = first.clone();
        again.hold = Some(spec);
        let (summary, kept) = harness.reattach(again, gate, tx, attached).await;
        let summary = summary.unwrap();
        assert!(summary.ok && kept.is_none());
        assert_eq!(summary.cost_usd, Some(0.25));
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        // Only what came after the hand-off: no second session line, no second "before".
        assert_eq!(events, vec![HarnessEvent::Text { text: "after".into() }]);
        assert_eq!(std::fs::read_to_string(&started).unwrap(), "x\n", "the agent was started again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_claude_session_removes_session_jsonl_and_dir_only() {
        let home = std::env::temp_dir().join(format!("tm-claude-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let proj = home.join(".claude/projects/-tmp-demo");
        std::fs::create_dir_all(proj.join("sess-clean-1")).unwrap();
        let target = proj.join("sess-clean-1.jsonl");
        let other = proj.join("sess-keep-2.jsonl");
        std::fs::write(&target, "{}\n").unwrap();
        std::fs::write(&other, "{}\n").unwrap();

        assert_eq!(cleanup_claude_session(&home, "sess-clean-1"), 2);
        assert!(!target.exists());
        assert!(!proj.join("sess-clean-1").exists());
        assert!(other.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_claude_stream_is_stopped_as_stalled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tm-claude-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude-hang");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let harness = ClaudeCode { binary: bin.display().to_string(), sandbox: None };
        let mut t = task(false, false, false);
        t.cwd = dir.clone();
        t.timeout = std::time::Duration::from_secs(30);
        t.stall = Some(std::time::Duration::from_millis(120));
        let gate: Arc<dyn PermissionGate> = Arc::new(crate::gate::WorktreeGate::new(&dir, vec![]));
        let (tx, _rx) = mpsc::channel(8);
        let err = harness.run(t, gate, tx).await.unwrap_err();
        assert!(
            err.downcast_ref::<crate::Stalled>().is_some(),
            "expected Stalled error on silent Claude stream, got: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_attaches_dictionary_mcp_and_allows_mcp_tools_in_code_mode_off_and_on() {
        let harness = sandboxed();
        for (read_only, code_mode) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut t = task(read_only, true, code_mode);
            t.dictionary = Some(crate::DictionaryAccess {
                name: "dictionary".into(),
                url: "http://127.0.0.1:43210/mcp".into(),
                bearer_token: "tok-claude".into(),
            });
            let args = harness.args(&t);
            assert!(
                after(&args, "--allowedTools").contains(&"mcp__dictionary"),
                "mcp__dictionary must be allowed for read_only={read_only}, code_mode={code_mode}: {args:?}"
            );
            let cfg_raw = after(&args, "--mcp-config").into_iter().next().expect("missing --mcp-config");
            let cfg: Value = serde_json::from_str(cfg_raw).unwrap();
            assert_eq!(cfg["mcpServers"]["dictionary"]["url"], "http://127.0.0.1:43210/mcp");
            assert_eq!(cfg["mcpServers"]["dictionary"]["headers"]["Authorization"], "Bearer tok-claude");
        }
    }
}
