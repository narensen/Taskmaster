//! Agent Client Protocol adapter, used for Antigravity, Grok Build and Opencode.
//!
//! ACP is JSON-RPC over the agent's stdio. The session is created with its working
//! directory and MCP servers, so nothing is written to the user's global configuration.
//! The agent sends `session/request_permission` before shell commands and MCP calls; each
//! one is answered by the [`PermissionGate`]. File access is confined by the [`Sandbox`].

use std::collections::BTreeSet;
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
use crate::{Harness, HarnessEvent, RunSummary, Stalled, Usage, WorkerTask};

pub struct AcpHarness {
    pub id: &'static str,
    pub binary: String,
    pub args: Vec<String>,
    /// Required for agents that touch files without asking. `None` runs the agent unconfined.
    pub sandbox: Option<Sandbox>,
    /// How this harness names an MCP server's tools, with `{server}` for the server name.
    /// A tool whose name starts with this, for the Dictionary server, is a Dictionary call.
    pub mcp_tool_prefix: &'static str,
    /// The flag that sets reasoning effort, inserted after the first argument. `None` ignores effort.
    pub effort_flag: Option<&'static str>,
}

impl AcpHarness {
    pub fn antigravity() -> anyhow::Result<Self> {
        Ok(AcpHarness {
            id: "antigravity",
            binary: "agy".into(),
            args: vec!["acp".into()],
            sandbox: Some(Sandbox::antigravity()?),
            mcp_tool_prefix: "mcp_{server}_",
            effort_flag: None,
        })
    }

    /// Grok Build (`grok agent stdio`).
    pub fn grok() -> anyhow::Result<Self> {
        Ok(AcpHarness {
            id: "grok-build",
            binary: "grok".into(),
            args: vec!["agent".into(), "stdio".into()],
            sandbox: Some(Sandbox::grok()?),
            mcp_tool_prefix: "{server}__",
            // `grok agent --reasoning-effort low stdio`
            effort_flag: Some("--reasoning-effort"),
        })
    }

    /// Opencode (`opencode acp`). It does not ask before file edits, so the sandbox is what
    /// confines it, as with Antigravity. Its model is a session option, not `set_model`.
    pub fn opencode() -> anyhow::Result<Self> {
        Ok(AcpHarness {
            id: "opencode",
            binary: "opencode".into(),
            args: vec!["acp".into()],
            sandbox: Some(Sandbox::opencode()?),
            mcp_tool_prefix: "{server}_",
            effort_flag: None,
        })
    }

    /// The agent's arguments, with the effort flag after the subcommand when one is asked for.
    fn args_for(&self, task: &WorkerTask) -> Vec<String> {
        let mut args = self.args.clone();
        if let (Some(flag), Some(effort)) = (self.effort_flag, &task.effort) {
            let at = args.len().min(1);
            args.splice(at..at, [flag.to_string(), effort.clone()]);
        }
        args
    }

    fn command(&self, task: &WorkerTask) -> anyhow::Result<Command> {
        let args = self.args_for(task);
        let Some(sandbox) = &self.sandbox else {
            let binary = resolve_binary(&self.binary)?;
            let mut cmd = Command::new(binary);
            cmd.args(&args).current_dir(&task.cwd);
            return Ok(cmd);
        };
        let binary = resolve_binary(&self.binary)?;
        let mut cmd = Command::new("bwrap");
        cmd.args(sandbox.bwrap_args(&binary, &args, &task.cwd, task.read_only));
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
        let mut command = self.command(&task)?;
        let (mut child, group) = crate::spawn_grouped(command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()))
            .with_context(|| format!("failed to start `{}` (is bubblewrap installed?)", self.binary))?;
        let mut session = Session {
            stdin: child.stdin.take().context("agent stdin was not captured")?,
            lines: BufReader::new(child.stdout.take().context("agent stdout was not captured")?)
                .lines(),
            next_id: 0,
            dictionary_prefix: task
                .dictionary
                .as_ref()
                .map(|d| self.mcp_tool_prefix.replace("{server}", &d.name)),
            gate,
            events,
            denied: 0,
            stall: task.stall,
            tools: BTreeSet::new(),
        };
        let timeout = task.timeout;
        let result = match tokio::time::timeout(timeout, session.drive(task)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("{} timed out after {timeout:?}", self.id)),
        };
        // The turn is over, finished or not: free the agent and everything it started.
        drop(session);
        crate::released(result, group.release(&mut child).await)
    }
}

/// The id of the session option that chooses the model, when the agent has one.
fn model_option(session: &Value) -> Option<&str> {
    session["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["category"] == "model" || option["id"] == "model")
        .and_then(|option| option["id"].as_str())
}

struct Session {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    dictionary_prefix: Option<String>,
    gate: Arc<dyn PermissionGate>,
    events: mpsc::Sender<HarnessEvent>,
    denied: u32,
    /// How long the agent may stay silent while it waits on its model.
    stall: Option<std::time::Duration>,
    /// Tool calls started and not yet finished. A running tool may be silent for a long time.
    tools: BTreeSet<String>,
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
            // Opencode offers the model as a session option. Antigravity and Grok Build
            // take `session/set_model`.
            let (method, params) = match model_option(&session) {
                Some(option) => (
                    "session/set_config_option",
                    json!({ "sessionId": session_id, "configId": option, "value": model }),
                ),
                None => ("session/set_model", json!({ "sessionId": session_id, "modelId": model })),
            };
            self.call(method, params).await.with_context(|| format!("could not select model `{model}`"))?;
        }

        let prompt = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": task.framed_brief() }],
        });
        let result = self.call("session/prompt", prompt).await?;
        Ok(RunSummary {
            ok: result["stopReason"] == "end_turn",
            usage: read_usage(&result),
            cost_usd: None,
            denied: self.denied,
            freed: None,
        })
    }

    /// Send a request and wait for its response, serving whatever the agent sends meanwhile.
    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        loop {
            let next = self.lines.next_line();
            let line = match self.stall.filter(|_| self.tools.is_empty()) {
                Some(limit) => match tokio::time::timeout(limit, next).await {
                    Ok(line) => line,
                    Err(_) => return Err(Stalled(limit).into()),
                },
                None => next.await,
            };
            let line = line?.ok_or_else(|| anyhow!("agent closed its output during `{method}`"))?;
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
                track_tool(&mut self.tools, &params["update"]);
                if let Some(event) = update_event(&params["update"]) {
                    let _ = self.events.send(event).await;
                }
            }
            Some("session/request_permission") => {
                let mut request = permission_request(&params["toolCall"]);
                let prefix = self.dictionary_prefix.as_deref();
                if prefix.is_some_and(|p| request.tool.starts_with(p)) {
                    request.kind = ToolKind::Dictionary;
                }
                let allowed = self.gate.decide_async(&request).await == Decision::Allow;
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

/// Keep the set of running tool calls current. A call is running from its first report
/// until an update says it completed or failed.
fn track_tool(tools: &mut BTreeSet<String>, update: &Value) {
    let Some(id) = update["toolCallId"].as_str() else { return };
    let finished = matches!(update["status"].as_str(), Some("completed" | "failed"));
    match update["sessionUpdate"].as_str() {
        Some("tool_call" | "tool_call_update") if finished => {
            tools.remove(id);
        }
        Some("tool_call") => {
            tools.insert(id.to_string());
        }
        _ => {}
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

/// Token counts from the prompt result. Harnesses put them in different places, and some
/// count cached tokens inside the input figure.
fn read_usage(result: &Value) -> Usage {
    let usage = if result["usage"].is_object() { &result["usage"] } else { &result["_meta"]["usage"] };
    let count = |keys: &[&str]| keys.iter().find_map(|k| usage[*k].as_u64()).unwrap_or(0);
    let (mut input, output) = (count(&["inputTokens"]), count(&["outputTokens"]));
    let cache_read = count(&["cachedReadTokens"]);
    // When input + output already equals the total, the cached tokens are inside the input.
    if usage["totalTokens"].as_u64() == Some(input + output) {
        input = input.saturating_sub(cache_read);
    }
    Usage { input, output, cache_read, cache_write: count(&["cachedWriteTokens", "cacheCreationTokens"]) }
}

/// The tool being called. Antigravity gives a `name`; Grok Build puts an MCP tool's name in
/// its input and a built-in's name in its metadata; the title is the last resort.
fn tool_name(tool_call: &Value) -> String {
    let name = tool_call["name"]
        .as_str()
        .or_else(|| tool_call["rawInput"]["tool_name"].as_str())
        .or_else(|| tool_call["_meta"]["x.ai/tool"]["name"].as_str())
        .or_else(|| tool_call["title"].as_str());
    name.unwrap_or_default().to_string()
}

fn permission_request(tool_call: &Value) -> PermissionRequest {
    let kind = match tool_call["kind"].as_str() {
        Some("read" | "search") => ToolKind::Read,
        Some("edit" | "delete" | "move") => ToolKind::Edit,
        Some("execute") => ToolKind::Execute,
        Some("fetch") => ToolKind::Fetch,
        _ => ToolKind::Other,
    };
    let raw = &tool_call["rawInput"];
    let locations = tool_call["locations"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut paths: Vec<PathBuf> =
        locations.iter().filter_map(|l| l["path"].as_str()).map(PathBuf::from).collect();
    if paths.is_empty() {
        // Not every harness fills `locations`; fall back to the tool's own path argument.
        let keys = ["file_path", "path", "Path", "filePath", "AbsolutePath"];
        paths.extend(keys.iter().find_map(|k| raw[*k].as_str()).map(PathBuf::from));
    }
    PermissionRequest {
        tool: tool_name(tool_call),
        kind,
        paths,
        command: raw["CommandLine"].as_str().or_else(|| raw["command"].as_str()).map(str::to_string),
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

    #[test]
    fn grok_takes_effort_on_the_agent_subcommand() {
        let harness = AcpHarness {
            id: "grok-build",
            binary: "grok".into(),
            args: vec!["agent".into(), "stdio".into()],
            sandbox: None,
            mcp_tool_prefix: "{server}__",
            effort_flag: Some("--reasoning-effort"),
        };
        let mut task = WorkerTask {
            brief: String::new(),
            cwd: PathBuf::from("/tmp"),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(1),
            stall: None,
            read_only: true,
        };
        assert_eq!(harness.args_for(&task), ["agent", "stdio"]);
        task.effort = Some("low".into());
        assert_eq!(harness.args_for(&task), ["agent", "--reasoning-effort", "low", "stdio"]);
    }

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

    // Shapes recorded from Grok Build 1.0.77.
    #[test]
    fn reads_grok_build_tool_calls() {
        let mcp = json!({
            "kind": "other", "title": "dict__cite",
            "rawInput": { "variant": "UseTool", "tool_name": "dict__cite", "tool_input": { "key": "k" } },
            "_meta": { "x.ai/tool": { "name": "use_tool", "kind": "use_tool" } }
        });
        assert_eq!(permission_request(&mcp).tool, "dict__cite");
        let write = json!({
            "kind": "edit", "title": "Write `/w/greeting.ts`",
            "rawInput": { "variant": "Write", "file_path": "/w/greeting.ts", "content": "x" },
            "_meta": { "x.ai/tool": { "name": "write", "kind": "write" } }
        });
        let request = permission_request(&write);
        assert_eq!((request.tool.as_str(), request.kind), ("write", ToolKind::Edit));
        assert_eq!(request.paths, vec![PathBuf::from("/w/greeting.ts")]);
    }

    #[test]
    fn usage_is_read_from_either_place_without_double_counting_cache() {
        // Antigravity: top-level usage, input excludes cached tokens.
        let agy = json!({ "usage": { "inputTokens": 46413, "outputTokens": 2879,
            "cachedReadTokens": 200132, "cachedWriteTokens": 0, "totalTokens": 249424 } });
        assert_eq!(read_usage(&agy).total(), 249424);
        // Grok Build: under _meta, input includes cached tokens.
        let grok = json!({ "_meta": { "usage": { "inputTokens": 75528, "outputTokens": 507,
            "totalTokens": 76035, "cachedReadTokens": 58368, "cacheCreationTokens": 0 } } });
        let usage = read_usage(&grok);
        assert_eq!((usage.input, usage.cache_read), (17160, 58368));
        assert_eq!(usage.total(), 76035);
    }

    #[tokio::test]
    async fn a_silent_agent_is_stopped_as_stalled() {
        let script = r#"read l; echo '{"jsonrpc":"2.0","id":1,"result":{}}'; read l; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}'; read l; sleep 30"#;
        let harness = AcpHarness {
            id: "fake",
            binary: "sh".into(),
            args: vec!["-c".into(), script.into()],
            sandbox: None,
            mcp_tool_prefix: "{server}__",
            effort_flag: None,
        };
        let task = WorkerTask {
            brief: "hi".into(),
            cwd: std::env::temp_dir(),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(20),
            stall: Some(std::time::Duration::from_millis(300)),
            read_only: true,
        };
        let (tx, _rx) = mpsc::channel(8);
        let started = std::time::Instant::now();
        let err = harness.run(task, Arc::new(crate::gate::WorktreeGate::new(std::env::temp_dir(), vec![])), tx).await.unwrap_err();
        assert!(err.downcast_ref::<Stalled>().is_some(), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_tool_runs_from_its_call_until_it_completes() {
        let mut tools = BTreeSet::new();
        track_tool(&mut tools, &json!({ "sessionUpdate": "tool_call", "toolCallId": "a", "status": "pending" }));
        track_tool(&mut tools, &json!({ "sessionUpdate": "tool_call", "toolCallId": "b", "status": "completed" }));
        assert_eq!(tools.len(), 1);
        track_tool(&mut tools, &json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "in_progress" }));
        assert_eq!(tools.len(), 1);
        track_tool(&mut tools, &json!({ "sessionUpdate": "tool_call_update", "toolCallId": "a", "status": "failed" }));
        assert!(tools.is_empty());
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
