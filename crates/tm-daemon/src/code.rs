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

const PRELUDE: &str = r#""use strict";
const readline = require("readline");
const lines = readline.createInterface({ input: process.stdin });
const waiting = new Map();
let next = 1;
lines.on("line", (line) => {
  const m = JSON.parse(line);
  const w = waiting.get(m.id);
  if (!w) return;
  waiting.delete(m.id);
  if (m.error !== undefined) w.reject(new Error(m.error));
  else w.resolve(m.result);
});
const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const call = (op, args) => new Promise((resolve, reject) => {
  const id = next++;
  waiting.set(id, { resolve, reject });
  send({ id, op, args });
});
const read = (path) => call("read", { path });
const write = (path, text) => call("write", { path, text: String(text) });
const edit = (path, find, replace, all = false) => call("edit", { path, find, replace, all });
const grep = (text, glob) => call("grep", { text, glob });
const ls = (glob) => call("ls", { glob });
const readMany = (globs) => call("readMany", { globs: Array.isArray(globs) ? globs : [globs] });
const test = () => call("test", {});
const sh = (command) => call("sh", { command });
const check = () => call("check", {});
const browser = (action, args = {}) => call("browser", { action, args });
const logs = [];
console.log = console.info = console.warn = console.error = (...parts) =>
  logs.push(parts.map((p) => (typeof p === "string" ? p : JSON.stringify(p))).join(" "));
(async () => {
  try {
    const result = await (async () => {
/*CODE*/
    })();
    send({ done: true, result: result === undefined ? null : result, logs });
  } catch (err) {
    send({ done: true, error: String((err && err.stack) || err).split("\n").slice(0, 4).join("\n"), logs });
  }
  lines.close();
  process.exit(0);
})();
"#;

/// What a page's scripts may touch, and through which gate.
struct Scope {
    cwd: PathBuf,
    gate: Arc<dyn PermissionGate>,
    /// The agent's sandboxed shell: what `sh` may write besides the worktree.
    shell: Option<tm_harness::ShellScope>,
}

pub struct CodeTools {
    node: PathBuf,
    bwrap: Option<PathBuf>,
    scopes: Mutex<HashMap<String, Arc<Scope>>>,
    browsers: Option<Arc<crate::browser::PageBrowsers>>,
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
            script_limit: Duration::from_secs(SCRIPT_SECS),
            sh_limit: Duration::from_secs(SH_SECS),
        }))
    }

    /// [`CodeTools::new`] with the time limits from the settings (seconds; 0 keeps the default).
    pub fn with_limits(browsers: Option<Arc<crate::browser::PageBrowsers>>, script_secs: u64, sh_secs: u64) -> Option<Arc<CodeTools>> {
        let tools = Arc::try_unwrap(Self::new(browsers)?).ok()?;
        let pick = |secs: u64, default: u64| Duration::from_secs(if secs == 0 { default } else { secs });
        Some(Arc::new(CodeTools { script_limit: pick(script_secs, SCRIPT_SECS), sh_limit: pick(sh_secs, SH_SECS), ..tools }))
    }

    pub fn attach(&self, page: &str, cwd: &Path, gate: Arc<dyn PermissionGate>, shell: Option<tm_harness::ShellScope>) {
        self.scopes.lock().unwrap().insert(page.to_string(), Arc::new(Scope { cwd: cwd.to_path_buf(), gate, shell }));
    }

    /// Drop the scope of a page and of every part of it (`page/unit`).
    pub fn detach_page(&self, page: &str) {
        let prefix = format!("{page}/");
        self.scopes.lock().unwrap().retain(|key, _| key != page && !key.starts_with(&prefix));
    }

    /// Run `code` for `grant`'s page. The text is what the agent reads back.
    pub async fn run(&self, grant: &Grant, code: &str) -> anyhow::Result<String> {
        let scope = self.scopes.lock().unwrap().get(&grant.agent()).cloned().context("this agent has no worktree to run code in")?;
        let scratch = std::env::temp_dir().join(format!("tm-code-{}-{}", std::process::id(), crate::secret::random_hex(6)?));
        std::fs::create_dir_all(&scratch)?;
        let script = scratch.join("run.js");
        std::fs::write(&script, PRELUDE.replace("/*CODE*/", code))?;
        let result = tokio::time::timeout(self.script_limit, self.drive(grant, &scope, &scratch, &script)).await;
        let _ = std::fs::remove_dir_all(&scratch);
        match result {
            Ok(result) => result,
            Err(_) => bail!("the script ran longer than {}s and was stopped", self.script_limit.as_secs()),
        }
    }

    async fn drive(&self, grant: &Grant, scope: &Scope, scratch: &Path, script: &Path) -> anyhow::Result<String> {
        let node_args = [
            "--permission".to_string(),
            format!("--allow-fs-read={}", script.display()),
            "--disable-proto=throw".to_string(),
            script.display().to_string(),
        ];
        let mut cmd = match &self.bwrap {
            Some(bwrap) => {
                let mut cmd = tokio::process::Command::new(bwrap);
                cmd.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp", "--unshare-net", "--die-with-parent"])
                    .arg("--ro-bind")
                    .arg(scratch)
                    .arg(scratch)
                    .arg(&self.node)
                    .args(&node_args);
                cmd
            }
            None => {
                let mut cmd = tokio::process::Command::new(&self.node);
                cmd.args(&node_args);
                cmd
            }
        };
        cmd.current_dir(scratch).env_clear().env("PATH", "/usr/bin:/bin").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let (mut child, _guard) = tm_harness::spawn_grouped(&mut cmd).context("could not start node")?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let mut out = BufReader::new(child.stdout.take().context("no stdout")?).lines();
        let mut stderr = child.stderr.take().context("no stderr")?;
        let mut calls = 0usize;
        while let Some(line) = out.next_line().await? {
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            if message["done"] == true {
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
            stdin.write_all(format!("{reply}\n").as_bytes()).await?;
        }
        let mut said = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut said).await;
        bail!("the script stopped before it finished: {}", said.lines().take(6).collect::<Vec<_>>().join("\n"))
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
                let file = path("path")?;
                self.allowed(scope, op, ToolKind::Edit, Some(&file), None).await?;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&file, args["text"].as_str().unwrap_or(""))?;
                Ok(json!(true))
            }
            "edit" => {
                let file = path("path")?;
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
                let venv = scope.shell.as_ref().and_then(|s| s.writable.first()).map(|scratch| scratch.join("venv")).unwrap_or_else(|| scope.cwd.join(".venv"));
                let steps = crate::runtime::steps(&scope.cwd, &venv);
                if !steps.iter().any(|step| !step.setup) {
                    return Ok(json!({ "ok": true, "ran": false, "failed": [], "output": "no tests were found to run" }));
                }
                let command = test_command(&steps);
                let (code, text) = self.sandboxed(scope, &command).await?;
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
            other => bail!("no function `{other}`"),
        }
    }
}

/// Project files under `cwd` (tracked and new, not ignored), filtered by `glob`.
/// One shell line that runs the project's test steps: setup steps may fail and are reported,
/// the checks must all pass.
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

/// Whether bubblewrap can make a sandbox here; some containers forbid user namespaces.
fn bwrap_works() -> bool {
    std::process::Command::new("bwrap")
        .args(["--ro-bind", "/", "/", "--unshare-net", "--die-with-parent", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[async_trait::async_trait]
impl Extension for CodeTools {
    fn tools(&self, grant: &Grant) -> Vec<Value> {
        if !self.scopes.lock().unwrap().contains_key(&grant.agent()) {
            return Vec::new();
        }
        vec![json!({
            "name": "run_code",
            "description": "Run a short JavaScript program (async; use await and return) over your worktree in one call, instead of many tool calls. \
                Functions: read(path), readMany(globs), write(path, text), edit(path, find, replace, all?), grep(text, glob?), ls(glob?), \
                sh(command), test() which runs the project's tests and returns {ok, failed, output}, check(), \
                browser(action, args) with action open|click|type|console, and console.log. Paths are relative to your worktree. \
                The same permissions as your own tools apply. Batch steps into one script and end with `return await test()`. \
                Returns the log lines and the returned value.",
            "inputSchema": { "type": "object", "properties": { "code": { "type": "string", "description": "The body of an async function" } }, "required": ["code"] },
        })]
    }

    async fn call(&self, grant: &Grant, name: &str, args: &Value) -> Option<Value> {
        if name != "run_code" {
            return None;
        }
        let code = args["code"].as_str().unwrap_or("");
        Some(match self.run(grant, code).await {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": text.contains("\nError: ") || text.starts_with("Error: ") }),
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
        let shell = tm_harness::ShellScope { writable: vec![scratch.clone()], env: vec![("TASKMASTER_SCRATCH".into(), scratch.display().to_string())] };
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
        let shell = tm_harness::ShellScope { writable: vec![scratch.clone()], env: vec![("PYTHONDONTWRITEBYTECODE".into(), "1".into())] };
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
        std::fs::remove_dir_all(dir).ok();
    }
}
