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
use sha2::{Digest, Sha256};
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

/// Verdict of running a contract test against an unmutated baseline and a stubbed implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MutationVerdict {
    /// Baseline ran and passed (with at least one test executed), and stubbing the target made it fail.
    Caught,
    /// Baseline ran and passed, and the test still passed after stubbing the target.
    Vacuous,
    /// Could not verify non-vacuity (e.g., baseline failed, all tests were skipped, or zero tests passed).
    Inconclusive,
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Deterministic cache key for a mutation-gate verdict keyed by `(contract hash, target hash, command)`.
pub fn mutation_cache_key(
    contract_rel_path: &str,
    contract_bytes: &[u8],
    impl_rel_path: &str,
    target_bytes: &[u8],
    test_cmd: &[&str],
) -> String {
    let contract_hash = sha256_hex(&[contract_rel_path.as_bytes(), contract_bytes]);
    let target_hash = sha256_hex(&[impl_rel_path.as_bytes(), target_bytes]);
    let cmd_joined = test_cmd.join("\0");
    let cmd_hash = sha256_hex(&[cmd_joined.as_bytes()]);
    format!("{contract_hash}:{target_hash}:{}", &cmd_hash[..16])
}

/// Cached variant of [`contract_mutation_sanity_with_env`]: returns `(verdict, cache_hit)`.
/// When `(contract hash, target hash, test_cmd)` is already in `cache`, returns the cached
/// verdict without copying the tree or spawning the test runner.
pub fn contract_mutation_sanity_cached(
    worktree: &Path,
    contract_rel_path: &str,
    impl_rel_path: &str,
    stub_content: &str,
    test_cmd: &[&str],
    extra_env: &[(String, String)],
    cache: &mut BTreeMap<String, MutationVerdict>,
) -> anyhow::Result<(MutationVerdict, bool)> {
    let contract_bytes = std::fs::read(worktree.join(contract_rel_path))?;
    let target_bytes = std::fs::read(worktree.join(impl_rel_path))?;
    let key = mutation_cache_key(contract_rel_path, &contract_bytes, impl_rel_path, &target_bytes, test_cmd);
    if let Some(&verdict) = cache.get(&key) {
        return Ok((verdict, true));
    }
    let verdict = contract_mutation_sanity_with_env(worktree, impl_rel_path, stub_content, test_cmd, extra_env)?;
    cache.insert(key, verdict);
    Ok((verdict, false))
}

/// Mutation sanity on a contract test: copy `worktree` into an isolated temporary
/// directory, verify the unmutated baseline runs and passes at least one test, then
/// replace `impl_rel_path` in the copy with a minimal throwing stub and run
/// `test_cmd` there so the live shared working tree is never modified.
pub fn contract_mutation_sanity(
    worktree: &Path,
    impl_rel_path: &str,
    stub_content: &str,
    test_cmd: &[&str],
) -> anyhow::Result<MutationVerdict> {
    contract_mutation_sanity_with_env(worktree, impl_rel_path, stub_content, test_cmd, &[])
}

/// Like [`contract_mutation_sanity`], applying additional environment variables (such as
/// a project virtualenv from [`crate::runtime::steps`]) when running `test_cmd` inside the
/// isolated temporary copy.
pub fn contract_mutation_sanity_with_env(
    worktree: &Path,
    impl_rel_path: &str,
    stub_content: &str,
    test_cmd: &[&str],
    extra_env: &[(String, String)],
) -> anyhow::Result<MutationVerdict> {
    if test_cmd.is_empty() {
        anyhow::bail!("test command cannot be empty");
    }
    if !crate::git::safe_rel(impl_rel_path) {
        anyhow::bail!("implementation path `{impl_rel_path}` leaves the worktree");
    }
    if !worktree.join(impl_rel_path).is_file() {
        anyhow::bail!("implementation file `{impl_rel_path}` does not exist");
    }

    struct TempMutationDir(std::path::PathBuf);
    impl Drop for TempMutationDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = TempMutationDir(std::env::temp_dir().join(format!(
        "tm-mut-{}-{seq}",
        std::process::id()
    )));
    let _ = std::fs::remove_dir_all(&tmp.0);
    std::fs::create_dir_all(&tmp.0)?;
    copy_tree_for_mutation(worktree, &tmp.0)?;

    let is_pytest = is_pytest_cmd(test_cmd);
    let is_cargo = is_cargo_test_cmd(test_cmd);
    let augmented = if is_pytest {
        augment_pytest_cmd(test_cmd)
    } else {
        test_cmd.iter().map(|s| (*s).to_string()).collect()
    };
    let effective_refs: Vec<&str> = augmented.iter().map(String::as_str).collect();

    // 1. Baseline check on the unmutated copy: if baseline fails or zero tests pass
    //    (e.g., all tests skipped via skipif), the gate is inconclusive rather than vacuous.
    let baseline = run_cmd_output_with_env(&tmp.0, &effective_refs, extra_env)?;
    if !baseline.status.success() {
        return Ok(MutationVerdict::Inconclusive);
    }
    if !run_passed_at_least_one_test(&baseline, is_pytest, is_cargo) {
        return Ok(MutationVerdict::Inconclusive);
    }

    // 2. Replace target implementation with a failing stub and re-run.
    let target = tmp.0.join(impl_rel_path);
    std::fs::write(&target, stub_content)?;
    let mutated = run_cmd_output_with_env(&tmp.0, &effective_refs, extra_env)?;
    if !mutated.status.success() {
        return Ok(MutationVerdict::Caught);
    }
    if !run_passed_at_least_one_test(&mutated, is_pytest, is_cargo) {
        return Ok(MutationVerdict::Inconclusive);
    }
    Ok(MutationVerdict::Vacuous)
}

fn is_pytest_cmd(cmd: &[&str]) -> bool {
    let Some(first) = cmd.first() else { return false };
    let prog = Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first);
    if prog == "pytest" || prog == "pytest.exe" {
        return true;
    }
    if matches!(prog, "sh" | "bash" | "cmd" | "cmd.exe" | "powershell") {
        return false;
    }
    cmd.iter().skip(1).any(|a| *a == "pytest")
}

fn is_cargo_test_cmd(cmd: &[&str]) -> bool {
    let Some(first) = cmd.first() else { return false };
    let prog = Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first);
    (prog == "cargo" || prog == "cargo.exe") && cmd.get(1) == Some(&"test")
}

/// Add `-rA -p no:cacheprovider` to a pytest invocation so the summary always reports
/// executed/skipped counts cleanly without writing `.pytest_cache` in the temp dir.
fn augment_pytest_cmd(cmd: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = cmd.iter().map(|s| (*s).to_string()).collect();
    let insert_at = out
        .iter()
        .position(|a| a == "pytest")
        .map(|i| i + 1)
        .unwrap_or(1.min(out.len()));
    let has_ra = out.iter().any(|a| a == "-rA");
    let has_nocache = out
        .windows(2)
        .any(|w| w[0] == "-p" && w[1] == "no:cacheprovider")
        || out.iter().any(|a| a == "-pno:cacheprovider");
    let mut extra = Vec::new();
    if !has_ra {
        extra.push("-rA".to_string());
    }
    if !has_nocache {
        extra.push("-p".to_string());
        extra.push("no:cacheprovider".to_string());
    }
    out.splice(insert_at..insert_at, extra);
    out
}

fn run_passed_at_least_one_test(output: &std::process::Output, is_pytest: bool, is_cargo: bool) -> bool {
    if is_pytest {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let passed = parse_pytest_summary_passed(&stdout)
            .or_else(|| parse_pytest_summary_passed(&stderr));
        return matches!(passed, Some(n) if n > 0);
    }
    if is_cargo {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(n) = parse_cargo_test_passed(&stdout) {
            return n > 0;
        }
    }
    true
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for ch in chars.by_ref() {
                if ('@'..='~').contains(&ch) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_pytest_summary_passed(output: &str) -> Option<usize> {
    let clean = strip_ansi(output);
    const KNOWN_LABELS: &[&str] = &[
        "passed",
        "failed",
        "skipped",
        "deselected",
        "xfailed",
        "xpassed",
        "error",
        "errors",
        "warning",
        "warnings",
        "rerun",
        "reruns",
        "subtests",
    ];
    for line in clean.lines().rev() {
        let trimmed = line.trim().trim_matches('=').trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((left, right)) = trimmed.rsplit_once(" in ") else {
            continue;
        };
        let right = right.trim();
        let looks_like_duration = right
            .split_whitespace()
            .next()
            .is_some_and(|tok| tok.ends_with('s') && tok.chars().next().is_some_and(|c| c.is_ascii_digit()));
        if !looks_like_duration {
            continue;
        }
        let left = left.trim();
        if left == "no tests ran" {
            return Some(0);
        }
        let mut passed = 0usize;
        let mut matched_any = false;
        let mut all_valid = true;
        for part in left.split(',') {
            let mut words = part.split_whitespace();
            let Some(num_str) = words.next() else {
                all_valid = false;
                break;
            };
            let Some(label) = words.next() else {
                all_valid = false;
                break;
            };
            let label_rest = words.next();
            if words.next().is_some() {
                all_valid = false;
                break;
            }
            let Ok(n) = num_str.parse::<usize>() else {
                all_valid = false;
                break;
            };
            let known = KNOWN_LABELS.contains(&label)
                && label_rest.is_none_or(|w| KNOWN_LABELS.contains(&w));
            if !known {
                all_valid = false;
                break;
            }
            matched_any = true;
            if label == "passed" {
                passed += n;
            }
        }
        if matched_any && all_valid {
            return Some(passed);
        }
    }
    if let Some((_, after)) = clean.split_once("short test summary info") {
        let mut passed = 0usize;
        let mut saw_status = false;
        for line in after.lines() {
            let t = line.trim();
            if t.starts_with("PASSED ") {
                passed += 1;
                saw_status = true;
            } else if t.starts_with("SKIPPED ")
                || t.starts_with("FAILED ")
                || t.starts_with("ERROR ")
                || t.starts_with("XFAIL ")
                || t.starts_with("XPASS ")
            {
                saw_status = true;
            }
        }
        if saw_status {
            return Some(passed);
        }
    }
    None
}

fn parse_cargo_test_passed(output: &str) -> Option<usize> {
    let mut saw_result = false;
    let mut total_passed = 0usize;
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("test result:") {
            saw_result = true;
            for segment in rest.split(';') {
                let words: Vec<&str> = segment.split_whitespace().collect();
                if let Some(pos) = words.iter().position(|w| *w == "passed")
                    && pos > 0
                    && let Ok(n) = words[pos - 1].parse::<usize>()
                {
                    total_passed += n;
                }
            }
        }
    }
    saw_result.then_some(total_passed)
}

/// Copy `src` into an isolated `dst` directory for mutation testing without touching `src`.
/// Heavy/internal directories (`.git`, `.taskmaster`, `target`, caches) are skipped, and
/// read-only dependency trees (`node_modules`) are symlinked on Unix.
fn copy_tree_for_mutation(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(
            name_str.as_ref(),
            ".git" | ".taskmaster" | "__pycache__" | ".pytest_cache" | "target"
        ) {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            #[cfg(unix)]
            {
                if let Ok(target) = std::fs::read_link(&src_path) {
                    let _ = std::os::unix::fs::symlink(&target, &dst_path);
                }
            }
            #[cfg(not(unix))]
            {
                if src_path.is_file() {
                    std::fs::copy(&src_path, &dst_path)?;
                }
            }
        } else if ft.is_dir() {
            #[cfg(unix)]
            if matches!(name_str.as_ref(), "node_modules" | ".venv" | "venv") {
                std::os::unix::fs::symlink(&src_path, &dst_path)?;
                continue;
            }
            std::fs::create_dir_all(&dst_path)?;
            copy_tree_for_mutation(&src_path, &dst_path)?;
        } else if ft.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Command and environment variables selected for contract mutation sanity.
pub type MutationCommand = (Vec<String>, Vec<(String, String)>);

/// Pick the project's proper test command (the same one `verify` uses via `runtime::steps`)
/// for running W4 contract mutation sanity on `contract_path`.
pub fn mutation_test_command(
    tree: &Path,
    venv: &Path,
    contract_path: &str,
) -> Option<MutationCommand> {
    let steps = crate::runtime::steps(tree, venv);
    if let Some(step) = steps.into_iter().find(|s| s.name == "test") {
        let is_pytest = step.args.iter().any(|a| a == "pytest");
        if is_pytest && contract_path.to_ascii_lowercase().ends_with(".py") {
            let content = std::fs::read_to_string(tree.join(contract_path)).unwrap_or_default();
            let has_test_defs = content
                .lines()
                .map(str::trim_start)
                .any(|l| l.starts_with("def test_") || l.starts_with("async def test_") || l.starts_with("class Test"));
            if !has_test_defs {
                // Standalone assertion script without pytest test functions: pytest would exit 5
                // (no tests collected) even on an unmutated tree, so run it directly in the copy.
                return standalone_test_command(contract_path).map(|cmd| (cmd, Vec::new()));
            }
            let mut program = step.program;
            let mut args = step.args;
            let mut env = step.env;
            // If verify's venv wasn't created (e.g. runtime_checks disabled), fall back to system python3.
            let is_missing_path = (program.contains('/') || program.contains('\\')) && !Path::new(&program).is_file();
            if is_missing_path {
                program = "python3".into();
                env.retain(|(k, _)| k != "VIRTUAL_ENV" && k != "PATH");
            }
            args.push(contract_path.to_string());
            let mut cmd = Vec::with_capacity(1 + args.len());
            cmd.push(program);
            cmd.extend(args);
            return Some((cmd, env));
        }
        let mut cmd = Vec::with_capacity(1 + step.args.len());
        cmd.push(step.program);
        cmd.extend(step.args);
        return Some((cmd, step.env));
    }
    standalone_test_command(contract_path).map(|cmd| (cmd, Vec::new()))
}

fn run_cmd_ok(dir: &Path, cmd: &[&str]) -> anyhow::Result<bool> {
    Ok(run_cmd_output_with_env(dir, cmd, &[])?.status.success())
}

fn run_cmd_output_with_env(
    dir: &Path,
    cmd: &[&str],
    extra_env: &[(String, String)],
) -> anyhow::Result<std::process::Output> {
    let mut c = Command::new(cmd[0]);
    c.args(&cmd[1..])
        .current_dir(dir)
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for (k, v) in extra_env {
        c.env(k, v);
    }
    let output = c.output()?;
    Ok(output)
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
    // 4. Dependency chain depth cap for small-to-medium plans (<= 6 units): chains deeper than 2
    //    serialize workers unnecessarily when locked contracts already define the interfaces.
    if plan.pages.len() <= 6 {
        let depth = max_dependency_chain_depth(plan);
        if depth > 2 {
            findings.push(PlanReviewFinding {
                severity: "warning".into(),
                category: "deep_dependency_chain".into(),
                message: format!(
                    "Plan has {} units with a dependency chain of depth {} (max recommended is 2); rely on locked contracts so units run in parallel.",
                    plan.pages.len(),
                    depth
                ),
                page: None,
            });
        }
    }
    findings
}

/// Longest `depends_on` chain length (in nodes) across `plan.pages`.
/// Independent units have depth 1; `A <- B` has depth 2; `A <- B <- C` has depth 3.
pub fn max_dependency_chain_depth(plan: &Plan) -> usize {
    let by_id: BTreeMap<&str, &[String]> = plan
        .pages
        .iter()
        .map(|p| (p.id.as_str(), p.depends_on.as_slice()))
        .collect();
    fn depth_of<'a>(
        id: &'a str,
        by_id: &BTreeMap<&'a str, &'a [String]>,
        visiting: &mut BTreeSet<&'a str>,
        memo: &mut BTreeMap<&'a str, usize>,
    ) -> usize {
        if let Some(&d) = memo.get(id) {
            return d;
        }
        if !visiting.insert(id) {
            return 1;
        }
        let mut best = 1usize;
        if let Some(deps) = by_id.get(id) {
            for dep in *deps {
                if dep.as_str() != id && by_id.contains_key(dep.as_str()) {
                    best = best.max(1 + depth_of(dep.as_str(), by_id, visiting, memo));
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
        .map(|p| depth_of(p.id.as_str(), &by_id, &mut visiting, &mut memo))
        .max()
        .unwrap_or(0)
}

/// True when `consumer`'s dependency on `dep_id` is satisfied by a locked non-test contract
/// (so `consumer` can start immediately in parallel and re-verify when `dep_id` lands).
pub fn dependency_is_contract_only(plan: &Plan, consumer: &tm_core::PagePlan, dep_id: &str) -> bool {
    let interfaces: Vec<&tm_core::ContractFile> = plan.contracts.iter().filter(|c| !c.is_test()).collect();
    if interfaces.is_empty() {
        return false;
    }
    let Some(dep_page) = plan.pages.iter().find(|p| p.id == dep_id) else {
        return true;
    };
    let consumer_text = format!("{}\n{}\n{}", consumer.brief, consumer.context, consumer.done);
    let dep_text = format!("{}\n{}\n{}", dep_page.brief, dep_page.context, dep_page.done);

    // If the consumer explicitly references a concrete non-contract file owned by `dep_page`
    // (e.g. `tinydb/executor.py` or `tinydb.executor`), keep the hard dependency.
    let contract_paths: BTreeSet<&str> = interfaces.iter().map(|c| c.path.as_str()).collect();
    for concrete in concrete_paths_in_glob(&dep_page.owned_glob) {
        if contract_paths.contains(concrete.as_str()) || is_test_path(&concrete) {
            continue;
        }
        if text_references_concrete_file(&consumer_text, &concrete) {
            return false;
        }
    }

    // Collect declared symbols and paths from locked interface contracts.
    let mut contract_symbols: BTreeSet<String> = BTreeSet::new();
    for c in &interfaces {
        contract_symbols.extend(extract_declared_symbols(&c.content));
    }

    // Check whether a locked interface contract covers `dep_page` (symbol matching dep id or stem)
    // or both `dep_page` and `consumer` coordinate through the same locked contract path/symbol.
    let dep_tokens = dep_identity_tokens(dep_page);
    let contract_covers_dep = contract_symbols.iter().any(|sym| {
        let low = sym.to_ascii_lowercase();
        dep_tokens.iter().any(|tok| tok.len() >= 3 && low.contains(tok.as_str()))
    });
    let consumer_mentions_contract = interfaces.iter().any(|c| {
        consumer_text.contains(&c.path)
            || c.path
                .strip_suffix(".py")
                .map(|s| s.replace('/', "."))
                .is_some_and(|m| !m.is_empty() && consumer_text.contains(&m))
    }) || contract_symbols.iter().any(|sym| sym.len() >= 3 && consumer_text.contains(sym.as_str()));

    if contract_covers_dep && consumer_mentions_contract {
        return true;
    }

    // Both producer and consumer explicitly reference the same locked contract file or symbol.
    let shared_contract_path = interfaces.iter().any(|c| {
        let mod_name = c.path.strip_suffix(".py").map(|s| s.replace('/', "."));
        let in_dep = dep_text.contains(&c.path) || mod_name.as_deref().is_some_and(|m| !m.is_empty() && dep_text.contains(m));
        let in_con = consumer_text.contains(&c.path) || mod_name.as_deref().is_some_and(|m| !m.is_empty() && consumer_text.contains(m));
        in_dep && in_con
    });
    let shared_contract_sym = contract_symbols.iter().any(|sym| {
        sym.len() >= 3 && dep_text.contains(sym.as_str()) && consumer_text.contains(sym.as_str())
    });
    shared_contract_path || shared_contract_sym
}

fn concrete_paths_in_glob(glob: &str) -> Vec<String> {
    let trimmed = glob.trim();
    let parts: Vec<&str> = if let Some(inner) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        inner.split(',').map(str::trim).collect()
    } else {
        vec![trimmed]
    };
    parts
        .into_iter()
        .filter(|p| !p.is_empty() && !p.contains(['*', '?', '[', '{']))
        .map(|p| p.trim_start_matches("./").to_string())
        .collect()
}

fn text_references_concrete_file(text: &str, path: &str) -> bool {
    if text.contains(path) {
        return true;
    }
    if let Some(stem) = path.strip_suffix(".py") {
        let dotted = stem.replace('/', ".");
        if !dotted.is_empty() && text.contains(&dotted) {
            return true;
        }
    }
    for ext in [".ts", ".tsx", ".js", ".jsx", ".rs"] {
        if let Some(stem) = path.strip_suffix(ext)
            && !stem.is_empty()
            && text.contains(stem)
        {
            return true;
        }
    }
    false
}

fn dep_identity_tokens(page: &tm_core::PagePlan) -> Vec<String> {
    let mut tokens = vec![page.id.to_ascii_lowercase()];
    for part in page.id.split(['-', '_']) {
        if part.len() >= 3 {
            tokens.push(part.to_ascii_lowercase());
        }
    }
    for path in concrete_paths_in_glob(&page.owned_glob) {
        if let Some(stem) = Path::new(&path).file_stem().and_then(|s| s.to_str())
            && stem.len() >= 3
        {
            tokens.push(stem.to_ascii_lowercase());
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

fn extract_declared_symbols(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        for prefix in [
            "class ",
            "def ",
            "async def ",
            "export interface ",
            "export type ",
            "export class ",
            "export function ",
            "export const ",
            "export enum ",
            "interface ",
            "type ",
            "pub trait ",
            "pub struct ",
            "pub enum ",
            "pub fn ",
            "trait ",
            "struct ",
        ] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() && !name.starts_with("__") {
                    out.push(name);
                }
                break;
            }
        }
    }
    out
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
    // Flatten over-deep dependency chains (depth > 2 on <= 6 units) by dropping contract-only edges.
    if plan.pages.len() <= 6 && max_dependency_chain_depth(plan) > 2 {
        let snapshot = plan.clone();
        for (idx, page) in plan.pages.iter_mut().enumerate() {
            let before = page.depends_on.len();
            let page_snapshot = &snapshot.pages[idx];
            page.depends_on
                .retain(|dep| !dependency_is_contract_only(&snapshot, page_snapshot, dep));
            if page.depends_on.len() != before {
                changed = true;
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Prompt I/O Example Extraction & Plan-vs-Prompt Consistency
// ---------------------------------------------------------------------------

/// A literal input/output example extracted from the user prompt that should be
/// preserved verbatim and turned into a locked end-to-end acceptance check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptIoExample {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub output: String,
}

/// Extract concrete literal input/output examples from a user prompt (fenced code
/// blocks labeled with input/output/example cues, `input -> output` mappings, CLI
/// `$ cmd` transcripts, and inline backticked output format examples).
pub fn extract_prompt_io_examples(prompt: &str) -> Vec<PromptIoExample> {
    let mut examples = Vec::new();
    let mut seen_outputs: BTreeSet<String> = BTreeSet::new();
    let mut push = |label: String, input: Option<String>, output: String| {
        let trimmed = output.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let key = format!("{}::{}", input.as_deref().unwrap_or(""), trimmed);
        if seen_outputs.insert(key) {
            examples.push(PromptIoExample { label, input, output: trimmed });
        }
    };

    // 1. Scan fenced code blocks (``` ... ```) and their surrounding context.
    let lines: Vec<&str> = prompt.lines().collect();
    let mut i = 0;
    let mut pending_input: Option<String> = None;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("```") {
            let prev_ctx = if i > 0 { lines[i - 1].trim().to_ascii_lowercase() } else { String::new() };
            let start = i + 1;
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                i += 1;
            }
            let block_lines = &lines[start..i];
            let block = block_lines.join("\n").trim().to_string();
            if !block.is_empty() {
                // Check for `$ cmd` or `> cmd` followed by expected output lines inside one block.
                if let Some((cmd, out)) = split_transcript_block(block_lines) {
                    push("fenced transcript".into(), Some(cmd), out);
                    pending_input = None;
                } else if is_input_cue(&prev_ctx) && !is_output_cue(&prev_ctx) {
                    pending_input = Some(block);
                } else if is_output_cue(&prev_ctx) || looks_like_delimited_sample(&block) {
                    let input = pending_input.take();
                    push("fenced example".into(), input, block);
                } else if let Some((inp, out)) = split_labeled_block(block_lines) {
                    push("fenced input/output".into(), Some(inp), out);
                    pending_input = None;
                }
            }
            if i < lines.len() {
                i += 1;
            }
            continue;
        }

        // 2. Inline `input` -> `output` / `=>` / `→` pairs on a single line.
        if let Some((inp, out)) = parse_arrow_example(trimmed) {
            push("inline mapping".into(), Some(inp), out);
        }

        // 3. Inline `Input: ...` / `Output: ...` lines outside fenced blocks.
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = strip_label_prefix(trimmed, &["input:", "command:", "query:"]) {
            if !rest.is_empty() {
                pending_input = Some(strip_wrapping_ticks(rest));
            }
        } else if let Some(rest) = strip_label_prefix(trimmed, &["output:", "expected:", "expected output:", "prints:", "returns:", "produces:"]) {
            if !rest.is_empty() {
                let input = pending_input.take();
                push("labeled output".into(), input, strip_wrapping_ticks(rest));
            }
        } else if is_output_cue(&lower) {
            // 4. Inline backticked format literals on a line that describes output/format (e.g. `id|name`).
            for lit in backtick_literals(trimmed) {
                if looks_like_delimited_sample(&lit) {
                    push("inline format literal".into(), None, lit);
                }
            }
        }
        i += 1;
    }

    examples
}

/// Render planning instructions turning extracted prompt I/O examples into a locked
/// end-to-end acceptance check.
pub fn format_prompt_acceptance_guidance(examples: &[PromptIoExample]) -> String {
    if examples.is_empty() {
        return "\nAcceptance fidelity: if the request includes any literal input/output examples or format strings, \
                turn them into a locked executable end-to-end acceptance contract test asserting the exact output verbatim, \
                and never lock format constants (separators, spacing, headers, borders) that contradict the prompt.\n"
            .to_string();
    }
    let mut out = String::from(
        "\nPrompt literal I/O examples (MUST be preserved verbatim and locked as an end-to-end acceptance check):\n",
    );
    for ex in examples.iter().take(8) {
        match &ex.input {
            Some(inp) => {
                out.push_str(&format!("- Input: `{}` => Expected output:\n```\n{}\n```\n", inp.replace('\n', "\\n"), ex.output));
            }
            None => {
                out.push_str(&format!("- Expected literal output/format:\n```\n{}\n```\n", ex.output));
            }
        }
    }
    out.push_str(
        "Turn these literal examples into a locked executable end-to-end acceptance contract test \
         (e.g. `tests/test_acceptance_contract.py` or `tests/contract/acceptance.test.ts`, outside every unit's `owned_glob`) \
         that runs the public entrypoint and asserts the exact output verbatim. Copy every delimiter, spacing, header, and literal \
         string directly from the prompt; do NOT invent extra spaces (e.g. `' | '` when the prompt shows `id|name`) or unrequested \
         header rules/borders (e.g. `'-+-'`).\n",
    );
    out
}

/// Check a drafted plan's locked contracts and acceptance coverage against the user prompt.
/// Flags contract constants or literal output expectations that contradict the prompt (for
/// example locking `' | '` or `'-+-'` when the prompt specified `id|name`), and flags missing
/// end-to-end acceptance contract tests when the prompt provides concrete I/O examples.
pub fn check_plan_prompt_consistency(prompt: &str, plan: &Plan) -> Vec<PlanReviewFinding> {
    let mut findings = Vec::new();
    let prompt_trim = prompt.trim();
    if prompt_trim.is_empty() {
        return findings;
    }

    let prompt_literals = collect_prompt_literals(prompt);
    let delimiter_evidence = detect_prompt_delimiters(prompt, &prompt_literals);
    let prompt_mentions_rule = prompt_requests_header_rule(prompt);

    for contract in &plan.contracts {
        let constants = extract_contract_constants(&contract.content);
        for (name, value) in &constants {
            let lower_name = name.to_ascii_lowercase();

            // 1. Check separator / delimiter constants against prompt delimiter evidence.
            if is_separator_const_name(&lower_name)
                && !is_header_rule_const_name(&lower_name)
                && let Some(reason) = check_separator_contradiction(value, &delimiter_evidence, prompt)
            {
                findings.push(PlanReviewFinding {
                    severity: "error".into(),
                    category: "prompt_contradiction".into(),
                    message: format!(
                        "Contract `{}` defines `{name} = {value:?}`, which contradicts the prompt: {reason}",
                        contract.path
                    ),
                    page: None,
                });
            }

            // 2. Check header-rule / border / decorative divider constants not requested by the prompt.
            if is_header_rule_const_name(&lower_name)
                && !value.is_empty()
                && looks_like_rule_string(value)
                && !prompt_mentions_rule
                && !prompt.contains(value.as_str())
                && !prompt.contains(value.trim())
            {
                findings.push(PlanReviewFinding {
                    severity: "error".into(),
                    category: "prompt_contradiction".into(),
                    message: format!(
                        "Contract `{}` defines `{name} = {value:?}`, adding a header rule/border that the prompt did not ask for.",
                        contract.path
                    ),
                    page: None,
                });
            } else if !is_header_rule_const_name(&lower_name)
                && !delimiter_evidence.is_empty()
                && looks_like_table_junction(value)
                && !prompt_mentions_rule
                && !prompt.contains(value.as_str())
            {
                findings.push(PlanReviewFinding {
                    severity: "error".into(),
                    category: "prompt_contradiction".into(),
                    message: format!(
                        "Contract `{}` defines `{name} = {value:?}`, introducing a table rule/junction not present in the prompt.",
                        contract.path
                    ),
                    page: None,
                });
            }

            // 3. Check literal header/output/format constants whose spacing contradicts a prompt literal.
            if is_literal_output_const_name(&lower_name) && !value.trim().is_empty() && !prompt.contains(value.as_str()) {
                let norm_val = collapse_delim_spaces(value);
                if let Some(prompt_lit) = prompt_literals.iter().find(|lit| {
                    lit.len() >= 3 && *lit != value && collapse_delim_spaces(lit) == norm_val
                }) {
                    findings.push(PlanReviewFinding {
                        severity: "error".into(),
                        category: "prompt_contradiction".into(),
                        message: format!(
                            "Contract `{}` defines `{name} = {value:?}`, which contradicts the prompt's literal `{prompt_lit}`.",
                            contract.path
                        ),
                        page: None,
                    });
                }
            }
        }

        // 4. Also inspect string literals in contract files (including contract tests) that
        //    directly re-format a prompt literal with contradicting spaces (e.g. "id | name" vs `id|name`).
        for lit in extract_string_literals(&contract.content) {
            if lit.len() < 3 || prompt.contains(lit.as_str()) {
                continue;
            }
            let norm = collapse_delim_spaces(&lit);
            if let Some(prompt_lit) = prompt_literals.iter().find(|p| {
                p.len() >= 3 && has_delim_char(p) && *p != &lit && collapse_delim_spaces(p) == norm
            }) {
                let msg = format!(
                    "Contract `{}` uses literal `{lit}`, which contradicts the prompt's literal `{prompt_lit}`.",
                    contract.path
                );
                if !findings.iter().any(|f| f.message == msg) {
                    findings.push(PlanReviewFinding {
                        severity: "error".into(),
                        category: "prompt_contradiction".into(),
                        message: msg,
                        page: None,
                    });
                }
            }
            if !delimiter_evidence.is_empty()
                && looks_like_table_junction(&lit)
                && !prompt_mentions_rule
                && !prompt.contains(lit.as_str())
            {
                let already = findings
                    .iter()
                    .any(|f| f.category == "prompt_contradiction" && f.message.contains(&contract.path) && f.message.contains(&lit));
                if !already {
                    findings.push(PlanReviewFinding {
                        severity: "error".into(),
                        category: "prompt_contradiction".into(),
                        message: format!(
                            "Contract `{}` includes table rule/junction `{lit}` that contradicts the prompt's plain output format.",
                            contract.path
                        ),
                        page: None,
                    });
                }
            }
        }
    }

    // 5. Require a locked end-to-end acceptance contract test when the prompt gives concrete
    //    literal input/output examples (for multi-unit plans, or when a single-unit plan omits them).
    let examples = extract_prompt_io_examples(prompt);
    let concrete: Vec<&PromptIoExample> = examples
        .iter()
        .filter(|ex| ex.input.is_some() || ex.output.contains('\n'))
        .collect();
    if !concrete.is_empty() {
        let has_contract_test = plan.contracts.iter().any(|c| c.is_test());
        let single_unit_embeds = plan.pages.len() == 1
            && concrete
                .iter()
                .all(|ex| plan.pages[0].brief.contains(&ex.output));
        if !has_contract_test && !single_unit_embeds {
            let sample = concrete[0].output.lines().next().unwrap_or(&concrete[0].output);
            findings.push(PlanReviewFinding {
                severity: "error".into(),
                category: "missing_acceptance_check".into(),
                message: format!(
                    "The prompt provides literal input/output examples (such as `{sample}`), \
                     but the plan locks no executable end-to-end acceptance contract test \
                     (e.g. `tests/test_acceptance_contract.py` or `tests/contract/acceptance.test.ts`)."
                ),
                page: None,
            });
        }
    }

    findings
}

#[derive(Debug, Clone)]
struct DelimiterEvidence {
    ch: char,
    unpadded_sample: Option<String>,
    padded_sample: Option<String>,
}

fn is_input_cue(lower: &str) -> bool {
    ["input", "command", "query", "given", "request", "stdin", "invocation"]
        .iter()
        .any(|k| lower.contains(k))
}

fn is_output_cue(lower: &str) -> bool {
    [
        "output", "expected", "prints", "print", "returns", "return", "produces", "produce",
        "format", "formatted", "display", "stdout", "example", "result", "rows", "table",
        "separator", "delimiter", "delimited", "separated",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn strip_label_prefix<'a>(line: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    for p in prefixes {
        if lower.starts_with(p) {
            return Some(line[p.len()..].trim());
        }
    }
    None
}

fn strip_wrapping_ticks(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() >= 2
        && ((trimmed.starts_with('`') && trimmed.ends_with('`'))
            || (trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn split_transcript_block(lines: &[&str]) -> Option<(String, String)> {
    let first = lines.first()?.trim();
    let cmd = first
        .strip_prefix("$ ")
        .or_else(|| first.strip_prefix("> "))
        .map(str::trim)?;
    if cmd.is_empty() || lines.len() < 2 {
        return None;
    }
    let out = lines[1..].join("\n").trim().to_string();
    if out.is_empty() || out.starts_with("$ ") {
        return None;
    }
    Some((cmd.to_string(), out))
}

fn split_labeled_block(lines: &[&str]) -> Option<(String, String)> {
    let mut input: Option<String> = None;
    let mut output_lines: Vec<&str> = Vec::new();
    let mut in_output = false;
    for line in lines {
        let trimmed = line.trim();
        if let Some(rest) = strip_label_prefix(trimmed, &["input:", "command:", "query:"]) {
            input = Some(strip_wrapping_ticks(rest));
            in_output = false;
        } else if let Some(rest) = strip_label_prefix(trimmed, &["output:", "expected:", "result:", "returns:"]) {
            if !rest.is_empty() {
                output_lines.push(rest);
            }
            in_output = true;
        } else if in_output {
            output_lines.push(line);
        }
    }
    let out = output_lines.join("\n").trim().to_string();
    match (input, out.is_empty()) {
        (Some(inp), false) => Some((inp, out)),
        _ => None,
    }
}

fn parse_arrow_example(line: &str) -> Option<(String, String)> {
    for arrow in [" => ", " -> ", " → "] {
        if let Some((left, right)) = line.split_once(arrow) {
            let l = left.trim().trim_start_matches('-').trim_start_matches('*').trim();
            let r = right.trim();
            if (l.starts_with('`') || l.starts_with('"') || l.starts_with('\''))
                && (r.starts_with('`') || r.starts_with('"') || r.starts_with('\''))
            {
                let inp = strip_wrapping_ticks(l);
                let out = strip_wrapping_ticks(r);
                if !inp.is_empty() && !out.is_empty() {
                    return Some((inp, out));
                }
            }
        }
    }
    None
}

fn backtick_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else { break };
        let lit = &after[..end];
        if !lit.is_empty() {
            out.push(lit.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

fn has_delim_char(s: &str) -> bool {
    s.contains('|') || s.contains(',') || s.contains('\t') || s.contains(';')
}

fn looks_like_delimited_sample(s: &str) -> bool {
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for delim in ['|', '\t'] {
            if let Some((a, b)) = line.split_once(delim) {
                let a = a.trim();
                let b = b.trim();
                if !a.is_empty()
                    && !b.is_empty()
                    && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
                    && b.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ' | '|' | '\t'))
                {
                    return true;
                }
            }
        }
    }
    false
}

fn collect_prompt_literals(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in prompt.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        } else {
            out.extend(backtick_literals(line));
        }
    }
    out
}

fn detect_prompt_delimiters(prompt: &str, literals: &[String]) -> Vec<DelimiterEvidence> {
    let mut by_char: BTreeMap<char, DelimiterEvidence> = BTreeMap::new();
    let lower = prompt.to_ascii_lowercase();
    for (phrase, ch) in [
        ("pipe-separated", '|'),
        ("pipe-delimited", '|'),
        ("pipe separated", '|'),
        ("pipe delimited", '|'),
        ("separated by `|`", '|'),
        ("separated by '|'", '|'),
        ("separated by \"|\"", '|'),
        ("tab-separated", '\t'),
        ("tab-delimited", '\t'),
        ("comma-separated", ','),
        ("comma-delimited", ','),
    ] {
        if lower.contains(phrase) {
            by_char.entry(ch).or_insert_with(|| DelimiterEvidence {
                ch,
                unpadded_sample: Some(phrase.to_string()),
                padded_sample: None,
            });
        }
    }
    for lit in literals {
        for ch in ['|', '\t', ','] {
            if !lit.contains(ch) {
                continue;
            }
            // Skip prose or code expressions (like `a || b`, `x | y` in types like `int | None`).
            if ch == '|' && (lit.contains("||") || lit.contains("None") || lit.contains("null") || lit.contains("undefined")) {
                continue;
            }
            if ch == ',' && lit.contains('(') {
                continue;
            }
            let padded = format!(" {ch} ");
            let parts: Vec<&str> = lit.split(ch).collect();
            let valid_fields = parts.len() >= 2
                && parts.iter().all(|p| {
                    let t = p.trim();
                    !t.is_empty()
                        && t.len() <= 32
                        && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
                });
            if !valid_fields {
                continue;
            }
            let entry = by_char.entry(ch).or_insert_with(|| DelimiterEvidence {
                ch,
                unpadded_sample: None,
                padded_sample: None,
            });
            if lit.contains(&padded) {
                if entry.padded_sample.is_none() {
                    entry.padded_sample = Some(lit.clone());
                }
            } else if entry.unpadded_sample.is_none() {
                entry.unpadded_sample = Some(lit.clone());
            }
        }
    }
    by_char.into_values().collect()
}

fn check_separator_contradiction(
    value: &str,
    evidence: &[DelimiterEvidence],
    prompt: &str,
) -> Option<String> {
    if value.is_empty() || evidence.is_empty() {
        return None;
    }
    let trimmed = value.trim();
    if trimmed.chars().count() == 1 {
        let ch = trimmed.chars().next().unwrap();
        let is_padded = value != trimmed;
        if let Some(ev) = evidence.iter().find(|e| e.ch == ch) {
            if is_padded && ev.unpadded_sample.is_some() && ev.padded_sample.is_none() && !prompt.contains(value) {
                return Some(format!(
                    "prompt specifies unpadded `{ch}` (as in `{}`), not `{value}`",
                    ev.unpadded_sample.as_deref().unwrap_or("")
                ));
            }
            if !is_padded && ev.padded_sample.is_some() && ev.unpadded_sample.is_none() {
                return Some(format!(
                    "prompt specifies padded ` {ch} ` (as in `{}`), not `{value}`",
                    ev.padded_sample.as_deref().unwrap_or("")
                ));
            }
            return None;
        }
        // Prompt specifies a single clear delimiter (e.g. `|`), but contract chose a different one (e.g. `,`).
        if evidence.len() == 1 && matches!(ch, '|' | ',' | '\t' | ';') && !prompt.contains(value) {
            let ev = &evidence[0];
            let sample = ev
                .unpadded_sample
                .as_deref()
                .or(ev.padded_sample.as_deref())
                .unwrap_or("");
            return Some(format!(
                "prompt uses `{}` delimiter (as in `{sample}`), not `{value}`",
                ev.ch
            ));
        }
    }
    None
}

fn prompt_requests_header_rule(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    [
        "header rule",
        "header separator",
        "separator line",
        "divider line",
        "horizontal rule",
        "underline",
        "border",
        "ascii table",
        "markdown table",
        "box drawing",
        "-+-",
        "---",
        "===",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

fn is_separator_const_name(lower: &str) -> bool {
    lower.contains("sep")
        || lower.contains("delim")
        || lower.contains("join")
        || lower.contains("col_div")
        || lower.contains("field_div")
}

fn is_header_rule_const_name(lower: &str) -> bool {
    lower.contains("header_rule")
        || lower.contains("header_sep")
        || lower.contains("header_line")
        || lower.contains("header_border")
        || lower.contains("header_div")
        || lower.contains("table_rule")
        || lower.contains("table_border")
        || lower.contains("row_rule")
        || lower.contains("sep_line")
        || lower.contains("separator_line")
        || lower.contains("divider_line")
        || lower.contains("underline")
        || lower.contains("hrule")
        || lower.contains("hline")
        || lower == "rule"
        || lower.ends_with("_rule")
        || lower.contains("border")
}

fn is_literal_output_const_name(lower: &str) -> bool {
    lower.contains("header")
        || lower.contains("output")
        || lower.contains("format")
        || lower.contains("template")
        || lower.contains("sample")
        || lower.contains("expected")
        || lower.contains("columns")
}

fn looks_like_rule_string(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| matches!(c, '-' | '+' | '=' | '|' | '_' | '─' | '┼' | '━' | '═' | ' '))
}

fn looks_like_table_junction(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 3
        && looks_like_rule_string(t)
        && (t.contains("-+-") || t.contains("+-+") || t.contains("---") || t.contains("===") || t.contains("─┼─"))
}

fn collapse_delim_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    for (idx, &ch) in chars.iter().enumerate() {
        if ch == ' ' {
            let prev = idx.checked_sub(1).and_then(|i| chars.get(i)).copied();
            let next = chars.get(idx + 1).copied();
            if prev.is_some_and(|c| matches!(c, '|' | ',' | ';' | '\t'))
                || next.is_some_and(|c| matches!(c, '|' | ',' | ';' | '\t'))
            {
                continue;
            }
        }
        out.push(ch);
    }
    out
}

/// Extract `(CONST_NAME, string_value)` pairs from constant assignments in Python, JS/TS, Rust, or Go contracts.
fn extract_contract_constants(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let mut line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        for prefix in ["export ", "pub ", "const ", "let ", "var ", "final ", "static "] {
            if let Some(rest) = line.strip_prefix(prefix) {
                line = rest.trim_start();
            }
        }
        let Some((lhs, rhs)) = line.split_once('=') else {
            continue;
        };
        if rhs.starts_with('=') {
            continue;
        }
        let name = lhs.split(':').next().unwrap_or(lhs).trim();
        if name.is_empty()
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || !name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            continue;
        }
        let rhs = rhs.trim().trim_end_matches(';').trim();
        if let Some(val) = parse_quoted_string_prefix(rhs) {
            out.push((name.to_string(), val));
        }
    }
    out
}

/// Parse a leading single- or double-quoted string literal (ignoring trailing comments).
fn parse_quoted_string_prefix(rhs: &str) -> Option<String> {
    let bytes = rhs.as_bytes();
    let quote = *bytes.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let mut out = String::new();
    let mut chars = rhs[1..].chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            }
        } else if c == quote as char {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

/// Extract quoted string literals from source text for contradiction checking.
fn extract_string_literals(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let mut i = 0;
        let bytes = line.as_bytes();
        while i < bytes.len() {
            if bytes[i] == b'"' || bytes[i] == b'\'' {
                let quote = bytes[i];
                let start = i;
                if let Some(val) = parse_quoted_string_prefix(&line[start..]) {
                    out.push(val);
                    // Advance past closing quote safely on UTF-8 char boundaries.
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' && i + 1 < bytes.len() {
                            let next_len = line[i + 1..].chars().next().map_or(1, char::len_utf8);
                            i += 1 + next_len;
                            continue;
                        }
                        if bytes[i] == quote {
                            i += 1;
                            break;
                        }
                        i += line[i..].chars().next().map_or(1, char::len_utf8);
                    }
                    continue;
                }
            }
            i += line[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
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
    } else if lower.ends_with(".rs") {
        Some("compile_error!(\"taskmaster mutation stub\");\n")
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

        // Mutation sanity: stubbing app.py breaks test_add.py -> Caught (non-vacuous).
        let caught = contract_mutation_sanity(
            &cand,
            "app.py",
            "def add(a, b): raise RuntimeError('stub')\n",
            &["python3", "test_add.py"],
        )
        .unwrap();
        assert_eq!(caught, MutationVerdict::Caught);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn plan_review_detects_overlapping_globs_and_fixes_them() {
        let mut plan = Plan {
            pages: vec![
                PagePlan { id: "a".into(), owned_glob: "src/**".into(), ..Default::default() },
                PagePlan { id: "b".into(), owned_glob: "src/**".into(), ..Default::default() },
            ],
            contracts: vec![ContractFile { path: "src/types.ts".into(), content: "export type X = number;".into(), phase: 0 }],
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

    #[test]
    fn prompt_io_examples_become_acceptance_guidance_and_catch_contradicting_constants() {
        let prompt = "Build a mini SQL CLI.\n\
            Query results are formatted as pipe-separated rows like `id|name`.\n\n\
            Input:\n\
            ```sql\n\
            SELECT id, name FROM users;\n\
            ```\n\
            Output:\n\
            ```\n\
            id|name\n\
            1|alice\n\
            ```\n";

        let examples = extract_prompt_io_examples(prompt);
        assert!(!examples.is_empty(), "expected extracted I/O examples");
        assert!(examples.iter().any(|e| e.output.contains("id|name") && e.output.contains("1|alice")), "{examples:#?}");

        let guidance = format_prompt_acceptance_guidance(&examples);
        assert!(guidance.contains("id|name"), "{guidance}");
        assert!(guidance.contains("locked executable end-to-end acceptance contract test"), "{guidance}");

        // Plan that locks contradicting constants (' | ' separator and '-+-' header rule) and omits an acceptance test.
        let bad_plan = Plan {
            kind: tm_core::ProjectKind::Cli,
            contracts: vec![ContractFile {
                path: "minisql/format.py".into(),
                content: "SEPARATOR = ' | '\nHEADER_RULE = '-+-'\n".into(),
                phase: 0,
            }],
            pages: vec![
                PagePlan { id: "parser".into(), owned_glob: "minisql/parser.py".into(), brief: "Parse SQL.".into(), ..Default::default() },
                PagePlan { id: "engine".into(), owned_glob: "minisql/engine.py".into(), brief: "Run SQL.".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let findings = check_plan_prompt_consistency(prompt, &bad_plan);
        assert!(
            findings.iter().any(|f| f.category == "prompt_contradiction" && f.message.contains("SEPARATOR")),
            "expected SEPARATOR contradiction finding, got: {findings:#?}"
        );
        assert!(
            findings.iter().any(|f| f.category == "prompt_contradiction" && f.message.contains("HEADER_RULE")),
            "expected HEADER_RULE contradiction finding, got: {findings:#?}"
        );
        assert!(
            findings.iter().any(|f| f.category == "missing_acceptance_check"),
            "expected missing acceptance test finding, got: {findings:#?}"
        );

        // Plan that matches the prompt ('|' separator, no '-+-' rule, locked acceptance contract test) passes cleanly.
        let good_plan = Plan {
            kind: tm_core::ProjectKind::Cli,
            contracts: vec![
                ContractFile {
                    path: "minisql/format.py".into(),
                    content: "SEPARATOR = '|'\n".into(),
                    phase: 0,
                },
                ContractFile {
                    path: "tests/test_acceptance_contract.py".into(),
                    content: "def test_select_output():\n    assert 'id|name\\n1|alice' == 'id|name\\n1|alice'\n".into(),
                    phase: 0,
                },
            ],
            pages: vec![
                PagePlan { id: "parser".into(), owned_glob: "minisql/parser.py".into(), brief: "Parse SQL.".into(), ..Default::default() },
                PagePlan { id: "engine".into(), owned_glob: "minisql/engine.py".into(), brief: "Run SQL.".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let good_findings = check_plan_prompt_consistency(prompt, &good_plan);
        assert!(good_findings.is_empty(), "expected clean plan to pass, got: {good_findings:#?}");
    }

    #[test]
    fn mutation_gate_runs_in_isolated_temp_copy_with_project_test_command() {
        let root = std::env::temp_dir().join(format!("tm-mut-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tests_dir = root.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let orig_impl = "def add(a, b):\n    return a + b\n";
        let live_impl = root.join("app.py");
        std::fs::write(&live_impl, orig_impl).unwrap();
        let marker = root.join(".live-check-result");

        // Contract test with a pytest-style def test_* function that checks:
        // 1. Live shared tree's app.py was NOT modified while the mutation test ran.
        // 2. CWD is an isolated temporary copy, not the live tree.
        // 3. Importing app in the isolated copy hits the mutation stub and fails the test.
        let contract_test = r#"import os
from pathlib import Path

def test_isolated_and_catches_stub():
    live_impl = Path(os.environ["LIVE_IMPL_PATH"])
    live_root = Path(os.environ["LIVE_ROOT_PATH"])
    marker = Path(os.environ["LIVE_MARKER_PATH"])
    live_content = live_impl.read_text()
    same_dir = Path.cwd().resolve() == live_root.resolve()
    marker.write_text(f"same_dir={same_dir}\ncontent={live_content}")
    import app
    assert app.add(2, 3) == 5
"#;
        std::fs::write(tests_dir.join("test_contract.py"), contract_test).unwrap();

        // mutation_test_command must select pytest (not bare `python3 tests/test_contract.py`)
        // for a pytest-style Python test file.
        let (cmd, mut env) = mutation_test_command(&root, &root.join(".taskmaster/venv"), "tests/test_contract.py")
            .expect("expected project test command for pytest file");
        assert!(
            cmd.iter().any(|a| a == "pytest"),
            "expected pytest command from runtime::steps, got: {cmd:?}"
        );
        assert!(cmd.iter().any(|a| a == "tests/test_contract.py"), "expected contract test path in cmd: {cmd:?}");

        env.push(("LIVE_IMPL_PATH".into(), live_impl.display().to_string()));
        env.push(("LIVE_ROOT_PATH".into(), root.display().to_string()));
        env.push(("LIVE_MARKER_PATH".into(), marker.display().to_string()));
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let caught = contract_mutation_sanity_with_env(
            &root,
            "app.py",
            mutation_stub_for("app.py").unwrap(),
            &refs,
            &env,
        )
        .unwrap();
        assert_eq!(caught, MutationVerdict::Caught, "non-vacuous pytest contract test must fail against the stub");
        let observed = std::fs::read_to_string(&marker).expect("pytest test must have executed");
        assert!(observed.starts_with("same_dir=False\n"), "must run in isolated temp copy, got: {observed}");
        assert!(
            observed.contains(orig_impl),
            "live shared tree file must stay untouched while mutation test runs, got: {observed}"
        );
        assert_eq!(std::fs::read_to_string(&live_impl).unwrap(), orig_impl);

        // Rust projects select cargo test via runtime::steps.
        let rs_root = root.join("rust_proj");
        std::fs::create_dir_all(rs_root.join("tests")).unwrap();
        std::fs::write(rs_root.join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(rs_root.join("tests/contract_test.rs"), "#[test]\nfn t() {}\n").unwrap();
        let (rs_cmd, _) = mutation_test_command(&rs_root, &rs_root.join(".taskmaster/venv"), "tests/contract_test.rs")
            .expect("expected cargo test command for Rust project");
        assert_eq!(rs_cmd[0], "cargo");
        assert_eq!(rs_cmd[1], "test");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mutation_gate_baseline_red_is_inconclusive() {
        let root = std::env::temp_dir().join(format!("tm-mut-base-red-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tests_dir = root.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        std::fs::write(root.join("app.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::write(
            tests_dir.join("test_contract.py"),
            "import app\n\ndef test_sum():\n    assert app.add(2, 3) == 5\n",
        )
        .unwrap();

        let (cmd, env) = mutation_test_command(&root, &root.join(".taskmaster/venv"), "tests/test_contract.py")
            .expect("expected pytest command");
        let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let verdict = contract_mutation_sanity_with_env(
            &root,
            "app.py",
            mutation_stub_for("app.py").unwrap(),
            &refs,
            &env,
        )
        .unwrap();
        assert_eq!(
            verdict,
            MutationVerdict::Inconclusive,
            "a contract test that fails on the unmutated baseline must be Inconclusive"
        );

        // Standalone failing script must also be Inconclusive.
        std::fs::write(root.join("check_fail.py"), "import app\nassert app.add(2, 3) == 5\n").unwrap();
        let standalone = contract_mutation_sanity(
            &root,
            "app.py",
            mutation_stub_for("app.py").unwrap(),
            &["python3", "check_fail.py"],
        )
        .unwrap();
        assert_eq!(standalone, MutationVerdict::Inconclusive);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parse_pytest_summary_counts_passed_and_skipped() {
        assert_eq!(parse_pytest_summary_passed("3 skipped in 0.01s\n"), Some(0));
        assert_eq!(parse_pytest_summary_passed("1 passed, 2 skipped in 0.02s\n"), Some(1));
        assert_eq!(parse_pytest_summary_passed("==== 2 passed, 1 warning in 0.12s ====\n"), Some(2));
        assert_eq!(parse_pytest_summary_passed("==== no tests ran in 0.00s ====\n"), Some(0));
        assert_eq!(parse_pytest_summary_passed("2 passed in 65.10s (0:01:05)\n"), Some(2));
        assert_eq!(
            parse_pytest_summary_passed("=== short test summary info ===\nSKIPPED [1] t.py:1: x\n"),
            Some(0)
        );
        assert_eq!(
            parse_pytest_summary_passed("=== short test summary info ===\nPASSED t.py::test_a\n"),
            Some(1)
        );
    }

    #[test]
    fn mutation_gate_verdict_is_cached_per_contract_and_target_hash() {
        let root = std::env::temp_dir().join(format!("tm-mut-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("app.py"), "def add(a, b):\n    return a + b\n").unwrap();
        let counter = root.join("invocations.log");
        let contract = format!(
            "import os, sys\nwith open({:?}, 'a') as f:\n    f.write('1\\n')\nsys.path.insert(0, os.getcwd())\nimport app\nassert app.add(2, 3) == 5\n",
            counter.display().to_string()
        );
        std::fs::write(root.join("tests/test_contract.py"), &contract).unwrap();

        let stub = mutation_stub_for("app.py").unwrap();
        let cmd = ["python3", "tests/test_contract.py"];
        let mut cache: BTreeMap<String, MutationVerdict> = BTreeMap::new();

        // 1. First verify: cache miss, runs baseline + mutated (2 invocations).
        let (v1, hit1) = contract_mutation_sanity_cached(&root, "tests/test_contract.py", "app.py", stub, &cmd, &[], &mut cache).unwrap();
        assert_eq!(v1, MutationVerdict::Caught);
        assert!(!hit1);
        let runs_after_1 = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs_after_1, 2, "expected baseline + mutated execution on cache miss");

        // 2. Second verify with unchanged contract and target: cache hit, 0 extra invocations!
        let (v2, hit2) = contract_mutation_sanity_cached(&root, "tests/test_contract.py", "app.py", stub, &cmd, &[], &mut cache).unwrap();
        assert_eq!(v2, MutationVerdict::Caught);
        assert!(hit2);
        let runs_after_2 = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs_after_2, 2, "cached mutation verdict must not re-run test command");

        // 3. Changing the target invalidates the cache and re-runs.
        std::fs::write(root.join("app.py"), "def add(a, b):\n    return b + a\n").unwrap();
        let (v3, hit3) = contract_mutation_sanity_cached(&root, "tests/test_contract.py", "app.py", stub, &cmd, &[], &mut cache).unwrap();
        assert_eq!(v3, MutationVerdict::Caught);
        assert!(!hit3);
        let runs_after_3 = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs_after_3, 4, "modifying target must invalidate cache and re-run");

        // 4. Changing the contract test invalidates the cache and re-runs.
        let vacuous_contract = format!(
            "with open({:?}, 'a') as f:\n    f.write('1\\n')\nassert True\n",
            counter.display().to_string()
        );
        std::fs::write(root.join("tests/test_contract.py"), vacuous_contract).unwrap();
        let (v4, hit4) = contract_mutation_sanity_cached(&root, "tests/test_contract.py", "app.py", stub, &cmd, &[], &mut cache).unwrap();
        assert_eq!(v4, MutationVerdict::Vacuous);
        assert!(!hit4);
        let runs_after_4 = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs_after_4, 6, "modifying contract must invalidate cache and re-run");

        let _ = std::fs::remove_dir_all(&root);
    }
}
