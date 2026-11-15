//! `tm-review`: review a range of commits in any git repository.
//!
//! ```text
//! tm-review <repo | url> [--base <rev>] [--head <rev>] [--pr <n>] [--dirty]
//!           [--reviewer <harness>] [--verifier <harness>]
//!           [--reviewer-model <m>] [--verifier-model <m>] [--install-lints] [--keep]
//! ```
//!
//! `url` is a GitHub, GitLab, Bitbucket, Gitea, Forgejo, Codeberg, Azure DevOps or SourceHut
//! repository or request. `--pr` reviews that request on the local `origin`. `--dirty`
//! reviews uncommitted changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tm_harness::{HARNESS_STATE_DIRS, Harness, HarnessEvent, Usage, WorkerTask, WorktreeGate, harness};
use tm_review::context::{self, Scope};
use tm_review::findings::{Finding, Kind, parse_findings, parse_verdict};
use tm_review::history::{History, Judged, Outcome, Record};
use tm_review::{CONTEXT_DIR, ROOT_RULES, brief, forge, git, history, lint, rules};
use tm_verify::{ImportGraph, load_sources};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

const PARALLEL: usize = 4;
const AGENT_TIMEOUT: Duration = Duration::from_secs(600);

struct Options {
    spec: String,
    repo: PathBuf,
    reviewer: String,
    verifier: String,
    reviewer_model: Option<String>,
    verifier_model: Option<String>,
    /// Install toolchain linters (clippy) when the diff needs them.
    install_lints: bool,
    /// Review uncommitted changes instead of a commit range.
    dirty: bool,
    pr: Option<u64>,
    base: Option<String>,
    head: Option<String>,
    keep: bool,
}

fn options() -> anyhow::Result<Options> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: tm-review <repo | url> [--base <rev>] [--head <rev>] [--pr <n>] [--dirty] \
                 [--reviewer <harness>] [--verifier <harness>] [--reviewer-model <m>] \
                 [--verifier-model <m>] [--install-lints] [--keep]\n\
                 url: GitHub, GitLab, Bitbucket, Gitea, Forgejo, Codeberg, Azure DevOps, SourceHut\n\
                 harnesses: claude, agy, grok, codex, opencode";
    let spec = args.next().context(usage)?;
    let mut o = Options {
        spec,
        repo: PathBuf::new(),
        // Different harnesses by default, so the verifier does not share the reviewer's blind spots.
        reviewer: "claude".into(),
        verifier: "agy".into(),
        reviewer_model: None,
        verifier_model: None,
        install_lints: false,
        dirty: false,
        pr: None,
        base: None,
        head: None,
        keep: false,
    };
    while let Some(flag) = args.next() {
        if flag == "--keep" {
            o.keep = true;
            continue;
        }
        if flag == "--install-lints" {
            o.install_lints = true;
            continue;
        }
        if flag == "--dirty" {
            o.dirty = true;
            continue;
        }
        let value = args.next().with_context(|| format!("{flag} needs a value\n{usage}"))?;
        match flag.as_str() {
            "--base" => o.base = Some(value),
            "--head" => o.head = Some(value),
            "--pr" => {
                o.pr = Some(value.parse().with_context(|| format!("`--pr` needs a request number\n{usage}"))?);
            }
            "--reviewer" => o.reviewer = value,
            "--verifier" => o.verifier = value,
            "--reviewer-model" => o.reviewer_model = Some(value),
            "--verifier-model" => o.verifier_model = Some(value),
            other => bail!("unknown option `{other}`\n{usage}"),
        }
    }
    Ok(o)
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
fn changes_of(o: &Options, base: &str, head: &str) -> anyhow::Result<Vec<tm_review::git::Change>> {
    if o.dirty { git::working_tree_changes(&o.repo) } else { git::changed_files(&o.repo, base, head) }
}

fn diff_of(o: &Options, base: &str, head: &str, paths: &[String]) -> anyhow::Result<String> {
    if o.dirty { git::working_tree_diff(&o.repo, paths) } else { git::diff(&o.repo, base, head, paths) }
}

fn prepare(o: &Options, worktree: &Path, base: &str, head: &str) -> anyhow::Result<Vec<(Scope, String)>> {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut o = options()?;
    let opened = forge::open(&o.spec, o.pr, o.base.as_deref(), o.head.as_deref(), o.dirty)?;
    o.repo = std::fs::canonicalize(&opened.repo).with_context(|| format!("no such directory: {}", opened.repo.display()))?;
    o.dirty = opened.dirty;
    let base = opened.base;
    let head = opened.head;
    eprintln!("{}", opened.describe);
    let reviewer = harness(&o.reviewer)?;
    let verifier = harness(&o.verifier)?;

    // The review's own checkout: detached at the reviewed commit, outside the user's tree.
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    let trees = home.join(".taskmaster/review-trees");
    std::fs::create_dir_all(&trees)?;
    let worktree = trees.join(format!("{}-{}", &head[..12], std::process::id()));
    git::add_worktree(&o.repo, &worktree, &head)?;
    if o.dirty {
        git::apply_working_tree(&o.repo, &worktree)?;
    }

    let result = review(&o, &worktree, &base, &head, reviewer, verifier).await;
    if o.keep {
        eprintln!("review checkout kept at {}", worktree.display());
    } else if let Err(e) = git::remove_worktree(&o.repo, &worktree) {
        eprintln!("could not remove the review checkout {}: {e}", worktree.display());
    }
    result
}

async fn review(
    o: &Options,
    worktree: &Path,
    base: &str,
    head: &str,
    reviewer: Arc<dyn Harness>,
    verifier: Arc<dyn Harness>,
) -> anyhow::Result<()> {
    let scopes = prepare(o, worktree, base, head)?;
    if scopes.is_empty() {
        if o.dirty {
            println!("Nothing to review: the working tree is clean.");
        } else {
            println!("Nothing to review: no files changed between {} and {}.", &base[..10], &head[..10]);
        }
        return Ok(());
    }
    let changed: Vec<String> = changes_of(o, base, head)?
        .into_iter()
        .filter(|c| c.status != 'D')
        .map(|c| c.path)
        .collect();
    eprintln!("running mechanical lints");
    let lint = lint::run(worktree, &changed, o.install_lints);
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
    let mut failed: Vec<String> = Vec::new();
    while let Some(done) = running.join_next().await {
        let (name, kind, reply) = done?;
        match reply {
            Ok(reply) => {
                add(reply.usage);
                match parse_findings(&reply.text, kind) {
                    Some(list) => found.extend(list),
                    None => failed.push(format!("{name}: the reviewer returned no findings list")),
                }
            }
            Err(e) => failed.push(format!("{name}: {e:#}")),
        }
    }

    // Keep findings that name a real file and a concrete failure, once each.
    let reported = found.len();
    found.retain(|f| f.is_actionable() && worktree.join(&f.file).is_file());
    let dropped = reported - found.len();
    found.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    found.dedup_by(|a, b| a.file == b.file && a.line == b.line);
    let duplicates = reported - dropped - found.len();
    eprintln!(
        "{reported} finding(s) reported: {dropped} without evidence, {duplicates} duplicate(s), verifying {}",
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
            Err(e) => tm_review::findings::Verdict { real: false, why: Some(format!("the verifier failed: {e:#}")) },
        };
        let outcome = if verdict.real { Outcome::Confirmed } else { Outcome::Rejected };
        judged.push(Judged { finding, outcome, why: verdict.why.filter(|_| !verdict.real) });
    }
    judged.sort_by(|a, b| (&a.finding.file, a.finding.line).cmp(&(&b.finding.file, b.finding.line)));
    // Linter findings are already checked by the tool that produced them.
    for finding in lint.findings {
        judged.push(Judged { finding, outcome: Outcome::Confirmed, why: None });
    }
    judged.sort_by(|a, b| (&a.finding.file, a.finding.line).cmp(&(&b.finding.file, b.finding.line)));

    let record = Record {
        at: now(),
        base: base.into(),
        head: head.into(),
        reviewer: reviewer.id().into(),
        verifier: verifier.id().into(),
        findings: judged.clone(),
    };
    History::open(&o.repo).append(&record)?;

    let confirmed: Vec<&Judged> = judged.iter().filter(|j| j.outcome == Outcome::Confirmed).collect();
    let from_lint = confirmed.iter().filter(|j| j.finding.kind == Kind::Lint).count();
    println!("# Review of {}..{}\n", &base[..10], &head[..10]);
    println!(
        "{} confirmed by the verifier, {from_lint} from lint, {} rejected by the verifier, {dropped} dropped for lack of evidence, {duplicates} duplicate(s) merged.\n",
        confirmed.len() - from_lint,
        judged.len() - confirmed.len()
    );
    for j in &confirmed {
        let kind = match j.finding.kind {
            Kind::Bug => "bug",
            Kind::Security => "security",
            Kind::Rule => "rule",
            Kind::Lint => "lint",
        };
        println!("- [{kind}] {}:{} {}", j.finding.file, j.finding.line, j.finding.failure);
    }
    for j in judged.iter().filter(|j| j.outcome == Outcome::Rejected) {
        println!("- rejected: {}:{} {} ({})", j.finding.file, j.finding.line, j.finding.failure, j.why.as_deref().unwrap_or("no reason given"));
    }
    for failure in &failed {
        println!("- REVIEWER FAILED, that part was not reviewed: {failure}");
    }
    if !lint.notes.is_empty() {
        println!("\n## Lint\n");
        for note in &lint.notes {
            println!("- {note}");
        }
    }
    if !std::path::Path::new(&o.repo.join(ROOT_RULES)).exists() {
        println!("\nNo rules file. Add project rules at `{ROOT_RULES}`, or a `REVIEW.md` in any directory.");
    }
    println!("\nTokens used: {} (reviewer {}, verifier {}).", usage.total(), reviewer.id(), verifier.id());
    println!("Recorded in {}.", tm_review::HISTORY_FILE);
    Ok(())
}
