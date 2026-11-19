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

use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, bail};
use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::{Harness, HarnessEvent, PermissionGate, RunSummary, Usage, WorkerTask};

/// The Dictionary token reaches Codex through this variable, never the command line.
const TOKEN_ENV: &str = "TASKMASTER_DICTIONARY_TOKEN";

pub struct Codex {
    pub binary: String,
}

impl Default for Codex {
    fn default() -> Self {
        Codex { binary: "codex".into() }
    }
}

#[async_trait]
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    async fn run(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(["exec", "--json", "--ephemeral", "--skip-git-repo-check"])
            .arg("-C")
            .arg(&task.cwd)
            .args(["--sandbox", if task.read_only { "read-only" } else { "workspace-write" }]);
        if let Some(model) = &task.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &task.effort {
            cmd.args(["-c", &format!("model_reasoning_effort={effort:?}")]);
        }
        if let Some(dict) = &task.dictionary {
            // TOML string values; `{:?}` quotes and escapes them.
            cmd.arg("-c").arg(format!("mcp_servers.{}.url={:?}", dict.name, dict.url));
            cmd.arg("-c")
                .arg(format!("mcp_servers.{}.bearer_token_env_var={TOKEN_ENV:?}", dict.name));
            cmd.env(TOKEN_ENV, &dict.bearer_token);
        }
        let (mut child, group) = crate::spawn_grouped(
            cmd.arg(task.framed_brief()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()),
        )
        .with_context(|| format!("failed to start `{}`", self.binary))?;
        let stdout = child.stdout.take().context("codex stdout was not captured")?;

        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match msg["type"].as_str() {
                    Some("item.completed") => {
                        for event in item_events(&msg["item"]) {
                            let _ = events.send(event).await;
                        }
                    }
                    Some("turn.completed") => {
                        let usage = read_usage(&msg["usage"]);
                        return Ok(RunSummary { ok: true, usage, cost_usd: None, denied: 0, freed: None });
                    }
                    Some("turn.failed") => bail!("codex: {}", error_message(&msg["error"])),
                    Some("error") => bail!("codex: {}", error_message(&msg)),
                    _ => {}
                }
            }
            bail!("codex exited without completing its turn")
        };
        let result = match tokio::time::timeout(task.timeout, read).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("codex timed out after {:?}", task.timeout)),
        };
        crate::released(result, group.release(&mut child).await)
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
}
