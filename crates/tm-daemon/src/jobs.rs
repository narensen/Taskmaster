//! Setup jobs: installing, updating and signing in to a harness CLI, or installing a
//! runtime, with the output followed as it happens.
//!
//! A job runs a fixed list of [`Step`]s chosen by `tm_harness::setup` from a provider id
//! and an action. Nothing in a request becomes part of a command. The one free input is
//! what a person types into a sign-in step's terminal, and only while that step runs.
//!
//! One job per provider or runtime at a time. Finished jobs stay readable until the daemon
//! stops, so a page opened later still shows what happened.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tm_harness::pty::{self, Key, PtyWriter};
use tm_harness::screen::{Screen, SignInPrompt, sign_in_prompt};
use tm_harness::setup::{Action, Step};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use crate::secret::random_hex;
use crate::time::rfc3339_now;

/// Lines of output a job keeps for the page.
const SHOW_LINES: usize = 400;
/// A terminal this wide keeps a long sign-in address on one line.
const COLS: u16 = 500;
const ROWS: u16 = 40;
/// Longest text a person may type into a sign-in screen at once.
pub const MAX_INPUT: usize = 512;

/// Chooses the steps for a target. Tests pass their own.
pub type Planner = Arc<dyn Fn(Target<'_>, Action) -> Option<Vec<Step>> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    Provider(&'a str),
    Runtime(&'a str),
}

pub fn default_planner() -> Planner {
    Arc::new(|target, action| match target {
        Target::Provider(id) => tm_harness::setup::provider_steps(id, action),
        Target::Runtime(id) if action == Action::Install => tm_harness::setup::runtime_steps(id),
        Target::Runtime(_) => None,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct StepView {
    pub label: String,
    pub command: String,
    /// `waiting`, `running`, `done` or `failed`.
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: String,
    /// `provider` or `runtime`.
    pub kind: &'static str,
    pub target: String,
    pub action: Action,
    /// `running`, `done`, `failed` or `cancelled`.
    pub state: &'static str,
    pub steps: Vec<StepView>,
    pub output: String,
    pub prompt: SignInPrompt,
    /// Whether typed text reaches the running step now.
    pub accepts_input: bool,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub started: String,
    pub finished: Option<String>,
}

pub struct Job {
    view: Mutex<JobView>,
    writer: Mutex<Option<PtyWriter>>,
    cancel: watch::Sender<bool>,
    changed: watch::Sender<u64>,
}

impl Job {
    pub fn view(&self) -> JobView {
        self.view.lock().unwrap().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn update(&self, change: impl FnOnce(&mut JobView)) {
        change(&mut self.view.lock().unwrap());
        self.changed.send_modify(|n| *n += 1);
    }

    /// Type into the running sign-in screen.
    pub fn input(&self, text: Option<&str>, key: Option<Key>) -> anyhow::Result<()> {
        let writer = self.writer.lock().unwrap().clone();
        let Some(writer) = writer else { anyhow::bail!("this step is not reading input") };
        if let Some(text) = text {
            writer.write(text.as_bytes())?;
        }
        if let Some(key) = key {
            writer.write(key.bytes())?;
        }
        Ok(())
    }

    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

pub struct Jobs {
    planner: Planner,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    /// The running job of each target, by `kind/target`.
    running: Mutex<HashMap<String, String>>,
}

impl Jobs {
    pub fn new(planner: Planner) -> Jobs {
        Jobs { planner, jobs: Mutex::new(HashMap::new()), running: Mutex::new(HashMap::new()) }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    /// The newest job of each target, newest first.
    pub fn latest(&self) -> Vec<JobView> {
        let mut views: Vec<JobView> = self.jobs.lock().unwrap().values().map(|job| job.view()).collect();
        views.sort_by(|a, b| b.started.cmp(&a.started));
        let mut seen = std::collections::HashSet::new();
        views.retain(|v| seen.insert(format!("{}/{}", v.kind, v.target)));
        views
    }

    pub fn stop_all(&self) {
        for job in self.jobs.lock().unwrap().values() {
            job.cancel();
        }
    }

    /// Start `action` on `target`. `Err` says why not, for the page.
    pub fn start(self: &Arc<Self>, target: Target<'_>, action: Action) -> Result<JobView, StartError> {
        let (kind, name) = match target {
            Target::Provider(id) => ("provider", id),
            Target::Runtime(id) => ("runtime", id),
        };
        let Some(steps) = (self.planner)(target, action) else {
            return Err(StartError::NotApplicable(format!("{kind} `{name}` cannot {} now", action.name())));
        };
        let slot = format!("{kind}/{name}");
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let id = format!("job-{n}-{}", random_hex(4).unwrap_or_default());
        {
            let mut running = self.running.lock().unwrap();
            if let Some(other) = running.get(&slot) {
                return Err(StartError::Busy(other.clone()));
            }
            running.insert(slot.clone(), id.clone());
        }
        let view = JobView {
            id: id.clone(),
            kind,
            target: name.to_string(),
            action,
            state: "running",
            steps: steps.iter().map(|s| StepView { label: s.label.clone(), command: s.command_line(), state: "waiting" }).collect(),
            output: String::new(),
            prompt: SignInPrompt::default(),
            accepts_input: false,
            exit_code: None,
            error: None,
            started: rfc3339_now(),
            finished: None,
        };
        let (cancel, cancelled) = watch::channel(false);
        let job = Arc::new(Job { view: Mutex::new(view.clone()), writer: Mutex::new(None), cancel, changed: watch::channel(0).0 });
        self.jobs.lock().unwrap().insert(id.clone(), job.clone());
        let jobs = self.clone();
        tokio::spawn(async move {
            let outcome = run(&job, steps, action, cancelled).await;
            *job.writer.lock().unwrap() = None;
            job.update(|v| {
                v.accepts_input = false;
                v.finished = Some(rfc3339_now());
                match outcome {
                    Ok(()) => v.state = "done",
                    Err(Stop::Cancelled) => v.state = "cancelled",
                    Err(Stop::Failed(code, why)) => {
                        v.state = "failed";
                        v.exit_code = code;
                        v.error = Some(why);
                    }
                }
            });
            if matches!(action, Action::Install | Action::Update) {
                tm_harness::setup::forget_latest();
            }
            jobs.running.lock().unwrap().remove(&slot);
        });
        Ok(view)
    }
}

#[derive(Debug)]
pub enum StartError {
    NotApplicable(String),
    /// Another job for the same target is running; its id.
    Busy(String),
}

enum Stop {
    Cancelled,
    Failed(Option<i32>, String),
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn env() -> Vec<(&'static str, std::ffi::OsString)> {
    vec![
        ("PATH", tm_harness::setup::job_path()),
        ("TERM", "xterm-256color".into()),
        ("COLUMNS", COLS.to_string().into()),
        ("LINES", ROWS.to_string().into()),
        // Installers that ask before changing a shell profile take the default.
        ("CI", "".into()),
    ]
}

async fn run(job: &Arc<Job>, steps: Vec<Step>, action: Action, mut cancelled: watch::Receiver<bool>) -> Result<(), Stop> {
    let screen = Arc::new(Mutex::new(Screen::new(ROWS, COLS)));
    let limit = if action == Action::Login { Duration::from_secs(15 * 60) } else { Duration::from_secs(20 * 60) };
    for (index, step) in steps.into_iter().enumerate() {
        job.update(|v| v.steps[index].state = "running");
        screen.lock().unwrap().feed(format!("\r\n$ {}\r\n", step.command_line()).as_bytes());
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let mut child = match start(&step, tx) {
            Ok(child) => child,
            Err(err) => {
                job.update(|v| v.steps[index].state = "failed");
                return Err(Stop::Failed(None, format!("{err:#}")));
            }
        };
        if let Some(writer) = child.writer.take() {
            *job.writer.lock().unwrap() = Some(writer);
            job.update(|v| v.accepts_input = true);
        }
        let deadline = tokio::time::Instant::now() + limit;
        let mut exit = None;
        let mut open = true;
        loop {
            tokio::select! {
                chunk = rx.recv(), if open => match chunk {
                    Some(bytes) => {
                        let (output, prompt) = {
                            let mut s = screen.lock().unwrap();
                            s.feed(&bytes);
                            (s.tail(SHOW_LINES), sign_in_prompt(&s.tail(60)))
                        };
                        job.update(|v| {
                            v.output = output;
                            if prompt.url.is_some() { v.prompt.url = prompt.url; }
                            if prompt.code.is_some() { v.prompt.code = prompt.code; }
                        });
                    }
                    None => open = false,
                },
                status = child.process.wait(), if exit.is_none() => {
                    exit = Some(status.ok().and_then(|s| s.code()));
                    // Output still in the pipe arrives shortly; then the step is over.
                    let rest = tokio::time::timeout(Duration::from_millis(300), async {
                        while let Some(bytes) = rx.recv().await { screen.lock().unwrap().feed(&bytes); }
                    }).await;
                    let _ = rest;
                    let output = screen.lock().unwrap().tail(SHOW_LINES);
                    job.update(|v| v.output = output);
                    break;
                }
                _ = cancelled.changed() => {
                    drop(child);
                    job.update(|v| v.steps[index].state = "failed");
                    return Err(Stop::Cancelled);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    drop(child);
                    job.update(|v| v.steps[index].state = "failed");
                    return Err(Stop::Failed(None, format!("{} took longer than {} minutes", step.label, limit.as_secs() / 60)));
                }
            }
        }
        *job.writer.lock().unwrap() = None;
        job.update(|v| v.accepts_input = false);
        let code = exit.flatten();
        if code != Some(0) {
            job.update(|v| v.steps[index].state = "failed");
            let why = match code {
                Some(code) => format!("{} exited with code {code}", step.label),
                None => format!("{} was stopped by a signal", step.label),
            };
            return Err(Stop::Failed(code, why));
        }
        job.update(|v| v.steps[index].state = "done");
    }
    Ok(())
}

/// A started step: its process, and the terminal's writer for a terminal step.
struct Running {
    process: tokio::process::Child,
    writer: Option<PtyWriter>,
    _guard: Option<tm_harness::GroupGuard>,
}

fn start(step: &Step, tx: mpsc::Sender<Vec<u8>>) -> anyhow::Result<Running> {
    let cwd = home();
    if step.terminal {
        let mut child = pty::spawn(std::path::Path::new(&step.program), &step.args.iter().map(String::as_str).collect::<Vec<_>>(), &cwd, env(), ROWS, COLS)?;
        let mut output = std::mem::replace(&mut child.output, mpsc::channel(1).1);
        tokio::spawn(async move {
            while let Some(bytes) = output.recv().await {
                if tx.send(bytes).await.is_err() {
                    return;
                }
            }
        });
        return Ok(Running { process: child.child, writer: Some(child.writer), _guard: Some(child.guard) });
    }
    let mut cmd = tokio::process::Command::new(&step.program);
    cmd.args(&step.args).current_dir(&cwd).envs(env()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut process, guard) = tm_harness::spawn_grouped(&mut cmd)?;
    for pipe in [process.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), process.stderr.take().map(|p| Box::new(p) as _)]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut pipe = pipe;
            let mut buf = vec![0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 || tx.send(buf[..n].to_vec()).await.is_err() {
                    return;
                }
            }
        });
    }
    Ok(Running { process, writer: None, _guard: Some(guard) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(label: &str, script: &str, terminal: bool) -> Step {
        Step { label: label.into(), program: "/bin/sh".into(), args: vec!["-c".into(), script.into()], terminal }
    }

    fn planner(steps: Vec<Step>) -> Planner {
        Arc::new(move |target, _| match target {
            Target::Provider("fake") => Some(steps.clone()),
            _ => None,
        })
    }

    async fn settle(job: &Arc<Job>) -> JobView {
        let mut changed = job.subscribe();
        for _ in 0..200 {
            let view = job.view();
            if view.state != "running" {
                return view;
            }
            let _ = tokio::time::timeout(Duration::from_millis(100), changed.changed()).await;
        }
        panic!("the job did not finish: {:?}", job.view());
    }

    #[tokio::test]
    async fn steps_run_in_order_and_stop_at_the_first_failure() {
        let jobs = Arc::new(Jobs::new(planner(vec![sh("one", "echo first", false), sh("two", "echo second >&2; exit 4", false), sh("three", "echo never", false)])));
        let view = jobs.start(Target::Provider("fake"), Action::Install).unwrap();
        let done = settle(&jobs.get(&view.id).unwrap()).await;
        assert_eq!(done.state, "failed");
        assert_eq!(done.exit_code, Some(4));
        let states: Vec<_> = done.steps.iter().map(|s| s.state).collect();
        assert_eq!(states, ["done", "failed", "waiting"]);
        assert!(done.output.contains("first") && done.output.contains("second"), "{}", done.output);
        assert!(!done.output.contains("never"));
        // The target is free again.
        assert!(jobs.start(Target::Provider("fake"), Action::Install).is_ok());
    }

    #[tokio::test]
    async fn a_sign_in_screen_shows_its_code_and_reads_what_is_typed() {
        let script = "printf '\\033[1mOpen https://auth.example.com/device\\033[0m\\nCode: WXYZ-9876\\n'; read answer; echo signed in as $answer";
        let jobs = Arc::new(Jobs::new(planner(vec![sh("Signing in", script, true)])));
        let view = jobs.start(Target::Provider("fake"), Action::Login).unwrap();
        let job = jobs.get(&view.id).unwrap();
        let mut changed = job.subscribe();
        while job.view().prompt.code.is_none() {
            tokio::time::timeout(Duration::from_secs(10), changed.changed()).await.unwrap().unwrap();
        }
        let shown = job.view();
        assert_eq!(shown.prompt.url.as_deref(), Some("https://auth.example.com/device"));
        assert_eq!(shown.prompt.code.as_deref(), Some("WXYZ-9876"));
        assert!(shown.accepts_input);
        job.input(Some("pat"), Some(Key::Enter)).unwrap();
        let done = settle(&job).await;
        assert_eq!(done.state, "done", "{done:?}");
        assert!(done.output.contains("signed in as pat"), "{}", done.output);
        assert!(!done.accepts_input);
        assert!(job.input(Some("late"), None).is_err());
    }

    #[tokio::test]
    async fn one_job_per_target_and_cancel_stops_it() {
        let jobs = Arc::new(Jobs::new(planner(vec![sh("slow", "sleep 30", false)])));
        let first = jobs.start(Target::Provider("fake"), Action::Install).unwrap();
        match jobs.start(Target::Provider("fake"), Action::Update) {
            Err(StartError::Busy(id)) => assert_eq!(id, first.id),
            other => panic!("{other:?}"),
        }
        assert!(matches!(jobs.start(Target::Provider("other"), Action::Install), Err(StartError::NotApplicable(_))));
        let job = jobs.get(&first.id).unwrap();
        job.cancel();
        assert_eq!(settle(&job).await.state, "cancelled");
        assert_eq!(jobs.latest().len(), 1);
    }
}
