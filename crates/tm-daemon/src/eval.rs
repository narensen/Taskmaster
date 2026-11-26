//! Run a plan twice: once with the Taskmaster ladder, once with a single model.

use std::sync::Arc;

use serde::Serialize;
use tm_core::{Event, Run, WorkerOutcome, totals};
use tm_harness::Harness;

use crate::store::Records;
use crate::{Config, Harnesses, Outcome, drive};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalSide {
    pub harness: String,
    pub ok: bool,
    pub passed: usize,
    pub failed: usize,
    /// Check failures recorded for that side. Each one is a repair attempt that did not pass.
    pub check_failures: usize,
    pub agent_calls: u64,
    /// Every token, cache reads included.
    pub tokens: u64,
    /// `tokens` without the cache reads.
    pub fresh_tokens: u64,
    /// The prices that were reported. `unpriced_calls` says how many calls it leaves out.
    pub cost_usd: f64,
    pub unpriced_calls: u64,
    pub seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalReport {
    pub taskmaster: EvalSide,
    pub baseline: Option<EvalSide>,
}

/// `baseline` builds every page. The reviewer and the verifier stay as they are.
/// The baseline run does not reuse the Taskmaster citation graph.
pub async fn evaluate(
    mut config: Config,
    harnesses: Harnesses,
    baseline: Arc<dyn Harness>,
    baseline_name: &str,
) -> anyhow::Result<(Outcome, EvalReport)> {
    let records = match &config.store {
        Some(store) => Records::new(store.clone()),
        None => Records::files(&config.project.canonicalize().unwrap_or_else(|_| config.project.clone())),
    };
    let project = config.project.canonicalize().unwrap_or_else(|_| config.project.clone());
    let id = config.run_id.clone();
    let taskmaster_name = harnesses.worker.id().to_string();
    config.incremental = true;
    // Benchmark evaluations stay offline so results stay deterministic and comparable.
    config.offline = true;
    let outcome = drive(config.clone(), harnesses.clone()).await?;
    let taskmaster = side(&records, &id, &taskmaster_name, &outcome)?;
    if !matches!(outcome, Outcome::Finished { .. }) {
        let report = EvalReport { taskmaster, baseline: None };
        records.save_eval(&id, &report)?;
        return Ok((outcome, report));
    }
    // A prompt is planned once. The baseline builds that same plan.
    if config.plan.is_none() {
        config.plan = Some(recorded_plan(&records, &id)?);
        config.prompt = None;
    }
    // The baseline starts from the commit the Taskmaster run started from, and what it
    // builds stays on its own branch: the project keeps the Taskmaster result.
    config.base = records.meta(&id)?.and_then(|meta| meta.base);
    config.merge = false;
    config.run_id = format!("{id}-baseline");
    config.incremental = false;
    let baseline_harnesses = Harnesses {
        worker: baseline.clone(),
        mid: baseline.clone(),
        master: baseline,
        reviewer: harnesses.reviewer,
        verifier: harnesses.verifier,
    };
    let baseline_outcome = drive(config, baseline_harnesses).await?;
    let baseline_side = side(&records, &format!("{id}-baseline"), baseline_name, &baseline_outcome)?;
    // The baseline was only ever measured. Its branch would otherwise pile up, one per eval.
    if let Some(branch) = records.meta(&format!("{id}-baseline"))?.and_then(|meta| meta.branch) {
        let _ = crate::git::git(&project, &["branch", "-D", &branch]);
    }
    let report = EvalReport { taskmaster, baseline: Some(baseline_side) };
    records.save_eval(&id, &report)?;
    Ok((outcome, report))
}

fn recorded_plan(records: &Records, run_id: &str) -> anyhow::Result<tm_core::Plan> {
    for event in records.events(run_id)? {
        if let Event::PlanReady(plan) = event {
            return Ok(plan);
        }
    }
    anyhow::bail!("run {run_id} finished without a plan")
}

fn side(records: &Records, run_id: &str, harness: &str, outcome: &Outcome) -> anyhow::Result<EvalSide> {
    let events = records.events(run_id)?;
    let check_failures = events
        .iter()
        .filter(|event| matches!(event, Event::WorkerFinished { outcome: WorkerOutcome::CheckFailed { .. }, .. }))
        .count();
    let focused = records.meta(run_id)?.is_some_and(|meta| meta.focused);
    let run = Run::replay_with(1, focused, events);
    let passed = run.pages.values().filter(|page| page.state == tm_core::PageState::Passed).count();
    let failed = run.pages.values().filter(|page| page.state == tm_core::PageState::Failed).count();
    let ok = matches!(outcome, Outcome::Finished { ok: true });
    let spent = totals(&records.ledger(run_id)?);
    Ok(EvalSide {
        harness: harness.to_string(),
        ok,
        passed,
        failed,
        check_failures,
        agent_calls: spent.calls,
        tokens: spent.tokens,
        fresh_tokens: spent.fresh,
        cost_usd: spent.cost_usd,
        unpriced_calls: spent.unpriced_calls,
        seconds: spent.seconds,
    })
}
