//! The daemon: the process that carries out a [`tm_core::Run`]'s commands.
//!
//! `tm-core` decides what happens next and does no I/O. This crate does the I/O. It writes
//! the locked contracts, runs one harness agent per page in its own git worktree, merges
//! only the files that page owns, reviews Dictionary connections, verifies findings, and
//! checks the import graph. Every event is appended to `.taskmaster/runs/<id>/events.jsonl`
//! before the commands that follow it are started, so a crashed run resumes from that log.
//!
//! `--prompt` asks the master harness for the plan and its Dictionary entries. `--plan`
//! skips that and locks the file as given.

mod engine;
mod eval;
mod git;
mod phone;
mod records;
mod reply;
mod resume;
mod time;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tm_core::Plan;
use tm_harness::Harness;
use tokio::sync::watch;

pub use engine::drive;
pub use eval::{EvalReport, EvalSide, evaluate};
pub use phone::{PhoneEndpoint, is_tailscale, tailscale_ipv4};

/// Where a run's event log, change log and status file are kept, inside the project.
pub fn run_directory(project: &std::path::Path, run_id: &str) -> PathBuf {
    project.join(".taskmaster").join("runs").join(run_id)
}

/// Which harness runs each tier, plus the read-only reviewer and the finding verifier.
pub struct Harnesses {
    pub worker: Arc<dyn Harness>,
    pub mid: Arc<dyn Harness>,
    pub master: Arc<dyn Harness>,
    pub reviewer: Arc<dyn Harness>,
    pub verifier: Arc<dyn Harness>,
}

/// Optional model names passed through to the harness. The harness id is the default.
#[derive(Debug, Clone, Default)]
pub struct Models {
    pub worker: Option<String>,
    pub mid: Option<String>,
    pub master: Option<String>,
    pub reviewer: Option<String>,
    pub verifier: Option<String>,
}

/// What the daemon was asked to do.
#[derive(Clone)]
pub struct Config {
    pub project: PathBuf,
    /// Applied only when the run is still in planning. A resumed run keeps its recorded plan.
    pub plan: Option<Plan>,
    /// Free-text request. Used only when `plan` is missing and the run is still planning.
    pub prompt: Option<String>,
    /// Pages built at once. A resumed run keeps the parallelism it started with.
    pub parallel: usize,
    /// Approve the plan and build. Without this, the plan is printed and the process stops.
    pub yes: bool,
    pub timeout: Duration,
    /// Dictionary directory. Defaults to `.taskmaster/dictionary` in the project.
    pub dictionary: Option<PathBuf>,
    /// Commons directory. Defaults to `.taskmaster/commons` in the project.
    pub commons: Option<PathBuf>,
    /// Skip pages whose cited Dictionary hashes still match the last build.
    pub incremental: bool,
    pub run_id: String,
    pub models: Models,
    /// Set to `true` to stop the run at the next command boundary.
    pub cancel: watch::Receiver<bool>,
}

impl Config {
    pub fn new(project: impl Into<PathBuf>) -> Self {
        let (_, cancel) = watch::channel(false);
        Config {
            project: project.into(),
            plan: None,
            prompt: None,
            parallel: 4,
            yes: false,
            timeout: Duration::from_secs(600),
            dictionary: None,
            commons: None,
            incremental: true,
            run_id: "current".into(),
            models: Models::default(),
            cancel,
        }
    }
}

impl Clone for Harnesses {
    fn clone(&self) -> Self {
        Harnesses {
            worker: self.worker.clone(),
            mid: self.mid.clone(),
            master: self.master.clone(),
            reviewer: self.reviewer.clone(),
            verifier: self.verifier.clone(),
        }
    }
}

/// How `drive` stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The plan is recorded. Run again with `yes` to build it.
    NeedsApproval { summary: String },
    Finished { ok: bool },
}
