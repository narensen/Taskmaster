//! The run state machine.
//!
//! `Run::step` is pure: it takes an event, updates the state and returns the commands the
//! executor must carry out. Replaying a run's event log through `step` rebuilds its state,
//! which is how a run resumes after a crash.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{DictKey, Flag, FlagResolution, PageId, Plan};

/// Repair rounds a page gets on one tier before it escalates.
pub const MAX_REPAIRS: u8 = 2;
/// Dictionary amendments allowed per run. This cap is what bounds fix-and-revert loops.
pub const MAX_AMENDMENTS: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Planning,
    AwaitingApproval,
    Building,
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
    /// Set when an entry this page cites was amended mid-build; its result is discarded.
    pub stale: bool,
    /// Check output handed to the next attempt.
    pub pending_errors: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutcome {
    /// The page's own checks passed.
    Passed,
    /// Typecheck or ownership check failed; `errors` goes to the repair attempt.
    CheckFailed { errors: String },
    Flagged(Flag),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    PlanReady(Plan),
    PlanApproved,
    PlanRejected,
    WorkerFinished { page: PageId, outcome: WorkerOutcome },
    FlagResolved { page: PageId, key: DictKey, resolution: FlagResolution },
    /// Whole-project verification. `failing` maps a page to the errors attributed to it.
    VerifyFinished { failing: BTreeMap<PageId, String>, ok: bool },
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
    RunVerify,
    Finish { ok: bool },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub phase: RunPhase,
    pub plan: Option<Plan>,
    pub pages: BTreeMap<PageId, PageRun>,
    pub amendments: u8,
    pub max_parallel: usize,
}

impl Run {
    pub fn new(max_parallel: usize) -> Self {
        Run {
            phase: RunPhase::Planning,
            plan: None,
            pages: BTreeMap::new(),
            amendments: 0,
            max_parallel: max_parallel.max(1),
        }
    }

    /// Rebuild a run from its event log.
    pub fn replay(max_parallel: usize, events: impl IntoIterator<Item = Event>) -> Self {
        let mut run = Run::new(max_parallel);
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
            (RunPhase::Planning, Event::PlanReady(plan)) => {
                self.pages = plan
                    .pages
                    .iter()
                    .map(|p| {
                        let page = PageRun {
                            state: PageState::Queued,
                            tier: Tier::Worker,
                            repairs: 0,
                            stale: false,
                            pending_errors: None,
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
                        WorkerOutcome::CheckFailed { errors } => Self::fail_page(p, errors),
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
            (RunPhase::Verifying, Event::VerifyFinished { ok: true, .. }) => {
                self.phase = RunPhase::Done;
                out.push(Command::Finish { ok: true });
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
                            Self::fail_page(p, errors);
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

    /// Repair on the same tier, then escalate, then give up.
    fn fail_page(p: &mut PageRun, errors: String) {
        if p.repairs < MAX_REPAIRS {
            p.repairs += 1;
        } else if let Some(next) = p.tier.next() {
            p.tier = next;
            p.repairs = 0;
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

    /// Start queued pages up to the parallel limit, then advance the phase if nothing is left.
    fn settle(&mut self, out: &mut Vec<Command>) {
        let mut running = self.count(PageState::Building);
        let order: Vec<PageId> = match &self.plan {
            Some(plan) => plan.pages.iter().map(|p| p.id.clone()).collect(),
            None => return,
        };
        for id in order {
            if running >= self.max_parallel {
                break;
            }
            let p = self.pages.get_mut(&id).expect("every planned page has a PageRun");
            if p.state == PageState::Queued {
                p.state = PageState::Building;
                running += 1;
                out.push(Command::SpawnWorker {
                    page: id,
                    tier: p.tier,
                    errors: p.pending_errors.take(),
                });
            }
        }
        let open = running + self.count(PageState::Flagged);
        if open == 0 {
            if self.count(PageState::Failed) == 0 {
                self.phase = RunPhase::Verifying;
                out.push(Command::RunVerify);
            } else {
                self.phase = RunPhase::Failed;
                out.push(Command::Finish { ok: false });
            }
        }
    }

    fn count(&self, state: PageState) -> usize {
        self.pages.values().filter(|p| p.state == state).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Estimate, PagePlan};

    fn plan(pages: &[(&str, &[&str])]) -> Plan {
        Plan {
            contracts: vec![],
            pages: pages
                .iter()
                .map(|(id, cites)| PagePlan {
                    id: id.to_string(),
                    route: format!("/{id}"),
                    owned_glob: format!("app/{id}/**"),
                    cites: cites.iter().map(|c| c.to_string()).collect(),
                    brief: String::new(),
                })
                .collect(),
            estimate: Estimate { tokens: 0, usd: 0.0 },
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
        assert_eq!(run.step(finished("b", WorkerOutcome::Passed)), vec![Command::RunVerify]);
        let done = run.step(Event::VerifyFinished { ok: true, failing: BTreeMap::new() });
        assert_eq!(done, vec![Command::Finish { ok: true }]);
        assert_eq!(run.phase, RunPhase::Done);
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
        loop {
            let cmds = run.step(finished("a", check_failed("TS2322")));
            match cmds.as_slice() {
                [Command::SpawnWorker { tier, errors, .. }] => {
                    assert_eq!(errors.as_deref(), Some("TS2322"));
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
}
