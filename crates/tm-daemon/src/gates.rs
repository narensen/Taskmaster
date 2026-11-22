//! Pipeline quality & correctness gates for Phase 2:
//! - Item 2.3: Red/green proof gate (bug-fix tests must fail on base and pass on result)
//!   plus contract-test mutation sanity (stubbing target code must fail the test).
//! - Item 2.4: Pre-approval plan review (ownership gaps, overlaps, oversized units, difficulty).
//! - Item 2.5: Best-of-N winner selection on hard units.
//! - Item 2.6: Worker release gate (send back once if worker skipped running tests).
//! - Item 2.12: Dynamic mid-flight Contract RFC classifier (additive auto-merge vs breaking blast radius).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use tm_core::{PageId, Plan};

// ---------------------------------------------------------------------------
// Item 2.3: Red/Green Proof Gate & Contract Mutation Sanity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedGreenResult {
    pub ok: bool,
    pub failed_on_base: bool,
    pub passed_on_candidate: bool,
    pub reason: String,
}

/// Run `test_cmd` against a `base_tree` (expected to FAIL = red) and against
/// `candidate_tree` (expected to PASS = green).
/// If the test already passes on the base tree, reject with a clear sentence.
pub fn verify_red_green(
    base_tree: &Path,
    candidate_tree: &Path,
    test_cmd: &[&str],
) -> anyhow::Result<RedGreenResult> {
    if test_cmd.is_empty() {
        anyhow::bail!("test command cannot be empty");
    }
    let base_ok = run_cmd_ok(base_tree, test_cmd)?;
    let cand_ok = run_cmd_ok(candidate_tree, test_cmd)?;
    if base_ok {
        return Ok(RedGreenResult {
            ok: false,
            failed_on_base: false,
            passed_on_candidate: cand_ok,
            reason: "Your new test already passes on the base commit; write a test that reproduces the bug first.".into(),
        });
    }
    if !cand_ok {
        return Ok(RedGreenResult {
            ok: false,
            failed_on_base: true,
            passed_on_candidate: false,
            reason: "The test failed on the base commit as expected, but still fails on your fix.".into(),
        });
    }
    Ok(RedGreenResult {
        ok: true,
        failed_on_base: true,
        passed_on_candidate: true,
        reason: "Red on base, green on candidate.".into(),
    })
}

/// Mutation sanity on a contract test: temporarily replace `impl_rel_path` in `worktree`
/// with a minimal throwing stub, run `test_cmd`, and restore the original file.
/// Returns `Ok(true)` if the contract test catches the broken implementation (fails),
/// `Ok(false)` if the test is vacuous (still passes with a broken implementation).
pub fn contract_mutation_sanity(
    worktree: &Path,
    impl_rel_path: &str,
    stub_content: &str,
    test_cmd: &[&str],
) -> anyhow::Result<bool> {
    let target = worktree.join(impl_rel_path);
    let original = std::fs::read(&target)?;
    std::fs::write(&target, stub_content)?;
    let result = run_cmd_ok(worktree, test_cmd);
    // Always restore original file even if command failed to spawn.
    let _ = std::fs::write(&target, &original);
    let passed_with_stub = result?;
    Ok(!passed_with_stub)
}

fn run_cmd_ok(dir: &Path, cmd: &[&str]) -> anyhow::Result<bool> {
    let status = Command::new(cmd[0]).args(&cmd[1..]).current_dir(dir).status()?;
    Ok(status.success())
}

// ---------------------------------------------------------------------------
// Item 2.4: Plan Review Before Approval
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReviewFinding {
    pub severity: String,
    pub category: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<PageId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReview {
    pub reviewer: String,
    pub findings: Vec<PlanReviewFinding>,
    pub revised: bool,
}

/// Deterministic structural plan review run before approval (and paired with a
/// second-model reviewer when configured): catches overlapping ownership globs,
/// missing contracts for multi-unit plans, oversized units, and miscalibrated difficulty.
pub fn review_plan_structure(plan: &Plan) -> Vec<PlanReviewFinding> {
    let mut findings = Vec::new();
    // 1. Overlapping ownership globs across pages.
    for i in 0..plan.pages.len() {
        for j in (i + 1)..plan.pages.len() {
            let a = &plan.pages[i];
            let b = &plan.pages[j];
            if globs_likely_overlap(&a.owned_glob, &b.owned_glob) {
                findings.push(PlanReviewFinding {
                    severity: "error".into(),
                    category: "overlap".into(),
                    message: format!(
                        "Units `{}` (`{}`) and `{}` (`{}`) have overlapping ownership globs.",
                        a.id, a.owned_glob, b.id, b.owned_glob
                    ),
                    page: Some(a.id.clone()),
                });
            }
        }
    }
    // 2. Multi-unit plans with zero contracts.
    if plan.pages.len() >= 2 && plan.contracts.is_empty() {
        findings.push(PlanReviewFinding {
            severity: "warning".into(),
            category: "missing_contracts".into(),
            message: "Multi-unit plan defines no locked contracts; parallel workers may drift on shared types.".into(),
            page: None,
        });
    }
    // 3. Oversized briefs or broad wildcard ownership (`**`).
    for page in &plan.pages {
        if page.owned_glob.trim() == "**" || page.owned_glob.trim() == "*" {
            findings.push(PlanReviewFinding {
                severity: "error".into(),
                category: "oversized_unit".into(),
                message: format!("Unit `{}` owns the entire repository (`{}`); narrow its glob.", page.id, page.owned_glob),
                page: Some(page.id.clone()),
            });
        }
        if page.brief.len() > 2500 && page.units.is_empty() {
            findings.push(PlanReviewFinding {
                severity: "warning".into(),
                category: "oversized_unit".into(),
                message: format!("Unit `{}` has a very large brief ({} chars) without sub-units; consider splitting it.", page.id, page.brief.len()),
                page: Some(page.id.clone()),
            });
        }
        if page.difficulty.as_deref().map(str::to_ascii_lowercase).as_deref() == Some("easy")
            && page.depends_on.len() >= 3
        {
            findings.push(PlanReviewFinding {
                severity: "warning".into(),
                category: "difficulty_mismatch".into(),
                message: format!("Unit `{}` is tagged `easy` but depends on {} other units; consider `normal` or `hard`.", page.id, page.depends_on.len()),
                page: Some(page.id.clone()),
            });
        }
    }
    findings
}

fn globs_likely_overlap(a: &str, b: &str) -> bool {
    let a = a.trim().trim_start_matches("./");
    let b = b.trim().trim_start_matches("./");
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b || a == "**" || b == "**" {
        return true;
    }
    fn prefix(g: &str) -> &str {
        g.split('*').next().unwrap_or(g).trim_end_matches('/')
    }
    let pa = prefix(a);
    let pb = prefix(b);
    if !pa.is_empty() && !pb.is_empty() {
        if a.contains('*') && (pb == pa || pb.starts_with(&format!("{pa}/"))) {
            return true;
        }
        if b.contains('*') && (pa == pb || pa.starts_with(&format!("{pb}/"))) {
            return true;
        }
    }
    false
}

/// Auto-fix mechanical findings from a plan review (narrowing overlapping globs and
/// raising miscalibrated `easy` difficulty on heavily dependent units).
pub fn apply_mechanical_plan_fixes(plan: &mut Plan) -> bool {
    let mut changed = false;
    let mut overlapping: BTreeSet<usize> = BTreeSet::new();
    for i in 0..plan.pages.len() {
        for j in (i + 1)..plan.pages.len() {
            if globs_likely_overlap(&plan.pages[i].owned_glob, &plan.pages[j].owned_glob) {
                overlapping.insert(i);
                overlapping.insert(j);
            }
        }
    }
    for idx in overlapping {
        plan.pages[idx].owned_glob = format!("src/{}/**", plan.pages[idx].id);
        changed = true;
    }
    for page in &mut plan.pages {
        if page.difficulty.as_deref().map(str::to_ascii_lowercase).as_deref() == Some("easy")
            && page.depends_on.len() >= 3
        {
            page.difficulty = Some("normal".into());
            changed = true;
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Item 2.5: Best-of-N Selection on Hard Units
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateResult {
    pub harness: String,
    pub model: Option<String>,
    pub worktree: String,
    pub checks_passed: bool,
    pub tests_passed: bool,
    pub warnings: usize,
    #[serde(default)]
    pub diff_lines: usize,
    pub cost_usd: f64,
    pub seconds: f64,
}

/// Pick the winning candidate from a Best-of-N race on a `hard` unit.
/// Prefers: (1) checks + tests passed, (2) checks passed, (3) fewest warnings,
/// (4) smaller diff, (5) lowest cost, (6) fastest wall time. Returns `(winner_index, loser_worktrees)`.
pub fn pick_best_candidate(candidates: &[CandidateResult]) -> Option<(usize, Vec<String>)> {
    if candidates.is_empty() {
        return None;
    }
    let best_idx = (0..candidates.len()).min_by(|&i, &j| {
        let a = &candidates[i];
        let b = &candidates[j];
        let score = |c: &CandidateResult| match (c.checks_passed, c.tests_passed) {
            (true, true) => 0u8,
            (true, false) => 1u8,
            (false, true) => 2u8,
            (false, false) => 3u8,
        };
        score(a)
            .cmp(&score(b))
            .then_with(|| a.warnings.cmp(&b.warnings))
            .then_with(|| a.diff_lines.cmp(&b.diff_lines))
            .then_with(|| a.cost_usd.partial_cmp(&b.cost_usd).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.seconds.partial_cmp(&b.seconds).unwrap_or(std::cmp::Ordering::Equal))
    })?;
    let losers = candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != best_idx)
        .map(|(_, c)| c.worktree.clone())
        .collect();
    Some((best_idx, losers))
}

/// True when a prompt asks to fix a bug or regression (eligible for W2 red/green proof).
pub fn is_bug_fix_prompt(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    ["fix ", "bug", "regression", "broken", "crash", "failing", "fails ", "error when", "doesn't work", "does not work"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// True when a repository-relative path looks like an automated test file.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = Path::new(&lower).file_name().and_then(|s| s.to_str()).unwrap_or("");
    lower.starts_with("tests/")
        || lower.starts_with("test/")
        || lower.contains("/tests/")
        || lower.contains("/__tests__/")
        || name.starts_with("test_")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
}

/// Minimal throwing stub for an implementation file during W4 contract mutation sanity.
pub fn mutation_stub_for(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".py") {
        Some("raise RuntimeError('taskmaster mutation stub')\n")
    } else if lower.ends_with(".ts") || lower.ends_with(".tsx") || lower.ends_with(".js") || lower.ends_with(".jsx") || lower.ends_with(".mjs") {
        Some("throw new Error('taskmaster mutation stub');\n")
    } else if lower.ends_with(".sh") {
        Some("#!/usr/bin/env sh\nexit 1\n")
    } else {
        None
    }
}

/// Choose a direct runnable command for a single standalone test file, when possible.
pub fn standalone_test_command(path: &str) -> Option<Vec<String>> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".py") {
        Some(vec!["python3".into(), path.into()])
    } else if lower.ends_with(".mjs") || lower.ends_with(".cjs") || lower.ends_with(".js") {
        Some(vec!["node".into(), path.into()])
    } else if lower.ends_with(".sh") {
        Some(vec!["sh".into(), path.into()])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Item 2.6: Release Gate (ensure worker ran tests before releasing)
// ---------------------------------------------------------------------------

/// Inspect a worker's transcript entries (`agents.jsonl` style JSON values) to determine
/// whether the worker ran tests before releasing.
pub fn transcript_ran_tests(entries: &[serde_json::Value], agent_id: &str) -> bool {
    for entry in entries {
        let matches_agent = entry["agent"]["id"].as_str() == Some(agent_id)
            || entry["agent"].as_str() == Some(agent_id);
        if !matches_agent {
            continue;
        }
        let text = entry.to_string().to_ascii_lowercase();
        for needle in [
            "testaffected",
            "npm test",
            "vitest",
            "jest",
            "pytest",
            "cargo test",
            "go test",
            "await test(",
            "\"tool\":\"test\"",
            "run_tests",
        ] {
            if text.contains(needle) {
                return true;
            }
        }
    }
    false
}

/// If the worker could run tests, did not run any, and has not already been nudged once,
/// return the single release-gate nudge prompt.
pub fn release_gate_nudge(
    can_run_tests: bool,
    already_nudged: bool,
    entries: &[serde_json::Value],
    agent_id: &str,
) -> Option<&'static str> {
    if !can_run_tests || already_nudged || transcript_ran_tests(entries, agent_id) {
        return None;
    }
    Some("Release gate: you have not run the unit's tests yet. Run the relevant tests now, fix any failure, and then finish.")
}

// ---------------------------------------------------------------------------
// Item 2.12: Dynamic Mid-Flight Contract RFCs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractChangeKind {
    /// Only adds new exports or optional fields; zero existing signatures removed or altered.
    Additive,
    /// Removes or alters an existing exported signature; requires pausing impacted dependents.
    Breaking,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractProposalEvaluation {
    pub path: String,
    pub kind: ContractChangeKind,
    pub auto_approved: bool,
    pub added_symbols: Vec<String>,
    pub changed_or_removed_symbols: Vec<String>,
    pub impacted_pages: Vec<PageId>,
}

/// Classify a proposed contract edit by comparing exported symbol signatures between
/// `old_content` and `new_content`, and compute the blast radius across `plan.pages`
/// using each page's cited/imported symbols.
pub fn evaluate_contract_proposal(
    path: &str,
    old_content: &str,
    new_content: &str,
    page_imports: &BTreeMap<PageId, Vec<String>>,
) -> ContractProposalEvaluation {
    let old_syms: BTreeMap<String, String> = crate::lsp::extract_symbols(path, old_content)
        .into_iter()
        .filter(|s| s.exported)
        .map(|s| (s.name, s.signature))
        .collect();
    let new_syms: BTreeMap<String, String> = crate::lsp::extract_symbols(path, new_content)
        .into_iter()
        .filter(|s| s.exported)
        .map(|s| (s.name, s.signature))
        .collect();

    let mut added = Vec::new();
    let mut changed_or_removed = Vec::new();
    for (name, old_sig) in &old_syms {
        match new_syms.get(name) {
            None => changed_or_removed.push(name.clone()),
            Some(new_sig) if new_sig != old_sig => changed_or_removed.push(name.clone()),
            Some(_) => {}
        }
    }
    for name in new_syms.keys() {
        if !old_syms.contains_key(name) {
            added.push(name.clone());
        }
    }

    let kind = if changed_or_removed.is_empty() {
        ContractChangeKind::Additive
    } else {
        ContractChangeKind::Breaking
    };
    let impacted_pages: Vec<PageId> = if kind == ContractChangeKind::Breaking {
        page_imports
            .iter()
            .filter(|(_, imports)| {
                imports.iter().any(|imp| {
                    imp == path || changed_or_removed.iter().any(|sym| imp.contains(sym))
                })
            })
            .map(|(page, _)| page.clone())
            .collect()
    } else {
        Vec::new()
    };

    ContractProposalEvaluation {
        path: path.to_string(),
        auto_approved: kind == ContractChangeKind::Additive,
        kind,
        added_symbols: added,
        changed_or_removed_symbols: changed_or_removed,
        impacted_pages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{ContractFile, PagePlan};

    #[test]
    fn red_green_gate_rejects_test_that_passes_on_base_and_accepts_red_then_green() {
        let root = std::env::temp_dir().join(format!("tm-rg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("base");
        let cand = root.join("cand");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&cand).unwrap();

        // Vacuous test: passes on both base and candidate -> rejected!
        std::fs::write(base.join("check.py"), "assert 1 + 1 == 2\n").unwrap();
        std::fs::write(cand.join("check.py"), "assert 1 + 1 == 2\n").unwrap();
        let vacuous = verify_red_green(&base, &cand, &["python3", "check.py"]).unwrap();
        assert!(!vacuous.ok);
        assert!(vacuous.reason.contains("already passes on the base commit"));

        // Real bug-fix test: fails on base, passes on candidate -> accepted!
        std::fs::write(base.join("app.py"), "def add(a, b): return a - b\n").unwrap();
        std::fs::write(cand.join("app.py"), "def add(a, b): return a + b\n").unwrap();
        std::fs::write(base.join("test_add.py"), "import app\nassert app.add(2, 3) == 5\n").unwrap();
        std::fs::write(cand.join("test_add.py"), "import app\nassert app.add(2, 3) == 5\n").unwrap();
        let valid = verify_red_green(&base, &cand, &["python3", "test_add.py"]).unwrap();
        assert!(valid.ok);
        assert!(valid.failed_on_base && valid.passed_on_candidate);

        // Mutation sanity: stubbing app.py breaks test_add.py -> true (non-vacuous).
        let caught = contract_mutation_sanity(
            &cand,
            "app.py",
            "def add(a, b): raise RuntimeError('stub')\n",
            &["python3", "test_add.py"],
        )
        .unwrap();
        assert!(caught);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_review_detects_overlapping_globs_and_fixes_them() {
        let mut plan = Plan {
            pages: vec![
                PagePlan { id: "a".into(), owned_glob: "src/**".into(), ..Default::default() },
                PagePlan { id: "b".into(), owned_glob: "src/**".into(), ..Default::default() },
            ],
            contracts: vec![ContractFile { path: "src/types.ts".into(), content: "export type X = number;".into() }],
            ..Default::default()
        };
        let findings = review_plan_structure(&plan);
        assert!(findings.iter().any(|f| f.category == "overlap"));
        assert!(apply_mechanical_plan_fixes(&mut plan));
        assert!(review_plan_structure(&plan).iter().all(|f| f.category != "overlap"));
    }

    #[test]
    fn best_of_n_picks_passing_cheapest_winner_and_lists_losers() {
        let cands = vec![
            CandidateResult {
                harness: "a".into(),
                model: None,
                worktree: "/tmp/wt-a".into(),
                checks_passed: true,
                tests_passed: false,
                warnings: 0,
                diff_lines: 10,
                cost_usd: 0.02,
                seconds: 5.0,
            },
            CandidateResult {
                harness: "b".into(),
                model: None,
                worktree: "/tmp/wt-b".into(),
                checks_passed: true,
                tests_passed: true,
                warnings: 0,
                diff_lines: 20,
                cost_usd: 0.08,
                seconds: 9.0,
            },
        ];
        let (winner, losers) = pick_best_candidate(&cands).unwrap();
        assert_eq!(winner, 1);
        assert_eq!(losers, vec!["/tmp/wt-a".to_string()]);
    }

    #[test]
    fn release_gate_nudges_once_when_worker_skips_tests() {
        let empty: Vec<serde_json::Value> = vec![serde_json::json!({
            "agent": { "id": "page:cart" },
            "kind": "tool",
            "tool": "write_file"
        })];
        assert!(release_gate_nudge(true, false, &empty, "page:cart").is_some());
        assert!(release_gate_nudge(true, true, &empty, "page:cart").is_none());

        let tested = vec![serde_json::json!({
            "agent": { "id": "page:cart" },
            "kind": "tool",
            "command": "npm test"
        })];
        assert!(release_gate_nudge(true, false, &tested, "page:cart").is_none());
    }

    #[test]
    fn contract_rfc_auto_approves_additive_and_flags_breaking_blast_radius() {
        let old = "export interface User { id: string; }\nexport function getUser(id: string): User;\n";
        let additive = "export interface User { id: string; }\nexport function getUser(id: string): User;\nexport function listUsers(): User[];\n";
        let breaking = "export interface User { id: string; }\nexport function getUser(id: number): User;\n";
        let mut imports = BTreeMap::new();
        imports.insert("profile".into(), vec!["getUser".into()]);
        imports.insert("billing".into(), vec!["Invoice".into()]);

        let eval_add = evaluate_contract_proposal("src/contracts.ts", old, additive, &imports);
        assert_eq!(eval_add.kind, ContractChangeKind::Additive);
        assert!(eval_add.auto_approved);
        assert_eq!(eval_add.added_symbols, vec!["listUsers"]);
        assert!(eval_add.impacted_pages.is_empty());

        let eval_brk = evaluate_contract_proposal("src/contracts.ts", old, breaking, &imports);
        assert_eq!(eval_brk.kind, ContractChangeKind::Breaking);
        assert!(!eval_brk.auto_approved);
        assert_eq!(eval_brk.changed_or_removed_symbols, vec!["getUser"]);
        assert_eq!(eval_brk.impacted_pages, vec!["profile".to_string()]);
    }
}
