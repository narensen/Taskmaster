//! Agent Client Protocol adapter, used for Antigravity (`agy acp`).
//!
//! ACP is JSON-RPC over the agent's stdio. The session is created with its working
//! directory and MCP servers, so nothing is written to the user's global configuration.
//! The agent sends `session/request_permission` before shell commands and MCP calls; each
//! one is answered by the [`PermissionGate`]. File access is confined by the [`Sandbox`].

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::gate::{Decision, PermissionGate, PermissionRequest, ToolKind};
use crate::sandbox::{Sandbox, resolve_binary};
use crate::{Harness, HarnessEvent, RunSummary, Usage, WorkerTask};

pub struct AcpHarness {
    pub id: &'static str,
    pub binary: String,
    pub args: Vec<String>,
    /// Required for agents that touch files without asking. `None` runs the agent unconfined.
    pub sandbox: Option<Sandbox>,
}

impl AcpHarness {
    pub fn antigravity() -> anyhow::Result<Self> {
        Ok(AcpHarness {
            id: "antigravity",
            binary: "agy".into(),
            args: vec!["acp".into()],
            sandbox: Some(Sandbox::antigravity()?),
        })
    }

    fn command(&self, task: &WorkerTask) -> anyhow::Result<Command> {
        let Some(sandbox) = &self.sandbox else {
            let mut cmd = Command::new(&self.binary);
            cmd.args(&self.args).current_dir(&task.cwd);
            return Ok(cmd);
        };
        let binary = resolve_binary(&self.binary)?;
        let mut cmd = Command::new("bwrap");
        cmd.args(sandbox.bwrap_args(&binary, &self.args, &task.cwd, task.read_only));
        Ok(cmd)
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn run(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let mut child = self
            .command(&task)?
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start `{}` (is bubblewrap installed?)", self.binary))?;
        let mut session = Session {
            stdin: child.stdin.take().context("agent stdin was not captured")?,
            lines: BufReader::new(child.stdout.take().context("agent stdout was not captured")?)
                .lines(),
            next_id: 0,
            gate,
            events,
            denied: 0,
        };
        let timeout = task.timeout;
        match tokio::time::timeout(timeout, session.drive(task)).await {
            Ok(result) => result,
            Err(_) => bail!("{} timed out after {timeout:?}", self.id),
        }
        // `child` is dropped here, which kills the agent process.
    }
}

struct Session {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    gate: Arc<dyn PermissionGate>,
    events: mpsc::Sender<HarnessEvent>,
    denied: u32,
}

impl Session {
    async fn drive(&mut self, task: WorkerTask) -> anyhow::Result<RunSummary> {
        let init = json!({
            "protocolVersion": 1,
            // The agent uses its own file tools; the gate confines them.
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false
            },
            "clientInfo": { "name": "taskmaster", "version": env!("CARGO_PKG_VERSION") },
        });
        self.call("initialize", init).await?;

        let mcp_servers: Vec<Value> = task
            .dictionary
            .iter()
            .map(|d| {
                let auth = format!("Bearer {}", d.bearer_token);
                json!({
                    "type": "http",
                    "name": d.name,
                    "url": d.url,
                    "headers": [{ "name": "Authorization", "value": auth }],
                })
            })
            .collect();
        let new = json!({ "cwd": task.cwd, "mcpServers": mcp_servers });
        let session = self.call("session/new", new).await?;
        let session_id =
            session["sessionId"].as_str().context("session/new returned no sessionId")?.to_string();

        if let Some(model) = &task.model {
            let params = json!({ "sessionId": session_id, "modelId": model });
            self.call("session/set_model", params)
                .await
                .with_context(|| format!("could not select model `{model}`"))?;
        }

        let prompt = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": task.framed_brief() }],
        });
        let result = self.call("session/prompt", prompt).await?;
        let usage = &result["usage"];
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        Ok(RunSummary {
            ok: result["stopReason"] == "end_turn",
            usage: Usage {
                input: count("inputTokens"),
                output: count("outputTokens"),
                cache_read: count("cachedReadTokens"),
                cache_write: count("cachedWriteTokens"),
            },
            cost_usd: None,
            denied: self.denied,
        })
    }

    /// Send a request and wait for its response, serving whatever the agent sends meanwhile.
    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        loop {
            let line = self
                .lines
                .next_line()
                .await?
                .ok_or_else(|| anyhow!("agent closed its output during `{method}`"))?;
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg.get("method").is_some() {
                self.incoming(msg).await?;
            } else if msg["id"] == id {
                if let Some(error) = msg.get("error") {
                    bail!("`{method}` failed: {error}");
                }
                return Ok(msg["result"].clone());
            }
        }
    }

    async fn incoming(&mut self, msg: Value) -> anyhow::Result<()> {
        let params = &msg["params"];
        match msg["method"].as_str() {
            Some("session/update") => {
                if let Some(event) = update_event(&params["update"]) {
                    let _ = self.events.send(event).await;
                }
            }
            Some("session/request_permission") => {
                let request = permission_request(&params["toolCall"]);
                let allowed = self.gate.decide(&request) == Decision::Allow;
                if !allowed {
                    self.denied += 1;
                }
                let _ = self
                    .events
                    .send(HarnessEvent::PermissionDecided { tool: request.tool, allowed })
                    .await;
                let outcome = match choose_option(&params["options"], allowed) {
                    Some(option) => json!({ "outcome": "selected", "optionId": option }),
                    // No matching option offered: cancelling is the safe answer either way.
                    None => json!({ "outcome": "cancelled" }),
                };
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": msg["id"],
                    "result": { "outcome": outcome },
                });
                self.send(reply).await?;
            }
            _ => {
                // An unknown request still needs a reply, or the agent waits forever.
                if msg.get("id").is_some() {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": msg["id"],
                        "error": { "code": -32601, "message": "method not supported" },
                    });
                    self.send(reply).await?;
                }
            }
        }
        Ok(())
    }

    async fn send(&mut self, msg: Value) -> anyhow::Result<()> {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

fn update_event(update: &Value) -> Option<HarnessEvent> {
    let text = || update["content"]["text"].as_str().unwrap_or_default().to_string();
    match update["sessionUpdate"].as_str()? {
        "agent_message_chunk" => Some(HarnessEvent::Text { text: text() }),
        "agent_thought_chunk" => Some(HarnessEvent::Thought { text: text() }),
        "tool_call" => Some(HarnessEvent::ToolCall {
            id: update["toolCallId"].as_str().unwrap_or_default().to_string(),
            name: tool_name(update),
            input: update["rawInput"].clone(),
        }),
        _ => None,
    }
}

fn tool_name(tool_call: &Value) -> String {
    let name = tool_call["name"].as_str().or_else(|| tool_call["title"].as_str());
    name.unwrap_or_default().to_string()
}

fn permission_request(tool_call: &Value) -> PermissionRequest {
    let kind = match tool_call["kind"].as_str() {
        Some("read" | "search") => ToolKind::Read,
        Some("edit" | "delete" | "move") => ToolKind::Edit,
        Some("execute") => ToolKind::Execute,
        _ => ToolKind::Other,
    };
    let locations = tool_call["locations"].as_array().map(Vec::as_slice).unwrap_or_default();
    PermissionRequest {
        tool: tool_name(tool_call),
        kind,
        paths: locations.iter().filter_map(|l| l["path"].as_str()).map(PathBuf::from).collect(),
        command: tool_call["rawInput"]["CommandLine"].as_str().map(str::to_string),
    }
}

/// Pick the one-off allow or reject option; never a standing "always" grant.
fn choose_option(options: &Value, allowed: bool) -> Option<String> {
    let wanted = if allowed { "allow_once" } else { "reject_once" };
    let options = options.as_array()?;
    let option = options.iter().find(|o| o["kind"] == wanted)?;
    option["optionId"].as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shapes recorded from Antigravity 1.5.9.
    fn shell_call() -> Value {
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "call_1", "name": "run_command",
            "status": "pending", "kind": "execute", "title": "ls -la /tmp",
            "rawInput": { "CommandLine": "ls -la /tmp", "WaitMsBeforeAsync": 5000 }
        })
    }

    #[test]
    fn reads_a_shell_permission_request() {
        let request = permission_request(&shell_call());
        assert_eq!(request.tool, "run_command");
        assert_eq!(request.kind, ToolKind::Execute);
        assert_eq!(request.command.as_deref(), Some("ls -la /tmp"));
        assert!(request.paths.is_empty());
    }

    #[test]
    fn reads_file_locations_from_an_edit() {
        let call = json!({
            "name": "write_file", "kind": "edit",
            "locations": [{ "path": "/work/a/greeting.ts" }]
        });
        let request = permission_request(&call);
        assert_eq!(request.kind, ToolKind::Edit);
        assert_eq!(request.paths, vec![PathBuf::from("/work/a/greeting.ts")]);
    }

    #[test]
    fn chooses_one_off_options_only() {
        let options = json!([
            { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
            { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "deny", "name": "Deny", "kind": "reject_once" }
        ]);
        assert_eq!(choose_option(&options, true).as_deref(), Some("allow"));
        assert_eq!(choose_option(&options, false).as_deref(), Some("deny"));
        assert_eq!(choose_option(&json!([]), true), None);
    }

    #[test]
    fn maps_session_updates_to_events() {
        let chunk = json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "hi" }
        });
        assert_eq!(update_event(&chunk), Some(HarnessEvent::Text { text: "hi".into() }));
        let call = update_event(&shell_call()).unwrap();
        assert!(matches!(call, HarnessEvent::ToolCall { ref name, .. } if name == "run_command"));
        assert_eq!(update_event(&json!({ "sessionUpdate": "usage_update", "used": 0 })), None);
    }
}
