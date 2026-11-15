//! Which commands to run after a restart.
//!
//! Replaying the event log rebuilds the phase and the page states, but `Run::step` only
//! returns the commands for the event just applied. A command that was in flight at the
//! crash is not in that list. The inflight file is the copy written before a command
//! started. When it is missing, the same commands are derived from the phase: building
//! pages are spawned again, and a flagged page's objection is read back out of the log.
//! Review scopes are not stored on the run (they are cleared when review begins), so a
//! review with no inflight file is recomputed from the whole plan.

use std::collections::BTreeMap;

use anyhow::bail;
use tm_core::{ChangeEntry, ChangeKind, Command, Event, Finding, Flag, PageState, Run, RunPhase, WorkerOutcome};

/// Commands still to execute, in the order the daemon should run them.
pub fn reconcile(
    run: &Run,
    events: &[Event],
    changes: &[ChangeEntry],
    inflight: Vec<Command>,
) -> anyhow::Result<Vec<Command>> {
    let mut kept: Vec<Command> = inflight.into_iter().filter(|cmd| still_needed(run, cmd)).collect();
    for cmd in implied(run, events, changes)? {
        if !covers(&kept, &cmd) {
            kept.push(cmd);
        }
    }
    for cmd in &mut kept {
        if let Command::VerifyFindings { findings } = cmd {
            if !run.pending_findings.is_empty() {
                *findings = run.pending_findings.clone();
            }
        }
    }
    kept.sort_by_key(|cmd| rank(run, cmd));
    Ok(kept)
}

fn still_needed(run: &Run, cmd: &Command) -> bool {
    match cmd {
        Command::RequestApproval => run.phase == RunPhase::AwaitingApproval,
        // Rewriting the same contracts is a no-op commit, so a stale lock command is safe.
        Command::LockContracts => run.phase == RunPhase::Building,
        Command::SpawnWorker { page, .. } => state(run, page) == Some(PageState::Building),
        Command::ArbitrateFlag(flag) => state(run, &flag.page) == Some(PageState::Flagged),
        Command::RunReview { .. } => run.phase == RunPhase::Reviewing && run.pending_findings.is_empty(),
        Command::VerifyFindings { .. } => run.phase == RunPhase::Reviewing && !run.pending_findings.is_empty(),
        Command::RunVerify => run.phase == RunPhase::Verifying,
        Command::Finish { .. } => matches!(run.phase, RunPhase::Done | RunPhase::Failed),
    }
}

fn implied(run: &Run, events: &[Event], changes: &[ChangeEntry]) -> anyhow::Result<Vec<Command>> {
    let mut out = Vec::new();
    match run.phase {
        RunPhase::Planning => {}
        RunPhase::AwaitingApproval => out.push(Command::RequestApproval),
        RunPhase::Building => {
            if !changes.iter().any(|c| c.kind == ChangeKind::ContractsLocked) {
                out.push(Command::LockContracts);
            }
            let Some(plan) = &run.plan else { return Ok(out) };
            for page in &plan.pages {
                match state(run, &page.id) {
                    Some(PageState::Building) => out.push(Command::SpawnWorker {
                        page: page.id.clone(),
                        tier: run.pages[&page.id].tier,
                        errors: recovered_errors(events, &page.id),
                    }),
                    Some(PageState::Flagged) => match last_flag(events, &page.id) {
                        Some(flag) => out.push(Command::ArbitrateFlag(flag)),
                        None => bail!("page `{}` is flagged but the event log has no objection", page.id),
                    },
                    _ => {}
                }
            }
        }
        RunPhase::Reviewing if run.pending_findings.is_empty() => {
            out.push(Command::RunReview { scopes: all_scopes(run), security: all_pages(run) });
        }
        RunPhase::Reviewing => {
            out.push(Command::VerifyFindings { findings: run.pending_findings.clone() });
        }
        RunPhase::Verifying => out.push(Command::RunVerify),
        RunPhase::Done => out.push(Command::Finish { ok: true }),
        RunPhase::Failed => out.push(Command::Finish { ok: false }),
    }
    Ok(out)
}

fn state(run: &Run, page: &str) -> Option<PageState> {
    run.pages.get(page).map(|p| p.state)
}

fn covers(cmds: &[Command], cmd: &Command) -> bool {
    cmds.iter().any(|have| match (have, cmd) {
        (Command::SpawnWorker { page: a, .. }, Command::SpawnWorker { page: b, .. }) => a == b,
        (Command::ArbitrateFlag(a), Command::ArbitrateFlag(b)) => a.page == b.page,
        (Command::RequestApproval, Command::RequestApproval) => true,
        (Command::LockContracts, Command::LockContracts) => true,
        (Command::RunReview { .. }, Command::RunReview { .. }) => true,
        (Command::VerifyFindings { .. }, Command::VerifyFindings { .. }) => true,
        (Command::RunVerify, Command::RunVerify) => true,
        (Command::Finish { .. }, Command::Finish { .. }) => true,
        _ => false,
    })
}

fn rank(run: &Run, cmd: &Command) -> (u8, usize) {
    match cmd {
        Command::RequestApproval => (0, 0),
        Command::LockContracts => (1, 0),
        Command::SpawnWorker { page, .. } => (2, index(run, page)),
        Command::ArbitrateFlag(flag) => (3, index(run, &flag.page)),
        Command::RunReview { .. } => (4, 0),
        Command::VerifyFindings { .. } => (5, 0),
        Command::RunVerify => (6, 0),
        Command::Finish { .. } => (7, 0),
    }
}

fn index(run: &Run, page: &str) -> usize {
    run.plan.as_ref().and_then(|p| p.pages.iter().position(|x| x.id == page)).unwrap_or(0)
}

fn all_pages(run: &Run) -> Vec<String> {
    run.plan.as_ref().map(|p| p.pages.iter().map(|page| page.id.clone()).collect()).unwrap_or_default()
}

/// Every Dictionary key in the plan. Used only when the original review scopes were lost.
fn all_scopes(run: &Run) -> Vec<tm_core::ReviewScope> {
    let Some(plan) = &run.plan else { return Vec::new() };
    let mut by_key: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for page in &plan.pages {
        for key in &page.cites {
            by_key.entry(key).or_default().push(page.id.clone());
        }
    }
    by_key.into_iter().map(|(key, consumers)| tm_core::ReviewScope { key: key.to_string(), consumers }).collect()
}

fn last_flag(events: &[Event], page: &str) -> Option<Flag> {
    events.iter().rev().find_map(|event| match event {
        Event::WorkerFinished { page: id, outcome: WorkerOutcome::Flagged(flag) } if id == page => Some(flag.clone()),
        _ => None,
    })
}

/// The repair text a building page was spawned with. `step` consumes it into the command,
/// so after a replay it is no longer on the page; the event that caused it still has it.
pub fn recovered_errors(events: &[Event], page: &str) -> Option<String> {
    let mut errors = None;
    for event in events {
        match event {
            Event::WorkerFinished { page: id, outcome: WorkerOutcome::CheckFailed { errors: text } } if id == page => {
                errors = Some(text.clone());
            }
            Event::WorkerFinished { page: id, outcome: WorkerOutcome::Passed | WorkerOutcome::Flagged(_) }
                if id == page =>
            {
                errors = None;
            }
            Event::FindingsVerified { confirmed } => {
                let lines = finding_lines(confirmed, page);
                if !lines.is_empty() {
                    errors = Some(lines);
                }
            }
            Event::VerifyFinished { failing, .. } => {
                if let Some(text) = failing.get(page) {
                    errors = Some(text.clone());
                }
            }
            Event::FlagResolved { page: id, .. } if id == page => errors = None,
            _ => {}
        }
    }
    errors
}

fn finding_lines(findings: &[Finding], page: &str) -> String {
    findings
        .iter()
        .filter(|f| f.page == page)
        .map(|f| format!("review: {}:{} {}", f.file, f.line, f.failure))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{Estimate, PagePlan, Plan, Tier};

    fn plan(pages: &[(&str, &[&str])]) -> Plan {
        Plan {
            contracts: vec![],
            pages: pages
                .iter()
                .map(|(id, cites)| PagePlan {
                    id: (*id).into(),
                    route: format!("/{id}"),
                    owned_glob: format!("app/{id}/**"),
                    cites: cites.iter().map(|c| (*c).to_string()).collect(),
                    brief: String::new(),
                })
                .collect(),
            estimate: Estimate { tokens: 0, usd: 0.0 },
        }
    }

    fn approved(pages: &[(&str, &[&str])], parallel: usize) -> (Run, Vec<Event>) {
        let events = vec![Event::PlanReady(plan(pages)), Event::PlanApproved];
        (Run::replay(parallel, events.clone()), events)
    }

    #[test]
    fn a_restart_after_approval_locks_and_respawns_building_pages() {
        let (run, events) = approved(&[("a", &[]), ("b", &[])], 4);
        let cmds = reconcile(&run, &events, &[], vec![]).unwrap();
        assert!(matches!(cmds[0], Command::LockContracts));
        assert!(matches!(&cmds[1], Command::SpawnWorker { page, .. } if page == "a"));
        assert!(matches!(&cmds[2], Command::SpawnWorker { page, .. } if page == "b"));
    }

    #[test]
    fn an_inflight_spawn_keeps_its_errors_and_a_finished_page_is_dropped() {
        let (mut run, mut events) = approved(&[("a", &[]), ("b", &[])], 1);
        let failed = Event::WorkerFinished {
            page: "a".into(),
            outcome: WorkerOutcome::CheckFailed { errors: "TS2322".into() },
        };
        events.push(failed.clone());
        run.step(failed);
        let inflight = vec![Command::SpawnWorker {
            page: "a".into(),
            tier: Tier::Worker,
            errors: Some("kept".into()),
        }];
        let cmds = reconcile(&run, &events, &[locked()], inflight).unwrap();
        assert_eq!(
            cmds,
            vec![Command::SpawnWorker { page: "a".into(), tier: Tier::Worker, errors: Some("kept".into()) }]
        );
    }

    #[test]
    fn lost_inflight_recovers_the_repair_text_from_the_event() {
        let (mut run, mut events) = approved(&[("a", &[])], 1);
        let failed = Event::WorkerFinished {
            page: "a".into(),
            outcome: WorkerOutcome::CheckFailed { errors: "TS2322".into() },
        };
        events.push(failed.clone());
        run.step(failed);
        let cmds = reconcile(&run, &events, &[locked()], vec![]).unwrap();
        assert_eq!(
            cmds,
            vec![Command::SpawnWorker { page: "a".into(), tier: Tier::Worker, errors: Some("TS2322".into()) }]
        );
    }

    #[test]
    fn a_flag_is_read_back_from_the_log() {
        let (mut run, mut events) = approved(&[("a", &["auth"])], 1);
        let flag = Flag { page: "a".into(), key: "auth".into(), issue: "missing role".into() };
        let event = Event::WorkerFinished { page: "a".into(), outcome: WorkerOutcome::Flagged(flag.clone()) };
        events.push(event.clone());
        run.step(event);
        let cmds = reconcile(&run, &events, &[locked()], vec![]).unwrap();
        assert_eq!(cmds, vec![Command::ArbitrateFlag(flag)]);
    }

    fn locked() -> ChangeEntry {
        ChangeEntry {
            at: String::new(),
            kind: ChangeKind::ContractsLocked,
            page: None,
            commit: None,
            model: None,
            files: vec![],
            keys: vec![],
            summary: String::new(),
        }
    }
}
