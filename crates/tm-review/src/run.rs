//! One review, from a target to a report.
//!
//! The binary and the daemon both call [`run`]. It checks the target out, writes the
//! context files, runs the linters, starts one reviewer per scope and a security reviewer,
//! has a second harness try to disprove every finding, and appends the result to the
//! repository's review history.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use tm_harness::{HARNESS_STATE_DIRS, Harness, HarnessEvent, Usage, WorkerTask, WorktreeGate};
use tm_verify::{ImportGraph, load_sources};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::context::{self, Scope};
use crate::findings::{Finding, Kind, parse_findings, parse_verdict};
use crate::history::{History, Judged, Outcome, Record};
use crate::{CONTEXT_DIR, HISTORY_FILE, ROOT_RULES, brief, forge, git, history, lint, rules};

const PARALLEL: usize = 4;
const AGENT_TIMEOUT: Duration = Duration::from_secs(600);

/// What to review and who reviews it.
#[derive(Debug, Clone)]
pub struct Options {
    /// A local path, a repository URL, or a request URL.
    pub spec: String,
    pub reviewer_model: Option<String>,
    pub verifier_model: Option<String>,
    /// Install toolchain linters (clippy) when the diff needs them.
    pub install_lints: bool,
    /// Review uncommitted changes instead of a commit range.
    pub dirty: bool,
    pub pr: Option<u64>,
    pub base: Option<String>,
    pub head: Option<String>,
    /// Leave the review checkout on disk.
    pub keep: bool,
}

impl Options {
    pub fn new(spec: impl Into<String>) -> Self {
        Options {
            spec: spec.into(),
            reviewer_model: None,
            verifier_model: None,
            install_lints: false,
            dirty: false,
            pr: None,
            base: None,
            head: None,
            keep: false,
        }
    }
}

/// The result of one review.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub at: String,
    pub base: String,
    pub head: String,
    pub dirty: bool,
    pub reviewer: String,
    pub verifier: String,
    /// `false` when nothing changed, so no reviewer was started.
    pub reviewed: bool,
    /// Confirmed and rejected findings, lint findings included.
    pub findings: Vec<Judged>,
    /// Reported without a file, a line, or a concrete failure.
    pub dropped: usize,
    pub duplicates: usize,
    /// Scopes whose reviewer did not return a findings list. Those were not reviewed.
    pub failed: Vec<String>,
    pub lint_notes: Vec<String>,
    pub has_rules: bool,
    pub usage: Usage,
}

impl Report {
    pub fn confirmed(&self) -> impl Iterator<Item = &Judged> {
        self.findings.iter().filter(|j| j.outcome == Outcome::Confirmed)
    }

    /// The report as the command line prints it.
    pub fn render(&self) -> String {
        let (base, head) = (short(&self.base), short(&self.head));
        if !self.reviewed {
            return if self.dirty {
                "Nothing to review: the working tree is clean.\n".into()
            } else {
                format!("Nothing to review: no files changed between {base} and {head}.\n")
            };
        }
        let confirmed: Vec<&Judged> = self.confirmed().collect();
        let from_lint = confirmed.iter().filter(|j| j.finding.kind == Kind::Lint).count();
        let mut out = format!("# Review of {base}..{head}\n\n");
        out.push_str(&format!(
            "{} confirmed by the verifier, {from_lint} from lint, {} rejected by the verifier, {} dropped for lack of evidence, {} duplicate(s) merged.\n\n",
            confirmed.len() - from_lint,
            self.findings.len() - confirmed.len(),
            self.dropped,
            self.duplicates,
        ));
        for j in &confirmed {
            let kind = match j.finding.kind {
                Kind::Bug => "bug",
                Kind::Security => "security",
                Kind::Rule => "rule",
                Kind::Lint => "lint",
            };
            out.push_str(&format!("- [{kind}] {}:{} {}\n", j.finding.file, j.finding.line, j.finding.failure));
        }
        for j in self.findings.iter().filter(|j| j.outcome == Outcome::Rejected) {
            out.push_str(&format!(
                "- rejected: {}:{} {} ({})\n",
                j.finding.file,
                j.finding.line,
                j.finding.failure,
                j.why.as_deref().unwrap_or("no reason given")
            ));
        }
        for failure in &self.failed {
            out.push_str(&format!("- REVIEWER FAILED, that part was not reviewed: {failure}\n"));
        }
        if !self.lint_notes.is_empty() {
            out.push_str("\n## Lint\n\n");
            for note in &self.lint_notes {
                out.push_str(&format!("- {note}\n"));
            }
        }
        if !self.has_rules {
            out.push_str(&format!(
                "\nNo rules file. Add project rules at `{ROOT_RULES}`, or a `REVIEW.md` in any directory.\n"
            ));
        }
        out.push_str(&format!(
            "\nTokens used: {} (reviewer {}, verifier {}).\n",
            self.usage.total(),
            self.reviewer,
            self.verifier
        ));
        out.push_str(&format!("Recorded in {HISTORY_FILE}.\n"));
        out
    }
}

fn short(rev: &str) -> &str {
    rev.get(..10).unwrap_or(rev)
}

/// The target once it is opened: the repository on disk and the range under review.
struct Target {
    repo: PathBuf,
    dirty: bool,
    install_lints: bool,
    reviewer_model: Option<String>,
    verifier_model: Option<String>,
}

/// What one agent run produced: everything it said, and what it cost.
struct Reply {
    text: String,
    usage: Usage,
}

/// Run one read-only agent in the review checkout and collect what it says.
async fn ask(
    harness: Arc<dyn Harness>,
    model: Option<String>,
    worktree: PathBuf,
    brief: String,
) -> anyhow::Result<Reply> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut gate = WorktreeGate::new(&worktree, vec![]).read_only();
    for dir in HARNESS_STATE_DIRS {
        gate = gate.with_readable(format!("{home}/{dir}"));
    }
    let task = WorkerTask {
        brief,
        cwd: worktree,
        model,
        dictionary: None,
        timeout: AGENT_TIMEOUT,
        read_only: true,
    };
    let (tx, mut rx) = mpsc::channel(256);
    let collect = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            if let HarnessEvent::Text { text: chunk } = event {
                text.push_str(&chunk);
            }
        }
        text
    });
    let summary = harness.run(task, Arc::new(gate), tx).await?;
    Ok(Reply { text: collect.await?, usage: summary.usage })
}

fn now() -> String {
    let out = std::process::Command::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%SZ"]).output();
    out.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// Write the context files into the checkout. Returns the scopes and their diff file names.
fn changes_of(o: &Target, base: &str, head: &str) -> anyhow::Result<Vec<git::Change>> {
    if o.dirty { git::working_tree_changes(&o.repo) } else { git::changed_files(&o.repo, base, head) }
}

fn diff_of(o: &Target, base: &str, head: &str, paths: &[String]) -> anyhow::Result<String> {
    if o.dirty { git::working_tree_diff(&o.repo, paths) } else { git::diff(&o.repo, base, head, paths) }
}

fn prepare(o: &Target, worktree: &Path, base: &str, head: &str) -> anyhow::Result<Vec<(Scope, String)>> {
    let changes = changes_of(o, base, head)?;
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let commits = if o.dirty { Vec::new() } else { git::log(&o.repo, base, head)? };
    let mut tracked = git::tracked_files(&o.repo, head)?;
    if o.dirty {
        for change in changes.iter().filter(|c| c.status != 'D') {
            if !tracked.contains(&change.path) {
                tracked.push(change.path.clone());
            }
        }
        tracked.sort();
    }
    let graph = ImportGraph::build(&load_sources(worktree)?, "");
    let reviewed: Vec<String> =
        changes.iter().filter(|c| c.status != 'D').map(|c| c.path.clone()).collect();

    let dir = worktree.join(CONTEXT_DIR);
    std::fs::create_dir_all(&dir)?;
    let existing = std::fs::read_to_string(worktree.join(".taskmaster/ARCHITECTURE.md")).ok();
    std::fs::write(dir.join("ARCHITECTURE.md"), context::render_architecture(&tracked, &graph, existing.as_deref()))?;
    let changes_doc = if o.dirty {
        context::render_working_tree(head, &changes)
    } else {
        context::render_changes(base, head, &changes, &commits)
    };
    std::fs::write(dir.join("CHANGES.md"), changes_doc)?;
    std::fs::write(dir.join("RULES.md"), rules::render(&rules::rules_for(worktree, &reviewed)))?;
    let past = History::open(&o.repo).records();
    std::fs::write(dir.join("HISTORY.md"), history::render(&past, &reviewed, 40))?;
    std::fs::write(dir.join("DIFF.patch"), diff_of(o, base, head, &[])?)?;
    // Written again after the linters run. Present now so a reviewer always has the file.
    std::fs::write(dir.join("LINT.md"), "# Mechanical lint\n\nNot run yet.\n")?;

    let mut scopes = Vec::new();
    for (i, scope) in context::scopes(&changes, &graph).into_iter().enumerate() {
        let name = format!("diff-{}.patch", i + 1);
        std::fs::write(dir.join(&name), diff_of(o, base, head, &scope.files)?)?;
        scopes.push((scope, name));
    }
    Ok(scopes)
}

/// Review `options.spec` and record the result in that repository's history.
pub async fn run(options: &Options, reviewer: Arc<dyn Harness>, verifier: Arc<dyn Harness>) -> anyhow::Result<Report> {
    let opened = forge::open(&options.spec, options.pr, options.base.as_deref(), options.head.as_deref(), options.dirty)?;
    let repo = std::fs::canonicalize(&opened.repo)
        .with_context(|| format!("no such directory: {}", opened.repo.display()))?;
    let target = Target {
        repo,
        dirty: opened.dirty,
        install_lints: options.install_lints,
        reviewer_model: options.reviewer_model.clone(),
        verifier_model: options.verifier_model.clone(),
    };
    let base = opened.base;
    let head = opened.head;
    eprintln!("{}", opened.describe);

    // The review's own checkout: detached at the reviewed commit, outside the user's tree.
    static N: AtomicU64 = AtomicU64::new(0);
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    let trees = home.join(".taskmaster/review-trees");
    std::fs::create_dir_all(&trees)?;
    let worktree = trees.join(format!(
        "{}-{}-{}",
        head.get(..12).unwrap_or(&head),
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    git::add_worktree(&target.repo, &worktree, &head)?;
    let result = async {
        if target.dirty {
            git::apply_working_tree(&target.repo, &worktree)?;
        }
        review(&target, &worktree, &base, &head, reviewer, verifier).await
    }
    .await;
    if options.keep {
        eprintln!("review checkout kept at {}", worktree.display());
    } else if let Err(e) = git::remove_worktree(&target.repo, &worktree) {
        eprintln!("could not remove the review checkout {}: {e}", worktree.display());
    }
    result
}

async fn review(
    o: &Target,
    worktree: &Path,
    base: &str,
    head: &str,
    reviewer: Arc<dyn Harness>,
    verifier: Arc<dyn Harness>,
) -> anyhow::Result<Report> {
    let mut report = Report {
        at: now(),
        base: base.into(),
        head: head.into(),
        dirty: o.dirty,
        reviewer: reviewer.id().into(),
        verifier: verifier.id().into(),
        reviewed: false,
        findings: Vec::new(),
        dropped: 0,
        duplicates: 0,
        failed: Vec::new(),
        lint_notes: Vec::new(),
        has_rules: o.repo.join(ROOT_RULES).exists(),
        usage: Usage::default(),
    };
    let scopes = prepare(o, worktree, base, head)?;
    if scopes.is_empty() {
        return Ok(report);
    }
    report.reviewed = true;
    let changed: Vec<String> = changes_of(o, base, head)?
        .into_iter()
        .filter(|c| c.status != 'D')
        .map(|c| c.path)
        .collect();
    eprintln!("running mechanical lints");
    let lint = {
        let (tree, install) = (worktree.to_path_buf(), o.install_lints);
        tokio::task::spawn_blocking(move || lint::run(&tree, &changed, install)).await.context("the linters panicked")?
    };
    std::fs::write(worktree.join(CONTEXT_DIR).join("LINT.md"), lint.markdown())?;
    let all_files: Vec<String> = scopes.iter().flat_map(|(s, _)| s.files.clone()).collect();
    eprintln!("reviewing {} file(s) with {} reviewer(s) plus a security reviewer", all_files.len(), scopes.len());

    // One reviewer per scope, plus one that looks only for security problems.
    let mut briefs: Vec<(String, Kind, String)> = scopes
        .iter()
        .map(|(scope, diff)| (scope.name.clone(), Kind::Bug, brief::review(scope, diff)))
        .collect();
    briefs.push(("security".into(), Kind::Security, brief::security(&all_files)));

    let limit = Arc::new(Semaphore::new(PARALLEL));
    let mut usage = Usage::default();
    let mut add = |u: Usage| {
        usage.input += u.input;
        usage.output += u.output;
        usage.cache_read += u.cache_read;
        usage.cache_write += u.cache_write;
    };

    let mut running = JoinSet::new();
    for (name, kind, text) in briefs {
        let (h, model, tree, limit) = (reviewer.clone(), o.reviewer_model.clone(), worktree.to_path_buf(), limit.clone());
        running.spawn(async move {
            let _permit = limit.acquire_owned().await;
            (name, kind, ask(h, model, tree, text).await)
        });
    }
    let mut found: Vec<Finding> = Vec::new();
    while let Some(done) = running.join_next().await {
        let (name, kind, reply) = done?;
        match reply {
            Ok(reply) => {
                add(reply.usage);
                match parse_findings(&reply.text, kind) {
                    Some(list) => found.extend(list),
                    None => report.failed.push(format!("{name}: the reviewer returned no findings list")),
                }
            }
            Err(e) => report.failed.push(format!("{name}: {e:#}")),
        }
    }

    // Keep findings that name a real file and a concrete failure, once each.
    let reported = found.len();
    found.retain(|f| f.is_actionable() && worktree.join(&f.file).is_file());
    report.dropped = reported - found.len();
    found.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    found.dedup_by(|a, b| a.file == b.file && a.line == b.line);
    report.duplicates = reported - report.dropped - found.len();
    eprintln!(
        "{reported} finding(s) reported: {} without evidence, {} duplicate(s), verifying {}",
        report.dropped,
        report.duplicates,
        found.len()
    );

    // Every remaining finding must survive an attempt to disprove it.
    let mut running = JoinSet::new();
    for finding in found {
        let (h, model, tree, limit) = (verifier.clone(), o.verifier_model.clone(), worktree.to_path_buf(), limit.clone());
        running.spawn(async move {
            let _permit = limit.acquire_owned().await;
            let reply = ask(h, model, tree, brief::verify(&finding)).await;
            (finding, reply)
        });
    }
    let mut judged: Vec<Judged> = Vec::new();
    while let Some(done) = running.join_next().await {
        let (finding, reply) = done?;
        let verdict = match reply {
            Ok(reply) => {
                add(reply.usage);
                parse_verdict(&reply.text)
            }
            // A verifier that fails has not confirmed anything.
            Err(e) => crate::findings::Verdict { real: false, why: Some(format!("the verifier failed: {e:#}")) },
        };
        let outcome = if verdict.real { Outcome::Confirmed } else { Outcome::Rejected };
        judged.push(Judged { finding, outcome, why: verdict.why.filter(|_| !verdict.real) });
    }
    // Linter findings are already checked by the tool that produced them.
    for finding in lint.findings {
        judged.push(Judged { finding, outcome: Outcome::Confirmed, why: None });
    }
    judged.sort_by(|a, b| (&a.finding.file, a.finding.line).cmp(&(&b.finding.file, b.finding.line)));

    let record = Record {
        at: report.at.clone(),
        base: base.into(),
        head: head.into(),
        reviewer: report.reviewer.clone(),
        verifier: report.verifier.clone(),
        findings: judged.clone(),
    };
    History::open(&o.repo).append(&record)?;

    report.findings = judged;
    report.lint_notes = lint.notes;
    report.usage = usage;
    Ok(report)
}
