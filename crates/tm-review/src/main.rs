//! `tm-review`: review a range of commits in any git repository.
//!
//! ```text
//! tm-review <repo> [--base <rev>] [--head <rev>] [--reviewer claude|agy] [--verifier claude|agy]
//!           [--reviewer-model <m>] [--verifier-model <m>] [--keep]
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tm_harness::acp::AcpHarness;
use tm_harness::claude::ClaudeCode;
use tm_harness::{Harness, HarnessEvent, Usage, WorkerTask, WorktreeGate};
use tm_review::context::{self, Scope};
use tm_review::findings::{Finding, Kind, parse_findings, parse_verdict};
use tm_review::history::{History, Judged, Outcome, Record};
use tm_review::{CONTEXT_DIR, ROOT_RULES, brief, git, history, rules};
use tm_verify::{ImportGraph, load_sources};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

const PARALLEL: usize = 4;
const AGENT_TIMEOUT: Duration = Duration::from_secs(600);

struct Options {
    repo: PathBuf,
    base: String,
    head: String,
    reviewer: String,
    verifier: String,
    reviewer_model: Option<String>,
    verifier_model: Option<String>,
    keep: bool,
}

fn options() -> anyhow::Result<Options> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: tm-review <repo> [--base <rev>] [--head <rev>] [--reviewer claude|agy] \
                 [--verifier claude|agy] [--reviewer-model <m>] [--verifier-model <m>] [--keep]";
    let repo = PathBuf::from(args.next().context(usage)?);
    let mut o = Options {
        repo,
        base: "HEAD~1".into(),
        head: "HEAD".into(),
        // Different harnesses by default, so the verifier does not share the reviewer's blind spots.
        reviewer: "claude".into(),
        verifier: "agy".into(),
        reviewer_model: None,
        verifier_model: None,
        keep: false,
    };
    while let Some(flag) = args.next() {
        if flag == "--keep" {
            o.keep = true;
            continue;
        }
        let value = args.next().with_context(|| format!("{flag} needs a value\n{usage}"))?;
        match flag.as_str() {
            "--base" => o.base = value,
            "--head" => o.head = value,
            "--reviewer" => o.reviewer = value,
            "--verifier" => o.verifier = value,
            "--reviewer-model" => o.reviewer_model = Some(value),
            "--verifier-model" => o.verifier_model = Some(value),
            other => bail!("unknown option `{other}`\n{usage}"),
        }
    }
    Ok(o)
}

fn harness(name: &str) -> anyhow::Result<Arc<dyn Harness>> {
    Ok(match name {
        "claude" => Arc::new(ClaudeCode::default()),
        "agy" => Arc::new(AcpHarness::antigravity()?),
        other => bail!("unknown harness `{other}`: use `claude` or `agy`"),
    })
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
    let gate = WorktreeGate::new(&worktree, vec![]).with_readable(format!("{home}/.gemini")).read_only();
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
fn prepare(o: &Options, worktree: &Path, base: &str, head: &str) -> anyhow::Result<Vec<(Scope, String)>> {
    let changes = git::changed_files(&o.repo, base, head)?;
    if changes.is_empty() {
        return Ok(Vec::new());
    }
    let commits = git::log(&o.repo, base, head)?;
    let tracked = git::tracked_files(&o.repo, head)?;
    let graph = ImportGraph::build(&load_sources(worktree)?, "");
    let reviewed: Vec<String> =
        changes.iter().filter(|c| c.status != 'D').map(|c| c.path.clone()).collect();

    let dir = worktree.join(CONTEXT_DIR);
    std::fs::create_dir_all(&dir)?;
    let existing = std::fs::read_to_string(worktree.join(".taskmaster/ARCHITECTURE.md")).ok();
    std::fs::write(dir.join("ARCHITECTURE.md"), context::render_architecture(&tracked, &graph, existing.as_deref()))?;
    std::fs::write(dir.join("CHANGES.md"), context::render_changes(base, head, &changes, &commits))?;
    std::fs::write(dir.join("RULES.md"), rules::render(&rules::rules_for(worktree, &reviewed)))?;
    let past = History::open(&o.repo).records();
    std::fs::write(dir.join("HISTORY.md"), history::render(&past, &reviewed, 40))?;
    std::fs::write(dir.join("DIFF.patch"), git::diff(&o.repo, base, head, &[])?)?;

    let mut scopes = Vec::new();
    for (i, scope) in context::scopes(&changes, &graph).into_iter().enumerate() {
        let name = format!("diff-{}.patch", i + 1);
        std::fs::write(dir.join(&name), git::diff(&o.repo, base, head, &scope.files)?)?;
        scopes.push((scope, name));
    }
    Ok(scopes)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut o = options()?;
    o.repo = std::fs::canonicalize(&o.repo).with_context(|| format!("no such directory: {}", o.repo.display()))?;
    let base = git::resolve(&o.repo, &o.base)?;
    let head = git::resolve(&o.repo, &o.head)?;
    let reviewer = harness(&o.reviewer)?;
    let verifier = harness(&o.verifier)?;

    // The review's own checkout: detached at the reviewed commit, outside the user's tree.
    let home = PathBuf::from(std::env::var("HOME").context("HOME is not set")?);
    let trees = home.join(".taskmaster/review-trees");
    std::fs::create_dir_all(&trees)?;
    let worktree = trees.join(format!("{}-{}", &head[..12], std::process::id()));
    git::add_worktree(&o.repo, &worktree, &head)?;

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
        println!("Nothing to review: no files changed between {} and {}.", &base[..10], &head[..10]);
        return Ok(());
    }
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
    println!("# Review of {}..{}\n", &base[..10], &head[..10]);
    println!(
        "{} confirmed finding(s), {} rejected by the verifier, {dropped} dropped for lack of evidence, {duplicates} duplicate(s) merged.\n",
        confirmed.len(),
        judged.len() - confirmed.len()
    );
    for j in &confirmed {
        let kind = match j.finding.kind {
            Kind::Bug => "bug",
            Kind::Security => "security",
            Kind::Rule => "rule",
        };
        println!("- [{kind}] {}:{} {}", j.finding.file, j.finding.line, j.finding.failure);
    }
    for j in judged.iter().filter(|j| j.outcome == Outcome::Rejected) {
        println!("- rejected: {}:{} {} ({})", j.finding.file, j.finding.line, j.finding.failure, j.why.as_deref().unwrap_or("no reason given"));
    }
    for failure in &failed {
        println!("- REVIEWER FAILED, that part was not reviewed: {failure}");
    }
    if !std::path::Path::new(&o.repo.join(ROOT_RULES)).exists() {
        println!("\nNo rules file. Add project rules at `{ROOT_RULES}`, or a `REVIEW.md` in any directory.");
    }
    println!("\nTokens used: {} (reviewer {}, verifier {}).", usage.total(), reviewer.id(), verifier.id());
    println!("Recorded in {}.", tm_review::HISTORY_FILE);
    Ok(())
}
