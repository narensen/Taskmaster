//! Per-tool approval: a risky call an agent makes waits here until a person answers.
//!
//! The gate that confines a worker to its worktree refuses shell commands, network
//! fetches and writes outside the worktree. With the policy `risky` those refusals become
//! questions; with `always` every call except the Dictionary's is a question; with `never`
//! (the default) nothing is asked and the refusals stand. A call that would be refused
//! for any other reason, such as an edit by a read-only reviewer, is never asked about.
//!
//! A question is a pending approval. The API lists them and takes an answer; an unanswered
//! one is refused after the timeout, and so is every one of a run that is cancelled. Each
//! question and answer is also written to the run's agent transcript, which is how the
//! event stream carries them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use tm_harness::{Decision, PermissionGate, PermissionRequest, ToolKind, WorktreeGate};
use tokio::sync::oneshot;

use crate::live::Live;
use crate::secret::random_hex;
use crate::time::rfc3339_now;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Ask before every tool call (the Dictionary's excepted).
    Always,
    /// Ask before shell commands, network fetches and writes outside the worktree.
    Risky,
    /// Never ask: what the gate refuses stays refused.
    Never,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Mode> {
        match text.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "always" | "always-ask" | "ask" => Some(Mode::Always),
            "risky" | "ask-risky" | "ask-for-risky" => Some(Mode::Risky),
            "never" | "" | "off" => Some(Mode::Never),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Mode::Always => "always",
            Mode::Risky => "risky",
            Mode::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    /// How long a question waits before it is refused.
    pub timeout: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { mode: Mode::Never, timeout: Duration::from_secs(600) }
    }
}

struct Pending {
    view: Value,
    run: String,
    answer: oneshot::Sender<(bool, String)>,
}

#[derive(Default)]
pub struct Approvals {
    policy: RwLock<Policy>,
    pending: Mutex<BTreeMap<String, Pending>>,
    /// Runs that were cancelled and have not been started again: nothing more is asked.
    stopped: Mutex<std::collections::BTreeSet<String>>,
}

impl Approvals {
    pub fn policy(&self) -> Policy {
        *self.policy.read().unwrap()
    }

    pub fn set_policy(&self, policy: Policy) {
        *self.policy.write().unwrap() = policy;
    }

    /// The questions waiting for an answer, oldest first, for one run or for all.
    pub fn list(&self, run: Option<&str>) -> Vec<Value> {
        let mut found: Vec<Value> = self
            .pending
            .lock()
            .unwrap()
            .values()
            .filter(|p| run.is_none_or(|run| p.run == run))
            .map(|p| p.view.clone())
            .collect();
        found.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()).then(a["id"].as_str().cmp(&b["id"].as_str())));
        found
    }

    pub fn count(&self, run: &str) -> usize {
        self.pending.lock().unwrap().values().filter(|p| p.run == run).count()
    }

    /// Answer a question. `false` when there is no such question (answered, timed out, or
    /// its run ended).
    pub fn answer(&self, id: &str, allow: bool, by: &str) -> Option<Value> {
        let pending = self.pending.lock().unwrap().remove(id)?;
        let view = pending.view.clone();
        let _ = pending.answer.send((allow, by.to_string()));
        Some(view)
    }

    /// Refuse every question of a run, as when it is cancelled.
    pub fn refuse_run(&self, run: &str) {
        self.stopped.lock().unwrap().insert(run.to_string());
        let ids: Vec<String> = self.pending.lock().unwrap().iter().filter(|(_, p)| p.run == run).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.answer(&id, false, "cancelled");
        }
    }

    /// A run is driven again: its agents may ask once more.
    pub fn reopen(&self, run: &str) {
        self.stopped.lock().unwrap().remove(run);
    }

    pub fn refuse_all(&self) {
        let ids: Vec<String> = self.pending.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.answer(&id, false, "cancelled");
        }
    }
}

/// The worktree gate with the policy in front of it. One per agent call.
pub struct ApprovalGate {
    inner: WorktreeGate,
    approvals: Arc<Approvals>,
    live: Arc<Live>,
    run: String,
    agent: Value,
}

impl ApprovalGate {
    pub fn new(inner: WorktreeGate, live: Arc<Live>, run: String, agent: Value) -> Self {
        ApprovalGate { approvals: live.approvals.clone(), inner, live, run, agent }
    }

    /// What to ask, or `None` when the gate's own answer stands.
    fn question(&self, request: &PermissionRequest, base: Decision, mode: Mode) -> Option<String> {
        if mode == Mode::Never || self.inner.is_read_only() {
            return None;
        }
        match (base, self.inner.risk(request)) {
            (Decision::Deny, Some(reason)) => Some(reason.to_string()),
            (Decision::Allow, _) if mode == Mode::Always && request.kind != ToolKind::Dictionary => {
                Some("asks before every tool call".to_string())
            }
            _ => None,
        }
    }
}

fn paths(request: &PermissionRequest) -> Vec<String> {
    request.paths.iter().map(|p: &PathBuf| p.display().to_string()).collect()
}

#[async_trait]
impl PermissionGate for ApprovalGate {
    fn decide(&self, request: &PermissionRequest) -> Decision {
        self.inner.decide(request)
    }

    async fn decide_async(&self, request: &PermissionRequest) -> Decision {
        let base = self.inner.decide(request);
        let policy = self.approvals.policy();
        let Some(reason) = self.question(request, base, policy.mode) else { return base };

        if self.approvals.stopped.lock().unwrap().contains(&self.run) {
            return Decision::Deny;
        }
        let Ok(id) = random_hex(6) else { return Decision::Deny };
        let view = json!({
            "id": id,
            "run": self.run,
            "agent": self.agent,
            "tool": request.tool,
            "kind": kind_name(request.kind),
            "command": request.command,
            "paths": paths(request),
            "reason": reason,
            "at": rfc3339_now(),
            "timeout_secs": policy.timeout.as_secs(),
        });
        let (tx, rx) = oneshot::channel();
        self.approvals.pending.lock().unwrap().insert(id.clone(), Pending { view: view.clone(), run: self.run.clone(), answer: tx });
        self.live.push(&self.run, &self.agent, "approval_requested", view);
        let (allowed, by) = match tokio::time::timeout(policy.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => (false, "cancelled".to_string()),
            Err(_) => {
                self.approvals.pending.lock().unwrap().remove(&id);
                (false, "timeout".to_string())
            }
        };
        self.live.push(&self.run, &self.agent, "approval_resolved", json!({ "id": id, "tool": request.tool, "allowed": allowed, "by": by }));
        if allowed { Decision::Allow } else { Decision::Deny }
    }
}

fn kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Execute => "execute",
        ToolKind::Fetch => "fetch",
        ToolKind::Dictionary => "dictionary",
        ToolKind::Other => "other",
    }
}
