//! Agent Client Protocol adapter, used for Antigravity, Grok Build and Opencode.
//!
//! ACP is JSON-RPC over the agent's stdio. The session is created with its working
//! directory and MCP servers, so nothing is written to the user's global configuration.
//! The agent sends `session/request_permission` before shell commands and MCP calls; each
//! one is answered by the [`PermissionGate`]. File access is confined by the [`Sandbox`].

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;
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
        // When Code Mode is active with the daemon's MCP server attached, writes and shell
        // commands go through `run_code` (which has its own sandboxed worktree access), so the
        // agent's own sandbox stays read-only even for harnesses that edit without asking.
        let enforce_code_mode = task.code_mode && !task.read_only && task.dictionary.is_some();
        let read_only = task.read_only || enforce_code_mode;
        let shell = (!enforce_code_mode).then_some(task.shell.as_ref()).flatten();
        cmd.args(sandbox.bwrap_args_with(&binary, &args, &task.cwd, read_only, shell));
        Ok(cmd)
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> &'static str {
        self.id
    }

    fn sandboxed_shell(&self) -> bool {
        self.sandbox.is_some() && crate::sandbox::bwrap_usable()
    }

    async fn run(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let (result, kept) = self.start(task, gate, events, false, None).await;
        debug_assert!(kept.is_none());
        result
    }

    async fn run_kept(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        self.start(task, gate, events, true, None).await
    }

    async fn resume_kept(
        &self,
        session_id: &str,
        prompt: String,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        self.start(task, gate, events, true, Some((session_id.to_string(), prompt))).await
    }

    async fn reattach(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
        attached: crate::child::Attached,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        let rebuilt = Rebuilt::from(&attached.history);
        let crate::child::Attached { io, proc, keep, .. } = attached;
        let mut session = self.session(&task, io, gate, events);
        session.next_id = rebuilt.next_id;
        session.id = rebuilt.session.clone();
        session.cost = rebuilt.cost;
        session.tools = rebuilt.tools.clone();
        session.denied = rebuilt.denied;
        let turn = async {
            let Some(prompt) = rebuilt.prompt else {
                // The turn ended before this daemon took over: its reply is in the log.
                return match &rebuilt.answered {
                    Some(result) => Ok(session.summary(result, rebuilt.cost_before)),
                    None => Err(anyhow!("the held agent is not inside a turn")),
                };
            };
            // What the agent asked that the last daemon never answered is asked again now.
            for msg in rebuilt.asked.clone() {
                session.incoming(msg).await?;
            }
            let result = session.wait(prompt, "session/prompt").await?;
            Ok(session.summary(&result, rebuilt.cost_before))
        };
        let timeout = task.timeout;
        let result = match tokio::time::timeout(timeout, turn).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("{} timed out after {timeout:?}", self.id)),
        };
        self.finish(session, proc, result, keep).await
    }
}

impl AcpHarness {
    /// Start the agent and run the first turn. With `keep`, a turn that finished leaves the
    /// agent running with its session open, for follow-up turns.
    async fn start(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
        keep: bool,
        resume: Option<(String, String)>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        let mut command = match self.command(&task) {
            Ok(command) => command,
            Err(err) => return (Err(err), None),
        };
        let spawned = crate::child::start(&mut command, true, task.hold.as_ref())
            .await
            .with_context(|| format!("failed to start `{}` (is bubblewrap installed?)", self.binary));
        let (proc, io) = match spawned {
            Ok(spawned) => spawned,
            Err(err) => return (Err(err), None),
        };
        let mut session = self.session(&task, io, gate, events);
        let timeout = task.timeout;
        let result = match tokio::time::timeout(timeout, session.drive(task, resume)).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("{} timed out after {timeout:?}", self.id)),
        };
        self.finish(session, proc, result, keep).await
    }

    fn session(&self, task: &WorkerTask, io: crate::child::AgentIo, gate: Arc<dyn PermissionGate>, events: mpsc::Sender<HarnessEvent>) -> Session {
        Session {
            io,
            next_id: 0,
            dictionary_prefix: task.dictionary.as_ref().map(|d| self.mcp_tool_prefix.replace("{server}", &d.name)),
            code_mode: task.code_mode && !task.read_only && task.dictionary.is_some(),
            gate,
            events,
            denied: 0,
            stall: task.stall,
            tools: BTreeSet::new(),
            id: None,
            cost: None,
        }
    }

    /// With `keep`, a turn that finished leaves the agent running with its session open;
    /// otherwise the agent and everything it started is freed.
    async fn finish(
        &self,
        mut session: Session,
        proc: crate::child::AgentProc,
        result: anyhow::Result<RunSummary>,
        keep: bool,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn crate::Session>>) {
        if keep && result.as_ref().is_ok_and(|summary| summary.ok) && session.id.is_some() {
            // The caller reads this turn's events until their channel closes: the kept session
            // must not hold its sender, or the turn never ends for the caller.
            session.events = closed_sender();
            // Between turns it is not handed off to a next daemon.
            session.io.set_in_turn(false);
            let kept = Kept { session, proc: Some(proc), harness: self.id };
            return (result, Some(Box::new(kept)));
        }
        // The turn is over, finished or not: free the agent and everything it started.
        drop(session);
        (crate::released(result, proc.release().await), None)
    }
}

/// A sender whose receiver is already gone: events sent to it go nowhere.
fn closed_sender() -> mpsc::Sender<HarnessEvent> {
    mpsc::channel(1).0
}

/// An ACP agent left running after its turn, with its session open.
struct Kept {
    session: Session,
    proc: Option<crate::child::AgentProc>,
    harness: &'static str,
}

#[async_trait]
impl crate::Session for Kept {
    async fn follow_up(&mut self, text: String, events: mpsc::Sender<HarnessEvent>, timeout: Duration) -> anyhow::Result<RunSummary> {
        self.session.events = events;
        self.session.denied = 0;
        self.session.io.set_in_turn(true);
        let turn = self.session.prompt(text);
        let result = match tokio::time::timeout(timeout, turn).await {
            Ok(result) => result,
            Err(_) => Err(anyhow!("{} timed out after {timeout:?}", self.harness)),
        };
        // Let go of this turn's channel, so the caller's reader finishes.
        self.session.events = closed_sender();
        self.session.io.set_in_turn(false);
        result
    }

    fn session_id(&self) -> Option<&str> {
        self.session.id.as_deref()
    }

    async fn close(mut self: Box<Self>) -> crate::Freed {
        match self.proc.take() {
            Some(proc) => proc.release().await,
            None => crate::Freed::default(),
        }
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
    io: crate::child::AgentIo,
    next_id: u64,
    dictionary_prefix: Option<String>,
    /// When Code Mode (`run_code`) is active on a builder, steer native shell/edit prompts to `run_code`.
    code_mode: bool,
    gate: Arc<dyn PermissionGate>,
    events: mpsc::Sender<HarnessEvent>,
    denied: u32,
    /// How long the agent may stay silent while it waits on its model.
    stall: Option<std::time::Duration>,
    /// Tool calls started and not yet finished. A running tool may be silent for a long time.
    tools: BTreeSet<String>,
    /// The ACP session, once `session/new` answered.
    id: Option<String>,
    /// The session's running cost, from the agent's `usage_update` events, when it sends one.
    cost: Option<f64>,
}

impl Session {
    async fn drive(&mut self, task: WorkerTask, resume: Option<(String, String)>) -> anyhow::Result<RunSummary> {
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

        if let Some((session_id, prompt)) = resume {
            let load = json!({ "sessionId": session_id, "cwd": task.cwd, "mcpServers": mcp_servers });
            self.call("session/load", load).await?;
            let _ = self.events.send(HarnessEvent::SessionStarted { session_id: session_id.clone() }).await;
            self.id = Some(session_id);
            return self.prompt(prompt).await;
        }

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

        let _ = self.events.send(HarnessEvent::SessionStarted { session_id: session_id.clone() }).await;
        self.id = Some(session_id);
        let run_code_tool = match &self.dictionary_prefix {
            Some(prefix) => format!("{prefix}run_code"),
            None => "run_code".to_string(),
        };
        self.prompt(task.framed_brief_for(&run_code_tool)).await
    }

    /// One turn in the open session.
    async fn prompt(&mut self, text: String) -> anyhow::Result<RunSummary> {
        self.tools.clear();
        let session_id = self.id.clone().context("the session is not open")?;
        let prompt = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        });
        let before = self.cost;
        let result = self.call("session/prompt", prompt).await?;
        Ok(self.summary(&result, before))
    }

    /// A turn's summary from its prompt reply, with `before` the running cost when it began.
    fn summary(&self, result: &Value, before: Option<f64>) -> RunSummary {
        // The reply's own price if it has one, else what the running cost grew by this turn.
        let cost = read_cost(result).or_else(|| self.cost.map(|now| (now - before.unwrap_or(0.0)).max(0.0)));
        RunSummary {
            ok: result["stopReason"] == "end_turn",
            usage: read_usage(result),
            cost_usd: cost,
            denied: self.denied,
            freed: None,
        }
    }

    /// Send a request and wait for its response, serving whatever the agent sends meanwhile.
    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        self.wait(id, method).await
    }

    /// Wait for the response to request `id`, serving whatever the agent sends meanwhile.
    async fn wait(&mut self, id: u64, method: &str) -> anyhow::Result<Value> {
        loop {
            let next = self.io.next_line();
            let line = match self.stall.filter(|_| self.tools.is_empty()) {
                Some(limit) => match tokio::time::timeout(limit, next).await {
                    Ok(line) => line,
                    Err(_) => return Err(Stalled(limit).into()),
                },
                None => next.await,
            };
            let line = line?.ok_or_else(|| anyhow!("agent closed its output during `{method}`"))?;
            trace("<", &line);
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
                if let Some(cost) = running_cost(&params["update"]) {
                    self.cost = Some(cost);
                }
                if let Some(report) = crate::parse_rate_limit_value(&params["update"], "acp") {
                    let _ = self.events.send(HarnessEvent::RateLimit(report)).await;
                }
                for event in update_events(&params["update"]) {
                    let _ = self.events.send(event).await;
                }
            }
            Some("session/request_permission") => {
                let mut request = permission_request(&params["toolCall"]);
                let prefix = self.dictionary_prefix.as_deref();
                if prefix.is_some_and(|p| request.tool.starts_with(p)) {
                    request.kind = if is_browser_tool(&request.tool) { ToolKind::Browser } else { ToolKind::Dictionary };
                } else if is_browser_tool(&request.tool) {
                    request.kind = ToolKind::Browser;
                }
                let blocked_by_code_mode = self.code_mode && matches!(request.kind, ToolKind::Execute | ToolKind::Edit);
                let allowed = !blocked_by_code_mode && self.gate.decide_async(&request).await == Decision::Allow;
                if !allowed {
                    if let Some(id) = params["toolCall"]["toolCallId"].as_str() {
                        self.tools.remove(id);
                    }
                    if !blocked_by_code_mode {
                        self.denied += 1;
                    }
                }
                let reported_tool = if blocked_by_code_mode {
                    format!("{} (use run_code)", request.tool)
                } else {
                    request.tool
                };
                let _ = self
                    .events
                    .send(HarnessEvent::PermissionDecided { tool: reported_tool, allowed })
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
        let line = msg.to_string();
        trace(">", &line);
        self.io.write_line(&line).await?;
        Ok(())
    }
}

/// What an ACP agent's log says about its session, for a daemon that takes it over (H3).
#[derive(Debug, Default)]
struct Rebuilt {
    /// The highest request id this side sent; the next request takes the one after.
    next_id: u64,
    session: Option<String>,
    cost: Option<f64>,
    /// The running cost when the last turn began.
    cost_before: Option<f64>,
    tools: BTreeSet<String>,
    /// The turn's `session/prompt`, still waiting for its reply.
    prompt: Option<u64>,
    /// The last turn's reply, when it came before the hand-off.
    answered: Option<Value>,
    /// Requests from the agent that were never answered, in order.
    asked: Vec<Value>,
    /// Permission requests refused this turn.
    denied: u32,
}

impl Rebuilt {
    fn from(history: &[crate::hold::Logged]) -> Rebuilt {
        use crate::hold::Logged;
        let mut out = Rebuilt::default();
        let mut sent: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        for entry in history {
            let (line, ours) = match entry {
                Logged::In(line) => (line, true),
                Logged::Out(line) => (line, false),
            };
            let Ok(msg) = serde_json::from_str::<Value>(line) else { continue };
            let method = msg["method"].as_str();
            match (ours, method) {
                (true, Some(method)) => {
                    let Some(id) = msg["id"].as_u64() else { continue };
                    out.next_id = out.next_id.max(id);
                    sent.insert(id, method.to_string());
                    match method {
                        "session/load" => out.session = msg["params"]["sessionId"].as_str().map(str::to_string),
                        "session/prompt" => {
                            out.prompt = Some(id);
                            out.answered = None;
                            out.cost_before = out.cost;
                            out.denied = 0;
                        }
                        _ => {}
                    }
                }
                (true, None) => {
                    // An answer to one of the agent's requests.
                    let Some(at) = out.asked.iter().position(|asked| asked["id"] == msg["id"]) else { continue };
                    let asked = out.asked.remove(at);
                    if asked["method"] == "session/request_permission" && !allowed_by(&asked, &msg) {
                        out.denied += 1;
                    }
                }
                (false, Some("session/update")) => {
                    let update = &msg["params"]["update"];
                    track_tool(&mut out.tools, update);
                    if let Some(cost) = running_cost(update) {
                        out.cost = Some(cost);
                    }
                }
                (false, Some(_)) if msg.get("id").is_some() => out.asked.push(msg),
                (false, Some(_)) => {}
                (false, None) => {
                    let Some(id) = msg["id"].as_u64() else { continue };
                    match sent.get(&id).map(String::as_str) {
                        Some("session/new") => out.session = msg["result"]["sessionId"].as_str().map(str::to_string),
                        Some("session/prompt") if out.prompt == Some(id) => {
                            out.prompt = None;
                            out.answered = Some(msg["result"].clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }
}

/// Whether an answer to a permission request allowed the call.
fn allowed_by(asked: &Value, reply: &Value) -> bool {
    let outcome = &reply["result"]["outcome"];
    let Some(chosen) = outcome["optionId"].as_str().filter(|_| outcome["outcome"] == "selected") else { return false };
    asked["params"]["options"]
        .as_array()
        .and_then(|options| options.iter().find(|o| o["optionId"] == chosen))
        .is_some_and(|option| option["kind"].as_str().is_some_and(|kind| kind.starts_with("allow")))
}

/// With `TASKMASTER_ACP_TRACE=<file>`, every line to (`>`) and from (`<`) an ACP agent is
/// appended to that file, for finding out why an agent stopped answering. Off by default.
fn trace(way: &str, line: &str) {
    let Some(path) = std::env::var_os("TASKMASTER_ACP_TRACE") else { return };
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{way} {line}");
    }
}

/// Keep the set of running tool calls current. A call is running from its first report
/// until an update says it reached any terminal status.
fn track_tool(tools: &mut BTreeSet<String>, update: &Value) {
    let Some(id) = update["toolCallId"].as_str() else { return };
    let finished = matches!(
        update["status"].as_str(),
        Some("completed" | "failed" | "cancelled" | "canceled" | "rejected" | "denied" | "error")
    );
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

fn update_events(update: &Value) -> Vec<HarnessEvent> {
    let text = || update["content"]["text"].as_str().unwrap_or_default().to_string();
    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => vec![HarnessEvent::Text { text: text() }],
        Some("agent_thought_chunk") => vec![HarnessEvent::Thought { text: text() }],
        Some("tool_call") => {
            let id = update["toolCallId"].as_str().unwrap_or_default().to_string();
            let mut events = vec![HarnessEvent::ToolCall {
                id: id.clone(),
                name: tool_name(update),
                input: update["rawInput"].clone(),
            }];
            if let Some(result) = tool_result(&id, update) {
                events.push(result);
            }
            events
        }
        // A later update carries the call's output. Status alone is not a result.
        Some("tool_call_update") => {
            let id = update["toolCallId"].as_str().unwrap_or_default().to_string();
            tool_result(&id, update).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// Text and a diff from an ACP tool call, only from fields the agent sent.
/// `content` is a list of `{ type: "content", content: { type: "text", text } }`
/// or `{ type: "diff", path, oldText?, newText }`. `rawOutput` is the fallback.
fn tool_result(id: &str, update: &Value) -> Option<HarnessEvent> {
    let mut text = String::new();
    let mut diffs = Vec::new();
    if let Some(blocks) = update["content"].as_array() {
        for block in blocks {
            match block["type"].as_str() {
                Some("content") => push_text(&mut text, content_text(&block["content"])),
                Some("diff") => {
                    if let Some(diff) = acp_diff(block) {
                        diffs.push(diff);
                    }
                }
                _ => {}
            }
        }
    }
    if text.is_empty() {
        push_text(&mut text, loose_output(&update["rawOutput"]));
    }
    if text.is_empty() {
        push_text(&mut text, loose_output(&update["output"]));
    }
    if text.is_empty() && diffs.is_empty() {
        return None;
    }
    Some(HarnessEvent::ToolResult {
        id: id.to_string(),
        text: crate::clip_result(&text),
        diff: (!diffs.is_empty()).then(|| crate::clip_result(&diffs.join("\n"))),
    })
}

fn push_text(into: &mut String, next: String) {
    if next.is_empty() {
        return;
    }
    if !into.is_empty() {
        into.push('\n');
    }
    into.push_str(&next);
}

fn content_text(content: &Value) -> String {
    if let Some(text) = content["text"].as_str() {
        return text.to_string();
    }
    content.as_str().unwrap_or_default().to_string()
}

fn loose_output(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    for key in ["stdout", "output", "text", "content"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    String::new()
}

/// A unified diff from the `oldText` and `newText` an ACP diff block already carried.
fn acp_diff(block: &Value) -> Option<String> {
    if let Some(diff) = block["diff"].as_str().filter(|d| !d.is_empty()) {
        return Some(diff.to_string());
    }
    if block.get("newText").is_none() && block.get("oldText").is_none() {
        return None;
    }
    let path = block["path"].as_str().filter(|p| !p.is_empty()).unwrap_or("file");
    let old = block["oldText"].as_str().unwrap_or("");
    let new = block["newText"].as_str().unwrap_or("");
    Some(unified(path, old, new))
}

fn unified(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = split_lines(old);
    let new_lines: Vec<&str> = split_lines(new);
    let mut prefix = 0;
    while prefix < old_lines.len() && prefix < new_lines.len() && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_hunk = &old_lines[prefix..old_lines.len() - suffix];
    let new_hunk = &new_lines[prefix..new_lines.len() - suffix];
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    if old_hunk.is_empty() && new_hunk.is_empty() {
        return out;
    }
    out.push_str(&format!("@@ -{},{} +{},{} @@\n", prefix + 1, old_hunk.len(), prefix + 1, new_hunk.len()));
    for line in old_hunk {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in new_hunk {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
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

/// A session's running cost from a `usage_update` (opencode: `{"cost":{"amount":0.12,"currency":"USD"}}`).
/// Only US dollars are read.
fn running_cost(update: &Value) -> Option<f64> {
    if update["sessionUpdate"] != "usage_update" {
        return None;
    }
    let cost = &update["cost"];
    let currency = cost["currency"].as_str().unwrap_or("USD");
    currency.eq_ignore_ascii_case("usd").then(|| cost["amount"].as_f64()).flatten()
}

/// A price a prompt reply carries, wherever the harness put it.
fn read_cost(result: &Value) -> Option<f64> {
    let places = [&result["usage"], &result["_meta"]["usage"], &result["_meta"], result];
    let keys = ["costUsd", "cost_usd", "totalCostUsd", "total_cost_usd"];
    places.iter().find_map(|place| keys.iter().find_map(|key| place[*key].as_f64()))
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
        url: raw["url"].as_str().or_else(|| raw["path"].as_str().filter(|p| p.starts_with('/'))).map(str::to_string),
    }
}

/// Taskmaster's browser tools, however the harness prefixes their names.
pub fn is_browser_tool(name: &str) -> bool {
    ["browser_open", "browser_click", "browser_type", "browser_screenshot", "browser_console"].iter().any(|tool| name.ends_with(tool))
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

    impl AcpHarness {
        fn fake() -> AcpHarness {
            AcpHarness { id: "fake", binary: "sh".into(), args: Vec::new(), sandbox: None, mcp_tool_prefix: "{server}__", effort_flag: None }
        }
    }

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
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: None,
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
    fn a_price_is_read_from_the_reply_or_from_the_running_cost() {
        assert_eq!(read_cost(&json!({ "stopReason": "end_turn", "_meta": { "usage": { "costUsd": 0.42 } } })), Some(0.42));
        assert_eq!(read_cost(&json!({ "stopReason": "end_turn", "total_cost_usd": 0.98 })), Some(0.98));
        assert_eq!(read_cost(&json!({ "stopReason": "end_turn", "usage": { "inputTokens": 5 } })), None);
        // Recorded from opencode 2.3.0 with a free model.
        let free = json!({ "sessionUpdate": "usage_update", "used": 7863, "size": 200000, "cost": { "amount": 0, "currency": "USD" } });
        assert_eq!(running_cost(&free), Some(0.0));
        assert_eq!(running_cost(&json!({ "sessionUpdate": "usage_update", "cost": { "amount": 3, "currency": "EUR" } })), None);
        assert_eq!(running_cost(&json!({ "sessionUpdate": "agent_message_chunk" })), None);
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
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: None,
        };
        let (tx, _rx) = mpsc::channel(8);
        let started = std::time::Instant::now();
        let err = harness.run(task, Arc::new(crate::gate::WorktreeGate::new(std::env::temp_dir(), vec![])), tx).await.unwrap_err();
        assert!(err.downcast_ref::<Stalled>().is_some(), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_kept_session_takes_a_follow_up_turn_and_closes() {
        let say = |id: u32, text: &str| format!(
            r#"echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{text}"}}}}}}}}'; echo '{{"jsonrpc":"2.0","id":{id},"result":{{"stopReason":"end_turn"}}}}'"#
        );
        let script = format!(
            r#"read l; echo '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'; read l; echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s"}}}}'; read l; {}; read l; case "$l" in *again*) {};; esac; sleep 30"#,
            say(3, "first"),
            say(4, "second")
        );
        let harness = AcpHarness { id: "fake", binary: "sh".into(), args: vec!["-c".into(), script], sandbox: None, mcp_tool_prefix: "{server}__", effort_flag: None };
        let task = WorkerTask {
            brief: "build".into(),
            cwd: std::env::temp_dir(),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(10),
            stall: None,
            read_only: false,
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: None,
        };
        let gate = Arc::new(crate::gate::WorktreeGate::new(std::env::temp_dir(), vec![]));
        let (tx, mut rx) = mpsc::channel(32);
        let (first, kept) = harness.run_kept(task, gate, tx).await;
        assert!(first.unwrap().ok);
        assert_eq!(rx.recv().await, Some(HarnessEvent::SessionStarted { session_id: "s".into() }));
        assert_eq!(rx.recv().await, Some(HarnessEvent::Text { text: "first".into() }));
        // The turn is over for the caller: its event channel closes even though the session
        // stays open. (A kept sender here once hung every builder after its first turn.)
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
        assert_eq!(closed, Ok(None), "the kept session still held the turn's event sender");
        let mut kept = kept.expect("the session stays open after a finished turn");
        let (tx, mut rx) = mpsc::channel(32);
        let second = kept.follow_up("try again".into(), tx, std::time::Duration::from_secs(10)).await.unwrap();
        assert!(second.ok);
        assert_eq!(rx.recv().await, Some(HarnessEvent::Text { text: "second".into() }));
        let closed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await;
        assert_eq!(closed, Ok(None), "a follow-up's channel closes when the turn ends");
        let freed = kept.close().await;
        assert!(freed.processes >= 1 && freed.lingering == 0, "{freed:?}");
    }

    /// A gate that never answers, like a person who has not looked yet.
    struct Waiting;

    #[async_trait]
    impl PermissionGate for Waiting {
        fn decide(&self, _request: &PermissionRequest) -> Decision {
            Decision::Deny
        }
        async fn decide_async(&self, _request: &PermissionRequest) -> Decision {
            std::future::pending().await
        }
    }

    struct Allow;

    #[async_trait]
    impl PermissionGate for Allow {
        fn decide(&self, _request: &PermissionRequest) -> Decision {
            Decision::Allow
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_question_left_open_at_a_hand_off_is_answered_by_the_next_daemon_and_the_turn_ends_once() {
        let say = |text: &str| format!(r#"echo '{{"jsonrpc":"2.0","method":"session/update","params":{{"update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{text}"}}}}}}}}'"#);
        // The agent asks before a command, then says whether it was let through. A request sent
        // twice would be read as the answer and come out as "refused".
        let script = format!(
            r#"read l; echo '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read l; echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s-h3"}}}}'
read l
{before}
echo '{{"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{{"toolCall":{{"name":"run_command","kind":"execute","rawInput":{{"command":"ls"}}}},"options":[{{"optionId":"yes","kind":"allow_once"}},{{"optionId":"no","kind":"reject_once"}}]}}}}'
read r
case "$r" in *'"optionId":"yes"'*) {allowed};; *) {refused};; esac
echo '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
sleep 30"#,
            before = say("before"),
            allowed = say("allowed"),
            refused = say("refused"),
        );
        let dir = std::env::temp_dir().join(format!("tm-acp-reattach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let holder = crate::hold::in_process(&dir).unwrap();
        let harness = AcpHarness { id: "fake", binary: "sh".into(), args: vec!["-c".into(), script], sandbox: None, mcp_tool_prefix: "{server}__", effort_flag: None };
        let task = WorkerTask {
            brief: "build".into(),
            cwd: std::env::temp_dir(),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(20),
            stall: None,
            read_only: false,
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: Some(crate::hold::HoldSpec { holder: holder.clone(), key: "p/r/unit:a".into(), meta: json!({}) }),
        };
        let (tx, mut rx) = mpsc::channel(32);
        let running = tokio::spawn({
            let (harness, task) = (AcpHarness { args: harness.args.clone(), binary: harness.binary.clone(), ..AcpHarness::fake() }, task.clone());
            async move { harness.run(task, Arc::new(Waiting), tx).await }
        });
        assert_eq!(rx.recv().await, Some(HarnessEvent::SessionStarted { session_id: "s-h3".into() }));
        assert_eq!(rx.recv().await, Some(HarnessEvent::Text { text: "before".into() }));
        // The question is out and nobody has answered when the daemon hands off.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(holder.hand_off(std::time::Duration::from_secs(60), std::time::Duration::from_millis(50)).await.unwrap(), 1);
        running.abort();
        let _ = running.await;

        let next = crate::hold::Holder::new(holder.socket(), None);
        let spec = crate::hold::HoldSpec { holder: next, key: "p/r/unit:a".into(), meta: json!({}) };
        let attached = crate::child::attach(&spec, false).await.unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let (summary, kept) = harness.reattach(WorkerTask { hold: Some(spec), ..task }, Arc::new(Allow), tx, attached).await;
        assert!(summary.unwrap().ok && kept.is_none());
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert_eq!(
            events,
            vec![
                HarnessEvent::PermissionDecided { tool: "run_command".into(), allowed: true },
                HarnessEvent::Text { text: "allowed".into() },
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_gives_the_session_the_open_prompt_and_the_questions_never_answered() {
        use crate::hold::Logged::{In, Out};
        let history = vec![
            In(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into()),
            Out(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.into()),
            In(r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}"#.into()),
            Out(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s9"}}"#.into()),
            In(r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{}}"#.into()),
            Out(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"usage_update","cost":{"amount":0.5,"currency":"USD"}}}}"#.into()),
            Out(r#"{"jsonrpc":"2.0","id":"q1","method":"session/request_permission","params":{"options":[{"optionId":"n","kind":"reject_once"}]}}"#.into()),
            In(r#"{"jsonrpc":"2.0","id":"q1","result":{"outcome":{"outcome":"selected","optionId":"n"}}}"#.into()),
            Out(r#"{"jsonrpc":"2.0","id":"q2","method":"session/request_permission","params":{}}"#.into()),
            Out(r#"{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","status":"pending"}}}"#.into()),
        ];
        let rebuilt = Rebuilt::from(&history);
        assert_eq!((rebuilt.next_id, rebuilt.session.as_deref(), rebuilt.prompt), (3, Some("s9"), Some(3)));
        assert_eq!((rebuilt.cost, rebuilt.cost_before, rebuilt.denied), (Some(0.5), None, 1));
        assert_eq!(rebuilt.asked.len(), 1);
        assert_eq!(rebuilt.asked[0]["id"], "q2");
        assert!(rebuilt.tools.contains("t1"));
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
        assert_eq!(update_events(&chunk), vec![HarnessEvent::Text { text: "hi".into() }]);
        let call = update_events(&shell_call());
        assert!(matches!(&call[..], [HarnessEvent::ToolCall { name, .. }] if name == "run_command"));
        assert!(update_events(&json!({ "sessionUpdate": "usage_update", "used": 0 })).is_empty());
    }

    #[test]
    fn a_tool_update_attaches_its_output_and_diff_and_nothing_else() {
        let update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_1",
            "status": "completed",
            "content": [
                { "type": "content", "content": { "type": "text", "text": "ok\n" } },
                { "type": "diff", "path": "a.ts", "oldText": "a\n", "newText": "b\n" }
            ]
        });
        let events = update_events(&update);
        assert_eq!(events.len(), 1);
        match &events[0] {
            HarnessEvent::ToolResult { id, text, diff } => {
                assert_eq!(id, "call_1");
                assert_eq!(text, "ok\n");
                let diff = diff.as_deref().unwrap();
                assert!(diff.contains("--- a/a.ts") && diff.contains("-a") && diff.contains("+b"), "{diff}");
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
        assert!(update_events(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "call_1", "status": "completed"
        }))
        .is_empty());
        let both = json!({
            "sessionUpdate": "tool_call", "toolCallId": "call_2", "name": "write",
            "rawInput": { "file_path": "a.ts" }, "status": "completed",
            "rawOutput": "wrote a.ts"
        });
        let events = update_events(&both);
        assert!(matches!(&events[0], HarnessEvent::ToolCall { name, .. } if name == "write"));
        assert!(matches!(&events[1], HarnessEvent::ToolResult { text, diff: None, .. } if text == "wrote a.ts"));
        assert!(!events.iter().any(|event| matches!(event, HarnessEvent::Thought { .. })));
    }

    #[tokio::test]
    async fn code_mode_blocks_native_edit_and_execute_and_allows_run_code() {
        let script = r#"
read l; echo '{"jsonrpc":"2.0","id":1,"result":{}}'
read l; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}'
read l
echo '{"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{"toolCall":{"name":"write_file","kind":"edit","locations":[{"path":"/tmp/a.ts"}]},"options":[{"optionId":"allow","kind":"allow_once"},{"optionId":"deny","kind":"reject_once"}]}}'
read r1
echo '{"jsonrpc":"2.0","id":11,"method":"session/request_permission","params":{"toolCall":{"name":"taskmaster__run_code","kind":"other","rawInput":{"code":"return 1"}},"options":[{"optionId":"allow","kind":"allow_once"},{"optionId":"deny","kind":"reject_once"}]}}'
read r2
echo '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
"#;
        let harness = AcpHarness {
            id: "fake",
            binary: "sh".into(),
            args: vec!["-c".into(), script.into()],
            sandbox: None,
            mcp_tool_prefix: "{server}__",
            effort_flag: None,
        };
        let task = WorkerTask {
            brief: "build".into(),
            cwd: PathBuf::from("/tmp"),
            model: None,
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
        let gate = Arc::new(crate::gate::WorktreeGate::new(PathBuf::from("/tmp"), vec![]));
        let (tx, mut rx) = mpsc::channel(16);
        let summary = harness.run(task, gate, tx).await.unwrap();
        assert!(summary.ok);
        assert_eq!(summary.denied, 0, "Code Mode redirection should not count as a capability denial");
        let mut decisions = Vec::new();
        while let Some(ev) = rx.recv().await {
            if let HarnessEvent::PermissionDecided { tool, allowed } = ev {
                decisions.push((tool, allowed));
            }
        }
        assert_eq!(
            decisions,
            vec![
                ("write_file (use run_code)".to_string(), false),
                ("taskmaster__run_code".to_string(), true),
            ]
        );
    }

    #[tokio::test]
    async fn antigravity_code_mode_prompt_names_mcp_dictionary_run_code() {
        assert_eq!(crate::run_code_tool_name("antigravity", "dictionary"), "mcp_dictionary_run_code");
        assert_eq!(crate::run_code_tool_name("claude", "dictionary"), "mcp__dictionary__run_code");
        assert_eq!(crate::run_code_tool_name("grok-build", "dictionary"), "dictionary__run_code");
        assert_eq!(crate::run_code_tool_name("opencode", "dictionary"), "dictionary_run_code");

        let capture = std::env::temp_dir().join(format!("tm-acp-prompt-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&capture);
        let script = format!(
            r#"
read l; echo '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read l; echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s"}}}}'
read l; printf '%s\n' "$l" > "{}"
echo '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
"#,
            capture.display()
        );
        let harness = AcpHarness {
            id: "antigravity",
            binary: "sh".into(),
            args: vec!["-c".into(), script],
            sandbox: None,
            mcp_tool_prefix: "mcp_{server}_",
            effort_flag: None,
        };
        let task = WorkerTask {
            brief: "implement parser".into(),
            cwd: PathBuf::from("/tmp"),
            model: None,
            effort: None,
            dictionary: Some(crate::DictionaryAccess {
                name: "dictionary".into(),
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
        let gate = Arc::new(crate::gate::WorktreeGate::new(PathBuf::from("/tmp"), vec![]));
        let (tx, _rx) = mpsc::channel(16);
        let summary = harness.run(task, gate, tx).await.unwrap();
        assert!(summary.ok);
        let sent = std::fs::read_to_string(&capture).unwrap();
        let _ = std::fs::remove_file(&capture);
        assert!(
            sent.contains("mcp_dictionary_run_code"),
            "expected prompt to name mcp_dictionary_run_code, got: {sent}"
        );
        assert!(
            !sent.contains("`run_code`"),
            "prompt for antigravity should not tell agent to call bare `run_code`: {sent}"
        );
    }

    #[tokio::test]
    async fn denied_or_cancelled_tool_call_does_not_disable_stall_timer() {
        let mut tools = BTreeSet::new();
        for status in ["cancelled", "canceled", "rejected", "denied", "error"] {
            track_tool(&mut tools, &json!({ "sessionUpdate": "tool_call", "toolCallId": "t1" }));
            assert!(tools.contains("t1"));
            track_tool(&mut tools, &json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": status }));
            assert!(!tools.contains("t1"), "status `{status}` must clear tracked tool");
        }

        // Agent announces tool_call t-denied, requests permission (which is denied), never emits
        // a completed update, and hangs silently: the stall timer must still fire!
        let script = r#"
read l; echo '{"jsonrpc":"2.0","id":1,"result":{}}'
read l; echo '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}'
read l
echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t-denied","name":"shell","kind":"execute"}}}'
echo '{"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"t-denied","name":"shell","kind":"execute","rawInput":{"command":"id"}},"options":[{"optionId":"allow","kind":"allow_once"},{"optionId":"deny","kind":"reject_once"}]}}'
read r
sleep 30
"#;
        let harness = AcpHarness {
            id: "fake",
            binary: "sh".into(),
            args: vec!["-c".into(), script.into()],
            sandbox: None,
            mcp_tool_prefix: "{server}__",
            effort_flag: None,
        };
        let task = WorkerTask {
            brief: "build".into(),
            cwd: PathBuf::from("/tmp"),
            model: None,
            effort: None,
            dictionary: None,
            timeout: std::time::Duration::from_secs(10),
            stall: Some(std::time::Duration::from_millis(150)),
            read_only: false,
            code_mode: false,
            shell: None,
            permission_mode: None,
            hold: None,
        };
        let gate = Arc::new(crate::gate::WorktreeGate::new(PathBuf::from("/tmp"), vec![]));
        let (tx, _rx) = mpsc::channel(16);
        let err = harness.run(task, gate, tx).await.unwrap_err();
        assert!(
            err.downcast_ref::<Stalled>().is_some(),
            "denied tool call must not permanently suppress stall detection; got: {err:#}"
        );
    }

    #[tokio::test]
    async fn acp_attaches_dictionary_mcp_and_allows_flow_query_in_code_mode_off_and_on() {
        let cases: &[(&'static str, &'static str, &str)] = &[
            ("antigravity", "mcp_{server}_", "mcp_dictionary_flow_query"),
            ("grok-build", "{server}__", "dictionary__flow_query"),
            ("opencode", "{server}_", "dictionary_flow_query"),
        ];
        for &(hid, prefix, flow_tool) in cases {
            for code_mode in [false, true] {
                let capture = std::env::temp_dir().join(format!(
                    "tm-acp-flow-mcp-{hid}-{code_mode}-{}.txt",
                    std::process::id()
                ));
                let _ = std::fs::remove_file(&capture);
                let script = format!(
                    r#"
read l; echo '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read l; printf '%s\n' "$l" > "{}"
echo '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s"}}}}'
read l
echo '{{"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{{"toolCall":{{"toolCallId":"t-flow","name":"{flow_tool}","kind":"other","rawInput":{{"op":"find","query":"Store"}}}},"options":[{{"optionId":"allow","kind":"allow_once"}},{{"optionId":"deny","kind":"reject_once"}}]}} }}'
read perm_flow
printf '%s\n' "$perm_flow" >> "{}"
echo '{{"jsonrpc":"2.0","id":3,"result":{{"stopReason":"end_turn"}}}}'
"#,
                    capture.display(),
                    capture.display(),
                );
                let harness = AcpHarness {
                    id: hid,
                    binary: "sh".into(),
                    args: vec!["-c".into(), script],
                    sandbox: None,
                    mcp_tool_prefix: prefix,
                    effort_flag: None,
                };
                let task = WorkerTask {
                    brief: "build".into(),
                    cwd: PathBuf::from("/tmp"),
                    model: None,
                    effort: None,
                    dictionary: Some(crate::DictionaryAccess {
                        name: "dictionary".into(),
                        url: "http://127.0.0.1:43210/mcp".into(),
                        bearer_token: "secret-tok".into(),
                    }),
                    timeout: std::time::Duration::from_secs(10),
                    stall: None,
                    read_only: false,
                    code_mode,
                    shell: None,
                    permission_mode: None,
                    hold: None,
                };
                let gate = Arc::new(crate::gate::WorktreeGate::new(PathBuf::from("/tmp"), vec![]));
                let (tx, _rx) = mpsc::channel(16);
                let summary = harness.run(task, gate, tx).await.unwrap();
                assert!(summary.ok, "{hid} code_mode={code_mode}");
                assert_eq!(summary.denied, 0, "{flow_tool} must be allowed on {hid} (code_mode={code_mode})");
                let log = std::fs::read_to_string(&capture).unwrap();
                let _ = std::fs::remove_file(&capture);
                let mut lines = log.lines();
                let session_new: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
                let servers = session_new["params"]["mcpServers"].as_array().unwrap();
                assert_eq!(servers.len(), 1);
                assert_eq!(servers[0]["name"], "dictionary");
                assert_eq!(servers[0]["url"], "http://127.0.0.1:43210/mcp");
                assert_eq!(servers[0]["headers"][0]["value"], "Bearer secret-tok");
                let perm_reply: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
                assert_eq!(
                    perm_reply["result"]["outcome"]["optionId"], "allow",
                    "{flow_tool} must be allowed by permission gate on {hid} (code_mode={code_mode}): {perm_reply}"
                );
            }
        }
    }
}
