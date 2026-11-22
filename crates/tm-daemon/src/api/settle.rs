//! Pin, Settle and Archive on the daemon (A8), and the daemon settling runs on its own.

use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tm_core::RunPhase;

use super::{App, Reply, fail, ok};
use crate::settling::{RunFlags, Seen};

/// How often the daemon looks for runs to settle. Cost: runs × a few small reads, six
/// times an hour, plus one `gh` call per run with an open pull request.
const SWEEP_EVERY: Duration = Duration::from_secs(600);

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FlagsBody {
    pinned: Option<bool>,
    settled: Option<bool>,
    archived: Option<bool>,
    never_settle: Option<bool>,
}

impl App {
    pub(super) fn run_flags(&self, id: &str) -> RunFlags {
        self.records().flags(id).ok().flatten().unwrap_or_default()
    }

    fn save_run_flags(&self, id: &str, flags: &RunFlags) -> anyhow::Result<()> {
        self.records().save_flags(id, flags)?;
        // The run list caches each settled run's row by its files' sizes and times, and
        // flags.json is not one of them.
        self.summary_cache.lock().unwrap().remove(&(self.project(), id.to_string()));
        Ok(())
    }

    /// Settle `id`: stop its preview and remove its leftover worktrees. `by` is `you` or
    /// `taskmaster`.
    fn settle(&self, id: &str, mut flags: RunFlags, by: &str, reason: Option<String>) -> anyhow::Result<RunFlags> {
        flags.settled = true;
        flags.settled_by = Some(by.to_string());
        flags.settled_reason = reason;
        flags.settled_at = Some(crate::time::rfc3339_now());
        self.save_run_flags(id, &flags)?;
        self.tidy(id);
        Ok(flags)
    }

    /// What a settled or archived run no longer needs: its preview and its worktrees.
    fn tidy(&self, id: &str) {
        if self.is_active(id) {
            return;
        }
        self.previews.stop(id);
        let removed = crate::settling::remove_leftover_worktrees(&self.project(), id);
        if removed > 0 {
            crate::log::note(&format!("run {id}: removed {removed} leftover worktree(s)"));
        }
    }

    /// A run the daemon settled comes back out when it starts working again.
    pub(super) fn unsettle_on_start(&self, id: &str) {
        let mut flags = self.run_flags(id);
        if flags.settled && flags.settled_by.as_deref() == Some("taskmaster") {
            flags.settled = false;
            flags.settled_by = None;
            flags.settled_reason = None;
            flags.settled_at = None;
            let _ = self.save_run_flags(id, &flags);
        }
    }

    /// Settle `id` if the rules say so. `ask_gh` also asks `gh` whether its pull request
    /// merged; the sweep does, the end of a run does not.
    pub(super) fn auto_settle(&self, id: &str, ask_gh: bool) -> Option<String> {
        if self.is_active(id) || !self.live.approvals.list(Some(id)).is_empty() {
            return None;
        }
        let run = self.replay(id).ok().flatten()?;
        if !matches!(run.phase, RunPhase::Done | RunPhase::Failed) {
            return None;
        }
        let flags = self.run_flags(id);
        if flags.settled || flags.never_settle {
            return None;
        }
        let records = self.records();
        let status = records.status(id).ok().flatten().unwrap_or_default();
        let ledger = records.ledger(id).unwrap_or_default();
        let last_at = ledger.iter().map(|entry| entry.at.as_str()).max().map(str::to_string);
        let pr_merged = ask_gh && flags.pr_url.as_deref().is_some_and(|url| crate::settling::pr_merged(&self.project(), url));
        let seen = Seen { merged: status["merged"].as_bool().unwrap_or(false), pr_merged, last_at: last_at.as_deref() };
        let rules = crate::settings::Settings::load(&self.project()).map(|s| s.settle).unwrap_or_default();
        let now = crate::time::parse_rfc3339(&crate::time::rfc3339_now()).unwrap_or(0);
        let reason = crate::settling::auto_reason(&flags, &seen, now, rules)?;
        self.settle(id, flags, "taskmaster", Some(reason.clone())).ok()?;
        self.live.notify_events(id);
        Some(reason)
    }

    /// Record the pull request a run opened, so its merge settles the run.
    pub(super) fn remember_pr(&self, id: &str, url: &str) {
        let mut flags = self.run_flags(id);
        flags.pr_url = Some(url.to_string());
        let _ = self.save_run_flags(id, &flags);
    }
}

/// Look for runs to settle every few minutes, for as long as the daemon runs.
pub(super) fn spawn_sweeper(app: Weak<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let Some(app) = app.upgrade() else { return };
            let _ = tokio::task::spawn_blocking(move || {
                for id in app.records().run_ids().unwrap_or_default() {
                    if let Some(reason) = app.auto_settle(&id, true) {
                        crate::log::note(&format!("run {id} settled: {reason}"));
                    }
                }
            })
            .await;
        }
    });
}

/// `POST /api/runs/{id}/flags`: pin, settle, archive, or keep the daemon from settling it.
pub(super) async fn set_flags(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<FlagsBody>) -> Reply {
    app.known(&id)?;
    let mut flags = app.run_flags(&id);
    let internal = |err: anyhow::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"));
    if let Some(on) = body.pinned {
        flags.pinned = on;
    }
    if let Some(on) = body.never_settle {
        flags.never_settle = on;
    }
    if let Some(on) = body.archived {
        flags.archived = on;
    }
    match body.settled {
        Some(true) if !flags.settled => {
            flags = app.settle(&id, flags, "you", None).map_err(internal)?;
        }
        Some(false) => {
            flags.settled = false;
            flags.settled_by = None;
            flags.settled_reason = None;
            flags.settled_at = None;
            app.save_run_flags(&id, &flags).map_err(internal)?;
        }
        _ => app.save_run_flags(&id, &flags).map_err(internal)?,
    }
    if body.archived == Some(true) {
        app.tidy(&id);
    }
    app.live.notify_events(&id);
    ok(json!({ "flags": flags }))
}
