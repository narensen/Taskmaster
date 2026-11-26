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
            "always" | "always-ask" | "ask" | "supervised" => Some(Mode::Always),
            "risky" | "ask-risky" | "ask-for-risky" | "auto-edits" | "auto-accept-edits" | "accept-edits" => {
                Some(Mode::Risky)
            }
            "never" | "" | "off" | "auto" => Some(Mode::Never),
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

    pub fn to_permission_mode(self) -> tm_harness::PermissionMode {
        match self {
            Mode::Always => tm_harness::PermissionMode::Supervised,
            Mode::Risky => tm_harness::PermissionMode::AutoEdits,
            Mode::Never => tm_harness::PermissionMode::Auto,
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

/// O8: who may answer a worker's low-risk question before the person does. Given the
/// question, it returns a reason to allow it, or `None` to leave it to the person.
pub type Advisor = Arc<dyn Fn(Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send>> + Send + Sync>;

#[derive(Default)]
pub struct Approvals {
    policy: RwLock<Policy>,
    /// O8: the master of each run that lets it answer, when the person turned that on.
    advisors: RwLock<BTreeMap<String, Advisor>>,
    /// Per-run permission mode override set from RunRequest (A3).
    run_modes: RwLock<BTreeMap<String, tm_harness::PermissionMode>>,
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

    /// O8: let `advisor` answer this run's low-risk worker questions, or stop that.
    pub fn set_advisor(&self, run: &str, advisor: Option<Advisor>) {
        let mut advisors = self.advisors.write().unwrap();
        match advisor {
            Some(a) => {
                advisors.insert(run.to_string(), a);
            }
            None => {
                advisors.remove(run);
            }
        }
    }

    fn advisor(&self, run: &str) -> Option<Advisor> {
        self.advisors.read().unwrap().get(run).cloned()
    }

    /// Set or clear a per-run permission mode override (A3).
    pub fn set_run_mode(&self, run: &str, mode: Option<tm_harness::PermissionMode>) {
        let mut modes = self.run_modes.write().unwrap();
        match mode {
            Some(m) => {
                modes.insert(run.to_string(), m);
            }
            None => {
                modes.remove(run);
            }
        }
    }

    /// Effective `(approval_mode, full_access)` for `run`, consulting the per-run override
    /// first and falling back to the global Settings policy.
    pub fn effective_mode(&self, run: &str) -> (Mode, bool) {
        match self.run_modes.read().unwrap().get(run).copied() {
            Some(tm_harness::PermissionMode::Supervised) => (Mode::Always, false),
            Some(tm_harness::PermissionMode::AutoEdits) => (Mode::Risky, false),
            Some(tm_harness::PermissionMode::Auto) => (Mode::Never, false),
            Some(tm_harness::PermissionMode::FullAccess) => (Mode::Never, true),
            None => (self.policy().mode, false),
        }
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

    /// Ask a person about an off-allowlist `web_fetch` / `web_search` call (Item 1.3).
    /// In `Never` mode, returns `false` immediately; in `Risky` or `Always` mode, pauses
    /// as a pending `ToolKind::Fetch` approval until answered, cancelled, or timed out.
    pub async fn ask_fetch(&self, live: &Live, run: &str, agent: &Value, tool: &str, url: &str, reason: &str) -> bool {
        let policy = self.policy();
        let (mode, full_access) = self.effective_mode(run);
        if full_access {
            return true;
        }
        if ask_reason(mode, ToolKind::Fetch, false).is_none() {
            return false;
        }
        if self.stopped.lock().unwrap().contains(run) {
            return false;
        }
        let Ok(id) = random_hex(6) else { return false };
        let view = json!({
            "id": id,
            "run": run,
            "agent": agent,
            "tool": tool,
            "kind": kind_name(ToolKind::Fetch),
            "command": url,
            "paths": [url],
            "reason": reason,
            "at": rfc3339_now(),
            "timeout_secs": policy.timeout.as_secs(),
        });
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), Pending { view: view.clone(), run: run.to_string(), answer: tx });
        live.push(run, agent, "approval_requested", view);
        let (allowed, by) = match tokio::time::timeout(policy.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => (false, "cancelled".to_string()),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                (false, "timeout".to_string())
            }
        };
        live.push(run, agent, "approval_resolved", json!({ "id": id, "tool": tool, "allowed": allowed, "by": by }));
        allowed
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
        // The gate holds the browser to the run's own preview; there is nothing to ask.
        (_, ToolKind::Browser) => None,
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
        // Workers may never write into `.flow/`, even under FullAccess, AutoEdits, or Supervised modes.
        if self.inner.is_flow_edit(request) {
            return Decision::Deny;
        }
        let base = self.inner.decide(request);
        let policy = self.approvals.policy();
        let (mode, full_access) = self.approvals.effective_mode(&self.run);
        if full_access && !self.inner.is_read_only() {
            return Decision::Allow;
        }
        let Some(reason) = self.question(request, base, mode) else { return base };

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
        self.live.push(&self.run, &self.agent, "approval_requested", view.clone());
        // O8: the master may answer a worker's low-risk question first; the person can
        // still answer, and the first answer wins.
        if self.agent["role"] == "worker"
            && low_risk_candidate(request)
            && let Some(advisor) = self.approvals.advisor(&self.run)
        {
            let approvals = self.approvals.clone();
            let id = id.clone();
            tokio::spawn(async move {
                if let Some(reason) = advisor(view).await {
                    approvals.answer(&id, true, &format!("master: {reason}"));
                }
            });
        }
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

/// O8: what the master may be asked about at all. A shell command with none of the marks
/// of real harm, or a network fetch; never an edit outside the worktree or the page.
pub fn low_risk_candidate(request: &PermissionRequest) -> bool {
    const HARM: [&str; 24] = [
        "sudo", "rm -rf", "rm -fr", "| sh", "|sh", "| bash", "|bash", "curl ", "wget ", "chmod", "chown", "git push", "ssh ",
        "scp ", "> /", ">/", "mkfs", "dd ", ":(){", "shutdown", "reboot", "kill -9", " publish", "eval ",
    ];
    match request.kind {
        ToolKind::Execute => request.command.as_deref().is_some_and(|cmd| {
            let lower = cmd.to_ascii_lowercase();
            !lower.trim().is_empty() && !HARM.iter().any(|mark| lower.contains(mark))
        }),
        ToolKind::Fetch => true,
        _ => false,
    }
}

/// O8: what the master is asked about a worker's question.
pub fn advice_brief(question: &Value) -> String {
    let what = question["command"].as_str().map(|c| format!("run `{c}`")).unwrap_or_else(|| {
        let tool = question["tool"].as_str().unwrap_or("a tool");
        format!("use {tool}")
    });
    let page = question["agent"]["page"].as_str().unwrap_or("its page");
    let reason = question["reason"].as_str().unwrap_or("it needs a person's yes");
    format!(
        "You are the master of a coding run. The worker building `{page}` asks to {what}. It waits because {reason}.\n\n\
         Approve only when it is plainly low risk and part of building the page: running the project's tests, a build, a \
         linter or formatter, installing a package the plan or the project's manifest names, or reading public documentation. \
         Anything that deletes, publishes, pushes, changes the machine, or reaches somewhere unexpected is for the person.\n\n\
         Do not use any tools. Reply with one line: `APPROVE: <a short reason>` or `ASK`."
    )
}

/// The reason in an `APPROVE: …` reply, or `None` for anything else.
pub fn approval_from(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?.trim_matches('`');
    let rest = line.strip_prefix("APPROVE")?.trim_start_matches([':', ' ', '-']).trim();
    Some(if rest.is_empty() { "low risk".to_string() } else { rest.chars().take(120).collect() })
}

fn kind_name(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "read",
        ToolKind::Edit => "edit",
        ToolKind::Execute => "execute",
        ToolKind::Fetch => "fetch",
        ToolKind::Dictionary => "dictionary",
        ToolKind::Browser => "browser",
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

    #[tokio::test]
    async fn approval_gate_never_allows_or_prompts_flow_writes_even_in_full_access_or_risky() {
        let dir = std::env::temp_dir().join(format!("tm-flow-approval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".flow")).unwrap();
        let live = Arc::new(Live::new(Arc::new(crate::store::FileStore::new(dir.join("state")))));
        let agent = json!({ "id": "page:core", "role": "worker", "owned_glob": "src/**" });
        let req = PermissionRequest {
            tool: "run_code.write".into(),
            kind: ToolKind::Edit,
            paths: vec![dir.join(".flow/meta.json")],
            command: None,
            url: None,
        };

        for perm in [
            tm_harness::PermissionMode::FullAccess,
            tm_harness::PermissionMode::AutoEdits,
            tm_harness::PermissionMode::Supervised,
            tm_harness::PermissionMode::Auto,
        ] {
            live.approvals.set_run_mode("r-flow", Some(perm));
            let mut inner = WorktreeGate::new(&dir, vec![]);
            if perm == tm_harness::PermissionMode::FullAccess {
                inner = inner.with_full_access();
            }
            let gate = ApprovalGate::new(inner, live.clone(), "r-flow".into(), agent.clone());
            assert_eq!(
                gate.decide_async(&req).await,
                Decision::Deny,
                "`.flow/` edit must be unconditionally denied under {perm:?}"
            );
            assert_eq!(
                live.approvals.count("r-flow"),
                0,
                "`.flow/` edit must never queue an approval prompt under {perm:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
