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
                        if let Some(event) = item_event(&msg["item"]) {
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

fn item_event(item: &Value) -> Option<HarnessEvent> {
    let text = || item["text"].as_str().unwrap_or_default().to_string();
    let id = item["id"].as_str().unwrap_or_default().to_string();
    match item["type"].as_str()? {
        "agent_message" => Some(HarnessEvent::Text { text: text() }),
        "reasoning" => Some(HarnessEvent::Thought { text: text() }),
        "command_execution" => {
            Some(HarnessEvent::ToolCall { id, name: "shell".into(), input: item["command"].clone() })
        }
        "file_change" => {
            Some(HarnessEvent::ToolCall { id, name: "edit".into(), input: item["changes"].clone() })
        }
        "mcp_tool_call" => {
            let server = item["server"].as_str().unwrap_or_default();
            let tool = item["tool"].as_str().unwrap_or_default();
            Some(HarnessEvent::ToolCall { id, name: format!("{server}__{tool}"), input: item["arguments"].clone() })
        }
        _ => None,
    }
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
        assert_eq!(item_event(&message), Some(HarnessEvent::Text { text: "done".into() }));
        let call = json!({ "id": "item_2", "type": "mcp_tool_call", "server": "dict", "tool": "cite",
                           "arguments": { "key": "k" } });
        assert!(matches!(item_event(&call), Some(HarnessEvent::ToolCall { ref name, .. }) if name == "dict__cite"));
        assert_eq!(item_event(&json!({ "type": "todo_list" })), None);
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
