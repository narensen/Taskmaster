//! The live preview: one dev server per run, started on request and stopped with the run.
//!
//! A run that is merged (or built in place) is previewed from the project itself. A run
//! that still sits on its branch is previewed from a throwaway checkout of that branch,
//! which borrows the project's installed packages and is removed when the server stops.
//! The server listens on this machine only, on a port chosen here. Its command is the
//! settings file's `preview.command`, or the project's own `dev` script (then `start`, then
//! `preview`), after installing its packages when they are missing. A project with none of
//! those is served as static files, and so is one whose dev server dies before it answers:
//! a preview always opens.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::git;
use crate::time::rfc3339_now;

/// How long a dev server may take to start answering.
const START_WITHIN: Duration = Duration::from_secs(90);
/// The same, when its packages are installed first.
const INSTALL_WITHIN: Duration = Duration::from_secs(300);
const KEEP_LOG: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// `starting`, `running`, `stopped` or `failed`.
    pub state: &'static str,
    pub port: u16,
    pub url: String,
    pub command: String,
    pub dir: String,
    pub started: String,
    /// Why it failed or stopped, when it did, with the end of its output.
    pub reason: Option<String>,
}

struct Entry {
    status: Arc<Mutex<Status>>,
    pid: Option<u32>,
    /// The built-in static server, when that is what runs: from the start, or once the dev
    /// server died and the watcher took over.
    task: Arc<Mutex<Option<tokio::task::AbortHandle>>>,
    log: Arc<Mutex<String>>,
}

/// Removes a throwaway checkout when the server that used it ends or is aborted.
struct Cleanup(PathBuf, Option<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(tree) = &self.1 {
            let _ = git::remove_worktree(&self.0, tree);
            let _ = std::fs::remove_dir_all(tree);
        }
    }
}

/// Dev servers started for runs. Dropping the handle stops each one.
pub struct Previews {
    project: Mutex<PathBuf>,
    command: String,
    runs: Mutex<HashMap<String, Entry>>,
}

/// The command that starts a dev server in `dir` on `port`: the settings' command, or the
/// project's `dev` script, else its `start` or `preview` script. Packages are installed
/// first when `package.json` lists some and `node_modules` is missing.
pub fn dev_command(dir: &Path, configured: &str, port: u16) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.replace("{port}", &port.to_string()));
    }
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let package: Value = serde_json::from_str(&text).ok()?;
    let (name, script) = ["dev", "start", "preview"].into_iter().find_map(|name| package["scripts"][name].as_str().map(|script| (name, script)))?;
    let runner = if dir.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if dir.join("yarn.lock").is_file() {
        "yarn"
    } else if dir.join("bun.lockb").is_file() || dir.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    };
    let deps = |name: &str| package["dependencies"][name].is_string() || package["devDependencies"][name].is_string();
    // Say the port the way the framework's own server wants it. A `start` script is only
    // told when it runs the framework itself; anything else reads `$PORT`.
    let dev = name == "dev";
    let flags = if script.contains("next") || (dev && deps("next")) {
        format!("-p {port} -H 127.0.0.1")
    } else if script.contains("vite") || (dev && deps("vite")) {
        format!("--port {port} --host 127.0.0.1 --strictPort")
    } else {
        String::new()
    };
    let run = match (runner, flags.is_empty()) {
        ("yarn" | "bun", true) => format!("{runner} run {name}"),
        ("yarn" | "bun", false) => format!("{runner} run {name} {flags}"),
        (_, true) => format!("{runner} run {name}"),
        (_, false) => format!("{runner} run {name} -- {flags}"),
    };
    Some(match needs_install(dir, &package) {
        true => format!("{} && {run}", install_command(runner)),
        false => run,
    })
}

/// `package.json` lists packages and none are installed.
fn needs_install(dir: &Path, package: &Value) -> bool {
    let lists = |key: &str| package[key].as_object().is_some_and(|deps| !deps.is_empty());
    (lists("dependencies") || lists("devDependencies")) && !dir.join("node_modules").is_dir()
}

fn install_command(runner: &str) -> &'static str {
    match runner {
        "pnpm" => "pnpm install",
        "yarn" => "yarn install",
        "bun" => "bun install",
        _ => "npm install --no-audit --no-fund --loglevel=error",
    }
}

impl Previews {
    pub fn new(project: PathBuf, command: String) -> Self {
        Previews { project: Mutex::new(project), command, runs: Mutex::new(HashMap::new()) }
    }

    fn project(&self) -> PathBuf {
        self.project.lock().unwrap().clone()
    }

    /// Point later previews at another project. Running preview servers are stopped first.
    pub fn retarget(&self, project: PathBuf) {
        self.stop_all();
        *self.project.lock().unwrap() = project;
    }

    /// Whether this project can be previewed. Always: a project with no dev server is
    /// served as static files, and one with no page gets a page listing its files.
    pub fn available(&self) -> bool {
        true
    }

    pub fn status(&self, run: &str) -> Option<Status> {
        self.runs.lock().unwrap().get(run).map(|entry| entry.status.lock().unwrap().clone())
    }

    /// Start the run's dev server from `branch` (a checkout of it) or, with `None`, from
    /// the project. A second start while that server is starting or running returns it
    /// instead of spawning another.
    pub async fn start(&self, run: &str, branch: Option<&str>) -> anyhow::Result<Status> {
        if let Some(status) = self.reserve(run).await {
            return Ok(status);
        }
        // If this start returns before the process is recorded, free the slot.
        let mut claim = ClearClaim { previews: self, run, live: true };
        // Held until the server is chosen: the static server serves on it, so no other
        // process can take the port in between. Pick outside the OS ephemeral port range so
        // concurrent `bind("127.0.0.1:0")` callers cannot steal the port after we drop it for
        // an external dev server.
        let listener = reserve_port().await?;
        let port = listener.local_addr()?.port();
        let proj = self.project();
        let loaded_yaml = crate::project_config::load(&proj).ok().flatten();
        let yaml_cfg = loaded_yaml.as_ref().map(|l| &l.config);
        let (dir, tree) = match branch {
            Some(branch) => {
                let tree = git::worktree_dir(&proj, run, "preview");
                if tree.exists() {
                    let _ = git::remove_worktree(&proj, &tree);
                    let _ = std::fs::remove_dir_all(&tree);
                }
                git::add_worktree(&proj, &tree, branch)?;
                crate::project_config::apply_shared_and_copy(&proj, &tree, yaml_cfg);
                (tree.clone(), Some(tree))
            }
            None => (proj.clone(), None),
        };
        let yaml_preview = loaded_yaml
            .as_ref()
            .filter(|l| l.trusted)
            .and_then(|l| l.config.preview.as_deref())
            .map(|cmd| {
                cmd.replace("$PORT", &port.to_string())
                    .replace("${PORT}", &port.to_string())
            });
        let Some(command) = yaml_preview.or_else(|| dev_command(&dir, &self.command, port)) else {
            return self.start_static(run, listener, dir, tree, claim).await;
        };
        let within = if command.contains(" install") && command.contains(" && ") { INSTALL_WITHIN } else { START_WITHIN };
        // The dev server binds the port itself.
        drop(listener);
        let mut process = Command::new("sh");
        process
            .arg("-c")
            .arg(&command)
            .current_dir(&dir)
            .env("PORT", port.to_string())
            .env("HOST", "127.0.0.1")
            .env("BROWSER", "none")
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so stopping it stops what the script started too.
        #[cfg(unix)]
        process.process_group(0);
        let spawned = process.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(err) => {
                if let Some(tree) = &tree {
                    let _ = git::remove_worktree(&self.project(), tree);
                }
                let reason = format!("could not start `{command}`: {err}");
                self.fail_start(run, &reason);
                anyhow::bail!("{reason}");
            }
        };
        let status = Arc::new(Mutex::new(Status {
            state: "starting",
            port,
            url: format!("http://127.0.0.1:{port}/"),
            command: command.clone(),
            dir: dir.display().to_string(),
            started: rfc3339_now(),
            reason: None,
        }));
        let log = Arc::new(Mutex::new(String::new()));
        for stream in [child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>), child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)]
            .into_iter()
            .flatten()
        {
            let log = log.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut log = log.lock().unwrap();
                    log.push_str(&line);
                    log.push('\n');
                    if log.len() > KEEP_LOG * 2 {
                        let mut cut = log.len() - KEEP_LOG;
                        while !log.is_char_boundary(cut) {
                            cut += 1;
                        }
                        log.drain(..cut);
                    }
                }
            });
        }
        let pid = child.id();
        let slot: Arc<Mutex<Option<tokio::task::AbortHandle>>> = Arc::default();
        self.runs.lock().unwrap().insert(run.to_string(), Entry { status: status.clone(), pid, task: slot.clone(), log: log.clone() });
        claim.live = false;

        // Watch it: running once it answers, stopped or failed once it exits. One that dies
        // before it answers is replaced by the static server when there is a page to serve.
        let project = self.project();
        let (watched, watched_log) = (status.clone(), log);
        let served = dir.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + within;
            let mut answered = false;
            let exit = loop {
                tokio::select! {
                    exit = child.wait() => break exit.ok(),
                    _ = tokio::time::sleep(Duration::from_millis(150)) => {
                        if !answered && tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                            answered = true;
                            let mut status = watched.lock().unwrap();
                            if status.state == "starting" {
                                status.state = "running";
                            }
                        }
                        if !answered && Instant::now() > deadline {
                            let _ = child.start_kill();
                            let mut status = watched.lock().unwrap();
                            status.reason = Some(format!("it did not answer on port {port} within {} seconds", within.as_secs()));
                        }
                    }
                }
            };
            let output = tail(&watched_log.lock().unwrap());
            let asked = watched.lock().unwrap().state == "stopped";
            if !answered && !asked && crate::static_site::any_page(&served).is_some() {
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                    Ok(listener) => Some(listener),
                    Err(_) => tokio::net::TcpListener::bind("127.0.0.1:0").await.ok(),
                };
                if let Some(listener) = listener {
                    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                    let cleanup = Cleanup(project.clone(), tree.clone());
                    let root = served.clone();
                    let task = tokio::spawn(async move {
                        let _cleanup = cleanup;
                        crate::static_site::serve(listener, root).await;
                    });
                    *slot.lock().unwrap() = Some(task.abort_handle());
                    let mut status = watched.lock().unwrap();
                    if status.state == "stopped" {
                        task.abort();
                    } else {
                        let why = status.reason.take().unwrap_or_else(|| {
                            let code = exit.and_then(|e| e.code()).map(|c| format!(" with code {c}")).unwrap_or_default();
                            format!("`{}` exited{code}", status.command)
                        });
                        status.state = "running";
                        status.port = bound;
                        status.url = format!("http://127.0.0.1:{bound}/");
                        status.reason = Some(format!("{why}{}; serving the project's files instead", if output.is_empty() { String::new() } else { format!(": {output}") }));
                        status.command = "built-in static file server".into();
                    }
                    return;
                }
            }
            {
                let mut status = watched.lock().unwrap();
                let asked = status.state == "stopped";
                if !asked {
                    status.state = if answered { "stopped" } else { "failed" };
                    if status.reason.is_none() {
                        let code = exit.and_then(|e| e.code()).map(|c| format!(" with code {c}")).unwrap_or_default();
                        status.reason = Some(format!("the dev server exited{code}{}", if output.is_empty() { String::new() } else { format!(": {output}") }));
                    }
                }
            }
            if let Some(tree) = tree {
                let _ = git::remove_worktree(&project, &tree);
                let _ = std::fs::remove_dir_all(&tree);
            }
        });
        let status = status.lock().unwrap().clone();
        Ok(status)
    }

    /// Serve the project's files from this process, on `port`.
    async fn start_static(&self, run: &str, listener: tokio::net::TcpListener, dir: PathBuf, tree: Option<PathBuf>, mut claim: ClearClaim<'_>) -> anyhow::Result<Status> {
        let cleanup = Cleanup(self.project(), tree);
        let port = listener.local_addr()?.port();
        let root = dir.clone();
        let page = crate::static_site::any_page(&dir);
        let task = tokio::spawn(async move {
            let _cleanup = cleanup;
            crate::static_site::serve(listener, root).await;
        });
        let status = Arc::new(Mutex::new(Status {
            state: "running",
            port,
            url: format!("http://127.0.0.1:{port}/"),
            command: "built-in static file server".into(),
            dir: dir.display().to_string(),
            started: rfc3339_now(),
            reason: page.is_none().then(|| "no page and no dev script yet: the preview lists the project's files".to_string()),
        }));
        let log = Arc::new(Mutex::new(String::new()));
        let slot = Arc::new(Mutex::new(Some(task.abort_handle())));
        self.runs.lock().unwrap().insert(run.to_string(), Entry { status: status.clone(), pid: None, task: slot, log });
        claim.live = false;
        let status = status.lock().unwrap().clone();
        Ok(status)
    }

    /// Stop a run's dev server and everything it started. The checkout it ran from is
    /// removed as soon as the server is gone.
    pub fn stop(&self, run: &str) {
        let runs = self.runs.lock().unwrap();
        let Some(entry) = runs.get(run) else { return };
        {
            let mut status = entry.status.lock().unwrap();
            if !matches!(status.state, "starting" | "running") {
                return;
            }
            status.state = "stopped";
            status.reason = Some("stopped".into());
        }
        if let Some(pid) = entry.pid {
            kill_group(pid);
        }
        if let Some(task) = entry.task.lock().unwrap().as_ref() {
            task.abort();
        }
    }

    pub fn stop_all(&self) {
        let ids: Vec<String> = self.runs.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.stop(&id);
        }
    }

    /// Take the slot for `run` before anything is spawned. `Some` is a server that is
    /// already starting or running, and this call must not start a second one.
    async fn reserve(&self, run: &str) -> Option<Status> {
        loop {
            enum Claim {
                Mine,
                Theirs(Status),
            }
            let claim = {
                let mut runs = self.runs.lock().unwrap();
                let theirs = runs.get(run).and_then(|entry| {
                    let current = entry.status.lock().unwrap().clone();
                    matches!(current.state, "starting" | "running").then_some(current)
                });
                if let Some(current) = theirs {
                    Claim::Theirs(current)
                } else {
                    let status = Arc::new(Mutex::new(Status {
                        state: "starting",
                        port: 0,
                        url: String::new(),
                        command: String::new(),
                        dir: String::new(),
                        started: rfc3339_now(),
                        reason: None,
                    }));
                    let log = Arc::new(Mutex::new(String::new()));
                    runs.insert(run.to_string(), Entry { status, pid: None, task: Arc::default(), log });
                    Claim::Mine
                }
            };
            match claim {
                Claim::Mine => return None,
                // The owner has the slot but has not bound a port yet.
                Claim::Theirs(current) if current.port == 0 => tokio::time::sleep(Duration::from_millis(10)).await,
                Claim::Theirs(current) => return Some(current),
            }
        }
    }

    /// The slot was claimed, but no process was spawned. Let a later start try again.
    fn fail_start(&self, run: &str, reason: &str) {
        let runs = self.runs.lock().unwrap();
        let Some(entry) = runs.get(run) else { return };
        let mut status = entry.status.lock().unwrap();
        if status.state == "starting" {
            status.state = "failed";
            status.reason = Some(reason.to_string());
        }
    }

    /// The end of what a run's dev server printed.
    pub fn output(&self, run: &str) -> String {
        self.runs.lock().unwrap().get(run).map(|e| tail(&e.log.lock().unwrap())).unwrap_or_default()
    }
}

impl Drop for Previews {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn tail(text: &str) -> String {
    let text = text.trim();
    if text.len() <= 600 {
        return text.to_string();
    }
    let mut start = text.len() - 600;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

/// Frees a reserved slot when a start fails before it records a process.
struct ClearClaim<'a> {
    previews: &'a Previews,
    run: &'a str,
    live: bool,
}

impl Drop for ClearClaim<'_> {
    fn drop(&mut self) {
        if self.live {
            self.previews.fail_start(self.run, "the dev server did not start");
        }
    }
}

/// Pick a free loopback port outside Linux's default ephemeral range (`32768..60999`) using a
/// monotonic cursor so concurrent preview starts and `bind("127.0.0.1:0")` callers do not steal
/// the port between `drop(listener)` and the dev server binding it.
async fn reserve_port() -> std::io::Result<tokio::net::TcpListener> {
    use std::sync::atomic::{AtomicU16, Ordering};
    const BASE: u16 = 20_000;
    const SPAN: u16 = 11_000;
    static CURSOR: AtomicU16 = AtomicU16::new(0);
    let seed = ((std::process::id() as u16).wrapping_mul(137)) % SPAN;
    for _ in 0..64 {
        let offset = CURSOR.fetch_add(1, Ordering::Relaxed);
        let port = BASE + (seed.wrapping_add(offset) % SPAN);
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(listener);
        }
    }
    tokio::net::TcpListener::bind("127.0.0.1:0").await
}

/// Ask a process group to end, and make sure it does.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let _ = std::process::Command::new("kill").args(["-TERM", "--", &group]).stderr(Stdio::null()).status();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            let group = format!("-{pid}");
            let _ = std::process::Command::new("kill").args(["-KILL", "--", &group]).stderr(Stdio::null()).status();
        });
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reserve_port_returns_distinct_non_ephemeral_ports() {
        let a = reserve_port().await.unwrap();
        let b = reserve_port().await.unwrap();
        let pa = a.local_addr().unwrap().port();
        let pb = b.local_addr().unwrap().port();
        assert_ne!(pa, pb);
        assert!((20_000..31_000).contains(&pa), "unexpected port {pa}");
        assert!((20_000..31_000).contains(&pb), "unexpected port {pb}");
    }
}
