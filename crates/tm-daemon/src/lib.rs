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
pub mod atlas;
pub mod attachments;
pub mod auth;
mod browser;
pub mod audit;
pub mod commons;
mod checker;
mod code;
mod checks;
pub mod doctor;
pub mod egress;
mod engine;
mod eval;
pub mod eval_suite;
pub mod gates;
mod git;
mod hosts;
pub mod issues;
mod log;
pub mod jobs;
pub mod live;
pub mod lsp;
pub mod mcp_user;
pub mod memory;
pub mod merge_queue;
pub mod mesh;
pub mod metrics;
pub mod notify;
pub mod policy;
pub mod pr_loop;
pub mod preview;
pub mod project_config;
pub mod repomap;
pub mod reply;
pub mod report;
mod resume;
pub mod retention;
mod runtime;
pub mod scheduler;
mod secret;
pub mod secrets;
mod small;
mod static_site;
pub mod search;
pub mod settings;
pub mod skills;
pub mod sqlite;
pub mod store;
pub mod supply_chain;
pub mod template;
mod telemetry;
mod time;
mod turns;
pub mod provider_cache;
mod undo;
pub mod web_access;

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

/// The harness for each role. There is no built-in choice: an empty name means nobody
/// has chosen one yet, and a run cannot start until every role has a harness, either from
/// the request or from what the person saved in the settings file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Roles {
    pub worker: String,
    pub mid: String,
    pub master: String,
    pub reviewer: String,
    pub verifier: String,
}

impl Roles {
    pub const NAMES: [&'static str; 5] = ["master", "mid", "worker", "reviewer", "verifier"];

    pub fn get(&self, role: &str) -> &str {
        match role {
            "worker" => &self.worker,
            "mid" => &self.mid,
            "master" => &self.master,
            "reviewer" => &self.reviewer,
            "verifier" => &self.verifier,
            _ => "",
        }
    }

    /// Roles nobody has chosen a harness for.
    pub fn missing(&self) -> Vec<&'static str> {
        Self::NAMES.into_iter().filter(|role| self.get(role).trim().is_empty()).collect()
    }

    /// The request's choices over the saved ones. Fails, naming the roles, when any is
    /// still unchosen, because nothing here picks a provider for the person.
    pub fn resolve(
        saved: &serde_json::Map<String, serde_json::Value>,
        chosen: &serde_json::Map<String, serde_json::Value>,
    ) -> anyhow::Result<Roles> {
        let mut merged = serde_json::Map::new();
        for (role, value) in saved.iter().chain(chosen.iter()) {
            if value.as_str().is_some_and(|name| !name.trim().is_empty()) {
                merged.insert(role.clone(), value.clone());
            }
        }
        let roles: Roles = serde_json::from_value(serde_json::Value::Object(merged))?;
        let missing = roles.missing();
        if !missing.is_empty() {
            bail!("choose a provider for the {} role{}", missing.join(", "), if missing.len() == 1 { "" } else { "s" });
        }
        Ok(roles)
    }

    /// Resolve `"auto"` on the worker role using the project's stratified ledger scorecard
    /// (Item 5.3), falling back to `mid` when no model has enough history yet.
    pub fn resolve_auto(&mut self, records: &crate::store::Records, models: &mut Models, difficulty: &str, language: &str) {
        if !self.worker.eq_ignore_ascii_case("auto") {
            return;
        }
        if let Ok(Some(picked)) = records.auto_route(crate::store::SCORECARD_RUNS, Some(difficulty), Some(language))
            && HARNESSES.contains(&picked.harness.as_str())
        {
            self.worker = picked.harness;
            if models.worker.is_none() {
                models.worker = picked.model;
            }
            return;
        }
        self.worker = if !self.mid.is_empty() && !self.mid.eq_ignore_ascii_case("auto") {
            self.mid.clone()
        } else {
            self.master.clone()
        };
    }
}

/// A chosen model must be one its provider lists right now. A role with no model chosen
/// keeps the provider's default, and the provider is not asked.
pub async fn check_models(roles: &Roles, models: &Models) -> anyhow::Result<()> {
    check_models_with(roles, models, |id: &str| {
        let id = id.to_string();
        async move { provider_report(&id).await }
    })
    .await
}

/// [`check_models`] with the reports from `report_of`, such as the daemon's cache.
pub async fn check_models_with<F, Fut>(roles: &Roles, models: &Models, report_of: F) -> anyhow::Result<()>
where
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = tm_harness::ProviderReport>,
{
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
        let report = report_of(id).await;
        if let Some(note) = report.error.as_deref().filter(|_| !report.models.is_empty()) {
            eprintln!("taskmaster: {id}: {note}");
        }
        for (flag, model) in chosen {
            require_model(&report, flag, model)?;
        }
    }
    Ok(())
}

/// Who does the work. `Code` is the master planning parallel workers. `Agent` is one agent
/// owning the whole project, with no planning call: it fixes what is wrong across as many
/// files as it needs. `Auto` lets the daemon choose: a scoped edit or a follow-up goes to
/// one agent, anything bigger to the master.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    #[default]
    Auto,
    Code,
    Agent,
}

impl RunMode {
    pub fn parse(text: &str) -> Option<RunMode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(RunMode::Auto),
            "code" | "master" | "plan" => Some(RunMode::Code),
            "agent" | "single" => Some(RunMode::Agent),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            RunMode::Auto => "auto",
            RunMode::Code => "code",
            RunMode::Agent => "agent",
        }
    }
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
    /// Optional per-run USD budget cap; overrides `settings.agents.budget_usd`.
    pub max_cost_usd: Option<f64>,
    /// `next`, or a directory. See [`template`].
    pub template: Option<String>,
    /// Who does the work: see [`RunMode`]. Runs saved before modes existed are `Auto`.
    pub mode: RunMode,
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
    /// Optional per-page restart signals shared with the HTTP API.
    pub worker_restarts: Option<WorkerRestarts>,
    /// Where agents' text, thoughts and tool calls are written as they happen. `None`
    /// writes them next to the run's records.
    pub live: Option<Arc<live::Live>>,
    /// The skills store the planner may attach from. `None` attaches no skills.
    pub skills: Option<PathBuf>,
    /// Give workers on a web page a browser on that page's own preview (needs Chrome).
    pub browser: bool,
    /// Have the mid model check each finished page that is worth it (see `checker.rs`).
    pub checker: bool,
    /// Code Mode: workers get `run_code`, and harnesses run in Code Mode.
    pub code_mode: bool,
    /// Direct / Lite Fast-Path: scoped bug fixes and small edits in existing repos skip heavy
    /// Master Planning and go straight to 1 Code-Mode worker + deterministic free checks.
    pub fast_path: bool,
    /// Who does the work: one agent, the master and its workers, or the daemon's choice.
    pub mode: RunMode,
    /// Optional 2-mode cancel signal (`Graceful` drains active workers; `Force` kills active
    /// agent process groups and aborts immediately). Works alongside legacy `cancel`.
    pub cancel_mode: Option<watch::Receiver<CancelMode>>,
    /// Optional USD budget guardrail for this run; once reached, no new repairs/units spawn.
    pub max_cost_usd: Option<f64>,
    /// True when resuming a previously cancelled run (`Event::Resumed`).
    pub resume_cancelled: bool,
    /// Seconds a `run_code` script, and one `sh`/`test` call in it, may run (0 = default).
    pub code_script_secs: u64,
    pub code_sh_secs: u64,
    /// Builders whose harness runs inside the sandbox get a shell there.
    pub worker_shell: bool,
    /// Fetch tools may reach the package indexes.
    pub package_indexes: bool,
    /// Dollars per million tokens by model id or harness id, for calls with no reported price.
    pub prices: std::collections::BTreeMap<String, tm_core::Prices>,
    /// Best-of-N on `difficulty: hard` units (W3): off by default.
    pub best_of_n: bool,
    /// Optional explicit second harness for Best-of-N races (defaults to mid/master when distinct).
    pub best_of_n_alt: Option<Arc<dyn Harness>>,
}

/// How an active run should stop when cancelled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    #[default]
    None,
    /// Stop scheduling queued work and let currently active workers finish & save cleanly.
    Graceful,
    /// Immediately abort in-flight planner/workers/reviewers, kill child process groups, and clean up.
    Force,
}

impl CancelMode {
    pub fn is_cancelled(self) -> bool {
        !matches!(self, CancelMode::None)
    }

    pub fn is_force(self) -> bool {
        matches!(self, CancelMode::Force)
    }

    pub fn as_str(self) -> Option<&'static str> {
        match self {
            CancelMode::None => None,
            CancelMode::Graceful => Some("graceful"),
            CancelMode::Force => Some("force"),
        }
    }
}

/// Per-page restart signals shared between the API and a running driver.
pub type WorkerRestarts = Arc<std::sync::Mutex<std::collections::HashMap<String, watch::Sender<u64>>>>;

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
            timeout: crate::settings::NO_LIMIT,
            dictionary: None,
            commons: None,
            incremental: true,
            run_id: "current".into(),
            models: Models::default(),
            cancel,
            worker_restarts: None,
            live: None,
            skills: None,
            browser: true,
            checker: false,
            code_mode: true,
            fast_path: true,
            mode: RunMode::Auto,
            cancel_mode: None,
            max_cost_usd: None,
            resume_cancelled: false,
            code_script_secs: 0,
            code_sh_secs: 0,
            worker_shell: true,
            package_indexes: true,
            prices: crate::settings::built_in_prices(),
            best_of_n: false,
            best_of_n_alt: None,
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
