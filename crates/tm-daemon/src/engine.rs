//! Drive a run until it finishes, waits for approval, or is cancelled.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tm_core::docs::{
    ARCHITECTURE_FILE, CHANGES_FILE, render_architecture, render_changes, review_brief, security_brief, verify_brief,
};
use tm_core::{
    ChangeEntry, ChangeKind, Citation, Command, DictTier, DraftEntry, EntryKind, Event, Finding, Flag,
    FlagResolution, PageId, PagePlan, Plan, ReviewScope, Run, RunPhase, Tier, WorkerOutcome, check_plan,
    page_is_fresh, valid_key,
};
use tm_dictionary::{Catalog, Grant, LocalStore, McpState};
use tm_harness::{
    DictionaryAccess, HARNESS_STATE_DIRS, Harness, HarnessEvent, WorkerTask, WorktreeGate,
};
use tm_verify::{ImportGraph, Owners, load_sources, outside_owned};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::git;
use crate::records;
use crate::reply::{self, Resolution};
use crate::resume;
use crate::time::rfc3339_now;
use crate::{Config, Harnesses, Outcome, run_directory};

struct Product {
    text: String,
    ok: bool,
    worktree: PathBuf,
}

struct AgentRequest {
    harness: Arc<dyn Harness>,
    brief: String,
    cwd: PathBuf,
    model: Option<String>,
    dictionary: Option<DictionaryAccess>,
    timeout: Duration,
    read_only: bool,
}

enum Step {
    Continue,
    Stop(Outcome),
}

#[derive(Serialize, Deserialize)]
struct Meta {
    parallel: usize,
}

struct Driver {
    config: Config,
    project: PathBuf,
    dir: PathBuf,
    run: Run,
    events: Vec<Event>,
    changes: Vec<ChangeEntry>,
    pending: VecDeque<Command>,
    running: Vec<Command>,
    jobs: JoinSet<(PageId, anyhow::Result<Product>)>,
    harnesses: Harnesses,
    url: String,
    mcp: Arc<McpState>,
    dict: PathBuf,
    commons: PathBuf,
    tokens: HashMap<String, String>,
    reviewer_token: String,
    verifier_token: String,
    master_token: String,
    trees: Vec<PathBuf>,
    parallel: usize,
}

impl Drop for Driver {
    fn drop(&mut self) {
        for path in std::mem::take(&mut self.trees) {
            let _ = git::remove_worktree(&self.project, &path);
            let _ = fs::remove_dir_all(&path);
        }
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub async fn drive(config: Config, harnesses: Harnesses) -> anyhow::Result<Outcome> {
    let project = config
        .project
        .canonicalize()
        .with_context(|| format!("cannot open {}", config.project.display()))?;
    if !git::is_repo(&project) {
        bail!("{} is not a git repository", project.display());
    }
    git::require_identity(&project)?;
    git::ensure_exclude(&project)?;

    let dir = run_directory(&project, &config.run_id);
    fs::create_dir_all(&dir)?;
    let events: Vec<Event> = load_lines(&dir.join("events.jsonl"))?;
    let parallel = load_parallel(&dir, !events.is_empty(), config.parallel)?;
    let run = Run::replay(parallel, events.clone());
    let changes: Vec<ChangeEntry> = load_lines(&dir.join("changes.jsonl"))?;
    let inflight = load_inflight(&dir.join("inflight.json"))?;
    let pending = match inflight {
        Some(cmds) => resume::reconcile(&run, &events, &changes, cmds)?,
        None if !events.is_empty() => resume::reconcile(&run, &events, &changes, vec![])?,
        None => Vec::new(),
    };

    let dict = config.dictionary.clone().unwrap_or_else(|| project.join(".taskmaster/dictionary"));
    let commons = config.commons.clone().unwrap_or_else(|| project.join(".taskmaster/commons"));
    let mcp = McpState::with_commons(LocalStore::open(&dict)?, LocalStore::open(&commons)?);
    let listener = TcpListener::bind("127.0.0.1:0").await.context("cannot bind the dictionary server")?;
    let addr = listener.local_addr().context("dictionary server has no address")?;
    let url = format!("http://{addr}/mcp");
    let served = mcp.clone();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, tm_dictionary::router(served)).await;
    });
    let _server = AbortOnDrop(server);

    let reviewer_token = fresh_token("reviewer");
    let verifier_token = fresh_token("verifier");
    let master_token = fresh_token("master");
    let run_id = config.run_id.clone();
    mcp.grant(&reviewer_token, Grant { run: run_id.clone(), page: "reviewer".into() });
    mcp.grant(&verifier_token, Grant { run: run_id.clone(), page: "verifier".into() });
    mcp.grant(&master_token, Grant { run: run_id, page: "master".into() });

    let mut driver = Driver {
        config,
        project,
        dir,
        run,
        events,
        changes,
        pending: VecDeque::from(pending),
        running: Vec::new(),
        jobs: JoinSet::new(),
        harnesses,
        url,
        mcp,
        dict,
        commons,
        tokens: HashMap::new(),
        reviewer_token,
        verifier_token,
        master_token,
        trees: Vec::new(),
        parallel,
    };
    driver.grant_pages();
    driver.r#loop().await
}

impl Driver {
    async fn r#loop(&mut self) -> anyhow::Result<Outcome> {
        loop {
            if self.should_cancel() {
                if !self.running.is_empty() {
                    self.join_one().await?;
                    continue;
                }
                self.pending.clear();
                self.push_event(Event::Cancelled)?;
                continue;
            }
            let room = self.running.len() < self.parallel;
            let next_spawn = matches!(self.pending.front(), Some(Command::SpawnWorker { .. }));
            if !self.running.is_empty() && (!room || !next_spawn) {
                self.join_one().await?;
                continue;
            }
            if next_spawn {
                self.start_spawn()?;
                continue;
            }
            if let Some(cmd) = self.pending.pop_front() {
                if let Step::Stop(outcome) = self.dispatch(cmd).await? {
                    return Ok(outcome);
                }
                continue;
            }
            if let Step::Stop(outcome) = self.idle().await? {
                return Ok(outcome);
            }
        }
    }

    fn should_cancel(&self) -> bool {
        *self.config.cancel.borrow() && !matches!(self.run.phase, RunPhase::Done | RunPhase::Failed)
    }

    async fn idle(&mut self) -> anyhow::Result<Step> {
        match self.run.phase {
            RunPhase::Planning => {
                let plan = if let Some(plan) = self.config.plan.clone() {
                    self.config.plan = None;
                    plan
                } else if self.config.prompt.as_ref().is_some_and(|prompt| !prompt.trim().is_empty()) {
                    let prompt = self.config.prompt.clone().unwrap_or_default();
                    let plan = self.draft_plan(&prompt).await?;
                    self.config.prompt = None;
                    plan
                } else {
                    bail!("no plan for this run; pass --plan <file> or --prompt <text>");
                };
                self.push_event(Event::PlanReady(plan))?;
                self.grant_pages();
                Ok(Step::Continue)
            }
            RunPhase::AwaitingApproval => {
                self.pending.push_back(Command::RequestApproval);
                Ok(Step::Continue)
            }
            RunPhase::Done | RunPhase::Failed => {
                let ok = self.run.phase == RunPhase::Done;
                self.write_status(ok)?;
                self.clear_inflight()?;
                println!("taskmaster: run {} {}", self.config.run_id, if ok { "finished" } else { "failed" });
                Ok(Step::Stop(Outcome::Finished { ok }))
            }
            _ => bail!("run is {:?} with nothing left to do", self.run.phase),
        }
    }

    /// Ask the master harness for a plan. One rejection is sent back with the reasons.
    /// Dictionary entries are published before the plan is recorded.
    async fn draft_plan(&mut self, prompt: &str) -> anyhow::Result<Plan> {
        let mut errors = Vec::new();
        for attempt in 1..=2 {
            let brief = self.planner_brief(prompt, &errors)?;
            let cwd = self.planner_dir()?;
            let scratch = cwd != self.project;
            let token = self.master_token.clone();
            let request = self.read_request(
                &cwd,
                brief,
                self.harnesses.master.clone(),
                self.config.models.master.clone(),
                &token,
            )?;
            let asked = ask(request).await;
            if scratch {
                self.drop_worktree(&cwd);
            }
            let (text, ok) = asked?;
            if !ok {
                errors = vec!["the planner did not finish".into()];
            } else {
                match reply::parse_plan(&text) {
                    Ok((plan, drafts)) => {
                        let existing = self.existing_content(&plan, &drafts)?;
                        match check_plan(&plan, &drafts, &existing) {
                            Ok(()) => {
                                let published = self.publish_drafts(&drafts, &existing)?;
                                atomic_write(&self.dir.join("plan.json"), &serde_json::to_vec_pretty(&plan)?)?;
                                eprintln!(
                                    "taskmaster: planned {} pages and {} new dictionary entries",
                                    plan.pages.len(),
                                    published
                                );
                                return Ok(plan);
                            }
                            Err(found) => errors = found,
                        }
                    }
                    Err(err) => errors = vec![format!("{err:#}")],
                }
            }
            if attempt == 1 {
                eprintln!("taskmaster: plan rejected, asking once more");
            }
        }
        bail!("plan is not usable:\n{}", errors.join("\n"))
    }

    fn planner_brief(&self, prompt: &str, errors: &[String]) -> anyhow::Result<String> {
        let mut brief = format!(
            "Plan this product and lock its contracts.\n\n\
             Request:\n{prompt}\n\n\
             Search the Dictionary before inventing a key. Cite an existing entry instead of rewriting it. \
             If you include an existing key, its content must match exactly.\n\n\
             Dictionary entries already stored:\n"
        );
        let found = self.catalog()?.search("")?;
        if found.is_empty() {
            brief.push_str("- none\n");
        }
        for entry in &found {
            let place = match entry.tier {
                DictTier::Commons => "commons",
                DictTier::Local => "local",
            };
            brief.push_str(&format!(
                "- `{key}` ({place}, {kind}): {summary}\n{content}\n",
                key = entry.key,
                kind = kind_name(entry.kind),
                summary = entry.summary,
                content = entry.content,
            ));
        }
        if found.len() == 20 {
            brief.push_str("Further entries may be stored. Search the Dictionary before adding a key.\n");
        }
        brief.push_str(
            "\nReply with one JSON object and do not edit files. \
             `contracts` are shared source files. Each page owns one glob. \
             `dictionary` holds new entries those pages cite. \
             `kind` is schema, routes, component_api, pattern, or hint.\n\
             {\"contracts\":[{\"path\":\"app/types.ts\",\"content\":\"...\"}],\
             \"pages\":[{\"id\":\"home\",\"route\":\"/home\",\"owned_glob\":\"app/home/**\",\"cites\":[\"theme\"],\"brief\":\"...\"}],\
             \"estimate\":{\"tokens\":0,\"usd\":0},\
             \"dictionary\":[{\"key\":\"theme\",\"kind\":\"hint\",\"summary\":\"...\",\"content\":\"...\",\"stack\":\"next\"}]}\n\
             \nRules: at least one page; page ids use letters, digits, `_` and `-`; \
             owned_glob and contract paths are relative and do not contain `..`; \
             every cited key is an existing entry or one of the dictionary items; \
             briefs and contents are non-empty.\n",
        );
        if !errors.is_empty() {
            brief.push_str("\nThe last plan was rejected:\n");
            for error in errors {
                brief.push_str(&format!("- {error}\n"));
            }
            brief.push_str("Reply with a corrected JSON object.\n");
        }
        Ok(brief)
    }

    /// A detached worktree when the project has a commit, otherwise the project itself.
    /// The planner is read-only, so the project directory is only used before the first commit.
    fn planner_dir(&mut self) -> anyhow::Result<PathBuf> {
        if git::has_head(&self.project) {
            self.fresh_worktree("plan")
        } else {
            Ok(self.project.clone())
        }
    }

    fn existing_content(&self, plan: &Plan, drafts: &[DraftEntry]) -> anyhow::Result<BTreeMap<String, String>> {
        let catalog = self.catalog()?;
        let mut keys = BTreeSet::new();
        for draft in drafts {
            if valid_key(&draft.key) {
                keys.insert(draft.key.clone());
            }
        }
        for page in &plan.pages {
            for cite in &page.cites {
                if valid_key(cite) {
                    keys.insert(cite.clone());
                }
            }
        }
        let mut existing = BTreeMap::new();
        for key in keys {
            if let Some(entry) = catalog.resolve(&key)? {
                existing.insert(key, entry.content);
            }
        }
        Ok(existing)
    }

    /// Publish drafts that are not already stored with the same content.
    fn publish_drafts(&self, drafts: &[DraftEntry], existing: &BTreeMap<String, String>) -> anyhow::Result<usize> {
        let store = LocalStore::open(&self.dict)?;
        let mut published = 0;
        for draft in drafts {
            if existing.get(&draft.key).is_some_and(|have| have == &draft.content) {
                continue;
            }
            store.publish(&draft.key, draft.kind, &draft.summary, &draft.content, &draft.stack)?;
            published += 1;
        }
        Ok(published)
    }

    async fn dispatch(&mut self, cmd: Command) -> anyhow::Result<Step> {
        match cmd {
            Command::RequestApproval => self.approve(),
            Command::LockContracts => {
                self.lock()?;
                self.persist()?;
                Ok(Step::Continue)
            }
            Command::SpawnWorker { .. } => bail!("a worker spawn was queued behind another command"),
            Command::ArbitrateFlag(flag) => {
                let event = self.arbitrate(flag).await?;
                self.push_event(event)?;
                Ok(Step::Continue)
            }
            Command::RunReview { scopes, security } => {
                let findings = self.review(scopes, security).await?;
                self.push_event(Event::ReviewFinished { findings })?;
                Ok(Step::Continue)
            }
            Command::VerifyFindings { findings } => {
                let confirmed = self.verify_findings(findings).await?;
                self.note_findings(&confirmed)?;
                self.push_event(Event::FindingsVerified { confirmed })?;
                Ok(Step::Continue)
            }
            Command::RunVerify => {
                let (ok, failing) = self.verify_project()?;
                self.push_event(Event::VerifyFinished { ok, failing })?;
                Ok(Step::Continue)
            }
            Command::Finish { ok } => {
                self.settle_commons(ok)?;
                self.write_status(ok)?;
                self.persist()?;
                Ok(Step::Continue)
            }
        }
    }

    fn approve(&mut self) -> anyhow::Result<Step> {
        if !self.config.yes {
            self.pending.push_front(Command::RequestApproval);
            self.persist()?;
            let summary = self.plan_summary();
            println!("{summary}");
            return Ok(Step::Stop(Outcome::NeedsApproval { summary }));
        }
        if self.config.plan.is_some() {
            eprintln!("taskmaster: approving the plan already recorded for this run");
        }
        eprintln!("taskmaster: plan approved");
        self.push_event(Event::PlanApproved)?;
        Ok(Step::Continue)
    }

    fn start_spawn(&mut self) -> anyhow::Result<()> {
        let Some(Command::SpawnWorker { page, tier, errors }) = self.pending.pop_front() else {
            bail!("expected a worker spawn");
        };
        if self.is_stale(&page) {
            let _ = fs::remove_file(self.marker(&page));
            eprintln!("taskmaster: {page} was built against an amended entry; discarding it");
            let outcome = WorkerOutcome::Passed;
            self.push_event(Event::WorkerFinished { page, outcome })?;
            return Ok(());
        }
        if self.marker(&page).is_file() {
            let _ = fs::remove_file(self.marker(&page));
            eprintln!("taskmaster: {page} was already merged; recording it");
            self.record_citations(&page)?;
            self.push_event(Event::WorkerFinished { page, outcome: WorkerOutcome::Passed })?;
            return Ok(());
        }
        if errors.is_none() && self.citations_fresh(&page)? {
            eprintln!("taskmaster: {page} citations are unchanged");
            self.push_event(Event::PageFresh { page })?;
            return Ok(());
        }
        let created = self.fresh_worktree(&page);
        let worktree = match created {
            Ok(path) => path,
            Err(err) => {
                self.pending.push_front(Command::SpawnWorker { page, tier, errors });
                let _ = self.persist();
                return Err(err);
            }
        };
        let built = {
            let text = errors.as_deref();
            self.worker_request(&page, tier, text, worktree.clone())
        };
        let request = match built {
            Ok(request) => request,
            Err(err) => {
                self.pending.push_front(Command::SpawnWorker { page, tier, errors });
                let _ = self.persist();
                return Err(err);
            }
        };
        eprintln!("taskmaster: building {page} ({})", tier_name(tier));
        self.running.push(Command::SpawnWorker { page: page.clone(), tier, errors });
        self.jobs.spawn(async move {
            let result = ask(request).await.map(|(text, ok)| Product { text, ok, worktree });
            (page, result)
        });
        self.persist()?;
        Ok(())
    }

    async fn join_one(&mut self) -> anyhow::Result<()> {
        let (page, result) = self
            .jobs
            .join_next()
            .await
            .context("a worker disappeared")?
            .context("a worker task panicked")?;
        let cmd = self.take_running(&page);
        let product = match result {
            Ok(product) => product,
            Err(err) => {
                if let Some(cmd) = cmd {
                    self.running.push(cmd);
                }
                self.persist()?;
                return Err(err);
            }
        };
        if self.is_stale(&page) {
            self.drop_worktree(&product.worktree);
            let _ = fs::remove_file(self.marker(&page));
            eprintln!("taskmaster: {page} was built against an amended entry; discarding it");
            self.push_event(Event::WorkerFinished { page, outcome: WorkerOutcome::Passed })?;
            return Ok(());
        }
        let (tier, errors) = match &cmd {
            Some(Command::SpawnWorker { tier, errors, .. }) => (*tier, errors.clone()),
            _ => (Tier::Worker, None),
        };
        let text = product.text.clone();
        let outcome = match self.judge(&page, tier, &errors, product) {
            Ok(outcome) => outcome,
            Err(err) => {
                if let Some(cmd) = cmd {
                    self.running.push(cmd);
                }
                self.persist()?;
                return Err(err);
            }
        };
        match &outcome {
            WorkerOutcome::Passed => eprintln!("taskmaster: {page} passed"),
            WorkerOutcome::CheckFailed { .. } => eprintln!("taskmaster: {page} failed its checks"),
            WorkerOutcome::Flagged(flag) => eprintln!("taskmaster: {page} flagged {}", flag.key),
        }
        match &outcome {
            WorkerOutcome::CheckFailed { errors: failure } => self.note_mistake(tier, failure)?,
            WorkerOutcome::Passed => {
                if let Some(failure) = &errors {
                    self.learn_fix(tier, failure, &text)?;
                }
                self.record_citations(&page)?;
            }
            WorkerOutcome::Flagged(_) => {}
        }
        self.push_event(Event::WorkerFinished { page: page.clone(), outcome })?;
        let _ = fs::remove_file(self.marker(&page));
        Ok(())
    }

    fn judge(&mut self, page: &str, tier: Tier, errors: &Option<String>, product: Product) -> anyhow::Result<WorkerOutcome> {
        let outcome = self.judge_inner(page, tier, errors, &product);
        self.drop_worktree(&product.worktree);
        outcome
    }

    fn judge_inner(
        &mut self,
        page: &str,
        tier: Tier,
        errors: &Option<String>,
        product: &Product,
    ) -> anyhow::Result<WorkerOutcome> {
        if let Some((key, issue)) = reply::parse_flag(&product.text) {
            return Ok(WorkerOutcome::Flagged(Flag { page: page.to_string(), key, issue }));
        }
        let changes = git::worktree_changes(&product.worktree)?;
        for change in &changes {
            if !git::safe_rel(&change.path) {
                return Ok(failed(format!("refusing to merge `{}`", change.path)));
            }
            if change.status != 'D' {
                let src = product.worktree.join(&change.path);
                let meta = fs::symlink_metadata(&src).with_context(|| format!("cannot read {}", change.path))?;
                if meta.file_type().is_symlink() {
                    return Ok(failed(format!("refusing to merge symlink `{}`", change.path)));
                }
            }
        }
        let plan = self.plan()?.clone();
        let owners = Owners::from_plan(&plan)?;
        let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        let outside = outside_owned(&owners, page, &paths);
        if !outside.is_empty() {
            let glob = self.page(page)?.owned_glob.clone();
            return Ok(failed(format!(
                "these files are outside `{glob}` and were not merged: {}",
                outside.join(", ")
            )));
        }
        if changes.is_empty() {
            let why = if product.ok {
                "worker produced no changes under its owned files"
            } else {
                "harness failed and produced no changes"
            };
            return Ok(failed(why));
        }
        let mut files = Vec::new();
        for change in &changes {
            let dest = self.project.join(&change.path);
            if change.status == 'D' {
                if dest.is_file() {
                    fs::remove_file(&dest)?;
                }
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(product.worktree.join(&change.path), &dest)
                    .with_context(|| format!("cannot copy {}", change.path))?;
            }
            files.push(change.path.clone());
        }
        let repaired = errors.is_some();
        let kind = if repaired { ChangeKind::PageRepaired } else { ChangeKind::PageBuilt };
        let summary = format!("{} page {page}", if repaired { "repaired" } else { "built" });
        let message = format!("taskmaster: {summary}");
        let model = self.byline(self.harness_for(tier).id(), &self.model_for(tier));
        let entry = ChangeEntry {
            at: rfc3339_now(),
            kind,
            page: Some(page.to_string()),
            commit: None,
            model: Some(model),
            files: files.clone(),
            keys: self.page(page)?.cites.clone(),
            summary,
        };
        let Some(hash) = self.commit_entries(vec![entry], files, &message)? else {
            return Ok(failed("worker produced no changes under its owned files"));
        };
        fs::create_dir_all(self.dir.join("merged"))?;
        fs::write(self.marker(page), hash)?;
        Ok(WorkerOutcome::Passed)
    }

    fn lock(&mut self) -> anyhow::Result<()> {
        let plan = self.plan()?.clone();
        for contract in &plan.contracts {
            if !git::safe_rel(&contract.path) {
                bail!("contract path `{}` leaves the project", contract.path);
            }
            let dest = self.project.join(&contract.path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, &contract.content)?;
        }
        let files: Vec<String> = plan.contracts.iter().map(|c| c.path.clone()).collect();
        let mut keys: Vec<String> = plan.pages.iter().flat_map(|p| p.cites.clone()).collect();
        keys.sort();
        keys.dedup();
        let entry = ChangeEntry {
            at: rfc3339_now(),
            kind: ChangeKind::ContractsLocked,
            page: None,
            commit: None,
            model: None,
            files: files.clone(),
            keys,
            summary: format!("locked {} contract files for {} pages", plan.contracts.len(), plan.pages.len()),
        };
        self.commit_entries(vec![entry], files, "taskmaster: lock contracts")?;
        eprintln!("taskmaster: locked contracts");
        Ok(())
    }

    async fn arbitrate(&mut self, flag: Flag) -> anyhow::Result<Event> {
        eprintln!("taskmaster: arbitrating {} on {}", flag.key, flag.page);
        let entry = LocalStore::open(&self.dict)?.get(&flag.key)?;
        let current = match &entry {
            Some(entry) => format!("{} ({})\n{}", entry.summary, kind_name(entry.kind), entry.content),
            None => "This key is not in the dictionary.".into(),
        };
        let brief = format!(
            "A worker on page `{page}` objected to Dictionary entry `{key}`.\n\
             Issue: {issue}\n\n\
             Current entry:\n{current}\n\n\
             If the entry is right, reply {{\"resolution\":\"conform\"}}.\n\
             If it should change, reply {{\"resolution\":\"amended\",\"kind\":\"hint\",\"summary\":\"...\",\"content\":\"...\",\"stack\":\"...\"}}.\n\
             Reply with only that JSON. Do not edit files.",
            page = flag.page,
            key = flag.key,
            issue = flag.issue,
        );
        let worktree = self.fresh_worktree("arbitrate")?;
        let request = self.read_request(
            &worktree,
            brief,
            self.harnesses.master.clone(),
            self.config.models.master.clone(),
            &self.master_token.clone(),
        )?;
        let (text, _) = ask(request).await?;
        self.drop_worktree(&worktree);
        let resolution = match reply::parse_resolution(&text)? {
            Resolution::Conform => {
                eprintln!("taskmaster: {} stands; {} must conform", flag.key, flag.page);
                FlagResolution::Conform
            }
            Resolution::Amend(amend) => {
                let hash = LocalStore::open(&self.dict)?.publish(
                    &flag.key,
                    amend.kind,
                    &amend.summary,
                    &amend.content,
                    &amend.stack,
                )?;
                eprintln!("taskmaster: amended {} ({hash})", flag.key);
                let entry = ChangeEntry {
                    at: rfc3339_now(),
                    kind: ChangeKind::EntryAmended,
                    page: Some(flag.page.clone()),
                    commit: None,
                    model: Some(self.byline(self.harnesses.master.id(), &self.config.models.master)),
                    files: vec![],
                    keys: vec![flag.key.clone()],
                    summary: amend.summary,
                };
                self.commit_entries(vec![entry], vec![], &format!("taskmaster: amend {}", flag.key))?;
                FlagResolution::Amended
            }
        };
        Ok(Event::FlagResolved { page: flag.page, key: flag.key, resolution })
    }

    async fn review(&mut self, scopes: Vec<ReviewScope>, security: Vec<PageId>) -> anyhow::Result<Vec<Finding>> {
        eprintln!("taskmaster: reviewing {} connections", scopes.len());
        let worktree = self.fresh_worktree("review")?;
        let sem = Arc::new(Semaphore::new(self.parallel.max(1)));
        let mut set = JoinSet::new();
        for scope in scopes {
            let key = scope.key.clone();
            let brief = review_brief(&scope);
            let request = self.read_request(
                &worktree,
                brief,
                self.harnesses.reviewer.clone(),
                self.config.models.reviewer.clone(),
                &self.reviewer_token.clone(),
            )?;
            let permit = sem.clone();
            set.spawn(async move {
                let _permit = permit.acquire_owned().await.context("review stopped")?;
                let (text, _) = ask(request).await?;
                reply::parse_review(&text, Some(key))
            });
        }
        if !security.is_empty() {
            let brief = security_brief(&security);
            let request = self.read_request(
                &worktree,
                brief,
                self.harnesses.reviewer.clone(),
                self.config.models.reviewer.clone(),
                &self.reviewer_token.clone(),
            )?;
            let permit = sem.clone();
            set.spawn(async move {
                let _permit = permit.acquire_owned().await.context("review stopped")?;
                let (text, _) = ask(request).await?;
                reply::parse_review(&text, None)
            });
        }
        let mut findings = Vec::new();
        while let Some(joined) = set.join_next().await {
            findings.extend(joined.context("review task panicked")??);
        }
        self.drop_worktree(&worktree);
        Ok(findings)
    }

    async fn verify_findings(&mut self, findings: Vec<Finding>) -> anyhow::Result<Vec<Finding>> {
        eprintln!("taskmaster: verifying {} findings", findings.len());
        let worktree = self.fresh_worktree("verify")?;
        let sem = Arc::new(Semaphore::new(self.parallel.max(1)));
        let mut set = JoinSet::new();
        for finding in findings {
            let brief = verify_brief(&finding);
            let request = self.read_request(
                &worktree,
                brief,
                self.harnesses.verifier.clone(),
                self.config.models.verifier.clone(),
                &self.verifier_token.clone(),
            )?;
            let permit = sem.clone();
            set.spawn(async move {
                let _permit = permit.acquire_owned().await.context("verifier stopped")?;
                let (text, _) = ask(request).await?;
                Ok::<_, anyhow::Error>((finding, reply::parse_verdict(&text)))
            });
        }
        let mut confirmed = Vec::new();
        while let Some(joined) = set.join_next().await {
            let (finding, real) = joined.context("verifier task panicked")??;
            if real {
                confirmed.push(finding);
            }
        }
        self.drop_worktree(&worktree);
        Ok(confirmed)
    }

    fn note_findings(&mut self, confirmed: &[Finding]) -> anyhow::Result<()> {
        if confirmed.is_empty() {
            return Ok(());
        }
        for finding in confirmed {
            let model = self.author(&finding.page);
            self.note_mistake_model(&model, &finding.failure)?;
        }
        let model = self.byline(self.harnesses.reviewer.id(), &self.config.models.reviewer);
        let entries = confirmed
            .iter()
            .map(|finding| ChangeEntry {
                at: rfc3339_now(),
                kind: ChangeKind::ReviewFinding,
                page: Some(finding.page.clone()),
                commit: None,
                model: Some(model.clone()),
                files: vec![finding.file.clone()],
                keys: finding.key.iter().cloned().collect(),
                summary: format!("{}:{} {}", finding.file, finding.line, finding.failure),
            })
            .collect();
        self.commit_entries(entries, vec![], "taskmaster: record review findings")?;
        Ok(())
    }

    fn verify_project(&mut self) -> anyhow::Result<(bool, BTreeMap<PageId, String>)> {
        eprintln!("taskmaster: verifying the project");
        let plan = self.plan()?.clone();
        let owners = Owners::from_plan(&plan)?;
        let sources = load_sources(&self.project)?;
        let mut failing = ImportGraph::build(&sources, "").cross_page_imports(&owners);
        if let Some(note) = self.run_verify_script()? {
            let mut attributed = false;
            for line in note.lines() {
                let Some(rest) = line.strip_prefix("page:") else { continue };
                let Some((id, message)) = rest.split_once(':') else { continue };
                let id = id.trim();
                if self.run.pages.contains_key(id) && !message.trim().is_empty() {
                    attributed = true;
                    failing.entry(id.to_string()).or_default().push_str(message.trim());
                    failing.get_mut(id).unwrap().push('\n');
                }
            }
            if !attributed && failing.is_empty() {
                return Ok((false, BTreeMap::new()));
            }
            if !attributed {
                for errors in failing.values_mut() {
                    errors.push('\n');
                    errors.push_str(note.trim());
                }
            }
        }
        for (page, errors) in &failing {
            let model = self.author(page);
            self.note_mistake_model(&model, errors)?;
        }
        if !failing.is_empty() {
            let entries = failing
                .iter()
                .map(|(page, errors)| ChangeEntry {
                    at: rfc3339_now(),
                    kind: ChangeKind::VerifyFailed,
                    page: Some(page.clone()),
                    commit: None,
                    model: None,
                    files: vec![],
                    keys: vec![],
                    summary: errors.clone(),
                })
                .collect();
            self.commit_entries(entries, vec![], "taskmaster: record verification failure")?;
        }
        Ok((failing.is_empty(), failing))
    }

    /// Run `.taskmaster/verify` when the project already has an executable there.
    /// `Ok(None)` when there is no script or it passed. The text is the combined output.
    fn run_verify_script(&self) -> anyhow::Result<Option<String>> {
        let script = self.project.join(".taskmaster/verify");
        if !script.is_file() {
            return Ok(None);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script)?.permissions().mode();
            if mode & 0o111 == 0 {
                return Ok(None);
            }
        }
        let out = std::process::Command::new(&script)
            .current_dir(&self.project)
            .output()
            .context("failed to run .taskmaster/verify")?;
        if out.status.success() {
            return Ok(None);
        }
        let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Ok(Some(text))
    }

    fn worker_request(&mut self, page: &str, tier: Tier, errors: Option<&str>, cwd: PathBuf) -> anyhow::Result<AgentRequest> {
        let brief = self.worker_brief(page, tier, errors)?;
        let token = self.tokens.get(page).cloned().context("no dictionary token for this page")?;
        Ok(AgentRequest {
            harness: self.harness_for(tier),
            brief,
            cwd,
            model: self.model_for(tier),
            dictionary: Some(self.access(&token)),
            timeout: self.config.timeout,
            read_only: false,
        })
    }

    fn worker_brief(&self, page_id: &str, tier: Tier, errors: Option<&str>) -> anyhow::Result<String> {
        let page = self.page(page_id)?.clone();
        let contracts: Vec<String> = self.plan()?.contracts.iter().map(|c| c.path.clone()).collect();
        let cites = if page.cites.is_empty() { "none".to_string() } else { page.cites.join(", ") };
        let contract_list = if contracts.is_empty() {
            "- none\n".to_string()
        } else {
            contracts.iter().map(|path| format!("- `{path}`\n")).collect()
        };
        let mut brief = format!(
            "Build page `{id}` for route `{route}`.\n\
             You may create or edit only files matching `{glob}`. Files outside it are discarded and the page fails its check.\n\
             Cite only these Dictionary keys, and follow them: {cites}.\n\
             Contracts already in the tree:\n{contract_list}\n\
             {body}\n\
             If a Dictionary entry is wrong, stop and reply with {{\"flag\":{{\"key\":\"...\",\"issue\":\"...\"}}}} and do not edit other pages.\n",
            id = page.id,
            route = page.route,
            glob = page.owned_glob,
            body = page.brief,
        );
        if let Some(errors) = errors {
            brief.push_str("\nChecks failed last time. Fix these and leave every other page alone:\n");
            brief.push_str(errors);
            brief.push('\n');
        }
        let model = self.model_byline(tier);
        let book = records::load_book(&self.project)?;
        if let Some(errors) = errors {
            if let Some(fix) = book.fix_for(&model, errors) {
                brief.push_str("\nKnown fix for this model:\n");
                brief.push_str(fix);
                brief.push('\n');
            }
        }
        let warnings = book.warnings(&model);
        if !warnings.is_empty() {
            brief.push_str("\nMistakes this model keeps making. Do not repeat them:\n");
            for mistake in warnings {
                brief.push_str(&format!("- {} ({} times)\n", mistake.sample, mistake.count));
                if let Some(fix) = &mistake.fix {
                    brief.push_str(&format!("  Fix: {fix}\n"));
                }
            }
        }
        let catalog = self.catalog()?;
        brief.push_str("\nSearch the Commons before inventing a new contract.\n");
        let query = format!("{} {}", page.brief, page.cites.join(" "));
        let mut shown = 0;
        for entry in catalog.search(&query)? {
            if entry.tier != DictTier::Commons || page.cites.iter().any(|key| key == &entry.key) {
                continue;
            }
            if shown == 5 {
                break;
            }
            shown += 1;
            brief.push_str(&format!(
                "\nCommons `{key}`: {summary}\n{content}\n",
                key = entry.key,
                summary = entry.summary,
                content = entry.content,
            ));
        }
        for key in &page.cites {
            match catalog.resolve(key)? {
                Some(entry) => {
                    let tier = match entry.tier {
                        DictTier::Commons => "commons",
                        DictTier::Local => "local",
                    };
                    brief.push_str(&format!(
                        "\nDictionary `{key}` ({tier}, {kind}): {summary}\n{content}\n",
                        kind = kind_name(entry.kind),
                        summary = entry.summary,
                        content = entry.content,
                    ));
                }
                None => brief.push_str(&format!("\nDictionary `{key}` is not published yet.\n")),
            }
        }
        Ok(brief)
    }

    fn read_request(
        &self,
        cwd: &Path,
        brief: String,
        harness: Arc<dyn Harness>,
        model: Option<String>,
        token: &str,
    ) -> anyhow::Result<AgentRequest> {
        Ok(AgentRequest {
            harness,
            brief,
            cwd: cwd.to_path_buf(),
            model,
            dictionary: Some(self.access(token)),
            timeout: self.config.timeout,
            read_only: true,
        })
    }

    fn access(&self, token: &str) -> DictionaryAccess {
        DictionaryAccess { name: "dictionary".into(), url: self.url.clone(), bearer_token: token.to_string() }
    }

    fn grant_pages(&mut self) {
        let pages: Vec<String> =
            self.run.plan.as_ref().map(|plan| plan.pages.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
        for page in pages {
            if self.tokens.contains_key(&page) {
                continue;
            }
            let token = fresh_token(&page);
            self.mcp.grant(&token, Grant { run: self.config.run_id.clone(), page: page.clone() });
            self.tokens.insert(page, token);
        }
    }

    fn push_event(&mut self, event: Event) -> anyhow::Result<()> {
        self.events.push(event.clone());
        self.save_events()?;
        let commands = self.run.step(event);
        for command in commands.into_iter().rev() {
            self.pending.push_front(command);
        }
        self.persist()?;
        Ok(())
    }

    fn commit_entries(&mut self, entries: Vec<ChangeEntry>, mut paths: Vec<String>, message: &str) -> anyhow::Result<Option<String>> {
        if entries.is_empty() {
            return Ok(None);
        }
        let count = entries.len();
        self.changes.extend(entries);
        self.write_docs()?;
        self.save_changes()?;
        paths.push(ARCHITECTURE_FILE.to_string());
        paths.push(CHANGES_FILE.to_string());
        paths.retain(|path| git::safe_rel(path));
        paths.sort();
        paths.dedup();
        match git::commit_paths(&self.project, message, &paths)? {
            Some(hash) => {
                for entry in self.changes.iter_mut().rev().take(count) {
                    entry.commit = Some(hash.clone());
                }
                self.save_changes()?;
                Ok(Some(hash))
            }
            None => {
                self.changes.truncate(self.changes.len().saturating_sub(count));
                self.save_changes()?;
                self.write_docs()?;
                let _ = git::git(&self.project, &["checkout", "--", ARCHITECTURE_FILE, CHANGES_FILE]);
                Ok(None)
            }
        }
    }

    fn write_docs(&self) -> anyhow::Result<()> {
        if let Some(plan) = &self.run.plan {
            let path = self.project.join(ARCHITECTURE_FILE);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, render_architecture(plan))?;
        }
        let path = self.project.join(CHANGES_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, render_changes(&self.changes, 100))?;
        Ok(())
    }

    fn persist(&self) -> anyhow::Result<()> {
        let mut commands = self.running.clone();
        commands.extend(self.pending.iter().cloned());
        let path = self.dir.join("inflight.json");
        if commands.is_empty() {
            let _ = fs::remove_file(path);
            return Ok(());
        }
        atomic_write(&path, &serde_json::to_vec_pretty(&commands)?)?;
        Ok(())
    }

    fn clear_inflight(&self) -> anyhow::Result<()> {
        let _ = fs::remove_file(self.dir.join("inflight.json"));
        Ok(())
    }

    fn save_events(&self) -> anyhow::Result<()> {
        let mut body = String::new();
        for event in &self.events {
            body.push_str(&serde_json::to_string(event)?);
            body.push('\n');
        }
        atomic_write(&self.dir.join("events.jsonl"), body.as_bytes())
    }

    fn save_changes(&self) -> anyhow::Result<()> {
        let mut body = String::new();
        for entry in &self.changes {
            body.push_str(&serde_json::to_string(entry)?);
            body.push('\n');
        }
        atomic_write(&self.dir.join("changes.jsonl"), body.as_bytes())
    }

    fn write_status(&self, ok: bool) -> anyhow::Result<()> {
        let body = serde_json::json!({ "ok": ok, "phase": self.run.phase });
        atomic_write(&self.dir.join("status.json"), &serde_json::to_vec_pretty(&body)?)
    }

    fn fresh_worktree(&mut self, label: &str) -> anyhow::Result<PathBuf> {
        if !git::has_head(&self.project) {
            bail!("the project has no commits yet");
        }
        let path = git::worktree_dir(&self.project, &self.config.run_id, label);
        if path.exists() {
            let _ = git::remove_worktree(&self.project, &path);
            let _ = fs::remove_dir_all(&path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let head = git::head(&self.project)?;
        git::add_worktree(&self.project, &path, &head)?;
        self.trees.push(path.clone());
        Ok(path)
    }

    fn drop_worktree(&mut self, path: &Path) {
        let _ = git::remove_worktree(&self.project, path);
        let _ = fs::remove_dir_all(path);
        self.trees.retain(|have| have != path);
    }

    fn marker(&self, page: &str) -> PathBuf {
        self.dir.join("merged").join(page)
    }

    fn is_stale(&self, page: &str) -> bool {
        self.run.pages.get(page).is_some_and(|page| page.stale)
    }

    fn take_running(&mut self, page: &str) -> Option<Command> {
        let index = self.running.iter().position(|cmd| matches!(cmd, Command::SpawnWorker { page: id, .. } if id == page))?;
        Some(self.running.remove(index))
    }

    fn plan(&self) -> anyhow::Result<&Plan> {
        self.run.plan.as_ref().context("this run has no plan")
    }

    fn page(&self, id: &str) -> anyhow::Result<&PagePlan> {
        self.plan()?.pages.iter().find(|page| page.id == id).with_context(|| format!("plan has no page `{id}`"))
    }

    fn plan_summary(&self) -> String {
        let Some(plan) = &self.run.plan else { return "No plan is recorded.".into() };
        let mut out = String::from("Plan awaiting approval. Re-run with --yes to build it.\n");
        for page in &plan.pages {
            let cites = if page.cites.is_empty() { "-".to_string() } else { page.cites.join(", ") };
            out.push_str(&format!("- {} {} owns `{}` cites {}\n", page.id, page.route, page.owned_glob, cites));
        }
        out.push_str(&format!("estimate: {} tokens, ${:.2}\n", plan.estimate.tokens, plan.estimate.usd));
        out
    }

    fn harness_for(&self, tier: Tier) -> Arc<dyn Harness> {
        match tier {
            Tier::Worker => self.harnesses.worker.clone(),
            Tier::Mid => self.harnesses.mid.clone(),
            Tier::Master => self.harnesses.master.clone(),
        }
    }

    fn model_for(&self, tier: Tier) -> Option<String> {
        match tier {
            Tier::Worker => self.config.models.worker.clone(),
            Tier::Mid => self.config.models.mid.clone(),
            Tier::Master => self.config.models.master.clone(),
        }
    }

    fn byline(&self, id: &str, model: &Option<String>) -> String {
        match model {
            Some(model) => format!("{id} {model}"),
            None => id.to_string(),
        }
    }

    fn model_byline(&self, tier: Tier) -> String {
        let id = self.harness_for(tier).id();
        let model = self.model_for(tier);
        self.byline(id, &model)
    }

    /// The model that last built the page. Findings and verify failures are that model's mistakes.
    fn author(&self, page: &str) -> String {
        self.changes
            .iter()
            .rev()
            .find(|entry| {
                entry.page.as_deref() == Some(page)
                    && matches!(entry.kind, ChangeKind::PageBuilt | ChangeKind::PageRepaired)
            })
            .and_then(|entry| entry.model.clone())
            .unwrap_or_else(|| self.model_byline(Tier::Worker))
    }

    fn catalog(&self) -> anyhow::Result<Catalog> {
        Catalog::open(&self.dict, &self.commons)
    }

    fn note_mistake(&self, tier: Tier, failure: &str) -> anyhow::Result<()> {
        let model = self.model_byline(tier);
        self.note_mistake_model(&model, failure)
    }

    fn note_mistake_model(&self, model: &str, failure: &str) -> anyhow::Result<()> {
        let mut book = records::load_book(&self.project)?;
        book.record(model, failure);
        records::save_book(&self.project, &book)
    }

    fn learn_fix(&self, tier: Tier, failure: &str, text: &str) -> anyhow::Result<()> {
        let model = self.model_byline(tier);
        let mut book = records::load_book(&self.project)?;
        book.learn_fix(&model, failure, text);
        records::save_book(&self.project, &book)
    }

    fn citations_fresh(&self, page: &str) -> anyhow::Result<bool> {
        if !self.config.incremental {
            return Ok(false);
        }
        let citations = records::load_citations(&self.project)?;
        let cites = self.page(page)?.cites.clone();
        let catalog = self.catalog()?;
        let mut current = BTreeMap::new();
        for key in &cites {
            let hash = catalog.resolve(key)?.map(|entry| entry.hash).unwrap_or_default();
            current.insert(key.clone(), hash);
        }
        Ok(page_is_fresh(&cites, citations.pages.get(page).map(Vec::as_slice), &current))
    }

    fn record_citations(&self, page: &str) -> anyhow::Result<()> {
        if !self.config.incremental {
            return Ok(());
        }
        let mut citations = records::load_citations(&self.project)?;
        let cites = self.page(page)?.cites.clone();
        let catalog = self.catalog()?;
        let mut recorded = Vec::new();
        for key in &cites {
            let hash = catalog.resolve(key)?.map(|entry| entry.hash).unwrap_or_default();
            recorded.push(Citation { key: key.clone(), hash });
        }
        citations.pages.insert(page.to_string(), recorded);
        records::save_citations(&self.project, &citations)
    }

    /// Promote local entries that a passed page cited, and update Commons pass rates.
    fn settle_commons(&self, _ok: bool) -> anyhow::Result<()> {
        let Some(plan) = self.run.plan.clone() else { return Ok(()) };
        let catalog = self.catalog()?;
        for page in &plan.pages {
            let passed = self.run.pages.get(&page.id).is_some_and(|page| page.state == tm_core::PageState::Passed);
            for key in &page.cites {
                let Some(entry) = catalog.resolve(key)? else { continue };
                if entry.tier == DictTier::Commons {
                    catalog.note_result(key, passed)?;
                } else if passed {
                    catalog.promote(key)?;
                    catalog.note_result(key, true)?;
                }
            }
        }
        Ok(())
    }
}

fn failed(errors: impl Into<String>) -> WorkerOutcome {
    WorkerOutcome::CheckFailed { errors: errors.into() }
}

async fn ask(request: AgentRequest) -> anyhow::Result<(String, bool)> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut gate = WorktreeGate::new(&request.cwd, dict_prefixes());
    if request.read_only {
        gate = gate.read_only();
    }
    for dir in HARNESS_STATE_DIRS {
        gate = gate.with_readable(format!("{home}/{dir}"));
    }
    let task = WorkerTask {
        brief: request.brief,
        cwd: request.cwd,
        model: request.model,
        dictionary: request.dictionary,
        timeout: request.timeout,
        read_only: request.read_only,
    };
    let (tx, mut rx) = mpsc::channel(256);
    let collect = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            if let HarnessEvent::Text { text: chunk } = event {
                if text.len() < 1_000_000 {
                    text.push_str(&chunk);
                }
            }
        }
        text
    });
    let run = request.harness.run(task, Arc::new(gate), tx).await;
    let text = collect.await.unwrap_or_default();
    let summary = run?;
    Ok((text, summary.ok))
}

fn dict_prefixes() -> Vec<String> {
    ["dict__", "dictionary__", "dictionary_", "mcp__dictionary", "mcp_dictionary_"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn fresh_token(label: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{label}-{n}")
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Worker => "worker",
        Tier::Mid => "mid",
        Tier::Master => "master",
    }
}

fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Schema => "schema",
        EntryKind::Routes => "routes",
        EntryKind::ComponentApi => "component_api",
        EntryKind::Pattern => "pattern",
        EntryKind::Hint => "hint",
    }
}

fn load_parallel(dir: &Path, resuming: bool, requested: usize) -> anyhow::Result<usize> {
    let path = dir.join("meta.json");
    if let Ok(text) = fs::read_to_string(&path) {
        let meta: Meta = serde_json::from_str(&text).with_context(|| format!("cannot read {}", path.display()))?;
        let parallel = meta.parallel.max(1);
        if resuming && parallel != requested.max(1) {
            eprintln!("taskmaster: resuming with parallel {parallel} from the run record");
        }
        return Ok(parallel);
    }
    let parallel = requested.max(1);
    atomic_write(&path, &serde_json::to_vec_pretty(&Meta { parallel })?)?;
    Ok(parallel)
}

fn load_inflight(path: &Path) -> anyhow::Result<Option<Vec<Command>>> {
    let Ok(text) = fs::read_to_string(path) else { return Ok(None) };
    if text.trim().is_empty() {
        return Ok(Some(Vec::new()));
    }
    Ok(Some(serde_json::from_str(&text).with_context(|| format!("cannot read {}", path.display()))?))
}

fn load_lines<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    let Ok(text) = fs::read_to_string(path) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line).with_context(|| format!("{}:{}", path.display(), index + 1))?);
    }
    Ok(out)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}
