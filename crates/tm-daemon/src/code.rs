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
//! await test(paths?)                  {ok, failed, output, targets, interpreter}: project tests (or explicit paths), in the sandbox
//! await testAffected(paths?)          {ok, affectedFiles, testFiles, failed, output}: runs only affected tests matching paths/git status
//! await check()                       problems the static checks find in the worktree
//! await browser(action, args)         open | click | type | screenshot | console on the page's preview
//! console.log(...)                    lines returned with the result
//! ```
//!
//! The tool is served beside the Dictionary under the page's token, so a page that is not
//! being built has none.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tm_dictionary::{Extension, Grant};
use tm_harness::{Decision, PermissionGate, PermissionRequest, ToolKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long one script may run, all calls included, unless the settings say otherwise.
pub const SCRIPT_SECS: u64 = 600;
/// How long one `sh` or `test` call may run, unless the settings say otherwise.
pub const SH_SECS: u64 = 300;
const MAX_READ: usize = 256 * 1024;
const MAX_REPLY: usize = 24 * 1024;
const MAX_TOOL_OUTPUT: usize = 4096;
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
let activeState = null;
process.on("unhandledRejection", (reason) => {
  if (activeState) {
    activeState.unhandled.push(String((reason && reason.stack) || reason).split("\n").slice(0, 4).join("\n"));
  }
});
process.on("uncaughtException", (err) => {
  if (activeState) {
    activeState.unhandled.push(String((err && err.stack) || err).split("\n").slice(0, 4).join("\n"));
  }
});
const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const call = (op, args) => {
  let markDone;
  const done = new Promise((r) => { markDone = r; });
  if (activeState) {
    activeState.pending.push(done);
  }
  return new Promise((resolve, reject) => {
    const id = next++;
    waiting.set(id, { resolve, reject, markDone });
    send({ id, op, args });
  });
};
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
const edit = (path, find, replace, all = false) => {
  if (find && typeof find === "object") {
    return call("edit", {
      path,
      find: find.find ?? find.target,
      replace: find.replace ?? find.replacement ?? "",
      all: Boolean(find.all),
    });
  }
  return call("edit", { path, find, replace, all });
};
const patch = (path, edits) => call("patch", { path, edits });
const applyPatch = (diff) => call("applyPatch", { diff: String(diff) });
const tryPatch = (diff) => call("tryPatch", { diff: String(diff) });
const grep = (text, glob) => call("grep", { text, glob });
const ls = (glob) => call("ls", { glob });
const diff = (glob) => call("diff", { glob });
const symbols = (glob) => call("symbols", { glob });
const flow = (op, args = {}) => call("flow", { op: String(op ?? ""), args: args && typeof args === "object" ? args : {} });
const readMany = (globs) => call("readMany", { globs: Array.isArray(globs) ? globs : [globs] });
const test = (...paths) => {
  const flat = paths.flat().filter((p) => p !== undefined && p !== null && String(p).trim() !== "").map(String);
  return call("test", flat.length ? { files: flat } : {});
};
const testAffected = (...paths) => {
  const flat = paths.flat().filter((p) => p !== undefined && p !== null && String(p).trim() !== "").map(String);
  return call("testAffected", { files: flat });
};
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
  const state = { pending: [], unhandled: [] };
  activeState = state;
  try {
    const result = await fn(
      read, write, writeFile, edit, patch, applyPatch, tryPatch, grep, ls, diff, symbols, flow,
      readMany, test, testAffected, probe, diagnostics, outline, definition, references,
      sh, check, browser, status, wait, scopedConsole, require
    );
    if (state.pending.length > 0) {
      await Promise.all(state.pending);
      await new Promise((r) => setImmediate(r));
    }
    if (state.unhandled.length > 0) {
      send({
        done: true,
        error: "Unawaited call failed (always `await` every helper call):\n" + state.unhandled.join("\n"),
        logs,
      });
    } else {
      send({ done: true, result: result === undefined ? null : result, logs });
    }
  } catch (err) {
    if (state.pending.length > 0) {
      await Promise.all(state.pending);
      await new Promise((r) => setImmediate(r));
    }
    const extra = state.unhandled.length > 0 ? "\nUnawaited errors:\n" + state.unhandled.join("\n") : "";
    send({ done: true, error: String((err && err.stack) || err).split("\n").slice(0, 4).join("\n") + extra, logs });
  } finally {
    if (activeState === state) activeState = null;
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
  w.markDone();
});
lines.on("close", () => process.exit(0));
"#;

/// A warm sandboxed Node worker bound to one agent scope, reused across `run_code` calls.
struct WarmNode {
    child: tokio::process::Child,
    _guard: tm_harness::GroupGuard,
    stdin: tokio::process::ChildStdin,
    out: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stderr_buf: Arc<Mutex<String>>,
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
            .stderr(Stdio::piped());
        let (mut child, guard) = tm_harness::spawn_grouped(&mut cmd).context("could not start node")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let out = BufReader::new(child.stdout.take().context("no stdout")?).lines();
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let sink = stderr_buf.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut reader = stderr;
                let mut chunk = [0u8; 1024];
                loop {
                    match reader.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let piece = String::from_utf8_lossy(&chunk[..n]);
                            let mut buf = sink.lock().unwrap();
                            buf.push_str(&piece);
                            if buf.len() > 4096 {
                                let trimmed = tail(&buf, 4096);
                                *buf = trimmed;
                            }
                        }
                    }
                }
            });
        }
        Ok(WarmNode { child, _guard: guard, stdin, out, stderr_buf, scratch })
    }

    fn take_stderr(&self) -> String {
        let mut buf = self.stderr_buf.lock().unwrap();
        let out = buf.trim().to_string();
        buf.clear();
        out
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

/// Race-safe live-sync coordinator shared between [`crate::engine::Driver`] and a worker's
/// [`CodeTools`] [`Scope`].
///
/// Invariant: sibling files are NEVER copied into `worktree` while a sandboxed shell/test process
/// (`sh`, `test`, `testAffected`) is executing. Sandboxed commands hold a shared `rw.read()` guard;
/// live sync from `Driver::settle` uses `rw.try_write()` and defers via `dirty = true` when busy,
/// and `lock_for_exec()` flushes any deferred sync under `rw.write()` before starting the next
/// command.
pub struct WorktreeSync {
    worktree: PathBuf,
    tree: PathBuf,
    exclude_globs: Vec<String>,
    rw: tokio::sync::RwLock<()>,
    dirty: AtomicBool,
    snapshots: Mutex<HashMap<String, Option<[u8; 32]>>>,
    pending_deps: Mutex<BTreeSet<String>>,
}

impl WorktreeSync {
    pub fn new(
        worktree: PathBuf,
        tree: PathBuf,
        exclude_globs: Vec<String>,
        pending_deps: BTreeSet<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            worktree,
            tree,
            exclude_globs,
            rw: tokio::sync::RwLock::new(()),
            dirty: AtomicBool::new(false),
            snapshots: Mutex::new(HashMap::new()),
            pending_deps: Mutex::new(pending_deps),
        })
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Record that an upstream dependency has landed in `self.tree`.
    pub fn mark_dep_landed(&self, dep: &str) {
        self.pending_deps.lock().unwrap().remove(dep);
    }

    /// Upstream dependencies that have not landed yet.
    pub fn pending_deps(&self) -> Vec<String> {
        self.pending_deps.lock().unwrap().iter().cloned().collect()
    }

    fn record_snapshots(&self, report: &crate::git::SyncReport) {
        let mut map = self.snapshots.lock().unwrap();
        for snap in &report.snapshots {
            map.insert(snap.path.clone(), if snap.deleted { None } else { snap.sha256 });
        }
    }

    /// Mark the worktree dirty after a sibling lands in `tree`, and copy immediately ONLY if no
    /// sandboxed command (`test`/`sh`) is currently running in this worktree. When busy, leaves
    /// `dirty = true` so the next `test()`/`sh()`/read flushes safely before running.
    pub fn mark_dirty_and_try_sync(&self) -> Vec<String> {
        self.dirty.store(true, Ordering::Release);
        let Ok(_write_guard) = self.rw.try_write() else {
            return Vec::new();
        };
        match crate::git::sync_worktree_excluding_globs(&self.worktree, &self.tree, &self.exclude_globs) {
            Ok(report) => {
                self.record_snapshots(&report);
                self.dirty.store(false, Ordering::Release);
                report.updated
            }
            Err(_) => Vec::new(),
        }
    }

    /// Flush any deferred sibling sync under the exclusive write lock before a read or exec.
    pub async fn sync_if_dirty(&self) -> Vec<String> {
        if !self.dirty.load(Ordering::Acquire) {
            return Vec::new();
        }
        let _write_guard = self.rw.write().await;
        if !self.dirty.load(Ordering::Acquire) {
            return Vec::new();
        }
        let worktree = self.worktree.clone();
        let tree = self.tree.clone();
        let globs = self.exclude_globs.clone();
        match tokio::task::spawn_blocking(move || crate::git::sync_worktree_excluding_globs(&worktree, &tree, &globs)).await {
            Ok(Ok(report)) => {
                self.record_snapshots(&report);
                self.dirty.store(false, Ordering::Release);
                report.updated
            }
            _ => Vec::new(),
        }
    }

    /// Flush any pending sibling sync, then hold a shared exec guard so no sibling copy can race
    /// with an in-flight `sh`/`test`/`testAffected` process.
    pub async fn lock_for_exec(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        let _ = self.sync_if_dirty().await;
        self.rw.read().await
    }

    /// True when `change` in `self.worktree` still matches the exact file state last written by
    /// daemon live sync (protecting against false ownership failures if `tree` later moves on).
    pub fn matches_synced(&self, change: &crate::git::Change) -> bool {
        let expected = match self.snapshots.lock().unwrap().get(&change.path) {
            Some(entry) => *entry,
            None => return false,
        };
        let path = self.worktree.join(&change.path);
        match expected {
            None => change.status == 'D' && !path.exists(),
            Some(hash) => {
                if change.status == 'D' {
                    return false;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    return false;
                };
                let actual: [u8; 32] = Sha256::digest(&bytes).into();
                actual == hash
            }
        }
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
    /// Optional race-safe live sibling sync coordinator for active worker worktrees.
    sync: Option<Arc<WorktreeSync>>,
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

    /// Resolve the transcript agent tag registered for `grant` (falling back to a minimal tag).
    pub fn tag_for(&self, grant: &Grant) -> Value {
        let scope = grant.agent();
        self.tags.lock().unwrap().get(&scope).cloned().unwrap_or_else(|| {
            let id = match scope.as_str() {
                "master" | "reviewer" | "verifier" => scope.clone(),
                other if other.starts_with("page:") || other.starts_with("unit:") => other.to_string(),
                other => format!("page:{other}"),
            };
            json!({ "id": id, "role": if scope == "master" { "master" } else { "worker" } })
        })
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
        let tag = self.tag_for(grant);
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
        if tm_dictionary::strip_dict_prefix(name) != "set_status" {
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
        let bwrap = tm_harness::sandbox::resolve_binary("bwrap").ok().filter(|_| tm_harness::sandbox::bwrap_usable());
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
        self.attach_with_sync(page, cwd, gate, shell, None);
    }

    pub fn attach_with_sync(
        &self,
        page: &str,
        cwd: &Path,
        gate: Arc<dyn PermissionGate>,
        shell: Option<tm_harness::ShellScope>,
        sync: Option<Arc<WorktreeSync>>,
    ) {
        self.scopes.lock().unwrap().insert(
            page.to_string(),
            Arc::new(Scope {
                cwd: cwd.to_path_buf(),
                gate,
                shell,
                read_only: false,
                sync,
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
                sync: None,
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
        let _ = worker.take_stderr();
        let req = json!({ "exec": true, "code": code });
        worker
            .stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .context("could not send script to warm node worker")
            .map_err(DriveError::Broken)?;
        let mut calls = 0usize;
        let mut applied: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        while let Some(line) = worker.out.next_line().await.map_err(|e| DriveError::Broken(e.into()))? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message["done"] == true {
                if let Some(syntax) = message["syntaxError"].as_str() {
                    return Err(DriveError::Syntax(syntax.to_string()));
                }
                let mut text = summary(&message);
                if message["error"].as_str().is_some() {
                    if !applied.is_empty() {
                        text.push_str(&format!(
                            "\nPartial edits already applied on disk: {}. Inspect/continue from those edits and retry `run_code` rather than ending.",
                            applied.join(", ")
                        ));
                    } else {
                        text.push_str(
                            "\nNo file edits were applied by this call. Fix the script error and retry `run_code` (always `await` every helper call) rather than ending.",
                        );
                    }
                    text = tail(&text, MAX_REPLY);
                }
                return Ok(text);
            }
            calls += 1;
            let id = message["id"].clone();
            let op_name = message["op"].as_str().unwrap_or("");
            let op_args = &message["args"];
            let reply = if calls > MAX_CALLS {
                let msg = format!("more than {MAX_CALLS} calls in one script");
                failed.push(format!("{op_name}: {msg}"));
                json!({ "id": id, "error": msg })
            } else {
                match self.op(grant, scope, op_name, op_args).await {
                    Ok(result) => {
                        record_applied_op(&mut applied, op_name, op_args, &result);
                        json!({ "id": id, "result": result })
                    }
                    Err(err) => {
                        let target = op_args["path"].as_str().map(|p| format!(" {p}")).unwrap_or_default();
                        failed.push(format!("{op_name}{target}: {err:#}"));
                        json!({ "id": id, "error": format!("{err:#}") })
                    }
                }
            };
            if let Err(err) = worker.stdin.write_all(format!("{reply}\n").as_bytes()).await {
                return Err(DriveError::Broken(format_aborted_run_code(
                    worker,
                    &applied,
                    &failed,
                    Some(&format!("worker stopped accepting replies ({err})")),
                ).await));
            }
        }
        Err(DriveError::Broken(format_aborted_run_code(worker, &applied, &failed, None).await))
    }

    /// Run `command` with `sh -c` in the agent's sandbox: the worktree and its scratch are
    /// writable and nothing else. Refused without the sandbox, never run loose.
    async fn sandboxed(&self, scope: &Scope, command: &str) -> anyhow::Result<(Option<i32>, String)> {
        let (Some(_), Some(shell)) = (&self.bwrap, &scope.shell) else {
            bail!("refused: sh needs the sandbox, and this agent has none");
        };
        // Flush any deferred sibling sync cleanly before running, and hold the read guard for
        // the entire subprocess so live sibling sync never mutates files mid-test or mid-sh.
        let _exec_guard = match &scope.sync {
            Some(sync) => Some(sync.lock_for_exec().await),
            None => None,
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

    /// Run the project's test steps (optionally narrowed to explicit `targets` via
    /// [`crate::runtime::filter_test_steps`]) in the worker's sandbox. Shared by both `test`
    /// and `testAffected` so bare `test()` always runs the full suite (preserving acceptance
    /// checks) while targeted calls run only the requested/affected test paths.
    async fn execute_test_steps(&self, scope: &Scope, targets: &[String], record_env: bool) -> anyhow::Result<Value> {
        if let Some(sync) = &scope.sync {
            let _ = sync.sync_if_dirty().await;
        }
        let (venv, raw_steps) = select_test_steps(scope).await;
        if !raw_steps.iter().any(|step| !step.setup) {
            return Ok(json!({
                "ok": true,
                "ran": false,
                "targets": targets,
                "failed": [],
                "output": "no tests were found to run"
            }));
        }
        let mut steps = raw_steps.clone();
        crate::runtime::filter_test_steps(&mut steps, targets);
        let interpreter = steps
            .iter()
            .rev()
            .find(|s| s.name == "test" && !s.setup)
            .map(|s| s.program.clone());
        let command = test_command(&steps);
        let (code, mut text) = self.sandboxed(scope, &command).await?;
        let pending_upstreams = scope
            .sync
            .as_ref()
            .map(|s| s.pending_deps())
            .unwrap_or_default();
        if code != Some(0) && !pending_upstreams.is_empty() {
            text.push_str(&format!(
                "\nNote: upstream unit(s) still building: {}. Their files will sync into this worktree automatically once they land; do not reimplement them inline.",
                pending_upstreams.join(", ")
            ));
        }
        if record_env && code == Some(0) {
            let flow_root = self.flow_root.lock().unwrap().clone();
            let rel_base = self
                .project_root
                .lock()
                .unwrap()
                .clone()
                .or_else(|| flow_root.clone());
            if let (Some(root), Some(base)) = (flow_root, rel_base)
                && tm_flow::read_env(&root).ok().flatten().is_none()
                && let Some(facts) = crate::runtime::env_facts_from_steps_rel(&raw_steps, &venv, &base)
            {
                let _ = tm_flow::write_env(&root, &facts);
            }
        }
        let mut out = json!({
            "ok": code == Some(0),
            "ran": true,
            "targets": targets,
            "failed": failed_tests(&text),
            "output": tail(&text, MAX_TOOL_OUTPUT)
        });
        if let Some(interp) = interpreter {
            out["interpreter"] = json!(interp);
        }
        if !pending_upstreams.is_empty() {
            out["pendingUpstreams"] = json!(pending_upstreams);
        }
        Ok(out)
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
        if matches!(
            op,
            "read" | "readMany" | "ls" | "grep" | "symbols" | "probeAst" | "check" | "outline" | "definition" | "references" | "diagnostics"
        )
            && let Some(sync) = &scope.sync
            && sync.is_dirty()
        {
            let _ = sync.sync_if_dirty().await;
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
                Ok(json!({ "code": code, "output": tail(&text, MAX_TOOL_OUTPUT) }))
            }
            "test" => {
                self.allowed(scope, op, ToolKind::Execute, None, Some("the project's tests")).await?;
                let targets = parse_test_targets(args)?;
                let res = self.execute_test_steps(scope, &targets, true).await?;
                Ok(res)
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
                let requested = parse_test_targets(args)?;
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
                let matched_tests = resolve_affected_test_targets(&changed, &all_files);
                let mut res = self.execute_test_steps(scope, &matched_tests, false).await?;
                if let Some(obj) = res.as_object_mut() {
                    obj.insert("affectedFiles".into(), json!(changed));
                    obj.insert("testFiles".into(), json!(matched_tests));
                }
                Ok(res)
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

/// Validate and normalize one test target path (or pytest `path::node_id` selector).
/// Rejects flag injection (`-...`), traversal (`..`), quotes, and control characters.
fn sanitize_test_target(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("empty test target");
    }
    if trimmed.starts_with('-') {
        bail!("invalid test target `{trimmed}`: option flags are not allowed");
    }
    if let Some((file_part, node_part)) = trimmed.split_once("::") {
        let clean_file = sanitize_file_path(file_part)?;
        if clean_file.starts_with('-') {
            bail!("invalid test target `{trimmed}`: option flags are not allowed");
        }
        if node_part.is_empty()
            || node_part.chars().any(|c| c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ';' | '|' | '&' | '$' | '\\'))
        {
            bail!("invalid pytest node selector in `{trimmed}`");
        }
        Ok(format!("{clean_file}::{node_part}"))
    } else {
        let clean = sanitize_file_path(trimmed)?;
        if clean.starts_with('-') {
            bail!("invalid test target `{trimmed}`: option flags are not allowed");
        }
        Ok(clean)
    }
}

/// Extract validated test target paths from `{ files: [...] }` or `{ path: "..." }`.
fn parse_test_targets(args: &Value) -> anyhow::Result<Vec<String>> {
    let mut raw: Vec<&str> = Vec::new();
    match &args["files"] {
        Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str().filter(|s| !s.trim().is_empty()) {
                    raw.push(s);
                }
            }
        }
        Value::String(one) if !one.trim().is_empty() => raw.push(one.as_str()),
        _ => {}
    }
    if raw.is_empty()
        && let Some(one) = args["path"].as_str().filter(|s| !s.trim().is_empty())
    {
        raw.push(one);
    }
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let clean = sanitize_test_target(item)?;
        if !out.contains(&clean) {
            out.push(clean);
        }
    }
    Ok(out)
}

/// Map changed or requested files to concrete test file targets for `testAffected`:
/// - Direct test paths (or pytest `file::node` selectors) in `changed` are kept directly.
/// - Non-test source files match test files in `all_files` whose filename contains the stem.
fn resolve_affected_test_targets(changed: &[String], all_files: &[String]) -> Vec<String> {
    let looks_like_test = |p: &str| {
        let base = p.split("::").next().unwrap_or(p);
        let lower = base.to_ascii_lowercase();
        lower.contains(".test.")
            || lower.contains(".spec.")
            || lower.contains("test_")
            || lower.ends_with("_test.rs")
            || lower.ends_with("_test.py")
            || lower.ends_with("_test.go")
            || tm_core::is_test_path(base)
    };
    let mut matched: Vec<String> = Vec::new();
    let mut stems: Vec<String> = Vec::new();
    for path in changed {
        if looks_like_test(path) {
            if !matched.contains(path) {
                matched.push(path.clone());
            }
        } else if let Some(stem) = Path::new(path).file_stem().and_then(|s| s.to_str()) {
            let low = stem.to_ascii_lowercase();
            if !low.is_empty() && !stems.contains(&low) {
                stems.push(low);
            }
        }
    }
    if !stems.is_empty() || (changed.is_empty() && matched.is_empty()) {
        for f in all_files {
            if matched.len() >= 12 {
                break;
            }
            if !looks_like_test(f) {
                continue;
            }
            let lower = f.to_ascii_lowercase();
            if (stems.is_empty() || stems.iter().any(|st| lower.contains(st))) && !matched.contains(f) {
                matched.push(f.clone());
            }
        }
    }
    matched.truncate(12);
    matched
}

/// Project files under `cwd` (tracked and new, not ignored), filtered by `glob`.
/// One shell line that runs the project's test steps: setup steps may fail and are reported,
/// the checks must all pass.
async fn select_test_steps(scope: &Scope) -> (PathBuf, Vec<crate::runtime::Step>) {
    let fallback_venv = scope
        .shell
        .as_ref()
        .and_then(|s| s.writable.first())
        .map(|scratch| scratch.join("venv"))
        .unwrap_or_else(|| scope.cwd.join(".venv"));
    // Prefer an already-prepared shared read-only venv mounted into the worker's sandbox
    // when it needs zero setup writes AND its pytest is actually importable under the worker
    // sandbox view (never masked by host `~/.local/lib`); otherwise fall back to the writable scratch venv.
    let shared_venv = scope
        .shell
        .as_ref()
        .and_then(|s| s.readable.iter().find(|d| d.join("pyvenv.cfg").is_file()))
        .cloned();
    match shared_venv {
        Some(shared) => {
            let candidate = crate::runtime::steps(&scope.cwd, &shared);
            let needs_pytest = candidate.iter().any(crate::runtime::is_pytest_step);
            let has_pytest = !needs_pytest || crate::runtime::venv_has_pytest(&shared).await;
            let ready = candidate.iter().any(|s| !s.setup)
                && !candidate.iter().any(|s| s.setup)
                && has_pytest;
            if ready {
                (shared, candidate)
            } else if needs_pytest
                && has_pytest
                && !crate::runtime::project_has_external_deps(&scope.cwd)
                && !crate::runtime::pyproject_has_scripts(&scope.cwd)
            {
                // Shared read-only venv already has working pytest and the worktree declares no
                // third-party packages or console scripts: keep reusing `shared` directly with
                // `PYTHONPATH=.` rather than building an offline scratch venv.
                let mut test_only: Vec<crate::runtime::Step> =
                    candidate.into_iter().filter(|s| !s.setup).collect();
                for step in &mut test_only {
                    if !step.env.iter().any(|(k, _)| k == "PYTHONPATH") {
                        step.env.push(("PYTHONPATH".into(), ".".into()));
                    }
                }
                (shared, test_only)
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

fn record_applied_op(applied: &mut Vec<String>, op: &str, args: &Value, result: &Value) {
    let entry = match op {
        "write" | "edit" | "patch" => args["path"].as_str().map(|p| format!("{op}({p})")),
        "applyPatch" => result["files"]
            .as_array()
            .filter(|a| !a.is_empty())
            .map(|a| {
                let paths: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                format!("applyPatch({})", paths.join(", "))
            }),
        "tryPatch" if result["applied"] == true => result["files"]
            .as_array()
            .filter(|a| !a.is_empty())
            .map(|a| {
                let paths: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                format!("tryPatch({})", paths.join(", "))
            }),
        _ => None,
    };
    if let Some(item) = entry
        && !applied.contains(&item)
    {
        applied.push(item);
    }
}

async fn format_aborted_run_code(
    worker: &mut WarmNode,
    applied: &[String],
    failed: &[String],
    extra: Option<&str>,
) -> anyhow::Error {
    // Give the stderr drain task and child exit status a brief moment to settle.
    tokio::time::sleep(Duration::from_millis(25)).await;
    let status = worker
        .child
        .try_wait()
        .ok()
        .flatten()
        .map(|s| format!(" (exit {s})"))
        .unwrap_or_default();
    let stderr = worker.take_stderr();
    let mut msg = format!("run_code script aborted before finishing{status} (the script stopped before it finished).");
    if let Some(reason) = extra {
        msg.push_str(&format!(" Cause: {reason}."));
    }
    if !stderr.is_empty() {
        msg.push_str(&format!("\nStderr:\n{}", tail(&stderr, 1200)));
    }
    if applied.is_empty() {
        msg.push_str("\nApplied edits on disk before abort: none (worktree unchanged by this call).");
    } else {
        msg.push_str(&format!("\nApplied edits on disk before abort: {}.", applied.join(", ")));
    }
    if !failed.is_empty() {
        msg.push_str(&format!("\nFailed ops before abort:\n- {}", failed.join("\n- ")));
    }
    msg.push_str(
        "\nRecovery: fix the script error and call `run_code` again (always `await` every helper call; use `edit(path, find, replace)` or `files`/`writeFile`, and end with `return await test()`). Do NOT end your turn or switch to native shell/edit tools.",
    );
    anyhow::anyhow!(msg)
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

/// Accept both the canonical MCP name (`run_code`) and harness-prefixed aliases
/// (such as `mcp_dictionary_run_code` on Antigravity or `mcp__dictionary__run_code` on Claude).
fn is_run_code_tool(name: &str) -> bool {
    tm_dictionary::strip_dict_prefix(name) == "run_code"
}

/// Accept `writeFile` / `write_file` (with or without a Dictionary prefix) taking `{path, content}`
/// as separate MCP arguments so raw files bypass JS string escaping.
fn is_write_file_tool(name: &str) -> bool {
    matches!(tm_dictionary::strip_dict_prefix(name), "writeFile" | "write_file")
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
            "Async JS body over repo (relative paths). Helpers: read, readMany, grep, ls, diff, symbols, flow(op, args), probe.ast, check, status, wait, console.log."
        } else {
            "Async JS body over worktree (relative paths; await/return). Helpers: read, readMany, write, writeFile, edit, patch, applyPatch, tryPatch, grep, ls, diff, symbols, flow(op, args), probe.ast, sh, test(paths?), testAffected(paths?), check, browser, status, wait, console.log. Use `files` or `writeFile` for raw multi-line files; end with `return await test(...)`."
        };
        let properties = if scope.read_only {
            json!({
                "code": { "type": "string", "description": "Async JS body" }
            })
        } else {
            json!({
                "code": { "type": "string", "description": "Async JS body (optional if files/writeFile set)" },
                "files": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Relative path -> raw UTF-8 content written before code runs"
                },
                "writeFile": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "raw": { "type": "string" }
                    },
                    "required": ["path"],
                    "description": "Single raw write {path, content} before code runs"
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
    async fn tok11_code_mode_guide_and_two_call_write_then_fix_discipline() {
        let guide = crate::engine::code_mode_guide("antigravity");
        assert!(guide.contains("Write + test (1-2 calls)"), "{guide}");
        assert!(guide.contains("return await test()"), "{guide}");
        assert!(guide.contains("test(\"tests/test_<unit>.py\")"), "{guide}");
        assert!(guide.contains("Fix + re-test"), "{guide}");
        assert!(guide.contains("mcp_dictionary_run_code"), "{guide}");
        assert!(!guide.contains(" run_code"), "bare run_code should be rewritten: {guide}");

        assert!(tm_harness::CODE_MODE_WRITE_RULES.contains("1-2 `run_code` calls"));
        assert!(tm_harness::CODE_MODE_WRITE_RULES.contains("return await test(\"tests/test_<unit>.py\")"));
        assert!(tm_harness::CODE_MODE_WRITE_RULES.contains("return await test()"));
        assert!(tm_harness::CODE_MODE_WRITE_RULES.contains("no whole-file rewrites"));

        let Some(tools) = CodeTools::new(None) else {
            return;
        };
        let dir = repo("tok11-two-call");
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);

        // Call 1: write implementation + unit test via `files` and run script body in ONE call.
        let call1 = tools
            .call(
                &grant(),
                "run_code",
                &json!({
                    "files": {
                        "pkg/calc.py": "def add(a, b):\n    return a - b\n",
                        "tests/test_calc.py": "from pkg.calc import add\ndef test_add(): assert add(2, 3) == 5\n"
                    },
                    "code": "return { src: await read('pkg/calc.py'), tst: await read('tests/test_calc.py') };"
                }),
            )
            .await
            .unwrap();
        assert_eq!(call1["isError"], false, "{call1}");
        assert!(call1["content"][0]["text"].as_str().unwrap().contains("return a - b"));

        // Call 2: targeted `edit` fix in the second call (no whole-file rewrite).
        let call2 = tools
            .call(
                &grant(),
                "run_code",
                &json!({
                    "code": "await edit('pkg/calc.py', 'return a - b', 'return a + b'); return await read('pkg/calc.py');"
                }),
            )
            .await
            .unwrap();
        assert_eq!(call2["isError"], false, "{call2}");
        assert!(call2["content"][0]["text"].as_str().unwrap().contains("return a + b"));

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
            sync: None,
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

    #[tokio::test]
    async fn select_test_steps_rejects_shared_venv_missing_sandbox_pytest() {
        let has_python3 = std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !has_python3 {
            return;
        }
        let ver_out = std::process::Command::new("python3")
            .args(["-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"])
            .output()
            .unwrap();
        let py_mm = String::from_utf8_lossy(&ver_out.stdout).trim().to_string();

        let dir = repo("select-shared-venv");
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/test_ok.py"), "def test_ok():\n    assert True\n").unwrap();
        let scratch = dir.join(".scratch");
        let shared = dir.join(".shared-venv");
        std::fs::create_dir_all(&scratch).unwrap();

        // Create an isolated shared venv WITHOUT pytest installed in it, then stamp it as if a
        // buggy host probe had approved it.
        let mk = std::process::Command::new("python3")
            .args(["-m", "venv", &shared.display().to_string()])
            .status()
            .unwrap();
        assert!(mk.success());
        // Write matching `.taskmaster-deps` stamp (compute by letting runtime::steps tell us the stamp).
        let probe_steps = crate::runtime::steps(&dir, &shared);
        let stamp_pair = probe_steps.iter().find_map(|s| s.stamp.clone()).expect("stamp on setup step");
        std::fs::write(&stamp_pair.0, &stamp_pair.1).unwrap();

        let scope = Scope {
            cwd: dir.clone(),
            gate: Arc::new(WorktreeGate::new(&dir, vec![])),
            shell: Some(tm_harness::ShellScope {
                writable: vec![scratch.clone()],
                readable: vec![shared.clone()],
                env: vec![],
            }),
            read_only: false,
            sync: None,
            warm: tokio::sync::Mutex::new(None),
        };

        // Even though `.taskmaster-deps` matches, `select_test_steps` must reject `shared` because
        // `pytest` is not importable in the sandbox view, falling back to `scratch/venv`.
        let (chosen_before, _) = select_test_steps(&scope).await;
        assert_eq!(chosen_before, scratch.join("venv"), "broken shared venv must not be selected");

        // Once pytest exists inside `shared`'s own site-packages, `select_test_steps` selects it.
        let venv_site = shared.join(format!("lib/python{py_mm}/site-packages/pytest"));
        std::fs::create_dir_all(&venv_site).unwrap();
        std::fs::write(venv_site.join("__init__.py"), "__version__ = '0.0.0-venv'\n").unwrap();
        std::fs::write(venv_site.join("__main__.py"), "print('pytest 0.0.0-venv')\n").unwrap();

        let (chosen_after, steps_after) = select_test_steps(&scope).await;
        assert_eq!(chosen_after, shared, "self-contained shared venv must be selected");
        assert!(steps_after.iter().all(|s| !s.setup), "ready shared venv needs no setup writes");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn tok14_tool_output_cap_keeps_4kb_traceback_tail() {
        let prefix = "verbose test output line before failure\n".repeat(300);
        let traceback = "E       AssertionError: bad checksum\nFAILED tests/test_hash.py::test_digest\n1 failed in 0.08s\n";
        let full = format!("{prefix}{traceback}");
        assert!(full.len() > MAX_TOOL_OUTPUT * 2);
        let kept = tail(&full, MAX_TOOL_OUTPUT);
        assert!(kept.len() <= MAX_TOOL_OUTPUT + 4, "kept len {} exceeded 4KB cap", kept.len());
        assert!(kept.starts_with('…'));
        assert!(kept.ends_with(traceback), "must keep trailing traceback: {kept}");
    }

    #[tokio::test]
    async fn fix_n8_aborted_run_code_reports_partial_edits_and_unawaited_errors_without_crashing() {
        let Some(tools) = CodeTools::new(None) else { return };
        let dir = repo("n8-recover");
        std::fs::write(dir.join("app/a.py"), "x = 1\n").unwrap();
        tools.attach("home", &dir, Arc::new(WorktreeGate::new(&dir, vec![])), None);

        // 1. Object-arg `edit(path, { target, replacement })` works cleanly.
        let obj_res = tools
            .run(
                &grant(),
                r#"await edit("app/a.py", { target: "x = 1", replacement: "x = 2" }); return await read("app/a.py");"#,
            )
            .await
            .unwrap();
        assert!(obj_res.contains("x = 2"), "object-arg edit should apply: {obj_res}");

        // 2. Unawaited failing edit() must NOT kill the warm Node process; it reports a clear error
        //    and names partial edits that already landed on disk.
        let unawaited = tools
            .run(
                &grant(),
                r#"
                await write("app/partial.py", "ok = True\n");
                edit("app/a.py", "does_not_exist", "x = 3");
                return "finished";
                "#,
            )
            .await
            .unwrap();
        assert!(
            unawaited.contains("Unawaited call failed") && unawaited.contains("does not occur"),
            "expected actionable unawaited error, got:\n{unawaited}"
        );
        assert!(
            unawaited.contains("Partial edits already applied on disk: write(app/partial.py)"),
            "expected partial-edit report, got:\n{unawaited}"
        );

        // 3. Genuine script abort (process.exit(1) after writing a file and failing an op) returns
        //    an actionable error listing applied edits, failed ops, stderr, and recovery advice,
        //    and the next run_code call spawns a fresh worker and succeeds.
        let aborted = tools
            .run(
                &grant(),
                r#"
                await write("app/before_abort.py", "saved = 1\n");
                try { await edit("app/a.py", "missing_needle", "y"); } catch (_) {}
                process.stderr.write("fatal boom\n");
                process.exit(1);
                "#,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(aborted.contains("run_code script aborted before finishing"), "{aborted}");
        assert!(aborted.contains("write(app/before_abort.py)"), "must list applied edits:\n{aborted}");
        assert!(
            aborted.contains("Failed ops before abort") && aborted.contains("does not occur"),
            "must list failed op:\n{aborted}"
        );
        assert!(aborted.contains("fatal boom"), "must surface stderr:\n{aborted}");
        assert!(aborted.contains("Recovery:"), "{aborted}");

        let recovered = tools.run(&grant(), r#"return await read("app/before_abort.py");"#).await.unwrap();
        assert!(recovered.contains("saved = 1"), "next run_code call must recover cleanly: {recovered}");

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn tb18_test_path_args_and_test_affected_run_only_targeted_tests() {
        let Some(tools) = CodeTools::new(None) else { return };
        if !tm_harness::sandbox::bwrap_usable() {
            return;
        }
        let npm = std::env::var_os("PATH")
            .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("npm").is_file()));
        if !npm {
            return;
        }
        let dir = repo("tb18-test-paths");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("src/alpha.js"), "export const a = 1;\n").unwrap();
        std::fs::write(dir.join("tests/alpha.test.js"), "// ok\n").unwrap();
        std::fs::write(dir.join("tests/acceptance.test.js"), "// fails when full suite runs\n").unwrap();
        // Test runner script: when passed explicit args, only runs those args (fails only if
        // `acceptance` is among them); when passed NO args (full suite), runs both and fails!
        std::fs::write(
            dir.join("run_tests.sh"),
            "#!/bin/sh\n\
             if [ \"$#\" -eq 0 ]; then\n\
               echo 'RAN=FULL_SUITE'\n\
               echo 'FAILED tests/acceptance.test.js - AssertionError: missing sibling' >&2\n\
               exit 1\n\
             fi\n\
             echo \"RAN=$*\"\n\
             for arg in \"$@\"; do\n\
               case \"$arg\" in\n\
                 *acceptance*) echo \"FAILED $arg - AssertionError\" >&2; exit 1 ;;\n\
               esac\n\
             done\n\
             echo '1 passed'\n\
             exit 0\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"tb18-test-paths","private":true,"scripts":{"test":"sh run_tests.sh"}}"#,
        )
        .unwrap();

        let scratch = dir.join(".scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let gate = Arc::new(WorktreeGate::new(&dir, vec![]).with_shell());
        let shell = tm_harness::ShellScope { writable: vec![scratch], readable: vec![], env: vec![] };
        tools.attach("home", &dir, gate, Some(shell));

        // 1. Targeted `test("tests/alpha.test.js")` runs ONLY that file and passes.
        let targeted = tools
            .run(&grant(), r#"return await test("tests/alpha.test.js");"#)
            .await
            .unwrap();
        assert!(targeted.contains("\"ok\": true"), "targeted test should pass:\n{targeted}");
        assert!(targeted.contains("RAN=tests/alpha.test.js"), "must forward target arg:\n{targeted}");
        assert!(!targeted.contains("RAN=FULL_SUITE"), "must not run full suite when targeted:\n{targeted}");
        assert!(targeted.contains("\"interpreter\""), "must report interpreter to avoid probe loops:\n{targeted}");

        // 2. Bare `test()` MUST still run the full suite (preserving acceptance tests).
        let full = tools.run(&grant(), r#"return await test();"#).await.unwrap();
        assert!(full.contains("\"ok\": false"), "bare test() must run full suite including acceptance:\n{full}");
        assert!(full.contains("RAN=FULL_SUITE"), "bare test() must pass no filter args:\n{full}");

        // 3. `testAffected("src/alpha.js")` resolves `tests/alpha.test.js` and runs ONLY it (passes!).
        let affected = tools
            .run(&grant(), r#"return await testAffected("src/alpha.js");"#)
            .await
            .unwrap();
        assert!(affected.contains("\"ok\": true"), "testAffected must run only affected test:\n{affected}");
        assert!(affected.contains("RAN=tests/alpha.test.js"), "testAffected must pass matched test to runner:\n{affected}");
        assert!(affected.contains("\"testFiles\""), "{affected}");

        // 4. Rejects traversal / option-flag injection in test() targets.
        let bad_flag = tools
            .run(&grant(), r#"try { await test("--collect-only"); return "ALLOWED"; } catch (e) { return e.message; }"#)
            .await
            .unwrap();
        assert!(!bad_flag.contains("ALLOWED") && bad_flag.contains("option flags are not allowed"), "{bad_flag}");
        let bad_trav = tools
            .run(&grant(), r#"try { await test("../escape.py"); return "ALLOWED"; } catch (e) { return e.message; }"#)
            .await
            .unwrap();
        assert!(!bad_trav.contains("ALLOWED") && bad_trav.contains(".."), "{bad_trav}");

        tools.detach_all();
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn tb18_worktree_sync_waits_for_inflight_exec_and_syncs_before_test() {
        let root = std::env::temp_dir().join(format!("tm-code-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let wt = root.join("wt");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        std::process::Command::new("git").args(["init", "-q", "-b", "main"]).current_dir(&repo).status().unwrap();
        crate::git::git(&repo, &["config", "user.name", "Test"]).unwrap();
        crate::git::git(&repo, &["config", "user.email", "test@example.com"]).unwrap();
        std::fs::write(repo.join("pkg/__init__.py"), "").unwrap();
        crate::git::git(&repo, &["add", "."]).unwrap();
        crate::git::git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        let h1 = crate::git::head(&repo).unwrap();
        crate::git::add_worktree(&repo, &wt, &h1).unwrap();

        let pending: BTreeSet<String> = ["parser".to_string()].into_iter().collect();
        let sync = WorktreeSync::new(wt.clone(), repo.clone(), vec!["pkg/cli.py".into()], pending);
        std::fs::write(wt.join("pkg/cli.py"), "WIP = 1\n").unwrap();

        // Hold an exec read guard (simulating an in-flight `test()`/`sh()`).
        let exec_guard = sync.lock_for_exec().await;

        // Sibling lands in `repo` while exec is running.
        std::fs::write(repo.join("pkg/parser.py"), "VAL = 99\n").unwrap();
        crate::git::git(&repo, &["add", "pkg/parser.py"]).unwrap();
        crate::git::git(&repo, &["commit", "-q", "-m", "parser landed"]).unwrap();

        sync.mark_dep_landed("parser");
        let copied_while_busy = sync.mark_dirty_and_try_sync();
        assert!(copied_while_busy.is_empty(), "must NOT copy while exec guard is held");
        assert!(sync.is_dirty(), "deferred sync must remain marked dirty");
        assert!(!wt.join("pkg/parser.py").exists(), "sibling file must not appear mid-exec");
        assert!(sync.pending_deps().is_empty());

        // Release exec guard -> next `lock_for_exec()` flushes the deferred sync atomically!
        drop(exec_guard);
        let _next_guard = sync.lock_for_exec().await;
        assert!(!sync.is_dirty(), "dirty flag must clear after flush");
        assert_eq!(std::fs::read_to_string(wt.join("pkg/parser.py")).unwrap(), "VAL = 99\n");
        assert_eq!(std::fs::read_to_string(wt.join("pkg/cli.py")).unwrap(), "WIP = 1\n");
        assert!(sync.matches_synced(&crate::git::Change { status: 'A', path: "pkg/parser.py".into() }));

        let _ = crate::git::remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn tb18_select_test_steps_reuses_shared_venv_after_stdlib_pyproject_added() {
        let Some(py_mm) = std::process::Command::new("python3")
            .args(["-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        else {
            return;
        };
        let dir = repo("tb18-shared-venv-reuse");
        let shared = dir.join("shared-venv");
        let scratch = dir.join("scratch");
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(dir.join("tests/test_ok.py"), "def test_ok(): assert True\n").unwrap();
        let mk = std::process::Command::new("python3")
            .args(["-m", "venv", &shared.display().to_string()])
            .status()
            .unwrap();
        if !mk.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let venv_site = shared.join(format!("lib/python{py_mm}/site-packages/pytest"));
        std::fs::create_dir_all(&venv_site).unwrap();
        std::fs::write(venv_site.join("__init__.py"), "__version__ = '0.0.0-venv'\n").unwrap();
        std::fs::write(venv_site.join("__main__.py"), "print('pytest 0.0.0-venv')\n").unwrap();

        // Stamp was written BEFORE worker added a stdlib-only `pyproject.toml`.
        let probe = crate::runtime::steps(&dir, &shared);
        let stamp_pair = probe.iter().find_map(|s| s.stamp.clone()).expect("stamp");
        std::fs::write(&stamp_pair.0, &stamp_pair.1).unwrap();

        // Worker writes a stdlib-only `pyproject.toml` with pytest config + empty deps.
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"calcx\"\ndependencies = []\n\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
        )
        .unwrap();

        let scope = Scope {
            cwd: dir.clone(),
            gate: Arc::new(WorktreeGate::new(&dir, vec![])),
            shell: Some(tm_harness::ShellScope {
                writable: vec![scratch.clone()],
                readable: vec![shared.clone()],
                env: vec![],
            }),
            read_only: false,
            sync: None,
            warm: tokio::sync::Mutex::new(None),
        };
        let (chosen, chosen_steps) = select_test_steps(&scope).await;
        assert_eq!(chosen, shared, "stdlib pyproject.toml must keep reusing ready shared RO venv");
        assert!(chosen_steps.iter().all(|s| !s.setup), "must emit no setup steps: {chosen_steps:?}");

        // Even if the stamp was deliberately stale (e.g. legacy stamp), stdlib repo with working
        // shared pytest still keeps `shared` instead of falling back to scratch!
        std::fs::write(&stamp_pair.0, "stale-old-stamp").unwrap();
        let (chosen_stale, steps_stale) = select_test_steps(&scope).await;
        assert_eq!(chosen_stale, shared, "stdlib repo with working shared pytest must not fall back to scratch");
        assert!(steps_stale.iter().all(|s| !s.setup));
        assert!(steps_stale.iter().any(|s| s.env.iter().any(|(k, v)| k == "PYTHONPATH" && v == ".")));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
