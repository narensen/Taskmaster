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
        let risk = self.inner.risk(request);
        // Always asks about what the gate allows, and about the refusals worth a question.
        // Other refusals (a read outside the worktree) stay refused.
        if mode == Mode::Always && base == Decision::Deny && risk.is_none() {
            return None;
        }
        let reason = ask_reason(mode, request.kind, risk.is_some() || self.outside_glob(request))?;
        Some(risk.filter(|_| mode == Mode::Risky).unwrap_or(reason).to_string())
    }

    /// Whether an edit touches a path outside the page's owned glob. An agent with no
    /// glob (the master, in shared files) is not confined to one.
    fn outside_glob(&self, request: &PermissionRequest) -> bool {
        if request.kind != ToolKind::Edit {
            return false;
        }
        let Some(glob) = self.agent["owned_glob"].as_str().and_then(|g| globset::Glob::new(g).ok()) else { return false };
        let matcher = glob.compile_matcher();
        request.paths.iter().any(|p| self.inner.relative(p).is_none_or(|rel| !matcher.is_match(rel)))
    }
}

/// The rule: why a call must wait for a person under `mode`, or `None` to let the gate's
/// answer stand. `unsafe_edit` is true when an edit lands outside the worktree or outside
/// the page's owned glob. `Risky` asks about shell commands, network fetches and such
/// edits; reads and in-glob edits pass. `Always` asks about everything but the Dictionary.
pub fn ask_reason(mode: Mode, kind: ToolKind, unsafe_edit: bool) -> Option<&'static str> {
    match (mode, kind) {
        (Mode::Never, _) => None,
        (Mode::Always, ToolKind::Dictionary) => None,
        (Mode::Always, _) => Some("asks before every tool call"),
        (Mode::Risky, ToolKind::Execute) => Some("runs a shell command"),
        (Mode::Risky, ToolKind::Fetch) => Some("fetches from the network"),
        (Mode::Risky, ToolKind::Edit) if unsafe_edit => Some("writes outside the page's files"),
        (Mode::Risky, _) => None,
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

#[cfg(test)]
mod rule_tests {
    use super::*;

    #[test]
    fn risky_asks_for_shell_network_and_out_of_glob_writes() {
        assert!(ask_reason(Mode::Risky, ToolKind::Execute, false).is_some());
        assert!(ask_reason(Mode::Risky, ToolKind::Fetch, false).is_some());
        assert!(ask_reason(Mode::Risky, ToolKind::Edit, true).is_some());
    }

    #[test]
    fn risky_allows_reads_and_in_glob_writes() {
        assert!(ask_reason(Mode::Risky, ToolKind::Edit, false).is_none());
        assert!(ask_reason(Mode::Risky, ToolKind::Read, false).is_none());
        assert!(ask_reason(Mode::Risky, ToolKind::Dictionary, false).is_none());
    }

    #[test]
    fn always_and_never_are_unchanged() {
        assert!(ask_reason(Mode::Always, ToolKind::Read, false).is_some());
        assert!(ask_reason(Mode::Always, ToolKind::Edit, false).is_some());
        assert!(ask_reason(Mode::Always, ToolKind::Dictionary, false).is_none());
        for kind in [ToolKind::Read, ToolKind::Edit, ToolKind::Execute, ToolKind::Fetch] {
            assert!(ask_reason(Mode::Never, kind, true).is_none());
        }
    }
}
