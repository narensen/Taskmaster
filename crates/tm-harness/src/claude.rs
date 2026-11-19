//! Claude Code adapter: `claude -p --output-format stream-json`.
//!
//! The stream is one-way, so confinement is set by launch flags instead of the gate: the
//! shell tool is removed, edits are auto-accepted only inside the working directory, and
//! any other call that would need approval is refused because nobody is there to approve.
//!
//! With `code_mode`, the task runs in an interactive session instead, because Claude
//! Code's own code mode (`CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1`) is not offered with `-p`.
//! The session runs on a terminal ([`crate::pty`]) with the same flags. A Stop hook,
//! given in session-only settings, says when the turn is over, and the session's own
//! transcript (`~/.claude/projects/<dir>/<session>.jsonl`) gives the text, the tool calls
//! and the usage of each message.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Harness, HarnessEvent, PermissionGate, RunSummary, Usage, WorkerTask};

pub struct ClaudeCode {
    pub binary: String,
}

impl Default for ClaudeCode {
    fn default() -> Self {
        ClaudeCode { binary: "claude".into() }
    }
}

#[async_trait]
impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    async fn run(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        if task.code_mode && !task.read_only {
            return self.run_code_mode(task, events).await;
        }
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&task.cwd)
            .arg("-p")
            .arg(task.framed_brief())
            .args(["--output-format", "stream-json", "--verbose"])
            .args(["--strict-mcp-config", "--no-session-persistence"]);
        if task.read_only {
            // With no edit tools and nobody to approve a prompt, the session can only read.
            cmd.args(["--permission-mode", "default"])
                .args(["--disallowedTools", "Bash", "Edit", "Write", "NotebookEdit"]);
        } else {
            cmd.args(["--permission-mode", "acceptEdits"]).args(["--disallowedTools", "Bash"]);
        }
        if let Some(model) = &task.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &task.effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(dict) = &task.dictionary {
            let config = json!({ "mcpServers": { &dict.name: {
                "type": "http",
                "url": dict.url,
                "headers": { "Authorization": format!("Bearer {}", dict.bearer_token) },
            }}});
            cmd.args(["--mcp-config", &config.to_string()]);
            cmd.args(["--allowedTools", &format!("mcp__{}", dict.name)]);
        }
        let (mut child, group) = crate::spawn_grouped(
            cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()),
        )
        .with_context(|| format!("failed to start `{}`", self.binary))?;
        let stdout = child.stdout.take().context("claude stdout was not captured")?;

        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match msg["type"].as_str() {
                    Some("assistant") => {
                        for event in content_events(&msg["message"]["content"]) {
                            let _ = events.send(event).await;
                        }
                    }
                    Some("user") => {
                        for event in tool_results(&msg["message"]["content"]) {
                            let _ = events.send(event).await;
                        }
                    }
                    Some("result") => return Ok(summary(&msg)),
                    _ => {}
                }
            }
            bail!("claude exited without a result event")
        };
        let result = match tokio::time::timeout(task.timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("claude timed out after {:?}", task.timeout)),
        };
        crate::released(result, group.release(&mut child).await)
    }
}

impl ClaudeCode {
    /// The flags both modes share: permissions, model, effort and the Dictionary.
    fn common_args(task: &WorkerTask) -> Vec<String> {
        let mut args: Vec<String> = vec!["--strict-mcp-config".into(), "--permission-mode".into(), "acceptEdits".into(), "--disallowedTools".into(), "Bash".into()];
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
            args.extend(["--mcp-config".into(), config.to_string(), "--allowedTools".into(), format!("mcp__{}", dict.name)]);
        }
        args
    }

    async fn run_code_mode(&self, task: WorkerTask, events: mpsc::Sender<HarnessEvent>) -> anyhow::Result<RunSummary> {
        let session = session_id()?;
        let scratch = std::env::temp_dir().join(format!("tm-claude-{session}"));
        std::fs::create_dir_all(&scratch)?;
        // The hook's input names the session's transcript; it is kept as the signal.
        let stopped = scratch.join("stop.json");
        let settings = json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": format!("cat > {}", shell_quote(&stopped.display().to_string())) }] }] } });
        let mut args = Self::common_args(&task);
        args.extend(["--session-id".into(), session.clone(), "--settings".into(), settings.to_string()]);
        args.push(task.framed_brief());
        let program = crate::sandbox::resolve_binary(&self.binary).with_context(|| format!("`{}` is not installed", self.binary))?;
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let env = [("CLAUDE_CODE_EXPERIMENTAL_CODE_MODE", "1"), ("TERM", "xterm-256color"), ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")];
        let mut child = crate::pty::spawn(&program, &arg_refs, &task.cwd, env, 50, 200)?;
        let mut transcript = transcript_path(&task.cwd, &session);
        // A worktree is new to Claude Code: the folder it makes for it is removed after.
        let made_folder = transcript.as_ref().and_then(|t| t.parent()).filter(|dir| !dir.exists()).map(std::path::Path::to_path_buf);
        let mut screen = crate::screen::Screen::new(50, 200);
        let mut follow = Transcript::default();
        let mut answered: Option<tokio::time::Instant> = None;
        let deadline = tokio::time::Instant::now() + task.timeout;
        let finished = loop {
            if tokio::time::Instant::now() > deadline {
                break false;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(250), child.output.recv()).await {
                Ok(Some(bytes)) => screen.feed(&bytes),
                Ok(None) => break stop_transcript(&stopped).is_some(),
                Err(_) => {}
            }
            // First-run screens (the theme, trusting a new folder, a notice) take their
            // default, the first option. At most once every two seconds, so one is not
            // answered twice while it redraws.
            let shown = screen.tail(24);
            if let Some(keys) = setup_answer(&shown)
                && answered.is_none_or(|at| at.elapsed() > std::time::Duration::from_secs(2))
            {
                answered = Some(tokio::time::Instant::now());
                let _ = child.writer.write(keys);
            }
            for event in follow.read(transcript.as_deref()) {
                let _ = events.send(event).await;
            }
            if let Some(path) = stop_transcript(&stopped) {
                transcript = Some(path);
                break true;
            }
        };
        // Leave the session the way a person would, so it writes out its transcript, then
        // read what it wrote after the hook fired.
        let _ = child.writer.write(b"/exit\r");
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while child.output.recv().await.is_some() {}
        })
        .await;
        for event in follow.read(transcript.as_deref()) {
            let _ = events.send(event).await;
        }
        let freed = child.guard.release(&mut child.child).await;
        let _ = std::fs::remove_dir_all(&scratch);
        if let Some(dir) = made_folder {
            let _ = std::fs::remove_dir_all(dir);
        }
        if !finished {
            bail!("claude did not finish its turn within {:?}: {}", task.timeout, screen.tail(30));
        }
        Ok(RunSummary { ok: true, usage: follow.usage(), cost_usd: None, denied: 0, freed: Some(freed) })
    }
}

/// Reads a session transcript as it grows: each assistant message once, by its id, with
/// the usage of its last line (the totals for that message).
#[derive(Default)]
pub struct Transcript {
    offset: u64,
    seen: std::collections::HashSet<String>,
    usage: std::collections::HashMap<String, Usage>,
    partial: String,
}

impl Transcript {
    pub fn read(&mut self, path: Option<&std::path::Path>) -> Vec<HarnessEvent> {
        use std::io::{Read, Seek, SeekFrom};
        let Some(path) = path else { return Vec::new() };
        let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut text = String::new();
        let Ok(n) = file.read_to_string(&mut text) else { return Vec::new() };
        self.offset += n as u64;
        self.partial.push_str(&text);
        let mut out = Vec::new();
        while let Some(end) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=end).collect();
            out.extend(self.line(line.trim()));
        }
        out
    }

    pub fn line(&mut self, line: &str) -> Vec<HarnessEvent> {
        let Ok(value) = serde_json::from_str::<Value>(line) else { return Vec::new() };
        if value["type"] == "user" {
            return tool_results(&value["message"]["content"]);
        }
        if value["type"] != "assistant" {
            return Vec::new();
        }
        let message = &value["message"];
        let id = message["id"].as_str().unwrap_or("").to_string();
        let usage = &message["usage"];
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        self.usage.insert(
            id.clone(),
            Usage { input: count("input_tokens"), output: count("output_tokens"), cache_read: count("cache_read_input_tokens"), cache_write: count("cache_creation_input_tokens") },
        );
        // One line per content block: dedupe by message and block.
        let key = format!("{id}:{}", serde_json::to_string(&message["content"]).unwrap_or_default());
        if !self.seen.insert(key) {
            return Vec::new();
        }
        content_events(&message["content"])
    }

    pub fn usage(&self) -> Usage {
        self.usage.values().fold(Usage::default(), |sum, u| Usage {
            input: sum.input + u.input,
            output: sum.output + u.output,
            cache_read: sum.cache_read + u.cache_read,
            cache_write: sum.cache_write + u.cache_write,
        })
    }
}

/// The transcript the Stop hook named, once the hook has run.
fn stop_transcript(file: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let value: Value = serde_json::from_str(text.trim()).ok()?;
    value["transcript_path"].as_str().map(std::path::PathBuf::from)
}

/// Claude Code keeps a session under `~/.claude/projects/` in a folder named after the
/// working directory, with every character but letters and digits turned into `-`.
fn transcript_path(cwd: &std::path::Path, session: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".claude")))?;
    Some(transcript_in(&home, cwd, session))
}

fn transcript_in(home: &std::path::Path, cwd: &std::path::Path, session: &str) -> std::path::PathBuf {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let dir: String = cwd.display().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    home.join("projects").join(dir).join(format!("{session}.jsonl"))
}

/// The keys that get past a screen Claude Code shows before the session starts, or
/// `None` when this is not one. A Taskmaster worktree is trusted: the folder question's
/// cursor starts on "No, exit", so it moves down to "Yes" first.
fn setup_answer(text: &str) -> Option<&'static [u8]> {
    let lower = text.to_ascii_lowercase();
    let trust = lower.contains("trust this folder") || (lower.contains("trust") && lower.contains("folder") && lower.contains("yes"));
    if trust {
        let cursor_on_no = text.lines().any(|line| line.trim_start().starts_with('❯') && line.to_ascii_lowercase().contains("no"));
        return Some(if cursor_on_no { b"\x1b[B\r" } else { b"\r" });
    }
    let theme = lower.contains("syntax theme") || lower.contains("choose the text style");
    let notice = lower.contains("press enter to continue");
    (theme || notice).then_some(b"\r")
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A random version-4 UUID, which `--session-id` requires.
fn session_id() -> anyhow::Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]))
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_gives_each_message_once_and_sums_its_usage() {
        let mut t = Transcript::default();
        let line = |id: &str, content: Value, out: u64| {
            json!({ "type": "assistant", "message": { "id": id, "content": content, "usage": {
                "input_tokens": 10, "output_tokens": out, "cache_read_input_tokens": 500, "cache_creation_input_tokens": 40 } } })
            .to_string()
        };
        let mut events = t.line(&line("m1", json!([{ "type": "text", "text": "Reading the brief." }]), 5));
        // The same block written again (a later line of the same message) is not repeated.
        events.extend(t.line(&line("m1", json!([{ "type": "text", "text": "Reading the brief." }]), 9)));
        events.extend(t.line(&line("m1", json!([{ "type": "tool_use", "id": "t1", "name": "Write", "input": { "file_path": "a" } }]), 30)));
        events.extend(t.line(&line("m2", json!([{ "type": "text", "text": "Done." }]), 3)));
        events.extend(t.line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"wrote a"},{"type":"text","text":"not a result"}]}}"#,
        ));
        events.extend(t.line(r#"{"type":"user","message":{"content":"x"}}"#));
        assert_eq!(events.len(), 4);
        assert!(matches!(&events[1], HarnessEvent::ToolCall { name, .. } if name == "Write"));
        assert!(matches!(&events[3], HarnessEvent::ToolResult { id, text, diff: None } if id == "t1" && text == "wrote a"));
        // m1's last line carries its totals; m2 adds its own.
        assert_eq!(t.usage(), Usage { input: 20, output: 33, cache_read: 1000, cache_write: 80 });
    }

    #[test]
    fn the_session_folder_and_the_trust_prompt_are_recognised() {
        let path = transcript_in(std::path::Path::new("/nonexistent/claude"), std::path::Path::new("/nonexistent/tm.x/work-a"), "abc");
        assert_eq!(path, std::path::PathBuf::from("/nonexistent/claude/projects/-nonexistent-tm-x-work-a/abc.jsonl"));
        let trust = " Quick safety check: Is this a project you created or one you trust?\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm · Esc to cancel";
        assert_eq!(setup_answer(trust), Some(&b"\x1b[B\r"[..]));
        assert_eq!(setup_answer(&trust.replace("❯ No", "  No").replace("   Yes", " ❯ Yes")), Some(&b"\r"[..]));
        assert_eq!(setup_answer("Choose the text style that looks best with your terminal\n Syntax theme: Monokai Extended"), Some(&b"\r"[..]));
        assert_eq!(setup_answer("Writing app/home/page.tsx"), None);
        let id = session_id().unwrap();
        assert_eq!((id.len(), &id[14..15]), (36, "4"));
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
}
