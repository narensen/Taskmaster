//! Observability: Prometheus `/metrics` exposition, structured span tracing, and
//! monthly project spend aggregation with budget alerts (Item 3.8).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tm_core::{Event, WorkerOutcome, totals};

use crate::store::Records;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonthlySpendReport {
    pub current_month: String,
    pub current_month_usd: f64,
    pub alert_threshold_usd: Option<f64>,
    pub alert_triggered: bool,
    pub by_month: BTreeMap<String, f64>,
    pub total_runs: usize,
    pub total_calls: u64,
}

/// Render Prometheus text format metrics for `GET /metrics`.
pub fn render_prometheus(records: &Records, active_runs: usize) -> String {
    let run_ids = records.run_ids().unwrap_or_default();
    let mut total_calls = 0u64;
    let mut total_tokens = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_cost_usd = 0.0f64;
    let mut total_seconds = 0.0f64;
    let mut total_check_failures = 0u64;
    let mut runs_done = 0u64;
    let mut runs_failed = 0u64;

    for id in &run_ids {
        if let Ok(entries) = records.ledger(id) {
            let sum = totals(&entries);
            total_calls += sum.calls;
            total_tokens += sum.tokens;
            total_cache_read += sum.cache_read;
            total_cost_usd += sum.cost_usd;
            total_seconds += sum.seconds;
        }
        if let Ok(events) = records.events(id) {
            total_check_failures += events
                .iter()
                .filter(|ev| matches!(ev, Event::WorkerFinished { outcome: WorkerOutcome::CheckFailed { .. }, .. }))
                .count() as u64;
        }
        if let Ok(Some(status)) = records.status(id) {
            match status["phase"].as_str() {
                Some("done") => runs_done += 1,
                Some("failed") => runs_failed += 1,
                _ => {}
            }
        }
    }

    format!(
        "# HELP taskmaster_runs_total Total recorded runs in the open project.\n\
         # TYPE taskmaster_runs_total counter\n\
         taskmaster_runs_total {}\n\
         # HELP taskmaster_runs_by_outcome Recorded runs by terminal outcome.\n\
         # TYPE taskmaster_runs_by_outcome counter\n\
         taskmaster_runs_by_outcome{{outcome=\"done\"}} {runs_done}\n\
         taskmaster_runs_by_outcome{{outcome=\"failed\"}} {runs_failed}\n\
         # HELP taskmaster_active_runs Currently running orchestrations.\n\
         # TYPE taskmaster_active_runs gauge\n\
         taskmaster_active_runs {active_runs}\n\
         # HELP taskmaster_agent_calls_total Total agent calls across runs.\n\
         # TYPE taskmaster_agent_calls_total counter\n\
         taskmaster_agent_calls_total {total_calls}\n\
         # HELP taskmaster_tokens_total Total tokens processed (including cache reads).\n\
         # TYPE taskmaster_tokens_total counter\n\
         taskmaster_tokens_total {total_tokens}\n\
         # HELP taskmaster_cache_read_tokens_total Total tokens served from provider prompt cache.\n\
         # TYPE taskmaster_cache_read_tokens_total counter\n\
         taskmaster_cache_read_tokens_total {total_cache_read}\n\
         # HELP taskmaster_cost_usd_total Cumulative recorded USD spend.\n\
         # TYPE taskmaster_cost_usd_total counter\n\
         taskmaster_cost_usd_total {total_cost_usd:.6}\n\
         # HELP taskmaster_agent_seconds_total Cumulative agent wall seconds.\n\
         # TYPE taskmaster_agent_seconds_total counter\n\
         taskmaster_agent_seconds_total {total_seconds:.3}\n\
         # HELP taskmaster_check_failures_total Total check failures across runs.\n\
         # TYPE taskmaster_check_failures_total counter\n\
         taskmaster_check_failures_total {total_check_failures}\n",
        run_ids.len()
    )
}

/// Aggregate USD spend by calendar month (`YYYY-MM`) and flag when current-month spend
/// exceeds `alert_threshold_usd`.
pub fn monthly_spend(records: &Records, alert_threshold_usd: Option<f64>) -> MonthlySpendReport {
    let now = crate::time::rfc3339_now();
    let current_month = now.get(0..7).unwrap_or("unknown").to_string();
    let mut by_month: BTreeMap<String, f64> = BTreeMap::new();
    let run_ids = records.run_ids().unwrap_or_default();
    let mut total_calls = 0u64;

    for id in &run_ids {
        if let Ok(entries) = records.ledger(id) {
            for e in entries {
                total_calls += 1;
                if let Some(usd) = e.cost_usd {
                    let month = e.at.get(0..7).filter(|m| m.len() == 7).unwrap_or(&current_month).to_string();
                    *by_month.entry(month).or_insert(0.0) += usd;
                }
            }
        }
    }
    let current_month_usd = *by_month.get(&current_month).unwrap_or(&0.0);
    let alert_triggered = alert_threshold_usd.is_some_and(|t| t > 0.0 && current_month_usd >= t);
    MonthlySpendReport {
        current_month,
        current_month_usd,
        alert_threshold_usd,
        alert_triggered,
        by_month,
        total_runs: run_ids.len(),
        total_calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{LedgerEntry, Role};

    #[test]
    fn prometheus_and_monthly_spend_reflect_recorded_ledger_and_alert() {
        let dir = std::env::temp_dir().join(format!("tm-metrics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let records = Records::files(&dir);
        let now = crate::time::rfc3339_now();
        records
            .charge(
                "run-1",
                &LedgerEntry {
                    at: now,
                    role: Role::Worker,
                    page: Some("a".into()),
                    harness: "grok".into(),
                    model: Some("grok-4".into()),
                    input: 1000,
                    output: 200,
                    cache_read: 800,
                    cache_write: 0,
                    cost_usd: Some(12.50),
                    seconds: 4.2,
                    ok: true,
                    refused: vec![],
                    agent: Some("page:a".into()),
                    unit: None,
                    priced_by: Some("provider".into()),
                    brief_hash: None,
                    difficulty: None,
                    language: None,
                },
            )
            .unwrap();

        let prom = render_prometheus(&records, 1);
        assert!(prom.contains("taskmaster_active_runs 1"));
        assert!(prom.contains("taskmaster_agent_calls_total 1"));
        assert!(prom.contains("taskmaster_cost_usd_total 12.500000"));
        assert!(prom.contains("taskmaster_cache_read_tokens_total 800"));

        let spend = monthly_spend(&records, Some(10.0));
        assert!(spend.alert_triggered);
        assert!((spend.current_month_usd - 12.50).abs() < 1e-6);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
