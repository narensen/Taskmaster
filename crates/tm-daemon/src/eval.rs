//! Run a plan twice: once with the Taskmaster ladder, once with a single model.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use serde::Serialize;
use tm_core::{Event, Run, WorkerOutcome};
use tm_harness::Harness;

use crate::records;
use crate::{Config, Harnesses, Outcome, drive, run_directory};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EvalSide {
    pub harness: String,
    pub ok: bool,
    pub passed: usize,
    pub failed: usize,
    /// Check failures recorded for that side. Each one is a repair attempt that did not pass.
    pub check_failures: usize,
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
    let project = config.project.clone();
    let id = config.run_id.clone();
    let taskmaster_name = harnesses.worker.id().to_string();
    config.incremental = true;
    let outcome = drive(config.clone(), harnesses.clone()).await?;
    let taskmaster = side(&project, &id, &taskmaster_name, &outcome)?;
    if !matches!(outcome, Outcome::Finished { .. }) {
        let report = EvalReport { taskmaster, baseline: None };
        write_report(&project, &id, &report)?;
        return Ok((outcome, report));
    }
    // A prompt is planned once. The baseline builds that same plan.
    if config.plan.is_none() {
        config.plan = Some(recorded_plan(&project, &id)?);
        config.prompt = None;
    }
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
    let baseline_side = side(&project, &format!("{id}-baseline"), baseline_name, &baseline_outcome)?;
    let report = EvalReport { taskmaster, baseline: Some(baseline_side) };
    write_report(&project, &id, &report)?;
    Ok((outcome, report))
}

fn recorded_plan(project: &Path, run_id: &str) -> anyhow::Result<tm_core::Plan> {
    let path = run_directory(project, run_id).join("events.jsonl");
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), index + 1))?;
        if let Event::PlanReady(plan) = event {
            return Ok(plan);
        }
    }
    anyhow::bail!("run {run_id} finished without a plan")
}

fn side(project: &Path, run_id: &str, harness: &str, outcome: &Outcome) -> anyhow::Result<EvalSide> {
    let path = run_directory(project, run_id).join("events.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        events.push(
            serde_json::from_str::<Event>(line).with_context(|| format!("{}:{}", path.display(), index + 1))?,
        );
    }
    let check_failures = events
        .iter()
        .filter(|event| matches!(event, Event::WorkerFinished { outcome: WorkerOutcome::CheckFailed { .. }, .. }))
        .count();
    let run = Run::replay(1, events);
    let passed = run.pages.values().filter(|page| page.state == tm_core::PageState::Passed).count();
    let failed = run.pages.values().filter(|page| page.state == tm_core::PageState::Failed).count();
    let ok = matches!(outcome, Outcome::Finished { ok: true });
    Ok(EvalSide { harness: harness.to_string(), ok, passed, failed, check_failures })
}

fn write_report(project: &Path, id: &str, report: &EvalReport) -> anyhow::Result<()> {
    let dir = project.join(".taskmaster/evals");
    std::fs::create_dir_all(&dir)?;
    let name = id.replace('/', "_");
    records::save_pretty(&dir.join(format!("{name}.json")), report)
}
