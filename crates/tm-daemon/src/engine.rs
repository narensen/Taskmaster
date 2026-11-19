//! Drive a run until it finishes, waits for approval, or is cancelled.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tm_core::docs::{
    ARCHITECTURE_FILE, CHANGES_FILE, render_architecture, render_changes, review_brief, security_brief, verify_brief,
};
use tm_core::{
    ChangeEntry, ChangeKind, Citation, Command, DictTier, DraftEntry, EntryKind, Event, Finding, Flag,
    FlagResolution, LedgerEntry, PageId, PagePlan, Plan, ReviewScope, Role, Run, RunPhase, Tier, WorkerOutcome,
    check_plan, forecast, page_is_fresh, totals, valid_key,
};
use tm_dictionary::{Catalog, CommonsRemote, Grant, LocalStore, McpState};
use tm_harness::{
    DictionaryAccess, HARNESS_STATE_DIRS, Harness, HarnessEvent, PermissionGate, RunSummary, Usage, WorkerTask, WorktreeGate,
};
use crate::checks::{self, Timing};
use tm_verify::{ImportGraph, Owners, load_sources, outside_owned};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinSet;

use crate::approvals::ApprovalGate;
use crate::git;
use crate::live::{Live, tool_target};
use crate::reply::{self, Resolution};
use crate::resume;
use crate::secret::random_hex;
use crate::runtime;
use crate::store::{Attempt, Meta, Records};
use crate::template::{self, Template};
use crate::time::rfc3339_now;
use crate::{Config, Harnesses, Outcome};

/// What one agent said and what it cost.
struct Answer {
    text: String,
    ok: bool,
    usage: Usage,
    cost_usd: Option<f64>,
    seconds: f64,
}

/// Who a call is charged to in the ledger.
#[derive(Clone)]
struct Bill {
    role: Role,
    page: Option<PageId>,
    harness: String,
    model: Option<String>,
}

/// What the checker is given about a page that just passed.
struct CheckInput {
    diff: String,
    files: Vec<String>,
    /// A screenshot of the page as the worker left it, when it had the page open.
    shot: Option<Vec<u8>>,
    /// What the worker said when it finished.
    said: String,
}

/// A finished mid-tier check: the page, the answer (or why there is none), what to bill,
/// and the scratch directory to remove.
type CheckDone = (PageId, anyhow::Result<Answer>, Bill, PathBuf);

struct Product {
    answer: Answer,
    bill: Bill,
    worktree: PathBuf,
}

struct AgentRequest {
    harness: Arc<dyn Harness>,
    brief: String,
    cwd: PathBuf,
    model: Option<String>,
    effort: Option<String>,
    dictionary: Option<DictionaryAccess>,
    timeout: Duration,
    /// Silence, with no tool running, after which the agent is replaced by a fresh one.
    stall: Option<Duration>,
    read_only: bool,
    /// Where the agent's events go as they arrive, tagged with who it is.
    feed: Option<Feed>,
    /// The page's own preview origin, when the agent has the browser tool.
    browser: Option<String>,
    /// Ask the harness for its native code mode where it has one (Claude Code).
    code_mode: bool,
    /// For a planner: when to stop looking and write. See [`Commit`].
    commit: Option<Commit>,
}

/// When a planner has looked and thought long enough. It is stopped after `tools` tool
/// calls, or once `after` has passed without a word of its reply, and is then asked once,
/// with no tools, to write the plan from its own notes.
#[derive(Debug, Clone, Copy)]
struct Commit {
    tools: usize,
    after: Duration,
}

/// What [`ask_once`] watches for a [`Commit`] budget.
#[derive(Default)]
struct Watched {
    tools: usize,
    writing: bool,
    thoughts: String,
}

/// The last `max` bytes of `text`, cut at a character boundary.
fn tail_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let start = (text.len() - max..text.len()).find(|&i| text.is_char_boundary(i)).unwrap_or(text.len());
    text[start..].to_string()
}

/// A planner stopped by its [`Commit`] budget: why, and the end of what it was thinking.
#[derive(Debug)]
struct Overrun {
    why: String,
    notes: String,
}

impl std::fmt::Display for Overrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stopped: {}", self.why)
    }
}

impl std::error::Error for Overrun {}

/// The thoughts kept for an [`Overrun`]: enough to write the plan from.
const OVERRUN_NOTES: usize = 12_000;

/// Reasoning effort for planning an empty project on the full pass.
const FRESH_EFFORT: &str = "medium";

/// A planner must have begun its reply within this window: 40% of the agent time limit,
/// kept between these two.
const COMMIT_AFTER_MIN: Duration = Duration::from_secs(90);
const COMMIT_AFTER_MAX: Duration = Duration::from_secs(240);

/// How many stored entries a search for everything returns at most.
const ENTRIES_LISTED: usize = 20;

/// The second planning turn after an [`Overrun`]: the same brief, why it was stopped, and
/// the planner's own notes, with the order to write the plan now.
fn commit_brief(brief: &str, why: &str, notes: &str) -> String {
    let mut out = format!(
        "{brief}\nYou were stopped because {why}. Do not call any tools and do not deliberate further: \
         reply now with the plan as one JSON object, from the decisions you already made.\n"
    );
    if !notes.trim().is_empty() {
        out.push_str("\nYour notes so far:\n");
        out.push_str(notes.trim());
        out.push('\n');
    }
    out
}

struct Feed {
    live: Arc<Live>,
    run: String,
    agent: serde_json::Value,
}

impl AgentRequest {
    fn bill(&self, role: Role, page: Option<&str>) -> Bill {
        Bill { role, page: page.map(str::to_string), harness: self.harness.id().to_string(), model: self.model.clone() }
    }
}

/// Shared-file repairs the master gets in one run.
const MAX_SHARED_REPAIRS: usize = 2;

/// What the build, the tests and the browser found.
#[derive(Default)]
struct Ran {
    pages: BTreeMap<PageId, String>,
    /// For the master: shared files the output names, and the output (with no file).
    shared: Vec<(String, String)>,
    timings: Vec<Timing>,
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn first_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.trim().lines().collect();
    let mut out = lines.iter().take(n).copied().collect::<Vec<_>>().join("\n    ");
    if lines.len() > n {
        out.push_str("\n    …");
    }
    out
}

/// What the project's own tools said about the files a run wrote.
#[derive(Default)]
struct Checked {
    timings: Vec<Timing>,
    /// Errors in a page's files, by page.
    pages: BTreeMap<PageId, String>,
    /// Errors in shared files the run wrote: the file, and the line to show.
    shared: Vec<(String, String)>,
}

enum Step {
    Continue,
    Stop(Outcome),
}

/// Runs in one process share the project's Errorography, its citation record and its
/// checked-out branch. Each is read, changed and written back under this lock.
static PROJECT: Mutex<()> = Mutex::new(());

struct Driver {
    config: Config,
    /// The repository the run was started in. Records and the Dictionary live here.
    project: PathBuf,
    /// Where the run writes and commits. A checkout of the run's branch, or `project`
    /// itself when the run builds in place.
    tree: PathBuf,
    branch: Option<String>,
    /// Set once the run's branch has been offered to the project: the branch, and
    /// whether it was merged.
    delivery: Option<(String, bool)>,
    /// The base branch and short commit the run was merged as, once it was.
    merge_commit: Option<(String, String)>,
    /// Citations of pages built on the run's branch. They are written to the project's
    /// record only once the branch is merged: until then the project does not have the
    /// pages, and a later run must not skip them.
    staged: BTreeMap<PageId, Vec<Citation>>,
    template: Option<Template>,
    /// The run id, as the records name it.
    id: String,
    records: Records,
    remote: Option<Arc<dyn CommonsRemote>>,
    run: Run,
    events: Vec<Event>,
    changes: Vec<ChangeEntry>,
    pending: VecDeque<Command>,
    running: Vec<Command>,
    jobs: JoinSet<(PageId, usize, anyhow::Result<Product>)>,
    /// Finished units of pages that still have units running.
    batches: HashMap<PageId, Vec<(usize, Product)>>,
    /// Caps concurrent worker calls at `parallel`, counting every unit of every page.
    slots: Arc<Semaphore>,
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
    live: Arc<Live>,
    /// Instructions the master wrote after reading a person's notes. Every later worker
    /// brief includes them. Empty until the master has read such a note.
    handed: String,
    /// The master's decision for the page note on this repair only. Cleared once the
    /// briefs for that spawn are written.
    repair_decision: String,
    /// The plan came from the small planning pass: first builds run at low reasoning effort.
    small: bool,
    /// Each page's preview and tab for the browser tool. `None` without Chrome.
    browsers: Option<Arc<crate::browser::PageBrowsers>>,
    /// `run_code` for each page being built, when Code Mode is on and Node is here.
    code: Option<Arc<crate::code::CodeTools>>,
    /// Mid-tier checks running beside the build (see `checker.rs`).
    checks: JoinSet<CheckDone>,
    /// Pages already given to the checker: each is checked once.
    checked: BTreeSet<PageId>,
    /// UI findings already sent to their page for one repair: (page, check).
    ui_sent: BTreeSet<(PageId, String)>,
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

    let id = config.run_id.clone();
    let records = match &config.store {
        Some(store) => Records::new(store.clone()),
        None => Records::files(&project),
    };
    let events = records.events(&id)?;
    let meta = load_meta(&records, &project, &config, !events.is_empty())?;
    let parallel = meta.parallel.max(1);
    let template = meta.template.as_deref().map(template::load).transpose()?;
    let run = Run::replay(parallel, events.clone());
    let changes = records.changes(&id)?;
    let inflight = records.inflight(&id)?;
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
    // Workers on a web page get a browser on that page's own preview.
    let browsers = config.browser.then(crate::runtime::chrome).flatten().map(|chrome| crate::browser::PageBrowsers::new(chrome, project.clone()));
    // Code Mode: run_code, beside the browser, on the same server.
    let code = if config.code_mode { crate::code::CodeTools::new(browsers.clone()) } else { None };
    let mut extensions: Vec<Arc<dyn tm_dictionary::Extension>> = Vec::new();
    if let Some(browsers) = &browsers {
        extensions.push(browsers.clone());
    }
    if let Some(code) = &code {
        extensions.push(code.clone());
    }
    if !extensions.is_empty() {
        mcp.extend(Arc::new(crate::code::Extensions(extensions)));
    }

    let reviewer_token = fresh_token()?;
    let verifier_token = fresh_token()?;
    let master_token = fresh_token()?;
    let run_id = config.run_id.clone();
    let remote = config.remote.clone();
    let live = config.live.clone().unwrap_or_else(|| {
        let store: Arc<dyn crate::store::Store> = match &config.store {
            Some(store) => store.clone(),
            None => Arc::new(crate::store::FileStore::in_project(&project)),
        };
        Arc::new(Live::new(store))
    });
    mcp.grant(&reviewer_token, Grant { run: run_id.clone(), page: "reviewer".into() });
    mcp.grant(&verifier_token, Grant { run: run_id.clone(), page: "verifier".into() });
    mcp.grant(&master_token, Grant { run: run_id, page: "master".into() });

    let mut driver = Driver {
        config,
        tree: project.clone(),
        project,
        branch: meta.branch.clone(),
        delivery: None,
        merge_commit: None,
        staged: BTreeMap::new(),
        template,
        id,
        records,
        remote,
        run,
        events,
        changes,
        pending: VecDeque::from(pending),
        running: Vec::new(),
        jobs: JoinSet::new(),
        batches: HashMap::new(),
        slots: Arc::new(Semaphore::new(parallel)),
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
        live,
        handed: String::new(),
        repair_decision: String::new(),
        small: false,
        browsers,
        code,
        checks: JoinSet::new(),
        checked: BTreeSet::new(),
        ui_sent: BTreeSet::new(),
    };
    driver.open_tree(meta.base.as_deref())?;
    driver.grant_pages()?;
    driver.pull_commons();
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
                self.start_spawn().await?;
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
                let plan = self.with_forecast(plan)?;
                self.push_event(Event::PlanReady(plan))?;
                self.grant_pages()?;
                Ok(Step::Continue)
            }
            RunPhase::AwaitingApproval => {
                self.pending.push_back(Command::RequestApproval);
                Ok(Step::Continue)
            }
            RunPhase::Done | RunPhase::Failed => {
                // A note that arrived after the last worker, or after the run had already
                // stopped, is a new master turn. The master reads it before anyone builds.
                let pending = self.live.has_pending_where(&self.id, |note| {
                    note.to == "master"
                        || note.to == "workers"
                        || note.to == "reviewer"
                        || note.to == "verifier"
                        || note.to.starts_with("page:")
                });
                if pending {
                    self.push_event(Event::FollowUp)?;
                    return Ok(Step::Continue);
                }
                let ok = self.run.phase == RunPhase::Done;
                self.write_status(ok)?;
                self.records.save_inflight(&self.id, &[])?;
                println!("taskmaster: run {} {}", self.config.run_id, if ok { "finished" } else { "failed" });
                println!("taskmaster: {}", self.spend_line()?);
                Ok(Step::Stop(Outcome::Finished { ok }))
            }
            _ => bail!("run is {:?} with nothing left to do", self.run.phase),
        }
    }

    /// Ask the master harness for a plan. One rejection is sent back with the reasons.
    /// Dictionary entries are published before the plan is recorded.
    async fn draft_plan(&mut self, prompt: &str) -> anyhow::Result<Plan> {
        let mut errors = Vec::new();
        let master_notes = self.live.take_notes(&self.id, &["master"]);
        for attempt in 1..=2 {
            let brief = self.with_notes(self.planner_brief(prompt, &errors)?, &master_notes);
            let cwd = self.planner_dir()?;
            let scratch = cwd != self.tree;
            let token = self.master_token.clone();
            let mut request = self.read_request(
                &cwd,
                brief.clone(),
                self.harnesses.master.clone(),
                self.config.models.master.clone(),
                &token,
            )?;
            self.small = self.small_pass(prompt);
            let fresh = self.fresh_project()?;
            if self.small || fresh {
                // Every stored entry is in the brief, so the master needs no tools.
                request.dictionary = None;
            }
            request.effort = if self.small {
                Some(crate::small::SMALL_EFFORT.to_string())
            } else if fresh {
                Some(FRESH_EFFORT.to_string())
            } else {
                None
            };
            request.commit = Some(self.commit_budget(self.small || fresh));
            self.tag(&mut request, "master", "planner", None, None);
            let bill = request.bill(Role::Planner, None);
            let mut asked = ask(request).await;
            // Looked or thought too long: one more turn, with no tools, to write it down.
            let stopped = asked.as_ref().err().and_then(|err| err.downcast_ref::<Overrun>()).map(|over| (over.why.clone(), over.notes.clone()));
            if let Some((why, notes)) = stopped {
                eprintln!("taskmaster: the planner was stopped ({why}); asking it to write the plan now");
                let mut quick = self.read_request(
                    &cwd,
                    commit_brief(&brief, &why, &notes),
                    self.harnesses.master.clone(),
                    self.config.models.master.clone(),
                    &token,
                )?;
                quick.dictionary = None;
                quick.effort = Some(crate::small::SMALL_EFFORT.to_string());
                self.tag(&mut quick, "master", "planner", None, None);
                asked = ask(quick).await;
            }
            if scratch {
                self.drop_worktree(&cwd);
            }
            let answer = asked?;
            self.charge(&bill, &answer)?;
            let (text, ok) = (answer.text, answer.ok);
            if !ok {
                errors = vec!["the planner did not finish".into()];
            } else {
                match reply::parse_plan(&text) {
                    Ok((mut plan, drafts)) => {
                        self.settle_skills(&mut plan, prompt);
                        let existing = self.existing_content(&plan, &drafts)?;
                        match check_plan(&plan, &drafts, &existing) {
                            Ok(()) => {
                                let published = self.publish_drafts(&drafts, &existing)?;
                                self.records.save_plan(&self.id, &plan)?;
                                eprintln!(
                                    "taskmaster: planned {} pages and {} new dictionary entries",
                                    plan.pages.len(),
                                    published
                                );
                                for line in agent_lines(&plan).lines() {
                                    eprintln!("taskmaster: {line}");
                                }
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

    /// An empty project whose stored Dictionary entries all fit in the brief: there is
    /// nothing a planner could find with tools that it is not already given.
    fn fresh_project(&self) -> anyhow::Result<bool> {
        Ok(self.existing_files() == 0 && self.entry_listing()?.len() < ENTRIES_LISTED)
    }

    /// How long a planner may look and think before it must write. A fresh project gives
    /// it nothing to look at, so a stray tool call or two is all it gets.
    fn commit_budget(&self, fresh: bool) -> Commit {
        let after = (self.config.timeout * 2 / 5).clamp(COMMIT_AFTER_MIN, COMMIT_AFTER_MAX);
        Commit { tools: if fresh { 4 } else { 12 }, after }
    }

    /// Project files other than a README and dotfiles, at the top level.
    fn existing_files(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.tree) else { return 0 };
        entries
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_lowercase();
                !name.starts_with('.') && !name.starts_with("readme")
            })
            .count()
    }

    /// Small prompts on an empty project take the light planning pass. Stored Dictionary
    /// entries go into its brief in full, so they do not force the slow, tool-driven pass.
    fn small_pass(&self, prompt: &str) -> bool {
        crate::small::is_small_prompt(prompt, self.existing_files())
    }

    /// Stored Dictionary entries, one formatted item each, for a planning brief.
    fn entry_listing(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .catalog()?
            .search("")?
            .iter()
            .map(|entry| {
                let place = match entry.tier {
                    DictTier::Commons => "commons",
                    DictTier::Local => "local",
                };
                format!(
                    "- `{key}` ({place}, {kind}): {summary}\n{content}\n",
                    key = entry.key,
                    kind = kind_name(entry.kind),
                    summary = entry.summary,
                    content = entry.content,
                )
            })
            .collect())
    }

    fn planner_brief(&self, prompt: &str, errors: &[String]) -> anyhow::Result<String> {
        let mut brief = self.planner_brief_inner(prompt, errors)?;
        brief.push_str(&self.skills_offer(prompt));
        Ok(brief)
    }

    /// The enabled skills the planner may attach to pages, and those the person named.
    fn skills_offer(&self, prompt: &str) -> String {
        let Some(store) = self.skills_store() else { return String::new() };
        let enabled = store.enabled();
        if enabled.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\nSkills are installed. A page may list some in `\"skills\":[\"name\"]`; its agent then reads that SKILL.md \
             before it starts. Attach a skill only where it clearly applies to that page's work, and none otherwise:\n",
        );
        for skill in &enabled {
            let about: String = skill.description.chars().take(200).collect();
            out.push_str(&format!("- {}: {}\n", skill.name, if about.is_empty() { "no description" } else { &about }));
        }
        let asked: Vec<String> = crate::skills::asked_for(prompt).into_iter().filter(|n| enabled.iter().any(|s| &s.name == n)).collect();
        if !asked.is_empty() {
            out.push_str(&format!("The request asks for {}: attach each to every page it applies to.\n", asked.join(", ")));
        }
        out
    }

    fn skills_store(&self) -> Option<crate::skills::Store> {
        let root = self.config.skills.clone()?;
        Some(crate::skills::Store::new(root, Arc::new(crate::skills::Web)))
    }

    /// Keep only skills that are installed and enabled, and give the ones the person asked
    /// for to every page when the planner attached them nowhere.
    fn settle_skills(&self, plan: &mut Plan, prompt: &str) {
        let enabled: Vec<String> = self.skills_store().map(|s| s.enabled().into_iter().map(|s| s.name).collect()).unwrap_or_default();
        for page in &mut plan.pages {
            page.skills.retain(|n| enabled.contains(n));
            page.skills.dedup();
        }
        for name in crate::skills::asked_for(prompt).into_iter().filter(|n| enabled.contains(n)) {
            if !plan.pages.iter().any(|p| p.skills.contains(&name)) {
                for page in &mut plan.pages {
                    page.skills.push(name.clone());
                }
            }
        }
    }

    fn planner_brief_inner(&self, prompt: &str, errors: &[String]) -> anyhow::Result<String> {
        if self.small_pass(prompt) {
            let template = self.template.as_ref().map(|t| t.brief()).unwrap_or_default();
            let mut brief = crate::small::small_brief(prompt, &self.entry_listing()?, &template, self.parallel);
            if !errors.is_empty() {
                brief.push_str("\nThe last plan was rejected:\n");
                for error in errors {
                    brief.push_str(&format!("- {error}\n"));
                }
                brief.push_str("Reply with a corrected JSON object.\n");
            }
            return Ok(brief);
        }
        let look = if self.fresh_project()? {
            "The project is empty and every Dictionary entry already stored is listed below, so you need no tools: \
             do not call any, and plan from this brief alone."
        } else {
            "Search the Dictionary before inventing a key."
        };
        let mut brief = format!(
            "Plan this product and lock its contracts.\n\n\
             Request:\n{prompt}\n\n\
             {look} Cite an existing entry instead of rewriting it. \
             If you include an existing key, its content must match exactly.\n\n\
             Dictionary entries already stored:\n"
        );
        let found = self.entry_listing()?;
        if found.is_empty() {
            brief.push_str("- none\n");
        }
        for entry in &found {
            brief.push_str(entry);
        }
        if found.len() >= ENTRIES_LISTED {
            brief.push_str("Further entries may be stored. Search the Dictionary before adding a key.\n");
        }
        if let Some(template) = &self.template {
            brief.push_str(&template.brief());
        }
        brief.push_str(
            "\nReply with one JSON object and do not edit files. \
             Leave `estimate` at zero: Taskmaster works it out from what past runs cost. \
             `contracts` are shared source files. Each page owns one glob. \
             `dictionary` holds new entries those pages cite. \
             `kind` is schema, routes, component_api, pattern, or hint. \
             Every page, and every unit, is one agent: its `brief` is that agent's whole task and `done` is one line \
             saying how it knows it is finished.\n\
             {\"design\":\"...\",\"contracts\":[{\"path\":\"app/types.ts\",\"content\":\"...\"}],\
             \"pages\":[{\"id\":\"home\",\"route\":\"/home\",\"owned_glob\":\"app/home/**\",\"cites\":[\"theme\"],\"brief\":\"...\",\"done\":\"...\",\"skills\":[]}],\
             \"estimate\":{\"tokens\":0,\"usd\":0},\
             \"dictionary\":[{\"key\":\"theme\",\"kind\":\"hint\",\"summary\":\"...\",\"content\":\"...\",\"stack\":\"next\"}]}\n\
             \nRules: at least one page; page ids use letters, digits, `_` and `-`; \
             owned_glob and contract paths are relative and do not contain `..`; \
             every cited key is an existing entry or one of the dictionary items; \
             briefs and contents are non-empty.\n",
        );
        brief.push_str(crate::small::DESIGN_RULE);
        brief.push_str(crate::small::DECIDE_RULE);
        brief.push_str(&crate::small::units_rule(self.parallel));
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
        if git::has_head(&self.tree) {
            self.fresh_worktree("plan")
        } else {
            Ok(self.tree.clone())
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
                let (ok, mut failing) = self.verify_project().await?;
                let fixes = self.harvest_checks(&mut failing).await?;
                let ok = ok && !fixes;
                self.push_event(Event::VerifyFinished { ok, failing })?;
                Ok(Step::Continue)
            }
            Command::Finish { ok } => {
                self.settle_commons(ok)?;
                self.deliver(ok);
                self.write_status(ok)?;
                self.persist()?;
                Ok(Step::Continue)
            }
        }
    }

    fn approve(&mut self) -> anyhow::Result<Step> {
        if self.config.reject {
            self.config.reject = false;
            eprintln!("taskmaster: plan rejected");
            self.push_event(Event::PlanRejected)?;
            let replan = self.config.plan.is_some()
                || self.config.prompt.as_ref().is_some_and(|prompt| !prompt.trim().is_empty());
            return Ok(if replan { Step::Continue } else { Step::Stop(Outcome::Rejected) });
        }
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

    async fn start_spawn(&mut self) -> anyhow::Result<()> {
        let Some(Command::SpawnWorker { page, tier, errors }) = self.pending.pop_front() else {
            bail!("expected a worker spawn");
        };
        if self.is_stale(&page) {
            self.records.clear_merged(&self.id, &page);
            eprintln!("taskmaster: {page} was built against an amended entry; discarding it");
            let outcome = WorkerOutcome::Passed;
            self.push_event(Event::WorkerFinished { page, outcome })?;
            return Ok(());
        }
        if self.records.merged(&self.id, &page) {
            self.records.clear_merged(&self.id, &page);
            eprintln!("taskmaster: {page} was already merged; recording it");
            self.record_citations(&page)?;
            self.push_event(Event::WorkerFinished { page, outcome: WorkerOutcome::Passed })?;
            return Ok(());
        }
        let repair = errors.is_some();
        let steer = self.live.has_pending_where(&self.id, |note| note.to == "master" || note.to == "workers");
        if !repair && self.citations_fresh(&page)? && !steer && !self.opened_by_note(&page) {
            eprintln!("taskmaster: {page} citations are unchanged");
            self.push_event(Event::PageFresh { page })?;
            return Ok(());
        }
        if let Err(err) = self.read_notes_before_workers(&page, repair).await {
            self.pending.push_front(Command::SpawnWorker { page, tier, errors });
            let _ = self.persist();
            return Err(err);
        }
        let units = self.page(&page)?.work_units();
        let mut prepared: Vec<(AgentRequest, PathBuf)> = Vec::new();
        let mut failure = None;
        for (index, unit) in units.iter().enumerate() {
            let label = if units.len() == 1 { format!("page-{page}") } else { format!("page-{page}-{}", unit.id) };
            let step = self.fresh_worktree(&label).and_then(|worktree| {
                let built = self.worker_request(&page, index, units.len(), tier, errors.as_deref(), worktree.clone());
                if built.is_err() {
                    self.drop_worktree(&worktree);
                }
                built.map(|request| (request, worktree))
            });
            match step {
                Ok(done) => prepared.push(done),
                Err(err) => {
                    failure = Some(err);
                    break;
                }
            }
        }
        if let Some(err) = failure {
            for (_, worktree) in &prepared {
                self.drop_worktree(worktree);
            }
            self.pending.push_front(Command::SpawnWorker { page, tier, errors });
            let _ = self.persist();
            return Err(err);
        }
        if units.len() == 1 {
            eprintln!("taskmaster: building {page} ({})", tier_name(tier));
        } else {
            eprintln!("taskmaster: building {page} ({} workers, {})", units.len(), tier_name(tier));
        }
        self.running.push(Command::SpawnWorker { page: page.clone(), tier, errors });
        self.batches.remove(&page);
        for (index, (mut request, worktree)) in prepared.into_iter().enumerate() {
            // Each unit is its own agent: `page:<page>/<unit>` when the page is split.
            let (agent, unit) = if units.len() > 1 {
                (format!("page:{page}/{}", units[index].id), Some(units[index].id.as_str()))
            } else {
                (format!("page:{page}"), None)
            };
            self.tag(&mut request, &agent, tier_name(tier), Some(&page), Some(&units[index].owned_glob));
            if units.len() == 1 {
                self.offer_browser(&page, &worktree, &mut request);
                self.offer_code(&page, &worktree, &mut request);
            }
            request.code_mode = self.config.code_mode;
            if let (Some(unit), Some(feed)) = (unit, request.feed.as_mut()) {
                feed.agent["unit"] = serde_json::json!(unit);
            }
            let bill = request.bill(Role::builder(tier), Some(&page));
            let (page, slots) = (page.clone(), self.slots.clone());
            self.jobs.spawn(async move {
                let permit = slots.acquire_owned().await.ok();
                let result = ask(request).await.map(|answer| Product { answer, bill, worktree });
                // The agent is released; its slot goes to the next waiting unit at once.
                drop(permit);
                (page, index, result)
            });
        }
        self.persist()?;
        Ok(())
    }

    async fn join_one(&mut self) -> anyhow::Result<()> {
        let (page, index, result) = self
            .jobs
            .join_next()
            .await
            .context("a worker disappeared")?
            .context("a worker task panicked")?;
        let shot = match (&self.browsers, self.config.checker && !self.checked.contains(&page)) {
            (Some(browsers), true) => browsers.snapshot(&page).await,
            _ => None,
        };
        if let Some(browsers) = &self.browsers {
            browsers.detach(&page);
        }
        if let Some(code) = &self.code {
            code.detach(&page);
        }
        let product = match result {
            Ok(product) => product,
            Err(err) => {
                self.persist()?;
                return Err(err);
            }
        };
        self.charge(&product.bill, &product.answer)?;
        let wanted = self.page(&page).map(|p| p.work_units().len()).unwrap_or(1);
        let batch = self.batches.entry(page.clone()).or_default();
        batch.push((index, product));
        if batch.len() < wanted {
            return Ok(());
        }
        let mut products = self.batches.remove(&page).unwrap_or_default();
        products.sort_by_key(|(index, _)| *index);
        let cmd = self.take_running(&page);
        if self.is_stale(&page) {
            for (_, product) in &products {
                self.drop_worktree(&product.worktree);
            }
            self.records.clear_merged(&self.id, &page);
            eprintln!("taskmaster: {page} was built against an amended entry; discarding it");
            self.push_event(Event::WorkerFinished { page, outcome: WorkerOutcome::Passed })?;
            return Ok(());
        }
        let (tier, errors) = match &cmd {
            Some(Command::SpawnWorker { tier, errors, .. }) => (*tier, errors.clone()),
            _ => (Tier::Worker, None),
        };
        let text = products.iter().map(|(_, p)| p.answer.text.as_str()).collect::<Vec<_>>().join("\n");
        // What the checker needs, read before the worktrees go.
        let review = (self.config.checker && !self.checked.contains(&page)).then(|| {
            let diff: String = products.iter().filter_map(|(_, p)| git::worktree_diff(&p.worktree).ok()).collect::<Vec<_>>().join("\n");
            let files: Vec<String> = products
                .iter()
                .flat_map(|(_, p)| git::worktree_changes(&p.worktree).unwrap_or_default().into_iter().map(|c| c.path))
                .collect();
            (diff, files)
        });
        let outcome = match self.judge(&page, tier, &errors, products) {
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
        if let (WorkerOutcome::Passed, Some((diff, files))) = (&outcome, review) {
            self.start_check(&page, tier, wanted, CheckInput { diff, files, shot, said: text });
        }
        self.push_event(Event::WorkerFinished { page: page.clone(), outcome })?;
        self.records.clear_merged(&self.id, &page);
        Ok(())
    }

    /// Give a page that just passed to the mid model for a second look, beside the build.
    fn start_check(&mut self, page: &str, tier: Tier, units: usize, input: CheckInput) {
        let CheckInput { diff, files, shot, said } = input;
        let (diff, said) = (diff.as_str(), said.as_str());
        let Some(why) = crate::checker::worth_checking(&files, diff.lines().count(), tier != Tier::Worker, units) else { return };
        let Ok(plan_page) = self.page(page).cloned() else { return };
        self.checked.insert(page.to_string());
        let dir = std::env::temp_dir().join(format!("tm-check-{}-{page}-{}", self.id, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        if fs::create_dir_all(&dir).is_err() || fs::write(dir.join("diff.patch"), clip(diff, 40_000)).is_err() {
            return;
        }
        let screenshot = shot.is_some_and(|png| fs::write(dir.join("screen.jpg"), png).is_ok());
        let tail: String = said.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
        let brief = crate::checker::brief(&crate::checker::Inputs {
            page,
            route: &plan_page.route,
            brief: &plan_page.brief,
            done: &plan_page.done,
            said: &tail,
            screenshot,
        });
        let mut request = AgentRequest {
            harness: self.harnesses.mid.clone(),
            brief,
            cwd: dir.clone(),
            model: self.config.models.mid.clone(),
            effort: Some("low".into()),
            dictionary: None,
            timeout: self.config.timeout.min(Duration::from_secs(300)),
            stall: None,
            read_only: true,
            feed: None,
            browser: None,
            code_mode: false,
            commit: None,
        };
        self.tag(&mut request, &format!("checker:{page}"), "checker", Some(page), None);
        let bill = request.bill(Role::Checker, Some(page));
        eprintln!("taskmaster: checking {page} with the mid model ({why})");
        let page = page.to_string();
        self.checks.spawn(async move {
            let answer = ask(request).await;
            (page, answer, bill, dir)
        });
    }

    /// Collect finished checks, waiting at most `checker::GRACE` for the rest, and add
    /// their fixes to the pages that verification failed or passed. True when a check
    /// asked for a fix.
    async fn harvest_checks(&mut self, failing: &mut BTreeMap<PageId, String>) -> anyhow::Result<bool> {
        if self.checks.is_empty() {
            return Ok(false);
        }
        let mut fixed = false;
        let started = Instant::now();
        let deadline = started + crate::checker::GRACE;
        let mut lines = Vec::new();
        while !self.checks.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(Some(joined)) = tokio::time::timeout(left, self.checks.join_next()).await else { break };
            let Ok((page, answer, bill, dir)) = joined else { continue };
            let _ = fs::remove_dir_all(&dir);
            let answer = match answer {
                Ok(answer) => answer,
                Err(err) => {
                    lines.push(format!("{page}: no answer ({err:#})"));
                    continue;
                }
            };
            self.charge(&bill, &answer)?;
            match crate::checker::verdict(&answer.text) {
                crate::checker::Verdict::Pass => lines.push(format!("{page}: pass ({:.1}s)", answer.seconds)),
                crate::checker::Verdict::Fixes(fixes) => {
                    lines.push(format!("{page}: {} fix{} ({:.1}s)", fixes.len(), if fixes.len() == 1 { "" } else { "es" }, answer.seconds));
                    fixed = true;
                    let entry = failing.entry(page).or_default();
                    if !entry.is_empty() {
                        entry.push('\n');
                    }
                    entry.push_str(&crate::checker::as_errors(&fixes));
                }
            }
        }
        let late = self.checks.len();
        self.checks.abort_all();
        let waited = started.elapsed().as_secs_f64();
        let mut text = format!("checker: {}; waited {waited:.1}s after the build", lines.join(", "));
        if late > 0 {
            text.push_str(&format!("; {late} still running were dropped"));
        }
        eprintln!("taskmaster: {text}");
        self.terminal("output", serde_json::json!({ "ok": true, "text": text }));
        Ok(fixed)
    }

    fn judge(&mut self, page: &str, tier: Tier, errors: &Option<String>, products: Vec<(usize, Product)>) -> anyhow::Result<WorkerOutcome> {
        let outcome = self.judge_inner(page, tier, errors, &products);
        if let Ok(outcome) = &outcome {
            for (_, product) in &products {
                self.note_attempt(page, tier, errors.as_deref(), product, outcome);
            }
        }
        for (_, product) in &products {
            self.drop_worktree(&product.worktree);
        }
        outcome
    }

    /// Keep what this attempt did, so the next one on this page knows. A change that was
    /// thrown away is kept as a diff, since it is not in the tree.
    fn note_attempt(&self, page: &str, tier: Tier, fixing: Option<&str>, product: &Product, outcome: &WorkerOutcome) {
        let files: Vec<String> =
            git::worktree_changes(&product.worktree).map(|changes| changes.into_iter().map(|c| c.path).collect()).unwrap_or_default();
        let (label, failure, diff) = match outcome {
            WorkerOutcome::Passed => ("merged", None, None),
            WorkerOutcome::CheckFailed { errors } => {
                let diff = git::worktree_diff(&product.worktree).ok().filter(|d| !d.trim().is_empty()).map(|d| clip(&d, 6000));
                ("discarded", Some(clip(errors, 1500)), diff)
            }
            WorkerOutcome::Flagged(flag) => ("flagged", Some(format!("objected to `{}`: {}", flag.key, flag.issue)), None),
        };
        let attempt = Attempt {
            at: rfc3339_now(),
            tier,
            model: self.model_byline(tier),
            outcome: label.into(),
            fixing: fixing.map(|text| clip(text, 1500)),
            failure,
            files,
            said: clip(product.answer.text.trim(), 1500),
            diff,
        };
        if let Err(err) = self.records.note_attempt(&self.id, page, &attempt) {
            eprintln!("taskmaster: the attempt at {page} was not recorded: {err:#}");
        }
    }

    fn judge_inner(
        &mut self,
        page: &str,
        tier: Tier,
        errors: &Option<String>,
        products: &[(usize, Product)],
    ) -> anyhow::Result<WorkerOutcome> {
        for (_, product) in products {
            if let Some((key, issue)) = reply::parse_flag(&product.answer.text) {
                return Ok(WorkerOutcome::Flagged(Flag { page: page.to_string(), key, issue }));
            }
        }
        let plan = self.plan()?.clone();
        let owners = Owners::from_plan(&plan)?;
        let units = self.page(page)?.work_units();
        let mut all: Vec<(PathBuf, git::Change)> = Vec::new();
        let mut seen = BTreeSet::new();
        for (index, product) in products {
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
            let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
            let mut outside: Vec<&str> = outside_owned(&owners, page, &paths);
            let unit_glob = units.get(*index).map(|u| u.owned_glob.clone()).unwrap_or_default();
            if units.len() > 1 {
                let matcher = globset::Glob::new(&unit_glob)
                    .with_context(|| format!("unit glob `{unit_glob}` is invalid"))?
                    .compile_matcher();
                outside.extend(paths.iter().filter(|p| !matcher.is_match(p.as_str())).map(String::as_str));
                outside.sort_unstable();
                outside.dedup();
            }
            if !outside.is_empty() {
                let glob = if units.len() > 1 { unit_glob } else { self.page(page)?.owned_glob.clone() };
                return Ok(failed(format!(
                    "these files are outside `{glob}` and were not merged: {}",
                    outside.join(", ")
                )));
            }
            for change in changes {
                if !seen.insert(change.path.clone()) {
                    return Ok(failed(format!("`{}` was changed by two parallel workers and was not merged", change.path)));
                }
                all.push((product.worktree.clone(), change));
            }
        }
        if all.is_empty() {
            let ok = products.iter().any(|(_, p)| p.answer.ok);
            let why = if ok {
                "worker produced no changes under its owned files"
            } else {
                "harness failed and produced no changes"
            };
            return Ok(failed(why));
        }
        let mut files = Vec::new();
        for (worktree, change) in &all {
            let dest = self.tree.join(&change.path);
            if change.status == 'D' {
                if dest.is_file() {
                    fs::remove_file(&dest)?;
                }
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(worktree.join(&change.path), &dest)
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
        self.records.mark_merged(&self.id, page, &hash)?;
        Ok(WorkerOutcome::Passed)
    }

    fn lock(&mut self) -> anyhow::Result<()> {
        let plan = self.plan()?.clone();
        let mut files: Vec<String> = Vec::new();
        // The template first, and only where the project has nothing: a contract with the
        // same path is written after it and wins.
        for file in self.template.iter().flat_map(|template| &template.files) {
            if !git::safe_rel(&file.path) {
                bail!("template path `{}` leaves the project", file.path);
            }
            let dest = self.tree.join(&file.path);
            if dest.exists() {
                continue;
            }
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, &file.content)?;
            files.push(file.path.clone());
        }
        for contract in &plan.contracts {
            if !git::safe_rel(&contract.path) {
                bail!("contract path `{}` leaves the project", contract.path);
            }
            let dest = self.tree.join(&contract.path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dest, &contract.content)?;
            files.push(contract.path.clone());
        }
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
        let mut request = self.read_request(
            &worktree,
            brief,
            self.harnesses.master.clone(),
            self.config.models.master.clone(),
            &self.master_token.clone(),
        )?;
        self.tag(&mut request, "master", "arbiter", Some(&flag.page), None);
        let bill = request.bill(Role::Arbiter, Some(&flag.page));
        let asked = ask(request).await;
        self.drop_worktree(&worktree);
        let answer = asked?;
        self.charge(&bill, &answer)?;
        let resolution = match reply::parse_resolution(&answer.text)? {
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
        let reviewer_notes = self.decision_for(|note| note.to == "reviewer").await?;
        for scope in scopes {
            let key = scope.key.clone();
            let brief = self.with_notes(review_brief(&scope), &reviewer_notes);
            let mut request = self.read_request(
                &worktree,
                brief,
                self.harnesses.reviewer.clone(),
                self.config.models.reviewer.clone(),
                &self.reviewer_token.clone(),
            )?;
            self.tag(&mut request, "reviewer", "reviewer", None, None);
            let permit = sem.clone();
            let bill = request.bill(Role::Reviewer, None);
            set.spawn(async move {
                let _permit = permit.acquire_owned().await.context("review stopped")?;
                Ok::<_, anyhow::Error>((bill, Some(key), ask(request).await?))
            });
        }
        if !security.is_empty() {
            let brief = self.with_notes(security_brief(&security), &reviewer_notes);
            let mut request = self.read_request(
                &worktree,
                brief,
                self.harnesses.reviewer.clone(),
                self.config.models.reviewer.clone(),
                &self.reviewer_token.clone(),
            )?;
            self.tag(&mut request, "reviewer", "security", None, None);
            let permit = sem.clone();
            let bill = request.bill(Role::Security, None);
            set.spawn(async move {
                let _permit = permit.acquire_owned().await.context("review stopped")?;
                Ok::<_, anyhow::Error>((bill, None, ask(request).await?))
            });
        }
        let mut findings = Vec::new();
        let mut failure = None;
        while let Some(joined) = set.join_next().await {
            let parsed = joined.context("review task panicked").and_then(|done| done).and_then(|(bill, key, answer)| {
                self.charge(&bill, &answer)?;
                reply::parse_review(&answer.text, key)
            });
            match parsed {
                Ok(found) => findings.extend(found),
                Err(err) => failure = Some(err),
            }
        }
        self.drop_worktree(&worktree);
        if let Some(err) = failure {
            return Err(err);
        }
        Ok(findings)
    }

    async fn verify_findings(&mut self, findings: Vec<Finding>) -> anyhow::Result<Vec<Finding>> {
        eprintln!("taskmaster: verifying {} findings", findings.len());
        let worktree = self.fresh_worktree("verify")?;
        let sem = Arc::new(Semaphore::new(self.parallel.max(1)));
        let mut set = JoinSet::new();
        let verifier_notes = self.decision_for(|note| note.to == "verifier").await?;
        for finding in findings {
            let brief = self.with_notes(verify_brief(&finding), &verifier_notes);
            let mut request = self.read_request(
                &worktree,
                brief,
                self.harnesses.verifier.clone(),
                self.config.models.verifier.clone(),
                &self.verifier_token.clone(),
            )?;
            self.tag(&mut request, "verifier", "verifier", Some(&finding.page), None);
            let permit = sem.clone();
            let bill = request.bill(Role::Verifier, Some(&finding.page));
            set.spawn(async move {
                let _permit = permit.acquire_owned().await.context("verifier stopped")?;
                Ok::<_, anyhow::Error>((finding, bill, ask(request).await?))
            });
        }
        let mut confirmed = Vec::new();
        let mut failure = None;
        while let Some(joined) = set.join_next().await {
            match joined.context("verifier task panicked").and_then(|done| done) {
                Ok((finding, bill, answer)) => {
                    self.charge(&bill, &answer)?;
                    if reply::parse_verdict(&answer.text) {
                        confirmed.push(finding);
                    }
                }
                Err(err) => failure = Some(err),
            }
        }
        self.drop_worktree(&worktree);
        if let Some(err) = failure {
            return Err(err);
        }
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

    /// Free checks only: no model is called. Reports how long each check took.
    async fn verify_project(&mut self) -> anyhow::Result<(bool, BTreeMap<PageId, String>)> {
        eprintln!("taskmaster: verifying the project");
        let mut timings = Vec::new();
        let verdict = self.verify_checks(&mut timings).await;
        let text = checks::report(&timings);
        eprintln!("taskmaster: {text}");
        self.terminal("output", serde_json::json!({ "ok": matches!(&verdict, Ok((true, _))), "text": text }));
        verdict
    }

    async fn verify_checks(&mut self, timings: &mut Vec<Timing>) -> anyhow::Result<(bool, BTreeMap<PageId, String>)> {
        let plan = self.plan()?.clone();
        let owners = Owners::from_plan(&plan)?;
        let sources = load_sources(&self.tree)?;
        let mut failing = ImportGraph::build(&sources, "").cross_page_imports(&owners);
        // The static checks and the project's own build and tests do not wait for each other.
        let steps = if self.config.runtime_checks { runtime::steps(&self.tree) } else { Vec::new() };
        let (checked, built) = {
            let tree = self.tree.clone();
            let build = async {
                if steps.is_empty() { None } else { Some(runtime::run_steps(&tree, &steps, checks::STEP_TIMEOUT).await) }
            };
            let (checked, built) = tokio::join!(self.mechanical_checks(&owners), build);
            (checked?, built)
        };
        let mut checked = checked;
        let mut built = built;
        timings.append(&mut checked.timings);
        if !checked.shared.is_empty() && self.shared_repairs() < MAX_SHARED_REPAIRS {
            self.repair_shared(&checked.shared).await?;
            checked = self.mechanical_checks(&owners).await?;
            timings.append(&mut checked.timings);
            built = None; // the files changed, so the build is run again
        }
        for (page, errors) in checked.pages {
            let text = failing.entry(page).or_default();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&errors);
        }
        if failing.is_empty() && !checked.shared.is_empty() {
            // No page owns these files and the master could not repair them, so there is
            // nobody left to send the errors to.
            let lines: Vec<String> = checked.shared.iter().map(|(_, line)| line.clone()).collect();
            eprintln!("taskmaster: shared files still fail the project's checks:\n{}", lines.join("\n"));
            let entry = ChangeEntry {
                at: rfc3339_now(),
                kind: ChangeKind::VerifyFailed,
                page: None,
                commit: None,
                model: None,
                files: checked.shared.iter().map(|(file, _)| file.clone()).collect(),
                keys: vec![],
                summary: lines.join("\n"),
            };
            self.commit_entries(vec![entry], vec![], "taskmaster: record verification failure")?;
            return Ok((false, BTreeMap::new()));
        }
        if failing.is_empty() && self.config.runtime_checks {
            let routes: Vec<(PageId, String)> = plan.pages.iter().map(|p| (p.id.clone(), p.route.clone())).collect();
            let mut ran = self.runtime_checks(&owners, &routes, built).await?;
            timings.append(&mut ran.timings);
            if ran.pages.is_empty() && !ran.shared.is_empty() && self.shared_repairs() < MAX_SHARED_REPAIRS {
                self.repair_shared(&ran.shared).await?;
                ran = self.runtime_checks(&owners, &routes, None).await?;
                timings.append(&mut ran.timings);
            }
            failing.extend(ran.pages);
            if failing.is_empty() && !ran.shared.is_empty() {
                let lines: Vec<String> = ran.shared.iter().map(|(_, line)| line.clone()).collect();
                eprintln!("taskmaster: the project's build or tests still fail in shared files:\n{}", lines.join("\n"));
                let entry = ChangeEntry {
                    at: rfc3339_now(),
                    kind: ChangeKind::VerifyFailed,
                    page: None,
                    commit: None,
                    model: None,
                    files: ran.shared.iter().map(|(file, _)| file.clone()).filter(|f| !f.is_empty()).collect(),
                    keys: vec![],
                    summary: clip(&lines.join("\n"), 4000),
                };
                self.commit_entries(vec![entry], vec![], "taskmaster: record verification failure")?;
                return Ok((false, BTreeMap::new()));
            }
        }
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

    /// Build and test the project with its own scripts, then open each page in a browser.
    /// Failures in a page's files or at its route go to that page. Ones that name a shared
    /// file this run wrote go to the master. Anything else is in files the run did not
    /// write, or in the setup, and is reported without spending tokens on it.
    async fn runtime_checks(
        &mut self,
        owners: &Owners,
        routes: &[(PageId, String)],
        built: Option<(Vec<Timing>, Option<runtime::Failure>)>,
    ) -> anyhow::Result<Ran> {
        let mut ran = Ran::default();
        let steps = runtime::steps(&self.tree);
        for step in &steps {
            self.terminal(
                "command",
                serde_json::json!({ "command": format!("{} {}", step.program, step.args.join(" ")), "cwd": self.tree.display().to_string() }),
            );
        }
        // A static site's smoke check does not need a build, so it runs alongside one.
        let alongside = checks::is_static_site(&self.tree);
        let (steps_timed, failed, early_smoke) = match built {
            Some((timed, failed)) => (timed, failed, None),
            None if alongside => {
                let (steps_run, smoke) = tokio::join!(
                    runtime::run_steps(&self.tree, &steps, checks::STEP_TIMEOUT),
                    self.smoke(routes)
                );
                (steps_run.0, steps_run.1, Some(smoke))
            }
            None => {
                let (timed, failed) = runtime::run_steps(&self.tree, &steps, checks::STEP_TIMEOUT).await;
                (timed, failed, None)
            }
        };
        ran.timings.extend(steps_timed);
        if !steps.is_empty() {
            match &failed {
                Some(failure) => self.terminal("output", serde_json::json!({ "ok": false, "step": failure.step, "text": failure.output })),
                None => self.terminal("output", serde_json::json!({ "ok": true, "text": format!("{} step(s) passed", steps.len()) })),
            }
        }
        if let Some(failure) = failed {
            let (pages, rest) = runtime::attribute(&failure.output, owners, routes);
            for (page, text) in pages {
                ran.pages.insert(page, format!("`{}` failed:\n{text}", failure.step));
            }
            if !rest.is_empty() {
                let locked = self.locked_files();
                let named: BTreeSet<String> =
                    rest.lines().flat_map(runtime::paths_in).filter(|path| locked.contains(path)).collect();
                if !named.is_empty() {
                    let text = format!("`{}` failed:\n{rest}", failure.step);
                    ran.shared = named.into_iter().map(|file| (file, String::new())).collect();
                    ran.shared.push((String::new(), text));
                } else if ran.pages.is_empty() {
                    eprintln!(
                        "taskmaster: `{}` failed, and the output names no file this run wrote, so nothing was sent for repair:\n{rest}",
                        failure.step
                    );
                }
            }
            return Ok(ran);
        }
        self.terminal(
            "command",
            serde_json::json!({ "command": format!("browser check: {} route(s)", routes.len()), "cwd": self.tree.display().to_string() }),
        );
        let (browsed, smoke_timing) = match early_smoke {
            Some(done) => done,
            None => self.smoke(routes).await,
        };
        ran.timings.extend(smoke_timing);
        match &browsed {
            Ok(Some(browsed)) => {
                let mut text: Vec<String> = browsed.notes.clone();
                for (page, problem) in &browsed.pages {
                    text.push(format!("{page}: {problem}"));
                }
                for (page, audit) in &browsed.audits {
                    for finding in &audit.found {
                        text.push(format!("{page}: {}", finding.text));
                    }
                }
                let routes: Vec<_> = routes.iter().map(|(page, route)| serde_json::json!({ "page": page, "route": route })).collect();
                self.terminal("preview", serde_json::json!({ "routes": routes, "shots": browsed.screenshots.len() }));
                self.terminal("output", serde_json::json!({ "ok": browsed.pages.is_empty(), "text": text.join("\n") }));
            }
            Ok(None) => self.terminal("output", serde_json::json!({ "ok": true, "text": "no browser check for this project" })),
            Err(err) => self.terminal("output", serde_json::json!({ "ok": false, "text": format!("the browser check did not run: {err:#}") })),
        }
        match browsed {
            Ok(Some(browsed)) => {
                for (page, png) in &browsed.screenshots {
                    if let Err(err) = self.records.save_screenshot(&self.id, page, png) {
                        eprintln!("taskmaster: the screenshot of {page} was not kept: {err:#}");
                    }
                }
                for note in &browsed.notes {
                    eprintln!("taskmaster: browser check: {note}");
                }
                eprintln!(
                    "taskmaster: opened {} page(s) in a browser; {} had errors",
                    browsed.screenshots.len(),
                    browsed.pages.len()
                );
                ran.pages = browsed.pages;
                // A UI finding goes to its page once. If the repair did not fix it, it is a
                // warning from then on, so a check that misreads a page cannot fail it.
                for (page, audit) in &browsed.audits {
                    let mut first = Vec::new();
                    for finding in &audit.found {
                        if self.ui_sent.insert((page.clone(), finding.check.clone())) {
                            first.push(format!("In the browser: {}", finding.text));
                        } else {
                            eprintln!("taskmaster: {page} still has a UI problem after its repair, left as a warning: {}", finding.text);
                        }
                    }
                    if !first.is_empty() {
                        let errors = ran.pages.entry(page.clone()).or_default();
                        if !errors.is_empty() {
                            errors.push('\n');
                        }
                        errors.push_str(&first.join("\n"));
                    }
                }
                let flagged: usize = browsed.audits.values().map(|a| a.found.len()).sum();
                if flagged > 0 {
                    eprintln!("taskmaster: the UI checks found {flagged} problem(s)");
                }
            }
            Ok(None) => {}
            Err(err) => eprintln!("taskmaster: the browser check did not run: {err:#}"),
        }
        Ok(ran)
    }

    /// The browser smoke check, held to its own short timeout. A check that cannot run, or
    /// runs out of time, is reported but does not fail the project.
    async fn smoke(&self, routes: &[(PageId, String)]) -> (anyhow::Result<Option<runtime::Browsed>>, Option<Timing>) {
        let began = Instant::now();
        match tokio::time::timeout(checks::SMOKE_TIMEOUT, runtime::browse(&self.tree, routes)).await {
            Ok(Ok(None)) => (Ok(None), Some(Timing::new("smoke", began.elapsed(), "skipped"))),
            Ok(Ok(Some(browsed))) => {
                let status = if browsed.pages.is_empty() && !browsed.screenshots.is_empty() { "ok" } else if browsed.pages.is_empty() { "skipped" } else { "failed" };
                (Ok(Some(browsed)), Some(Timing::new("smoke", began.elapsed(), status)))
            }
            Ok(Err(err)) => (Err(err), Some(Timing::new("smoke", began.elapsed(), "skipped"))),
            Err(_) => (
                Err(anyhow::anyhow!("the smoke check did not finish in {} seconds", checks::SMOKE_TIMEOUT.as_secs())),
                Some(Timing::new("smoke", began.elapsed(), "timeout")),
            ),
        }
    }

    /// Shared files this run wrote: the contracts and the template.
    fn locked_files(&self) -> BTreeSet<String> {
        self.changes
            .iter()
            .filter(|entry| matches!(entry.kind, ChangeKind::ContractsLocked | ChangeKind::SharedRepaired))
            .flat_map(|entry| entry.files.iter().cloned())
            .collect()
    }

    fn shared_repairs(&self) -> usize {
        self.changes.iter().filter(|entry| entry.kind == ChangeKind::SharedRepaired).count()
    }

    /// Ask the master to fix shared files the run wrote. Workers may not touch them, and
    /// the master may touch nothing else: any other edit it makes is discarded.
    async fn repair_shared(&mut self, errors: &[(String, String)]) -> anyhow::Result<()> {
        let allowed: BTreeSet<String> = errors.iter().map(|(file, _)| file.clone()).filter(|file| !file.is_empty()).collect();
        eprintln!("taskmaster: asking the master to repair {} shared file(s)", allowed.len());
        let mut brief = String::from(
            "Shared files this run wrote fail the project's checks. Fix the errors below.\n\
             You may edit only these files; any other edit is discarded:\n",
        );
        for file in &allowed {
            brief.push_str(&format!("- `{file}`\n"));
        }
        brief.push_str("\nPages already import these files, so keep every exported name and type that still makes sense.\n\nErrors:\n");
        for (_, line) in errors.iter().filter(|(_, line)| !line.is_empty()) {
            brief.push_str(&format!("{line}\n"));
        }
        let notes = self.live.take_notes(&self.id, &["master"]);
        let brief = self.with_notes(brief, &notes);
        let worktree = self.fresh_worktree("shared")?;
        let mut request = AgentRequest {
            harness: self.harnesses.master.clone(),
            brief,
            cwd: worktree.clone(),
            model: self.config.models.master.clone(),
            effort: None,
            dictionary: Some(self.access(&self.master_token.clone())),
            timeout: self.config.timeout,
            stall: None,
            read_only: false,
            feed: None,
            browser: None,
            code_mode: false,
            commit: None,
        };
        self.tag(&mut request, "master", "master", None, None);
        let bill = request.bill(Role::Master, None);
        let asked = ask(request).await;
        let merged = match asked {
            Ok(answer) => {
                self.charge(&bill, &answer)?;
                self.merge_shared(&worktree, &allowed)
            }
            Err(err) => Err(err),
        };
        self.drop_worktree(&worktree);
        let files = merged?;
        // The attempt is recorded even when nothing changed, so it is not made again.
        let summary = if files.is_empty() {
            "the master changed no shared file".to_string()
        } else {
            format!("repaired {}", files.join(", "))
        };
        let entry = ChangeEntry {
            at: rfc3339_now(),
            kind: ChangeKind::SharedRepaired,
            page: None,
            commit: None,
            model: Some(self.byline(self.harnesses.master.id(), &self.config.models.master)),
            files: files.clone(),
            keys: vec![],
            summary,
        };
        if self.commit_entries(vec![entry.clone()], files, "taskmaster: repair shared files")?.is_none() {
            self.changes.push(entry);
            self.save_changes()?;
        }
        Ok(())
    }

    fn merge_shared(&self, worktree: &Path, allowed: &BTreeSet<String>) -> anyhow::Result<Vec<String>> {
        let mut files = Vec::new();
        for change in git::worktree_changes(worktree)? {
            if !allowed.contains(&change.path) || change.status == 'D' || !git::safe_rel(&change.path) {
                continue;
            }
            let src = worktree.join(&change.path);
            if fs::symlink_metadata(&src)?.file_type().is_symlink() {
                continue;
            }
            fs::copy(&src, self.tree.join(&change.path)).with_context(|| format!("cannot copy {}", change.path))?;
            files.push(change.path);
        }
        Ok(files)
    }

    /// Typecheck and lint the files this run built, with the project's own tools. No tokens
    /// are spent. An error in a page's file goes back to that page. An error in a shared
    /// file this run wrote goes to the master. An error anywhere else was there before the
    /// run, so it is printed and nothing more.
    async fn mechanical_checks(&self, owners: &Owners) -> anyhow::Result<Checked> {
        if !self.config.checks {
            return Ok(Checked::default());
        }
        let mut built: BTreeSet<String> = BTreeSet::new();
        for entry in &self.changes {
            if matches!(entry.kind, ChangeKind::PageBuilt | ChangeKind::PageRepaired) {
                built.extend(entry.files.iter().filter(|file| self.tree.join(file).is_file()).cloned());
            }
        }
        if built.is_empty() {
            return Ok(Checked::default());
        }
        let locked = self.locked_files();
        // Shared files are checked too: a page can be right and its contract wrong.
        built.extend(locked.iter().filter(|file| self.tree.join(file).is_file()).cloned());
        let (project, files) = (self.tree.clone(), built.into_iter().collect::<Vec<_>>());
        let picked: Vec<&str> = checks::select(&project, &files).into_iter().filter(|c| c.is_static()).map(|c| c.name()).collect();
        if picked.is_empty() {
            return Ok(Checked::default());
        }
        let began = Instant::now();
        self.terminal("command", serde_json::json!({ "command": format!("{} on {} file(s)", picked.join("+"), files.len()), "cwd": project.display().to_string() }));
        let mut sane = checks::html_sanity(&project, &files);
        // Quality bars are held only where a page is plain files the checks can read whole.
        if checks::is_static_site(&project) {
            sane.extend(checks::html_quality(&project, &files));
        }
        let lint_files = files.clone();
        let lint_project = project.clone();
        let linted = if picked == ["html"] {
            Ok(Ok(tm_review::lint::Report { findings: vec![], notes: vec![] }))
        } else {
            tokio::time::timeout(
                checks::STATIC_TIMEOUT,
                tokio::task::spawn_blocking(move || tm_review::lint::run_errors(&lint_project, &lint_files)),
            )
            .await
        };
        let (report, mut status) = match linted {
            Ok(joined) => (joined.context("the project checks panicked")?, "ok"),
            Err(_) => (
                tm_review::lint::Report { findings: vec![], notes: vec![format!("the static checks did not finish in {} seconds", checks::STATIC_TIMEOUT.as_secs())] },
                "timeout",
            ),
        };
        let mut lines: Vec<String> = report.findings.iter().map(|f| format!("{}:{} {}", f.file, f.line, f.failure)).collect();
        lines.extend(sane.iter().map(|(_, line)| line.clone()));
        if !lines.is_empty() && status == "ok" {
            status = "failed";
        }
        let timing = Timing::new(picked.join("+"), began.elapsed(), status);
        self.terminal(
            "output",
            serde_json::json!({ "ok": lines.is_empty(), "text": if lines.is_empty() { "no errors".to_string() } else { lines.join("\n") } }),
        );
        for note in &report.notes {
            eprintln!("taskmaster: check: {note}");
        }
        let mut checked = Checked { timings: vec![timing], ..Checked::default() };
        let mut older = 0;
        for (file, line) in &sane {
            match owners.owner(file) {
                Some(page) => {
                    let text = checked.pages.entry(page.clone()).or_default();
                    text.push_str(line);
                    text.push('\n');
                }
                None if locked.contains(file) => checked.shared.push((file.clone(), line.clone())),
                None => eprintln!("taskmaster: {line}"),
            }
        }
        for finding in &report.findings {
            let line = format!("{}:{} {}", finding.file, finding.line, finding.failure);
            match owners.owner(&finding.file) {
                Some(page) => {
                    let text = checked.pages.entry(page.clone()).or_default();
                    text.push_str(&line);
                    text.push('\n');
                }
                None if locked.contains(&finding.file) => checked.shared.push((finding.file.clone(), line)),
                None => {
                    older += 1;
                    eprintln!("taskmaster: {line}");
                }
            }
        }
        if older > 0 {
            eprintln!("taskmaster: {older} error(s) are in files this run did not write, so they were left alone");
        }
        Ok(checked)
    }

    /// Run `.taskmaster/verify` when the project already has an executable there.
    /// `Ok(None)` when there is no script or it passed. The text is the combined output.
    fn run_verify_script(&self) -> anyhow::Result<Option<String>> {
        // A script the project committed is in the run's checkout. One it keeps untracked
        // is only in the project.
        let script = [&self.tree, &self.project]
            .into_iter()
            .map(|root| root.join(".taskmaster/verify"))
            .find(|path| path.is_file());
        let Some(script) = script else { return Ok(None) };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script)?.permissions().mode();
            if mode & 0o111 == 0 {
                return Ok(None);
            }
        }
        self.terminal("command", serde_json::json!({ "command": script.display().to_string(), "cwd": self.tree.display().to_string() }));
        let out = std::process::Command::new(&script)
            .current_dir(&self.tree)
            .output()
            .context("failed to run .taskmaster/verify")?;
        let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        self.terminal("output", serde_json::json!({ "ok": out.status.success(), "text": clip(text.trim(), 8000) }));
        if out.status.success() {
            return Ok(None);
        }
        Ok(Some(text))
    }

    fn worker_request(&mut self, page: &str, unit: usize, units: usize, tier: Tier, errors: Option<&str>, cwd: PathBuf) -> anyhow::Result<AgentRequest> {
        // A person's notes never land here raw. The master has already read them and
        // written `handed`, which every worker follows.
        let owned = self.owned_files(page, unit, units, &cwd);
        let mut brief = self.worker_brief(page, unit, units, tier, errors, &owned)?;
        if !self.handed.is_empty() {
            brief.push_str("\nThe master decided, after reading the person's notes. Follow it:\n");
            brief.push_str(&self.handed);
        }
        if !self.repair_decision.is_empty() {
            brief.push_str("\nThe master decided this for the repair. Follow it:\n");
            brief.push_str(&self.repair_decision);
        }
        brief.push_str(&self.deliver_skills(page, &cwd));
        let token = self.tokens.get(page).cloned().context("no dictionary token for this page")?;
        Ok(AgentRequest {
            harness: self.harness_for(tier),
            brief,
            cwd,
            model: self.model_for(tier),
            // A small product's first build needs little deliberation; repairs think at the default.
            effort: (self.small && errors.is_none()).then(|| crate::small::SMALL_EFFORT.to_string()),
            dictionary: Some(self.access(&token)),
            timeout: self.config.timeout,
            stall: None,
            read_only: false,
            feed: None,
            browser: None,
            code_mode: false,
            commit: None,
        })
    }

    /// Serve the worktree as the page's preview and give the agent the browser on it, when
    /// this is a web project and Chrome is here.
    fn offer_browser(&self, page: &str, cwd: &Path, request: &mut AgentRequest) {
        let Some(browsers) = &self.browsers else { return };
        if !self.looks_like_web(cwd) {
            return;
        }
        let Ok(origin) = browsers.attach(page, cwd) else { return };
        let route = self.page(page).map(|p| p.route.clone()).unwrap_or_else(|_| "/".into());
        let dev = crate::preview::dev_command(cwd, "", 0).is_some();
        let path = if dev { route } else { "/ (files are served as they are in your worktree, such as /app/index.html)".to_string() };
        request.brief.push_str(&format!(
            "\nA browser is ready on your own page's preview, {origin}; nothing else opens in it. Before you reply that you are done, \
             look at what you built: browser_open with path {path}, then browser_console for errors, and browser_screenshot to see it. \
             Fix what is wrong and look again. Use browser_click and browser_type to try what the page does.\n"
        ));
        request.browser = Some(origin);
    }

    /// Give the agent `run_code` over its worktree, through the gate its own tools face.
    fn offer_code(&self, page: &str, cwd: &Path, request: &mut AgentRequest) {
        let Some(code) = &self.code else { return };
        code.attach(page, cwd, gate_for(request));
        request.brief.push_str(
            "\nYou have run_code: one call runs a short async JavaScript program with read, write, edit, grep, ls, check, sh and browser. \
             Prefer one run_code call that does several steps (read what you need, write your files, then `return await check()`) \
             over many single tool calls. Keep your replies short: the files are the work.\n",
        );
    }

    /// A project a browser can show: one with a package.json, an HTML start page, or plan
    /// files of the web's kinds.
    fn looks_like_web(&self, cwd: &Path) -> bool {
        if cwd.join("package.json").is_file() || crate::static_site::start_page(cwd).is_some() {
            return true;
        }
        const WEB: [&str; 6] = [".html", ".css", ".tsx", ".jsx", ".vue", ".svelte"];
        let Ok(plan) = self.plan() else { return false };
        plan.contracts.iter().map(|c| c.path.as_str()).chain(plan.pages.iter().map(|p| p.owned_glob.as_str())).any(|text| WEB.iter().any(|ext| text.contains(ext)))
    }

    /// Copy the page's skills into the worktree, and the lines that point the agent at them.
    fn deliver_skills(&self, page: &str, cwd: &Path) -> String {
        let Ok(names) = self.page(page).map(|p| p.skills.clone()) else { return String::new() };
        if names.is_empty() {
            return String::new();
        }
        let Some(store) = self.skills_store() else { return String::new() };
        let given = store.deliver(&names, cwd);
        if given.is_empty() {
            return String::new();
        }
        let mut out = String::from("\nSkills for this page. Read each SKILL.md before the work it covers, and follow it:\n");
        for (name, about) in given {
            out.push_str(&format!("- `.taskmaster/skills/{name}/SKILL.md`{}\n", if about.is_empty() { String::new() } else { format!(": {about}") }));
        }
        out
    }

    /// Files already in the worktree that this agent owns, so it need not look for them.
    fn owned_files(&self, page: &str, unit: usize, units: usize, cwd: &Path) -> Vec<String> {
        let Ok(page) = self.page(page) else { return vec![] };
        let glob = if units > 1 { page.work_units().get(unit).map(|u| u.owned_glob.clone()) } else { None };
        let glob = glob.unwrap_or_else(|| page.owned_glob.clone());
        let Ok(glob) = globset::Glob::new(&glob) else { return vec![] };
        let matcher = glob.compile_matcher();
        let locked = self.locked_files();
        let files = git::tracked_files(cwd).unwrap_or_default();
        files.into_iter().filter(|f| matcher.is_match(f) && !locked.contains(f)).take(40).collect()
    }

    /// One agent's whole context: its task, the files it owns, the contracts and Dictionary
    /// entries it uses, in full, and when it is done. Nothing else from the repository.
    fn worker_brief(&self, page_id: &str, unit: usize, units: usize, tier: Tier, errors: Option<&str>, owned: &[String]) -> anyhow::Result<String> {
        let page = self.page(page_id)?.clone();
        let contracts = &self.plan()?.contracts;
        let cites = if page.cites.is_empty() { "none".to_string() } else { page.cites.join(", ") };
        // Contracts are locked and short; giving them in full saves a read per file.
        let inline = contracts.iter().map(|c| c.content.len()).sum::<usize>() <= CONTRACTS_INLINE;
        let contract_list = if contracts.is_empty() {
            "- none\n".to_string()
        } else if inline {
            contracts.iter().map(|c| format!("- `{}` (locked, do not edit):\n```\n{}\n```\n", c.path, c.content.trim_end())).collect()
        } else {
            contracts.iter().map(|c| format!("- `{}`\n", c.path)).collect()
        };
        let split = if units > 1 { page.work_units().get(unit).cloned() } else { None };
        let mut brief = format!(
            "Build page `{id}` for route `{route}`.\n\
             You may create or edit only files matching `{glob}`. Files outside it are discarded and the page fails its check.\n\
             Cite only these Dictionary keys, and follow them: {cites}.\n\
             Contracts{given} in the tree:\n{contract_list}\n\
             {body}\n\
             File paths in this brief and in Dictionary entries are from the project root; a link or import inside a file \
             is relative to that file (from app/index.html, app/styles.css is `styles.css`).\n\
             If a Dictionary entry is wrong, stop and reply with {{\"flag\":{{\"key\":\"...\",\"issue\":\"...\"}}}} and do not edit other pages.\n",
            id = page.id,
            route = page.route,
            glob = split.as_ref().map_or(page.owned_glob.as_str(), |u| u.owned_glob.as_str()),
            body = page.brief,
            given = if inline && !contracts.is_empty() { ", given in full and already" } else { " already" },
        );
        if owned.is_empty() {
            brief.push_str("None of your files exist yet: create them.\n");
        } else {
            brief.push_str(&format!("Your files that exist now: {}.\n", owned.iter().map(|f| format!("`{f}`")).collect::<Vec<_>>().join(", ")));
        }
        if let Some(mine) = &split {
            brief.push_str(&format!(
                "\nThis page is split across {units} parallel workers who each own different files. You are unit `{}`: build only {}. {}\n",
                mine.id, mine.owned_glob, mine.brief
            ));
            for other in page.work_units().iter().filter(|u| u.id != mine.id) {
                brief.push_str(&format!("Unit `{}` (another worker) owns `{}`.\n", other.id, other.owned_glob));
            }
            brief.push_str("Rely only on the locked contracts and Dictionary entries to line up with them; do not write their files.\n");
        }
        if let Some(errors) = errors {
            brief.push_str("\nChecks failed last time. Fix these and leave every other page alone:\n");
            brief.push_str(errors);
            brief.push('\n');
            brief.push_str(&self.handoff(page_id, tier)?);
        }
        let model = self.model_byline(tier);
        let book = self.records.book()?;
        if let Some(errors) = errors
            && let Some(fix) = book.fix_for(&model, errors) {
                brief.push_str("\nKnown fix for this model:\n");
                brief.push_str(fix);
                brief.push('\n');
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
        // A small or split build has its contracts settled by the plan: Commons suggestions
        // would only be more to read.
        let suggest = !self.small && split.is_none();
        if suggest {
            brief.push_str("\nSearch the Commons before inventing a new contract.\n");
        }
        let query = format!("{} {}", page.brief, page.cites.join(" "));
        let mut shown = 0;
        for entry in catalog.search(&query)?.into_iter().filter(|_| suggest) {
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
        if !page.cites.is_empty() {
            // Each lookup is another model turn; the entries are already here.
            brief.push_str("\nThe cited entries above are complete: do not look them up again with the Dictionary tools.\n");
        }
        let design = self.plan()?.design.trim().to_string();
        if !design.is_empty() {
            brief.push_str(&format!(
                "\nDesign spec, shared by every agent of this run. Follow it exactly; do not invent another look:\n{design}\n"
            ));
            brief.push_str(crate::small::QUALITY_BARS);
        }
        let done = split.as_ref().map_or(page.done.as_str(), |u| u.done.as_str()).trim();
        let done = if done.is_empty() { tm_core::DEFAULT_DONE } else { done }.trim_end_matches('.');
        if design.is_empty() {
            brief.push_str(&format!("\nYou are done when: {done}.\n"));
        } else {
            brief.push_str(&format!("\nYou are done when: {done}, and your files meet the design spec and the quality bars.\n"));
        }
        let agent = match &split {
            Some(mine) => format!("page:{}/{}", page.id, mine.id),
            None => format!("page:{}", page.id),
        };
        brief.push_str(&format!(
            "Then stop: end your reply with the line `{RELEASE} {agent}` to report done and ask Taskmaster to release \
             you and anything you started. Start no background processes.\n"
        ));
        brief.push_str(if errors.is_some() {
            "This brief is your whole context. Read only the files you own that you are fixing; do not list or search the directory.\n"
        } else if inline || contracts.is_empty() {
            "This brief is your whole context: do not list, search or read other files. Write your files directly.\n"
        } else {
            "This brief is your whole context: read only the contracts you use, do not list or search the directory, then write your files.\n"
        });
        Ok(brief)
    }

    /// What earlier attempts at this page did, for the one taking over.
    fn handoff(&self, page: &str, tier: Tier) -> anyhow::Result<String> {
        let attempts = self.records.attempts(&self.id, page)?;
        if attempts.is_empty() {
            return Ok(String::new());
        }
        let mut out = String::from("\nWhat was tried on this page before, latest last:\n");
        let shown = &attempts[attempts.len().saturating_sub(2)..];
        for attempt in shown {
            out.push_str(&format!("\n- {} ({}) — {}.", attempt.model, tier_name(attempt.tier), attempt.outcome));
            if !attempt.files.is_empty() {
                out.push_str(&format!(" Changed {}.", attempt.files.join(", ")));
            }
            out.push('\n');
            if let Some(fixing) = &attempt.fixing {
                out.push_str(&format!("  It was fixing: {}\n", first_lines(fixing, 6)));
            }
            if let Some(failure) = &attempt.failure {
                out.push_str(&format!("  Why it was not kept: {}\n", first_lines(failure, 8)));
            }
            if !attempt.said.is_empty() {
                out.push_str(&format!("  What it said when it finished: {}\n", first_lines(&attempt.said, 12)));
            }
            if let Some(diff) = &attempt.diff {
                out.push_str("  Its change, which is not in your files:\n```diff\n");
                out.push_str(diff);
                out.push_str("\n```\n");
            }
        }
        if let Some(last) = shown.last() {
            if last.outcome == "merged" {
                out.push_str("\nThe last attempt's work is already in your files. Build on it; do not start over.\n");
            }
            if last.tier != tier {
                out.push_str("\nYou are taking this page over from a cheaper model. Its notes above say where it got stuck.\n");
            }
        }
        Ok(out)
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
            effort: None,
            dictionary: Some(self.access(token)),
            timeout: self.config.timeout,
            stall: None,
            read_only: true,
            feed: None,
            browser: None,
            code_mode: false,
            commit: None,
        })
    }

    /// Send this call's events to the run's agent transcript, under `agent`.
    fn tag(&self, request: &mut AgentRequest, agent: &str, role: &str, page: Option<&str>, glob: Option<&str>) {
        let owned_glob = glob.map(str::to_string).or_else(|| page.and_then(|id| self.page(id).ok()).map(|page| page.owned_glob.clone()));
        let tag = serde_json::json!({
            "id": agent,
            "role": role,
            "page": page,
            "harness": request.harness.id(),
            "model": request.model,
            "owned_glob": owned_glob,
            "worktree": request.cwd.display().to_string(),
            "branch": self.branch,
            "call": self.live.next_call(),
        });
        request.feed = Some(Feed { live: self.live.clone(), run: self.id.clone(), agent: tag });
        request.stall = Some(if self.small { SMALL_STALL } else { STALL });
    }

    /// Notes addressed to the master or to every worker. The master reads them and the
    /// decision stays on later worker briefs. A note addressed to this page is read only
    /// on a repair, and only that repair's briefs carry it.
    async fn read_notes_before_workers(&mut self, page: &str, repair: bool) -> anyhow::Result<()> {
        self.repair_decision.clear();
        // A follow-up is one master turn over every note still waiting, including one
        // addressed to a single page, before any worker starts.
        let reopen = self.opened_by_note(page);
        if let Some(text) = self.consult(|note| {
            reopen || note.to == "master" || note.to == "workers"
        }).await? {
            if !self.handed.is_empty() {
                self.handed.push('\n');
            }
            self.handed.push_str(text.trim());
            self.handed.push('\n');
        }
        if !repair {
            return Ok(());
        }
        let page_key = format!("page:{page}");
        let page_unit = format!("page:{page}/");
        if let Some(text) = self.consult(|note| note.to == page_key || note.to.starts_with(&page_unit)).await? {
            self.repair_decision = text;
        }
        Ok(())
    }

    /// The master reads matching notes. The returned lines are what the next agent follows.
    async fn decision_for(&mut self, pred: impl Fn(&crate::live::Note) -> bool) -> anyhow::Result<Vec<String>> {
        Ok(self.consult(pred).await?.into_iter().collect())
    }

    /// One master turn over the notes `pred` selects. Nothing is asked when there are none.
    async fn consult(&mut self, pred: impl Fn(&crate::live::Note) -> bool) -> anyhow::Result<Option<String>> {
        let notes = self.live.take_fresh_where(&self.id, pred);
        if notes.is_empty() {
            return Ok(None);
        }
        let mut brief = String::from(
            "Read these notes and decide what the agents should do next. Reply in plain text. Do not edit files.\n\n",
        );
        for note in &notes {
            brief.push_str(&format!("- to {}: {}\n", note.to, note.text.trim()));
        }
        let worktree = self.fresh_worktree("notes")?;
        let mut request = self.read_request(
            &worktree,
            brief,
            self.harnesses.master.clone(),
            self.config.models.master.clone(),
            &self.master_token.clone(),
        )?;
        self.tag(&mut request, "master", "master", None, None);
        let bill = request.bill(Role::Master, None);
        let asked = ask(request).await;
        self.drop_worktree(&worktree);
        let answer = asked?;
        self.charge(&bill, &answer)?;
        if !answer.ok {
            bail!("the master did not finish reading the notes");
        }
        Ok(Some(answer.text))
    }

    /// A brief with the notes a person queued for this agent.
    fn with_notes(&self, mut brief: String, notes: &[String]) -> String {
        if notes.is_empty() {
            return brief;
        }
        brief.push_str("\nNotes from the person running Taskmaster:\n");
        for note in notes {
            brief.push_str(&format!("- {}\n", note.trim()));
        }
        brief
    }

    /// A line in the run's terminal transcript: a command the daemon ran, and what it said.
    fn terminal(&self, kind: &str, fields: serde_json::Value) {
        let agent = serde_json::json!({ "id": "runtime", "role": "runtime" });
        self.live.push(&self.id, &agent, kind, fields);
    }

    fn access(&self, token: &str) -> DictionaryAccess {
        DictionaryAccess { name: "dictionary".into(), url: self.url.clone(), bearer_token: token.to_string() }
    }

    fn grant_pages(&mut self) -> anyhow::Result<()> {
        let pages: Vec<String> =
            self.run.plan.as_ref().map(|plan| plan.pages.iter().map(|p| p.id.clone()).collect()).unwrap_or_default();
        for page in pages {
            if self.tokens.contains_key(&page) {
                continue;
            }
            let token = fresh_token()?;
            self.mcp.grant(&token, Grant { run: self.config.run_id.clone(), page: page.clone() });
            self.tokens.insert(page, token);
        }
        Ok(())
    }

    /// Record one agent call in the run's ledger.
    fn charge(&self, bill: &Bill, answer: &Answer) -> anyhow::Result<()> {
        let entry = LedgerEntry {
            at: rfc3339_now(),
            role: bill.role,
            page: bill.page.clone(),
            harness: bill.harness.clone(),
            model: bill.model.clone(),
            input: answer.usage.input,
            output: answer.usage.output,
            cache_read: answer.usage.cache_read,
            cache_write: answer.usage.cache_write,
            cost_usd: answer.cost_usd,
            seconds: answer.seconds,
            ok: answer.ok,
        };
        self.records.charge(&self.id, &entry)
    }

    /// Replace the plan's estimate with one worked out from what this project's earlier
    /// runs cost, plus what planning this one has already cost. A planner cannot know how
    /// much each harness adds to every call, so its own number is not used.
    fn with_forecast(&self, mut plan: Plan) -> anyhow::Result<Plan> {
        let mut history = Vec::new();
        for run in self.records.run_ids()? {
            if run != self.id {
                history.extend(self.records.ledger(&run)?);
            }
        }
        // Review is on demand, so a run's own cost is only its builds.
        let (mut estimate, basis) = forecast(&history, plan.pages.len(), 0);
        let planning = totals(&self.records.ledger(&self.id)?);
        estimate.tokens += planning.fresh;
        estimate.usd += planning.cost_usd;
        let source = if basis == 0 { "no earlier runs here, so from defaults".to_string() } else { format!("from {basis} earlier calls") };
        eprintln!("taskmaster: estimated {} tokens, ${:.2} ({source})", estimate.tokens, estimate.usd);
        plan.estimate = estimate;
        Ok(plan)
    }

    fn spend_line(&self) -> anyhow::Result<String> {
        let sum = totals(&self.records.ledger(&self.id)?);
        let mut line = format!("{} agent calls, {} tokens", sum.calls, sum.fresh);
        line.push_str(&format!(" ({} in, {} out", sum.input, sum.output));
        if sum.cache_write > 0 {
            line.push_str(&format!(", {} written to cache", sum.cache_write));
        }
        line.push(')');
        if sum.cache_read > 0 {
            line.push_str(&format!(" plus {} read from cache", sum.cache_read));
        }
        if sum.unpriced_calls < sum.calls {
            line.push_str(&format!(", ${:.2}", sum.cost_usd));
            if sum.unpriced_calls > 0 {
                let calls = if sum.unpriced_calls == 1 { "call" } else { "calls" };
                line.push_str(&format!(" ({} {calls} reported no price)", sum.unpriced_calls));
            }
        }
        if let Some(plan) = &self.run.plan {
            line.push_str(&format!("; the plan estimated {} tokens", plan.estimate.tokens));
        }
        Ok(line)
    }

    /// Bring in what other people published. A remote that cannot be reached is not a
    /// reason to stop: the local Commons still works.
    fn pull_commons(&self) {
        let Some(remote) = &self.remote else { return };
        match LocalStore::open(&self.commons).and_then(|commons| remote.pull(&commons)) {
            Ok(0) => {}
            Ok(n) => eprintln!("taskmaster: {n} Commons entries arrived"),
            Err(err) => eprintln!("taskmaster: the Commons was not synced: {err:#}"),
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
        match git::commit_paths(&self.tree, message, &paths)? {
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
                let _ = git::git(&self.tree, &["checkout", "--", ARCHITECTURE_FILE, CHANGES_FILE]);
                Ok(None)
            }
        }
    }

    fn write_docs(&self) -> anyhow::Result<()> {
        if let Some(plan) = &self.run.plan {
            let path = self.tree.join(ARCHITECTURE_FILE);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, render_architecture(plan))?;
        }
        let path = self.tree.join(CHANGES_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, render_changes(&self.changes, 100))?;
        Ok(())
    }

    fn persist(&self) -> anyhow::Result<()> {
        let mut commands = self.running.clone();
        commands.extend(self.pending.iter().cloned());
        self.records.save_inflight(&self.id, &commands)
    }

    fn save_events(&self) -> anyhow::Result<()> {
        self.records.save_events(&self.id, &self.events)
    }

    fn save_changes(&self) -> anyhow::Result<()> {
        self.records.save_changes(&self.id, &self.changes)
    }

    fn write_status(&self, ok: bool) -> anyhow::Result<()> {
        let spent = totals(&self.records.ledger(&self.id)?);
        let estimate = self.run.plan.as_ref().map(|plan| &plan.estimate);
        let mut body = serde_json::json!({ "ok": ok, "phase": self.run.phase, "spent": spent, "estimate": estimate });
        match &self.delivery {
            Some((branch, merged)) => {
                body["branch"] = serde_json::json!(branch);
                body["merged"] = serde_json::json!(merged);
                if let Some((base, commit)) = &self.merge_commit {
                    body["base"] = serde_json::json!(base);
                    body["merge_commit"] = serde_json::json!(commit);
                }
            }
            // A finished run that is driven again has nothing new to deliver.
            None => {
                let before = self.records.status(&self.id)?.unwrap_or_default();
                for key in ["branch", "merged", "base", "merge_commit"] {
                    if let Some(value) = before.get(key) {
                        body[key] = value.clone();
                    }
                }
            }
        }
        self.records.save_status(&self.id, &body)
    }

    fn fresh_worktree(&mut self, label: &str) -> anyhow::Result<PathBuf> {
        if !git::has_head(&self.tree) {
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
        let head = git::head(&self.tree)?;
        git::add_worktree(&self.project, &path, &head)?;
        self.trees.push(path.clone());
        Ok(path)
    }

    fn drop_worktree(&mut self, path: &Path) {
        let _ = git::remove_worktree(&self.project, path);
        let _ = fs::remove_dir_all(path);
        self.trees.retain(|have| have != path);
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
        out.push_str(&agent_lines(plan));
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
        let _project = PROJECT.lock().unwrap();
        let mut book = self.records.book()?;
        book.record(model, failure);
        self.records.save_book(&book)
    }

    fn learn_fix(&self, tier: Tier, failure: &str, text: &str) -> anyhow::Result<()> {
        let model = self.model_byline(tier);
        let _project = PROJECT.lock().unwrap();
        let mut book = self.records.book()?;
        book.learn_fix(&model, failure, text);
        self.records.save_book(&book)
    }

    /// True when a follow-up reopened this page and it has not been built since.
    fn opened_by_note(&self, page: &str) -> bool {
        let mut opened = false;
        for event in &self.events {
            match event {
                Event::FollowUp => opened = true,
                Event::WorkerFinished { page: id, .. } | Event::PageFresh { page: id } if id == page => opened = false,
                _ => {}
            }
        }
        opened
    }

    fn citations_fresh(&self, page: &str) -> anyhow::Result<bool> {
        if !self.config.incremental {
            return Ok(false);
        }
        let citations = self.records.citations()?;
        let cites = self.page(page)?.cites.clone();
        let catalog = self.catalog()?;
        let mut current = BTreeMap::new();
        for key in &cites {
            let hash = catalog.resolve(key)?.map(|entry| entry.hash).unwrap_or_default();
            current.insert(key.clone(), hash);
        }
        Ok(page_is_fresh(&cites, citations.pages.get(page).map(Vec::as_slice), &current))
    }

    fn record_citations(&mut self, page: &str) -> anyhow::Result<()> {
        if !self.config.incremental {
            return Ok(());
        }
        let _project = PROJECT.lock().unwrap();
        let cites = self.page(page)?.cites.clone();
        let catalog = self.catalog()?;
        let mut recorded = Vec::new();
        for key in &cites {
            let hash = catalog.resolve(key)?.map(|entry| entry.hash).unwrap_or_default();
            recorded.push(Citation { key: key.clone(), hash });
        }
        if self.branch.is_some() {
            self.staged.insert(page.to_string(), recorded);
            return Ok(());
        }
        let mut citations = self.records.citations()?;
        citations.pages.insert(page.to_string(), recorded);
        self.records.save_citations(&citations)
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
                    if let Some(remote) = &self.remote
                        && let Err(err) = remote.report(&entry, passed)
                    {
                        eprintln!("taskmaster: {err:#}");
                    }
                } else if passed {
                    catalog.promote(key)?;
                    catalog.note_result(key, true)?;
                    self.push_commons(&catalog, key);
                }
            }
        }
        Ok(())
    }
}

impl Driver {
    /// Check the run's branch out in a directory of its own, so the project's working tree
    /// is not touched until the run is over. A run with no branch builds in the project.
    fn open_tree(&mut self, base: Option<&str>) -> anyhow::Result<()> {
        let Some(branch) = self.branch.clone() else { return Ok(()) };
        if !git::branch_exists(&self.project, &branch) {
            let followed_up = self.events.iter().any(|event| matches!(event, Event::FollowUp));
            if followed_up || matches!(self.run.phase, RunPhase::Done | RunPhase::Failed) {
                // Delivered and tidied away on an earlier pass. A follow-up continues
                // on the project as it stands, rather than recreating the old branch.
                self.branch = None;
                return Ok(());
            }
        }
        let path = git::worktree_dir(&self.project, &self.id, "tree");
        if path.exists() {
            let _ = git::remove_worktree(&self.project, &path);
            let _ = fs::remove_dir_all(&path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let base = match base {
            Some(base) => base.to_string(),
            None => git::head(&self.project)?,
        };
        git::open_branch(&self.project, &path, &branch, &base)?;
        self.trees.push(path.clone());
        // The project's installed packages are what its typecheck and linters run from.
        // They are not in git, so the checkout borrows them.
        git::borrow_node_modules(&self.project, &path);
        self.tree = path;
        Ok(())
    }

    /// The pages are in the project now, so a later run may skip the unchanged ones.
    fn flush_citations(&mut self) -> anyhow::Result<()> {
        if self.staged.is_empty() {
            return Ok(());
        }
        let _project = PROJECT.lock().unwrap();
        let mut citations = self.records.citations()?;
        citations.pages.append(&mut self.staged);
        self.records.save_citations(&citations)
    }

    /// Hand the finished branch to the project. A run that failed, or one git cannot merge
    /// cleanly, stays on its branch and the project is left as it was.
    fn deliver(&mut self, ok: bool) {
        let Some(branch) = self.branch.clone() else { return };
        if !git::branch_exists(&self.project, &branch) {
            return;
        }
        if !ok || !self.config.merge {
            let why = if ok { "was not merged" } else { "failed" };
            eprintln!("taskmaster: run {} {why}; what it built is on branch `{branch}`", self.id);
            self.delivery = Some((branch, false));
            return;
        }
        let merged = {
            let _project = PROJECT.lock().unwrap();
            git::merge_branch(&self.project, &branch, &[ARCHITECTURE_FILE, CHANGES_FILE])
        };
        if merged {
            let tree = self.tree.clone();
            self.drop_worktree(&tree);
            self.tree = self.project.clone();
            if let Ok(tip) = git::git(&self.project, &["rev-parse", "--verify", &format!("{branch}^{{commit}}")])
                && let Err(err) = self.live.set_tip(&self.id, &tip)
            {
                eprintln!("taskmaster: the branch tip was not kept, so this run's diff needs the branch: {err:#}");
            }
            let base = git::git(&self.project, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
            let commit = git::git(&self.project, &["rev-parse", "--short=8", "HEAD"]).unwrap_or_default();
            if !commit.trim().is_empty() {
                self.merge_commit = Some((base.trim().to_string(), commit.trim().to_string()));
            }
            let _ = git::git(&self.project, &["branch", "-D", &branch]);
            self.branch = None;
            eprintln!("taskmaster: merged `{branch}` into the project");
            if let Err(err) = self.flush_citations() {
                eprintln!("taskmaster: citations were not recorded, so these pages rebuild next time: {err:#}");
            }
        } else {
            eprintln!(
                "taskmaster: `{branch}` does not merge cleanly into the project, so it was left alone. \
                 Merge it yourself: git merge {branch}"
            );
        }
        self.delivery = Some((branch, merged));
    }

    /// Offer a promoted entry to the shared Commons. Promotion already happened here, so
    /// a failed push is only reported.
    fn push_commons(&self, catalog: &Catalog, key: &str) {
        let Some(remote) = &self.remote else { return };
        let pushed = catalog.commons.get(key).and_then(|entry| match entry {
            Some(entry) => remote.push(&entry),
            None => Ok(()),
        });
        if let Err(err) = pushed {
            eprintln!("taskmaster: `{key}` was not sent to the Commons: {err:#}");
        }
    }
}

/// The agents a plan starts, one line each: who, what files, what to do, when it is done.
fn agent_lines(plan: &Plan) -> String {
    let agents = plan.agents();
    let mut out = format!("plan: {} agent{}\n", agents.len(), if agents.len() == 1 { "" } else { "s" });
    for agent in &agents {
        out.push_str(&format!(
            "  {} owns `{}`: {} | done when {}\n",
            agent.id,
            agent.owned_glob,
            clip(agent.task.lines().next().unwrap_or_default(), 160),
            clip(agent.done.lines().next().unwrap_or_default(), 160)
        ));
    }
    if !plan.design.trim().is_empty() {
        out.push_str(&format!("  design: {}\n", clip(&plan.design.split_whitespace().collect::<Vec<_>>().join(" "), 240)));
    }
    out
}

fn role_label(role: &str) -> &'static str {
    match role {
        "planner" | "master" | "arbiter" => "Master",
        "worker" => "Worker",
        "mid" => "Mid",
        "reviewer" | "security" => "Reviewer",
        "verifier" => "Verifier",
        _ => "Agent",
    }
}

fn reported_limit(request: &AgentRequest, hit: reply::ProviderLimit) -> reply::ReportedLimit {
    let role = request
        .feed
        .as_ref()
        .and_then(|feed| feed.agent["role"].as_str())
        .map(role_label)
        .unwrap_or("Agent");
    reply::ReportedLimit::new(request.harness.id(), role, hit)
}

fn failed(errors: impl Into<String>) -> WorkerOutcome {
    WorkerOutcome::CheckFailed { errors: errors.into() }
}

/// The line a worker ends with to report done and ask to be released.
const RELEASE: &str = "RELEASE";

/// Contracts up to this many bytes in total are given in full in each worker's brief.
const CONTRACTS_INLINE: usize = 12_000;

/// A model normally starts answering a small, low-effort brief within half a minute; a
/// minute of silence with no tool running means the provider has queued the request.
const SMALL_STALL: Duration = Duration::from_secs(60);
/// Larger briefs at default effort can think for a while before the first token.
const STALL: Duration = Duration::from_secs(180);
/// Fresh agents started after a stall. The last one may wait for the whole timeout.
const STALL_RESTARTS: usize = 2;

/// Run one agent. One that stalls is stopped and replaced by a fresh agent with the same brief.
async fn ask(mut request: AgentRequest) -> anyhow::Result<Answer> {
    for restart in 0..=STALL_RESTARTS {
        if restart == STALL_RESTARTS {
            request.stall = None;
        }
        match ask_once(&request).await {
            Err(err) if restart < STALL_RESTARTS && err.downcast_ref::<tm_harness::Stalled>().is_some() => {
                let who = request.feed.as_ref().and_then(|f| f.agent["id"].as_str().map(str::to_string)).unwrap_or_else(|| "an agent".into());
                eprintln!("taskmaster: {who} stalled ({err}); starting a fresh agent");
                if let Some(feed) = &request.feed {
                    feed.live.push(&feed.run, &feed.agent, "stalled", serde_json::json!({ "error": format!("{err:#}"), "restart": restart + 1 }));
                }
            }
            Err(err) => {
                if let Some(hit) = reply::limit_of(&format!("{err:#}")) {
                    return Err(reported_limit(&request, hit).into());
                }
                return Err(err);
            }
            Ok(answer) => {
                if let Some(hit) = reply::limit_of(&answer.text) {
                    return Err(reported_limit(&request, hit).into());
                }
                return Ok(answer);
            }
        }
    }
    unreachable!("the last attempt always returns")
}

/// The gate an agent's tool calls (and its run_code calls) are put to.
fn gate_for(request: &AgentRequest) -> Arc<dyn PermissionGate> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut gate = WorktreeGate::new(&request.cwd, dict_prefixes());
    if request.read_only {
        gate = gate.read_only();
    }
    if let Some(origin) = &request.browser {
        gate = gate.with_browser(origin.clone());
    }
    for dir in HARNESS_STATE_DIRS {
        gate = gate.with_readable(format!("{home}/{dir}"));
    }
    // With a policy that asks, risky calls wait for a person; otherwise the gate stands alone.
    match &request.feed {
        Some(feed) => Arc::new(ApprovalGate::new(gate, feed.live.clone(), feed.run.clone(), feed.agent.clone())),
        None => Arc::new(gate),
    }
}

async fn ask_once(request: &AgentRequest) -> anyhow::Result<Answer> {
    let gate = gate_for(request);
    let task = WorkerTask {
        brief: request.brief.clone(),
        cwd: request.cwd.clone(),
        model: request.model.clone(),
        effort: request.effort.clone(),
        dictionary: request.dictionary.clone(),
        timeout: request.timeout,
        stall: request.stall,
        read_only: request.read_only,
        code_mode: request.code_mode,
        shell: None,
    };
    let feed = &request.feed;
    if let Some(feed) = &feed {
        feed.live.push(&feed.run, &feed.agent, "start", serde_json::json!({ "brief": clip(&task.brief, 4000) }));
    }
    let (tx, mut rx) = mpsc::channel(256);
    let tee = feed.as_ref().map(|feed| (feed.live.clone(), feed.run.clone(), feed.agent.clone()));
    // What a commit budget watches: tool calls, whether the reply has begun, the thoughts.
    let watch = Arc::new(std::sync::Mutex::new(Watched::default()));
    let watching = request.commit.is_some().then(|| watch.clone());
    let collect = tokio::spawn(async move {
        // The answer is only the text. Thoughts and tool calls go to the transcript.
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            if let Some(watch) = &watching {
                let mut seen = watch.lock().unwrap();
                match &event {
                    HarnessEvent::ToolCall { .. } => seen.tools += 1,
                    // The plan is a JSON object: its first brace is the reply beginning.
                    HarnessEvent::Text { text: chunk } => seen.writing |= chunk.contains('{'),
                    HarnessEvent::Thought { text: chunk } => {
                        seen.thoughts.push_str(chunk);
                        if seen.thoughts.len() > 2 * OVERRUN_NOTES {
                            let cut = seen.thoughts.len() - OVERRUN_NOTES;
                            let cut = (cut..seen.thoughts.len()).find(|&i| seen.thoughts.is_char_boundary(i)).unwrap_or(0);
                            seen.thoughts.drain(..cut);
                        }
                    }
                    _ => {}
                }
            }
            if let Some((live, run, agent)) = &tee {
                match &event {
                    HarnessEvent::Text { text: chunk } => live.push(run, agent, "text", serde_json::json!({ "text": chunk })),
                    HarnessEvent::Thought { text: chunk } => {
                        live.push(run, agent, "thought", serde_json::json!({ "text": chunk }))
                    }
                    HarnessEvent::ToolCall { id, name, input } => live.push(
                        run,
                        agent,
                        "tool_call",
                        serde_json::json!({ "id": id, "name": name, "target": tool_target(input), "input": input }),
                    ),
                    HarnessEvent::PermissionDecided { tool, allowed } => {
                        live.push(run, agent, "permission", serde_json::json!({ "tool": tool, "allowed": allowed }))
                    }
                    HarnessEvent::ToolResult { id, text, diff } => {
                        let mut fields = serde_json::json!({ "id": id, "text": text });
                        if let Some(diff) = diff {
                            fields["diff"] = serde_json::json!(diff);
                        }
                        live.push(run, agent, "output", fields);
                    }
                }
            }
            if let HarnessEvent::Text { text: chunk } = event
                && text.len() < 1_000_000 {
                    text.push_str(&chunk);
                }
        }
        text
    });
    let started = Instant::now();
    let run = match request.commit {
        None => request.harness.run(task, gate, tx).await,
        Some(commit) => {
            let seen = watch.clone();
            let overrun = async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    let seen = seen.lock().unwrap();
                    if seen.tools > commit.tools {
                        return format!("it had made {} tool calls", seen.tools);
                    }
                    if !seen.writing && started.elapsed() >= commit.after {
                        return format!("it had not started the plan after {} seconds", commit.after.as_secs());
                    }
                }
            };
            // Dropping the harness's run stops the agent and everything it started.
            tokio::select! {
                run = request.harness.run(task, gate, tx) => run,
                why = overrun => Err(anyhow::Error::new(Overrun { why, notes: String::new() })),
            }
        }
    };
    let text = collect.await.unwrap_or_default();
    let run = run.map_err(|err| match err.downcast::<Overrun>() {
        Ok(over) => {
            let notes = tail_chars(&watch.lock().unwrap().thoughts, OVERRUN_NOTES);
            anyhow::Error::new(Overrun { notes, ..over })
        }
        Err(err) => err,
    });
    if let Some(feed) = &feed {
        let mut end = serde_json::json!({ "seconds": started.elapsed().as_secs_f64() });
        match &run {
            Ok(summary) => {
                end["ok"] = serde_json::json!(summary.ok);
                end["denied"] = serde_json::json!(summary.denied);
                end["cost_usd"] = serde_json::json!(summary.cost_usd);
                let usage = &summary.usage;
                // Only a provider that reports usage gets a meter.
                if usage.input + usage.output + usage.cache_read + usage.cache_write > 0 {
                    end["usage"] = serde_json::json!(usage);
                }
            }
            Err(err) => {
                end["ok"] = serde_json::json!(false);
                end["error"] = serde_json::json!(format!("{err:#}"));
            }
        }
        feed.live.push(&feed.run, &feed.agent, "end", end);
        if let Ok(RunSummary { freed: Some(freed), .. }) = &run {
            let id = feed.agent["id"].as_str().unwrap_or("agent");
            let asked = text.lines().any(|line| line.trim().trim_matches('`') == format!("{RELEASE} {id}"));
            feed.live.push(&feed.run, &feed.agent, "released", serde_json::json!({ "asked": asked, "processes": freed.processes, "lingering": freed.lingering }));
            if id.starts_with("page:") {
                let how = if asked { "it reported done" } else { "its turn ended" };
                let left = if freed.lingering == 0 { "nothing left running".to_string() } else { format!("{} still running", freed.lingering) };
                eprintln!("taskmaster: released {id} ({how}; freed {} process{}, {left})", freed.processes, if freed.processes == 1 { "" } else { "es" });
            }
        }
    }
    let summary = run?;
    Ok(Answer {
        text,
        ok: summary.ok,
        usage: summary.usage,
        cost_usd: summary.cost_usd,
        seconds: started.elapsed().as_secs_f64(),
    })
}

fn dict_prefixes() -> Vec<String> {
    ["dict__", "dictionary__", "dictionary_", "mcp__dictionary", "mcp_dictionary_"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// A Dictionary token. Any process on this machine can reach the server, so the token
/// must not be guessable.
fn fresh_token() -> anyhow::Result<String> {
    random_hex(24)
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

/// The settings fixed when the run first started. A resumed run keeps them.
fn load_meta(records: &Records, project: &Path, config: &Config, resuming: bool) -> anyhow::Result<Meta> {
    let requested = config.parallel.max(1);
    if let Some(meta) = records.meta(&config.run_id)? {
        if resuming && meta.parallel.max(1) != requested {
            eprintln!("taskmaster: resuming with parallel {} from the run record", meta.parallel.max(1));
        }
        return Ok(meta);
    }
    // A repository with no commit has nothing to branch from, so its first run builds in place.
    let rooted = git::has_head(project);
    let slug: String =
        config.run_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let meta = Meta {
        parallel: requested,
        branch: rooted.then(|| format!("taskmaster/{slug}")),
        base: match (&config.base, rooted) {
            (Some(base), true) => Some(base.clone()),
            (None, true) => Some(git::head(project)?),
            (_, false) => None,
        },
        template: config.template.clone(),
    };
    if let Some(name) = &meta.template {
        template::load(name)?;
    }
    records.save_meta(&config.run_id, &meta)?;
    Ok(meta)
}
