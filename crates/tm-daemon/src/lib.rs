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
//!
//! Three things are behind traits so they can be replaced without touching the engine:
//! [`Store`] (files or SQLite), [`Auth`] (a pairing code, accounts, or both), and
//! [`tm_dictionary::CommonsRemote`] (another daemon over HTTP). [`settings`] chooses.
//! [`api`] is the HTTP surface a frontend talks to.

pub mod api;
pub mod approvals;
pub mod auth;
pub mod commons;
mod checks;
mod engine;
mod eval;
mod git;
pub mod live;
pub mod preview;
mod reply;
mod resume;
mod runtime;
mod secret;
mod small;
mod static_site;
pub mod search;
pub mod settings;
pub mod sqlite;
pub mod store;
pub mod template;
mod time;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use serde::{Deserialize, Serialize};
use tm_core::Plan;
use tm_dictionary::CommonsRemote;
use tm_harness::{HARNESSES, Harness, provider_report, require_model};
use tokio::sync::watch;

pub use api::{Server, ServerOptions, is_tailscale, tailscale_ipv4};
pub use auth::{AccountAuth, AnyAuth, Auth, PairingAuth, Principal};
pub use commons::HttpCommons;
pub use settings::Settings;
pub use sqlite::SqliteStore;
pub use engine::drive;
pub use eval::{EvalReport, EvalSide, evaluate};
pub use store::{FileStore, Records, Store};

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

impl Harnesses {
    /// Build the five adapters from their names, with `make` deciding what a name means.
    pub fn from_roles(
        roles: &Roles,
        make: &dyn Fn(&str) -> anyhow::Result<Arc<dyn Harness>>,
    ) -> anyhow::Result<Self> {
        Ok(Harnesses {
            worker: make(&roles.worker)?,
            mid: make(&roles.mid)?,
            master: make(&roles.master)?,
            reviewer: make(&roles.reviewer)?,
            verifier: make(&roles.verifier)?,
        })
    }
}

/// The harness name for each role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Roles {
    pub worker: String,
    pub mid: String,
    pub master: String,
    pub reviewer: String,
    pub verifier: String,
}

impl Default for Roles {
    /// Every role is Grok Build until the settings file or the request says otherwise.
    fn default() -> Self {
        let grok = || "grok".to_string();
        Roles { worker: grok(), mid: grok(), master: grok(), reviewer: grok(), verifier: grok() }
    }
}

/// A chosen model must be one its provider lists right now. A role with no model chosen
/// keeps the provider's default, and the provider is not asked.
pub async fn check_models(roles: &Roles, models: &Models) -> anyhow::Result<()> {
    let chosen = [
        ("--worker-model", roles.worker.as_str(), models.worker.as_deref()),
        ("--mid-model", roles.mid.as_str(), models.mid.as_deref()),
        ("--master-model", roles.master.as_str(), models.master.as_deref()),
        ("--reviewer-model", roles.reviewer.as_str(), models.reviewer.as_deref()),
        ("--verifier-model", roles.verifier.as_str(), models.verifier.as_deref()),
    ];
    let mut groups: std::collections::BTreeMap<&str, Vec<(&str, &str)>> = Default::default();
    for (flag, id, model) in chosen {
        let Some(model) = model else { continue };
        if !HARNESSES.contains(&id) {
            bail!("unknown harness `{id}`: use one of {}", HARNESSES.join(", "));
        }
        groups.entry(id).or_default().push((flag, model));
    }
    for (id, chosen) in groups {
        let report = provider_report(id).await;
        if let Some(note) = report.error.as_deref().filter(|_| !report.models.is_empty()) {
            eprintln!("taskmaster: {id}: {note}");
        }
        for (flag, model) in chosen {
            require_model(&report, flag, model)?;
        }
    }
    Ok(())
}

/// How a run was asked for. It is stored with the run, so a plan approved later, from
/// another process or another device, is built by the harnesses that were chosen then.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunRequest {
    pub prompt: Option<String>,
    pub plan: Option<Plan>,
    pub roles: Roles,
    pub models: Models,
    pub parallel: Option<usize>,
    pub timeout_secs: Option<u64>,
    /// `next`, or a directory. See [`template`].
    pub template: Option<String>,
}

/// Optional model names passed through to the harness. The harness id is the default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
    /// Throw away the plan that is waiting for approval. With a new `prompt` or `plan`
    /// the run plans again; without one it stops with [`Outcome::Rejected`].
    pub reject: bool,
    /// Typecheck and lint what was built, with the project's own tools, before finishing.
    pub checks: bool,
    /// Run the project's own build and tests once the type checks pass, and open each page
    /// of a Next.js app in headless Chrome. No tokens are spent on either.
    pub runtime_checks: bool,
    /// Merge the run's branch into the project when the run finishes clean. When this is
    /// off the result stays on `taskmaster/<run id>`.
    pub merge: bool,
    /// The commit a new run starts from. `None` is the project's `HEAD`.
    pub base: Option<String>,
    /// Files a new project starts with: `next`, or a directory. See [`template`].
    pub template: Option<String>,
    /// Where run records are kept. `None` is `.taskmaster/` in the project.
    pub store: Option<Arc<dyn Store>>,
    /// The Commons other people share. `None` keeps the Commons on this machine.
    pub remote: Option<Arc<dyn CommonsRemote>>,
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
    /// Where agents' text, thoughts and tool calls are written as they happen. `None`
    /// writes them next to the run's records.
    pub live: Option<Arc<live::Live>>,
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
            reject: false,
            checks: true,
            runtime_checks: true,
            merge: true,
            base: None,
            template: None,
            store: None,
            remote: None,
            timeout: Duration::from_secs(600),
            dictionary: None,
            commons: None,
            incremental: true,
            run_id: "current".into(),
            models: Models::default(),
            cancel,
            live: None,
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
    /// The waiting plan was rejected and nothing replaced it. The run is back at planning.
    Rejected,
    Finished { ok: bool },
}
