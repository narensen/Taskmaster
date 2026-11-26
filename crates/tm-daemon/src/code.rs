//! Code Mode for every harness: `run_code`, one tool call that runs a short JavaScript
//! program over the agent's worktree.
//!
//! An agent that would make ten tool calls (read three files, edit two, search, check)
//! writes one script instead, and gets back one answer, which saves the round trips and
//! the output tokens each call costs. The script runs in Node with its permission model
//! on (no files, no child processes, no workers) and, where bubblewrap is installed,
//! without a network. It reaches the worktree only through these functions, each sent
//! back to the daemon and put to the same permission gate the agent's own tools face:
//!
//! ```text
//! await read(path)                    file text
//! await write(path, text)
//! await edit(path, find, replace, all?)   find must occur once unless all is true
//! await readMany(globs)               {path: text} for every file the globs match
//! await grep(text, glob?)             ["path:line: text", ...]
//! await ls(glob?)                     project files
//! await sh(command)                   {code, output}; only inside bubblewrap, when the gate allows a shell
//! await test()                        {ok, failed, output}: the project's tests, in the sandbox
//! await check()                       problems the static checks find in the worktree
//! await browser(action, args)         open | click | type | screenshot | console on the page's preview
//! console.log(...)                    lines returned with the result
//! ```
//!
//! The tool is served beside the Dictionary under the page's token, so a page that is not
//! being built has none.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use tm_dictionary::{Extension, Grant};
use tm_harness::{Decision, PermissionGate, PermissionRequest, ToolKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long one script may run, all calls included, unless the settings say otherwise.
pub const SCRIPT_SECS: u64 = 600;
/// How long one `sh` or `test` call may run, unless the settings say otherwise.
pub const SH_SECS: u64 = 300;
const MAX_READ: usize = 256 * 1024;
const MAX_REPLY: usize = 24 * 1024;
const MAX_CALLS: usize = 200;

/// Persistent warm Node sandbox worker script: executes multiple `run_code` bodies over
/// NDJSON stdin/stdout without paying `bwrap` + `node` cold-start cost on every tool call.
/// Each script body runs in a fresh `AsyncFunction` scope with its own `logs` and `console`.
const WORKER_SCRIPT: &str = r#""use strict";
const readline = require("readline");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const lines = readline.createInterface({ input: process.stdin });
const waiting = new Map();
let next = 1;
const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const call = (op, args) => new Promise((resolve, reject) => {
  const id = next++;
  waiting.set(id, { resolve, reject });
  send({ id, op, args });
});
const read = (path) => call("read", { path });
const extractText = (value) => {
  if (value && typeof value === "object") {
    if (typeof value.raw === "string") return value.raw;
    if (typeof value.content === "string") return value.content;
  }
  return String(value ?? "");
};
const write = (path, text) => call("write", { path, text: extractText(text) });
const writeFile = (path, text) => call("write", { path, text: extractText(text) });
const edit = (path, find, replace, all = false) => call("edit", { path, find, replace, all });
const patch = (path, edits) => call("patch", { path, edits });
const applyPatch = (diff) => call("applyPatch", { diff: String(diff) });
const tryPatch = (diff) => call("tryPatch", { diff: String(diff) });
const grep = (text, glob) => call("grep", { text, glob });
const ls = (glob) => call("ls", { glob });
const diff = (glob) => call("diff", { glob });
const symbols = (glob) => call("symbols", { glob });
const flow = (op, args = {}) => call("flow", { op: String(op ?? ""), args: args && typeof args === "object" ? args : {} });
const readMany = (globs) => call("readMany", { globs: Array.isArray(globs) ? globs : [globs] });
const test = () => call("test", {});
const testAffected = (paths) => call("testAffected", { files: Array.isArray(paths) ? paths : (paths ? [paths] : []) });
const probe = {
  ast: (path) => call("probeAst", { path }),
};
const diagnostics = (files) => call("diagnostics", { files: Array.isArray(files) ? files : (files ? [files] : []) });
const outline = (path) => call("outline", { path });
const definition = (symbol) => call("definition", { symbol: String(symbol) });
const references = (symbol) => call("references", { symbol: String(symbol) });
const sh = (command) => call("sh", { command });
const check = () => call("check", {});
const browser = (action, args = {}) => call("browser", { action, args });
const status = (text) => call("status", { text: String(text ?? "") });
const wait = (opts) => call("wait", opts && typeof opts === "object" ? opts : {});

async function runBody(code) {
  const logs = [];
  const logFn = (...parts) =>
    logs.push(parts.map((p) => (typeof p === "string" ? p : JSON.stringify(p))).join(" "));
  const scopedConsole = { log: logFn, info: logFn, warn: logFn, error: logFn };
  let fn;
  try {
    fn = new AsyncFunction(
      "read", "write", "writeFile", "edit", "patch", "applyPatch", "tryPatch", "grep", "ls", "diff", "symbols", "flow",
      "readMany", "test", "testAffected", "probe", "diagnostics", "outline", "definition", "references",
      "sh", "check", "browser", "status", "wait", "console", "require",
      `return (async () => {\n${code}\n})();`
    );
  } catch (err) {
    send({ done: true, syntaxError: String((err && err.stack) || err).split("\n").slice(0, 4).join("\n") });
    return;
  }
  try {
    const result = await fn(
      read, write, writeFile, edit, patch, applyPatch, tryPatch, grep, ls, diff, symbols, flow,
      readMany, test, testAffected, probe, diagnostics, outline, definition, references,
      sh, check, browser, status, wait, scopedConsole, require
    );
    send({ done: true, result: result === undefined ? null : result, logs });
  } catch (err) {
    send({ done: true, error: String((err && err.stack) || err).split("\n").slice(0, 4).join("\n"), logs });
  }
}

lines.on("line", (line) => {
  let m;
  try {
    m = JSON.parse(line);
  } catch {
    return;
  }
  if (m.exec) {
    void runBody(m.code || "");
    return;
  }
  const w = waiting.get(m.id);
  if (!w) return;
  waiting.delete(m.id);
  if (m.error !== undefined) w.reject(new Error(m.error));
  else w.resolve(m.result);
});
lines.on("close", () => process.exit(0));
"#;

/// A warm sandboxed Node worker bound to one agent scope, reused across `run_code` calls.
struct WarmNode {
    child: tokio::process::Child,
    _guard: tm_harness::GroupGuard,
    stdin: tokio::process::ChildStdin,
    out: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    scratch: PathBuf,
}

impl WarmNode {
    fn spawn(node: &Path, bwrap: Option<&Path>) -> anyhow::Result<Self> {
        let scratch = std::env::temp_dir().join(format!("tm-code-{}-{}", std::process::id(), crate::secret::random_hex(6)?));
        std::fs::create_dir_all(&scratch)?;
        let script = scratch.join("worker.js");
        std::fs::write(&script, WORKER_SCRIPT)?;
        let node_args = [
            "--permission".to_string(),
            format!("--allow-fs-read={}", script.display()),
            "--disable-proto=throw".to_string(),
            script.display().to_string(),
        ];
        let mut cmd = match bwrap {
            Some(bwrap) => {
                let mut cmd = tokio::process::Command::new(bwrap);
                cmd.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--unshare-net", "--die-with-parent"])
                    .arg("--ro-bind")
                    .arg(&scratch)
                    .arg(&scratch)
                    .arg(node)
                    .args(&node_args);
                cmd
            }
            None => {
                let mut cmd = tokio::process::Command::new(node);
                cmd.args(&node_args);
                cmd
            }
        };
        cmd.current_dir(&scratch)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let (mut child, guard) = tm_harness::spawn_grouped(&mut cmd).context("could not start node")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let out = BufReader::new(child.stdout.take().context("no stdout")?).lines();
        Ok(WarmNode { child, _guard: guard, stdin, out, scratch })
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for WarmNode {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// What a page's scripts may touch, and through which gate.
struct Scope {
    cwd: PathBuf,
    gate: Arc<dyn PermissionGate>,
    /// The agent's sandboxed shell: what `sh` may write besides the worktree.
    shell: Option<tm_harness::ShellScope>,
    /// Read-only agents (planners, reviewers, verifiers) may only call read/search/diff/symbols/check.
    read_only: bool,
    /// Persistent warm Node sandbox process, lazily started on first `run_code` call.
    warm: tokio::sync::Mutex<Option<WarmNode>>,
}

/// Shared status-line MCP extension (O6): lets any agent (with or without Code Mode) publish a
/// concise one-line status shown on its card and mission node ahead of the derived headline.
pub struct StatusTools {
    live: Arc<crate::live::Live>,
    tags: Mutex<HashMap<String, Value>>,
}

impl StatusTools {
    pub fn new(live: Arc<crate::live::Live>) -> Self {
        Self { live, tags: Mutex::new(HashMap::new()) }
    }

    /// Record the full transcript agent tag for a grant scope (`master`, `a`, `a/part`, etc.).
    pub fn register_tag(&self, scope: &str, tag: Value) {
        self.tags.lock().unwrap().insert(scope.to_string(), tag);
    }

    /// Publish a one-line status update into the run's transcript for `grant`.
    pub fn set_status(&self, grant: &Grant, raw: &str) -> String {
        let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
        let cleaned: String = first_line
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(160)
            .collect();
        if cleaned.is_empty() {
            return String::new();
        }
        let scope = grant.agent();
        let tag = self.tags.lock().unwrap().get(&scope).cloned().unwrap_or_else(|| {
            let id = match scope.as_str() {
                "master" | "reviewer" | "verifier" => scope.clone(),
                other if other.starts_with("page:") || other.starts_with("unit:") => other.to_string(),
                other => format!("page:{other}"),
            };
            json!({ "id": id, "role": if scope == "master" { "master" } else { "worker" } })
        });
        self.live.push(&grant.run, &tag, "status", json!({ "text": cleaned }));
        cleaned
    }
}

#[async_trait::async_trait]
impl Extension for StatusTools {
    fn tools(&self, _grant: &Grant) -> Vec<Value> {
        vec![json!({
            "name": "set_status",
            "description": "Set a short one-line status describing what you are doing right now (shown on your agent card and mission node).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Short one-line status (max 160 chars)" }
                },
                "required": ["text"]
            }
        })]
    }

    async fn call(&self, grant: &Grant, name: &str, args: &Value) -> Option<Value> {
        if strip_dict_prefix(name) != "set_status" {
            return None;
        }
        let text = args["text"].as_str().unwrap_or("");
        let set = self.set_status(grant, text);
        Some(json!({
            "content": [{ "type": "text", "text": if set.is_empty() { "status cleared".to_string() } else { format!("status: {set}") } }],
            "isError": false
        }))
    }
}

#[derive(Clone)]
struct CodeRuntimeCtx {
    live: Arc<crate::live::Live>,
    records: crate::store::Records,
    status: Arc<StatusTools>,
}

pub struct CodeTools {
    node: PathBuf,
    bwrap: Option<PathBuf>,
    scopes: Mutex<HashMap<String, Arc<Scope>>>,
    browsers: Option<Arc<crate::browser::PageBrowsers>>,
    runtime: Mutex<Option<CodeRuntimeCtx>>,
    flow_root: Mutex<Option<PathBuf>>,
    project_root: Mutex<Option<PathBuf>>,
    /// How long one script may run.
    script_limit: Duration,
    /// How long one `sh` or `test` call may run.
    sh_limit: Duration,
}

impl CodeTools {
    /// `None` when Node is not installed: there is nothing to run scripts with.
    pub fn new(browsers: Option<Arc<crate::browser::PageBrowsers>>) -> Option<Arc<CodeTools>> {
        let node = tm_harness::sandbox::resolve_binary("node").ok()?;
        let bwrap = tm_harness::sandbox::resolve_binary("bwrap").ok().filter(|_| bwrap_works() && tm_harness::sandbox::bwrap_usable());
        Some(Arc::new(CodeTools {
            node,
            bwrap,
            scopes: Mutex::new(HashMap::new()),
            browsers,
            runtime: Mutex::new(None),
            flow_root: Mutex::new(None),
            project_root: Mutex::new(None),
            script_limit: Duration::from_secs(SCRIPT_SECS),
            sh_limit: Duration::from_secs(SH_SECS),
        }))
    }

    /// Set the run's live `.flow` tree root (e.g. `driver.tree`) used by `flow(op, args)`.
    pub fn set_flow_root(&self, root: PathBuf) {
        *self.flow_root.lock().unwrap() = Some(root);
    }

    /// Set the project root used to relativize paths written to `.flow/env.json`.
    pub fn set_project_root(&self, root: PathBuf) {
        *self.project_root.lock().unwrap() = Some(root);
    }

    /// Attach run-scoped `Live`, `Records`, and `StatusTools` so `status()` and `wait()` work.
    pub fn attach_runtime(&self, live: Arc<crate::live::Live>, records: crate::store::Records, status: Arc<StatusTools>) {
        *self.runtime.lock().unwrap() = Some(CodeRuntimeCtx { live, records, status });
    }

    /// [`CodeTools::new`] with the time limits from the settings (seconds; 0 keeps the default).
    pub fn with_limits(browsers: Option<Arc<crate::browser::PageBrowsers>>, script_secs: u64, sh_secs: u64) -> Option<Arc<CodeTools>> {
        let tools = Arc::try_unwrap(Self::new(browsers)?).ok()?;
        let pick = |secs: u64, default: u64| Duration::from_secs(if secs == 0 { default } else { secs });
        Some(Arc::new(CodeTools { script_limit: pick(script_secs, SCRIPT_SECS), sh_limit: pick(sh_secs, SH_SECS), ..tools }))
    }

    pub fn attach(&self, page: &str, cwd: &Path, gate: Arc<dyn PermissionGate>, shell: Option<tm_harness::ShellScope>) {
        self.scopes.lock().unwrap().insert(
            page.to_string(),
            Arc::new(Scope {
                cwd: cwd.to_path_buf(),
                gate,
                shell,
                read_only: false,
                warm: tokio::sync::Mutex::new(None),
            }),
        );
    }

    /// Attach a read-only scope (planners, reviewers, verifiers) so they can batch `readMany`, `grep`, `ls`, `symbols`, and `diff`.
    pub fn attach_readonly(&self, agent: &str, cwd: &Path, gate: Arc<dyn PermissionGate>) {
        self.scopes.lock().unwrap().insert(
            agent.to_string(),
            Arc::new(Scope {
                cwd: cwd.to_path_buf(),
                gate,
                shell: None,
                read_only: true,
                warm: tokio::sync::Mutex::new(None),
            }),
        );
    }

    /// Drop a single agent's scope (and tear down its warm Node sandbox process).
    pub fn detach(&self, agent: &str) {
        self.scopes.lock().unwrap().remove(agent);
    }

    /// Drop the scope of a page and of every part of it (`page/unit`).
    pub fn detach_page(&self, page: &str) {
        let prefix = format!("{page}/");
        self.scopes.lock().unwrap().retain(|key, _| key != page && !key.starts_with(&prefix));
    }

    /// Drop every attached scope and kill every warm Node sandbox process at once.
    pub fn detach_all(&self) {
        self.scopes.lock().unwrap().clear();
    }

    /// Run `code` for `grant`'s page in its warm sandboxed Node worker. The text is what the agent reads back.
    pub async fn run(&self, grant: &Grant, code: &str) -> anyhow::Result<String> {
        let scope = self.scopes.lock().unwrap().get(&grant.agent()).cloned().context("this agent has no worktree to run code in")?;
        let mut slot = scope.warm.lock().await;
        if slot.as_mut().is_none_or(|w| !w.is_alive()) {
            *slot = Some(WarmNode::spawn(&self.node, self.bwrap.as_deref())?);
        }
        let worker = slot.as_mut().expect("warm worker spawned");
        let result = tokio::time::timeout(self.script_limit, self.drive_warm(grant, &scope, worker, code)).await;
        match result {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(DriveError::Syntax(msg))) => {
                // Worker is still healthy and idle; keep it warm for the next call.
                bail!("the script had a syntax error: {msg}")
            }
            Ok(Err(DriveError::Broken(err))) => {
                *slot = None;
                Err(err)
            }
            Err(_) => {
                // Timeout: kill the stuck worker so the next call gets a clean one.
                *slot = None;
                bail!("the script ran longer than {}s and was stopped", self.script_limit.as_secs())
            }
        }
    }

    async fn drive_warm(&self, grant: &Grant, scope: &Scope, worker: &mut WarmNode, code: &str) -> Result<String, DriveError> {
        let req = json!({ "exec": true, "code": code });
        worker
            .stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .context("could not send script to warm node worker")
            .map_err(DriveError::Broken)?;
        let mut calls = 0usize;
        while let Some(line) = worker.out.next_line().await.map_err(|e| DriveError::Broken(e.into()))? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message["done"] == true {
                if let Some(syntax) = message["syntaxError"].as_str() {
                    return Err(DriveError::Syntax(syntax.to_string()));
                }
                return Ok(summary(&message));
            }
            calls += 1;
            let id = message["id"].clone();
            let reply = if calls > MAX_CALLS {
                json!({ "id": id, "error": format!("more than {MAX_CALLS} calls in one script") })
            } else {
                match self.op(grant, scope, message["op"].as_str().unwrap_or(""), &message["args"]).await {
                    Ok(result) => json!({ "id": id, "result": result }),
                    Err(err) => json!({ "id": id, "error": format!("{err:#}") }),
                }
            };
            worker
                .stdin
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .context("warm node worker stopped accepting replies")
                .map_err(DriveError::Broken)?;
        }
        Err(DriveError::Broken(anyhow::anyhow!("the script stopped before it finished")))
    }

    /// Run `command` with `sh -c` in the agent's sandbox: the worktree and its scratch are
    /// writable and nothing else. Refused without the sandbox, never run loose.
    async fn sandboxed(&self, scope: &Scope, command: &str) -> anyhow::Result<(Option<i32>, String)> {
        let (Some(_), Some(shell)) = (&self.bwrap, &scope.shell) else {
            bail!("refused: sh needs the sandbox, and this agent has none");
        };
        let sh = std::fs::canonicalize("/bin/sh").context("no /bin/sh")?;
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let sandbox = tm_harness::sandbox::Sandbox { home, state_dirs: vec![] };
        let args = sandbox.bwrap_args_with(&sh, &["-c".to_string(), command.to_string()], &scope.cwd, false, Some(shell));
        let mut cmd = tokio::process::Command::new("bwrap");
        cmd.args(args).current_dir(&scope.cwd).stdin(Stdio::null());
        let output = tokio::time::timeout(self.sh_limit, tm_harness::spawn_grouped(cmd.stdout(Stdio::piped()).stderr(Stdio::piped()))?.0.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("`{command}` ran longer than {}s", self.sh_limit.as_secs()))??;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((output.status.code(), text))
    }

    async fn allowed(&self, scope: &Scope, op: &str, kind: ToolKind, path: Option<&Path>, command: Option<&str>) -> anyhow::Result<()> {
        let request = PermissionRequest {
            tool: format!("run_code.{op}"),
            kind,
            paths: path.map(|p| vec![p.to_path_buf()]).unwrap_or_default(),
            command: command.map(str::to_string),
            url: None,
        };
        match scope.gate.decide_async(&request).await {
            Decision::Allow => Ok(()),
            Decision::Deny => bail!("refused: {op}{} is not allowed here", path.map(|p| format!(" {}", p.display())).unwrap_or_default()),
        }
    }

    async fn op(&self, grant: &Grant, scope: &Scope, op: &str, args: &Value) -> anyhow::Result<Value> {
        if scope.read_only
            && matches!(
                op,
                "write" | "edit" | "patch" | "applyPatch" | "tryPatch" | "sh" | "test" | "testAffected" | "browser"
            )
        {
            bail!("refused: `{op}` is disabled in a read-only scope (use read, readMany, grep, ls, diff, symbols, flow, probe.ast, check)");
        }
        let path = |key: &str| -> anyhow::Result<PathBuf> {
            let raw = args[key].as_str().context("a path is required")?;
            Ok(if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { scope.cwd.join(raw) })
        };
        match op {
            "read" => {
                let file = path("path")?;
                self.allowed(scope, op, ToolKind::Read, Some(&file), None).await?;
                let bytes = std::fs::read(&file).with_context(|| format!("cannot read {}", args["path"].as_str().unwrap_or("")))?;
                if bytes.len() > MAX_READ {
                    bail!("{} is larger than {} KB", args["path"].as_str().unwrap_or(""), MAX_READ / 1024);
                }
                Ok(json!(String::from_utf8_lossy(&bytes)))
            }
            "write" => {
                let raw = args["path"].as_str().context("a path is required")?;
                let clean = sanitize_file_path(raw)?;
                let file = if Path::new(&clean).is_absolute() {
                    PathBuf::from(&clean)
                } else {
                    scope.cwd.join(&clean)
                };
                check_policy_write(scope, &file)?;
                self.allowed(scope, op, ToolKind::Edit, Some(&file), None).await?;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&file, args["text"].as_str().unwrap_or(""))?;
                Ok(json!(true))
            }
            "edit" => {
                let file = path("path")?;
                check_policy_write(scope, &file)?;
                self.allowed(scope, op, ToolKind::Edit, Some(&file), None).await?;
                let find = args["find"].as_str().filter(|f| !f.is_empty()).context("`find` is required")?;
                let replace = args["replace"].as_str().unwrap_or("");
                let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", args["path"].as_str().unwrap_or("")))?;
                let count = text.matches(find).count();
                let all = args["all"].as_bool() == Some(true);
                if count == 0 {
                    bail!("`find` does not occur in {}", args["path"].as_str().unwrap_or(""));
                }
                if count > 1 && !all {
                    bail!("`find` occurs {count} times; make it unique or pass all = true");
                }
                let next = if all { text.replace(find, replace) } else { text.replacen(find, replace, 1) };
                std::fs::write(&file, next)?;
                Ok(json!(count))
            }
            "patch" => {
                let file = path("path")?;
                check_policy_write(scope, &file)?;
                self.allowed(scope, op, ToolKind::Edit, Some(&file), None).await?;
                let edits = args["edits"].as_array().context("`edits` must be an array of {find, replace, all?}")?;
                if edits.is_empty() {
                    return Ok(json!(0));
                }
                let mut text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {}", args["path"].as_str().unwrap_or("")))?;
                let mut applied = 0usize;
                for (i, item) in edits.iter().enumerate() {
                    let find = item["find"].as_str().filter(|f| !f.is_empty()).with_context(|| format!("edits[{i}].find is required"))?;
                    let replace = item["replace"].as_str().unwrap_or("");
                    let all = item["all"].as_bool() == Some(true);
                    let count = text.matches(find).count();
                    if count == 0 {
                        bail!("edits[{i}].find does not occur in {}", args["path"].as_str().unwrap_or(""));
                    }
                    if count > 1 && !all {
                        bail!("edits[{i}].find occurs {count} times; make it unique or set all: true");
                    }
                    text = if all { text.replace(find, replace) } else { text.replacen(find, replace, 1) };
                    applied += count;
                }
                std::fs::write(&file, text)?;
                Ok(json!(applied))
            }
            "applyPatch" => {
                let raw = args["diff"].as_str().filter(|d| !d.trim().is_empty()).context("`diff` is required")?;
                let patches = parse_unified_diff(raw)?;
                if patches.is_empty() {
                    bail!("no unified diff hunks found in `diff`");
                }
                // Validate permission and apply every file in memory first so a refused path or
                // mismatched hunk never partially writes earlier files.
                let mut staged: Vec<(PathBuf, Option<String>, String)> = Vec::new();
                let mut total_hunks = 0usize;
                for file_patch in &patches {
                    let rel = file_patch.target_path();
                    let full = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { scope.cwd.join(rel) };
                    check_policy_write(scope, &full)?;
                    self.allowed(scope, op, ToolKind::Edit, Some(&full), None).await?;
                    if file_patch.is_delete() {
                        let existing = std::fs::read_to_string(&full).with_context(|| format!("cannot read {rel} to delete"))?;
                        let _ = apply_file_hunks(rel, &existing, &file_patch.hunks)?;
                        staged.push((full, None, rel.to_string()));
                    } else {
                        let existing = if file_patch.is_create() && !full.exists() {
                            String::new()
                        } else {
                            std::fs::read_to_string(&full).with_context(|| format!("cannot read {rel}"))?
                        };
                        let updated = apply_file_hunks(rel, &existing, &file_patch.hunks)?;
                        staged.push((full, Some(updated), rel.to_string()));
                    }
                    total_hunks += file_patch.hunks.len();
                }
                let mut touched = Vec::with_capacity(staged.len());
                for (full, content, rel) in staged {
                    match content {
                        Some(text) => {
                            if let Some(dir) = full.parent() {
                                std::fs::create_dir_all(dir)?;
                            }
                            std::fs::write(&full, text)?;
                        }
                        None => {
                            if full.exists() {
                                std::fs::remove_file(&full)?;
                            }
                        }
                    }
                    touched.push(rel);
                }
                Ok(json!({ "files": touched, "hunks": total_hunks }))
            }
            "diff" => {
                self.allowed(scope, op, ToolKind::Read, Some(&scope.cwd), None).await?;
                let mut cmd = std::process::Command::new("git");
                cmd.arg("-C").arg(&scope.cwd).args(["diff", "--no-color"]);
                if let Some(glob) = args["glob"].as_str().filter(|g| !g.is_empty()) {
                    cmd.arg("--").arg(glob);
                }
                let out = cmd.output().context("failed to run git diff")?;
                let text = String::from_utf8_lossy(&out.stdout);
                Ok(json!(tail(&text, 16_000)))
            }
            "symbols" => {
                self.allowed(scope, op, ToolKind::Read, Some(&scope.cwd), None).await?;
                let syms = crate::repomap::extract_symbols(&scope.cwd, args["glob"].as_str())?;
                Ok(json!(syms))
            }
            "flow" => {
                self.allowed(scope, op, ToolKind::Read, Some(&scope.cwd), None).await?;
                let sub_op = args["op"].as_str().unwrap_or("").trim().to_string();
                if sub_op.is_empty() {
                    bail!("flow(op, args) requires `op` (`find`, `node`, `neighbors`, or `source`)");
                }
                let sub_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
                let root = self
                    .flow_root
                    .lock()
                    .unwrap()
                    .clone()
                    .filter(|r| r.join(tm_flow::FLOW_DIR).is_dir())
                    .unwrap_or_else(|| scope.cwd.clone());
                let out = tokio::task::spawn_blocking(move || {
                    if !root.join(tm_flow::FLOW_DIR).is_dir() {
                        let _ = tm_flow::build(&root);
                    }
                    tm_flow::execute_op(&root, &sub_op, &sub_args)
                })
                .await
                .context("flow query task panicked")??;
                Ok(out.json)
            }
            "ls" | "grep" => {
                self.allowed(scope, op, ToolKind::Read, Some(&scope.cwd), None).await?;
                let files = files(&scope.cwd, args["glob"].as_str())?;
                if op == "ls" {
                    return Ok(json!(files));
                }
                let wanted = args["text"].as_str().filter(|t| !t.is_empty()).context("`text` is required")?;
                let mut hits = Vec::new();
                for file in files {
                    let Ok(text) = std::fs::read_to_string(scope.cwd.join(&file)) else { continue };
                    for (n, line) in text.lines().enumerate() {
                        if line.contains(wanted) {
                            hits.push(format!("{file}:{}: {}", n + 1, line.trim().chars().take(200).collect::<String>()));
                            if hits.len() >= 200 {
                                return Ok(json!(hits));
                            }
                        }
                    }
                }
                Ok(json!(hits))
            }
            "sh" => {
                let command = args["command"].as_str().context("`command` is required")?;
                self.allowed(scope, op, ToolKind::Execute, None, Some(command)).await?;
                let (code, text) = self.sandboxed(scope, command).await?;
                Ok(json!({ "code": code, "output": tail(&text, 8000) }))
            }
            "test" => {
                self.allowed(scope, op, ToolKind::Execute, None, Some("the project's tests")).await?;
                let (venv, steps) = select_test_steps(scope);
                if !steps.iter().any(|step| !step.setup) {
                    return Ok(json!({ "ok": true, "ran": false, "failed": [], "output": "no tests were found to run" }));
                }
                let command = test_command(&steps);
                let (code, text) = self.sandboxed(scope, &command).await?;
                if code == Some(0) {
                    let flow_root = self.flow_root.lock().unwrap().clone();
                    let rel_base = self
                        .project_root
                        .lock()
                        .unwrap()
                        .clone()
                        .or_else(|| flow_root.clone());
                    if let (Some(root), Some(base)) = (flow_root, rel_base)
                        && tm_flow::read_env(&root).ok().flatten().is_none()
                        && let Some(facts) = crate::runtime::env_facts_from_steps_rel(&steps, &venv, &base)
                    {
                        let _ = tm_flow::write_env(&root, &facts);
                    }
                }
                Ok(json!({ "ok": code == Some(0), "ran": true, "failed": failed_tests(&text), "output": tail(&text, 6000) }))
            }
            "readMany" => {
                self.allowed(scope, op, ToolKind::Read, Some(&scope.cwd), None).await?;
                let mut out = serde_json::Map::new();
                let mut total = 0usize;
                let globs: Vec<String> = match &args["globs"] {
                    Value::Array(items) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                    Value::String(one) => vec![one.clone()],
                    _ => Vec::new(),
                };
                for glob in globs {
                    for file in files(&scope.cwd, Some(&glob))? {
                        if out.contains_key(&file) {
                            continue;
                        }
                        let full = scope.cwd.join(&file);
                        self.allowed(scope, op, ToolKind::Read, Some(&full), None).await?;
                        let Ok(bytes) = std::fs::read(&full) else { continue };
                        if total + bytes.len() > MAX_READ {
                            out.insert(file, json!(null));
                            continue;
                        }
                        total += bytes.len();
                        out.insert(file, json!(String::from_utf8_lossy(&bytes)));
                    }
                }
                Ok(Value::Object(out))
            }
            "check" => {
                self.allowed(scope, op, ToolKind::Read, Some(&scope.cwd), None).await?;
                let files = files(&scope.cwd, None)?;
                let mut problems: Vec<String> = crate::checks::html_sanity(&scope.cwd, &files)
                    .into_iter()
                    .chain(crate::checks::html_quality(&scope.cwd, &files))
                    .map(|(file, problem)| format!("{file}: {problem}"))
                    .collect();
                for file in files.iter().filter(|f| f.ends_with(".json")) {
                    if let Ok(text) = std::fs::read_to_string(scope.cwd.join(file))
                        && let Err(err) = serde_json::from_str::<Value>(&text)
                    {
                        problems.push(format!("{file}: not valid JSON ({err})"));
                    }
                }
                Ok(json!(problems))
            }
            "browser" => {
                let browsers = self.browsers.as_ref().context("this agent has no browser")?;
                let action = args["action"].as_str().context("`action` is required")?;
                let reply = browsers.call(grant, &format!("browser_{action}"), &args["args"]).await.context("no such browser action")?;
                if reply["isError"] == true {
                    bail!("{}", reply["content"][0]["text"].as_str().unwrap_or("the browser refused"));
                }
                let first = &reply["content"][0];
                Ok(if first["type"] == "image" { json!("(a screenshot was taken; call browser_screenshot yourself to see it)") } else { first["text"].clone() })
            }
            "tryPatch" => {
                let raw = args["diff"].as_str().filter(|d| !d.trim().is_empty()).context("`diff` is required")?;
                let patches = match parse_unified_diff(raw) {
                    Ok(p) if !p.is_empty() => p,
                    Ok(_) => return Ok(json!({ "applied": false, "error": "no unified diff hunks found", "files": [], "diagnostics": [] })),
                    Err(err) => return Ok(json!({ "applied": false, "error": format!("{err:#}"), "files": [], "diagnostics": [] })),
                };
                let policy = crate::policy::Policy::load(&scope.cwd);
                let mut staged: Vec<(PathBuf, Option<String>, String)> = Vec::new();
                for file_patch in &patches {
                    let rel = file_patch.target_path();
                    if tm_flow::is_flow_path(rel) {
                        return Ok(json!({
                            "applied": false,
                            "error": format!("policy denied write to `{rel}` (`.flow/` is managed by Taskmaster)"),
                            "files": [],
                            "diagnostics": [],
                        }));
                    }
                    if let crate::policy::PathVerdict::Denied(glob) = policy.check_path(rel) {
                        return Ok(json!({
                            "applied": false,
                            "error": format!("policy denied write to `{rel}` (matched `{glob}`)"),
                            "files": [],
                            "diagnostics": [],
                        }));
                    }
                    let full = if Path::new(rel).is_absolute() { PathBuf::from(rel) } else { scope.cwd.join(rel) };
                    if let Err(err) = self.allowed(scope, "tryPatch", ToolKind::Edit, Some(&full), None).await {
                        return Ok(json!({
                            "applied": false,
                            "error": format!("{err:#}"),
                            "files": [],
                            "diagnostics": [],
                        }));
                    }
                    let next = if file_patch.is_delete() {
                        let existing = match std::fs::read_to_string(&full) {
                            Ok(t) => t,
                            Err(err) => return Ok(json!({ "applied": false, "error": format!("cannot read {rel}: {err}"), "files": [], "diagnostics": [] })),
                        };
                        if let Err(err) = apply_file_hunks(rel, &existing, &file_patch.hunks) {
                            return Ok(json!({ "applied": false, "error": format!("{err:#}"), "files": [], "diagnostics": [] }));
                        }
                        None
                    } else {
                        let existing = if file_patch.is_create() && !full.exists() {
                            String::new()
                        } else {
                            match std::fs::read_to_string(&full) {
                                Ok(t) => t,
                                Err(err) => return Ok(json!({ "applied": false, "error": format!("cannot read {rel}: {err}"), "files": [], "diagnostics": [] })),
                            }
                        };
                        match apply_file_hunks(rel, &existing, &file_patch.hunks) {
                            Ok(updated) => Some(updated),
                            Err(err) => return Ok(json!({ "applied": false, "error": format!("{err:#}"), "files": [], "diagnostics": [] })),
                        }
                    };
                    staged.push((full, next, rel.to_string()));
                }
                let mut touched = Vec::with_capacity(staged.len());
                for (full, content, rel) in staged {
                    match content {
                        Some(text) => {
                            if let Some(dir) = full.parent() {
                                std::fs::create_dir_all(dir)?;
                            }
                            std::fs::write(&full, text)?;
                        }
                        None => {
                            if full.exists() {
                                std::fs::remove_file(&full)?;
                            }
                        }
                    }
                    touched.push(rel);
                }
                // Run fast static checks immediately on touched files in the same turn.
                let mut diagnostics: Vec<String> = crate::checks::html_sanity(&scope.cwd, &touched)
                    .into_iter()
                    .chain(crate::checks::html_quality(&scope.cwd, &touched))
                    .map(|(file, problem)| format!("{file}: {problem}"))
                    .collect();
                for file in touched.iter().filter(|f| f.ends_with(".json")) {
                    if let Ok(text) = std::fs::read_to_string(scope.cwd.join(file))
                        && let Err(err) = serde_json::from_str::<Value>(&text)
                    {
                        diagnostics.push(format!("{file}: not valid JSON ({err})"));
                    }
                }
                Ok(json!({ "applied": true, "files": touched, "diagnostics": diagnostics }))
            }
            "testAffected" => {
                self.allowed(scope, op, ToolKind::Execute, None, Some("affected project tests")).await?;
                let requested: Vec<String> = args["files"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                let changed: Vec<String> = if !requested.is_empty() {
                    requested
                } else {
                    let out = std::process::Command::new("git")
                        .arg("-C")
                        .arg(&scope.cwd)
                        .args(["status", "--porcelain"])
                        .output()
                        .ok();
                    out.map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .filter_map(|l| l.get(3..))
                            .map(|s| s.trim().to_string())
                            .collect()
                    })
                    .unwrap_or_default()
                };
                let all_files = files(&scope.cwd, None).unwrap_or_default();
                let stems: Vec<String> = changed
                    .iter()
                    .filter_map(|p| Path::new(p).file_stem().and_then(|s| s.to_str()).map(str::to_ascii_lowercase))
                    .collect();
                let matched_tests: Vec<String> = all_files
                    .into_iter()
                    .filter(|f| {
                        let lower = f.to_ascii_lowercase();
                        let looks_test = lower.contains(".test.")
                            || lower.contains(".spec.")
                            || lower.contains("test_")
                            || lower.ends_with("_test.rs");
                        looks_test && (stems.is_empty() || stems.iter().any(|st| lower.contains(st)))
                    })
                    .take(12)
                    .collect();
                let (_, steps) = select_test_steps(scope);
                if !steps.iter().any(|step| !step.setup) {
                    return Ok(json!({
                        "ok": true,
                        "ran": false,
                        "affectedFiles": changed,
                        "testFiles": matched_tests,
                        "failed": [],
                        "output": "no tests were found to run"
                    }));
                }
                let command = test_command(&steps);
                let (code, text) = self.sandboxed(scope, &command).await?;
                Ok(json!({
                    "ok": code == Some(0),
                    "ran": true,
                    "affectedFiles": changed,
                    "testFiles": matched_tests,
                    "failed": failed_tests(&text),
                    "output": tail(&text, 6000)
                }))
            }
            "probeAst" => {
                let file = path("path")?;
                self.allowed(scope, op, ToolKind::Read, Some(&file), None).await?;
                let rel = args["path"].as_str().unwrap_or("");
                let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {rel}"))?;
                let mut exports = Vec::new();
                let mut imports = Vec::new();
                for (idx, line) in text.lines().enumerate() {
                    let t = line.trim();
                    if t.starts_with("export ")
                        || t.starts_with("pub fn ")
                        || t.starts_with("pub async fn ")
                        || t.starts_with("pub struct ")
                        || t.starts_with("pub enum ")
                        || t.starts_with("pub trait ")
                        || t.starts_with("def ")
                        || t.starts_with("class ")
                    {
                        exports.push(json!({ "line": idx + 1, "sig": t.chars().take(120).collect::<String>() }));
                    } else if t.starts_with("import ") || t.starts_with("use ") || t.starts_with("from ") {
                        imports.push(json!({ "line": idx + 1, "raw": t.chars().take(120).collect::<String>() }));
                    }
                }
                Ok(json!({
                    "path": rel,
                    "lines": text.lines().count(),
                    "exports": exports,
                    "imports": imports,
                }))
            }
            "diagnostics" => {
                self.allowed(scope, op, ToolKind::Read, None, None).await?;
                let mut files: Vec<String> = args["files"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                if files.is_empty() {
                    files = crate::lsp::workspace_source_files(&scope.cwd).into_iter().take(25).collect();
                }
                let mgr = crate::lsp::LspManager::new();
                let diags = mgr.diagnostics_for_files(&scope.cwd, &files);
                mgr.shutdown_root(&scope.cwd);
                Ok(serde_json::to_value(diags)?)
            }
            "outline" => {
                let file = path("path")?;
                self.allowed(scope, op, ToolKind::Read, Some(&file), None).await?;
                let rel = args["path"].as_str().unwrap_or("");
                let syms = crate::lsp::outline_file(&scope.cwd, rel);
                Ok(serde_json::to_value(syms)?)
            }
            "definition" => {
                self.allowed(scope, op, ToolKind::Read, None, None).await?;
                let sym = args["symbol"].as_str().unwrap_or("").trim();
                let defs = crate::lsp::find_definitions(&scope.cwd, sym);
                Ok(serde_json::to_value(defs)?)
            }
            "references" => {
                self.allowed(scope, op, ToolKind::Read, None, None).await?;
                let sym = args["symbol"].as_str().unwrap_or("").trim();
                let refs = crate::lsp::find_references(&scope.cwd, sym, 60);
                Ok(serde_json::to_value(refs)?)
            }
            "status" => {
                let text = args["text"].as_str().unwrap_or("");
                let ctx = self.runtime.lock().unwrap().clone();
                let set = match ctx {
                    Some(ctx) => ctx.status.set_status(grant, text),
                    None => text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect(),
                };
                Ok(json!({ "ok": true, "status": set }))
            }
            "wait" => self.wait_op(grant, scope, args).await,
            other => bail!("no function `{other}`"),
        }
    }

    /// H4: `wait({ url, port, log, agent, timeout })` — poll a local dev server, TCP port,
    /// log file/transcript line, or sibling unit state without sleep loops.
    async fn wait_op(&self, grant: &Grant, scope: &Scope, args: &Value) -> anyhow::Result<Value> {
        let timeout_ms = args
            .get("timeoutMs")
            .or_else(|| args.get("timeout_ms"))
            .and_then(|v| v.as_u64())
            .or_else(|| {
                args.get("timeout").and_then(|v| {
                    if let Some(secs) = v.as_f64() {
                        // Treat numbers <= 600 as seconds, larger values as milliseconds.
                        if secs <= 600.0 { Some((secs.max(0.05) * 1000.0) as u64) } else { Some(secs as u64) }
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(30_000)
            .clamp(50, 120_000);
        let url = args.get("url").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let port = args.get("port").and_then(|v| v.as_u64()).and_then(|p| u16::try_from(p).ok());
        let agent = args.get("agent").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let log_spec: Option<(Option<PathBuf>, String)> = match args.get("log") {
            Some(Value::String(pat)) if !pat.is_empty() => {
                let file = args
                    .get("file")
                    .or_else(|| args.get("path"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|raw| if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { scope.cwd.join(raw) });
                Some((file, pat.clone()))
            }
            Some(Value::Object(obj)) => {
                let pat = obj
                    .get("pattern")
                    .or_else(|| obj.get("contains"))
                    .or_else(|| obj.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if pat.is_empty() {
                    None
                } else {
                    let file = obj
                        .get("path")
                        .or_else(|| obj.get("file"))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|raw| if Path::new(raw).is_absolute() { PathBuf::from(raw) } else { scope.cwd.join(raw) });
                    Some((file, pat))
                }
            }
            _ => None,
        };

        if url.is_none() && port.is_none() && agent.is_none() && log_spec.is_none() {
            bail!("wait requires at least one of `url`, `port`, `log`, or `agent`");
        }
        if let Some((Some(ref log_file), _)) = log_spec {
            self.allowed(scope, "wait", ToolKind::Read, Some(log_file), None).await?;
        }
        if let Some(ref u) = url {
            validate_local_wait_url(u)?;
        }

        let started = std::time::Instant::now();
        let deadline = started + Duration::from_millis(timeout_ms);
        let ctx = self.runtime.lock().unwrap().clone();
        loop {
            let mut all_ready = true;
            let mut detail = serde_json::Map::new();

            if let Some(p) = port {
                let open = tokio::time::timeout(Duration::from_millis(250), tokio::net::TcpStream::connect(("127.0.0.1", p)))
                    .await
                    .is_ok_and(|r| r.is_ok());
                if open {
                    detail.insert("port".into(), json!(p));
                } else {
                    all_ready = false;
                }
            }

            if all_ready
                && let Some(ref u) = url
            {
                match probe_local_http(u).await {
                    Some(status) if (100..500).contains(&status) => {
                        detail.insert("url".into(), json!(u));
                        detail.insert("status".into(), json!(status));
                    }
                    _ => {
                        all_ready = false;
                    }
                }
            }

            if all_ready
                && let Some((ref log_file, ref pattern)) = log_spec
            {
                let matched_line = if let Some(file) = log_file {
                    std::fs::read_to_string(file)
                        .ok()
                        .and_then(|text| text.lines().find(|l| l.contains(pattern.as_str())).map(str::to_string))
                } else if let Some(ref ctx) = ctx {
                    let (_, entries) = ctx.live.read(&grant.run, 0);
                    entries.iter().rev().find_map(|e| {
                        let s = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        s.contains(pattern.as_str()).then(|| s.chars().take(240).collect::<String>())
                    })
                } else {
                    None
                };
                if let Some(line) = matched_line {
                    detail.insert("log".into(), json!(line));
                } else {
                    all_ready = false;
                }
            }

            if all_ready
                && let Some(ref target) = agent
            {
                let page_id = target
                    .strip_prefix("page:")
                    .or_else(|| target.strip_prefix("unit:"))
                    .unwrap_or(target.as_str());
                let state = ctx.as_ref().and_then(|ctx| {
                    let events = ctx.records.events(&grant.run).ok()?;
                    let run = tm_core::Run::replay(1, events);
                    run.pages.get(page_id).map(|p| p.state)
                });
                match state {
                    Some(tm_core::PageState::Passed) => {
                        detail.insert("agent".into(), json!(page_id));
                        detail.insert("state".into(), json!("passed"));
                    }
                    Some(tm_core::PageState::Failed) => {
                        return Ok(json!({
                            "ok": false,
                            "timeout": false,
                            "agent": page_id,
                            "state": "failed",
                            "waited_ms": started.elapsed().as_millis() as u64,
                        }));
                    }
                    _ => {
                        all_ready = false;
                    }
                }
            }

            if all_ready {
                detail.insert("ok".into(), json!(true));
                detail.insert("waited_ms".into(), json!(started.elapsed().as_millis() as u64));
                return Ok(Value::Object(detail));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(json!({
                    "ok": false,
                    "timeout": true,
                    "waited_ms": started.elapsed().as_millis() as u64,
                    "reason": "condition did not become ready before timeout"
                }));
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }
}

fn validate_local_wait_url(url: &str) -> anyhow::Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("wait({{ url }}) only supports local http:// URLs"))?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(inner) = host_port.strip_prefix('[') {
        let (h, tail) = inner.split_once(']').ok_or_else(|| anyhow::anyhow!("invalid IPv6 host in `{url}`"))?;
        let p = tail.strip_prefix(':').map(|s| s.parse::<u16>()).transpose()?.unwrap_or(80);
        (h.to_string(), p)
    } else if let Some((h, p)) = host_port.rsplit_once(':') {
        (h.to_string(), p.parse::<u16>().with_context(|| format!("invalid port in `{url}`"))?)
    } else {
        (host_port.to_string(), 80)
    };
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1") {
        bail!("wait({{ url }}) only permits loopback addresses (got `{host}`)");
    }
    Ok((host, port, path.to_string()))
}

async fn probe_local_http(url: &str) -> Option<u16> {
    let (host, port, path) = validate_local_wait_url(url).ok()?;
    let addr = if host == "localhost" { "127.0.0.1".to_string() } else { host.clone() };
    let mut stream = tokio::time::timeout(
        Duration::from_millis(300),
        tokio::net::TcpStream::connect((addr.as_str(), port)),
    )
    .await
    .ok()?
    .ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.ok()?;
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    tokio::time::timeout(Duration::from_millis(400), reader.read_line(&mut first)).await.ok()?.ok()?;
    first.split_whitespace().nth(1)?.parse::<u16>().ok()
}

/// Project files under `cwd` (tracked and new, not ignored), filtered by `glob`.
/// One shell line that runs the project's test steps: setup steps may fail and are reported,
/// the checks must all pass.
fn select_test_steps(scope: &Scope) -> (PathBuf, Vec<crate::runtime::Step>) {
    let fallback_venv = scope
        .shell
        .as_ref()
        .and_then(|s| s.writable.first())
        .map(|scratch| scratch.join("venv"))
        .unwrap_or_else(|| scope.cwd.join(".venv"));
    // Prefer an already-prepared shared read-only venv mounted into the worker's sandbox
    // when it needs zero setup writes; otherwise fall back to the writable scratch venv.
    let shared_venv = scope
        .shell
        .as_ref()
        .and_then(|s| s.readable.iter().find(|d| d.join("pyvenv.cfg").is_file()))
        .cloned();
    match shared_venv {
        Some(shared) => {
            let candidate = crate::runtime::steps(&scope.cwd, &shared);
            if candidate.iter().any(|s| !s.setup) && !candidate.iter().any(|s| s.setup) {
                (shared, candidate)
            } else {
                let steps = crate::runtime::steps(&scope.cwd, &fallback_venv);
                (fallback_venv, steps)
            }
        }
        None => {
            let steps = crate::runtime::steps(&scope.cwd, &fallback_venv);
            (fallback_venv, steps)
        }
    }
}

fn test_command(steps: &[crate::runtime::Step]) -> String {
    let line = |step: &crate::runtime::Step| {
        let mut parts: Vec<String> = step.env.iter().map(|(k, v)| format!("{k}={}", quote(v))).collect();
        parts.push(quote(&step.program));
        parts.extend(step.args.iter().map(|a| quote(a)));
        parts.join(" ")
    };
    let mut out = Vec::new();
    for step in steps {
        if step.setup {
            out.push(format!("{{ {} >/dev/null 2>&1 || echo 'setup failed: {}'; }}", line(step), step.line().replace('\'', "")));
        } else {
            out.push(line(step));
        }
    }
    // Setup steps are joined with `;`, so a failed one does not stop the tests.
    let mut command = String::new();
    for (i, part) in out.iter().enumerate() {
        if i > 0 {
            command.push_str(if steps[i - 1].setup { "; " } else { " && " });
        }
        command.push_str(part);
    }
    command
}

fn quote(text: &str) -> String {
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+".contains(c)) {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The failing tests a runner named: pytest's `FAILED tests/x.py::t`, cargo's
/// `test a::b ... FAILED`, and node's `✗`/`not ok` lines.
fn failed_tests(output: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        let name = if let Some(rest) = line.strip_prefix("FAILED ") {
            Some(rest.split(" - ").next().unwrap_or(rest).to_string())
        } else if line.starts_with("test ") && line.ends_with("... FAILED") {
            Some(line.trim_start_matches("test ").trim_end_matches("... FAILED").trim().to_string())
        } else {
            line.strip_prefix("not ok ").map(str::to_string)
        };
        if let Some(name) = name
            && !out.contains(&name)
            && out.len() < 50
        {
            out.push(name);
        }
    }
    out
}

fn files(cwd: &Path, glob: Option<&str>) -> anyhow::Result<Vec<String>> {
    let output = std::process::Command::new("git").arg("-C").arg(cwd).args(["ls-files", "--cached", "--others", "--exclude-standard"]).output()?;
    let all = String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect::<Vec<_>>();
    let Some(glob) = glob.filter(|g| !g.is_empty()) else { return Ok(all) };
    let matcher = globset::Glob::new(glob).with_context(|| format!("`{glob}` is not a glob"))?.compile_matcher();
    Ok(all.into_iter().filter(|f| matcher.is_match(f)).collect())
}

fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &text[cut..])
}

/// The script's answer as the agent reads it: its log lines, then its result or error.
fn summary(done: &Value) -> String {
    let mut out = String::new();
    for line in done["logs"].as_array().into_iter().flatten() {
        out.push_str(line.as_str().unwrap_or(""));
        out.push('\n');
    }
    if let Some(error) = done["error"].as_str() {
        out.push_str(&format!("Error: {error}"));
    } else {
        let result = &done["result"];
        let shown = match result {
            Value::Null => "done".to_string(),
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        out.push_str(&shown);
    }
    tail(&out, MAX_REPLY)
}

/// Internal drive failure distinction: syntax errors leave the warm Node worker healthy;
/// I/O or protocol errors tear down and respawn it on the next call.
enum DriveError {
    Syntax(String),
    Broken(anyhow::Error),
}

struct FilePatch {
    old_path: String,
    new_path: String,
    hunks: Vec<Hunk>,
}

impl FilePatch {
    fn is_create(&self) -> bool {
        self.old_path == "/dev/null"
    }

    fn is_delete(&self) -> bool {
        self.new_path == "/dev/null"
    }

    fn target_path(&self) -> &str {
        if self.is_delete() { &self.old_path } else { &self.new_path }
    }
}

struct Hunk {
    old_start: usize,
    lines: Vec<(char, String)>,
}

fn strip_diff_path(raw: &str) -> String {
    let trimmed = raw.split('\t').next().unwrap_or(raw).trim();
    if trimmed == "/dev/null" {
        return "/dev/null".to_string();
    }
    trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed)
        .to_string()
}

fn parse_hunk_header(line: &str) -> anyhow::Result<usize> {
    let rest = line.strip_prefix("@@ ").context("expected hunk header")?;
    let range = rest.split(" @@").next().context("malformed hunk header")?;
    let old_part = range.split_whitespace().next().context("missing old range in hunk header")?;
    let old_nums = old_part.strip_prefix('-').context("missing '-' in hunk header")?;
    let start_str = old_nums.split(',').next().unwrap_or(old_nums);
    let start: usize = start_str.parse().with_context(|| format!("invalid hunk start line `{start_str}`"))?;
    Ok(start)
}

fn parse_unified_diff(diff: &str) -> anyhow::Result<Vec<FilePatch>> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut i = 0;
    let mut patches = Vec::new();
    while i < lines.len() {
        if lines[i].starts_with("--- ") {
            let old_path = strip_diff_path(&lines[i][4..]);
            i += 1;
            if i >= lines.len() || !lines[i].starts_with("+++ ") {
                bail!("malformed unified diff: expected `+++ ` after `--- `");
            }
            let new_path = strip_diff_path(&lines[i][4..]);
            i += 1;
            let mut hunks = Vec::new();
            while i < lines.len() {
                if lines[i].starts_with("--- ") || lines[i].starts_with("diff --git ") {
                    break;
                }
                if lines[i].starts_with("@@ ") {
                    let old_start = parse_hunk_header(lines[i])?;
                    i += 1;
                    let mut hunk_lines = Vec::new();
                    while i < lines.len() {
                        let line = lines[i];
                        if line.starts_with("@@ ") || line.starts_with("--- ") || line.starts_with("diff --git ") {
                            break;
                        }
                        if line.starts_with("\\ No newline") {
                            i += 1;
                            continue;
                        }
                        let mut chars = line.chars();
                        match chars.next() {
                            Some(tag @ (' ' | '-' | '+')) => hunk_lines.push((tag, chars.as_str().to_string())),
                            None => hunk_lines.push((' ', String::new())),
                            Some(_) => break,
                        }
                        i += 1;
                    }
                    hunks.push(Hunk { old_start, lines: hunk_lines });
                } else {
                    i += 1;
                }
            }
            if !hunks.is_empty() {
                patches.push(FilePatch { old_path, new_path, hunks });
            }
        } else {
            i += 1;
        }
    }
    Ok(patches)
}

fn apply_file_hunks(rel: &str, existing: &str, hunks: &[Hunk]) -> anyhow::Result<String> {
    let trailing_newline = existing.ends_with('\n') || existing.is_empty();
    let mut lines: Vec<String> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.lines().map(str::to_string).collect()
    };
    let mut offset: isize = 0;

    for (hunk_idx, hunk) in hunks.iter().enumerate() {
        let expected: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|(tag, _)| *tag == ' ' || *tag == '-')
            .map(|(_, s)| s.as_str())
            .collect();
        let replacement: Vec<String> = hunk
            .lines
            .iter()
            .filter(|(tag, _)| *tag == ' ' || *tag == '+')
            .map(|(_, s)| s.clone())
            .collect();

        let base = if hunk.old_start == 0 {
            0isize
        } else {
            (hunk.old_start as isize - 1) + offset
        };
        let target_idx = find_hunk_pos(&lines, &expected, base).with_context(|| {
            format!("hunk #{} did not match `{rel}` near line {}", hunk_idx + 1, hunk.old_start)
        })?;
        lines.splice(target_idx..target_idx + expected.len(), replacement.iter().cloned());
        offset += replacement.len() as isize - expected.len() as isize;
    }

    let mut out = lines.join("\n");
    if !out.is_empty() && trailing_newline {
        out.push('\n');
    }
    Ok(out)
}

fn find_hunk_pos(lines: &[String], expected: &[&str], hint: isize) -> Option<usize> {
    if expected.is_empty() {
        return Some(hint.clamp(0, lines.len() as isize) as usize);
    }
    if expected.len() > lines.len() {
        return None;
    }
    let max_start = lines.len() - expected.len();
    let center = hint.clamp(0, max_start as isize) as usize;
    let matches_at = |pos: usize| expected.iter().enumerate().all(|(k, want)| lines[pos + k] == *want);
    if matches_at(center) {
        return Some(center);
    }
    // Search outward up to ±50 lines in case earlier edits shifted line numbers.
    for delta in 1..=50usize {
        if let Some(pos) = center.checked_sub(delta)
            && matches_at(pos)
        {
            return Some(pos);
        }
        let pos = center + delta;
        if pos <= max_start && matches_at(pos) {
            return Some(pos);
        }
    }
    None
}

/// Whether bubblewrap can make a sandbox here; some containers forbid user namespaces.
fn bwrap_works() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--unshare-net", "--die-with-parent", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Strip any known Dictionary MCP server prefix from a tool name so aliases work uniformly.
pub(crate) fn strip_dict_prefix(name: &str) -> &str {
    for prefix in ["mcp_dictionary_", "mcp__dictionary__", "dictionary__", "dictionary_", "dict__", "taskmaster__"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            return rest;
        }
    }
    name
}

/// Accept both the canonical MCP name (`run_code`) and harness-prefixed aliases
/// (such as `mcp_dictionary_run_code` on Antigravity or `mcp__dictionary__run_code` on Claude).
fn is_run_code_tool(name: &str) -> bool {
    strip_dict_prefix(name) == "run_code"
}

/// Accept `writeFile` / `write_file` (with or without a Dictionary prefix) taking `{path, content}`
/// as separate MCP arguments so raw files bypass JS string escaping.
fn is_write_file_tool(name: &str) -> bool {
    matches!(strip_dict_prefix(name), "writeFile" | "write_file")
}

/// Refuse writes into Taskmaster's `.flow/` directory or matching `Policy::denied_globs`
/// (such as `.env`, `*.pem`, `*.key`) across every Code Mode write/edit primitive, matching `tryPatch`.
fn check_policy_write(scope: &Scope, file: &Path) -> anyhow::Result<()> {
    let rel = file
        .strip_prefix(&scope.cwd)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    let rel = rel.trim_start_matches("./");
    if tm_flow::is_flow_path(rel) {
        bail!("policy denied write to `{rel}` (`.flow/` is managed by Taskmaster)");
    }
    let policy = crate::policy::Policy::load(&scope.cwd);
    if let crate::policy::PathVerdict::Denied(glob) = policy.check_path(rel) {
        bail!("policy denied write to `{rel}` (matched `{glob}`)");
    }
    Ok(())
}

/// Sanitize a file path coming from Code Mode `files` map keys or `writeFile`/`write` arguments.
///
/// Models sometimes wrap JSON map keys in literal quotes (e.g. `"\"tinydb/x.py\""` or `"'README.md'"`)
/// or pad them with spaces. Strip surrounding whitespace and matching quote pairs, then reject any
/// path that is still empty or invalid (unmatched/embedded quotes, control characters, parent
/// directory traversal `..`, trailing separators, or no normal filename component).
pub(crate) fn sanitize_file_path(raw: &str) -> anyhow::Result<String> {
    if raw.chars().any(|c| c.is_control() && c != '\t') {
        bail!("invalid file path {raw:?}: control characters are not allowed");
    }
    let mut s = raw.trim();
    loop {
        let stripped = s
            .strip_prefix('"')
            .and_then(|rest| rest.strip_suffix('"'))
            .or_else(|| s.strip_prefix('\'').and_then(|rest| rest.strip_suffix('\'')))
            .or_else(|| s.strip_prefix('`').and_then(|rest| rest.strip_suffix('`')));
        match stripped {
            Some(inner) => s = inner.trim(),
            None => break,
        }
    }
    if s.is_empty() {
        bail!("a path is required");
    }
    if s.contains(['"', '`']) || s.starts_with('\'') || s.ends_with('\'') {
        bail!("invalid file path `{s}`: quote characters are not allowed");
    }
    if s.chars().any(char::is_control) {
        bail!("invalid file path `{s}`: control characters are not allowed");
    }
    if s.ends_with('/') || s.ends_with('\\') {
        bail!("invalid file path `{s}`: path must name a file, not a directory");
    }
    let path = Path::new(s);
    let mut has_normal = false;
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                bail!("invalid file path `{s}`: parent directory `..` is not allowed");
            }
            std::path::Component::Normal(part) => {
                has_normal = true;
                let part = part.to_string_lossy();
                if part.trim() != part
                    || part.contains(['"', '`'])
                    || part.starts_with('\'')
                    || part.ends_with('\'')
                {
                    bail!("invalid file path `{s}`: path segment `{part}` has invalid whitespace or quotes");
                }
            }
            std::path::Component::CurDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    if !has_normal {
        bail!("invalid file path `{s}`: path must name a file");
    }
    Ok(s.to_string())
}

impl CodeTools {
    async fn write_raw_file(&self, scope: &Scope, rel_path: &str, content: &str) -> anyhow::Result<String> {
        if scope.read_only {
            bail!("refused: `writeFile` is disabled in a read-only scope (use read, readMany, grep, ls, diff, symbols, probe.ast, check)");
        }
        let clean = sanitize_file_path(rel_path)?;
        let file = if Path::new(&clean).is_absolute() {
            PathBuf::from(&clean)
        } else {
            scope.cwd.join(&clean)
        };
        check_policy_write(scope, &file)?;
        self.allowed(scope, "write", ToolKind::Edit, Some(&file), None).await?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&file, content)?;
        Ok(clean)
    }
}

#[async_trait::async_trait]
impl Extension for CodeTools {
    fn tools(&self, grant: &Grant) -> Vec<Value> {
        let Some(scope) = self.scopes.lock().unwrap().get(&grant.agent()).cloned() else {
            return Vec::new();
        };
        let description = if scope.read_only {
            "Run a short read-only JavaScript program (async; use await and return) over the repo in one call instead of many read/search calls. \
             Functions: read(path), readMany(globs), grep(text, glob?), ls(glob?), diff(glob?), symbols(glob?), flow(op, args) (query the .flow code graph: find/node/neighbors/source), probe.ast(path), check(), status(text), wait({url, port, log, agent, timeout}), and console.log. \
             Paths are relative to the worktree. Batch all discovery into 1-2 scripts."
        } else {
            "Run a short JavaScript program (async; use await and return) over your worktree in one call, instead of many tool calls. \
             Functions: read(path), readMany(globs), write(path, text | { raw }), writeFile(path, content | { raw }), edit(path, find, replace, all?), patch(path, [{find, replace, all?}]), \
             applyPatch(diff) or transactional tryPatch(diff) (validates policy/permissions & returns same-turn diagnostics), \
             grep(text, glob?), ls(glob?), diff(glob?), symbols(glob?), flow(op, args) (query the .flow code graph: find/node/neighbors/source), probe.ast(path), sh(command), \
             test() / testAffected(files?) which run tests and return {ok, failed, output}, check(), browser(action, args), \
             status(text) to set a live one-line status, wait({url, port, log, agent, timeout}) to wait for a server/port/log/sibling unit, \
             and console.log. For Python/code with triple quotes or backslashes, pass raw file content via the separate `files` (`{path: content}`) or `writeFile` (`{path, content}`) JSON arguments (written before `code` runs), or use `String.raw` with `write(path, { raw })`. \
             Paths are relative to your worktree. The same permissions as your own tools apply. \
             Batch steps into one script and end with `return await test()`. Returns the log lines and the returned value."
        };
        let properties = if scope.read_only {
            json!({
                "code": { "type": "string", "description": "The body of an async function" }
            })
        } else {
            json!({
                "code": { "type": "string", "description": "The body of an async function (optional when `files` or `writeFile` is provided)" },
                "files": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Optional map of unquoted relative path (e.g. `src/app.py`, not `\"src/app.py\"`) -> raw UTF-8 file content written directly before `code` runs; avoids JS string escaping for Python triple quotes, backticks, or backslashes"
                },
                "writeFile": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "raw": { "type": "string" }
                    },
                    "required": ["path"],
                    "description": "Optional single raw file write `{path, content}` applied before `code` runs"
                }
            })
        };
        vec![json!({
            "name": "run_code",
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": if scope.read_only { json!(["code"]) } else { json!([]) }
            },
        })]
    }

    async fn call(&self, grant: &Grant, name: &str, args: &Value) -> Option<Value> {
        if is_write_file_tool(name) {
            let scope = match self.scopes.lock().unwrap().get(&grant.agent()).cloned() {
                Some(scope) => scope,
                None => {
                    return Some(json!({
                        "content": [{ "type": "text", "text": "this agent has no worktree to write files in" }],
                        "isError": true
                    }));
                }
            };
            let rel_path = args["path"].as_str().unwrap_or("");
            let content = args["content"]
                .as_str()
                .or_else(|| args["raw"].as_str())
                .or_else(|| args["text"].as_str())
                .unwrap_or("");
            return Some(match self.write_raw_file(&scope, rel_path, content).await {
                Ok(clean) => {
                    let msg = if clean != rel_path {
                        format!("wrote {clean} (sanitized path from {rel_path:?})")
                    } else {
                        format!("wrote {clean}")
                    };
                    json!({ "content": [{ "type": "text", "text": msg }], "isError": false })
                }
                Err(err) => json!({ "content": [{ "type": "text", "text": format!("{err:#}") }], "isError": true }),
            });
        }
        if !is_run_code_tool(name) {
            return None;
        }
        let mut wrote: Vec<String> = Vec::new();
        let mut sanitized_notes: Vec<String> = Vec::new();
        let has_files = args.get("files").and_then(Value::as_object).is_some_and(|m| !m.is_empty());
        let has_write_file = args.get("writeFile").and_then(Value::as_object).is_some();
        if has_files || has_write_file {
            let scope = match self.scopes.lock().unwrap().get(&grant.agent()).cloned() {
                Some(scope) => scope,
                None => {
                    return Some(json!({
                        "content": [{ "type": "text", "text": "this agent has no worktree to run code in" }],
                        "isError": true
                    }));
                }
            };
            if let Some(obj) = args.get("writeFile").and_then(Value::as_object) {
                let rel_path = obj.get("path").and_then(Value::as_str).unwrap_or("");
                let content = obj
                    .get("content")
                    .and_then(Value::as_str)
                    .or_else(|| obj.get("raw").and_then(Value::as_str))
                    .or_else(|| obj.get("text").and_then(Value::as_str))
                    .unwrap_or("");
                let clean = match self.write_raw_file(&scope, rel_path, content).await {
                    Ok(clean) => clean,
                    Err(err) => {
                        return Some(json!({ "content": [{ "type": "text", "text": format!("{err:#}") }], "isError": true }));
                    }
                };
                if clean != rel_path {
                    sanitized_notes.push(format!("{rel_path:?} -> {clean}"));
                }
                wrote.push(clean);
            }
            if let Some(map) = args.get("files").and_then(Value::as_object) {
                for (raw_path, value) in map {
                    let clean_path = match sanitize_file_path(raw_path) {
                        Ok(clean) => clean,
                        Err(err) => {
                            return Some(json!({
                                "content": [{ "type": "text", "text": format!("invalid `files` key {raw_path:?}: {err:#}") }],
                                "isError": true
                            }));
                        }
                    };
                    let Some(content) = value.as_str() else {
                        return Some(json!({
                            "content": [{ "type": "text", "text": format!("`files[\"{clean_path}\"]` must be a string") }],
                            "isError": true
                        }));
                    };
                    let written = match self.write_raw_file(&scope, &clean_path, content).await {
                        Ok(written) => written,
                        Err(err) => {
                            return Some(json!({ "content": [{ "type": "text", "text": format!("{err:#}") }], "isError": true }));
                        }
                    };
                    if written != *raw_path {
                        sanitized_notes.push(format!("{raw_path:?} -> {written}"));
                    }
                    wrote.push(written);
                }
            }
        }
        let code = args["code"].as_str().unwrap_or("");
        let note_suffix = if sanitized_notes.is_empty() {
            String::new()
        } else {
            format!(" (sanitized quoted/padded key(s): {})", sanitized_notes.join(", "))
        };
        if code.trim().is_empty() && !wrote.is_empty() {
            return Some(json!({
                "content": [{ "type": "text", "text": format!("wrote {} file(s): {}{note_suffix}", wrote.len(), wrote.join(", ")) }],
                "isError": false
            }));
        }
        Some(match self.run(grant, code).await {
            Ok(text) => {
                let is_err = text.contains("\nError: ") || text.starts_with("Error: ");
                let body = if sanitized_notes.is_empty() {
                    text
                } else {
                    format!("note: sanitized quoted/padded path key(s): {}\n{text}", sanitized_notes.join(", "))
                };
                json!({ "content": [{ "type": "text", "text": body }], "isError": is_err })
            }
            Err(err) => json!({ "content": [{ "type": "text", "text": format!("{err:#}") }], "isError": true }),
        })
    }
}

/// Several extensions behind one MCP server: each is asked in turn.
pub struct Extensions(pub Vec<Arc<dyn Extension>>);

#[async_trait::async_trait]
impl Extension for Extensions {
    fn tools(&self, grant: &Grant) -> Vec<Value> {
        self.0.iter().flat_map(|e| e.tools(grant)).collect()
    }

    async fn call(&self, grant: &Grant, name: &str, args: &Value) -> Option<Value> {
        for extension in &self.0 {
            if let Some(reply) = extension.call(grant, name, args).await {
                return Some(reply);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_harness::WorktreeGate;

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tm-code-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::process::Command::new("git").arg("-C").arg(&dir).args(["init", "-q"]).status().unwrap();
        std::fs::write(dir.join("app/index.html"), "<h1>Old title</h1>\n<p>Old body</p>\n").unwrap();
        std::fs::write(dir.join("app/data.json"), "{\"ok\": true}\n").unwrap();
        dir.canonicalize().unwrap()
    }

    fn grant() -> Grant {
        Grant { run: "r".into(), page: "home".into(), unit: String::new() }
    }

    #[tokio::test]
    async fn one_script_reads_edits_searches_and_checks() {
        let Some(tools) = CodeTools::new(None) else {
            eprintln!("no node here; run_code is not exercised");
            return;
        };
        let dir = repo("many");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        let code = r#"
            const page = await read("app/index.html");
            console.log("lines:", page.split("\n").length - 1);
            await edit("app/index.html", "Old title", "Tip jar");
            await write("app/notes/todo.txt", "one\ntwo\n");
            const hits = await grep("Tip jar");
            return { hits, files: (await ls("app/**")).length, problems: await check() };
        "#;
        let text = tools.run(&grant(), code).await.unwrap();
        assert!(text.starts_with("lines: 2\n"), "{text}");
        assert!(text.contains("app/index.html:1: <h1>Tip jar</h1>"), "{text}");
        assert!(text.contains("\"files\": 3"), "{text}");
        assert_eq!(std::fs::read_to_string(dir.join("app/notes/todo.txt")).unwrap(), "one\ntwo\n");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn the_gate_and_the_sandbox_hold() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("held");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        // Outside the worktree, and a shell: the gate refuses both, as it does for the agent's own tools.
        let text = tools.run(&grant(), r#"try { await read("/etc/hostname"); } catch (e) { console.log(e.message); } return await sh("echo hi");"#).await.unwrap();
        assert!(text.contains("refused: read /etc/hostname"), "{text}");
        assert!(text.contains("Error: Error: refused: sh"), "{text}");
        // Node itself has no files and no child processes.
        let text = tools.run(&grant(), r#"return require("fs").readFileSync("/etc/hostname", "utf8");"#).await.unwrap();
        assert!(text.contains("Error:") && text.contains("Access to this API has been restricted"), "{text}");
        let text = tools.run(&grant(), r#"return require("child_process").execSync("id").toString();"#).await.unwrap();
        assert!(text.contains("Error:"), "{text}");
        // An ambiguous edit is refused rather than guessed.
        std::fs::write(dir.join("app/twice.txt"), "x x").unwrap();
        let text = tools.run(&grant(), r#"return await edit("app/twice.txt", "x", "y");"#).await.unwrap();
        assert!(text.contains("occurs 2 times"), "{text}");
        // A page with no worktree has no tool.
        assert!(tools.tools(&Grant { run: "r".into(), page: "other".into(), unit: String::new() }).is_empty());
        assert_eq!(tools.tools(&grant()).len(), 1);
        tools.detach_page("home");
        assert!(tools.run(&grant(), "return 1").await.is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    /// With a shell, `sh` runs inside bubblewrap: the worktree and the scratch can be written,
    /// nothing else can. Skipped where bubblewrap cannot run.
    #[tokio::test]
    async fn sh_runs_in_the_sandbox_and_writes_only_the_worktree() {
        let Some(tools) = CodeTools::new(None) else { return };
        if tools.bwrap.is_none() {
            eprintln!("bubblewrap is not usable here; skipping");
            return;
        }
        let dir = repo("sh-sandbox");
        let scratch = dir.with_extension("scratch");
        let outside = dir.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let shell = tm_harness::ShellScope { writable: vec![scratch.clone()], readable: vec![], env: vec![("TASKMASTER_SCRATCH".into(), scratch.display().to_string())] };
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![]).with_shell()), Some(shell));
        let script = format!(
            r#"return await sh("echo in > made.txt && echo s > $TASKMASTER_SCRATCH/s.txt && (echo out > {}/x.txt || echo refused-outside)");"#,
            outside.display()
        );
        let text = tools.run(&grant(), &script).await.unwrap();
        assert!(dir.join("made.txt").is_file(), "{text}");
        assert!(scratch.join("s.txt").is_file(), "{text}");
        assert!(!outside.join("x.txt").exists(), "{text}");
        assert!(text.contains("refused-outside"), "{text}");
        for d in [&dir, &scratch, &outside] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[tokio::test]
    async fn read_many_loads_every_file_the_globs_match_in_one_call() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("read-many");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        let text = tools.run(&grant(), r#"const files = await readMany(["app/*.html", "app/*.json"]); return Object.keys(files).sort();"#).await.unwrap();
        assert!(text.contains("app/data.json") && text.contains("app/index.html"), "{text}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn failing_tests_are_read_from_each_runners_output() {
        let pytest = "tests/test_db.py::test_insert FAILED\nFAILED tests/test_db.py::test_insert - assert 1 == 2\n1 failed, 3 passed";
        assert_eq!(failed_tests(pytest), vec!["tests/test_db.py::test_insert"]);
        let cargo = "test store::tests::keeps_order ... FAILED\ntest store::tests::ok ... ok";
        assert_eq!(failed_tests(cargo), vec!["store::tests::keeps_order"]);
        let steps = vec![
            crate::runtime::Step { name: "setup".into(), program: "python3".into(), args: vec!["-m".into(), "venv".into(), "/s/venv".into()], setup: true, ..Default::default() },
            crate::runtime::Step { name: "test".into(), program: "/s/venv/bin/python".into(), args: vec!["-m".into(), "pytest".into(), "-q".into()], env: vec![("VIRTUAL_ENV".into(), "/s/venv".into())], ..Default::default() },
        ];
        let line = test_command(&steps);
        assert!(line.starts_with("{ python3 -m venv /s/venv >/dev/null 2>&1 || echo"), "{line}");
        assert!(line.ends_with("; VIRTUAL_ENV=/s/venv /s/venv/bin/python -m pytest -q"), "{line}");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    /// `test()` runs the project's own tests in the sandbox and names what failed. Skipped
    /// without bubblewrap, or without Python and pytest.
    #[tokio::test]
    async fn test_runs_the_projects_tests_in_the_sandbox_and_names_failures() {
        let Some(tools) = CodeTools::new(None) else { return };
        let pytest = std::process::Command::new("python3").args(["-c", "import pytest"]).status().is_ok_and(|s| s.success());
        if tools.bwrap.is_none() || !pytest {
            eprintln!("bubblewrap or pytest is missing; skipping");
            return;
        }
        let dir = repo("test-op");
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("conftest.py"), "").unwrap();
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::write(dir.join("tests/test_calc.py"), "from calc import add\n\ndef test_add():\n    assert add(2, 2) == 4\n").unwrap();
        let scratch = dir.with_extension("scratch");
        let shell = tm_harness::ShellScope { writable: vec![scratch.clone()], readable: vec![], env: vec![("PYTHONDONTWRITEBYTECODE".into(), "1".into())] };
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![]).with_shell()), Some(shell));
        let text = tools.run(&grant(), "const r = await test(); return { ok: r.ok, failed: r.failed };").await.unwrap();
        assert!(text.contains("\"ok\": false") && text.contains("tests/test_calc.py::test_add"), "{text}");
        assert!(scratch.join("venv/pyvenv.cfg").is_file(), "the venv is made in the agent's scratch, not the worktree");
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        let text = tools.run(&grant(), "return (await test()).ok;").await.unwrap();
        assert!(text.contains("true"), "{text}");
        for d in [&dir, &scratch] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[tokio::test]
    async fn a_script_that_never_returns_is_stopped() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("crash");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        let text = tools.run(&grant(), "throw new Error('nope')").await.unwrap();
        assert!(text.contains("Error: Error: nope"), "{text}");
        let syntax = tools.run(&grant(), "return (").await;
        assert!(syntax.is_err(), "{syntax:?}");
        // Warm worker stays alive and usable after a syntax error.
        let after = tools.run(&grant(), "return 2 + 2;").await.unwrap();
        assert_eq!(after.trim(), "4");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn warm_node_worker_is_reused_across_calls_with_isolated_scope() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("warm-node");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        let pid1 = tools.run(&grant(), "const x = 1; return process.pid;").await.unwrap();
        let pid2 = tools.run(&grant(), "const x = 2; return process.pid;").await.unwrap();
        assert_eq!(pid1.trim(), pid2.trim(), "second run_code call should reuse the same warm Node process");
        tools.detach_page("home");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn apply_patch_edits_creates_deletes_and_obeys_the_gate() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("apply-patch");
        std::fs::write(dir.join("app/old.txt"), "remove me\n").unwrap();
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        let script = r#"
            const diff = [
                "--- a/app/index.html",
                "+++ b/app/index.html",
                "@@ -1,2 +1,2 @@",
                "-<h1>Old title</h1>",
                "+<h1>Patched title</h1>",
                " <p>Old body</p>",
                "--- /dev/null",
                "+++ b/app/created.txt",
                "@@ -0,0 +1,2 @@",
                "+alpha",
                "+beta",
                "--- a/app/old.txt",
                "+++ /dev/null",
                "@@ -1,1 +0,0 @@",
                "-remove me",
            ].join("\n");
            return await applyPatch(diff);
        "#;
        let text = tools.run(&grant(), script).await.unwrap();
        assert!(text.contains("app/index.html") && text.contains("app/created.txt") && text.contains("app/old.txt"), "{text}");
        assert_eq!(std::fs::read_to_string(dir.join("app/index.html")).unwrap(), "<h1>Patched title</h1>\n<p>Old body</p>\n");
        assert_eq!(std::fs::read_to_string(dir.join("app/created.txt")).unwrap(), "alpha\nbeta\n");
        assert!(!dir.join("app/old.txt").exists());

        // Refusing an outside path leaves earlier files in the same patch untouched (atomic check).
        let refused = tools
            .run(
                &grant(),
                r#"
                const diff = [
                    "--- a/app/index.html",
                    "+++ b/app/index.html",
                    "@@ -1,2 +1,2 @@",
                    "-<h1>Patched title</h1>",
                    "+<h1>Should not apply</h1>",
                    " <p>Old body</p>",
                    "--- a//etc/hosts",
                    "+++ b//etc/hosts",
                    "@@ -1,1 +1,1 @@",
                    "-127.0.0.1 localhost",
                    "+127.0.0.1 hacked",
                ].join("\n");
                return await applyPatch(diff);
                "#,
            )
            .await
            .unwrap();
        assert!(refused.contains("refused: applyPatch"), "{refused}");
        assert!(std::fs::read_to_string(dir.join("app/index.html")).unwrap().contains("Patched title"));
        tools.detach_page("home");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn readonly_scope_batches_discovery_and_refuses_mutations() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("readonly");
        std::fs::write(dir.join("app/lib.ts"), "export function greet() { return 'hi'; }\n").unwrap();
        tools.attach_readonly("planner", &dir, Arc::new(WorktreeGate::new(&dir, vec![]).read_only()));
        let g = Grant { run: "r".into(), page: "planner".into(), unit: String::new() };
        let ok = tools
            .run(
                &g,
                r#"
                const files = await readMany(["app/*.html", "app/*.ts"]);
                const syms = await symbols("app/*.ts");
                return { keys: Object.keys(files).sort(), syms };
                "#,
            )
            .await
            .unwrap();
        assert!(ok.contains("app/index.html") && ok.contains("greet"), "{ok}");

        for op in [
            r#"await write("app/x.txt", "no")"#,
            r#"await edit("app/index.html", "Old", "New")"#,
            r#"await applyPatch("--- a/app/index.html\n+++ b/app/index.html\n@@ -1,1 +1,1 @@\n-<h1>Old title</h1>\n+<h1>X</h1>")"#,
            r#"await sh("echo hi")"#,
        ] {
            let res = tools.run(&g, op).await.unwrap();
            assert!(res.contains("disabled in a read-only scope"), "{op} => {res}");
        }
        tools.detach("planner");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn try_patch_and_probe_ast_work_in_the_same_turn() {
        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("trypatch");
        std::fs::write(
            dir.join("app/math.ts"),
            "import { x } from './x';\nexport function add(a: number, b: number) { return a + b; }\n",
        )
        .unwrap();
        tools.attach("p1", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        let g = Grant { run: "r".into(), page: "p1".into(), unit: String::new() };

        let out = tools
            .run(
                &g,
                r#"
                const ast = await probe.ast("app/math.ts");
                const denied = await tryPatch("--- /dev/null\n+++ b/.env\n@@ -0,0 +1,1 @@\n+SECRET=1\n");
                const badJson = await tryPatch("--- a/app/data.json\n+++ b/app/data.json\n@@ -1,1 +1,1 @@\n-{\"ok\": true}\n+{broken\n");
                return { ast, denied, badJson };
                "#,
            )
            .await
            .unwrap();
        assert!(out.contains("export function add"), "{out}");
        assert!(out.contains("policy denied write"), "{out}");
        assert!(out.contains("not valid JSON"), "{out}");
        assert!(!dir.join(".env").exists(), "policy denied file must never be created");
        tools.detach("p1");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn code_mode_prompt_and_alias_match_harness_tool_name() {
        let guide = crate::engine::code_mode_guide("antigravity");
        assert!(guide.contains("mcp_dictionary_run_code"), "{guide}");
        assert!(!guide.contains(" run_code "), "{guide}");
        assert!(crate::engine::code_mode_guide("claude").contains("mcp__dictionary__run_code"));
        assert!(crate::engine::code_mode_guide("grok-build").contains("dictionary__run_code"));
        assert!(crate::engine::code_mode_guide("opencode").contains("dictionary_run_code"));

        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("alias");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);
        for name in ["run_code", "mcp_dictionary_run_code", "mcp__dictionary__run_code", "dictionary__run_code"] {
            let reply = tools
                .call(&grant(), name, &json!({ "code": "return 2 + 2;" }))
                .await
                .unwrap_or_else(|| panic!("expected `{name}` to be handled"));
            assert_eq!(reply["isError"], false, "{name}: {reply}");
            assert!(reply["content"][0]["text"].as_str().unwrap_or("").contains('4'), "{name}: {reply}");
        }
        assert!(tools.call(&grant(), "unknown_tool", &json!({})).await.is_none());
        tools.detach("home");
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn raw_content_write_handles_python_triple_quotes_and_separate_arguments() {
        assert!(crate::engine::code_mode_guide("antigravity").contains("String.raw"));
        assert!(tm_harness::CODE_MODE_WRITE_RULES.contains("String.raw"));

        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("raw-write");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);

        // 1. Inside JS: write(path, { raw }) and writeFile(path, { raw }) with String.raw + Python triple quotes.
        let js = r#"
            const py = String.raw`def parse(s):
    """Parse SQL with 'quotes' and \"\"\"triple quotes\"\"\"."""
    return r"\d+\n"
`;
            await write("app/parser.py", { raw: py });
            await writeFile("app/btree.py", { raw: String.raw`"""B-tree\nmodule"""` });
            return { p: await read("app/parser.py"), b: await read("app/btree.py") };
        "#;
        let out = tools.run(&grant(), js).await.unwrap();
        assert!(!out.contains("Error:"), "{out}");
        let parser_disk = std::fs::read_to_string(dir.join("app/parser.py")).unwrap();
        assert!(parser_disk.contains(r#""""Parse SQL"#), "{parser_disk}");
        assert!(parser_disk.contains(r#"r"\d+\n""#), "{parser_disk}");
        assert_eq!(std::fs::read_to_string(dir.join("app/btree.py")).unwrap(), r#""""B-tree\nmodule""""#);

        // 2. Separate MCP `files` argument on run_code + trailing code execution (zero JS escaping needed).
        let py_src = "def exec_sql():\n    \"\"\"Triple quotes & `backticks` & ${not_js}\"\"\"\n    return r'\\s+'\n";
        let reply = tools
            .call(
                &grant(),
                "mcp_dictionary_run_code",
                &json!({
                    "files": { "app/executor.py": py_src },
                    "code": "return (await read('app/executor.py')).includes('Triple quotes');"
                }),
            )
            .await
            .unwrap();
        assert_eq!(reply["isError"], false, "{reply}");
        assert!(reply["content"][0]["text"].as_str().unwrap_or("").contains("true"), "{reply}");
        assert_eq!(std::fs::read_to_string(dir.join("app/executor.py")).unwrap(), py_src);

        // 3. Separate MCP `writeFile` argument on run_code (with no `code`) AND direct `writeFile` tool call.
        let reply_wf = tools
            .call(
                &grant(),
                "run_code",
                &json!({ "writeFile": { "path": "app/storage.py", "content": "\"\"\"storage\"\"\"\n" } }),
            )
            .await
            .unwrap();
        assert_eq!(reply_wf["isError"], false, "{reply_wf}");
        assert_eq!(std::fs::read_to_string(dir.join("app/storage.py")).unwrap(), "\"\"\"storage\"\"\"\n");

        let reply_direct = tools
            .call(
                &grant(),
                "mcp_dictionary_writeFile",
                &json!({ "path": "app/cli.py", "content": "print(\"\"\"hello\"\"\")\n" }),
            )
            .await
            .unwrap();
        assert_eq!(reply_direct["isError"], false, "{reply_direct}");
        assert_eq!(std::fs::read_to_string(dir.join("app/cli.py")).unwrap(), "print(\"\"\"hello\"\"\")\n");

        // 4. Gate & read-only scope still refuse unauthorized raw writes.
        let escaped = tools
            .call(&grant(), "writeFile", &json!({ "path": "/tmp/tm-escape-should-not-write.py", "content": "x" }))
            .await
            .unwrap();
        assert_eq!(escaped["isError"], true, "{escaped}");
        assert!(!std::path::Path::new("/tmp/tm-escape-should-not-write.py").exists());

        tools.attach_readonly("ro", &dir, Arc::new(WorktreeGate::new(&dir, vec![]).read_only()));
        let ro_grant = Grant { run: "r".into(), page: "ro".into(), unit: String::new() };
        let ro_res = tools
            .call(&ro_grant, "run_code", &json!({ "files": { "app/ro.py": "x" } }))
            .await
            .unwrap();
        assert_eq!(ro_res["isError"], true, "{ro_res}");
        assert!(!dir.join("app/ro.py").exists());

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn code_mode_write_and_edit_enforce_policy_denied_globs() {
        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("policy-write");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);

        let via_js = tools.run(&grant(), r#"await write(".env", "SECRET=1\n");"#).await.unwrap();
        assert!(via_js.contains("policy denied write"), "{via_js}");
        assert!(!dir.join(".env").exists());

        let via_raw = tools
            .call(&grant(), "writeFile", &json!({ "path": "tls/server.key", "content": "PRIVATE" }))
            .await
            .unwrap();
        assert_eq!(via_raw["isError"], true, "{via_raw}");
        assert!(
            via_raw["content"][0]["text"].as_str().unwrap_or("").contains("policy denied write"),
            "{via_raw}"
        );
        assert!(!dir.join("tls/server.key").exists());

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn concurrent_reviewer_and_security_scopes_are_isolated_and_prefixed_tools_work() {
        assert_eq!(tm_harness::mcp_tool_name("antigravity", "dictionary", "browser_open"), "mcp_dictionary_browser_open");
        assert_eq!(tm_harness::mcp_tool_name("antigravity", "dictionary", "docs"), "mcp_dictionary_docs");
        assert_eq!(tm_harness::mcp_tool_name("claude", "dictionary", "web_fetch"), "mcp__dictionary__web_fetch");

        let dir = repo("rev-iso");
        let store = Arc::new(crate::store::FileStore::in_project(&dir));
        let live = Arc::new(crate::live::Live::new(store));
        let status = StatusTools::new(live);
        let rev_grant = Grant { run: "r".into(), page: "reviewer".into(), unit: String::new() };
        let sec_grant = Grant { run: "r".into(), page: "reviewer".into(), unit: "security".into() };

        // StatusTools accepts both bare and harness-prefixed tool names.
        let s1 = status
            .call(&rev_grant, "mcp_dictionary_set_status", &json!({ "text": "checking connections" }))
            .await
            .expect("prefixed set_status must be handled");
        assert_eq!(s1["isError"], false, "{s1}");
        assert!(s1["content"][0]["text"].as_str().unwrap_or("").contains("checking connections"));

        // Concurrent reviewer and security CodeTools scopes do not overwrite each other.
        if let Some(tools) = CodeTools::new(None) {
            let gate = Arc::new(WorktreeGate::new(&dir, vec![]).read_only());
            tools.attach_readonly("reviewer", &dir, gate.clone());
            tools.attach_readonly("reviewer/security", &dir, gate);
            assert_eq!(tools.tools(&rev_grant).len(), 1);
            assert_eq!(tools.tools(&sec_grant).len(), 1);
            let (r_res, s_res) = tokio::join!(
                tools.run(&rev_grant, "return (await ls('app/**')).length;"),
                tools.run(&sec_grant, "return (await read('app/data.json')).includes('ok');"),
            );
            assert!(r_res.unwrap().contains('2'));
            assert!(s_res.unwrap().contains("true"));
            // Detaching security leaves reviewer intact.
            tools.detach("reviewer/security");
            assert!(tools.tools(&sec_grant).is_empty());
            assert_eq!(tools.tools(&rev_grant).len(), 1);
            tools.detach("reviewer");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn code_mode_flow_queries_graph_for_workers_and_readonly_roles() {
        let Some(tools) = CodeTools::new(None) else {
            eprintln!("no node here; run_code is not exercised");
            return;
        };
        let dir = repo("flow-code");
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(
            dir.join("pkg/lib.py"),
            "def greet(name: str) -> str:\n    \"\"\"Return greeting.\"\"\"\n    return f'hi {name}'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pkg/main.py"),
            "from pkg.lib import greet\ndef run() -> str:\n    return greet('world')\n",
        )
        .unwrap();
        tm_flow::build(&dir).unwrap();

        let worker_gate = Arc::new(WorktreeGate::new(&dir, vec![]));
        tools.attach("home", &dir, worker_gate, None);
        let ro_gate = Arc::new(WorktreeGate::new(&dir, vec![]).read_only());
        tools.attach_readonly("reviewer", &dir, ro_gate);

        let rev_grant = Grant { run: "r".into(), page: "reviewer".into(), unit: String::new() };
        assert!(tools.tools(&grant())[0]["description"].as_str().unwrap().contains("flow(op, args)"));
        assert!(tools.tools(&rev_grant)[0]["description"].as_str().unwrap().contains("flow(op, args)"));

        let script = r#"
            const found = await flow("find", { query: "greet" });
            const detail = await flow("node", { id: "py:pkg/main.py#run" });
            const nbrs = await flow("neighbors", { id: "py:pkg/main.py#run", k: 1, kinds: ["calls"], dir: "out" });
            const src = await flow("source", { id: "py:pkg/lib.py#greet" });
            return {
                foundId: found.nodes[0].id,
                callees: detail.outgoing.map((e) => e.peer_id),
                nbrCount: nbrs.nodes.length,
                srcHasDef: src.code.includes("def greet"),
            };
        "#;
        for g in [grant(), rev_grant] {
            let reply = tools.run(&g, script).await.unwrap();
            assert!(reply.contains("py:pkg/lib.py#greet"), "{reply}");
            assert!(reply.contains("\"srcHasDef\": true"), "{reply}");
        }

        // Code Mode also denies writing into `.flow/`.
        let write_flow = tools.run(&grant(), r#"await write(".flow/meta.json", "{}");"#).await.unwrap();
        assert!(write_flow.contains("policy denied write to `.flow/meta.json`"), "{write_flow}");

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn write_raw_file_and_files_map_sanitize_quoted_keys_and_reject_invalid_paths() {
        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("sanitize-keys");
        let scope = Scope {
            cwd: dir.clone(),
            gate: Arc::new(WorktreeGate::new(&dir, vec![])),
            shell: None,
            read_only: false,
            warm: tokio::sync::Mutex::new(None),
        };
        tools.attach("home", &dir, scope.gate.clone(), None);

        // Site 1: write_raw_file strips literal surrounding quotes and whitespace.
        tools
            .write_raw_file(&scope, "  \"tinydb/direct.py\"  ", "x = 1\n")
            .await
            .expect("quoted path should be sanitized and written");
        assert_eq!(std::fs::read_to_string(dir.join("tinydb/direct.py")).unwrap(), "x = 1\n");
        assert!(!dir.join("\"tinydb/direct.py\"").exists());
        assert!(!dir.join("  \"tinydb/direct.py\"  ").exists());

        tools
            .write_raw_file(&scope, "'tinydb/single.py'", "y = 2\n")
            .await
            .expect("single-quoted path should be sanitized");
        assert_eq!(std::fs::read_to_string(dir.join("tinydb/single.py")).unwrap(), "y = 2\n");
        assert!(!dir.join("'tinydb/single.py'").exists());

        // Site 1: write_raw_file rejects paths that remain invalid after sanitization.
        for bad in [
            "",
            "   ",
            "\"\"",
            "\"tinydb/unmatched.py",
            "tinydb/\"inner.py\"",
            "../escape.py",
            "\"../escape.py\"",
            "tinydb/dir/",
            ".",
            "tinydb/bad\nname.py",
        ] {
            let err = tools.write_raw_file(&scope, bad, "nope").await;
            assert!(err.is_err(), "write_raw_file should reject invalid path {bad:?}");
        }

        // Site 2: run_code `files` map sanitizes quoted keys and writes to the clean path.
        let reply = tools
            .call(
                &grant(),
                "mcp_dictionary_run_code",
                &json!({
                    "files": {
                        "\"tinydb/x.py\"": "def x():\n    return 42\n",
                        "  \"README.md\"  ": "# TinyDB\n"
                    },
                    "code": "return await read('tinydb/x.py');"
                }),
            )
            .await
            .unwrap();
        assert_eq!(reply["isError"], false, "{reply}");
        assert!(reply["content"][0]["text"].as_str().unwrap_or("").contains("return 42"), "{reply}");
        assert_eq!(std::fs::read_to_string(dir.join("tinydb/x.py")).unwrap(), "def x():\n    return 42\n");
        assert_eq!(std::fs::read_to_string(dir.join("README.md")).unwrap(), "# TinyDB\n");
        assert!(!dir.join("\"tinydb/x.py\"").exists(), "must not create literal quoted directory/file");
        assert!(!dir.join("\"README.md\"").exists(), "must not create literal quoted README.md");

        // Site 2: run_code `files` map rejects keys that are still invalid paths.
        for bad in [
            "\"\"",
            "\"tinydb/unmatched.py",
            "tinydb/\"inner.py\"",
            "\"../escape.py\"",
            "tinydb/",
            "tinydb/bad\0null.py",
        ] {
            let res = tools
                .call(&grant(), "run_code", &json!({ "files": { bad: "nope" } }))
                .await
                .unwrap();
            assert_eq!(res["isError"], true, "files map should reject invalid key {bad:?}: {res}");
        }

        // Also via direct writeFile tool and run_code writeFile argument.
        let wf = tools
            .call(
                &grant(),
                "writeFile",
                &json!({ "path": "\"tinydb/wf.py\"", "content": "z = 3\n" }),
            )
            .await
            .unwrap();
        assert_eq!(wf["isError"], false, "{wf}");
        assert_eq!(std::fs::read_to_string(dir.join("tinydb/wf.py")).unwrap(), "z = 3\n");
        assert!(!dir.join("\"tinydb/wf.py\"").exists());

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn code_mode_refuses_all_writes_and_patches_into_flow_dir() {
        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("flow-ro");
        std::fs::create_dir_all(dir.join(".flow")).unwrap();
        std::fs::write(dir.join(".flow/meta.json"), "{\"schema_version\":1}\n").unwrap();
        // Even with a full-access WorktreeGate, Code Mode must refuse every mutation under `.flow/`.
        let gate = Arc::new(WorktreeGate::new(&dir, vec![]).with_full_access());
        tools.attach("home", &dir, gate, None);

        // 1. JS write / edit / patch / applyPatch / tryPatch
        let res = tools
            .run(
                &grant(),
                r#"
                const errs = [];
                for (const [label, fn] of [
                    ["write", () => write(".flow/evil.json", "{}")],
                    ["edit", () => edit(".flow/meta.json", "1", "2")],
                    ["patch", () => patch(".flow/meta.json", [{ find: "1", replace: "2" }])],
                    ["applyPatch", () => applyPatch("--- a/.flow/meta.json\n+++ b/.flow/meta.json\n@@ -1 +1 @@\n-{\"schema_version\":1}\n+{\"schema_version\":2}\n")],
                ]) {
                    try { await fn(); errs.push(label + ":ALLOWED"); } catch (e) { errs.push(label + ":" + e.message); }
                }
                const tp = await tryPatch("--- a/.flow/meta.json\n+++ b/.flow/meta.json\n@@ -1 +1 @@\n-{\"schema_version\":1}\n+{\"schema_version\":2}\n");
                return { errs, tp };
                "#,
            )
            .await
            .unwrap();
        assert!(!res.contains(":ALLOWED"), "Code Mode allowed a `.flow/` write:\n{res}");
        assert!(res.contains("\"applied\": false"), "tryPatch must refuse `.flow/`:\n{res}");

        // 2. Direct writeFile & run_code `files` argument
        let wf = tools
            .call(&grant(), "writeFile", &json!({ "path": ".flow/evil2.json", "content": "{}" }))
            .await
            .unwrap();
        assert_eq!(wf["isError"], true, "{wf}");
        let fm = tools
            .call(&grant(), "run_code", &json!({ "files": { ".flow/evil3.json": "{}" } }))
            .await
            .unwrap();
        assert_eq!(fm["isError"], true, "{fm}");

        // Verify disk is untouched.
        assert_eq!(std::fs::read_to_string(dir.join(".flow/meta.json")).unwrap(), "{\"schema_version\":1}\n");
        assert!(!dir.join(".flow/evil.json").exists());
        assert!(!dir.join(".flow/evil2.json").exists());
        assert!(!dir.join(".flow/evil3.json").exists());

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }
}
