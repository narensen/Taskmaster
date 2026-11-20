//! `tm-daemon report`: where a run's time, tokens and money went, from its own records.
//!
//! Everything comes from the run's ledger (every agent call, with its end time and length),
//! its transcript (tool calls, and verify's own timing lines) and its status. Stages are
//! worked out from when calls ran, so overlapping calls are not counted twice, and the
//! time no call and no check covers is shown as idle.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use tm_core::{LedgerEntry, Role, totals};

use crate::store::Records;

/// One row of the stage table.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Stage {
    pub name: String,
    /// Wall-clock seconds the stage's calls covered.
    pub seconds: f64,
    pub calls: usize,
    pub fresh: u64,
    pub cache_read: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub run: String,
    pub outcome: String,
    pub stages: Vec<Stage>,
    pub planning_share: f64,
    pub total_seconds: f64,
    pub idle_seconds: f64,
    pub verify_seconds: f64,
    pub fresh: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Cache reads over all input: uncached, written to the cache, and read from it.
    pub cache_hit: f64,
    pub cost: String,
    pub tool_calls: usize,
    pub run_code_calls: usize,
    pub refused: usize,
}

fn seconds_of(at: &str) -> Option<f64> {
    crate::time::parse_rfc3339(at).map(|t| t as f64)
}

/// The length of the union of `spans` (start, end), in seconds.
fn covered(mut spans: Vec<(f64, f64)>) -> f64 {
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for (start, end) in spans {
        match &mut current {
            Some((_, last)) if start <= *last => *last = last.max(end),
            _ => {
                if let Some((s, e)) = current {
                    total += e - s;
                }
                current = Some((start, end));
            }
        }
    }
    if let Some((s, e)) = current {
        total += e - s;
    }
    total
}

fn span(entry: &LedgerEntry) -> Option<(f64, f64)> {
    let end = seconds_of(&entry.at)?;
    Some((end - entry.seconds, end))
}

fn stage(name: String, rows: &[&LedgerEntry]) -> Stage {
    Stage {
        name,
        seconds: covered(rows.iter().filter_map(|e| span(e)).collect()),
        calls: rows.len(),
        fresh: rows.iter().map(|e| e.input + e.output).sum(),
        cache_read: rows.iter().map(|e| e.cache_read).sum(),
    }
}

/// The seconds a `verify: a 1.2s ok, b 3.4s failed` line adds up to.
fn verify_line_seconds(text: &str) -> f64 {
    let Some(rest) = text.trim().strip_prefix("verify:") else { return 0.0 };
    rest.split(',')
        .filter_map(|part| part.split_whitespace().find_map(|w| w.strip_suffix('s').and_then(|n| n.parse::<f64>().ok())))
        .sum()
}

pub fn build(records: &Records, run: &str) -> anyhow::Result<Report> {
    let ledger = records.ledger(run)?;
    let transcript = records.transcript(run)?;
    let status = records.status(run)?.unwrap_or(Value::Null);
    let outcome = match status["ok"].as_bool() {
        Some(true) => "finished".to_string(),
        Some(false) => "failed".to_string(),
        None => status["phase"].as_str().unwrap_or("not finished").to_string(),
    };
    let planning: Vec<&LedgerEntry> = ledger.iter().filter(|e| e.role == Role::Planner).collect();
    let mut stages = vec![stage("planning".into(), &planning)];
    // Each unit's builders, in the order they first ran.
    let mut units: BTreeMap<String, (f64, Vec<&LedgerEntry>)> = BTreeMap::new();
    for entry in ledger.iter().filter(|e| e.page.is_some() && matches!(e.role, Role::Worker | Role::Mid | Role::Master)) {
        let first = span(entry).map_or(f64::MAX, |s| s.0);
        let slot = units.entry(entry.agent()).or_insert((first, Vec::new()));
        slot.0 = slot.0.min(first);
        slot.1.push(entry);
    }
    let mut ordered: Vec<(String, (f64, Vec<&LedgerEntry>))> = units.into_iter().collect();
    ordered.sort_by(|a, b| a.1.0.total_cmp(&b.1.0));
    for (agent, (_, rows)) in &ordered {
        let mut row = stage(format!("build {agent}"), rows);
        if rows.len() > 1 {
            row.name.push_str(&format!(" ({} repairs)", rows.len() - 1));
        }
        stages.push(row);
    }
    let builds: Vec<&LedgerEntry> = ordered.iter().flat_map(|(_, (_, rows))| rows.iter().copied()).collect();
    if ordered.len() > 1 {
        stages.push(stage("build, all units (wall clock)".into(), &builds));
    }
    let master: Vec<&LedgerEntry> = ledger.iter().filter(|e| e.page.is_none() && matches!(e.role, Role::Master | Role::Arbiter)).collect();
    if !master.is_empty() {
        stages.push(stage("master repairs".into(), &master));
    }
    let others: Vec<&LedgerEntry> = ledger.iter().filter(|e| matches!(e.role, Role::Reviewer | Role::Security | Role::Verifier | Role::Checker)).collect();
    if !others.is_empty() {
        stages.push(stage("review and checks by agents".into(), &others));
    }
    let runtime = |e: &&Value| e["agent"]["id"] == "runtime";
    let verify_seconds: f64 = transcript.iter().filter(runtime).filter(|e| e["kind"] == "output").filter_map(|e| e["text"].as_str()).map(verify_line_seconds).sum();
    stages.push(Stage { name: "verify (the project's own checks)".into(), seconds: verify_seconds, calls: 0, fresh: 0, cache_read: 0 });
    // The run's wall clock: from the first record to the last.
    let mut times: Vec<f64> = ledger.iter().filter_map(span).flat_map(|(s, e)| [s, e]).collect();
    times.extend(transcript.iter().filter_map(|e| e["at"].as_str()).filter_map(seconds_of));
    let (first, last) = (times.iter().copied().fold(f64::MAX, f64::min), times.iter().copied().fold(f64::MIN, f64::max));
    let total_seconds = if times.is_empty() { 0.0 } else { (last - first).max(0.0) };
    let busy = covered(ledger.iter().filter_map(span).collect()) + verify_seconds;
    let idle_seconds = (total_seconds - busy).max(0.0);
    let planning_seconds = stages[0].seconds;
    let sum = totals(&ledger);
    // Every input token is one of three: uncached, written to the cache, or read from it.
    let all_input = sum.input + sum.cache_write + sum.cache_read;
    let cache_hit = if all_input == 0 { 0.0 } else { sum.cache_read as f64 / all_input as f64 };
    let calls: Vec<&Value> = transcript.iter().filter(|e| e["kind"] == "tool_call").collect();
    Ok(Report {
        run: run.to_string(),
        outcome,
        stages,
        planning_share: if total_seconds > 0.0 { planning_seconds / total_seconds } else { 0.0 },
        total_seconds,
        idle_seconds,
        verify_seconds,
        fresh: sum.fresh,
        input: sum.input,
        output: sum.output,
        cache_read: sum.cache_read,
        cache_hit,
        cost: crate::engine::cost_words(&ledger),
        tool_calls: calls.len(),
        run_code_calls: calls.iter().filter(|e| e["name"].as_str().is_some_and(|n| n.contains("run_code"))).count(),
        refused: ledger.iter().map(|e| e.refused.len()).sum(),
    })
}

/// The report as text: the stage table, then the totals.
pub fn render(report: &Report) -> String {
    let mut out = format!("Run {} ({})\n\n", report.run, report.outcome);
    out.push_str("| Stage | Time | Calls | Fresh tokens | Cache reads |\n|---|---|---|---|---|\n");
    for stage in &report.stages {
        out.push_str(&format!("| {} | {:.0}s | {} | {} | {} |\n", stage.name, stage.seconds, stage.calls, stage.fresh, stage.cache_read));
    }
    out.push_str(&format!("| idle | {:.0}s | | | |\n", report.idle_seconds));
    out.push_str(&format!("| total | {:.0}s | | {} | {} |\n\n", report.total_seconds, report.fresh, report.cache_read));
    out.push_str(&format!("Planning share: {:.0}%\n", report.planning_share * 100.0));
    out.push_str(&format!(
        "Tokens: {} fresh ({} in, {} out), {} read from cache; cache-hit ratio {:.2}\n",
        report.fresh, report.input, report.output, report.cache_read, report.cache_hit
    ));
    out.push_str(&format!("Cost: {}\n", report.cost));
    out.push_str(&format!("Tool calls: {} (run_code {}), refused {}\n", report.tool_calls, report.run_code_calls, report.refused));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_calls_are_counted_once() {
        assert_eq!(covered(vec![(0.0, 10.0), (5.0, 15.0), (20.0, 25.0)]), 20.0);
        assert_eq!(covered(vec![]), 0.0);
        assert!((verify_line_seconds("verify: setup 1.7s ok, setup 0.4s ok, test 0.2s ok") - 2.3).abs() < 1e-9);
        assert_eq!(verify_line_seconds("taskmaster: nothing"), 0.0);
    }
}
