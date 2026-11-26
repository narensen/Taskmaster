//! Codex adapter: `codex exec --json`.
//!
//! Like Claude Code this is a one-way event stream, so confinement is set at launch: Codex
//! runs its own OS sandbox, `workspace-write` for workers and `read-only` for reviewers.
//! That sandbox limits writes to the working directory but does not hide the rest of the
//! disk from reads.
//!
//! The launch flags and the `error` / `turn.failed` events were checked against Codex
//! 0.169. The item and usage events are written from its documented format and have not
//! been run here.

use std::sync::Arc;

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Harness, HarnessEvent, PermissionGate, RunSummary, Usage, WorkerTask};

/// The Dictionary token reaches Codex through this variable, never the command line.
const TOKEN_ENV: &str = "TASKMASTER_DICTIONARY_TOKEN";

#[derive(Clone)]
pub struct Codex {
    pub binary: String,
}

impl Default for Codex {
    fn default() -> Self {
        Codex { binary: "codex".into() }
    }
}

impl Codex {
    /// Arguments for one turn: stateless (`persist = false`) or persistent/resumed (`resume = Some(thread_id)`).
    ///
    /// Matches `codex exec [OPTIONS] [PROMPT]` and `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]`.
    pub fn args_with(&self, task: &WorkerTask, prompt: &str, persist: bool, resume: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = if resume.is_some() {
            vec!["exec".into(), "resume".into()]
        } else {
            vec!["exec".into()]
        };
        args.extend(["--json".into(), "--skip-git-repo-check".into()]);
        if !persist {
            args.push("--ephemeral".into());
        }
        // In Code Mode with Taskmaster's MCP server attached, disable Codex's native shell tool
        // and keep Codex's own sandbox read-only so edits and commands batch through `run_code`.
        let enforce_code_mode = task.code_mode && !task.read_only && task.dictionary.is_some();
        if enforce_code_mode {
            args.extend(["--disable".into(), "shell_tool".into()]);
        }
        if resume.is_none() {
            args.extend(["-C".into(), task.cwd.display().to_string()]);
            let sandbox = if task.read_only || enforce_code_mode {
                "read-only"
            } else if task.permission_mode == Some(crate::PermissionMode::FullAccess) {
                "danger-full-access"
            } else {
                "workspace-write"
            };
            args.extend(["--sandbox".into(), sandbox.into()]);
        }
        if let Some(model) = &task.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &task.effort {
            args.extend(["-c".into(), format!("model_reasoning_effort={effort:?}")]);
        }
        if let Some(dict) = &task.dictionary {
            // TOML string values; `{:?}` quotes and escapes them.
            args.extend(["-c".into(), format!("mcp_servers.{}.url={:?}", dict.name, dict.url)]);
            args.extend(["-c".into(), format!("mcp_servers.{}.bearer_token_env_var={TOKEN_ENV:?}", dict.name)]);
        }
        if let Some(thread_id) = resume {
            args.push(thread_id.to_string());
        }
        args.push(prompt.to_string());
        args
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
        let mut cmd = Command::new(&self.binary);
        cmd.args(self.args_with(task, prompt, persist, resume)).current_dir(&task.cwd);
        if let Some(dict) = &task.dictionary {
            cmd.env(TOKEN_ENV, &dict.bearer_token);
        }
        let (proc, mut io) = crate::child::start(&mut cmd, false, task.hold.as_ref())
            .await
            .with_context(|| format!("failed to start `{}`", self.binary))?;
        let mut stream = Stream { thread_id: resume.map(str::to_string), emitted: false, requests: 0, saw_tool: false };
        if let Some(id) = resume {
            let _ = events.send(HarnessEvent::SessionStarted { session_id: id.to_string() }).await;
            stream.emitted = true;
        }
        let result = match tokio::time::timeout(timeout, stream.read(&mut io, &events, task.stall)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("codex timed out after {:?}", timeout)),
        };
        let freed = proc.release().await;
        match result {
            Ok((mut summary, thread_id)) => {
                summary.freed = Some(freed);
                Ok((summary, thread_id))
            }
            Err(err) => Err(err),
        }
    }
}

/// What a turn's stream has said so far: its thread, and whether that was announced.
#[derive(Default)]
struct Stream {
    thread_id: Option<String>,
    emitted: bool,
    requests: u32,
    saw_tool: bool,
}

impl Stream {
    /// Read the stream to the end of its turn, stopping with [`crate::Stalled`] if the stream
    /// stays completely silent for `stall`.
    async fn read(
        &mut self,
        io: &mut crate::child::AgentIo,
        events: &mpsc::Sender<HarnessEvent>,
        stall: Option<std::time::Duration>,
    ) -> anyhow::Result<(RunSummary, Option<String>)> {
        loop {
            let next = io.next_line();
            let line = match stall {
                Some(limit) => match tokio::time::timeout(limit, next).await {
                    Ok(line) => line?,
                    Err(_) => return Err(crate::Stalled(limit).into()),
                },
                None => next.await?,
            };
            let Some(line) = line else {
                break;
            };
            if let Some(end) = self.line(&line, Some(events)).await {
                return end.map(|summary| (summary, self.thread_id.clone()));
            }
        }
        bail!("codex exited without completing its turn")
    }

    /// Take in one line, sending its events when there is somewhere to send them (a replay of
    /// lines an earlier daemon already sent has not). Returns the turn's end at its last line.
    async fn line(&mut self, line: &str, events: Option<&mpsc::Sender<HarnessEvent>>) -> Option<anyhow::Result<RunSummary>> {
        let Ok(msg) = serde_json::from_str::<Value>(line) else { return None };
        let mut out = Vec::new();
        if msg["type"].as_str() == Some("thread.started")
            && let Some(id) = msg["thread_id"].as_str().filter(|s| !s.is_empty())
        {
            let changed = self.thread_id.as_deref() != Some(id);
            self.thread_id = Some(id.to_string());
            if !self.emitted || changed {
                out.push(HarnessEvent::SessionStarted { session_id: id.to_string() });
                self.emitted = true;
            }
        }
        if (msg.get("rate_limits").is_some() || msg.get("info").and_then(|i| i.get("rate_limits")).is_some() || msg["type"].as_str() == Some("rate_limit"))
            && let Some(report) = crate::parse_rate_limit_value(&msg, "codex-cli")
        {
            out.push(HarnessEvent::RateLimit(report));
        }
        let end = match msg["type"].as_str() {
            Some("item.completed") => {
                let item_type = msg["item"]["type"].as_str().unwrap_or_default();
                let is_tool = matches!(item_type, "command_execution" | "file_change" | "mcp_tool_call");
                let is_model = matches!(item_type, "agent_message" | "reasoning") || is_tool;
                if is_model && (self.requests == 0 || self.saw_tool) {
                    self.requests = self.requests.saturating_add(1);
                    self.saw_tool = false;
                }
                if is_tool {
                    self.saw_tool = true;
                }
                out.extend(item_events(&msg["item"]));
                None
            }
            Some("turn.completed") => {
                let usage = read_usage(&msg["usage"]);
                let requests = (self.requests > 0).then_some(self.requests).or(Some(1));
                Some(Ok(RunSummary { ok: true, usage, cost_usd: None, denied: 0, freed: None, requests }))
            }
            Some("turn.failed") => Some(Err(anyhow::anyhow!("codex: {}", error_message(&msg["error"])))),
            Some("error") => Some(Err(anyhow::anyhow!("codex: {}", error_message(&msg)))),
            _ => None,
        };
        if let Some(events) = events {
            for event in out {
                let _ = events.send(event).await;
            }
        }
        end
    }
}

/// A persisted Codex thread resumed with `codex exec resume [OPTIONS] <thread_id>` for cached repair turns.
struct CodexSession {
    harness: Codex,
    task: WorkerTask,
    thread_id: String,
    cleaned: bool,
}

impl CodexSession {
    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        if let Some(root) = codex_home() {
            cleanup_codex_session(&root, &self.thread_id);
        }
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn codex_home() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(std::path::PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex"))
}

/// Remove persisted rollout JSONL files matching `thread_id` under `<codex_home>/sessions/**`.
fn cleanup_codex_session(codex_home: &std::path::Path, thread_id: &str) -> usize {
    if thread_id.len() < 8 || !thread_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return 0;
    }
    let sessions = codex_home.join("sessions");
    if !sessions.is_dir() {
        return 0;
    }
    let mut stack = vec![sessions];
    let mut removed = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.ends_with(".jsonl")
                && name.contains(thread_id)
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

#[async_trait]
impl crate::Session for CodexSession {
    async fn follow_up(
        &mut self,
        text: String,
        events: mpsc::Sender<HarnessEvent>,
        timeout: std::time::Duration,
    ) -> anyhow::Result<RunSummary> {
        let (summary, next_id) = self.harness.execute(&self.task, &text, true, Some(&self.thread_id), events, timeout).await?;
        if let Some(id) = next_id {
            self.thread_id = id;
        }
        Ok(summary)
    }

    fn session_id(&self) -> Option<&str> {
        Some(&self.thread_id)
    }

    fn detach(mut self: Box<Self>) -> Option<String> {
        // Leave the rollout JSONL intact on disk so a later repair or restart can resume it.
        self.cleaned = true;
        Some(self.thread_id.clone())
    }

    async fn close(mut self: Box<Self>) -> crate::Freed {
        self.cleanup();
        crate::Freed::default()
    }
}

#[async_trait]
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    /// Codex runs every command in its own `workspace-write` sandbox: writes stay in the
    /// working directory and the network is off unless its config turns it on.
    fn sandboxed_shell(&self) -> bool {
        true
    }

    async fn run_kept(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        let prompt = task.framed_brief();
        match self.execute(&task, &prompt, true, None, events, task.timeout).await {
            Ok((summary, Some(thread_id))) if summary.ok => {
                let kept: Box<dyn crate::Session> = Box::new(CodexSession { harness: self.clone(), task, thread_id, cleaned: false });
                (Ok(summary), Some(kept))
            }
            Ok((summary, Some(thread_id))) => {
                if let Some(root) = codex_home() {
                    cleanup_codex_session(&root, &thread_id);
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
                    Box::new(CodexSession { harness: self.clone(), task, thread_id: id, cleaned: false });
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
        // One-way like Claude Code: the read part gives the thread, and the end when the turn
        // finished before this daemon took over.
        let mut stream = Stream::default();
        let mut ended = None;
        for line in attached.outputs() {
            if let Some(end) = stream.line(line, None).await {
                ended = Some(end);
            }
        }
        let crate::child::Attached { mut io, proc, keep, .. } = attached;
        let result = match ended {
            Some(end) => end.map(|summary| (summary, stream.thread_id.clone())),
            None => match tokio::time::timeout(task.timeout, stream.read(&mut io, &events, task.stall)).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("codex timed out after {:?}", task.timeout)),
            },
        };
        drop(io);
        let freed = proc.release().await;
        match result {
            Ok((mut summary, thread_id)) => {
                summary.freed = Some(freed);
                let kept: Option<Box<dyn crate::Session>> = match thread_id {
                    Some(thread_id) if keep && summary.ok => Some(Box::new(CodexSession { harness: self.clone(), task, thread_id, cleaned: false })),
                    Some(thread_id) if keep => {
                        if let Some(root) = codex_home() {
                            cleanup_codex_session(&root, &thread_id);
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
        let prompt = task.framed_brief();
        self.execute(&task, &prompt, false, None, events, task.timeout).await.map(|(summary, _)| summary)
    }
}

fn error_message(error: &Value) -> String {
    error["message"].as_str().unwrap_or("unknown error").to_string()
}

fn item_events(item: &Value) -> Vec<HarnessEvent> {
    let text = || item["text"].as_str().unwrap_or_default().to_string();
    let id = item["id"].as_str().unwrap_or_default().to_string();
    match item["type"].as_str() {
        Some("agent_message") => vec![HarnessEvent::Text { text: text() }],
        Some("reasoning") => vec![HarnessEvent::Thought { text: text() }],
        Some("command_execution") => with_result(
            HarnessEvent::ToolCall { id: id.clone(), name: "shell".into(), input: item["command"].clone() },
            &id,
            item,
        ),
        Some("file_change") => with_result(
            HarnessEvent::ToolCall { id: id.clone(), name: "edit".into(), input: item["changes"].clone() },
            &id,
            item,
        ),
        Some("mcp_tool_call") => {
            let server = item["server"].as_str().unwrap_or_default();
            let tool = item["tool"].as_str().unwrap_or_default();
            with_result(
                HarnessEvent::ToolCall { id: id.clone(), name: format!("{server}__{tool}"), input: item["arguments"].clone() },
                &id,
                item,
            )
        }
        _ => Vec::new(),
    }
}

/// The call, then its output when Codex recorded `aggregated_output`, `output`, `result`, or a diff.
fn with_result(call: HarnessEvent, id: &str, item: &Value) -> Vec<HarnessEvent> {
    let mut events = vec![call];
    if let Some(result) = codex_result(id, item) {
        events.push(result);
    }
    events
}

fn codex_result(id: &str, item: &Value) -> Option<HarnessEvent> {
    let mut text = String::new();
    for key in ["aggregated_output", "output", "result"] {
        if let Some(value) = item.get(key) {
            if let Some(line) = value.as_str() {
                if !line.is_empty() {
                    text = line.to_string();
                    break;
                }
            } else if value.is_object() || value.is_array() {
                text = value.to_string();
                break;
            }
        }
    }
    let diff = item["diff"].as_str().filter(|d| !d.is_empty()).map(str::to_string).or_else(|| {
        let parts: Vec<&str> = item["changes"]
            .as_array()?
            .iter()
            .filter_map(|change| change["diff"].as_str())
            .filter(|diff| !diff.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    });
    if text.is_empty() && diff.is_none() {
        return None;
    }
    Some(HarnessEvent::ToolResult { id: id.to_string(), text: crate::clip_result(&text), diff: diff.map(|d| crate::clip_result(&d)) })
}

/// Codex counts cached tokens inside `input_tokens`.
fn read_usage(usage: &Value) -> Usage {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let cache_read = count("cached_input_tokens");
    Usage {
        input: count("input_tokens").saturating_sub(cache_read),
        output: count("output_tokens"),
        cache_read,
        cache_write: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    #[tokio::test]
    async fn a_codex_turn_handed_to_a_new_daemon_ends_with_its_usage_and_nothing_twice() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tm-codex-reattach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let go = dir.join("go");
        let message = |text: &str| format!(r#"echo '{{"type":"item.completed","item":{{"id":"i","type":"agent_message","text":"{text}"}}}}'"#);
        let script = format!(
            "#!/bin/sh\necho '{{\"type\":\"thread.started\",\"thread_id\":\"th-h3-0001\"}}'\n{}\nwhile [ ! -f '{}' ]; do sleep 0.05; done\n{}\necho '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":5,\"output_tokens\":6}}}}'\n",
            message("before"),
            go.display(),
            message("after")
        );
        let bin = dir.join("codex");
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let harness = Codex { binary: bin.display().to_string() };
        let holder = crate::hold::in_process(&dir.join("hold")).unwrap();
        let task = WorkerTask {
            brief: "build".into(),
            cwd: dir.clone(),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(20),
            stall: None,
            read_only: false,
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: Some(crate::hold::HoldSpec { holder: holder.clone(), key: "p/r/page:c".into(), meta: json!({}) }),
        };
        let gate: Arc<dyn PermissionGate> = Arc::new(crate::gate::WorktreeGate::new(&dir, vec![]));
        let (tx, mut rx) = mpsc::channel(32);
        let running = tokio::spawn({
            let (harness, gate, task) = (harness.clone(), gate.clone(), task.clone());
            async move { harness.run(task, gate, tx).await }
        });
        assert_eq!(rx.recv().await, Some(HarnessEvent::SessionStarted { session_id: "th-h3-0001".into() }));
        assert_eq!(rx.recv().await, Some(HarnessEvent::Text { text: "before".into() }));
        assert_eq!(holder.hand_off(std::time::Duration::from_secs(60), std::time::Duration::from_millis(50)).await.unwrap(), 1);
        running.abort();
        let _ = running.await;

        let spec = crate::hold::HoldSpec { holder: crate::hold::Holder::new(holder.socket(), None), key: "p/r/page:c".into(), meta: json!({}) };
        let attached = crate::child::attach(&spec, false).await.unwrap();
        std::fs::write(&go, "").unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let (summary, kept) = harness.reattach(task, gate, tx, attached).await;
        let summary = summary.unwrap();
        assert!(summary.ok && kept.is_none());
        assert_eq!(summary.usage.total(), 11);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert_eq!(events, vec![HarnessEvent::Text { text: "after".into() }]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maps_items_to_events() {
        let message = json!({ "id": "item_1", "type": "agent_message", "text": "done" });
        assert_eq!(item_events(&message), vec![HarnessEvent::Text { text: "done".into() }]);
        let call = json!({ "id": "item_2", "type": "mcp_tool_call", "server": "dict", "tool": "cite",
                           "arguments": { "key": "k" } });
        assert!(matches!(&item_events(&call)[..], [HarnessEvent::ToolCall { name, .. }] if name == "dict__cite"));
        assert!(item_events(&json!({ "type": "todo_list" })).is_empty());
        let shell = json!({
            "id": "item_3", "type": "command_execution", "command": "ls",
            "aggregated_output": "Cargo.toml\n", "exit_code": 0
        });
        let events = item_events(&shell);
        assert!(matches!(&events[0], HarnessEvent::ToolCall { name, .. } if name == "shell"));
        assert!(matches!(&events[1], HarnessEvent::ToolResult { text, diff: None, .. } if text == "Cargo.toml\n"));
        let edit = json!({
            "id": "item_4", "type": "file_change",
            "changes": [{ "path": "a.ts", "diff": "--- a/a.ts\n+++ b/a.ts\n+x\n" }]
        });
        let events = item_events(&edit);
        assert!(matches!(&events[1], HarnessEvent::ToolResult { diff: Some(diff), text, .. } if text.is_empty() && diff.contains("+x")));
        assert!(!events.iter().any(|event| matches!(event, HarnessEvent::Thought { .. })));
    }

    #[test]
    fn cached_tokens_are_not_counted_twice() {
        let usage = read_usage(&json!({ "input_tokens": 1000, "cached_input_tokens": 800, "output_tokens": 50 }));
        assert_eq!((usage.input, usage.cache_read, usage.total()), (200, 800, 1050));
    }

    #[test]
    fn reads_the_failure_recorded_from_codex() {
        // Recorded from Codex 0.169 when the account was over its usage limit.
        let failed = json!({ "type": "turn.failed", "error": { "message": "You've hit your usage limit." } });
        assert_eq!(error_message(&failed["error"]), "You've hit your usage limit.");
    }

    #[test]
    fn warm_session_omits_ephemeral_and_resumes_by_thread_id() {
        let harness = Codex::default();
        let task = WorkerTask {
            brief: "build".into(),
            cwd: "/w".into(),
            model: Some("gpt-5".into()),
            effort: Some("medium".into()),
            dictionary: None,
            timeout: std::time::Duration::from_secs(10),
            stall: None,
            read_only: false,
            code_mode: true,
            shell: None,
            permission_mode: None,
            hold: None,
        };
        let stateless = harness.args_with(&task, "initial", false, None);
        assert!(stateless.contains(&"--ephemeral".to_string()));
        assert_eq!(&stateless[..1], ["exec"]);

        let kept = harness.args_with(&task, "initial", true, None);
        assert!(!kept.contains(&"--ephemeral".to_string()));

        let resumed = harness.args_with(&task, "fix it", true, Some("thr_123"));
        assert!(!resumed.contains(&"--ephemeral".to_string()));
        assert_eq!(&resumed[..2], ["exec", "resume"]);
        // Codex CLI expects `codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]`.
        let tail = &resumed[resumed.len() - 2..];
        assert_eq!(tail, ["thr_123", "fix it"]);
    }

    #[test]
    fn code_mode_enforces_run_code_when_dictionary_is_attached() {
        let harness = Codex::default();
        let task = WorkerTask {
            brief: "build".into(),
            cwd: "/w".into(),
            model: Some("gpt-5".into()),
            effort: None,
            dictionary: Some(crate::DictionaryAccess {
                name: "taskmaster".into(),
                url: "http://127.0.0.1:9/mcp".into(),
                bearer_token: "tok".into(),
            }),
            timeout: std::time::Duration::from_secs(10),
            stall: None,
            read_only: false,
            code_mode: true,
            shell: None,
            permission_mode: None,
            hold: None,
        };
        let args = harness.args_with(&task, "build", false, None);
        assert!(args.windows(2).any(|w| w == ["--disable", "shell_tool"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]), "{args:?}");

        // Full access opens Codex's own sandbox for a builder with its own tools, and only then.
        let mut own_tools = task.clone();
        own_tools.code_mode = false;
        let sandbox = |t: &WorkerTask| {
            let args = harness.args_with(t, "build", false, None);
            args.windows(2).find(|w| w[0] == "--sandbox").map(|w| w[1].clone()).unwrap_or_default()
        };
        assert_eq!(sandbox(&own_tools), "workspace-write");
        own_tools.permission_mode = Some(crate::PermissionMode::FullAccess);
        assert_eq!(sandbox(&own_tools), "danger-full-access");
        let mut reader = own_tools.clone();
        reader.read_only = true;
        assert_eq!(sandbox(&reader), "read-only");
    }

    #[test]
    fn cleanup_codex_session_removes_matching_jsonl_only() {
        let root = std::env::temp_dir().join(format!("tm-codex-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("sessions/2026/11/22");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("rollout-2026-11-22-thr_cleanup_99.jsonl");
        let other = dir.join("rollout-2026-11-22-thr_other_11.jsonl");
        std::fs::write(&target, "{}\n").unwrap();
        std::fs::write(&other, "{}\n").unwrap();

        assert_eq!(cleanup_codex_session(&root, "thr_cleanup_99"), 1);
        assert!(!target.exists());
        assert!(other.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_codex_stream_is_stopped_as_stalled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tm-codex-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("codex-hang");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let harness = Codex { binary: bin.display().to_string() };
        let task = WorkerTask {
            brief: "build".into(),
            cwd: dir.clone(),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(30),
            stall: Some(std::time::Duration::from_millis(120)),
            read_only: false,
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: None,
        };
        let gate: Arc<dyn PermissionGate> = Arc::new(crate::gate::WorktreeGate::new(&dir, vec![]));
        let (tx, _rx) = mpsc::channel(8);
        let err = harness.run(task, gate, tx).await.unwrap_err();
        assert!(
            err.downcast_ref::<crate::Stalled>().is_some(),
            "expected Stalled error on silent Codex stream, got: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_attaches_dictionary_mcp_in_code_mode_off_and_on() {
        let harness = Codex::default();
        for (read_only, code_mode) in [(false, false), (false, true), (true, false), (true, true)] {
            let task = WorkerTask {
                brief: "build".into(),
                cwd: "/w".into(),
                model: None,
                effort: None,
                dictionary: Some(crate::DictionaryAccess {
                    name: "dictionary".into(),
                    url: "http://127.0.0.1:43210/mcp".into(),
                    bearer_token: "tok-codex".into(),
                }),
                timeout: std::time::Duration::from_secs(10),
                stall: None,
                read_only,
                code_mode,
                shell: None,
                permission_mode: None,
                hold: None,
            };
            let args = harness.args_with(&task, "build", false, None);
            assert!(
                args.iter().any(|a| a == r#"mcp_servers.dictionary.url="http://127.0.0.1:43210/mcp""#),
                "missing dictionary MCP url for read_only={read_only}, code_mode={code_mode}: {args:?}"
            );
            assert!(
                args.iter().any(|a| a == r#"mcp_servers.dictionary.bearer_token_env_var="TASKMASTER_DICTIONARY_TOKEN""#),
                "missing dictionary bearer env var for read_only={read_only}, code_mode={code_mode}: {args:?}"
            );
        }
    }
}
