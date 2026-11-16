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

use crate::store::Records;
use crate::{Config, Harnesses, Outcome, drive, evaluate, run_directory};

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
        let usage = Usage { input: 100, output: 20, cache_read: 0, cache_write: 0 };
        Ok(RunSummary { ok: true, usage, cost_usd: Some(0.01), denied: 0 })
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
        if brief.contains("Shared files this run wrote fail the project's checks") {
            if matches!(self.mode, Mode::Nothing) {
                return Ok(Script::text("I could not find the problem."));
            }
            // The second file is not on the list the master was given.
            return Ok(Script {
                text: "fixed".into(),
                files: vec![
                    ("contracts/types.ts".into(), Some("export type Id = string; // repaired\n".into())),
                    ("app/a/page.ts".into(), Some("hacked\n".into())),
                ],
            });
        }
        // A review of a repository, started through the API, not a run's own review.
        if brief.contains("You are reviewing a code change") {
            let found = matches!(self.mode, Mode::ConfirmFinding) && !brief.contains("security problems only");
            return Ok(Script::text(if found {
                "[{\"file\":\"src/lib.ts\",\"line\":1,\"kind\":\"bug\",\"failure\":\"returns the wrong total when the cart is empty\"}]"
            } else {
                "[]"
            }));
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

/// A repository that already has a commit, so a run gets a branch of its own.
fn seeded(name: &str) -> Tmp {
    let tmp = repo(name);
    std::fs::write(tmp.0.join("README.md"), "# project\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "start"]);
    tmp
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
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
    Records::files(&tmp.0).save_book(&book).unwrap();
    let world = World::new(Mode::Learn);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let page_b = calls.iter().find(|call| call.contains("Build page `b`")).expect("page b ran");
    assert!(page_b.contains("worker produced no changes under its owned files"));
    assert!(page_b.contains("Read the route param before rendering the header."));
    assert!(!calls.iter().any(|call| call.contains("claude-only mistake")));
    let saved = Records::files(&tmp.0).book().unwrap();
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
    // Each side is charged for its own calls, so the two can be compared on cost.
    assert_eq!(report.taskmaster.tokens, report.taskmaster.agent_calls * 120);
    assert!(baseline.agent_calls >= 3 && baseline.tokens == baseline.agent_calls * 120);
    assert!(tmp.0.join(".taskmaster/evals/score.json").is_file());
    // The baseline was measured, not kept.
    assert_eq!(git_out(&tmp.0, &["branch", "--list", "taskmaster/*"]).trim(), "");
    assert!(!read(&tmp.0.join("app/a/page.ts")).is_empty());
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

#[tokio::test]
async fn every_agent_call_is_charged_to_the_run() {
    let tmp = repo("ledger");
    let world = World::new(Mode::ConfirmFinding);
    let outcome = run(config(&tmp.0, plan(&[("a", &["auth"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let records = Records::files(&tmp.0);
    let entries = records.ledger("current").unwrap();
    assert_eq!(entries.len(), world.calls.lock().unwrap().len());
    let roles: Vec<&str> = entries.iter().map(|entry| entry.role.name()).collect();
    for role in ["worker", "reviewer", "security", "verifier"] {
        assert!(roles.contains(&role), "{role} missing from {roles:?}");
    }
    assert!(entries.iter().any(|entry| entry.role.name() == "worker" && entry.page.as_deref() == Some("a")));
    let status = records.status("current").unwrap().unwrap();
    assert_eq!(status["spent"]["calls"], entries.len());
    assert_eq!(status["spent"]["tokens"], entries.len() * 120);
    assert_eq!(status["spent"]["unpriced_calls"], 0);
    // One page and two reviewers, priced from defaults because nothing ran here before.
    assert_eq!(status["estimate"]["tokens"], 17_000 + 2 * 500);
}

#[tokio::test]
async fn a_run_keeps_its_records_wherever_the_store_puts_them() {
    let tmp = repo("memory");
    let world = World::new(Mode::Write);
    let store = Arc::new(crate::store::tests::Memory::default());
    let mut cfg = config(&tmp.0, plan(&[("a", &[])]), true, 1);
    cfg.store = Some(store.clone());
    assert_eq!(run(cfg, &world).await.unwrap(), Outcome::Finished { ok: true });
    assert!(tmp.0.join("app/a/page.ts").is_file());
    assert!(!tmp.0.join(".taskmaster/runs").exists());
    assert!(!tmp.0.join(".taskmaster/citations.json").exists());
    let keys: Vec<String> = store.0.lock().unwrap().keys().cloned().collect();
    for key in ["runs/current/events.jsonl", "runs/current/ledger.jsonl", "runs/current/status.json", "citations.json"] {
        assert!(keys.iter().any(|have| have == key), "{key} missing from {keys:?}");
    }
}

#[tokio::test]
async fn a_run_can_keep_its_records_in_a_database() {
    let tmp = repo("sqlite");
    let world = World::new(Mode::Write);
    let db = tmp.0.join(".taskmaster/taskmaster.db");
    let store: Arc<dyn crate::Store> = Arc::new(crate::SqliteStore::open(&db).unwrap());
    let mut cfg = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2);
    cfg.store = Some(store.clone());
    assert_eq!(run(cfg, &world).await.unwrap(), Outcome::Finished { ok: true });
    assert!(db.is_file() && !tmp.0.join(".taskmaster/runs").exists());
    let records = Records::new(store);
    assert_eq!(records.run_ids().unwrap(), vec!["current".to_string()]);
    let spent = records.ledger("current").unwrap();
    assert!(spent.len() >= 2);
    // Another process opens the same file and sees the same run.
    let again = Records::new(Arc::new(crate::SqliteStore::open(&db).unwrap()));
    assert_eq!(again.ledger("current").unwrap(), spent);
    assert_eq!(again.events("current").unwrap(), records.events("current").unwrap());
    assert!(again.status("current").unwrap().is_some());
}

#[derive(Default)]
struct Shared {
    pulls: AtomicUsize,
    pushed: Mutex<Vec<String>>,
}

impl tm_dictionary::CommonsRemote for Shared {
    fn pull(&self, commons: &LocalStore) -> anyhow::Result<usize> {
        self.pulls.fetch_add(1, Ordering::SeqCst);
        commons.publish("theme", EntryKind::Pattern, "color tokens", "export const INK = '#fff';\n", "next")?;
        Ok(1)
    }

    fn push(&self, entry: &tm_core::DictionaryEntry) -> anyhow::Result<()> {
        self.pushed.lock().unwrap().push(entry.key.clone());
        Ok(())
    }
}

#[tokio::test]
async fn a_shared_commons_is_pulled_before_planning_and_sent_what_is_promoted() {
    let tmp = repo("remote");
    let store = LocalStore::open(tmp.0.join(".taskmaster/dictionary")).unwrap();
    store.publish("greet", EntryKind::Hint, "greeting", "export const G = 1;\n", "next").unwrap();
    let remote = Arc::new(Shared::default());
    let world = World::new(Mode::Write);
    let mut cfg = config(&tmp.0, plan(&[("a", &["greet"])]), true, 1);
    cfg.remote = Some(remote.clone());
    assert_eq!(run(cfg, &world).await.unwrap(), Outcome::Finished { ok: true });
    assert_eq!(remote.pulls.load(Ordering::SeqCst), 1);
    assert_eq!(*remote.pushed.lock().unwrap(), vec!["greet".to_string()]);
    // What arrived is offered to the worker like any other Commons entry.
    let commons = LocalStore::open(tmp.0.join(".taskmaster/commons")).unwrap();
    assert!(commons.get("theme").unwrap().is_some());
}

#[tokio::test]
async fn a_rejected_plan_goes_back_to_planning_and_can_be_planned_again() {
    let tmp = repo("reject");
    let world = World::new(Mode::Plan);
    let outcome = run(prompt_config(&tmp.0, "Build page a.", false), &world).await.unwrap();
    assert!(matches!(outcome, Outcome::NeedsApproval { .. }));

    let mut cfg = Config::new(&tmp.0);
    cfg.reject = true;
    assert_eq!(run(cfg, &world).await.unwrap(), Outcome::Rejected);
    let events = Records::files(&tmp.0).events("current").unwrap();
    assert_eq!(Run::replay(1, events).phase, RunPhase::Planning);
    assert_eq!(world.plans.load(Ordering::SeqCst), 1);

    // A new request plans again and waits for approval as before.
    let outcome = run(prompt_config(&tmp.0, "Build page a, differently.", false), &world).await.unwrap();
    assert!(matches!(outcome, Outcome::NeedsApproval { .. }));
    assert_eq!(world.plans.load(Ordering::SeqCst), 2);
    assert!(!tmp.0.join("app/a/page.ts").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_type_error_goes_back_to_the_page_that_owns_the_file() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = repo("typecheck");
    std::fs::write(tmp.0.join("tsconfig.json"), "{}\n").unwrap();
    let bin = tmp.0.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    // Stands in for the project's own TypeScript: one error on the first run, none after.
    let script = "#!/bin/sh\nif [ ! -f node_modules/ran ]; then touch node_modules/ran; \
                  echo \"app/a/page.ts(1,1): error TS2304: Cannot find name 'nope'.\"; exit 2; fi\n";
    std::fs::write(bin.join("tsc"), script).unwrap();
    std::fs::set_permissions(bin.join("tsc"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let world = World::new(Mode::ConfirmFinding);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let repair = calls.iter().find(|call| call.contains("Checks failed last time")).expect("page a was repaired");
    assert!(repair.contains("app/a/page.ts:1 tsc: TS2304"), "{repair}");
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("a = 2"));
    let book = Records::files(&tmp.0).book().unwrap();
    assert!(book.models["fake"].iter().any(|mistake| mistake.sample.contains("TS2304")));
}

#[tokio::test]
async fn the_next_attempt_is_told_what_the_last_one_did_and_sees_its_discarded_change() {
    let tmp = repo("handoff");
    let world = World::new(Mode::Outside);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let repair = calls.iter().find(|call| call.contains("Checks failed last time")).expect("page a was repaired");
    assert!(repair.contains("What was tried on this page before"), "{repair}");
    assert!(repair.contains("fake (worker) — discarded."), "{repair}");
    assert!(repair.contains("Why it was not kept: these files are outside"), "{repair}");
    // The change was thrown away, so the next attempt gets it as a diff.
    assert!(repair.contains("```diff") && repair.contains("+hacked"), "{repair}");
    let attempts = Records::files(&tmp.0).attempts("current", "a").unwrap();
    assert_eq!(attempts.iter().map(|a| a.outcome.as_str()).collect::<Vec<_>>(), vec!["discarded", "merged"]);
    assert!(attempts[1].fixing.as_deref().unwrap().contains("outside"));
    assert!(attempts[1].diff.is_none(), "a merged change is in the tree, not in the record");
}

/// A project whose `npm run build` fails until `fixed_when` holds, printing `error`.
fn buildable(name: &str, error: &str, fixed_when: &str) -> Option<Tmp> {
    let npm = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("npm").is_file()));
    if !npm {
        eprintln!("npm is not installed; skipping");
        return None;
    }
    let tmp = repo(name);
    std::fs::write(tmp.0.join("package.json"), r#"{"name":"t","private":true,"scripts":{"build":"sh build.sh"}}"#).unwrap();
    std::fs::write(tmp.0.join("build.sh"), format!("if {fixed_when}; then exit 0; fi\nprintf '%s\\n' \"{error}\"\nexit 1\n")).unwrap();
    std::fs::write(tmp.0.join(".gitignore"), "node_modules/\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "start"]);
    std::fs::create_dir_all(tmp.0.join("node_modules")).unwrap();
    Some(tmp)
}

#[tokio::test]
async fn a_failing_build_goes_back_to_the_page_its_output_names() {
    let Some(tmp) = buildable("build-page", "./app/a/page.ts:1:1 Type error: the build broke here", "grep -q 'a = 2' app/a/page.ts") else {
        return;
    };
    let world = World::new(Mode::ConfirmFinding);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let repair = calls
        .iter()
        .find(|call| call.contains("`build` failed:"))
        .expect("the build failure went back to a page");
    assert!(repair.contains("Build page `a`") && repair.contains("the build broke here"), "{repair}");
    assert!(!calls.iter().any(|call| call.contains("Build page `b`") && call.contains("`build` failed")));
}

#[tokio::test]
async fn a_build_failure_in_no_file_of_this_run_costs_nothing() {
    let Some(tmp) = buildable("build-elsewhere", "lib/legacy.ts:3:1 something old is broken", "false") else { return };
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    assert!(!calls.iter().any(|call| call.contains("failed:")), "nobody was asked to fix it");
}

#[tokio::test]
async fn a_run_builds_on_its_own_branch_and_is_merged_when_it_finishes() {
    let tmp = seeded("branch");
    let world = World::new(Mode::Write);
    let planned = plan(&[("a", &[]), ("b", &[])]);
    let outcome = run(config(&tmp.0, planned.clone(), false, 2), &world).await.unwrap();
    assert!(matches!(outcome, Outcome::NeedsApproval { .. }));
    assert!(git_out(&tmp.0, &["branch", "--list", "taskmaster/*"]).contains("taskmaster/current"));

    // The project moves on while the plan waits. The run still lands on top of it.
    std::fs::write(tmp.0.join("notes.md"), "mine\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "my own work"]);

    let outcome = run(config(&tmp.0, planned, true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
    assert_eq!(read(&tmp.0.join("notes.md")), "mine\n");
    assert!(log(&tmp.0).contains("taskmaster: built page a") && log(&tmp.0).contains("my own work"));
    assert_eq!(git_out(&tmp.0, &["branch", "--list", "taskmaster/*"]).trim(), "");
    assert_eq!(git_out(&tmp.0, &["status", "--porcelain"]).trim(), "");
    let status = Records::files(&tmp.0).status("current").unwrap().unwrap();
    assert_eq!((status["branch"].as_str(), status["merged"].as_bool()), (Some("taskmaster/current"), Some(true)));
    assert!(Records::files(&tmp.0).citations().unwrap().pages.contains_key("a"));
}

#[tokio::test]
async fn a_failed_run_leaves_the_project_as_it_was() {
    let tmp = seeded("branch-fail");
    let world = World::new(Mode::Nothing);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: false });
    assert!(!tmp.0.join("contracts/types.ts").exists());
    assert_eq!(log(&tmp.0).trim(), "start");
    assert!(git_out(&tmp.0, &["log", "--format=%s", "taskmaster/current"]).contains("taskmaster: lock contracts"));
    let status = Records::files(&tmp.0).status("current").unwrap().unwrap();
    assert_eq!(status["merged"], false);
}

#[tokio::test]
async fn a_run_that_conflicts_with_the_project_stays_on_its_branch() {
    let tmp = seeded("branch-conflict");
    let world = World::new(Mode::Write);
    let planned = plan(&[("a", &[])]);
    assert!(matches!(run(config(&tmp.0, planned.clone(), false, 1), &world).await.unwrap(), Outcome::NeedsApproval { .. }));
    std::fs::create_dir_all(tmp.0.join("app/a")).unwrap();
    std::fs::write(tmp.0.join("app/a/page.ts"), "export const mine = true;\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "my page"]);

    let outcome = run(config(&tmp.0, planned, true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(read(&tmp.0.join("app/a/page.ts")), "export const mine = true;\n");
    assert_eq!(git_out(&tmp.0, &["status", "--porcelain"]).trim(), "");
    assert!(git_out(&tmp.0, &["show", "taskmaster/current:app/a/page.ts"]).contains("\"a\""));
    assert_eq!(Records::files(&tmp.0).status("current").unwrap().unwrap()["merged"], false);
    // The project does not have the run's page, so a later run must not skip it as unchanged.
    assert!(!Records::files(&tmp.0).citations().unwrap().pages.contains_key("a"));
}

#[tokio::test]
async fn a_template_gives_the_project_its_shared_files_and_the_planner_its_layout() {
    let tmp = seeded("template");
    std::fs::write(tmp.0.join("package.json"), "{\"name\":\"mine\"}\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "my package"]);
    let world = World::new(Mode::Plan);
    let mut cfg = prompt_config(&tmp.0, "Build page a.", true);
    cfg.template = Some("next".into());
    assert_eq!(run(cfg, &world).await.unwrap(), Outcome::Finished { ok: true });
    let planner = planner_calls(&world).remove(0);
    assert!(planner.contains("`next` template") && planner.contains("app/<page id>/page.tsx"), "{planner}");
    assert!(read(&tmp.0.join("tsconfig.json")).contains("\"strict\": true"));
    assert!(read(&tmp.0.join(".gitignore")).contains("node_modules"));
    assert!(tmp.0.join("app/layout.tsx").is_file());
    // A file the project already had is not replaced.
    assert_eq!(read(&tmp.0.join("package.json")), "{\"name\":\"mine\"}\n");
    assert!(!tmp.0.join("TEMPLATE.md").exists());
}

#[tokio::test]
async fn the_estimate_comes_from_what_earlier_runs_here_cost() {
    let tmp = seeded("forecast");
    let world = World::new(Mode::Write);
    let mut first = config(&tmp.0, plan(&[("a", &[])]), true, 1);
    first.run_id = "one".into();
    assert_eq!(run(first, &world).await.unwrap(), Outcome::Finished { ok: true });
    let mut second = config(&tmp.0, plan(&[("b", &[]), ("c", &[])]), true, 1);
    second.run_id = "two".into();
    assert_eq!(run(second, &world).await.unwrap(), Outcome::Finished { ok: true });
    // Every fake call is 120 tokens and one cent: two pages and one security reviewer.
    let status = Records::files(&tmp.0).status("two").unwrap().unwrap();
    assert_eq!(status["estimate"]["tokens"], 3 * 120);
    assert!((status["estimate"]["usd"].as_f64().unwrap() - 0.03).abs() < 1e-9);
    assert_eq!(status["spent"]["fresh"], 3 * 120);
}

/// A project whose `tsc` is a script: it reports `error` until `fixed_when` holds.
#[cfg(unix)]
fn typed(name: &str, error: &str, fixed_when: &str) -> Tmp {
    use std::os::unix::fs::PermissionsExt;
    let tmp = repo(name);
    std::fs::create_dir_all(tmp.0.join("lib")).unwrap();
    std::fs::write(tmp.0.join("tsconfig.json"), "{}\n").unwrap();
    std::fs::write(tmp.0.join("lib/old.ts"), "export const old = 1;\n").unwrap();
    std::fs::write(tmp.0.join(".gitignore"), "node_modules/\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "start"]);
    // Installed packages are not in git. The run's checkout has to borrow them.
    let bin = tmp.0.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = format!("#!/bin/sh\nif {fixed_when}; then exit 0; fi\necho \"{error}\"\nexit 2\n");
    std::fs::write(bin.join("tsc"), script).unwrap();
    std::fs::set_permissions(bin.join("tsc"), std::fs::Permissions::from_mode(0o755)).unwrap();
    tmp
}

#[cfg(unix)]
#[tokio::test]
async fn an_error_in_a_contract_is_repaired_by_the_master_and_nothing_else_is_touched() {
    let tmp = typed(
        "shared",
        "contracts/types.ts(1,1): error TS1005: ';' expected.",
        "grep -q repaired contracts/types.ts",
    );
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let repair = calls.iter().find(|call| call.contains("Shared files this run wrote")).expect("the master was asked");
    assert!(repair.contains("contracts/types.ts:1 tsc: TS1005") && repair.contains("- `contracts/types.ts`"), "{repair}");
    assert!(read(&tmp.0.join("contracts/types.ts")).contains("repaired"));
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
    assert!(log(&tmp.0).contains("taskmaster: repair shared files"));
    let entries = Records::files(&tmp.0).ledger("current").unwrap();
    assert!(entries.iter().any(|entry| entry.role.name() == "master" && entry.page.is_none()));
}

#[cfg(unix)]
#[tokio::test]
async fn a_contract_nobody_can_repair_fails_the_run() {
    let tmp = typed("shared-stuck", "contracts/types.ts(1,1): error TS1005: ';' expected.", "false");
    // Pages build, but the master's repair changes nothing.
    let world = World::new(Mode::Write);
    let stuck = World::new(Mode::Nothing);
    let mut harnesses = world.harnesses();
    harnesses.master = Arc::new(Fake(stuck.clone()));
    let cfg = config(&tmp.0, plan(&[("a", &[])]), true, 1);
    let outcome = tokio::time::timeout(Duration::from_secs(20), drive(cfg, harnesses)).await.unwrap().unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: false });
    assert_eq!(stuck.calls.lock().unwrap().len(), 1, "one repair attempt, then the run stops");
    assert!(!tmp.0.join("app/a/page.ts").exists());
    assert!(read(&run_directory(&tmp.0, "current").join("changes.jsonl")).contains("TS1005"));
}

#[cfg(unix)]
#[tokio::test]
async fn an_error_that_was_there_before_the_run_is_left_alone() {
    let tmp = typed("shared-older", "lib/old.ts(1,1): error TS2304: Cannot find name 'gone'.", "false");
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(!world.calls.lock().unwrap().iter().any(|call| call.contains("Shared files this run wrote")));
    assert!(tmp.0.join("app/a/page.ts").is_file());
}

mod http {
    //! The API over a real socket, with the fake harness behind it.

    use super::*;
    use crate::api::{Server, ServerOptions};
    use crate::auth::PairingAuth;
    use serde_json::{Value, json};

    struct Client {
        addr: String,
        token: Option<String>,
    }

    impl Client {
        async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(&self.addr).await.unwrap();
            let body = body.map(|value| value.to_string()).unwrap_or_default();
            let auth = self.token.as_ref().map(|token| format!("authorization: Bearer {token}\r\n")).unwrap_or_default();
            let request = format!(
                "{method} {path} HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{auth}\r\n{body}",
                self.addr,
                body.len()
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            let (head, rest) = reply.split_once("\r\n\r\n").unwrap_or(("", &reply));
            let status: u16 = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
            (status, serde_json::from_str(rest).unwrap_or(Value::Null))
        }

        /// The body as text, for a reply that is not JSON.
        async fn text(&self, path: &str) -> (u16, String) {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::TcpStream::connect(&self.addr).await.unwrap();
            let request = format!("GET {path} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\r\n", self.addr);
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            let (head, rest) = reply.split_once("\r\n\r\n").unwrap_or(("", &reply));
            (head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0), rest.to_string())
        }

        async fn get(&self, path: &str) -> (u16, Value) {
            self.call("GET", path, None).await
        }

        async fn post(&self, path: &str, body: Value) -> (u16, Value) {
            self.call("POST", path, Some(body)).await
        }

        /// Poll a run until it is idle in `phase`.
        async fn settled(&self, id: &str, phase: &str) -> Value {
            for _ in 0..400 {
                let (_, run) = self.get(&format!("/api/runs/{id}")).await;
                if run["phase"] == phase && run["active"] == false {
                    return run;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("run {id} never reached {phase}: {}", self.get(&format!("/api/runs/{id}")).await.1);
        }
    }

    async fn server(tmp: &Tmp, world: &Arc<World>) -> (Server, Client, String) {
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let code = auth.code().to_string();
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        let world = world.clone();
        options.harness = Arc::new(move |_| Ok(Arc::new(Fake(world.clone())) as Arc<dyn Harness>));
        let server = Server::serve(options).await.unwrap();
        let addr = server.url.trim_start_matches("http://").trim_end_matches('/').to_string();
        (server, Client { addr, token: None }, code)
    }

    #[tokio::test]
    async fn a_device_pairs_starts_a_run_approves_it_and_reads_what_it_cost() {
        let tmp = repo("api");
        LocalStore::open(tmp.0.join(".taskmaster/dictionary"))
            .unwrap()
            .publish("greet", EntryKind::Hint, "greeting", "export const G = 1;\n", "next")
            .unwrap();
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;

        assert_eq!(client.get("/api/health").await.0, 200);
        assert_eq!(client.get("/api/runs").await.0, 401);
        assert_eq!(client.post("/api/pair", json!({ "code": "WRONG" })).await.0, 401);
        let (status, paired) = client.post("/api/pair", json!({ "code": code })).await;
        assert_eq!(status, 200, "{paired}");
        client.token = Some(paired["token"].as_str().unwrap().to_string());
        assert_eq!(client.get("/api/runs").await.1, json!({ "runs": [] }));

        // Nothing is built until the plan is approved.
        let planned = serde_json::to_value(plan(&[("a", &["greet"]), ("b", &["greet"])])).unwrap();
        let (status, made) = client.post("/api/runs", json!({ "id": "web", "plan": planned, "parallel": 2 })).await;
        assert_eq!(status, 202, "{made}");
        let waiting = client.settled("web", "awaiting_approval").await;
        assert_eq!(waiting["plan"]["pages"].as_array().unwrap().len(), 2);
        assert_eq!(waiting["roles"]["worker"], "agy");
        assert!(!tmp.0.join("app/a/page.ts").exists());
        assert_eq!(client.post("/api/runs", json!({ "id": "web", "plan": planned })).await.0, 409);
        assert_eq!(client.post("/api/runs", json!({ "id": "../x", "plan": planned })).await.0, 400);
        assert_eq!(client.post("/api/runs", json!({ "id": "empty" })).await.0, 400);

        assert_eq!(client.post("/api/runs/web/approve", json!({})).await.0, 202);
        let done = client.settled("web", "done").await;
        assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
        assert!(done["pages"].as_array().unwrap().iter().all(|page| page["state"] == "passed"));
        assert_eq!(done["cancelled"], false);
        assert_eq!(done["estimate"]["tokens"], 2 * 17_000 + 2 * 500);
        let calls = done["spent"]["calls"].as_u64().unwrap();
        assert!(calls >= 3 && done["spent"]["tokens"] == calls * 120, "{done}");
        assert_eq!(client.post("/api/runs/web/approve", json!({})).await.0, 409);
        assert_eq!(client.post("/api/runs/web/cancel", json!({})).await.0, 409);

        let (_, ledger) = client.get("/api/runs/web/ledger").await;
        assert_eq!(ledger["entries"].as_array().unwrap().len() as u64, calls);
        assert!(ledger["totals"]["by_role"]["worker"]["calls"].as_u64().unwrap() >= 2);
        let (_, all) = client.get("/api/runs/web/events").await;
        let (_, later) = client.get("/api/runs/web/events?after=2").await;
        let total = all["next"].as_u64().unwrap() as usize;
        assert_eq!(later["events"].as_array().unwrap().len(), total - 2);
        let (_, attempts) = client.get("/api/runs/web/attempts/a").await;
        assert_eq!(attempts["attempts"][0]["outcome"], "merged", "{attempts}");
        assert_eq!(client.get("/api/runs/web/screens").await.1, json!({ "pages": [] }));
        assert_eq!(client.get("/api/runs/web/screens/a").await.0, 404);
        assert_eq!(client.get("/api/runs/web/attempts/..").await.0, 404);
        let (_, changes) = client.get("/api/runs/web/changes").await;
        assert!(changes["changes"].as_array().unwrap().iter().any(|change| change["kind"] == "page_built"));

        // The stream replays the log and stops once nothing is driving the run.
        let token = client.token.clone().unwrap();
        assert_eq!(client.text("/api/runs/web/stream").await.0, 401);
        let (status, feed) = client.text(&format!("/api/runs/web/stream?token={token}")).await;
        assert_eq!(status, 200);
        assert_eq!(feed.matches("event: event").count(), total, "{feed}");
        assert!(feed.contains("id: 1\n") && feed.contains("plan_ready"), "{feed}");
        assert!(feed.contains("event: idle") && feed.contains("{\"phase\":\"done\"}"), "{feed}");
        let (_, rest) = client.text(&format!("/api/runs/web/stream?token={token}&after={total}")).await;
        assert!(!rest.contains("event: event") && rest.contains("event: idle"), "{rest}");

        let (_, found) = client.get("/api/dictionary?q=greet").await;
        assert_eq!(found["entries"][0]["key"], "greet");
        assert_eq!(client.get("/api/dictionary/greet").await.1["content"], "export const G = 1;\n");
        assert_eq!(client.get("/api/dictionary/nope").await.0, 404);
        assert_eq!(client.get("/api/runs/nope").await.0, 404);
        assert_eq!(client.get("/api/errorography").await.0, 200);
        assert_eq!(client.get("/api/templates").await.1, json!({ "templates": ["next"] }));
        assert_eq!(client.get("/api/providers?harness=nope").await.0, 400);
    }

    #[tokio::test]
    async fn a_plan_is_rejected_then_the_run_is_cancelled() {
        let tmp = repo("api-reject");
        let world = World::new(Mode::Plan);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        let (status, made) = client.post("/api/runs", json!({ "prompt": "Build page a." })).await;
        assert_eq!(status, 202, "{made}");
        let id = made["id"].as_str().unwrap().to_string();
        let waiting = client.settled(&id, "awaiting_approval").await;
        assert_eq!(waiting["prompt"], "Build page a.");
        assert_eq!(waiting["spent"]["by_role"]["planner"]["calls"], 1);

        // A new prompt plans again; the run waits for approval a second time.
        let again = json!({ "prompt": "Build page a with a greeting." });
        assert_eq!(client.post(&format!("/api/runs/{id}/reject"), again).await.0, 202);
        for _ in 0..400 {
            if world.plans.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let waiting = client.settled(&id, "awaiting_approval").await;
        assert_eq!(waiting["prompt"], "Build page a with a greeting.");

        assert_eq!(client.post(&format!("/api/runs/{id}/reject"), json!({})).await.0, 202);
        client.settled(&id, "planning").await;
        assert_eq!(client.post(&format!("/api/runs/{id}/approve"), json!({})).await.0, 409);
        assert_eq!(client.post(&format!("/api/runs/{id}/cancel"), json!({})).await.0, 202);
        assert_eq!(client.settled(&id, "failed").await["cancelled"], true);
        assert!(!tmp.0.join("app/a/page.ts").exists());
    }

    #[tokio::test]
    async fn a_review_of_the_project_is_started_and_read_back() {
        let tmp = repo("api-review");
        std::fs::create_dir_all(tmp.0.join("src")).unwrap();
        std::fs::write(tmp.0.join("src/lib.ts"), "export const total = 0;\n").unwrap();
        git(&tmp.0, &["add", "-A"]);
        git(&tmp.0, &["commit", "-q", "-m", "first"]);
        std::fs::write(tmp.0.join("src/lib.ts"), "export const total = 1;\n").unwrap();
        git(&tmp.0, &["commit", "-q", "-am", "second"]);

        let world = World::new(Mode::ConfirmFinding);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        let (status, made) = client.post("/api/reviews", json!({})).await;
        assert_eq!(status, 202, "{made}");
        let id = made["id"].as_str().unwrap().to_string();
        let mut job = Value::Null;
        for _ in 0..400 {
            job = client.get(&format!("/api/reviews/{id}")).await.1;
            if job["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(job["status"], "done", "{job}");
        let findings = job["report"]["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{job}");
        assert_eq!(findings[0]["file"], "src/lib.ts");
        assert_eq!(findings[0]["outcome"], "confirmed");
        assert!(job["text"].as_str().unwrap().contains("[bug] src/lib.ts:1"));
        assert!(job["report"]["usage"]["input"].as_u64().unwrap() >= 300);
        let (_, listed) = client.get("/api/reviews").await;
        assert!(listed["reviews"].as_array().unwrap().iter().any(|row| row["id"] == id.as_str() && row["confirmed"] == 1));
        assert!(read(&tmp.0.join(".taskmaster/reviews/history.jsonl")).contains("wrong total"));
    }

    #[tokio::test]
    async fn two_runs_build_at_once_on_their_own_branches() {
        let tmp = seeded("api-two");
        let barrier = Arc::new(Barrier::new(2));
        let world = World::new(Mode::Parallel(Some(barrier)));
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        // Each worker waits until the other run's worker is building too.
        for (id, page) in [("one", "a"), ("two", "b")] {
            let planned = serde_json::to_value(plan(&[(page, &[])])).unwrap();
            let (status, made) = client.post("/api/runs", json!({ "id": id, "plan": planned, "yes": true })).await;
            assert_eq!(status, 202, "{made}");
        }
        for id in ["one", "two"] {
            let done = client.settled(id, "done").await;
            assert_eq!(done["merged"], true, "{done}");
        }
        assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
        assert_eq!(git_out(&tmp.0, &["branch", "--list", "taskmaster/*"]).trim(), "");
        assert_eq!(git_out(&tmp.0, &["status", "--porcelain"]).trim(), "");
    }

    #[tokio::test]
    async fn a_repository_with_no_commit_builds_one_run_at_a_time() {
        let tmp = repo("api-alone");
        let barrier = Arc::new(Barrier::new(2));
        let world = World::new(Mode::Parallel(Some(barrier.clone())));
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        let planned = serde_json::to_value(plan(&[("a", &[])])).unwrap();
        assert_eq!(client.post("/api/runs", json!({ "id": "one", "plan": planned, "yes": true })).await.0, 202);
        let (status, refused) = client.post("/api/runs", json!({ "id": "two", "plan": planned })).await;
        assert_eq!(status, 409, "{refused}");
        assert!(refused["error"].as_str().unwrap().contains("no commit"), "{refused}");
        barrier.wait().await;
        client.settled("one", "done").await;
    }

    #[tokio::test]
    async fn a_run_that_cannot_start_reports_why() {
        let tmp = repo("api-error");
        let world = World::new(Mode::PlanBad);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());
        let (_, made) = client.post("/api/runs", json!({ "id": "bad", "prompt": "Build page a." })).await;
        assert_eq!(made["id"], "bad");
        let run = client.settled("bad", "planning").await;
        assert!(run["error"].as_str().unwrap().contains("no pages"), "{run}");
        let (_, listed) = client.get("/api/runs").await;
        assert_eq!(listed["runs"][0]["id"], "bad");
    }

    #[tokio::test]
    async fn a_daemon_is_a_commons_hub_other_machines_push_to_and_pull_from() {
        use crate::commons::HttpCommons;
        use tm_dictionary::CommonsRemote;
        let tmp = repo("api-commons");
        let world = World::new(Mode::Write);
        let (server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        let token = paired["token"].as_str().unwrap().to_string();
        assert_eq!(client.get("/api/commons/entries").await.0, 401);
        client.token = Some(token.clone());
        assert_eq!(client.get("/api/commons/entries").await.1, json!({ "entries": [] }));

        // One machine promotes an entry and sends it to the hub.
        let sender = repo("commons-sender");
        let local = LocalStore::open(sender.0.join("commons")).unwrap();
        local.publish("card", EntryKind::Pattern, "a card", "export const Card = 1;\n", "next").unwrap();
        let mut entry = local.get("card").unwrap().unwrap();
        entry.stats = Some(EntryStats { reuses: 40, pass_rate: 1.0, passes: 40, fails: 0 });
        let hub = HttpCommons::new(&server.url, &token);
        let sent = entry.clone();
        let hub = tokio::task::spawn_blocking(move || hub.push(&sent).map(|_| hub)).await.unwrap().unwrap();

        // The hub keeps it in its Commons, with counts it has not earned yet.
        let (_, listed) = client.get("/api/commons/entries").await;
        assert_eq!(listed["entries"][0]["key"], "card");
        assert_eq!(listed["entries"][0]["tier"], "commons");
        assert_eq!(listed["entries"][0]["stats"]["passes"], 0);
        let (_, again) = client.post("/api/commons/entries", serde_json::to_value(&entry).unwrap()).await;
        assert_eq!(again["added"], false);

        // Content that does not match its hash, or a key that is a path, is refused.
        let mut forged = entry.clone();
        forged.content = "export const Card = 2;\n".into();
        assert_eq!(client.post("/api/commons/entries", serde_json::to_value(&forged).unwrap()).await.0, 400);
        let mut escape = entry.clone();
        escape.key = "../card".into();
        assert_eq!(client.post("/api/commons/entries", serde_json::to_value(&escape).unwrap()).await.0, 400);

        // Another machine pulls it once.
        let receiver = repo("commons-receiver");
        let (first, second) = tokio::task::spawn_blocking(move || {
            let theirs = LocalStore::open(receiver.0.join("commons")).unwrap();
            let first = hub.pull(&theirs).unwrap();
            let second = hub.pull(&theirs).unwrap();
            assert_eq!(theirs.get("card").unwrap().unwrap().content, "export const Card = 1;\n");
            (first, second)
        })
        .await
        .unwrap();
        assert_eq!((first, second), (1, 0));

        let nowhere = HttpCommons::new("http://127.0.0.1:9", "x");
        let error = tokio::task::spawn_blocking(move || nowhere.pull(&local).unwrap_err()).await.unwrap();
        assert!(format!("{error:#}").contains("cannot reach the Commons"), "{error:#}");
    }

    #[tokio::test]
    async fn the_built_frontend_is_served_beside_the_api() {
        let tmp = repo("api-web");
        let web = tmp.0.join("web-dist");
        std::fs::create_dir_all(web.join("assets")).unwrap();
        std::fs::write(web.join("index.html"), "<div id=app></div>").unwrap();
        std::fs::write(web.join("assets/app-1.js"), "console.log(1)").unwrap();
        std::fs::write(tmp.0.join("secret.txt"), "no").unwrap();
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        options.web = Some(web);
        let server = Server::serve(options).await.unwrap();
        let client = Client { addr: server.url.trim_start_matches("http://").trim_end_matches('/').to_string(), token: None };

        assert_eq!(client.text("/").await, (200, "<div id=app></div>".into()));
        // A page the app routes itself gets the app.
        assert_eq!(client.text("/runs/web").await, (200, "<div id=app></div>".into()));
        assert_eq!(client.text("/assets/app-1.js").await, (200, "console.log(1)".into()));
        assert_eq!(client.text("/assets/gone.js").await.0, 404);
        assert_eq!(client.text("/../secret.txt").await.1, "<div id=app></div>");
        assert_eq!(client.text("/api/nope").await.0, 404);
        assert_eq!(client.get("/api/health").await.0, 200);
    }
}

mod commons_service {
    //! The daemon against the real Commons service, with its database in memory.

    use std::collections::HashMap;

    use super::*;
    use crate::auth::{AccountAuth, Auth};
    use crate::commons::HttpCommons;
    use tm_commons::db::{Db, Identity, Memory, Status};
    use tm_dictionary::CommonsRemote;

    const SEED: [u8; 32] = [11; 32];

    async fn service() -> (String, Arc<Memory>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let vars: HashMap<&str, String> = [
            ("TM_COMMONS_URL", base.clone()),
            ("DATABASE_URL", "memory".to_string()),
            ("TM_COMMONS_SIGNING_KEY", tm_token::Signer::seed_from(&SEED).unwrap()),
            ("TM_COMMONS_SESSION_SECRET", "0123456789abcdef0123456789abcdef".to_string()),
            ("GITHUB_CLIENT_ID", "id".to_string()),
            ("GITHUB_CLIENT_SECRET", "secret".to_string()),
        ]
        .into_iter()
        .collect();
        let config = tm_commons::Config::from_vars(|name| vars.get(name).cloned()).unwrap();
        let db = Arc::new(Memory::default());
        let app = Arc::new(tm_commons::App::new(config, db.clone()).unwrap());
        tokio::spawn(async move { axum::serve(listener, tm_commons::router(app)).await.unwrap() });
        (base, db)
    }

    #[tokio::test]
    async fn entries_go_to_the_commons_come_back_reviewed_earn_a_pass_rate_and_can_be_withdrawn() {
        let (base, db) = service().await;
        let ana = db
            .upsert_user(&Identity { provider: "github".into(), subject: "1".into(), login: Some("ana".into()), email: None, name: None })
            .await
            .unwrap();
        let token = format!("tmc_{}", "ab".repeat(24));
        db.add_token(ana.id, &crate::secret::fingerprint(&token), "laptop").await.unwrap();

        let tmp = repo("commons-service");
        let mine = LocalStore::open(tmp.0.join("mine")).unwrap();
        mine.publish("card", EntryKind::Pattern, "a card", "export const Card = 1;\n", "next@15").unwrap();
        let card = mine.get("card").unwrap().unwrap();

        // Sharing is off unless asked for.
        let quiet = HttpCommons::new(&base, &token).sharing(false);
        let (shared, sent) = (HttpCommons::new(&base, &token).sharing(true), card.clone());
        tokio::task::spawn_blocking(move || {
            quiet.push(&sent).unwrap();
            shared.push(&sent).unwrap();
        })
        .await
        .unwrap();
        assert_eq!(db.submissions(Status::Pending).await.unwrap().len(), 1);

        // Nobody gets it until a moderator approves it.
        let theirs = LocalStore::open(tmp.0.join("theirs")).unwrap();
        let pull = |store: LocalStore| {
            let remote = HttpCommons::new(&base, "");
            async move { tokio::task::spawn_blocking(move || remote.pull(&store).unwrap()).await.unwrap() }
        };
        assert_eq!(pull(theirs.clone()).await, 0);
        db.review("card", &card.hash, Status::Approved, ana.id, None).await.unwrap();
        assert_eq!(pull(theirs.clone()).await, 1);
        let arrived = theirs.get("card").unwrap().unwrap();
        assert_eq!(arrived.tier, DictTier::Commons);

        // A run that used it reports back, and its pass rate is earned there.
        let reporter = HttpCommons::new(&base, &token);
        let used = arrived.clone();
        tokio::task::spawn_blocking(move || reporter.report(&used, true).unwrap()).await.unwrap();
        assert_eq!(db.submission("card", &card.hash).await.unwrap().unwrap().passes, 1);

        // Withdrawn: the next pull deletes it here.
        db.review("card", &card.hash, Status::Rejected, ana.id, Some("unsafe")).await.unwrap();
        assert_eq!(pull(theirs.clone()).await, 0);
        assert!(theirs.get("card").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_daemon_lets_in_a_listed_account_with_a_token_from_the_service() {
        let (base, _db) = service().await;
        let signer = tm_token::Signer::from_seed(&tm_token::Signer::seed_from(&SEED).unwrap()).unwrap();
        let token = |login: &str| {
            signer.sign(&tm_token::Claims {
                iss: base.clone(),
                sub: format!("github:{login}-id"),
                aud: tm_token::DAEMON_AUDIENCE.into(),
                iat: tm_token::now(),
                exp: tm_token::now() + 600,
                login: Some(login.into()),
                email: None,
                name: None,
            })
        };
        let auth = AccountAuth::new(&base, vec!["github:naren".into()]);
        // The keys are fetched in the background.
        let mut seen = None;
        for _ in 0..100 {
            seen = auth.identify(&token("naren"));
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(seen.unwrap().id, "github:naren-id");
        assert!(auth.identify(&token("someone")).is_none());
        assert_eq!(auth.describe()["sign_in"], base.as_str());
    }
}
