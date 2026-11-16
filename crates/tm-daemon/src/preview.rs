//! The live preview: one dev server per run, started on request and stopped with the run.
//!
//! A run that is merged (or built in place) is previewed from the project itself. A run
//! that still sits on its branch is previewed from a throwaway checkout of that branch,
//! which borrows the project's installed packages and is removed when the server stops.
//! The server listens on this machine only, on a port chosen here. Its command is the
//! settings file's `preview.command`, or the project's own `dev` script.

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
    log: Arc<Mutex<String>>,
}

/// Dev servers started for runs. Dropping the handle stops each one.
pub struct Previews {
    project: PathBuf,
    command: String,
    runs: Mutex<HashMap<String, Entry>>,
}

/// The command that starts a dev server in `dir` on `port`, from the settings' command or
/// the project's `dev` script.
pub fn dev_command(dir: &Path, configured: &str, port: u16) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.replace("{port}", &port.to_string()));
    }
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let package: Value = serde_json::from_str(&text).ok()?;
    let script = package["scripts"]["dev"].as_str()?;
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
    // Say the port the way the framework's own server wants it.
    let flags = if script.contains("next") || deps("next") {
        format!("-p {port} -H 127.0.0.1")
    } else if script.contains("vite") || deps("vite") {
        format!("--port {port} --host 127.0.0.1 --strictPort")
    } else {
        String::new()
    };
    Some(match (runner, flags.is_empty()) {
        ("yarn" | "bun", true) => format!("{runner} run dev"),
        ("yarn" | "bun", false) => format!("{runner} run dev {flags}"),
        (_, true) => format!("{runner} run dev"),
        (_, false) => format!("{runner} run dev -- {flags}"),
    })
}

impl Previews {
    pub fn new(project: PathBuf, command: String) -> Self {
        Previews { project, command, runs: Mutex::new(HashMap::new()) }
    }

    /// Whether this project has a dev server to start.
    pub fn available(&self) -> bool {
        dev_command(&self.project, &self.command, 0).is_some()
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
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            listener.local_addr()?.port()
        };
        let (dir, tree) = match branch {
            Some(branch) => {
                let tree = git::worktree_dir(&self.project, run, "preview");
                if tree.exists() {
                    let _ = git::remove_worktree(&self.project, &tree);
                    let _ = std::fs::remove_dir_all(&tree);
                }
                git::add_worktree(&self.project, &tree, branch)?;
                let installed = self.project.join("node_modules");
                #[cfg(unix)]
                if installed.is_dir() && !tree.join("node_modules").exists() {
                    let _ = std::os::unix::fs::symlink(&installed, tree.join("node_modules"));
                }
                (tree.clone(), Some(tree))
            }
            None => (self.project.clone(), None),
        };
        let Some(command) = dev_command(&dir, &self.command, port) else {
            if let Some(tree) = &tree {
                let _ = git::remove_worktree(&self.project, tree);
            }
            let reason = "this project has no dev server: add a `dev` script to package.json or set `preview.command` in the settings file";
            self.fail_start(run, reason);
            anyhow::bail!("{reason}");
        };
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
                    let _ = git::remove_worktree(&self.project, tree);
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
        self.runs.lock().unwrap().insert(run.to_string(), Entry { status: status.clone(), pid, log: log.clone() });
        claim.live = false;

        // Watch it: running once it answers, stopped or failed once it exits.
        let project = self.project.clone();
        let (watched, watched_log) = (status.clone(), log);
        tokio::spawn(async move {
            let deadline = Instant::now() + START_WITHIN;
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
                            status.reason = Some(format!("it did not answer on port {port} within {} seconds", START_WITHIN.as_secs()));
                        }
                    }
                }
            };
            let output = tail(&watched_log.lock().unwrap());
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
                    runs.insert(run.to_string(), Entry { status, pid: None, log });
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
