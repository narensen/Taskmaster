//! The daemon against a fake harness. No model is started.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use tm_core::{
    Book, ContractFile, DictTier, EntryKind, EntryStats, Estimate, Event, Mistake, PagePlan, Plan, Run, RunPhase,
    PROMOTE_AT,
};
use tm_dictionary::LocalStore;
use tm_harness::{Harness, HarnessEvent, PermissionGate, RunSummary, Usage, WorkerTask};
use tokio::sync::{Barrier, Notify, mpsc, watch};

use crate::{Config, Harnesses, Outcome, drive, evaluate, records, run_directory};

enum Mode {
    Write,
    Outside,
    Conform,
    Amend,
    RejectFinding,
    ConfirmFinding,
    CrossImport,
    /// `Some` waits until that many workers are inside the build at once.
    Parallel(Option<Arc<Barrier>>),
    Cancel(watch::Sender<bool>),
    Boom,
    /// Every attempt produces no files, so the page fails.
    Nothing,
    /// Page `a` fails three times, then passes with a fix the later page should see.
    Learn,
    /// Writes bytes that differ from a page already committed at HEAD.
    Rebuild,
    /// The first reply is a usable plan.
    Plan,
    /// The first reply cites a missing key. The second reply defines it.
    PlanFix,
    /// The first reply rewrites an existing entry. The second cites it unchanged.
    PlanClash,
    /// Every reply has no pages.
    PlanBad,
}

struct World {
    mode: Mode,
    calls: Mutex<Vec<String>>,
    plans: AtomicUsize,
    a: AtomicUsize,
    b: AtomicUsize,
    reviews: AtomicUsize,
    inflight: AtomicUsize,
    max_inflight: AtomicUsize,
    b_done: Notify,
}

struct Script {
    text: String,
    files: Vec<(String, Option<String>)>,
}

impl Script {
    fn text(text: &str) -> Self {
        Script { text: text.into(), files: vec![] }
    }

    fn page(id: &str, body: &str) -> Self {
        Script { text: "done".into(), files: vec![(format!("app/{id}/page.ts"), Some(body.into()))] }
    }
}

struct Fake(Arc<World>);

#[async_trait]
impl Harness for Fake {
    fn id(&self) -> &'static str {
        "fake"
    }

    async fn run(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let script = self.0.respond(&task).await?;
        for (path, content) in script.files {
            let full = task.cwd.join(&path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match content {
                Some(text) => std::fs::write(full, text)?,
                None => {
                    let _ = std::fs::remove_file(full);
                }
            }
        }
        let _ = events.send(HarnessEvent::Text { text: script.text }).await;
        Ok(RunSummary { ok: true, usage: Usage::default(), cost_usd: None, denied: 0 })
    }
}

impl World {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(World {
            mode,
            calls: Mutex::new(Vec::new()),
            plans: AtomicUsize::new(0),
            a: AtomicUsize::new(0),
            b: AtomicUsize::new(0),
            reviews: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            max_inflight: AtomicUsize::new(0),
            b_done: Notify::new(),
        })
    }

    fn harnesses(self: &Arc<Self>) -> Harnesses {
        let harness: Arc<dyn Harness> = Arc::new(Fake(self.clone()));
        Harnesses {
            worker: harness.clone(),
            mid: harness.clone(),
            master: harness.clone(),
            reviewer: harness.clone(),
            verifier: harness,
        }
    }

    async fn respond(&self, task: &WorkerTask) -> anyhow::Result<Script> {
        let brief = task.brief.clone();
        self.calls.lock().unwrap().push(brief.clone());
        if brief.contains("Plan this product and lock its contracts.") {
            return Ok(Script::text(&self.plan_reply()));
        }
        if let Some(id) = brief.split("Build page `").nth(1).and_then(|rest| rest.split('`').next()) {
            return self.build(id).await;
        }
        if brief.contains("objected to Dictionary") {
            return self.arbitrate();
        }
        if brief.contains("Decide whether it is real") {
            let real = matches!(self.mode, Mode::ConfirmFinding);
            return Ok(Script::text(if real { "{\"real\": true}" } else { "{\"real\": false}" }));
        }
        if brief.contains("reviewing one connection") || brief.contains("security problems") {
            return Ok(self.review(&brief));
        }
        bail!("unexpected brief: {}", brief.chars().take(240).collect::<String>());
    }

    fn plan_reply(&self) -> String {
        let n = self.plans.fetch_add(1, Ordering::SeqCst);
        let greet = r#"{"key":"greet","kind":"hint","summary":"greeting","content":"export const greet = \"hi\";\n","stack":"next@15"}"#;
        match &self.mode {
            Mode::PlanFix if n == 0 => plan_json("greet", ""),
            Mode::PlanClash if n == 0 => plan_json(
                "theme",
                r#"{"key":"theme","kind":"hint","summary":"light","content":"export const theme = \"light\";\n","stack":"next"}"#,
            ),
            Mode::PlanClash => plan_json("theme", ""),
            Mode::PlanBad => {
                r#"{"contracts":[],"pages":[],"estimate":{"tokens":0,"usd":0},"dictionary":[]}"#.into()
            }
            _ => plan_json("greet", greet),
        }
    }

    async fn build(&self, id: &str) -> anyhow::Result<Script> {
        let n = match id {
            "a" => self.a.fetch_add(1, Ordering::SeqCst),
            "b" => self.b.fetch_add(1, Ordering::SeqCst),
            _ => 0,
        };
        if matches!(self.mode, Mode::Parallel(_)) {
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, Ordering::SeqCst);
            if let Mode::Parallel(Some(barrier)) = &self.mode {
                barrier.wait().await;
            } else {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            self.inflight.fetch_sub(1, Ordering::SeqCst);
        }
        match &self.mode {
            Mode::Boom if id == "b" => bail!("boom"),
            Mode::Cancel(tx) => {
                let _ = tx.send(true);
                Ok(Script::page(id, &format!("export const page = \"{id}\";\n")))
            }
            Mode::Outside if id == "a" && n == 0 => Ok(Script {
                text: "done".into(),
                files: vec![
                    ("app/a/page.ts".into(), Some("export const a = 1;\n".into())),
                    ("contracts/types.ts".into(), Some("hacked\n".into())),
                ],
            }),
            // The repair keeps the owned file. The contract edit from the first attempt stays out.
            Mode::Outside if id == "a" => Ok(Script::page(id, "export const a = 1;\n")),
            Mode::Conform if id == "a" && n == 0 => {
                Ok(Script::text("{\"flag\":{\"key\":\"auth\",\"issue\":\"missing role\"}}"))
            }
            Mode::Amend if id == "a" && n == 0 => {
                self.b_done.notified().await;
                Ok(Script::text("{\"flag\":{\"key\":\"auth\",\"issue\":\"missing role\"}}"))
            }
            Mode::Amend if id == "b" && n == 0 => {
                let script = Script::page(id, "export const b = 1;\n");
                self.b_done.notify_one();
                Ok(script)
            }
            Mode::CrossImport if id == "a" && n == 0 => {
                Ok(Script::page(id, "import { b } from \"../b/page\";\nexport const a = 1;\n"))
            }
            Mode::CrossImport if id == "a" => Ok(Script::page(id, "export const a = 2;\n")),
            Mode::ConfirmFinding if id == "a" && n >= 1 => Ok(Script::page(id, "export const a = 2;\n")),
            Mode::Nothing => Ok(Script::text("I made no edits to this page.")),
            Mode::Learn if id == "a" && n < 3 => Ok(Script::text("no files yet")),
            Mode::Learn if id == "a" => Ok(Script {
                text: "Read the route param before rendering the header.".into(),
                files: vec![("app/a/page.ts".into(), Some("export const a = 1;\n".into()))],
            }),
            Mode::Rebuild => Ok(Script::page(id, &format!("export const rebuilt = \"{id}\";\n"))),
            _ => Ok(Script::page(id, &format!("export const page = \"{id}\";\n"))),
        }
    }

    fn arbitrate(&self) -> anyhow::Result<Script> {
        let text = if matches!(self.mode, Mode::Amend) {
            "{\"resolution\":\"amended\",\"kind\":\"hint\",\"summary\":\"roles required\",\"content\":\"export const ROLES = ['admin'];\\n\",\"stack\":\"next@15\"}"
        } else {
            "{\"resolution\":\"conform\"}"
        };
        Ok(Script::text(text))
    }

    fn review(&self, brief: &str) -> Script {
        if brief.contains("security problems") {
            return Script::text("[]");
        }
        let n = self.reviews.fetch_add(1, Ordering::SeqCst);
        let finding = matches!(self.mode, Mode::RejectFinding | Mode::ConfirmFinding) && n == 0;
        if finding {
            Script::text(
                "[{\"page\":\"a\",\"file\":\"\",\"line\":0,\"failure\":\"no\"},\
                  {\"page\":\"a\",\"file\":\"app/a/page.ts\",\"line\":1,\"failure\":\"empty id renders a blank header for guests\"}]",
            )
        } else {
            Script::text("[]")
        }
    }
}

fn plan_json(cite: &str, entry: &str) -> String {
    format!(
        r#"{{"contracts":[{{"path":"contracts/types.ts","content":"export type Id = string;\n"}}],"pages":[{{"id":"a","route":"/a","owned_glob":"app/a/**","cites":["{cite}"],"brief":"Implement a."}}],"estimate":{{"tokens":4,"usd":0}},"dictionary":[{entry}]}}"#
    )
}

fn plan(pages: &[(&str, &[&str])]) -> Plan {
    Plan {
        contracts: vec![ContractFile { path: "contracts/types.ts".into(), content: "export type Id = string;\n".into() }],
        pages: pages
            .iter()
            .map(|(id, cites)| PagePlan {
                id: (*id).into(),
                route: format!("/{id}"),
                owned_glob: format!("app/{id}/**"),
                cites: cites.iter().map(|c| (*c).to_string()).collect(),
                brief: format!("Implement {id}."),
            })
            .collect(),
        estimate: Estimate { tokens: 10, usd: 0.01 },
    }
}

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn repo(name: &str) -> Tmp {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "tm-daemon-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.invalid"]);
    Tmp(dir)
}

fn git(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    assert!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
}

fn prompt_config(project: &Path, prompt: &str, yes: bool) -> Config {
    let mut config = Config::new(project);
    config.prompt = Some(prompt.into());
    config.yes = yes;
    config.parallel = 1;
    config.timeout = Duration::from_secs(30);
    config
}

fn config(project: &Path, plan: Plan, yes: bool, parallel: usize) -> Config {
    let mut config = Config::new(project);
    config.plan = Some(plan);
    config.yes = yes;
    config.parallel = parallel;
    config.timeout = Duration::from_secs(30);
    config
}

async fn run(config: Config, world: &Arc<World>) -> anyhow::Result<Outcome> {
    let harnesses = world.harnesses();
    match tokio::time::timeout(Duration::from_secs(20), drive(config, harnesses)).await {
        Ok(result) => result,
        Err(_) => bail!("daemon timed out\n{}", world.calls.lock().unwrap().join("\n---\n")),
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn log(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--format=%s"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn happy_path_locks_contracts_and_merges_each_page() {
    let tmp = repo("happy");
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"]), ("b", &["greet"])]), true, 4), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(read(&tmp.0.join("contracts/types.ts")), "export type Id = string;\n");
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
    assert!(read(&tmp.0.join("app/b/page.ts")).contains("\"b\""));
    let architecture = read(&tmp.0.join(".taskmaster/ARCHITECTURE.md"));
    assert!(architecture.contains("greet") && architecture.contains("app/a/**"));
    let changes = read(&tmp.0.join(".taskmaster/CHANGES.md"));
    assert!(changes.contains("page built") && changes.contains("contracts locked"));
    let subjects = log(&tmp.0);
    assert!(subjects.contains("taskmaster: lock contracts"));
    assert!(subjects.contains("taskmaster: built page a") && subjects.contains("taskmaster: built page b"));
    let events = read(&run_directory(&tmp.0, "current").join("events.jsonl"));
    let replayed = Run::replay(4, events.lines().map(|line| serde_json::from_str::<Event>(line).unwrap()));
    assert_eq!(replayed.phase, RunPhase::Done);
    let status: serde_json::Value =
        serde_json::from_str(&read(&run_directory(&tmp.0, "current").join("status.json"))).unwrap();
    assert_eq!(status["ok"], true);
}

#[tokio::test]
async fn a_change_outside_the_owned_glob_is_not_merged() {
    let tmp = repo("outside");
    let world = World::new(Mode::Outside);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(read(&tmp.0.join("contracts/types.ts")), "export type Id = string;\n");
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("export const a"));
    assert!(!tmp.0.join("app/b/stolen.ts").exists());
    let calls = world.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.contains("were not merged") && c.contains("contracts/types.ts")));
}

#[tokio::test]
async fn a_conformed_flag_sends_the_page_back_to_work() {
    let tmp = repo("conform");
    let world = World::new(Mode::Conform);
    let outcome = run(config(&tmp.0, plan(&[("a", &["auth"]), ("b", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(world.a.load(Ordering::SeqCst), 2);
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
    assert!(world.calls.lock().unwrap().iter().any(|c| c.contains("objected to Dictionary")));
}

#[tokio::test]
async fn an_amendment_publishes_a_successor_and_rebuilds_the_other_citer() {
    let tmp = repo("amend");
    let store = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    let old = store.publish("auth", EntryKind::Hint, "roles", "export const ROLES = ['user'];\n", "next").unwrap();
    let world = World::new(Mode::Amend);
    let outcome = run(config(&tmp.0, plan(&[("a", &["auth"]), ("b", &["auth"])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let entry = store.get("auth").unwrap().unwrap();
    assert_eq!(entry.content, "export const ROLES = ['admin'];\n");
    assert_ne!(entry.hash, old);
    assert!(store.version("auth", &old).unwrap().is_some());
    assert_eq!(world.a.load(Ordering::SeqCst), 2);
    assert_eq!(world.b.load(Ordering::SeqCst), 2);
    assert!(read(&run_directory(&tmp.0, "current").join("changes.jsonl")).contains("entry_amended"));
}

#[tokio::test]
async fn a_rejected_finding_does_not_repair_the_page() {
    let tmp = repo("reject");
    let world = World::new(Mode::RejectFinding);
    let outcome = run(config(&tmp.0, plan(&[("a", &["auth"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(world.a.load(Ordering::SeqCst), 1);
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
    let calls = world.calls.lock().unwrap().clone();
    assert_eq!(calls.iter().filter(|c| c.contains("Decide whether it is real")).count(), 1);
}

#[tokio::test]
async fn a_confirmed_finding_is_repaired() {
    let tmp = repo("confirm");
    let world = World::new(Mode::ConfirmFinding);
    let outcome = run(config(&tmp.0, plan(&[("a", &["auth"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(world.a.load(Ordering::SeqCst), 2);
    assert_eq!(read(&tmp.0.join("app/a/page.ts")), "export const a = 2;\n");
    assert!(world.calls.lock().unwrap().iter().any(|c| c.contains("review: app/a/page.ts:1")));
    assert!(read(&tmp.0.join(".taskmaster/CHANGES.md")).contains("review finding"));
}

#[tokio::test]
async fn a_cross_page_import_is_sent_back_to_its_owner() {
    let tmp = repo("cross");
    let world = World::new(Mode::CrossImport);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(read(&tmp.0.join("app/a/page.ts")), "export const a = 2;\n");
    assert!(read(&tmp.0.join("app/b/page.ts")).contains("\"b\""));
    assert!(world.calls.lock().unwrap().iter().any(|c| c.contains("imports") && c.contains("Build page `a`")));
    let patch = std::process::Command::new("git")
        .arg("-C")
        .arg(&tmp.0)
        .args(["log", "-p", "--", "app/a/page.ts"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&patch.stdout).contains("../b/page"));
}

#[tokio::test]
async fn parallel_workers_overlap_and_a_limit_of_one_does_not() {
    let wide = repo("wide");
    let barrier = Arc::new(Barrier::new(2));
    let world = World::new(Mode::Parallel(Some(barrier)));
    let outcome = run(config(&wide.0, plan(&[("a", &[]), ("b", &[])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(world.max_inflight.load(Ordering::SeqCst) >= 2);

    let narrow = repo("narrow");
    let world = World::new(Mode::Parallel(None));
    let outcome = run(config(&narrow.0, plan(&[("a", &[]), ("b", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(world.max_inflight.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancel_fails_the_run_before_the_next_page() {
    let tmp = repo("cancel");
    let (tx, rx) = watch::channel(false);
    let world = World::new(Mode::Cancel(tx));
    let mut config = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1);
    config.cancel = rx;
    let outcome = run(config, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: false });
    assert!(tmp.0.join("app/a/page.ts").is_file());
    assert!(!tmp.0.join("app/b/page.ts").exists());
    assert!(read(&run_directory(&tmp.0, "current").join("events.jsonl")).contains("cancelled"));
}

#[tokio::test]
async fn approval_waits_until_yes() {
    let tmp = repo("approve");
    let world = World::new(Mode::Write);
    let planned = plan(&[("a", &[]), ("b", &[])]);
    let outcome = run(config(&tmp.0, planned.clone(), false, 2), &world).await.unwrap();
    assert!(matches!(outcome, Outcome::NeedsApproval { .. }));
    assert!(!tmp.0.join("app/a/page.ts").exists());
    let outcome = run(config(&tmp.0, planned, true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
}

#[tokio::test]
async fn a_failed_worker_is_resumed_from_the_log() {
    let tmp = repo("resume");
    let world = World::new(Mode::Boom);
    let planned = plan(&[("a", &[]), ("b", &[])]);
    let err = run(config(&tmp.0, planned.clone(), true, 1), &world).await.unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    assert!(tmp.0.join("app/a/page.ts").is_file());
    assert!(!tmp.0.join("app/b/page.ts").exists());

    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, planned, true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(tmp.0.join("app/b/page.ts").is_file());
    assert!(!world.calls.lock().unwrap().iter().any(|c| c.contains("Build page `a`")));
}

#[tokio::test]
async fn building_pages_are_respawned_when_the_inflight_file_is_gone() {
    let tmp = repo("lost");
    let planned = plan(&[("a", &[]), ("b", &[])]);
    let dir = run_directory(&tmp.0, "current");
    std::fs::create_dir_all(&dir).unwrap();
    let events = [Event::PlanReady(planned), Event::PlanApproved];
    let body = events.iter().map(|e| serde_json::to_string(e).unwrap()).collect::<Vec<_>>().join("\n");
    std::fs::write(dir.join("events.jsonl"), format!("{body}\n")).unwrap();

    let world = World::new(Mode::Write);
    let mut config = Config::new(&tmp.0);
    config.yes = true;
    config.parallel = 4;
    let outcome = run(config, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
    assert!(read(&tmp.0.join("contracts/types.ts")).contains("Id"));
}

#[tokio::test]
async fn mistakes_stay_with_the_model_that_made_them() {
    let tmp = repo("mistakes");
    let mut book = Book::default();
    book.models.insert(
        "claude".into(),
        vec![Mistake {
            signature: "claude only".into(),
            sample: "claude-only mistake that must stay private".into(),
            count: PROMOTE_AT,
            fix: None,
        }],
    );
    records::save_book(&tmp.0, &book).unwrap();
    let world = World::new(Mode::Learn);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let page_b = calls.iter().find(|call| call.contains("Build page `b`")).expect("page b ran");
    assert!(page_b.contains("worker produced no changes under its owned files"));
    assert!(page_b.contains("Read the route param before rendering the header."));
    assert!(!calls.iter().any(|call| call.contains("claude-only mistake")));
    let saved = records::load_book(&tmp.0).unwrap();
    assert!(saved.models["fake"].iter().any(|mistake| mistake.promoted()));
    assert_eq!(saved.models["claude"][0].count, PROMOTE_AT);
}

#[tokio::test]
async fn unchanged_citations_skip_the_rebuild_until_the_entry_changes() {
    let tmp = repo("fresh");
    let store = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    store.publish("greet", EntryKind::Hint, "greeting", "export const G = 1;\n", "next").unwrap();
    let world = World::new(Mode::Write);
    let mut cfg = config(&tmp.0, plan(&[("a", &["greet"]), ("b", &["greet"])]), true, 2);
    cfg.run_id = "one".into();
    assert_eq!(run(cfg, &world).await.unwrap(), Outcome::Finished { ok: true });
    let commons = LocalStore::open(tmp.0.join(".taskmaster/commons")).unwrap();
    let shared = commons.get("greet").unwrap().unwrap();
    assert_eq!(shared.tier, DictTier::Commons);
    assert!(shared.stats.unwrap().passes >= 1);
    assert!(read(&tmp.0.join(".taskmaster/citations.json")).contains("greet"));

    let again = World::new(Mode::Write);
    let mut cfg = config(&tmp.0, plan(&[("a", &["greet"]), ("b", &["greet"])]), true, 2);
    cfg.run_id = "two".into();
    assert_eq!(run(cfg, &again).await.unwrap(), Outcome::Finished { ok: true });
    assert!(!again.calls.lock().unwrap().iter().any(|call| call.contains("Build page `")));

    store.publish("greet", EntryKind::Hint, "greeting", "export const G = 2;\n", "next").unwrap();
    let third = World::new(Mode::Rebuild);
    let mut cfg = config(&tmp.0, plan(&[("a", &["greet"]), ("b", &["greet"])]), true, 2);
    cfg.run_id = "three".into();
    assert_eq!(run(cfg, &third).await.unwrap(), Outcome::Finished { ok: true });
    let calls = third.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|call| call.contains("Build page `a`")));
    assert!(calls.iter().any(|call| call.contains("Build page `b`")));
    assert!(calls.iter().any(|call| call.contains("export const G = 2;")));
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("rebuilt"));
}

#[tokio::test]
async fn eval_runs_the_plan_against_one_model() {
    let tmp = repo("eval");
    let world = World::new(Mode::Write);
    let baseline_world = World::new(Mode::Nothing);
    let mut cfg = config(&tmp.0, plan(&[("a", &[])]), true, 1);
    cfg.run_id = "score".into();
    let baseline: Arc<dyn Harness> = Arc::new(Fake(baseline_world));
    let (outcome, report) = match tokio::time::timeout(
        Duration::from_secs(20),
        evaluate(cfg, world.harnesses(), baseline, "baseline"),
    )
    .await
    {
        Ok(result) => result.unwrap(),
        Err(_) => panic!("eval timed out"),
    };
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(report.taskmaster.ok);
    assert_eq!(report.taskmaster.passed, 1);
    let baseline = report.baseline.expect("baseline ran");
    assert!(!baseline.ok);
    assert!(baseline.check_failures >= 3);
    assert!(baseline.failed >= 1);
    assert!(tmp.0.join(".taskmaster/evals/score.json").is_file());
}

fn planner_calls(world: &World) -> Vec<String> {
    world
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.contains("Plan this product and lock its contracts."))
        .cloned()
        .collect()
}

#[test]
fn the_scripted_plan_is_json() {
    let text = plan_json(
        "greet",
        r#"{"key":"greet","kind":"hint","summary":"greeting","content":"export const greet = \"hi\";\n","stack":"next@15"}"#,
    );
    let (plan, dictionary) = crate::reply::parse_plan(&text).unwrap();
    assert_eq!(plan.contracts[0].content, "export type Id = string;\n");
    assert_eq!(dictionary[0].content, "export const greet = \"hi\";\n");
    assert_eq!(plan.pages[0].cites, vec!["greet".to_string()]);
}

#[tokio::test]
async fn a_prompt_locks_the_plan_and_publishes_the_dictionary() {
    let tmp = repo("prompt");
    git(&tmp.0, &["commit", "--allow-empty", "-q", "-m", "init"]);
    let world = World::new(Mode::Plan);
    let outcome = run(prompt_config(&tmp.0, "Build page a with a greeting.", true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(world.plans.load(Ordering::SeqCst), 1);
    assert_eq!(read(&tmp.0.join("contracts/types.ts")), "export type Id = string;\n");
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
    let store = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    assert_eq!(store.get("greet").unwrap().unwrap().content, "export const greet = \"hi\";\n");
    assert!(read(&run_directory(&tmp.0, "current").join("plan.json")).contains("\"a\""));
    let calls = world.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|call| call.contains("Build page `a`") && call.contains("export const greet = \"hi\"")));
}

#[tokio::test]
async fn a_rejected_plan_is_asked_once_more() {
    let tmp = repo("plan-fix");
    let world = World::new(Mode::PlanFix);
    let outcome = run(prompt_config(&tmp.0, "Build page a.", true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = planner_calls(&world);
    assert_eq!(calls.len(), 2);
    assert!(calls[1].contains("no dictionary entry"), "{}", calls[1]);
    let store = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    assert!(store.get("greet").unwrap().is_some());
}

#[tokio::test]
async fn an_existing_dictionary_entry_is_cited_without_a_rewrite() {
    let tmp = repo("plan-clash");
    let commons = LocalStore::open(tmp.0.join(".taskmaster/commons")).unwrap();
    commons
        .publish("theme", EntryKind::Hint, "dark theme", "export const theme = \"dark\";\n", "next")
        .unwrap();
    let mut entry = commons.get("theme").unwrap().unwrap();
    entry.tier = DictTier::Commons;
    entry.stats = Some(EntryStats { reuses: 0, pass_rate: 1.0, passes: 0, fails: 0 });
    commons.put(&entry).unwrap();
    let world = World::new(Mode::PlanClash);
    let outcome = run(prompt_config(&tmp.0, "Build page a using the theme entry.", true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = planner_calls(&world);
    assert_eq!(calls.len(), 2);
    assert!(calls[0].contains("export const theme = \"dark\""), "{}", calls[0]);
    assert!(calls[1].contains("different content"), "{}", calls[1]);
    assert_eq!(commons.get("theme").unwrap().unwrap().content, "export const theme = \"dark\";\n");
    let local = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    assert!(local.get("theme").unwrap().is_none());
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
}

#[tokio::test]
async fn a_prompt_waits_for_approval_then_builds_the_recorded_plan() {
    let tmp = repo("prompt-approve");
    let world = World::new(Mode::Plan);
    let outcome = run(prompt_config(&tmp.0, "Build page a.", false), &world).await.unwrap();
    let Outcome::NeedsApproval { summary } = outcome else { panic!("expected approval, got {outcome:?}") };
    assert!(summary.contains("/a"));
    assert!(!tmp.0.join("app/a/page.ts").exists());
    let store = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    assert!(store.get("greet").unwrap().is_some());

    let again = World::new(Mode::Write);
    let outcome = run(prompt_config(&tmp.0, "Build page a.", true), &again).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(again.plans.load(Ordering::SeqCst), 0);
    assert!(tmp.0.join("app/a/page.ts").is_file());
}

#[tokio::test]
async fn a_prompt_that_never_plans_fails() {
    let tmp = repo("plan-bad");
    let world = World::new(Mode::PlanBad);
    let err = run(prompt_config(&tmp.0, "Build page a.", true), &world).await.unwrap_err();
    assert!(err.to_string().contains("no pages"), "{err}");
    assert_eq!(world.plans.load(Ordering::SeqCst), 2);
    assert!(!tmp.0.join("contracts/types.ts").exists());
}

#[tokio::test]
async fn eval_of_a_prompt_plans_once() {
    let tmp = repo("prompt-eval");
    let world = World::new(Mode::Plan);
    let baseline_world = World::new(Mode::Nothing);
    let mut cfg = prompt_config(&tmp.0, "Build page a with a greeting.", true);
    cfg.run_id = "score".into();
    let baseline: Arc<dyn Harness> = Arc::new(Fake(baseline_world.clone()));
    let (outcome, report) = match tokio::time::timeout(
        Duration::from_secs(20),
        evaluate(cfg, world.harnesses(), baseline, "baseline"),
    )
    .await
    {
        Ok(result) => result.unwrap(),
        Err(_) => panic!("eval timed out\n{}", world.calls.lock().unwrap().join("\n---\n")),
    };
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(world.plans.load(Ordering::SeqCst), 1);
    assert_eq!(baseline_world.plans.load(Ordering::SeqCst), 0);
    assert_eq!(report.taskmaster.passed, 1);
    let baseline = report.baseline.expect("baseline ran");
    assert!(!baseline.ok);
    assert!(baseline.check_failures >= 3);
}
