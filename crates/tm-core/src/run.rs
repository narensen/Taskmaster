//! The run state machine.
//!
//! `Run::step` is pure: it takes an event, updates the state and returns the commands the
//! executor must carry out. Replaying a run's event log through `step` rebuilds its state,
//! which is how a run resumes after a crash.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::types::{DictKey, Finding, Flag, FlagResolution, PageId, PagePlan, Plan, ReviewScope};

/// Repair rounds a page gets on one tier before it escalates.
pub const MAX_REPAIRS: u8 = 2;
/// Stuck repairs a page gets when every tier is the same model: escalating would only repeat
/// the attempt, so there is no ladder, just these focused repairs on the same error and then
/// a clear failure.
pub const MAX_FOCUSED_REPAIRS: u8 = 2;
/// Total repair rounds a page gets in focused mode when each round makes real progress
/// (produces a different failure signature, e.g. build error -> runtime error -> broken image).
pub const MAX_FOCUSED_PROGRESS_REPAIRS: u8 = 6;
/// Dictionary amendments allowed per run. This cap is what bounds fix-and-revert loops.
pub const MAX_AMENDMENTS: u8 = 1;
/// Review rounds per run. After the last one the run goes straight to verification.
pub const MAX_REVIEW_ROUNDS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Planning,
    AwaitingApproval,
    Building,
    Reviewing,
    Verifying,
    Done,
    Failed,
}

/// The escalation ladder: cheap worker, then a mid-tier model, then the Taskmaster itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Worker,
    Mid,
    Master,
}

impl Tier {
    fn next(self) -> Option<Tier> {
        match self {
            Tier::Worker => Some(Tier::Mid),
            Tier::Mid => Some(Tier::Master),
            Tier::Master => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageState {
    Queued,
    Building,
    Flagged,
    Passed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageRun {
    pub state: PageState,
    pub tier: Tier,
    pub repairs: u8,
    /// Total repair attempts across this page's lifecycle in the current turn, used to cap
    /// progress-aware repair loops in focused mode.
    #[serde(default)]
    pub total_repairs: u8,
    /// Set when an entry this page cites was amended mid-build; its result is discarded.
    pub stale: bool,
    /// Check output handed to the next attempt.
    pub pending_errors: Option<String>,
    /// What the last failure was counted as (see [`failure_key`]). A repair that comes back
    /// with the same failure is stuck, and goes up a tier at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

/// The error codes, or the words, a failure is counted under, in a fixed order: two
/// failures with the same key are the same failure in different files or lines.
pub fn failure_key(errors: &str) -> String {
    let mut keys: Vec<String> = crate::errorography::classify(errors).into_iter().map(|(key, _)| key).collect();
    keys.sort();
    keys.dedup();
    keys.join("\n")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    /// The page's own checks passed.
    Passed,
    /// Typecheck or ownership check failed; `errors` goes to the repair attempt.
    CheckFailed { errors: String },
    Flagged(Flag),
    /// The attempt could not do its work for a reason another attempt would meet again
    /// (a tool the gate refuses, such as a shell or the network). The page fails at once
    /// with the reason instead of being retried.
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    PlanReady(Plan),
    PlanApproved,
    PlanRejected,
    WorkerFinished { page: PageId, outcome: WorkerOutcome },
    FlagResolved { page: PageId, key: DictKey, resolution: FlagResolution },
    /// Reviewers report; they never edit. Findings without evidence are dropped.
    ReviewFinished { findings: Vec<Finding> },
    /// A second agent checked each finding against the code; only these are real.
    FindingsVerified { confirmed: Vec<Finding> },
    /// Whole-project verification. `failing` maps a page to the errors attributed to it.
    VerifyFinished { failing: BTreeMap<PageId, String>, ok: bool },
    /// The page's citations still match the last build, so it was not rebuilt.
    PageFresh { page: PageId },
    /// The next phase of the sketch was planned: its units and contracts join the plan.
    PhaseAdded { plan: Plan },
    /// A note arrived after the run had finished. The master reads it, then the pages
    /// are built again.
    FollowUp,
    /// A note arrived after the run had finished, for one agent rather than the master and
    /// its workers. `plan` is that agent's one unit, owning the whole project: it replaces
    /// the run's plan for this turn, so nothing that already passed is built again.
    AgentTurn { plan: Plan },
    /// A cancelled run was resumed: pages that already passed stay passed, and only
    /// unfinished pages are queued again.
    Resumed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    RequestApproval,
    /// Write the contract files and the shell (layout, nav) before any worker starts.
    LockContracts,
    SpawnWorker { page: PageId, tier: Tier, errors: Option<String> },
    ArbitrateFlag(Flag),
    /// One read-only reviewer per connection, on a different model than the author.
    /// `security` lists the pages one extra reviewer checks for security problems.
    RunReview { scopes: Vec<ReviewScope>, security: Vec<PageId> },
    /// Have a second agent confirm each finding before any worker is asked to repair it.
    VerifyFindings { findings: Vec<Finding> },
    RunVerify,
    /// The current phase passed its checks: record what it landed, then plan (or take the
    /// plan already made for) phase `phase` and add it.
    NextPhase { phase: usize },
    Finish { ok: bool },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub phase: RunPhase,
    pub plan: Option<Plan>,
    pub pages: BTreeMap<PageId, PageRun>,
    pub amendments: u8,
    pub review_rounds: u8,
    /// Pages built since the last review; only connections touching them are re-reviewed.
    pub dirty: BTreeSet<PageId>,
    /// Findings awaiting verification.
    pub pending_findings: Vec<Finding>,
    pub max_parallel: usize,
    /// Every tier is the same harness and model, so a failing page is not escalated: it gets
    /// [`MAX_FOCUSED_REPAIRS`] and then fails. Set by the executor from its configuration.
    #[serde(default)]
    pub focused: bool,
    /// The phase of the sketch being built, from 0. A run with no sketch stays at 0.
    #[serde(default)]
    pub phase_at: usize,
    /// Set while the next phase is being planned, after the current one passed.
    #[serde(default)]
    pub next_phase: Option<usize>,
}

impl Run {
    pub fn new(max_parallel: usize) -> Self {
        Run {
            phase: RunPhase::Planning,
            plan: None,
            pages: BTreeMap::new(),
            amendments: 0,
            review_rounds: 0,
            dirty: BTreeSet::new(),
            pending_findings: Vec::new(),
            max_parallel: max_parallel.max(1),
            focused: false,
            phase_at: 0,
            next_phase: None,
        }
    }

    /// Rebuild a run from its event log.
    pub fn replay(max_parallel: usize, events: impl IntoIterator<Item = Event>) -> Self {
        Self::replay_with(max_parallel, false, events)
    }

    /// [`Run::replay`] for a run whose tiers are all one model (see [`Run::focused`]).
    pub fn replay_with(max_parallel: usize, focused: bool, events: impl IntoIterator<Item = Event>) -> Self {
        let mut run = Run::new(max_parallel);
        run.focused = focused;
        for event in events {
            run.step(event);
        }
        run
    }

    /// Apply one event. Events that make no sense in the current phase are ignored, so a
    /// late or duplicated event cannot corrupt a run.
    pub fn step(&mut self, event: Event) -> Vec<Command> {
        let mut out = Vec::new();
        match (self.phase, event) {
            (RunPhase::Planning | RunPhase::AwaitingApproval, Event::PlanReady(plan)) => {
                let focused = self.focused;
                self.pages = plan
                    .pages
                    .iter()
                    .map(|p| {
                        let tier = if !focused && p.is_hard() { Tier::Mid } else { Tier::Worker };
                        let page = PageRun {
                            state: PageState::Queued,
                            tier,
                            repairs: 0,
                            total_repairs: 0,
                            stale: false,
                            pending_errors: None,
                            last_failure: None,
                        };
                        (p.id.clone(), page)
                    })
                    .collect();
                self.plan = Some(plan);
                self.phase = RunPhase::AwaitingApproval;
                out.push(Command::RequestApproval);
            }
            (RunPhase::AwaitingApproval, Event::PlanApproved) => {
                self.phase = RunPhase::Building;
                out.push(Command::LockContracts);
                self.settle(&mut out);
            }
            (RunPhase::AwaitingApproval, Event::PlanRejected) => {
                self.phase = RunPhase::Planning;
                self.plan = None;
                self.pages.clear();
            }
            (RunPhase::Building, Event::PageFresh { page }) => {
                let Some(p) = self.pages.get_mut(&page) else { return out };
                if p.state != PageState::Building {
                    return out;
                }
                // Not a rebuild, so the page drops out of the review set.
                p.stale = false;
                p.state = PageState::Passed;
                self.dirty.remove(&page);
                self.settle(&mut out);
            }
            (RunPhase::Building, Event::WorkerFinished { page, outcome }) => {
                let Some(p) = self.pages.get_mut(&page) else { return out };
                if p.state != PageState::Building {
                    return out;
                }
                if p.stale {
                    // Built against an entry that has since been amended: run it again.
                    p.stale = false;
                    p.state = PageState::Queued;
                } else {
                    match outcome {
                        WorkerOutcome::Passed => p.state = PageState::Passed,
                        WorkerOutcome::CheckFailed { errors } => Self::fail_page(p, errors, self.focused),
                        WorkerOutcome::Blocked { reason } => {
                            p.state = PageState::Failed;
                            p.pending_errors = Some(reason);
                        }
                        WorkerOutcome::Flagged(flag) => {
                            p.state = PageState::Flagged;
                            out.push(Command::ArbitrateFlag(flag));
                        }
                    }
                }
                self.settle(&mut out);
            }
            (RunPhase::Building, Event::FlagResolved { page, key, resolution }) => {
                let Some(p) = self.pages.get_mut(&page) else { return out };
                if p.state != PageState::Flagged {
                    return out;
                }
                match resolution {
                    FlagResolution::Conform => p.state = PageState::Queued,
                    FlagResolution::Amended if self.amendments >= MAX_AMENDMENTS => {
                        p.state = PageState::Failed;
                    }
                    FlagResolution::Amended => {
                        self.amendments += 1;
                        p.state = PageState::Queued;
                        self.invalidate_citers(&key, &page);
                    }
                }
                self.settle(&mut out);
            }
            (RunPhase::Reviewing, Event::ReviewFinished { findings })
                if self.pending_findings.is_empty() =>
            {
                let actionable: Vec<Finding> = findings
                    .into_iter()
                    .filter(|f| f.is_actionable() && self.pages.contains_key(&f.page))
                    .collect();
                if actionable.is_empty() {
                    self.phase = RunPhase::Verifying;
                    out.push(Command::RunVerify);
                } else {
                    self.pending_findings = actionable.clone();
                    out.push(Command::VerifyFindings { findings: actionable });
                }
            }
            (RunPhase::Reviewing, Event::FindingsVerified { confirmed })
                if !self.pending_findings.is_empty() =>
            {
                // A verifier can only confirm findings it was given, never add its own.
                let pending = std::mem::take(&mut self.pending_findings);
                let mut by_page: BTreeMap<PageId, Vec<String>> = BTreeMap::new();
                for f in pending.iter().filter(|f| confirmed.contains(f)) {
                    let line = format!("review: {}:{} {}", f.file, f.line, f.failure);
                    by_page.entry(f.page.clone()).or_default().push(line);
                }
                if by_page.is_empty() {
                    self.phase = RunPhase::Verifying;
                    out.push(Command::RunVerify);
                } else {
                    self.phase = RunPhase::Building;
                    for (page, lines) in by_page {
                        let p = self.pages.get_mut(&page).expect("pending findings name known pages");
                        Self::fail_page(p, lines.join("\n"), self.focused);
                    }
                    self.settle(&mut out);
                }
            }
            (RunPhase::Verifying, Event::VerifyFinished { ok: true, .. }) => {
                let phases = self.plan.as_ref().map_or(0, |plan| plan.phases.len());
                if self.phase_at + 1 < phases {
                    // This phase is done; the next one is planned (or was, while this one
                    // built) and joins the run.
                    let next = self.phase_at + 1;
                    self.phase = RunPhase::Building;
                    self.next_phase = Some(next);
                    out.push(Command::NextPhase { phase: next });
                } else {
                    self.phase = RunPhase::Done;
                    out.push(Command::Finish { ok: true });
                }
            }
            (RunPhase::Building, Event::PhaseAdded { plan: added }) if self.next_phase.is_some() => {
                let next = self.next_phase.take().unwrap_or(self.phase_at + 1);
                self.phase_at = next;
                let Some(plan) = self.plan.as_mut() else { return out };
                let focused = self.focused;
                for mut page in added.pages {
                    if self.pages.contains_key(&page.id) {
                        continue;
                    }
                    page.phase = next;
                    let tier = if !focused && page.is_hard() { Tier::Mid } else { Tier::Worker };
                    self.pages.insert(
                        page.id.clone(),
                        PageRun { state: PageState::Queued, tier, repairs: 0, total_repairs: 0, stale: false, pending_errors: None, last_failure: None },
                    );
                    plan.pages.push(page);
                }
                for mut contract in added.contracts {
                    if contract.phase == 0 {
                        contract.phase = next;
                    }
                    if !plan.contracts.iter().any(|c| c.path == contract.path) {
                        plan.contracts.push(contract);
                    }
                }
                out.push(Command::LockContracts);
                self.settle(&mut out);
            }
            (RunPhase::Done | RunPhase::Failed, Event::AgentTurn { plan }) => {
                self.phase = RunPhase::Building;
                self.pages = plan
                    .pages
                    .iter()
                    .map(|p| {
                        let page = PageRun { state: PageState::Queued, tier: Tier::Worker, repairs: 0, total_repairs: 0, stale: false, pending_errors: None, last_failure: None };
                        (p.id.clone(), page)
                    })
                    .collect();
                self.plan = Some(plan);
                self.dirty.clear();
                self.pending_findings.clear();
                self.phase_at = 0;
                self.next_phase = None;
                out.push(Command::LockContracts);
                self.settle(&mut out);
            }
            (RunPhase::Done | RunPhase::Failed, Event::FollowUp) => {
                self.phase = RunPhase::Building;
                let ids: Vec<PageId> = self.pages.keys().cloned().collect();
                for id in ids {
                    let Some(page) = self.pages.get_mut(&id) else { continue };
                    match page.state {
                        PageState::Passed | PageState::Failed | PageState::Queued => {
                            page.state = PageState::Building;
                            page.pending_errors = None;
                            page.last_failure = None;
                            page.repairs = 0;
                            page.total_repairs = 0;
                            page.tier = Tier::Worker;
                            page.stale = false;
                            self.dirty.insert(id.clone());
                            out.push(Command::SpawnWorker { page: id, tier: Tier::Worker, errors: None });
                        }
                        PageState::Building => {
                            let tier = page.tier;
                            let errors = page.pending_errors.clone();
                            self.dirty.insert(id.clone());
                            out.push(Command::SpawnWorker { page: id, tier, errors });
                        }
                        PageState::Flagged => {}
                    }
                }
            }
            (RunPhase::Verifying, Event::VerifyFinished { ok: false, failing }) => {
                if failing.is_empty() {
                    // Nothing to attribute the failure to, so no worker can repair it.
                    self.phase = RunPhase::Failed;
                    out.push(Command::Finish { ok: false });
                } else {
                    self.phase = RunPhase::Building;
                    for (page, errors) in failing {
                        if let Some(p) = self.pages.get_mut(&page) {
                            Self::fail_page(p, errors, self.focused);
                        }
                    }
                    self.settle(&mut out);
                }
            }
            (RunPhase::Failed, Event::Resumed) => {
                if self.plan.is_none() {
                    self.phase = RunPhase::Planning;
                } else if self.pages.is_empty() {
                    self.phase = RunPhase::AwaitingApproval;
                } else {
                    self.phase = RunPhase::Building;
                    for page in self.pages.values_mut() {
                        if page.state != PageState::Passed {
                            page.state = PageState::Queued;
                            page.repairs = 0;
                            page.total_repairs = 0;
                            page.stale = false;
                        }
                    }
                    self.settle(&mut out);
                }
            }
            (RunPhase::Done | RunPhase::Failed, _) => {}
            (_, Event::Cancelled) => {
                self.phase = RunPhase::Failed;
                out.push(Command::Finish { ok: false });
            }
            _ => {}
        }
        out
    }

    /// Repair on the same tier, then escalate, then give up. A repair that fails exactly as
    /// the attempt before it is stuck: it escalates at once rather than spending the rest of
    /// its tier's repairs on the same mistake. With `focused` (one model on every tier) there
    /// is nothing to escalate to: stuck failures stop after `MAX_FOCUSED_REPAIRS`, while
    /// distinct failures that show real progress (e.g. build error -> runtime error -> broken
    /// image) continue converging up to `MAX_FOCUSED_PROGRESS_REPAIRS`.
    fn fail_page(p: &mut PageRun, errors: String, focused: bool) {
        let key = failure_key(&errors);
        let had_previous = p.last_failure.is_some();
        let stuck = !key.is_empty() && p.last_failure.as_deref() == Some(key.as_str());
        let progressed = had_previous && !stuck && !key.is_empty();
        p.last_failure = Some(key);
        if stuck
            && !focused
            && let Some(next) = p.tier.next()
        {
            p.tier = next;
            p.repairs = 0;
            p.total_repairs = p.total_repairs.saturating_add(1);
            p.state = PageState::Queued;
            p.pending_errors = Some(errors);
            return;
        }
        if focused {
            if progressed {
                p.repairs = 0;
            }
            if p.repairs < MAX_FOCUSED_REPAIRS && p.total_repairs < MAX_FOCUSED_PROGRESS_REPAIRS {
                p.repairs += 1;
                p.total_repairs += 1;
                p.state = PageState::Queued;
                p.pending_errors = Some(errors);
            } else {
                p.state = PageState::Failed;
            }
            return;
        }
        if p.repairs < MAX_REPAIRS {
            p.repairs += 1;
            p.total_repairs = p.total_repairs.saturating_add(1);
        } else if let Some(next) = p.tier.next() {
            p.tier = next;
            p.repairs = 0;
            p.total_repairs = p.total_repairs.saturating_add(1);
        } else {
            p.state = PageState::Failed;
            return;
        }
        p.state = PageState::Queued;
        p.pending_errors = Some(errors);
    }

    /// After an entry is amended, every other page citing it must be rebuilt.
    fn invalidate_citers(&mut self, key: &DictKey, except: &PageId) {
        let Some(plan) = &self.plan else { return };
        for page in plan.pages.iter().filter(|p| &p.id != except && p.cites.contains(key)) {
            if let Some(p) = self.pages.get_mut(&page.id) {
                match p.state {
                    PageState::Passed => p.state = PageState::Queued,
                    PageState::Building => p.stale = true,
                    PageState::Queued | PageState::Flagged | PageState::Failed => {}
                }
            }
        }
    }

    /// Start queued pages up to the parallel limit, prioritizing critical-path units (longest
    /// downstream dependency chain, tie-breaking by estimated size), then advance the phase if
    /// nothing is left.
    ///
    /// Independent unblocked units (no unsatisfied dependency edge and no overlapping owned files)
    /// share a wave up to `max_parallel` and are never starved by blocked units sitting in the
    /// daemon's pending queue waiting on earlier dependencies.
    fn settle(&mut self, out: &mut Vec<Command>) {
        let Some(plan) = self.plan.clone() else { return };
        let is_unblocked = |page: &crate::types::PagePlan, pages: &BTreeMap<PageId, PageRun>| {
            page.depends_on.iter().all(|dep| {
                dep == &page.id
                    || pages.get(dep).is_none_or(|st| st.state == PageState::Passed)
            })
        };

        let mut unblocked_building = 0usize;
        let mut active_globs: Vec<String> = Vec::new();
        let mut all_building_globs: Vec<String> = Vec::new();
        for p in &plan.pages {
            if self.pages.get(&p.id).is_some_and(|st| st.state == PageState::Building) {
                all_building_globs.push(p.owned_glob.clone());
                if is_unblocked(p, &self.pages) {
                    unblocked_building += 1;
                    active_globs.push(p.owned_glob.clone());
                }
            }
        }

        let downstream = downstream_chain_lengths(&plan);
        let mut unblocked: Vec<(usize, usize, usize, PageId, String)> = Vec::new();
        let mut blocked: Vec<(usize, usize, usize, PageId, String)> = Vec::new();
        // Units of a later phase wait for theirs; a repair of an earlier one may run.
        for (idx, p) in plan.pages.iter().enumerate() {
            if p.phase > self.phase_at {
                continue;
            }
            if !self.pages.get(&p.id).is_some_and(|st| st.state == PageState::Queued) {
                continue;
            }
            let chain = downstream.get(&p.id).copied().unwrap_or(0);
            let size = estimated_unit_size(p);
            let item = (chain, size, idx, p.id.clone(), p.owned_glob.clone());
            if is_unblocked(p, &self.pages) {
                unblocked.push(item);
            } else {
                blocked.push(item);
            }
        }
        let sort_key = |a: &(usize, usize, usize, PageId, String), b: &(usize, usize, usize, PageId, String)| {
            b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)).then_with(|| a.2.cmp(&b.2))
        };
        unblocked.sort_by(sort_key);
        blocked.sort_by(sort_key);

        // Pass 1: fill wave slots with all independent unblocked units whose owned files do not
        // overlap an already-active unit.
        for (_, _, _, id, glob) in unblocked {
            if unblocked_building >= self.max_parallel {
                break;
            }
            if active_globs.iter().any(|g| crate::types::owned_globs_overlap(g, &glob)) {
                continue;
            }
            let p = self.pages.get_mut(&id).expect("every planned page has a PageRun");
            if p.state == PageState::Queued {
                p.state = PageState::Building;
                self.dirty.insert(id.clone());
                unblocked_building += 1;
                active_globs.push(glob.clone());
                all_building_globs.push(glob);
                out.push(Command::SpawnWorker {
                    page: id,
                    tier: p.tier,
                    errors: p.pending_errors.take(),
                });
            }
        }

        // Pass 2: when locked interface contracts exist and spare slots remain, speculatively
        // hand non-overlapping blocked units to the daemon so contract-only dependents can start
        // in parallel with their upstream. Because Pass 1 counts only `unblocked_building`, these
        // speculative entries never starve newly-unblocked independent units later.
        let has_interface_contracts = plan.contracts.iter().any(|c| !c.is_test());
        let mut total_building = self.count(PageState::Building);
        if has_interface_contracts && total_building < self.max_parallel {
            for (_, _, _, id, glob) in &blocked {
                if total_building >= self.max_parallel {
                    break;
                }
                if all_building_globs.iter().any(|g| crate::types::owned_globs_overlap(g, glob)) {
                    continue;
                }
                let p = self.pages.get_mut(id).expect("every planned page has a PageRun");
                if p.state == PageState::Queued {
                    p.state = PageState::Building;
                    self.dirty.insert(id.clone());
                    total_building += 1;
                    all_building_globs.push(glob.clone());
                    out.push(Command::SpawnWorker {
                        page: id.clone(),
                        tier: p.tier,
                        errors: p.pending_errors.take(),
                    });
                }
            }
        }

        // Fallback for cyclic dependency graphs where nothing is unblocked and nothing failed:
        // start non-overlapping queued units so the run never skips queued work.
        let mut open = self.count(PageState::Building) + self.count(PageState::Flagged);
        if open == 0 && self.count(PageState::Failed) == 0 && !blocked.is_empty() {
            let mut started = 0usize;
            for (_, _, _, id, glob) in blocked {
                if started >= self.max_parallel {
                    break;
                }
                if all_building_globs.iter().any(|g| crate::types::owned_globs_overlap(g, &glob)) {
                    continue;
                }
                let p = self.pages.get_mut(&id).expect("every planned page has a PageRun");
                if p.state == PageState::Queued {
                    p.state = PageState::Building;
                    self.dirty.insert(id.clone());
                    started += 1;
                    all_building_globs.push(glob);
                    out.push(Command::SpawnWorker {
                        page: id,
                        tier: p.tier,
                        errors: p.pending_errors.take(),
                    });
                }
            }
            open = self.count(PageState::Building) + self.count(PageState::Flagged);
        }
        if open == 0 {
            if self.count(PageState::Failed) == 0 {
                self.after_build(out);
            } else {
                self.phase = RunPhase::Failed;
                out.push(Command::Finish { ok: false });
            }
        }
    }

    /// Every page passed its own checks: go straight to the deterministic checks. Review is
    /// not part of a run; a person asks for it afterwards (`POST /api/runs/{id}/review`).
    fn after_build(&mut self, out: &mut Vec<Command>) {
        self.dirty.clear();
        self.phase = RunPhase::Verifying;
        out.push(Command::RunVerify);
    }

    fn count(&self, state: PageState) -> usize {
        self.pages.values().filter(|p| p.state == state).count()
    }
}

/// Longest downstream dependency chain length (in edges) from each unit in `plan`.
/// A unit with no dependents has length 0; `A <- B` gives `A: 1, B: 0`; `A <- B <- C` gives
/// `A: 2, B: 1, C: 0`. Cycles are broken safely.
pub fn downstream_chain_lengths(plan: &Plan) -> BTreeMap<PageId, usize> {
    let known: BTreeSet<&str> = plan.pages.iter().map(|p| p.id.as_str()).collect();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for page in &plan.pages {
        for dep in &page.depends_on {
            if dep.as_str() != page.id.as_str() && known.contains(dep.as_str()) {
                dependents.entry(dep.as_str()).or_default().push(page.id.as_str());
            }
        }
    }
    fn longest<'a>(
        id: &'a str,
        dependents: &BTreeMap<&'a str, Vec<&'a str>>,
        visiting: &mut BTreeSet<&'a str>,
        memo: &mut BTreeMap<&'a str, usize>,
    ) -> usize {
        if let Some(&len) = memo.get(id) {
            return len;
        }
        if !visiting.insert(id) {
            return 0;
        }
        let mut best = 0usize;
        if let Some(children) = dependents.get(id) {
            for child in children {
                if !visiting.contains(child) {
                    best = best.max(1 + longest(child, dependents, visiting, memo));
                }
            }
        }
        visiting.remove(id);
        memo.insert(id, best);
        best
    }
    let mut memo = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    plan.pages
        .iter()
        .map(|p| {
            let len = longest(p.id.as_str(), &dependents, &mut visiting, &mut memo);
            (p.id.clone(), len)
        })
        .collect()
}

/// Estimated work size for tie-breaking critical-path wave scheduling: combines owned file
/// count (brace alternatives / split sub-units) with specification length (`brief`, `context`,
/// `done`, and sub-unit specs). Units with identical glob cardinality and empty specs tie.
pub fn estimated_unit_size(page: &PagePlan) -> usize {
    let file_count = glob_item_count(&page.owned_glob).max(page.units.len());
    let spec_len = page.brief.len()
        + page.context.len()
        + page.done.len()
        + page
            .units
            .iter()
            .map(|u| u.brief.len() + u.context.len() + u.done.len())
            .sum::<usize>();
    spec_len.saturating_add(file_count.saturating_mul(256))
}

fn glob_item_count(glob: &str) -> usize {
    let trimmed = glob.trim();
    if trimmed.is_empty() {
        return 0;
    }
    if let Some(inner) = trimmed.strip_prefix('{').and_then(|g| g.strip_suffix('}'))
        && !inner.contains(['{', '}'])
    {
        return inner.split(',').filter(|s| !s.trim().is_empty()).count();
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PagePlan;

    fn plan(pages: &[(&str, &[&str])]) -> Plan {
        Plan {
            pages: pages
                .iter()
                .map(|(id, cites)| PagePlan {
                    id: id.to_string(),
                    route: format!("/{id}"),
                    owned_glob: format!("app/{id}/**"),
                    cites: cites.iter().map(|c| c.to_string()).collect(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn started(pages: &[(&str, &[&str])], parallel: usize) -> (Run, Vec<Command>) {
        let mut run = Run::new(parallel);
        assert_eq!(run.step(Event::PlanReady(plan(pages))), vec![Command::RequestApproval]);
        let cmds = run.step(Event::PlanApproved);
        (run, cmds)
    }

    fn spawn(page: &str, tier: Tier, errors: Option<&str>) -> Command {
        Command::SpawnWorker { page: page.into(), tier, errors: errors.map(Into::into) }
    }

    fn finished(page: &str, outcome: WorkerOutcome) -> Event {
        Event::WorkerFinished { page: page.into(), outcome }
    }

    fn check_failed(msg: &str) -> WorkerOutcome {
        WorkerOutcome::CheckFailed { errors: msg.into() }
    }

    #[test]
    fn happy_path_runs_to_done() {
        let (mut run, cmds) = started(&[("a", &[]), ("b", &[])], 4);
        assert_eq!(
            cmds,
            vec![
                Command::LockContracts,
                spawn("a", Tier::Worker, None),
                spawn("b", Tier::Worker, None)
            ]
        );
        assert!(run.step(finished("a", WorkerOutcome::Passed)).is_empty());
        // No review phase: the run goes straight to the deterministic checks.
        assert_eq!(run.step(finished("b", WorkerOutcome::Passed)), vec![Command::RunVerify]);
        assert_eq!(run.phase, RunPhase::Verifying);
        let done = run.step(Event::VerifyFinished { ok: true, failing: BTreeMap::new() });
        assert_eq!(done, vec![Command::Finish { ok: true }]);
        assert_eq!(run.phase, RunPhase::Done);
        let again = run.step(Event::FollowUp);
        assert_eq!(run.phase, RunPhase::Building);
        assert_eq!(again, vec![spawn("a", Tier::Worker, None), spawn("b", Tier::Worker, None)]);
        assert!(run.pages.values().all(|page| page.state == PageState::Building));
        assert!(run.step(Event::FollowUp).is_empty(), "a follow-up while building waits");
    }

    #[test]
    fn a_hard_unit_starts_on_the_mid_tier_unless_every_tier_is_one_model() {
        let mut planned = plan(&[("easy", &[]), ("core", &[])]);
        planned.pages[0].difficulty = Some("easy".into());
        planned.pages[1].difficulty = Some("Hard".into());
        let mut run = Run::new(4);
        run.step(Event::PlanReady(planned.clone()));
        let cmds = run.step(Event::PlanApproved);
        assert_eq!(cmds, vec![Command::LockContracts, spawn("easy", Tier::Worker, None), spawn("core", Tier::Mid, None)]);
        assert_eq!(planned.pages[0].effort_hint(), Some("low"));
        assert_eq!(planned.pages[1].effort_hint(), Some("high"));
        planned.pages[0].difficulty = Some("normal".into());
        assert_eq!(planned.pages[0].effort_hint(), None, "a normal unit keeps the harness's default");

        let mut focused = Run::new(4);
        focused.focused = true;
        focused.step(Event::PlanReady(planned));
        let cmds = focused.step(Event::PlanApproved);
        assert_eq!(cmds[2], spawn("core", Tier::Worker, None), "one model has no stronger tier to start on");
    }

    #[test]
    fn parallel_limit_is_respected() {
        let (mut run, cmds) = started(&[("a", &[]), ("b", &[]), ("c", &[])], 2);
        assert_eq!(cmds.len(), 3, "lock plus two spawns: {cmds:?}");
        assert_eq!(run.pages["c"].state, PageState::Queued);
        let next = run.step(finished("a", WorkerOutcome::Passed));
        assert_eq!(next, vec![spawn("c", Tier::Worker, None)]);
    }

    #[test]
    fn page_repairs_then_escalates_then_fails() {
        let (mut run, _) = started(&[("a", &[])], 1);
        let mut seen = Vec::new();
        for n in 0.. {
            // A different failure each time: every repair made progress, so none is stuck.
            let failure = format!("app/a/page.ts:1 tsc: TS{}: wrong", 2300 + n);
            let cmds = run.step(finished("a", check_failed(&failure)));
            match cmds.as_slice() {
                [Command::SpawnWorker { tier, errors, .. }] => {
                    assert_eq!(errors.as_deref(), Some(failure.as_str()));
                    seen.push(*tier);
                }
                [Command::Finish { ok: false }] => break,
                other => panic!("unexpected commands: {other:?}"),
            }
        }
        // Two repairs per tier, then the first attempt on the next tier.
        use Tier::*;
        assert_eq!(seen, vec![Worker, Worker, Mid, Mid, Mid, Master, Master, Master]);
        assert_eq!(run.phase, RunPhase::Failed);
    }

    #[test]
    fn a_repair_that_fails_the_same_way_goes_up_a_tier_at_once() {
        let (mut run, _) = started(&[("a", &[])], 1);
        let mut seen = Vec::new();
        loop {
            // The same mistake each time, in another file and line.
            let line = seen.len() + 1;
            let failure = format!("app/a/x{line}.ts:{line} tsc: TS2322: Type 'string' is not assignable to type 'number'.");
            match run.step(finished("a", check_failed(&failure))).as_slice() {
                [Command::SpawnWorker { tier, .. }] => seen.push(*tier),
                [Command::Finish { ok: false }] => break,
                other => panic!("unexpected commands: {other:?}"),
            }
        }
        use Tier::*;
        // One repair on the worker, then each stuck repair goes straight up; the master,
        // with nothing above it, keeps its repairs.
        assert_eq!(seen, vec![Worker, Mid, Master, Master, Master]);
    }

    #[test]
    fn one_model_on_every_tier_gets_focused_repairs_and_no_ladder() {
        let (mut run, _) = started(&[("a", &[])], 1);
        run.focused = true;
        let mut seen = Vec::new();
        loop {
            match run.step(finished("a", check_failed("1 failed"))).as_slice() {
                [Command::SpawnWorker { tier, .. }] => seen.push(*tier),
                [Command::Finish { ok: false }] => break,
                other => panic!("unexpected commands: {other:?}"),
            }
        }
        assert_eq!(seen, vec![Tier::Worker; MAX_FOCUSED_REPAIRS as usize], "same model, so no escalation");
        assert_eq!(run.phase, RunPhase::Failed);
    }

    #[test]
    fn a_sketched_run_builds_one_phase_at_a_time() {
        let mut first = plan(&[("protocol", &[])]);
        first.phases = vec![
            crate::types::PhaseSketch { id: "protocol".into(), goal: "parse".into(), ..Default::default() },
            crate::types::PhaseSketch { id: "server".into(), goal: "serve".into(), ..Default::default() },
        ];
        let mut run = Run::new(4);
        run.step(Event::PlanReady(first));
        run.step(Event::PlanApproved);
        assert_eq!(run.step(finished("protocol", WorkerOutcome::Passed)), vec![Command::RunVerify]);
        // The first phase passed: the run asks for the next one instead of finishing.
        let next = run.step(Event::VerifyFinished { ok: true, failing: BTreeMap::new() });
        assert_eq!(next, vec![Command::NextPhase { phase: 1 }]);
        assert_eq!((run.phase, run.next_phase), (RunPhase::Building, Some(1)));
        let added = plan(&[("server", &[]), ("protocol", &[])]);
        let cmds = run.step(Event::PhaseAdded { plan: added });
        assert_eq!(cmds, vec![Command::LockContracts, spawn("server", Tier::Worker, None)], "a known id is not added twice");
        assert_eq!(run.phase_at, 1);
        assert_eq!(run.plan.as_ref().unwrap().pages.iter().find(|p| p.id == "server").unwrap().phase, 1);
        assert_eq!(run.step(finished("server", WorkerOutcome::Passed)), vec![Command::RunVerify]);
        assert_eq!(run.step(Event::VerifyFinished { ok: true, failing: BTreeMap::new() }), vec![Command::Finish { ok: true }]);
    }

    #[test]
    fn a_blocked_page_fails_at_once_with_its_reason() {
        let (mut run, _) = started(&[("a", &[])], 1);
        let cmds = run.step(finished("a", WorkerOutcome::Blocked { reason: "needs a shell: Bash `poetry lock`".into() }));
        assert_eq!(cmds, vec![Command::Finish { ok: false }]);
        assert_eq!(run.pages["a"].state, PageState::Failed);
        assert!(run.pages["a"].pending_errors.as_deref().unwrap().contains("poetry lock"));
    }

    #[test]
    fn amendment_rebuilds_citing_pages_and_is_capped() {
        let pages: &[(&str, &[&str])] = &[("a", &["auth"]), ("b", &["auth"]), ("c", &["nav"])];
        let (mut run, _) = started(pages, 4);
        run.step(finished("b", WorkerOutcome::Passed));
        run.step(finished("c", WorkerOutcome::Passed));

        let flag = Flag { page: "a".into(), key: "auth".into(), issue: "missing role".into() };
        let cmds = run.step(finished("a", WorkerOutcome::Flagged(flag.clone())));
        assert_eq!(cmds, vec![Command::ArbitrateFlag(flag.clone())]);

        let amended = |page: &str| Event::FlagResolved {
            page: page.into(),
            key: "auth".into(),
            resolution: FlagResolution::Amended,
        };
        // `b` cites the amended entry and is rebuilt; `c` does not and keeps its result.
        let cmds = run.step(amended("a"));
        assert_eq!(cmds, vec![spawn("a", Tier::Worker, None), spawn("b", Tier::Worker, None)]);
        assert_eq!(run.pages["c"].state, PageState::Passed);

        // A second amendment in the same run is refused: the flagging page fails instead.
        run.step(finished("b", WorkerOutcome::Passed));
        run.step(finished("a", WorkerOutcome::Flagged(flag)));
        let cmds = run.step(amended("a"));
        assert_eq!(cmds, vec![Command::Finish { ok: false }]);
        assert_eq!(run.pages["a"].state, PageState::Failed);
    }

    #[test]
    fn result_built_against_an_amended_entry_is_discarded() {
        let (mut run, _) = started(&[("a", &["auth"]), ("b", &["auth"])], 4);
        let flag = Flag { page: "a".into(), key: "auth".into(), issue: "x".into() };
        run.step(finished("a", WorkerOutcome::Flagged(flag)));
        run.step(Event::FlagResolved {
            page: "a".into(),
            key: "auth".into(),
            resolution: FlagResolution::Amended,
        });
        assert!(run.pages["b"].stale);
        // `b` was mid-build when the entry changed, so its pass does not count.
        let cmds = run.step(finished("b", WorkerOutcome::Passed));
        assert_eq!(cmds, vec![spawn("b", Tier::Worker, None)]);
    }

    #[test]
    fn an_unchanged_page_is_not_rebuilt_and_the_run_verifies() {
        let (mut run, _) = started(&[("a", &["auth"]), ("b", &["nav"])], 4);
        run.step(Event::PageFresh { page: "a".into() });
        assert_eq!(run.pages["a"].state, PageState::Passed);
        let cmds = run.step(finished("b", WorkerOutcome::Passed));
        assert_eq!(cmds, vec![Command::RunVerify]);
    }

    #[test]
    fn verify_failure_sends_the_page_back_for_repair() {
        let (mut run, _) = started(&[("a", &[]), ("b", &[])], 4);
        run.step(finished("a", WorkerOutcome::Passed));
        run.step(finished("b", WorkerOutcome::Passed));
        let failing = BTreeMap::from([("b".to_string(), "route /b returned 500".to_string())]);
        let cmds = run.step(Event::VerifyFinished { ok: false, failing });
        assert_eq!(cmds, vec![spawn("b", Tier::Worker, Some("route /b returned 500"))]);
        assert_eq!(run.phase, RunPhase::Building);
    }

    #[test]
    fn replay_rebuilds_the_same_state() {
        let events = vec![
            Event::PlanReady(plan(&[("a", &[]), ("b", &[])])),
            Event::PlanApproved,
            finished("a", check_failed("e")),
            finished("b", WorkerOutcome::Passed),
            finished("a", WorkerOutcome::Passed),
        ];
        let mut live = Run::new(2);
        for e in events.clone() {
            live.step(e);
        }
        assert_eq!(Run::replay(2, events), live);
        assert_eq!(live.phase, RunPhase::Verifying);
    }

    #[test]
    fn stray_events_are_ignored() {
        let mut run = Run::new(1);
        assert!(run.step(Event::PlanApproved).is_empty());
        assert!(run.step(finished("ghost", WorkerOutcome::Passed)).is_empty());
        assert_eq!(run.phase, RunPhase::Planning);
    }

    #[test]
    fn an_agent_turn_builds_one_unit_and_leaves_the_old_pages_alone() {
        let mut run = Run::new(4);
        run.step(Event::PlanReady(plan(&[("a", &[]), ("b", &[])])));
        run.step(Event::PlanApproved);
        run.step(Event::WorkerFinished { page: "a".into(), outcome: WorkerOutcome::Passed });
        run.step(Event::WorkerFinished { page: "b".into(), outcome: WorkerOutcome::Blocked { reason: "no shell".into() } });
        assert_eq!(run.phase, RunPhase::Failed);

        let mut agent = plan(&[("agent-1", &[])]);
        agent.pages[0].owned_glob = "**".into();
        let out = run.step(Event::AgentTurn { plan: agent });
        assert_eq!(run.phase, RunPhase::Building);
        assert_eq!(
            out,
            vec![Command::LockContracts, Command::SpawnWorker { page: "agent-1".into(), tier: Tier::Worker, errors: None }]
        );
        assert_eq!(run.pages.keys().collect::<Vec<_>>(), vec!["agent-1"], "the old pages are not rebuilt");

        let out = run.step(Event::WorkerFinished { page: "agent-1".into(), outcome: WorkerOutcome::Passed });
        assert_eq!(out, vec![Command::RunVerify]);
        let out = run.step(Event::VerifyFinished { failing: BTreeMap::new(), ok: true });
        assert_eq!(out, vec![Command::Finish { ok: true }]);
        assert_eq!(run.phase, RunPhase::Done);
        // Only a finished run takes one.
        assert!(!run.clone().step(Event::AgentTurn { plan: plan(&[("x", &[])]) }).is_empty());
        let mut building = Run::new(1);
        building.step(Event::PlanReady(plan(&[("a", &[])])));
        assert!(building.step(Event::AgentTurn { plan: plan(&[("x", &[])]) }).is_empty());
    }

    #[test]
    fn resuming_a_cancelled_run_keeps_passed_pages_and_rebuilds_the_rest() {
        let (mut run, _) = started(&[("a", &[]), ("b", &[]), ("c", &[])], 2);
        run.step(finished("a", WorkerOutcome::Passed));
        // Cancel while `b` is still building and `c` is queued.
        let stop = run.step(Event::Cancelled);
        assert_eq!(stop, vec![Command::Finish { ok: false }]);
        assert_eq!(run.phase, RunPhase::Failed);
        assert_eq!(run.pages["a"].state, PageState::Passed);

        // Resuming keeps `a` passed and only spawns `b` and `c`.
        let cmds = run.step(Event::Resumed);
        assert_eq!(run.phase, RunPhase::Building);
        assert_eq!(run.pages["a"].state, PageState::Passed);
        assert_eq!(cmds, vec![spawn("b", Tier::Worker, None), spawn("c", Tier::Worker, None)]);
    }

    #[test]
<<<<<<< HEAD
    fn focused_mode_converges_across_distinct_errors_until_fixed_or_capped() {
        let (mut run, _) = started(&[("app", &[])], 1);
        run.focused = true;
        // Distinct verify errors across build -> runtime -> broken image -> narrow -> pass.
        let stages = [
            "app/page.tsx:1 tsc: TS2307: Cannot find module './horses'",
            "In the browser at /: uncaught error: TypeError: horses.map is not a function",
            "In the browser at /: BROKEN_IMAGE: 1 <img> element(s) failed to load (naturalWidth=0)",
            "In the browser: at 360px wide the page scrolls sideways by 42px",
        ];
        for err in stages {
            assert_eq!(run.step(finished("app", WorkerOutcome::Passed)), vec![Command::RunVerify]);
            let failing = BTreeMap::from([("app".to_string(), err.to_string())]);
            let cmds = run.step(Event::VerifyFinished { ok: false, failing });
            assert_eq!(cmds, vec![spawn("app", Tier::Worker, Some(err))]);
            assert_eq!(run.phase, RunPhase::Building);
        }
        // Final build succeeds and passes verify!
        assert_eq!(run.step(finished("app", WorkerOutcome::Passed)), vec![Command::RunVerify]);
        assert_eq!(run.step(Event::VerifyFinished { ok: true, failing: BTreeMap::new() }), vec![Command::Finish { ok: true }]);
        assert_eq!(run.phase, RunPhase::Done);

        // And distinct errors still hit a hard cap (MAX_FOCUSED_PROGRESS_REPAIRS) so a run cannot loop forever.
        let (mut capped, _) = started(&[("app", &[])], 1);
        capped.focused = true;
        let mut attempts = 0u8;
        for n in 0..20 {
            let err = format!("app/page.tsx:1 tsc: TS{}: error {n}", 2300 + n);
            match capped.step(finished("app", check_failed(&err))).as_slice() {
                [Command::SpawnWorker { .. }] => attempts += 1,
                [Command::Finish { ok: false }] => break,
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(attempts, MAX_FOCUSED_PROGRESS_REPAIRS);
        assert_eq!(capped.phase, RunPhase::Failed);
=======
    fn wave_starts_critical_path_and_largest_units_first_under_parallel_limit() {
        // Mirror the n4 tinydb shape: 4 ready units competing for 2 slots.
        // - `leaf` has no dependents (downstream = 0) and small brief
        // - `tokenizer` has 1 dependent (`cli`, downstream = 1) and small brief
        // - `executor` has 1 dependent (`cli`, downstream = 1) and larger brief + 3 owned files
        // - `btree` has a 2-hop downstream chain (`btree -> storage -> cli`, downstream = 2)
        // Declaration order puts the short units first, so without critical-path ordering
        // `leaf` and `tokenizer` would consume both slots ahead of `btree` and `executor`.
        let planned = Plan {
            pages: vec![
                PagePlan {
                    id: "leaf".into(),
                    owned_glob: "tinydb/leaf.py".into(),
                    brief: "Small helper.".into(),
                    ..Default::default()
                },
                PagePlan {
                    id: "tokenizer".into(),
                    owned_glob: "{tinydb/tokenizer.py,tests/test_tokenizer.py}".into(),
                    brief: "Short lexer.".into(),
                    ..Default::default()
                },
                PagePlan {
                    id: "executor".into(),
                    owned_glob: "{tinydb/planner.py,tinydb/executor.py,tests/test_executor.py}".into(),
                    brief: "Implement full query planner and SQL execution engine with joins, aggregates, sorting, limits, updates, deletes.".into(),
                    ..Default::default()
                },
                PagePlan {
                    id: "btree".into(),
                    owned_glob: "{tinydb/btree.py,tests/test_btree.py}".into(),
                    brief: "Balanced B-tree.".into(),
                    ..Default::default()
                },
                PagePlan {
                    id: "storage".into(),
                    owned_glob: "{tinydb/storage.py,tests/test_storage.py}".into(),
                    brief: "Persistent storage using BTree.".into(),
                    depends_on: vec!["btree".into()],
                    ..Default::default()
                },
                PagePlan {
                    id: "cli".into(),
                    owned_glob: "{tinydb/cli.py,tests/test_cli.py}".into(),
                    brief: "Wire CLI.".into(),
                    depends_on: vec!["tokenizer".into(), "executor".into(), "storage".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let mut run = Run::new(2);
        run.step(Event::PlanReady(planned));
        let cmds = run.step(Event::PlanApproved);
        let spawned: Vec<&str> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::SpawnWorker { page, .. } => Some(page.as_str()),
                _ => None,
            })
            .collect();
        // Wave 1 (2 slots): `btree` (longest downstream chain = 2) and `executor` (downstream = 1, largest size).
        assert_eq!(spawned, vec!["btree", "executor"], "long-pole units must fill wave 1 first");

        // Once `btree` finishes, `storage` becomes unblocked alongside `tokenizer` and `leaf`.
        // `storage` (downstream = 1, larger brief than `tokenizer`) beats `tokenizer` and `leaf`.
        let next = run.step(finished("btree", WorkerOutcome::Passed));
        assert_eq!(next, vec![spawn("storage", Tier::Worker, None)]);

        // Next slot goes to `tokenizer` (downstream = 1) ahead of `leaf` (downstream = 0).
        let next = run.step(finished("executor", WorkerOutcome::Passed));
        assert_eq!(next, vec![spawn("tokenizer", Tier::Worker, None)]);
    }

    #[test]
    fn fix_n9_independent_units_share_wave_without_starvation_from_blocked_units() {
        // 1. With an interface contract, wave 1 speculatively emits blocked `b` and `c` alongside `a`.
        //    When `a` finishes, `c` is STILL blocked on `b`, while `b`, `d`, and `e` are all
        //    unblocked, independent, and own disjoint files — so all 3 must share wave 2 (`parallel=3`).
        let planned = Plan {
            contracts: vec![crate::types::ContractFile {
                path: "pkg/contract.py".into(),
                content: "class Base: ...\n".into(),
                phase: 0,
            }],
            pages: vec![
                PagePlan { id: "a".into(), owned_glob: "pkg/a.py".into(), brief: "a".into(), ..Default::default() },
                PagePlan { id: "b".into(), owned_glob: "pkg/b.py".into(), brief: "b".into(), depends_on: vec!["a".into()], ..Default::default() },
                PagePlan { id: "c".into(), owned_glob: "pkg/c.py".into(), brief: "c".into(), depends_on: vec!["b".into()], ..Default::default() },
                PagePlan { id: "d".into(), owned_glob: "pkg/d.py".into(), brief: "d".into(), depends_on: vec!["a".into()], ..Default::default() },
                PagePlan { id: "e".into(), owned_glob: "pkg/e.py".into(), brief: "e".into(), depends_on: vec!["a".into()], ..Default::default() },
            ],
            ..Default::default()
        };
        let mut run = Run::new(3);
        run.step(Event::PlanReady(planned));
        let wave1 = run.step(Event::PlanApproved);
        assert!(wave1.iter().any(|c| matches!(c, Command::SpawnWorker { page, .. } if page == "a")));
        let wave2 = run.step(finished("a", WorkerOutcome::Passed));
        let spawned: Vec<&str> = wave2
            .iter()
            .filter_map(|c| match c {
                Command::SpawnWorker { page, .. } => Some(page.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            spawned,
            vec!["d", "e"],
            "blocked `c` must not starve independent unblocked `d` and `e` out of wave 2"
        );

        // 2. Units sharing an owned file inside brace globs do NOT share a wave, whereas disjoint
        //    independent units immediately share a wave.
        assert!(crate::owned_globs_overlap(
            "{pkg/a.py,pkg/shared.py}",
            "{pkg/shared.py,pkg/b.py}"
        ));
        assert!(!crate::owned_globs_overlap(
            "{pkg/a.py,tests/test_a.py}",
            "{pkg/b.py,tests/test_b.py}"
        ));
        let overlap_plan = Plan {
            pages: vec![
                PagePlan { id: "u1".into(), owned_glob: "{pkg/a.py,pkg/shared.py}".into(), brief: "u1".into(), ..Default::default() },
                PagePlan { id: "u2".into(), owned_glob: "{pkg/shared.py,pkg/b.py}".into(), brief: "u2".into(), ..Default::default() },
                PagePlan { id: "u3".into(), owned_glob: "{pkg/c.py,tests/test_c.py}".into(), brief: "u3".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let mut run2 = Run::new(3);
        run2.step(Event::PlanReady(overlap_plan));
        let cmds = run2.step(Event::PlanApproved);
        let w1: Vec<&str> = cmds
            .iter()
            .filter_map(|c| match c {
                Command::SpawnWorker { page, .. } => Some(page.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(w1, vec!["u1", "u3"], "disjoint u1 and u3 share wave 1 while overlapping u2 waits");
        assert_eq!(run2.pages["u2"].state, PageState::Queued);
        let after_u1 = run2.step(finished("u1", WorkerOutcome::Passed));
        assert_eq!(after_u1, vec![spawn("u2", Tier::Worker, None)]);
>>>>>>> f8ec87c12ada48009934f58c4b44d40e66c801e3
    }
}
