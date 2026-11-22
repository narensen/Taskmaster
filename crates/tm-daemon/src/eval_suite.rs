//! Multi-task benchmark eval suite runner and nightly regression gate (Items 5.1 & 5.2).
//!
//! Tasks live under `bench/suite/<id>/task.json` with hidden tests the agents never see.
//! [`check_regression_gate`] enforces that no pipeline or prompt change lowers the suite
//! pass rate or increases cost by more than 10%.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalTask {
    pub id: String,
    /// `web`, `library`, `cli`, or `python`.
    pub category: String,
    pub language: String,
    pub difficulty: String,
    pub prompt: String,
    /// Relative path -> initial file content seeded before the agent runs.
    #[serde(default)]
    pub starter_files: BTreeMap<String, String>,
    /// Hidden verification script written ONLY after the agent finishes, then executed.
    pub hidden_test_file: String,
    pub hidden_test_content: String,
    pub hidden_test_command: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuiteResultRow {
    pub at: String,
    pub task_id: String,
    pub category: String,
    pub language: String,
    pub difficulty: String,
    pub passed: bool,
    pub cost_usd: f64,
    pub seconds: f64,
    pub repairs: usize,
    pub cache_hit_rate: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateVerdict {
    pub ok: bool,
    pub baseline_pass_rate: f64,
    pub candidate_pass_rate: f64,
    pub baseline_cost_usd: f64,
    pub candidate_cost_usd: f64,
    pub cost_delta_pct: f64,
    pub reasons: Vec<String>,
}

/// Load all `task.json` files from `suite_dir/*/task.json` in sorted order.
pub fn load_suite(suite_dir: &Path) -> anyhow::Result<Vec<EvalTask>> {
    let mut tasks = Vec::new();
    let entries = std::fs::read_dir(suite_dir)?;
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    for dir in dirs {
        let manifest = dir.join("task.json");
        if manifest.is_file() {
            let raw = std::fs::read(&manifest)?;
            let task: EvalTask = serde_json::from_slice(&raw)?;
            tasks.push(task);
        }
    }
    Ok(tasks)
}

/// Materialize a task's starter files into `workspace` (without revealing hidden tests).
pub fn seed_task(task: &EvalTask, workspace: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(workspace)?;
    for (rel, content) in &task.starter_files {
        let dst = workspace.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(dst, content)?;
    }
    Ok(())
}

/// Write the task's hidden test into `workspace` and run `hidden_test_command`.
/// Returns `(passed, combined_output)`.
pub fn run_hidden_test(task: &EvalTask, workspace: &Path) -> anyhow::Result<(bool, String)> {
    if task.hidden_test_command.is_empty() {
        anyhow::bail!("task {} has empty hidden_test_command", task.id);
    }
    let test_path = workspace.join(&task.hidden_test_file);
    if let Some(parent) = test_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&test_path, &task.hidden_test_content)?;
    let out = std::process::Command::new(&task.hidden_test_command[0])
        .args(&task.hidden_test_command[1..])
        .current_dir(workspace)
        .output()?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), combined))
}

/// Enforce the Item 5.2 regression gate:
/// - candidate pass rate must NOT be lower than baseline pass rate, AND
/// - candidate total cost must NOT exceed baseline total cost by more than `max_cost_increase_pct` (10%).
pub fn check_regression_gate(
    baseline: &[SuiteResultRow],
    candidate: &[SuiteResultRow],
    max_cost_increase_pct: f64,
) -> GateVerdict {
    let pass_rate = |rows: &[SuiteResultRow]| {
        if rows.is_empty() {
            0.0
        } else {
            rows.iter().filter(|r| r.passed).count() as f64 / rows.len() as f64
        }
    };
    let total_cost = |rows: &[SuiteResultRow]| rows.iter().map(|r| r.cost_usd).sum::<f64>();

    let base_pass = pass_rate(baseline);
    let cand_pass = pass_rate(candidate);
    let base_cost = total_cost(baseline);
    let cand_cost = total_cost(candidate);
    let cost_delta_pct = if base_cost > 1e-9 {
        ((cand_cost - base_cost) / base_cost) * 100.0
    } else {
        0.0
    };

    let mut reasons = Vec::new();
    if cand_pass + 1e-9 < base_pass {
        reasons.push(format!(
            "Pass rate regressed from {:.1}% to {:.1}%.",
            base_pass * 100.0,
            cand_pass * 100.0
        ));
    }
    if base_cost > 1e-9 && cost_delta_pct > max_cost_increase_pct {
        reasons.push(format!(
            "Cost rose by {:.1}% (${base_cost:.4} -> ${cand_cost:.4}), exceeding the {:.1}% cap.",
            cost_delta_pct, max_cost_increase_pct
        ));
    }

    GateVerdict {
        ok: reasons.is_empty(),
        baseline_pass_rate: base_pass,
        candidate_pass_rate: cand_pass,
        baseline_cost_usd: base_cost,
        candidate_cost_usd: cand_cost,
        cost_delta_pct,
        reasons,
    }
}

/// Parse JSONL eval rows from disk.
pub fn read_results_jsonl(path: &Path) -> anyhow::Result<Vec<SuiteResultRow>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<SuiteResultRow>(l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, passed: bool, cost: f64) -> SuiteResultRow {
        SuiteResultRow {
            at: "2026-11-22T00:00:00Z".into(),
            task_id: id.into(),
            category: "python".into(),
            language: "py".into(),
            difficulty: "normal".into(),
            passed,
            cost_usd: cost,
            seconds: 10.0,
            repairs: 0,
            cache_hit_rate: 0.5,
            brief_hash: None,
        }
    }

    #[test]
    fn regression_gate_blocks_pass_rate_drop_or_cost_spike_over_10_percent() {
        let base = vec![row("t1", true, 1.0), row("t2", true, 1.0)];
        let good = vec![row("t1", true, 1.05), row("t2", true, 1.05)];
        assert!(check_regression_gate(&base, &good, 10.0).ok);

        let regressed = vec![row("t1", true, 0.9), row("t2", false, 0.9)];
        let v1 = check_regression_gate(&base, &regressed, 10.0);
        assert!(!v1.ok);
        assert!(v1.reasons[0].contains("Pass rate regressed"));

        let expensive = vec![row("t1", true, 1.2), row("t2", true, 1.2)];
        let v2 = check_regression_gate(&base, &expensive, 10.0);
        assert!(!v2.ok);
        assert!(v2.reasons[0].contains("Cost rose by"));
    }

    #[test]
    fn bench_suite_loads_at_least_ten_tasks_and_hidden_tests_fail_on_starters() {
        let suite_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/suite");
        if !suite_dir.is_dir() {
            return;
        }
        let tasks = load_suite(&suite_dir).unwrap();
        assert!(tasks.len() >= 10, "expected >= 10 eval tasks, got {}", tasks.len());
        // Verify starter code fails hidden test on the first python task so hidden tests are non-vacuous.
        if let Some(py_task) = tasks.iter().find(|t| t.language == "py") {
            let tmp = std::env::temp_dir().join(format!("tm-eval-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            seed_task(py_task, &tmp).unwrap();
            let (passed, _) = run_hidden_test(py_task, &tmp).unwrap();
            assert!(!passed, "starter code should fail hidden test for {}", py_task.id);
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
}
