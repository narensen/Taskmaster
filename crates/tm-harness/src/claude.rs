//! Claude Code adapter: `claude -p --output-format stream-json`.
//!
//! The stream is one-way, so confinement is set by launch flags instead of the gate: the
//! shell tool is removed, edits are auto-accepted only inside the working directory, and
//! any other call that would need approval is refused because nobody is there to approve.

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
                _ => None,
            }
        })
        .collect()
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
