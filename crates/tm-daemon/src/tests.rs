//! The daemon against a fake harness. No model is started.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use tm_core::{WorkUnit, 
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
    ConfirmFinding,
    CrossImport,
    /// `Some` waits until that many workers are inside the build at once.
    Parallel(Option<Arc<Barrier>>),
    Cancel(watch::Sender<bool>),
    /// Page `a` is split in two units that must be inside the build together.
    Units(Arc<Barrier>),
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
    /// Page `a`'s first agent stalls waiting on its model.
    Stall,
    /// Pages are UI files; the checker asks for one fix on page `a` and passes the rest.
    Checked,
    /// The planner thinks, keeps calling tools, and never writes, until it is stopped.
    Overthink,
    /// Page `a`'s first build fails because the account has no balance left (HTTP 402).
    Broke,
    /// Every build tries `poetry lock` in a shell, is refused, and changes nothing.
    Refused,
    /// The plan sketches two phases: `a` now, `b` once `a`'s interfaces are locked.
    Phased,
}

struct World {
    mode: Mode,
    calls: Mutex<Vec<String>>,
    /// The fake says its agents run inside the sandbox, so builders get a shell.
    sandboxed: std::sync::atomic::AtomicBool,
    /// For each call: whether it was read-only, and the shell it was given.
    shells: Mutex<Vec<(bool, Option<tm_harness::ShellScope>)>>,
    /// The fake keeps a builder's session open after its turn, like an ACP agent.
    keeps: std::sync::atomic::AtomicBool,
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

    fn sandboxed_shell(&self) -> bool {
        self.0.sandboxed.load(Ordering::SeqCst)
    }

    async fn run_kept(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> (anyhow::Result<RunSummary>, Option<Box<dyn tm_harness::Session>>) {
        let id = task.brief.split("Build page `").nth(1).and_then(|rest| rest.split('`').next()).map(str::to_string);
        let cwd = task.cwd.clone();
        let result = self.run(task, gate, events).await;
        let keep = self.0.keeps.load(Ordering::SeqCst) && result.as_ref().is_ok_and(|s| s.ok);
        match id.filter(|_| keep) {
            Some(id) => (result, Some(Box::new(FakeSession { world: self.0.clone(), id, cwd }))),
            None => (result, None),
        }
    }

    async fn run(
        &self,
        task: WorkerTask,
        _gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let planning = task.brief.contains("Plan this product and lock its contracts.") || task.brief.contains("Plan this small product quickly.");
        if matches!(self.0.mode, Mode::Overthink) && planning && !task.brief.contains("You were stopped because") {
            self.0.calls.lock().unwrap().push(format!("{}\n[effort={}][dictionary={}]", task.brief, task.effort.as_deref().unwrap_or("default"), task.dictionary.is_some()));
            let _ = events.send(HarnessEvent::Thought { text: "One page, a greeting card, cites greet.".into() }).await;
            for n in 0..30 {
                let input = serde_json::json!({ "path": format!("dir{n}") });
                let _ = events.send(HarnessEvent::ToolCall { id: format!("look{n}"), name: "list_dir".into(), input }).await;
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
            anyhow::bail!("the planner was never stopped");
        }
        self.0.shells.lock().unwrap().push((task.read_only, task.shell.clone()));
        if matches!(self.0.mode, Mode::Refused) && task.brief.contains("Build page `") {
            let input = serde_json::json!({ "command": "poetry lock" });
            let _ = events.send(HarnessEvent::ToolCall { id: "sh1".into(), name: "Bash".into(), input }).await;
            let _ = events.send(HarnessEvent::PermissionDecided { tool: "Bash".into(), allowed: false }).await;
        }
        let script = self.0.respond(&task).await?;
        // Thoughts and tool calls go to the transcript, never into the answer.
        let _ = events.send(HarnessEvent::Thought { text: "thinking".into() }).await;
        for (path, _) in &script.files {
            let input = serde_json::json!({ "file_path": path });
            let _ = events.send(HarnessEvent::ToolCall { id: "t1".into(), name: "Write".into(), input }).await;
            let _ = events.send(HarnessEvent::ToolResult {
                id: "t1".into(),
                text: "recorded-tool-result".into(),
                diff: Some("--- a/app/a/page.ts\n+++ b/app/a/page.ts\n+export\n".into()),
            }).await;
            let _ = events.send(HarnessEvent::PermissionDecided { tool: "Write".into(), allowed: true }).await;
        }
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
        Ok(RunSummary { ok: true, usage, cost_usd: Some(0.01), denied: 0, freed: Some(tm_harness::Freed { processes: 1, lingering: 0 }) })
    }
}

/// A builder's session the fake kept open. A follow-up fixes the page the plain way.
struct FakeSession {
    world: Arc<World>,
    id: String,
    cwd: PathBuf,
}

#[async_trait]
impl tm_harness::Session for FakeSession {
    async fn follow_up(&mut self, text: String, events: mpsc::Sender<HarnessEvent>, _timeout: Duration) -> anyhow::Result<RunSummary> {
        self.world.calls.lock().unwrap().push(format!("[follow-up {}] {text}", self.id));
        std::fs::create_dir_all(self.cwd.join(format!("app/{}", self.id)))?;
        std::fs::write(self.cwd.join(format!("app/{}/page.ts", self.id)), format!("export const page = \"{}\"; // fixed\n", self.id))?;
        let _ = events.send(HarnessEvent::Text { text: "fixed".into() }).await;
        Ok(RunSummary { ok: true, usage: Usage { input: 10, output: 5, cache_read: 90, cache_write: 0 }, cost_usd: Some(0.001), denied: 0, freed: None })
    }

    async fn close(self: Box<Self>) -> tm_harness::Freed {
        tm_harness::Freed::default()
    }
}

impl World {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(World {
            mode,
            calls: Mutex::new(Vec::new()),
            sandboxed: std::sync::atomic::AtomicBool::new(false),
            shells: Mutex::new(Vec::new()),
            keeps: std::sync::atomic::AtomicBool::new(false),
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
        // How the call was made rides on the recorded brief, after the text the daemon sent.
        let skills = std::fs::read_dir(task.cwd.join(".taskmaster/skills")).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>().join(",")).unwrap_or_default();
        let how = format!("\n[effort={}][dictionary={}][skills={skills}]", task.effort.as_deref().unwrap_or("default"), task.dictionary.is_some());
        self.calls.lock().unwrap().push(format!("{brief}{how}"));
        if brief.contains("Plan this product and lock its contracts.") || brief.contains("Plan this small product quickly.") || brief.contains("Repair this plan.") {
            return Ok(Script::text(&self.plan_reply()));
        }
        if brief.contains("Plan phase 2 of 2") {
            return Ok(Script::text(r#"{"contracts":[],"units":[{"id":"b","route":"/b","owned_glob":"app/b/**","cites":[],"brief":"Implement b on top of a."}],"dictionary":[]}"#));
        }
        if brief.contains("Read these notes and decide what the agents should do next.") {
            let echoed: String = brief.lines().filter(|line| line.starts_with("- to ")).map(|line| format!("{line}\n")).collect();
            return Ok(Script::text(&format!("Follow the notes.\n{echoed}")));
        }
        if let Mode::Units(barrier) = &self.mode
            && let Some(unit) = brief.split("You are unit `").nth(1).and_then(|rest| rest.split('`').next())
        {
            let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_inflight.fetch_max(now, Ordering::SeqCst);
            barrier.wait().await;
            let (path, body) = if unit == "markup" { ("app/a/index.html", "<h1>a</h1>\n") } else { ("app/a/main.js", "console.log(1);\n") };
            return Ok(Script { text: format!("done\nRELEASE page:a/{unit}"), files: vec![(path.into(), Some(body.into()))] });
        }
        if let Some(id) = brief.split("Build page `").nth(1).or_else(|| brief.split("Build unit `").nth(1)).and_then(|rest| rest.split('`').next()) {
            if matches!(self.mode, Mode::Broke) && id == "a" && self.a.fetch_add(1, Ordering::SeqCst) == 0 {
                bail!(r#"`session/prompt` failed: {{"code":-32603,"message":"API error (status 402 Payment Required): Grok Build usage balance exhausted"}}"#);
            }
            if matches!(self.mode, Mode::Stall) && id == "a" && self.a.load(Ordering::SeqCst) == 0 {
                assert!(task.stall.is_some(), "a worker runs with a stall limit");
                self.a.fetch_add(1, Ordering::SeqCst);
                return Err(tm_harness::Stalled(Duration::from_secs(60)).into());
            }
            return self.build(id).await;
        }
        if brief.contains("objected to Dictionary") {
            return self.arbitrate();
        }
        if brief.contains("Check one page another agent has just built") {
            assert!(task.read_only && task.cwd.join("diff.patch").is_file(), "the checker reads the diff in its own directory");
            let first = self.reviews.fetch_add(1, Ordering::SeqCst) == 0;
            if brief.contains("Page `a`") && first {
                return Ok(Script::text("One thing.\n- app/a/page.tsx: the total is never shown"));
            }
            return Ok(Script::text("pass"));
        }
        if brief.contains("Decide whether it is real") {
            let real = matches!(self.mode, Mode::ConfirmFinding);
            return Ok(Script::text(if real { "{\"real\": true}" } else { "{\"real\": false}" }));
        }
        if brief.contains("Decide independently for EACH numbered claim") {
            let real = matches!(self.mode, Mode::ConfirmFinding);
            let count = brief.lines().filter(|l| l.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()) && l.contains(" at line ")).count();
            let items: Vec<String> = (0..count).map(|i| format!("{{\"index\":{i},\"real\":{real}}}")).collect();
            return Ok(Script::text(&format!("[{}]", items.join(","))));
        }
        if brief.contains("the output names no file any unit owns") {
            // The master's fix: a file the project's build looks for.
            return Ok(Script { text: "fixed".into(), files: vec![("fixed.txt".into(), Some("ok\n".into()))] });
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
        if brief.contains("reviewing one connection") || brief.contains("reviewing all connections") || brief.contains("security problems") {
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
            Mode::Phased => format!(
                r#"{{"kind":"web","phases":[{{"id":"core","goal":"the shared core","test":"npm test"}},{{"id":"rest","goal":"the rest on top of it"}}],
                "contracts":[{{"path":"contracts/types.ts","content":"export type Id = string;\n"}}],
                "units":[{{"id":"a","route":"/a","owned_glob":"app/a/**","cites":["greet"],"brief":"Implement a."}}],"dictionary":[{greet}]}}"#
            ),
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
            Mode::Nothing | Mode::Refused => Ok(Script::text("I made no edits to this page.")),
            Mode::Learn if id == "a" && n < 3 => Ok(Script::text("no files yet")),
            Mode::Learn if id == "a" => Ok(Script {
                text: "Read the route param before rendering the header.".into(),
                files: vec![("app/a/page.ts".into(), Some(format!("export const a = {n};\n")))],
            }),
            Mode::Rebuild => Ok(Script::page(id, &format!("export const rebuilt = \"{id}\";\n"))),
            Mode::Checked => Ok(Script {
                text: "done".into(),
                files: vec![(format!("app/{id}/page.tsx"), Some(format!("export const page = \"{id}\"; // attempt {n}\n")))],
            }),
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
        let finding = matches!(self.mode, Mode::ConfirmFinding) && n == 0;
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
                ..Default::default()
            })
            .collect(),
        estimate: Estimate { tokens: 10, usd: 0.01, ..Default::default() },
        ..Default::default()
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

/// Page `a` is written with a type error the first time and fixed when its errors come
/// back. Page `b` holds its build open until `a`'s repair has started, so `a` can only be
/// repaired in time if its own files are checked as soon as it merges.
struct EarlyChecked {
    inner: Fake,
    order: Arc<Mutex<Vec<String>>>,
    repaired: Arc<Notify>,
}

#[async_trait]
impl Harness for EarlyChecked {
    fn id(&self) -> &'static str {
        "fake"
    }

    async fn run(&self, task: WorkerTask, gate: Arc<dyn PermissionGate>, events: mpsc::Sender<HarnessEvent>) -> anyhow::Result<RunSummary> {
        let page = task.brief.split("Build page `").nth(1).and_then(|r| r.split('`').next()).map(str::to_string);
        let Some(page) = page else { return self.inner.run(task, gate, events).await };
        let body = match page.as_str() {
            "a" if task.brief.contains("TS2304") => {
                self.order.lock().unwrap().push("a repaired".into());
                self.repaired.notify_one();
                "export const a: string = \"a\";\n"
            }
            "a" => {
                self.order.lock().unwrap().push("a built".into());
                "export const a: string = oops;\n"
            }
            _ => {
                let waited = tokio::time::timeout(Duration::from_secs(8), self.repaired.notified()).await;
                self.order.lock().unwrap().push(if waited.is_ok() { "b built".into() } else { "b gave up waiting".into() });
                "export const b: string = \"b\";\n"
            }
        };
        let file = task.cwd.join(format!("app/{page}/page.ts"));
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(file, body)?;
        let usage = Usage { input: 100, output: 20, cache_read: 0, cache_write: 0 };
        Ok(RunSummary { ok: true, usage, cost_usd: Some(0.01), denied: 0, freed: None })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_merged_page_is_checked_and_sent_back_while_other_pages_build() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = seeded("early-check");
    std::fs::write(tmp.0.join("tsconfig.json"), "{}\n").unwrap();
    std::fs::write(tmp.0.join(".gitignore"), "node_modules/\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "typescript"]);
    // A stand-in for `tsc`: every `oops` under app/ is an unknown name.
    let bin = tmp.0.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tsc = bin.join("tsc");
    std::fs::write(
        &tsc,
        "#!/bin/sh\ngrep -rn oops app 2>/dev/null | while IFS=: read -r file line rest; do echo \"$file($line,1): error TS2304: Cannot find name 'oops'.\"; done\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&tsc, std::fs::Permissions::from_mode(0o755)).unwrap();

    let world = World::new(Mode::Write);
    let order = Arc::new(Mutex::new(Vec::new()));
    let harness: Arc<dyn Harness> = Arc::new(EarlyChecked { inner: Fake(world.clone()), order: order.clone(), repaired: Arc::new(Notify::new()) });
    let harnesses = Harnesses { worker: harness.clone(), mid: harness.clone(), master: harness.clone(), reviewer: harness.clone(), verifier: harness };
    let config = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2);
    let outcome = tokio::time::timeout(Duration::from_secs(30), drive(config, harnesses)).await.expect("the run finished").unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(*order.lock().unwrap(), vec!["a built", "a repaired", "b built"], "a was repaired while b was still building");
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
}

/// Builds page `a` without the `total` its locked contract test imports, then with it once
/// the test's failure comes back. Every brief is kept.
struct ContractTested {
    inner: Fake,
    briefs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for ContractTested {
    fn id(&self) -> &'static str {
        "fake"
    }

    async fn run(&self, task: WorkerTask, gate: Arc<dyn PermissionGate>, events: mpsc::Sender<HarnessEvent>) -> anyhow::Result<RunSummary> {
        self.briefs.lock().unwrap().push(task.brief.clone());
        if !task.brief.contains("Build page `a`") {
            return self.inner.run(task, gate, events).await;
        }
        let body = if task.brief.contains("TS2305") { "export const total = (n: number[]) => n.length;\n" } else { "export const count = 0;\n" };
        let file = task.cwd.join("app/a/page.ts");
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(file, body)?;
        let usage = Usage { input: 100, output: 20, cache_read: 0, cache_write: 0 };
        Ok(RunSummary { ok: true, usage, cost_usd: Some(0.01), denied: 0, freed: None })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_failing_contract_test_goes_to_the_unit_it_imports_not_to_the_master() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = seeded("contract-test");
    std::fs::write(tmp.0.join("tsconfig.json"), "{}\n").unwrap();
    std::fs::write(tmp.0.join(".gitignore"), "node_modules/\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "typescript"]);
    // A stand-in for `tsc`: the contract test cannot import `total` until page a exports it.
    let bin = tmp.0.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tsc = bin.join("tsc");
    std::fs::write(
        &tsc,
        "#!/bin/sh\ngrep -q 'export const total' app/a/page.ts 2>/dev/null || echo \"tests/contract/a.test.ts(1,10): error TS2305: Module '../../app/a/page' has no exported member 'total'.\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&tsc, std::fs::Permissions::from_mode(0o755)).unwrap();

    let test_source = "import { total } from \"../../app/a/page\";\nexport const checks = total([1, 2]) === 2;\n";
    let mut plan = plan(&[("a", &[])]);
    plan.contracts.push(ContractFile { path: "tests/contract/a.test.ts".into(), content: test_source.into() });
    let world = World::new(Mode::Write);
    let briefs = Arc::new(Mutex::new(Vec::new()));
    let harness: Arc<dyn Harness> = Arc::new(ContractTested { inner: Fake(world.clone()), briefs: briefs.clone() });
    let harnesses = Harnesses { worker: harness.clone(), mid: harness.clone(), master: harness.clone(), reviewer: harness.clone(), verifier: harness };
    let outcome = tokio::time::timeout(Duration::from_secs(30), drive(config(&tmp.0, plan, true, 1), harnesses)).await.expect("the run finished").unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let briefs = briefs.lock().unwrap().clone();
    let builds: Vec<&String> = briefs.iter().filter(|b| b.contains("Build page `a`")).collect();
    assert_eq!(builds.len(), 2, "page a was built, then repaired");
    assert!(builds[0].contains("`tests/contract/a.test.ts` are the acceptance check"), "the test is named as the oracle up front");
    assert!(builds[1].contains("TS2305") && builds[1].contains("Never edit the test"), "{}", builds[1]);
    assert!(!briefs.iter().any(|b| b.contains("Shared files this run wrote fail")), "the master was not asked to rewrite the test");
    assert_eq!(read(&tmp.0.join("tests/contract/a.test.ts")), test_source);
}

/// The project's test script fails the first time it runs and passes after that.
#[tokio::test]
async fn a_test_that_fails_once_and_then_passes_is_flaky_and_gets_no_repair() {
    let npm = std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join("npm").is_file()));
    if !npm {
        eprintln!("npm is missing; skipping");
        return;
    }
    let tmp = seeded("flaky");
    std::fs::write(tmp.0.join("package.json"), r#"{"name":"flaky","private":true,"scripts":{"test":"sh flaky.sh"}}"#).unwrap();
    std::fs::write(tmp.0.join("flaky.sh"), "if [ -f .ran-once ]; then exit 0; fi\ntouch .ran-once\necho 'timeout waiting for port 3000' >&2\nexit 1\n").unwrap();
    std::fs::write(tmp.0.join(".gitignore"), "node_modules/\n.ran-once\n").unwrap();
    std::fs::create_dir_all(tmp.0.join("node_modules")).unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "tests"]);
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    assert_eq!(calls.iter().filter(|c| c.contains("Build page `a`")).count(), 1, "the page was not repaired");
    assert!(!calls.iter().any(|c| c.contains("The project's own build or tests fail")), "the master was not asked to repair a flaky test");
}

#[tokio::test]
async fn a_stalled_agent_is_replaced_by_a_fresh_one() {
    let tmp = repo("stall");
    let world = World::new(Mode::Stall);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let builds = world.calls.lock().unwrap().iter().filter(|c| c.contains("Build page `a`")).count();
    assert_eq!(builds, 2);
    let agents = read(&run_directory(&tmp.0, "current").join("agents.jsonl"));
    assert!(agents.contains("\"kind\":\"stalled\""), "{agents}");
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
async fn a_page_split_in_units_runs_them_in_parallel_and_merges_once() {
    let tmp = repo("units");
    let world = World::new(Mode::Units(Arc::new(Barrier::new(2))));
    let mut split = plan(&[("a", &[])]);
    split.pages[0].units = vec![
        WorkUnit { id: "markup".into(), owned_glob: "app/a/index.html".into(), brief: "The markup.".into(), ..Default::default() },
        WorkUnit { id: "logic".into(), owned_glob: "app/a/main.js".into(), brief: "The script.".into(), done: "clicking add shows a todo".into(), ..Default::default() },
    ];
    let outcome = run(config(&tmp.0, split, true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    // Each unit's brief is self-contained: its own task and done line, the contracts in
    // full, the other unit only by the files it owns.
    let calls = world.calls.lock().unwrap().clone();
    let logic = calls.iter().find(|c| c.contains("You are unit `logic`")).unwrap();
    assert!(logic.contains("You are done when: clicking add shows a todo."), "{logic}");
    assert!(logic.contains("export type Id = string;"), "{logic}");
    assert!(logic.contains("Unit `markup` (another worker) owns `app/a/index.html`.") && !logic.contains("The markup."), "{logic}");
    assert!(logic.contains("None of your files exist yet") && logic.contains("do not list, search or read other files"), "{logic}");
    assert!(!logic.contains("Search the Commons"), "{logic}");
    assert!(logic.contains("end your reply with the line `RELEASE page:a/logic`"), "{logic}");
    assert!(world.max_inflight.load(Ordering::SeqCst) >= 2);
    assert!(read(&tmp.0.join("app/a/index.html")).contains("<h1>"));
    assert!(read(&tmp.0.join("app/a/main.js")).contains("console.log"));
    assert_eq!(log(&tmp.0).matches("taskmaster: built page a").count(), 1);
    // Each unit is a separate agent in the transcript, tagged with its unit and glob.
    let said = read(&run_directory(&tmp.0, "current").join("agents.jsonl"));
    let starts: Vec<serde_json::Value> = said
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|e| e["kind"] == "start" && e["agent"]["page"] == "a")
        .collect();
    let mut ids: Vec<&str> = starts.iter().filter_map(|e| e["agent"]["id"].as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["page:a/logic", "page:a/markup"], "{said}");
    let markup = starts.iter().find(|e| e["agent"]["unit"] == "markup").unwrap();
    assert_eq!(markup["agent"]["owned_glob"], "app/a/index.html");
    // Each unit reported done, asked to be released, and was freed with nothing left running.
    let released: Vec<serde_json::Value> = said
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|e| e["kind"] == "released" && e["agent"]["page"] == "a")
        .collect();
    assert_eq!(released.len(), 2, "{said}");
    assert!(released.iter().all(|e| e["asked"] == true && e["lingering"] == 0), "{said}");
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
    let Outcome::NeedsApproval { summary } = &outcome else { panic!("{outcome:?}") };
    assert!(summary.contains("plan: 2 agents"), "{summary}");
    assert!(summary.contains("page:a owns `app/a/**`") && summary.contains("| done when "), "{summary}");
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
    // A different master model, so there is a ladder: a page that keeps failing climbs it.
    let mut cfg = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1);
    cfg.models.master = Some("stronger".into());
    let outcome = run(cfg, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let page_b = calls.iter().find(|call| call.contains("Build page `b`")).expect("page b ran");
    assert!(page_b.contains("changed none of your files"), "{page_b}");
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
        .filter(|call| call.contains("Plan this product and lock its contracts.") || call.contains("Plan this small product quickly.") || call.contains("Repair this plan."))
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
    assert!(planner_calls(&world)[0].contains("Dictionary is empty"));
    // The small pass's first build runs at low effort too.
    let builds: Vec<String> = world.calls.lock().unwrap().iter().filter(|c| c.contains("Build page `a`")).cloned().collect();
    assert!(!builds.is_empty() && builds[0].contains("[effort=low][dictionary=true]"), "{builds:?}");
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
async fn a_planner_that_keeps_looking_is_stopped_and_writes_the_plan_from_its_notes() {
    let tmp = repo("overthink");
    git(&tmp.0, &["commit", "--allow-empty", "-q", "-m", "init"]);
    let world = World::new(Mode::Overthink);
    let outcome = run(prompt_config(&tmp.0, "Build page a with a greeting.", true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = planner_calls(&world);
    assert_eq!(calls.len(), 2, "{calls:#?}");
    assert!(calls[1].contains("You were stopped because it had made"), "{}", calls[1]);
    assert!(calls[1].contains("Your notes so far:\nOne page, a greeting card, cites greet."), "{}", calls[1]);
    assert!(calls[1].contains("[effort=low][dictionary=false]"), "{}", calls[1]);
}

#[tokio::test]
async fn an_empty_project_is_planned_without_tools_and_told_to_decide() {
    let tmp = repo("fresh");
    git(&tmp.0, &["commit", "--allow-empty", "-q", "-m", "init"]);
    let world = World::new(Mode::Plan);
    // Long enough for the full planning pass rather than the small one.
    let prompt = format!("Build page a with a greeting. {}", "It is a card with a title, a short line of text, and a button. ".repeat(5));
    let outcome = run(prompt_config(&tmp.0, &prompt, true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let first = &planner_calls(&world)[0];
    assert!(first.contains("Plan this product and lock its contracts."), "{first}");
    assert!(first.contains("you need no tools") && first.contains("Decide quickly"), "{first}");
    assert!(first.contains("[effort=medium][dictionary=false]"), "{first}");
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
    // The second call repairs the first plan: it carries that plan and needs no tools.
    assert!(calls[1].contains("Repair this plan.") && calls[1].contains("\"units\""), "{}", calls[1]);
    assert!(calls[1].contains("[effort=low][dictionary=false]"), "{}", calls[1]);
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
    // A stored entry no longer forces the slow pass: it is listed, and the master gets no tools.
    assert!(calls[0].contains("Plan this small product quickly."), "{}", calls[0]);
    assert!(calls[0].contains("[effort=low][dictionary=false]"), "{}", calls[0]);
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
    assert!(err.to_string().contains("no units"), "{err}");
    // The first plan, then two repairs of it, then the run gives up.
    assert_eq!(world.plans.load(Ordering::SeqCst), 3);
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
    assert!(roles.contains(&"worker"), "worker missing from {roles:?}");
    assert!(!roles.contains(&"reviewer"), "a run does not review itself: {roles:?}");
    assert!(entries.iter().any(|entry| entry.role.name() == "worker" && entry.page.as_deref() == Some("a")));
    let status = records.status("current").unwrap().unwrap();
    assert_eq!(status["spent"]["calls"], entries.len());
    assert_eq!(status["spent"]["tokens"], entries.len() * 120);
    assert_eq!(status["spent"]["unpriced_calls"], 0);
    // One web page and no review, sized from the plan because nothing ran here before,
    // with the cache reads counted apart.
    assert_eq!(status["estimate"]["tokens"], 40_000);
    // (plus the contract every builder carries in its prompt)
    assert!((120_000..120_100).contains(&status["estimate"]["cache_read"].as_u64().unwrap()), "{status}");
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
async fn a_failure_no_unit_owns_goes_to_the_master_and_is_never_merged() {
    let Some(tmp) = buildable("build-elsewhere", "lib/legacy.ts:3:1 something old is broken", "false") else { return };
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    // It still fails after the master's repairs: the run fails, and the exit code says so.
    assert_eq!(outcome, Outcome::Finished { ok: false });
    let calls = world.calls.lock().unwrap().clone();
    let repairs: Vec<&String> = calls.iter().filter(|call| call.contains("the output names no file any unit owns")).collect();
    assert_eq!(repairs.len(), 2, "the master gets every repair the run has");
    assert!(repairs[0].contains("something old is broken"), "{}", repairs[0]);
    // Nothing was merged: the project is as it was, and the work stays on the run's branch.
    assert!(!tmp.0.join("app/a/page.ts").exists());
    assert!(!tmp.0.join("fixed.txt").exists());
    assert!(git_out(&tmp.0, &["branch", "--list", "taskmaster/*"]).contains("taskmaster/current"));
    let status = Records::files(&tmp.0).status("current").unwrap().unwrap();
    assert_eq!(status["ok"], false);
    assert_ne!(status["merged"], true);
}

#[tokio::test]
async fn the_master_repairs_a_failure_no_unit_owns_and_the_run_passes() {
    let Some(tmp) = buildable("build-unowned-fixed", "the build needs fixed.txt", "test -f fixed.txt") else { return };
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(read(&tmp.0.join("fixed.txt")), "ok\n", "the master's fix was merged with the run");
    assert!(log(&tmp.0).contains("taskmaster: repair a failure no unit owns"));
}

#[tokio::test]
async fn every_worker_brief_opens_with_the_same_shared_prefix() {
    let tmp = repo("prefix");
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"]), ("b", &["greet"])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let brief = |id: &str| calls.iter().find(|c| c.contains(&format!("Build page `{id}`"))).cloned().unwrap();
    let (a, b) = (brief("a"), brief("b"));
    let cut = |text: &str| text.split("--- Your task ---").next().unwrap().to_string();
    assert_eq!(cut(&a), cut(&b), "the shared part is identical, so the provider's cache can serve it");
    assert!(cut(&a).contains("export type Id = string;"), "the contracts are in it, in full");
    assert!(!cut(&a).contains("Build page"), "nothing unit-specific is in it");
    assert!(a.contains("Work down this list:"));
}

#[tokio::test]
async fn a_run_reports_its_stages_tokens_and_cost_from_its_own_records() {
    let tmp = repo("report");
    let world = World::new(Mode::Plan);
    let outcome = run(prompt_config(&tmp.0, "Build page a with a greeting.", true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let report = crate::report::build(&Records::files(&tmp.0), "current").unwrap();
    let names: Vec<&str> = report.stages.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names.first(), Some(&"planning"));
    assert!(names.contains(&"build page:a"), "{names:?}");
    assert!(names.iter().any(|n| n.starts_with("verify")));
    assert_eq!(report.stages[0].calls, 1);
    assert_eq!(report.outcome, "finished");
    assert_eq!(report.fresh, 240, "a planner call and a builder call, 120 each");
    assert_eq!(report.cost, "$0.02 (2 priced by the provider)");
    assert!(report.tool_calls >= 1 && report.run_code_calls == 0);
    let text = crate::report::render(&report);
    assert!(text.contains("| Stage | Time |") && text.contains("Planning share:") && text.contains("cache-hit ratio"), "{text}");
}

#[tokio::test]
async fn streamed_fragments_are_one_transcript_line_per_message() {
    let tmp = repo("merge");
    let live = Arc::new(crate::live::Live::new(Arc::new(crate::store::FileStore::in_project(&tmp.0))));
    let agent = serde_json::json!({ "id": "unit:calc" });
    let (tx, rx) = mpsc::channel(256);
    let heard = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let collect = crate::engine::collect_events(Some((live.clone(), "r".into(), agent)), None, rx, heard);
    for i in 0..50 {
        tx.send(HarnessEvent::Thought { text: format!("t{i} ") }).await.unwrap();
    }
    tx.send(HarnessEvent::ToolCall { id: "1".into(), name: "read".into(), input: serde_json::json!({ "path": "a.py" }) }).await.unwrap();
    for i in 0..50 {
        tx.send(HarnessEvent::Text { text: format!("w{i} ") }).await.unwrap();
    }
    drop(tx);
    let (text, _) = collect.await.unwrap();
    assert!(text.starts_with("w0 w1 ") && text.ends_with("w49 "), "the answer keeps every fragment");
    let (_, lines) = live.read("r", 0);
    let kinds: Vec<&str> = lines.iter().map(|l| l["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["thought", "tool_call", "text"], "one line per message, not 101");
    assert!(lines[0]["text"].as_str().unwrap().ends_with("t49 "));
}

#[test]
fn the_cost_line_says_where_each_price_came_from_and_never_fakes_zero() {
    use crate::engine::{cost_words, dollars};
    use tm_core::{LedgerEntry, Role};
    let call = |cost: Option<f64>, by: Option<&str>| LedgerEntry {
        at: String::new(),
        role: Role::Worker,
        page: Some("a".into()),
        harness: "grok-build".into(),
        model: None,
        input: 1000,
        output: 100,
        cache_read: 0,
        cache_write: 0,
        cost_usd: cost,
        seconds: 1.0,
        ok: true,
        refused: vec![],
        agent: None,
        unit: None,
        priced_by: by.map(str::to_string),
        brief_hash: None,
        difficulty: None,
        language: None,
    };
    assert!(cost_words(&[call(None, None)]).starts_with("unpriced"), "no price is not $0.00");
    assert!(cost_words(&[call(Some(0.0), Some("provider"))]).contains("charged nothing (a free model)"));
    let mixed = cost_words(&[call(Some(0.5), Some("provider")), call(Some(0.25), Some("prices")), call(None, None)]);
    assert_eq!(mixed, "$0.75 (1 priced by the provider, 1 from the price table, 1 unpriced)");
    assert!(dollars(&tm_core::Estimate { tokens: 1, ..Default::default() }).starts_with("unpriced"));
    assert_eq!(dollars(&tm_core::Estimate { tokens: 1, usd: 1.234, priced: true, ..Default::default() }), "$1.23");
    // Grok reports no price, so the built-in table prices it as the benchmark's grader does.
    let grok = crate::settings::built_in_prices()["grok-build"];
    assert!((grok.cost(1_000_000, 1_000_000, 1_000_000) - 8.5).abs() < 1e-9);
}

#[test]
fn a_silent_agent_gets_a_heartbeat_line_every_two_minutes() {
    use crate::engine::beat_due;
    let s = Duration::from_secs;
    assert!(!beat_due(s(60), None), "a minute of thinking is not news");
    assert!(beat_due(s(120), None), "two minutes of nothing is");
    assert!(!beat_due(s(150), Some((s(30), false))), "said 30s ago, about this same silence");
    assert!(beat_due(s(250), Some((s(130), false))), "and again two minutes later");
    assert!(beat_due(s(125), Some((s(200), true))), "a new silence after the agent spoke is reported anew");
}

#[test]
fn only_commons_entries_that_bear_on_the_request_reach_the_planner() {
    let entry = |key: &str, summary: &str, content: &str, tier: DictTier| tm_core::DictionaryEntry {
        key: key.into(),
        tier,
        kind: EntryKind::Hint,
        summary: summary.into(),
        content: content.into(),
        hash: String::new(),
        stack: String::new(),
        stats: None,
    };
    let todo = || vec![
        entry("todo-schema", "todo items in localStorage", "Todos persist in localStorage under the key todos.", DictTier::Commons),
        entry("todo-dom", "todo list markup", "ul#todo-list with one li per todo item", DictTier::Commons),
        entry("project-note", "this project's own note", "anything", DictTier::Local),
    ];
    let keys = |found: Vec<tm_core::DictionaryEntry>| found.into_iter().map(|e| e.key).collect::<Vec<_>>();
    // A document database has nothing to do with a todo app: only the project's own entry.
    let db = "Build tinydb, a small document database in Python with tables, queries and a JSON storage file.";
    assert_eq!(keys(crate::engine::relevant_entries(db, todo())), vec!["project-note"]);
    // A todo app gets them, and an entry named by its key always comes.
    let app = "A todo list page: add todo items, tick them done, and keep them in localStorage.";
    let found = keys(crate::engine::relevant_entries(app, todo()));
    assert!(found.contains(&"todo-schema".to_string()) && found.contains(&"todo-dom".to_string()), "{found:?}");
    assert!(keys(crate::engine::relevant_entries("Use the todo-dom entry.", todo())).contains(&"todo-dom".to_string()));
}

#[tokio::test]
async fn a_refused_shell_with_no_changes_is_not_retried_and_says_why() {
    let tmp = seeded("refused");
    let world = World::new(Mode::Refused);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: false });
    let builds = world.calls.lock().unwrap().iter().filter(|c| c.contains("Build page `a`")).count();
    assert_eq!(builds, 1, "the same refusal would meet every retry, so there is none");
    let attempts = Records::files(&tmp.0).attempts("current", "a").unwrap();
    let failure = attempts.last().and_then(|a| a.failure.clone()).unwrap_or_default();
    assert!(failure.contains("Bash poetry lock") && failure.contains("sandbox.shell"), "{failure}");
    // The ledger keeps what was refused beside whether the turn finished.
    let ledger = read(&run_directory(&tmp.0, "current").join("ledger.jsonl"));
    let row: serde_json::Value = ledger.lines().map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()).find(|r| r["page"] == "a").unwrap();
    assert_eq!(row["ok"], true);
    assert_eq!(row["refused"], serde_json::json!(["Bash poetry lock"]));
}

#[tokio::test]
async fn a_sketched_run_builds_phase_by_phase_and_keeps_project_md() {
    let tmp = seeded("phased");
    let world = World::new(Mode::Phased);
    let outcome = run(prompt_config(&tmp.0, &format!("Build a product with a shared core and pages on top. {}", "It has many parts. ".repeat(20)), true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let first = calls.iter().find(|c| c.contains("Plan this product")).unwrap();
    assert!(first.contains("`phases`") && first.contains("Sketch first"), "the first planner is asked for a sketch");
    let phase = calls.iter().find(|c| c.contains("Plan phase 2 of 2")).expect("the second phase was planned on its own");
    assert!(phase.contains("PROJECT.md") && phase.contains("`a` owns `app/a/**`"), "{phase}");
    // Phase two's unit was built after phase one's, never before.
    let a = calls.iter().position(|c| c.contains("Build page `a`")).unwrap();
    let b = calls.iter().position(|c| c.contains("Build page `b`")).unwrap();
    assert!(a < b);
    assert!(calls[b].contains("PROJECT.md, the plan of the whole project"), "workers share the sketch");
    assert!(calls[a].contains("run `npm test`") || calls[a].contains("check your files"), "{}", calls[a]);
    let md = read(&tmp.0.join("PROJECT.md"));
    assert!(md.contains("1. `core` (done)") && md.contains("2. `rest` (done)"), "{md}");
    assert!(md.contains("### Phase 1: `core`") && md.contains("`app/a/page.ts`") && md.contains("`app/b/page.ts`"), "{md}");
    assert!(tmp.0.join("app/b/page.ts").is_file());
    // Each finished phase is a checkpoint the run can roll back to.
    assert!(!git_out(&tmp.0, &["for-each-ref", "refs/taskmaster/current/phase-1"]).trim().is_empty());
    assert!(log(&tmp.0).contains("taskmaster: phase 1 landed"));
}

#[tokio::test]
async fn a_project_md_of_the_projects_own_is_never_overwritten() {
    let tmp = repo("own-md");
    std::fs::write(tmp.0.join("PROJECT.md"), "# Ours\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "ours"]);
    let world = World::new(Mode::Phased);
    let outcome = run(prompt_config(&tmp.0, &format!("Build a product. {}", "It has many parts. ".repeat(20)), true), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert_eq!(read(&tmp.0.join("PROJECT.md")), "# Ours\n");
}

#[tokio::test]
async fn a_repair_goes_to_the_builders_open_session() {
    let tmp = seeded("warm");
    let world = World::new(Mode::CrossImport);
    world.keeps.store(true, Ordering::SeqCst);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let fresh = calls.iter().filter(|c| c.contains("Build page `a`")).count();
    assert_eq!(fresh, 1, "the repair was not a fresh agent");
    let follow = calls.iter().find(|c| c.starts_with("[follow-up a]")).expect("the repair went to a's open session");
    assert!(follow.contains("The checks failed after your last turn") && follow.contains("RELEASE page:a"), "{follow}");
    assert!(!follow.contains("--- Your task ---"), "a follow-up does not resend the whole brief");
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("fixed"), "the follow-up's fix was merged");
    // The open worktree had b's files copied in; they were not taken as a's work.
    assert!(log(&tmp.0).contains("taskmaster: repaired page a"));
}

#[tokio::test]
async fn an_empty_balance_is_a_typed_limit_and_the_run_resumes() {
    let tmp = seeded("broke");
    let world = World::new(Mode::Broke);
    let err = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 1), &world).await.unwrap_err();
    let limit = err.downcast_ref::<crate::reply::ReportedLimit>().expect("a typed limit, not a raw error");
    assert!(limit.sentence().contains("no usage balance left"), "{}", limit.sentence());
    assert!(!limit.sentence().contains("-32603"), "the raw reply stays out of the sentence");
    // Nothing was thrown away: the same command picks the run up where it stopped.
    let again = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 1), &again).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(read(&tmp.0.join("app/a/page.ts")).contains("\"a\""));
    assert!(again.calls.lock().unwrap().iter().all(|c| !c.contains("Plan this")), "it was not planned again");
}

#[tokio::test]
async fn a_library_plan_builds_modules_with_no_route_and_no_browser() {
    let tmp = seeded("library");
    let world = World::new(Mode::Write);
    let mut planned = plan(&[("a", &[]), ("b", &[])]);
    planned.kind = tm_core::ProjectKind::Library;
    for page in &mut planned.pages {
        page.route.clear();
    }
    let outcome = run(config(&tmp.0, planned, true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let builds: Vec<&String> = calls.iter().filter(|c| c.contains("Build unit `")).collect();
    assert_eq!(builds.len(), 2, "{calls:#?}");
    assert!(builds.iter().all(|b| !b.contains("for route") && !b.contains("app/index.html") && !b.contains("Design spec")));
    let transcript = read(&run_directory(&tmp.0, "current").join("agents.jsonl"));
    assert!(!transcript.contains("browser check"), "nothing is opened in a browser");
    // A module is a unit everywhere: its agent, its release line, the ledger and the log.
    assert!(transcript.contains("\"id\":\"unit:a\"") && !transcript.contains("\"id\":\"page:a\""));
    assert!(builds.iter().all(|b| b.contains("`RELEASE unit:")), "{builds:#?}");
    let ledger = read(&run_directory(&tmp.0, "current").join("ledger.jsonl"));
    assert!(ledger.contains("\"agent\":\"unit:a\"") && ledger.contains("\"agent\":\"unit:b\""), "{ledger}");
    assert!(log(&tmp.0).contains("taskmaster: built unit a"));
}

#[tokio::test]
async fn a_sandboxed_builder_gets_a_shell_and_a_reviewer_never_does() {
    let tmp = seeded("shell");
    let world = World::new(Mode::Write);
    world.sandboxed.store(true, Ordering::SeqCst);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let shells = world.shells.lock().unwrap().clone();
    let builder = shells.iter().find(|(read_only, shell)| !read_only && shell.is_some()).map(|(_, s)| s.clone().unwrap()).expect("the builder had a shell");
    let scratch = builder.env.iter().find(|(k, _)| k == "TASKMASTER_SCRATCH").map(|(_, v)| PathBuf::from(v)).unwrap();
    assert!(scratch.starts_with(run_directory(&tmp.0, "current")), "{scratch:?}");
    assert!(builder.writable.contains(&scratch));
    assert!(builder.env.iter().any(|(k, v)| k == "PYTHONDONTWRITEBYTECODE" && v == "1"));
    assert!(shells.iter().filter(|(read_only, _)| *read_only).all(|(_, shell)| shell.is_none()), "readers get no shell");
    let calls = world.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.contains("Build page `a`")));

    // Off in the settings, or a harness the sandbox cannot hold: no shell for anyone.
    let tmp = seeded("no-shell");
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(world.shells.lock().unwrap().iter().all(|(_, shell)| shell.is_none()));
    let tmp = seeded("shell-off");
    let world = World::new(Mode::Write);
    world.sandboxed.store(true, Ordering::SeqCst);
    let mut cfg = config(&tmp.0, plan(&[("a", &["greet"])]), true, 1);
    cfg.worker_shell = false;
    run(cfg, &world).await.unwrap();
    assert!(world.shells.lock().unwrap().iter().all(|(_, shell)| shell.is_none()));
}

#[tokio::test]
async fn a_unit_whose_files_are_already_right_is_done_not_failed() {
    let tmp = seeded("already");
    std::fs::create_dir_all(tmp.0.join("app/a")).unwrap();
    std::fs::write(tmp.0.join("app/a/page.ts"), "export const page = \"a\";\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "a exists"]);
    let world = World::new(Mode::Nothing);
    let outcome = run(config(&tmp.0, plan(&[("a", &["greet"])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let builds = world.calls.lock().unwrap().iter().filter(|c| c.contains("Build page `a`")).count();
    assert_eq!(builds, 1, "no repair or escalation for a unit that was already done");
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
    let head = git_out(&tmp.0, &["rev-parse", "HEAD"]);
    let commit = status["merge_commit"].as_str().unwrap();
    assert!(commit.len() >= 7 && head.trim().starts_with(commit));
    assert_eq!(status["base"].as_str(), Some(git_out(&tmp.0, &["rev-parse", "--abbrev-ref", "HEAD"]).trim()));
}

#[tokio::test]
async fn a_merged_run_is_undone_in_one_commit_that_keeps_later_work() {
    let tmp = seeded("undo");
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(tmp.0.join("app/a/page.ts").is_file());
    std::fs::write(tmp.0.join("notes.md"), "mine\n").unwrap();
    git(&tmp.0, &["add", "notes.md"]);
    git(&tmp.0, &["commit", "-q", "-m", "my own work after the run"]);
    let records = Records::files(&tmp.0);
    assert!(records.citations().unwrap().pages.contains_key("a"));

    let undone = crate::undo::undo_run(&tmp.0, &records, "current").unwrap();
    let crate::undo::Undo::Done { commit, files } = undone else { panic!("refused: {undone:?}") };
    assert!(files >= 3, "the pages and the contract came back out: {files}");
    assert!(!tmp.0.join("app/a/page.ts").exists() && !tmp.0.join("contracts/types.ts").exists());
    assert_eq!(read(&tmp.0.join("notes.md")), "mine\n", "work after the merge is kept");
    assert_eq!(read(&tmp.0.join("README.md")), "# project\n");
    assert_eq!(git_out(&tmp.0, &["status", "--porcelain"]).trim(), "");
    assert!(log(&tmp.0).starts_with("taskmaster: undo run current\nmy own work after the run\n"));
    assert!(git_out(&tmp.0, &["rev-parse", "HEAD"]).starts_with(&commit));
    assert!(!records.citations().unwrap().pages.contains_key("a"), "the pages are built again next time");
    assert_eq!(records.status("current").unwrap().unwrap()["undone"]["commit"].as_str(), Some(commit.as_str()));
    let again = crate::undo::undo_run(&tmp.0, &records, "current").unwrap();
    assert!(matches!(&again, crate::undo::Undo::Refused(why) if why.contains("already undone")), "{again:?}");
}

#[tokio::test]
async fn a_run_whose_files_were_edited_after_the_merge_is_not_undone() {
    let tmp = seeded("undo-edited");
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    std::fs::write(tmp.0.join("app/a/page.ts"), "export const a = \"changed by hand\";\n").unwrap();
    let records = Records::files(&tmp.0);
    let dirty = crate::undo::undo_run(&tmp.0, &records, "current").unwrap();
    assert!(matches!(&dirty, crate::undo::Undo::Refused(why) if why.contains("uncommitted edits")), "{dirty:?}");
    git(&tmp.0, &["commit", "-q", "-am", "hand edit"]);
    let head = git_out(&tmp.0, &["rev-parse", "HEAD"]);
    let edited = crate::undo::undo_run(&tmp.0, &records, "current").unwrap();
    assert!(matches!(&edited, crate::undo::Undo::Refused(why) if why.contains("edited after it was merged") && why.contains("git revert")), "{edited:?}");
    assert_eq!(git_out(&tmp.0, &["rev-parse", "HEAD"]), head, "the project is left alone");
    assert_eq!(git_out(&tmp.0, &["status", "--porcelain"]).trim(), "");
    assert!(records.status("current").unwrap().unwrap().get("undone").is_none());
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
    // Every fake call is 120 tokens and one cent: two pages at 1.3 calls each, no review.
    let status = Records::files(&tmp.0).status("two").unwrap().unwrap();
    assert_eq!(status["estimate"]["tokens"], 312);
    // A cent per 120 tokens, over the fresh tokens and the contract read from the cache.
    assert!((0.026..0.03).contains(&status["estimate"]["usd"].as_f64().unwrap()), "{status}");
    assert_eq!(status["estimate"]["priced"], true);
    assert_eq!(status["spent"]["fresh"], 2 * 120);
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
async fn a_page_s_skills_are_in_its_worktree_and_brief_and_never_merged() {
    let tmp = seeded("skills");
    let store_dir = tmp.0.join("../").canonicalize().unwrap().join(format!("{}-store", tmp.0.file_name().unwrap().to_string_lossy()));
    let canned = Arc::new(crate::skills::tests::Canned::default());
    crate::skills::tests::canned_repo(&canned, "v1", "---\nname: tidy-css\ndescription: Keep stylesheets ordered.\n---\n");
    let store = crate::skills::Store::new(&store_dir, canned);
    store.install("acme/kit", "tidy-css").unwrap();
    let mut planned = plan(&[("a", &[]), ("b", &[])]);
    planned.pages[0].skills = vec!["tidy-css".into(), "not-installed".into()];
    let mut cfg = config(&tmp.0, planned, true, 2);
    cfg.skills = Some(store_dir.clone());
    let world = World::new(Mode::Write);
    let outcome = run(cfg, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let a = calls.iter().find(|c| c.contains("Build page `a`")).unwrap();
    assert!(a.contains("`.taskmaster/skills/tidy-css/SKILL.md`: Keep stylesheets ordered."), "{a}");
    assert!(a.contains("[skills=tidy-css]"), "{a}");
    let b = calls.iter().find(|c| c.contains("Build page `b`")).unwrap();
    assert!(!b.contains(".taskmaster/skills") && b.contains("[skills=]"), "{b}");
    // The copy stays out of the project.
    assert!(!tmp.0.join(".taskmaster/skills").exists());
    assert!(!git_out(&tmp.0, &["log", "--name-only", "--format="]).contains("SKILL.md"));
    std::fs::remove_dir_all(store_dir).ok();
}

#[tokio::test]
async fn the_planner_is_offered_enabled_skills_and_unknown_ones_are_dropped() {
    let tmp = seeded("skills-plan");
    let store_dir = std::env::temp_dir().join(format!("tm-skills-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&store_dir);
    let canned = Arc::new(crate::skills::tests::Canned::default());
    crate::skills::tests::canned_repo(&canned, "v1", "---\nname: tidy-css\ndescription: Keep stylesheets ordered.\n---\n");
    crate::skills::Store::new(&store_dir, canned).install("acme/kit", "tidy-css").unwrap();
    let mut cfg = Config::new(&tmp.0);
    cfg.prompt = Some("A notes app. Use $tidy-css.".into());
    cfg.skills = Some(store_dir.clone());
    cfg.timeout = Duration::from_secs(30);
    let world = World::new(Mode::Plan);
    let outcome = run(cfg, &world).await.unwrap();
    assert!(matches!(outcome, Outcome::NeedsApproval { .. }), "{outcome:?}");
    let calls = world.calls.lock().unwrap().clone();
    let brief = calls.iter().find(|c| c.contains("Skills are installed.")).expect("the planner saw the skills");
    assert!(brief.contains("- tidy-css: Keep stylesheets ordered."), "{brief}");
    assert!(brief.contains("The request asks for tidy-css"), "{brief}");
    // The planner attached nothing, so the skill the person named goes to every page.
    let saved: Plan = serde_json::from_str(&read(&tmp.0.join(".taskmaster/runs/current/plan.json"))).unwrap();
    assert!(saved.pages.iter().all(|p| p.skills == vec!["tidy-css".to_string()]), "{:?}", saved.pages);
    std::fs::remove_dir_all(store_dir).ok();
}

#[tokio::test]
async fn workers_on_a_web_project_get_a_browser_on_their_own_preview() {
    if crate::runtime::chrome().is_none() {
        eprintln!("no Chrome here; workers get no browser");
        return;
    }
    let tmp = seeded("browser-web");
    std::fs::write(tmp.0.join("package.json"), "{\"name\":\"shop\",\"private\":true}\n").unwrap();
    git(&tmp.0, &["add", "-A"]);
    git(&tmp.0, &["commit", "-q", "-m", "web"]);
    let world = World::new(Mode::Write);
    let outcome = run(config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2), &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let briefs: Vec<&String> = calls.iter().filter(|c| c.contains("Build page `")).collect();
    assert_eq!(briefs.len(), 2);
    let origins: Vec<&str> = briefs
        .iter()
        .map(|b| b.split("A browser is ready on your own page's preview, ").nth(1).and_then(|r| r.split(';').next()).expect("the browser is offered"))
        .collect();
    assert!(origins.iter().all(|o| o.starts_with("http://127.0.0.1:")), "{origins:?}");
    assert_ne!(origins[0], origins[1], "each page has a preview of its own");

    // A project with nothing for a browser to show gets none.
    let plain = seeded("browser-plain");
    let world = World::new(Mode::Write);
    run(config(&plain.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert!(!world.calls.lock().unwrap().iter().any(|c| c.contains("A browser is ready")));
}

#[tokio::test]
async fn the_checker_looks_once_at_ui_pages_and_its_fixes_become_one_repair() {
    let tmp = seeded("checker");
    let world = World::new(Mode::Checked);
    let mut cfg = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 2);
    cfg.checker = true;
    cfg.browser = false;
    let outcome = run(cfg, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    let calls = world.calls.lock().unwrap().clone();
    let checks = calls.iter().filter(|c| c.contains("Check one page another agent")).count();
    assert_eq!(checks, 2, "each page is checked once, the repair is not checked again");
    let a_builds: Vec<&String> = calls.iter().filter(|c| c.contains("Build page `a`")).collect();
    assert_eq!(a_builds.len(), 2, "one repair for the checker's fix");
    assert!(a_builds[1].contains("A second model checked this page") && a_builds[1].contains("the total is never shown"));
    assert_eq!(calls.iter().filter(|c| c.contains("Build page `b`")).count(), 1);
    let ledger = Records::files(&tmp.0).ledger("current").unwrap();
    assert_eq!(ledger.iter().filter(|e| e.role == tm_core::Role::Checker).count(), 2);

    // Off by default: no check at all.
    let quiet = seeded("checker-off");
    let world = World::new(Mode::Checked);
    run(config(&quiet.0, plan(&[("a", &[])]), true, 1), &world).await.unwrap();
    assert!(!world.calls.lock().unwrap().iter().any(|c| c.contains("Check one page")));
}

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

    /// The person's saved choice of a harness for every role; nothing is chosen for them.
    fn choose_roles(project: &std::path::Path) {
        let file = project.join(".taskmaster/config.json");
        let mut root: Value = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| json!({}));
        if root.get("roles").is_none() {
            root["roles"] = json!({ "worker": "grok", "mid": "grok", "master": "grok", "reviewer": "grok", "verifier": "grok" });
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, serde_json::to_string_pretty(&root).unwrap()).unwrap();
        }
    }

    async fn server(tmp: &Tmp, world: &Arc<World>) -> (Server, Client, String) {
        choose_roles(&tmp.0);
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let code = auth.code().to_string();
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        options.skills = tmp.0.join(".skills-store");
        let world = world.clone();
        options.harness = Arc::new(move |_| Ok(Arc::new(Fake(world.clone())) as Arc<dyn Harness>));
        let server = Server::serve(options).await.unwrap();
        let addr = server.url.trim_start_matches("http://").trim_end_matches('/').to_string();
        (server, Client { addr, token: None }, code)
    }

    #[tokio::test]
    async fn devices_pair_by_link_and_an_outside_agent_drives_a_run_over_mcp() {
        let tmp = repo("connect");
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let anon = Client { addr: client.addr.clone(), token: None };

        // The printed code still pairs, now with a name.
        let (_, paired) = client.post("/api/pair", json!({ "code": code, "label": "Laptop" })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());
        let (status, view) = client.get("/api/connect").await;
        assert_eq!(status, 200, "{view}");
        assert_eq!(view["routes"][0]["kind"], "this-computer");
        assert_eq!(view["only_this_computer"], true);
        assert!(view["mcp_url"].as_str().unwrap().ends_with("/mcp"), "{view}");

        // A one-time link: needs a signed-in device to make, pairs once, and names the device.
        assert_eq!(anon.post("/api/pairing", json!({})).await.0, 401);
        let (status, offer) = client.post("/api/pairing", json!({ "label": "Phone", "kind": "phone" })).await;
        assert_eq!(status, 200, "{offer}");
        let one_time = offer["code"].as_str().unwrap().to_string();
        assert!(offer["links"][0]["url"].as_str().unwrap().ends_with(&format!("/pair#code={one_time}")), "{offer}");
        let (status, phone) = anon.post("/api/pair", json!({ "code": one_time.to_lowercase(), "label": "Chrome on Android" })).await;
        assert_eq!(status, 200, "{phone}");
        assert_eq!(anon.post("/api/pair", json!({ "code": one_time })).await.0, 401, "a link is good once");
        let phone = Client { addr: client.addr.clone(), token: Some(phone["token"].as_str().unwrap().to_string()) };
        let (_, listed) = phone.get("/api/devices").await;
        let devices = listed["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 2, "{listed}");
        assert_eq!((devices[0]["label"].as_str(), devices[0]["kind"].as_str(), devices[0]["current"].as_bool()), (Some("Phone"), Some("phone"), Some(true)));

        // Revoked from the laptop, the phone is out at once.
        let phone_id = devices[0]["id"].as_str().unwrap().to_string();
        assert_eq!(client.post(&format!("/api/devices/{phone_id}/revoke"), json!({})).await.0, 200);
        assert_eq!(phone.get("/api/runs").await.0, 401);
        assert_eq!(client.post(&format!("/api/devices/{phone_id}/revoke"), json!({})).await.0, 404);
        let laptop_id = client.get("/api/devices").await.1["devices"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(client.post(&format!("/api/devices/{laptop_id}/rename"), json!({ "label": "Work laptop" })).await.0, 200);
        assert_eq!(client.get("/api/devices").await.1["devices"][0]["label"], "Work laptop");

        // An outside agent gets its own token and drives a run over MCP.
        let (_, minted) = client.post("/api/devices/agent", json!({ "label": "Claude Code" })).await;
        let agent = Client { addr: client.addr.clone(), token: Some(minted["token"].as_str().unwrap().to_string()) };
        let rpc = |id: u64, method: &str, params: Value| json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        assert_eq!(anon.post("/mcp", rpc(1, "initialize", json!({}))).await.0, 401);
        let (_, init) = agent.post("/mcp", rpc(1, "initialize", json!({ "protocolVersion": "2025-06-18" }))).await;
        assert_eq!(init["result"]["serverInfo"]["name"], "taskmaster", "{init}");
        assert_eq!(agent.post("/mcp", json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await.0, 202);
        let (_, tools) = agent.post("/mcp", rpc(2, "tools/list", json!({}))).await;
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        for want in ["taskmaster_start_run", "taskmaster_wait", "taskmaster_read_run", "taskmaster_send_note", "taskmaster_preview"] {
            assert!(names.contains(&want), "{names:?}");
        }
        let call = |id: u64, name: &str, args: Value| rpc(id, "tools/call", json!({ "name": name, "arguments": args }));
        let (_, started) = agent.post("/mcp", call(3, "taskmaster_start_run", json!({ "prompt": "A landing page that says hello", "mode": "agent", "approve": true }))).await;
        assert_eq!(started["result"]["isError"], false, "{started}");
        let run = started["result"]["structuredContent"]["id"].as_str().unwrap().to_string();
        assert!(started["result"]["structuredContent"]["link"].as_str().unwrap().ends_with(&format!("/runs/{run}")));
        let (_, waited) = agent.post("/mcp", call(4, "taskmaster_wait", json!({ "run": run, "timeout_secs": 60 }))).await;
        assert_eq!(waited["result"]["structuredContent"]["state"], "done", "{waited}");
        let (_, read) = agent.post("/mcp", call(5, "taskmaster_read_run", json!({ "run": run }))).await;
        let summary = read["result"]["structuredContent"]["summary"].as_str().unwrap();
        assert!(summary.contains("A landing page that says hello") && summary.contains("[page:agent]"), "{summary}");
        let (_, wrong) = agent.post("/mcp", call(6, "taskmaster_run_status", json!({ "run": "nope" }))).await;
        assert_eq!(wrong["result"]["isError"], true, "{wrong}");
        let (_, missing) = agent.post("/mcp", call(7, "taskmaster_start_run", json!({}))).await;
        assert!(missing["result"]["content"][0]["text"].as_str().unwrap().contains("`prompt` is required"));
        let devices = client.get("/api/devices").await.1;
        assert!(devices["devices"].as_array().unwrap().iter().any(|d| d["kind"] == "agent" && d["label"] == "Claude Code"), "{devices}");
        // The agent's token reaches the MCP server and nothing else.
        assert_eq!(agent.get("/api/runs").await.0, 403);
        assert_eq!(agent.post("/api/pairing", json!({})).await.0, 403);
        assert_eq!(agent.post(&format!("/api/devices/{laptop_id}/revoke"), json!({})).await.0, 403);

        // A pairing link opens a page even without the web app.
        let (status, page) = anon.text("/pair").await;
        assert_eq!(status, 200);
        assert!(page.contains("Taskmaster") && !page.contains("#code") && page.contains("location.hash"), "the page reads the code from the link");
    }

    #[tokio::test]
    async fn skills_are_searched_installed_toggled_updated_and_removed_over_the_api() {
        let tmp = seeded("skills-api");
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let code = auth.code().to_string();
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        let canned = Arc::new(crate::skills::tests::Canned::default());
        crate::skills::tests::canned_repo(&canned, "v1", "---\nname: tidy-css\ndescription: Keep stylesheets ordered.\n---\n");
        canned.set("https://skills.sh/api/search?q=tidy", r#"{"skills":[{"source":"acme/kit","skillId":"tidy-css","name":"tidy-css","installs":7}]}"#);
        options.skills = tmp.0.join(".skills-store");
        options.skills_fetch = canned.clone();
        choose_roles(&options.project);
        let server = Server::serve(options).await.unwrap();
        let mut client = Client { addr: server.url.trim_start_matches("http://").trim_end_matches('/').to_string(), token: None };
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        assert_eq!(client.get("/api/skills/search?q=t").await.0, 400);
        let (status, found) = client.get("/api/skills/search?q=tidy").await;
        assert_eq!(status, 200, "{found}");
        assert_eq!(found["hits"][0]["installed"], false);
        assert_eq!(client.post("/api/skills/install", json!({ "repo": "acme/kit", "skill": "../x" })).await.0, 400);
        let (status, skill) = client.post("/api/skills/install", json!({ "repo": "acme/kit", "skill": "tidy-css" })).await;
        assert_eq!(status, 200, "{skill}");
        assert_eq!((skill["name"].as_str(), skill["enabled"].as_bool()), (Some("tidy-css"), Some(true)));
        assert_eq!(client.get("/api/skills/search?q=tidy").await.1["hits"][0]["installed"], true);
        let (_, listed) = client.get("/api/skills").await;
        let stored: Vec<&Value> = listed["skills"].as_array().unwrap().iter().filter(|s| s["place"] == "store").collect();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0]["description"], "Keep stylesheets ordered.");

        assert_eq!(client.post("/api/skills/tidy-css/enable", json!({ "enabled": false })).await.1["enabled"], false);
        assert_eq!(client.get("/api/skills/updates").await.1["updates"], json!({}));
        crate::skills::tests::canned_repo(&canned, "v2", "---\nname: tidy-css\ndescription: Keep stylesheets ordered, v2.\n---\n");
        assert_eq!(client.get("/api/skills/updates").await.1["updates"]["tidy-css"], "v2");
        let (status, updated) = client.post("/api/skills/tidy-css/update", json!({})).await;
        assert_eq!(status, 200, "{updated}");
        assert_eq!(updated["description"], "Keep stylesheets ordered, v2.");
        assert_eq!(updated["enabled"], false, "an update keeps the switch where it was");
        assert_eq!(client.post("/api/skills/tidy-css/remove", json!({})).await.0, 200);
        assert_eq!(client.post("/api/skills/tidy-css/remove", json!({})).await.0, 404);
        let (_, left) = client.get("/api/skills").await;
        assert!(!left["skills"].as_array().unwrap().iter().any(|s| s["place"] == "store"), "{left}");
    }

    #[tokio::test]
    async fn providers_install_and_sign_in_through_jobs_that_run_only_listed_steps() {
        use tm_harness::setup::Step;
        let tmp = seeded("setup");
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let code = auth.code().to_string();
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        let sh = |label: &str, script: &str, terminal: bool| Step {
            label: label.into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            terminal,
        };
        let login = sh("Signing in", "echo 'Visit https://login.example.com/activate'; echo 'Enter code QRST-4321'; read a; echo welcome $a", true);
        let install = sh("Installing", "echo fetched; echo installed", false);
        options.setup_planner = Some(Arc::new(move |target, action| match (target, action) {
            (crate::jobs::Target::Provider("codex"), tm_harness::setup::Action::Login) => Some(vec![login.clone()]),
            (crate::jobs::Target::Provider("codex"), tm_harness::setup::Action::Install) => Some(vec![install.clone()]),
            _ => None,
        }));
        choose_roles(&options.project);
        let server = Server::serve(options).await.unwrap();
        let addr = server.url.trim_start_matches("http://").trim_end_matches('/').to_string();
        let mut client = Client { addr, token: None };
        assert_eq!(client.get("/api/setup").await.0, 401);
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        // Only known providers, runtimes and actions; a request never carries a command.
        assert_eq!(client.post("/api/setup/providers/nope/install", json!({})).await.0, 404);
        assert_eq!(client.post("/api/setup/providers/codex/exec", json!({ "command": "rm -rf ~" })).await.0, 404);
        assert_eq!(client.post("/api/setup/runtimes/perl/install", json!({})).await.0, 404);
        assert_eq!(client.post("/api/setup/providers/claude/update", json!({})).await.0, 409);

        let (status, job) = client.post("/api/setup/providers/codex/login", json!({})).await;
        assert_eq!(status, 202, "{job}");
        let id = job["id"].as_str().unwrap().to_string();
        assert_eq!(client.post("/api/setup/providers/codex/login", json!({})).await.0, 409);
        let mut shown = Value::Null;
        for _ in 0..200 {
            shown = client.get(&format!("/api/setup/jobs/{id}")).await.1;
            if shown["prompt"]["code"].is_string() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(shown["prompt"]["code"], "QRST-4321", "{shown}");
        assert_eq!(shown["prompt"]["url"], "https://login.example.com/activate");
        assert_eq!(shown["accepts_input"], true);
        assert_eq!(client.post(&format!("/api/setup/jobs/{id}/input"), json!({ "text": "a\u{3}b" })).await.0, 400);
        assert_eq!(client.post(&format!("/api/setup/jobs/{id}/input"), json!({ "key": "f12" })).await.0, 400);
        assert_eq!(client.post(&format!("/api/setup/jobs/{id}/input"), json!({ "text": "sam", "key": "enter" })).await.0, 200);
        let (_, streamed) = client.text(&format!("/api/setup/jobs/{id}/stream?token={}", client.token.clone().unwrap())).await;
        assert!(streamed.contains("event: job"), "{streamed}");
        assert!(streamed.contains("welcome sam"), "{streamed}");
        let done = client.get(&format!("/api/setup/jobs/{id}")).await.1;
        assert_eq!(done["state"], "done", "{done}");

        let (status, job) = client.post("/api/setup/providers/codex/install", json!({})).await;
        assert_eq!(status, 202, "{job}");
        let id = job["id"].as_str().unwrap();
        assert_eq!(client.post(&format!("/api/setup/jobs/{id}/input"), json!({ "text": "x" })).await.0, 409);
        let (_, overview) = client.get("/api/setup").await;
        assert_eq!(overview["providers"].as_array().unwrap().len(), 5);
        assert_eq!(overview["runtimes"].as_array().unwrap().len(), 3);
        assert!(overview["jobs"].as_array().unwrap().iter().any(|j| j["target"] == "codex"));
    }

    #[tokio::test]
    async fn a_device_pairs_starts_a_run_approves_it_and_reads_what_it_cost() {
        let tmp = seeded("api");
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
        // Forget every choice: an empty name removes it.
        let forget = json!({ "worker": "", "mid": "", "master": "", "reviewer": "", "verifier": "" });
        assert_eq!(client.post("/api/settings", json!({ "roles": forget })).await.0, 200);
        // Nobody has chosen a provider now, so a run that names none is refused, and says which roles.
        let (_, defaults) = client.get("/api/settings").await;
        assert_eq!(defaults["roles"], json!({}));
        assert!(defaults["machine"]["max_parallel"].as_u64().unwrap() >= 1);
        let (status, refused) = client.post("/api/runs", json!({ "id": "web", "plan": planned })).await;
        assert_eq!(status, 400, "{refused}");
        assert!(refused["error"].as_str().unwrap().contains("choose a provider"), "{refused}");

        let every = json!({ "worker": "grok", "mid": "grok", "master": "grok", "reviewer": "grok", "verifier": "grok" });
        let (status, made) = client.post("/api/runs", json!({ "id": "web", "plan": planned, "parallel": 2, "roles": every })).await;
        assert_eq!(status, 202, "{made}");
        let waiting = client.settled("web", "awaiting_approval").await;
        assert_eq!(waiting["plan"]["pages"].as_array().unwrap().len(), 2);
        assert_eq!(waiting["roles"]["worker"], "grok");

        // Saved choices are kept in the settings file and apply to runs that name no roles.
        assert_eq!(client.post("/api/settings", json!({ "roles": { "worker": "nope" } })).await.0, 400);
        assert_eq!(client.post("/api/settings", json!({ "roles": every, "models": {} })).await.0, 200);
        let (status, saved) = client.post("/api/settings", json!({ "roles": { "worker": "claude" }, "models": { "worker": null } })).await;
        assert_eq!(status, 200, "{saved}");
        assert_eq!((saved["roles"]["worker"].as_str(), saved["roles"]["master"].as_str(), saved["models"]["worker"].as_str()), (Some("claude"), Some("grok"), None));
        let config: Value = serde_json::from_str(&std::fs::read_to_string(tmp.0.join(".taskmaster/config.json")).unwrap()).unwrap();
        assert_eq!(config["roles"]["worker"], "claude");
        assert_eq!(client.get("/api/settings").await.1["roles"]["worker"], "claude");
        assert_eq!(crate::settings::Settings::load(&tmp.0).unwrap().roles.worker, "claude");
        let (status, made) = client.post("/api/runs", json!({ "id": "web2", "plan": planned })).await;
        assert_eq!(status, 202, "{made}");
        assert_eq!(client.settled("web2", "awaiting_approval").await["roles"]["worker"], "claude");
        let (_, put_back) = client.post("/api/settings", json!({ "roles": { "worker": "grok", "mid": "grok", "master": "grok", "reviewer": "grok", "verifier": "grok" }, "models": {} })).await;
        assert_eq!(put_back["roles"]["worker"], "grok");
        assert!(!tmp.0.join("app/a/page.ts").exists());
        assert_eq!(client.post("/api/runs", json!({ "id": "web", "plan": planned })).await.0, 409);
        assert_eq!(client.post("/api/runs", json!({ "id": "../x", "plan": planned })).await.0, 400);
        assert_eq!(client.post("/api/runs", json!({ "id": "empty" })).await.0, 400);

        assert_eq!(client.post("/api/runs/web/approve", json!({})).await.0, 202);
        let done = client.settled("web", "done").await;
        assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
        assert!(done["pages"].as_array().unwrap().iter().all(|page| page["state"] == "passed"));
        assert_eq!(done["cancelled"], false);
        assert_eq!(done["estimate"]["tokens"], 2 * 40_000);
        let calls = done["spent"]["calls"].as_u64().unwrap();
        assert!(calls >= 2 && done["spent"]["tokens"] == calls * 120, "{done}");
        assert_eq!(client.post("/api/runs/web/approve", json!({})).await.0, 409);
        assert_eq!(client.post("/api/runs/web/cancel", json!({})).await.0, 409);

        // The run never reviewed itself; review is asked for afterwards, once, for the whole run.
        let (_, reviews) = client.get("/api/reviews").await;
        assert!(reviews["reviews"].as_array().unwrap().is_empty());
        assert_eq!(client.post("/api/runs/nope/review", json!({})).await.0, 404);
        let (status, made) = client.post("/api/runs/web/review", json!({})).await;
        assert_eq!(status, 202, "{made}");
        let review_id = made["id"].as_str().unwrap().to_string();
        let mut job = Value::Null;
        for _ in 0..400 {
            job = client.get(&format!("/api/reviews/{review_id}")).await.1;
            if job["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(job["status"], "done", "{job}");
        assert_eq!(job["run"], "web");

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
    async fn a_follow_up_note_produces_a_master_step_before_any_worker_step() {
        let tmp = repo("api-agents");
        // The first attempt writes outside its files and fails; the repair passes.
        let world = World::new(Mode::Outside);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        let planned = serde_json::to_value(plan(&[("a", &[])])).unwrap();
        assert_eq!(client.post("/api/runs", json!({ "id": "talk", "plan": planned })).await.0, 202);
        client.settled("talk", "awaiting_approval").await;
        // A gutter comment is fix text for that page's next repair; it adds no phase.
        let (status, note) = client.post("/api/runs/talk/notes", json!({ "to": "page:a", "text": "keep the header" })).await;
        assert_eq!(status, 200, "{note}");
        assert_eq!(client.post("/api/runs/talk/notes", json!({ "to": "page:../x", "text": "x" })).await.0, 400);
        let gone = client.post("/api/runs/talk/notes", json!({ "to": "master", "text": "drop me" })).await.1;
        assert_eq!(client.post(&format!("/api/runs/talk/notes/{}", gone["id"].as_str().unwrap()), json!({})).await.0, 200);
        assert_eq!(client.get("/api/runs/talk/notes").await.1["notes"].as_array().unwrap().len(), 1);

        assert_eq!(client.post("/api/runs/talk/approve", json!({ "note": "use tabs" })).await.0, 202);
        client.settled("talk", "done").await;
        let briefs = world.calls.lock().unwrap().clone();
        let builds: Vec<&String> = briefs.iter().filter(|brief| brief.contains("Build page `a`")).collect();
        assert!(builds.len() >= 2, "{builds:?}");
        assert!(!builds[0].contains("keep the header") && builds[0].contains("use tabs"), "{}", builds[0]);
        assert!(builds[1].contains("keep the header") && builds[1].contains("use tabs"), "{}", builds[1]);
        let master_note = briefs.iter().position(|brief| brief.contains("Read these notes and decide")).expect("the master reads the note");
        let first_worker = briefs.iter().position(|brief| brief.contains("Build page `a`")).expect("a worker builds");
        assert!(master_note < first_worker, "a follow-up note produces a master step before any worker step");
        assert_eq!(builds.iter().filter(|brief| brief.contains("keep the header")).count(), 1);
        let (_, notes) = client.get("/api/runs/talk/notes").await;
        assert!(notes["notes"].as_array().unwrap().iter().all(|note| note["delivered"] == true), "{notes}");

        let (_, said) = client.get("/api/runs/talk/agents").await;
        let entries = said["entries"].as_array().unwrap();
        assert_eq!(said["next"].as_u64().unwrap() as usize, entries.len());
        let of = |kind: &str| entries.iter().filter(|e| e["kind"] == kind && e["agent"]["id"] == "page:a").count();
        assert!(of("start") >= 2 && of("end") >= 2 && of("text") >= 2 && of("thought") >= 2, "{said}");
        let call = entries.iter().find(|e| e["kind"] == "tool_call").unwrap();
        assert_eq!((call["name"].as_str(), call["target"].as_str()), (Some("Write"), Some("app/a/page.ts")));
        let output = entries.iter().find(|e| e["kind"] == "output" && e["id"] == "t1").unwrap();
        assert_eq!(output["text"].as_str(), Some("recorded-tool-result"));
        assert!(output["diff"].as_str().unwrap_or("").contains("+++ b/app/a/page.ts"));
        assert!(!briefs.iter().any(|brief| brief.contains("recorded-tool-result")), "a tool result stays out of the answer");
        assert!(entries.iter().any(|e| e["kind"] == "permission" && e["allowed"] == true));
        let end = entries.iter().find(|e| e["kind"] == "end").unwrap();
        assert_eq!(end["usage"]["input"], 100);
        let master_step = entries.iter().position(|e| e["kind"] == "start" && e["agent"]["id"] == "master").expect("master step");
        let worker_step = entries.iter().position(|e| e["kind"] == "start" && e["agent"]["id"] == "page:a").expect("worker step");
        assert!(master_step < worker_step, "the master step is recorded before any worker step");
        let start = entries.iter().find(|e| e["kind"] == "start" && e["agent"]["id"] == "page:a").unwrap();
        assert_eq!(start["agent"]["owned_glob"], "app/a/**");
        assert_eq!(start["agent"]["role"], "worker");
        assert!(start["agent"]["worktree"].as_str().unwrap().contains("page-a"), "{start}");
        // The answer kept only the text: thoughts never reached a brief or the plan.
        let (_, later) = client.get(&format!("/api/runs/talk/agents?after={}", entries.len() - 1)).await;
        assert_eq!(later["entries"].as_array().unwrap().len(), 1);

        let token = client.token.clone().unwrap();
        let (_, feed) = client.text(&format!("/api/runs/talk/stream?token={token}")).await;
        assert_eq!(feed.matches("event: agent").count(), entries.len(), "{feed}");
        let (_, rest) = client.text(&format!("/api/runs/talk/stream?token={token}&agents_after={}", entries.len())).await;
        assert!(!rest.contains("event: agent"), "{rest}");

        // A note after the run has finished, in Code mode, starts a new master turn, then a worker.
        let before = world.calls.lock().unwrap().len();
        let (status, noted) = client.post("/api/runs/talk/notes", json!({ "to": "master", "text": "rename the title", "mode": "code" })).await;
        assert_eq!(status, 200, "{noted}");
        client.settled("talk", "done").await;
        let briefs = world.calls.lock().unwrap().clone();
        let later = &briefs[before..];
        let master = later.iter().position(|brief| brief.contains("Read these notes") && brief.contains("rename the title")).expect("the master reads the finished-run note");
        let worker = later.iter().position(|brief| brief.contains("Build page `a`")).expect("a worker follows the master");
        assert!(master < worker, "the master turn comes before the worker: {later:?}");
        assert!(later[worker].contains("rename the title"), "the worker follows the master's decision: {}", later[worker]);
        let (_, notes) = client.get("/api/runs/talk/notes").await;
        assert!(notes["notes"].as_array().unwrap().iter().all(|note| note["delivered"] == true), "{notes}");

        // In Agent mode the next note is one agent's task: no master turn, no page rebuilt.
        assert_eq!(client.post("/api/runs/talk/notes", json!({ "to": "master", "text": "x", "mode": "nope" })).await.0, 400);
        let before = world.calls.lock().unwrap().len();
        let (status, noted) = client.post("/api/runs/talk/notes", json!({ "to": "master", "text": "the preview is blank: add the dev script", "mode": "agent" })).await;
        assert_eq!(status, 200, "{noted}");
        client.settled("talk", "done").await;
        let briefs = world.calls.lock().unwrap().clone();
        let later = &briefs[before..];
        assert!(!later.iter().any(|brief| brief.contains("Read these notes")), "no master turn: {later:?}");
        assert!(!later.iter().any(|brief| brief.contains("Build page `a`")), "page a is not rebuilt: {later:?}");
        let agent = later.iter().find(|brief| brief.contains("agent-3")).expect("one agent takes the note");
        assert!(agent.contains("add the dev script") && agent.contains("only agent"), "{agent}");
        let (_, run) = client.get("/api/runs/talk").await;
        assert_eq!(run["plan"]["pages"][0]["owned_glob"], "**", "{run}");
        let (_, said) = client.get("/api/runs/talk/agents").await;
        let turns: Vec<&Value> = said["entries"].as_array().unwrap().iter().filter(|e| e["kind"] == "turn").collect();
        assert_eq!(turns.len(), 2, "each follow-up opens a turn in the transcript");
        assert_eq!((turns[0]["mode"].as_str(), turns[0]["turn"].as_u64()), (Some("code"), Some(2)));
        assert_eq!((turns[1]["mode"].as_str(), turns[1]["turn"].as_u64()), (Some("agent"), Some(3)));
        assert_eq!(turns[1]["text"], "the preview is blank: add the dev script");
        assert_eq!(turns[1]["agent"]["id"], "person");
    }

    #[tokio::test]
    async fn paths_resolve_from_the_project_and_open_in_an_editor() {
        let tmp = repo("api-paths");
        std::fs::create_dir_all(tmp.0.join("src")).unwrap();
        std::fs::write(tmp.0.join("src/main.rs"), "fn main() {}\n").unwrap();
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        let (_, found) = client.post("/api/paths", json!({ "paths": ["src/main.rs", "src/nope.rs", "/etc/passwd"] })).await;
        let full = tmp.0.canonicalize().unwrap().join("src/main.rs").display().to_string();
        assert_eq!(found["paths"]["src/main.rs"], full.as_str());
        assert!(found["paths"]["src/nope.rs"].is_null() && found["paths"]["/etc/passwd"].is_null(), "{found}");
        let (status, preview) = client.get("/api/file?path=src/main.rs").await;
        assert_eq!(status, 200);
        assert_eq!(preview["content"], "fn main() {}\n");
        assert_eq!(client.get("/api/file?path=src/nope.rs").await.0, 404);
        assert_eq!(client.post("/api/open", json!({ "path": "src/nope.rs", "line": 3 })).await.0, 404);
        assert_eq!(client.post("/api/open", json!({ "path": "src/main.rs", "editor": "nope" })).await.0, 400);
        let (_, editors) = client.get("/api/editors").await;
        assert!(editors["editors"].as_array().unwrap().len() >= 21);
        // An editor that is not installed is named, with the full path, for the toast.
        let missing = editors["editors"].as_array().unwrap().iter().find(|e| e["installed"] == false && e["id"] != "files");
        if let Some(missing) = missing {
            let (status, reply) =
                client.post("/api/open", json!({ "path": "src/main.rs", "line": 1, "editor": missing["id"] })).await;
            assert_eq!(status, 424, "{reply}");
            assert_eq!(reply["command"], missing["command"]);
            assert_eq!(reply["path"], full.as_str());
        }
        let (_, files) = client.get("/api/files").await;
        assert!(files["files"].as_array().unwrap().iter().any(|f| f == "src/main.rs"), "{files}");
        let (_, skills) = client.get("/api/skills").await;
        assert!(!skills["skills"].as_array().unwrap().iter().any(|s| s["place"] == "store"), "{skills}");
        let (_, settings) = client.get("/api/settings").await;
        assert_eq!(settings["project"], tmp.0.canonicalize().unwrap().display().to_string().as_str());
    }

    #[tokio::test]
    async fn a_new_project_is_created_as_a_repository_under_the_workspace() {
        let tmp = repo("api-new-project");
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());
        let (_, settings) = client.get("/api/settings").await;
        let workspace = PathBuf::from(settings["workspace"].as_str().unwrap());
        assert_eq!(workspace, tmp.0.parent().unwrap().canonicalize().unwrap());

        assert_eq!(client.post("/api/projects", json!({ "name": "../escape" })).await.0, 400);
        assert_eq!(client.post("/api/projects", json!({ "name": "bad/name" })).await.0, 400);

        let name = format!("tm-new-{}", std::process::id());
        let dest = workspace.join(&name);
        let (status, body) = client.post("/api/projects", json!({ "name": name, "switch": false })).await;
        assert_eq!(status, 200, "{body}");
        let path = PathBuf::from(body["project"].as_str().unwrap());
        assert_eq!(path, dest.canonicalize().unwrap());
        assert!(path.join(".git").exists());
        assert!(crate::git::has_head(&path));
        let (_, still) = client.get("/api/settings").await;
        assert_eq!(still["project"], tmp.0.canonicalize().unwrap().display().to_string().as_str());
        let _ = std::fs::remove_dir_all(&path);
    }

    #[tokio::test]
    async fn the_terminal_runs_what_a_person_types_and_keeps_it_in_the_transcript() {
        let tmp = repo("api-terminal");
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());
        let planned = serde_json::to_value(plan(&[("a", &[])])).unwrap();
        client.post("/api/runs", json!({ "id": "t", "plan": planned })).await;
        client.settled("t", "awaiting_approval").await;
        let (status, ran) = client.post("/api/runs/t/terminal", json!({ "command": "echo hello" })).await;
        assert_eq!(status, 200, "{ran}");
        assert_eq!((ran["ok"].as_bool(), ran["output"].as_str()), (Some(true), Some("hello")));
        let (_, said) = client.get("/api/runs/t/agents").await;
        let typed: Vec<&Value> = said["entries"].as_array().unwrap().iter().filter(|e| e["agent"]["id"] == "person").collect();
        assert_eq!(typed.len(), 2);
        assert_eq!(typed[0]["command"], "echo hello");
        assert_eq!(client.post("/api/runs/t/pr", json!({})).await.0, 409);
        let (status, diff) = client.get("/api/runs/t/diff?scope=attempt").await;
        assert!(matches!(status, 400 | 404 | 409), "{diff}");
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
        assert!(job["report"]["usage"]["input"].as_u64().unwrap() >= 200);
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
        assert!(run["error"].as_str().unwrap().contains("no units"), "{run}");
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
        choose_roles(&options.project);
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
    /// A worker that asks the gate before a shell command, as an agent would, and says what it was told.
    struct Asking {
        inner: Fake,
        asked: Arc<Mutex<Vec<(String, tm_harness::Decision)>>>,
    }

    #[async_trait]
    impl Harness for Asking {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn run(&self, task: WorkerTask, gate: Arc<dyn PermissionGate>, events: mpsc::Sender<HarnessEvent>) -> anyhow::Result<RunSummary> {
            if task.brief.contains("Build page `") {
                let id = task.brief.split("Build page `").nth(1).and_then(|r| r.split('`').next()).unwrap_or("").to_string();
                let shell = tm_harness::PermissionRequest {
                    tool: "Bash".into(),
                    kind: tm_harness::ToolKind::Execute,
                    paths: vec![],
                    command: Some("npm test".into()),
                    url: None,
                };
                let outside = tm_harness::PermissionRequest {
                    tool: "Write".into(),
                    kind: tm_harness::ToolKind::Edit,
                    paths: vec!["/etc/hosts".into()],
                    command: None,
                    url: None,
                };
                let inside = tm_harness::PermissionRequest {
                    tool: "Write".into(),
                    kind: tm_harness::ToolKind::Edit,
                    paths: vec![task.cwd.join(format!("app/{id}/x.ts"))],
                    command: None,
                    url: None,
                };
                for (label, request) in [("shell", &shell), ("outside", &outside), ("inside", &inside)] {
                    let decision = gate.decide_async(request).await;
                    self.asked.lock().unwrap().push((format!("{id}:{label}"), decision));
                    let _ = events.send(HarnessEvent::PermissionDecided { tool: request.tool.clone(), allowed: decision == tm_harness::Decision::Allow }).await;
                }
            }
            self.inner.run(task, gate, events).await
        }
    }

    async fn pending(client: &Client, run: &str, n: usize) -> Vec<Value> {
        for _ in 0..400 {
            let (status, found) = client.get(&format!("/api/approvals?run={run}")).await;
            assert_eq!(status, 200, "{found}");
            if found["approvals"].as_array().unwrap().len() >= n {
                return found["approvals"].as_array().unwrap().clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no approval appeared for {run}");
    }

    #[tokio::test]
    async fn risky_tool_calls_wait_for_a_person_and_each_answer_is_kept() {
        use tm_harness::Decision::{Allow, Deny};
        let tmp = seeded("approvals");
        let world = World::new(Mode::Write);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let code = auth.code().to_string();
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        let (w, a) = (world.clone(), asked.clone());
        options.harness = Arc::new(move |_| Ok(Arc::new(Asking { inner: Fake(w.clone()), asked: a.clone() }) as Arc<dyn Harness>));
        options.approvals = crate::approvals::Policy { mode: crate::approvals::Mode::Risky, timeout: Duration::from_secs(30) };
        choose_roles(&options.project);
        let server = Server::serve(options).await.unwrap();
        let mut client = Client { addr: server.url.trim_start_matches("http://").trim_end_matches('/').to_string(), token: None };
        assert_eq!(client.get("/api/approvals").await.0, 401);
        client.token = Some(client.post("/api/pair", json!({ "code": code })).await.1["token"].as_str().unwrap().to_string());
        let (_, settings) = client.get("/api/settings").await;
        assert_eq!(settings["approvals"]["mode"], "risky");

        // A shell command and a write outside the worktree wait; a write inside does not.
        let planned = serde_json::to_value(plan(&[("c", &[])])).unwrap();
        assert_eq!(client.post("/api/runs", json!({ "id": "one", "plan": planned, "yes": true })).await.0, 202);
        let first = pending(&client, "one", 1).await;
        assert_eq!((first[0]["tool"].as_str(), first[0]["kind"].as_str()), (Some("Bash"), Some("execute")));
        assert_eq!(first[0]["command"], "npm test");
        assert!(first[0]["reason"].as_str().unwrap().contains("shell"), "{first:?}");
        assert_eq!(first[0]["agent"]["role"], "worker");
        let (_, view) = client.get("/api/runs/one").await;
        assert_eq!(view["pending_approvals"], 1);
        assert_eq!(client.get("/api/approvals?run=other").await.1["approvals"], json!([]));
        assert_eq!(client.get("/api/approvals?run=..%2Fx").await.0, 400);
        let id = first[0]["id"].as_str().unwrap();
        assert_eq!(client.post(&format!("/api/approvals/{id}/maybe"), json!({})).await.0, 404);
        assert_eq!(client.post("/api/approvals/nope/approve", json!({})).await.0, 409);
        let (status, answered) = client.post(&format!("/api/approvals/{id}/approve"), json!({})).await;
        assert_eq!((status, answered["allowed"].as_bool()), (200, Some(true)), "{answered}");
        assert_eq!(client.post(&format!("/api/approvals/{id}/approve"), json!({})).await.0, 409, "an answer is final");
        let second = pending(&client, "one", 1).await;
        assert_eq!(second[0]["kind"], "edit");
        assert_eq!(second[0]["paths"][0], "/etc/hosts");
        assert!(second[0]["reason"].as_str().unwrap().contains("outside the worktree"));
        let id = second[0]["id"].as_str().unwrap();
        assert_eq!(client.post(&format!("/api/approvals/{id}/deny"), json!({})).await.0, 200);
        client.settled("one", "done").await;
        let decisions = asked.lock().unwrap().clone();
        assert_eq!(
            decisions,
            vec![("c:shell".into(), Allow), ("c:outside".into(), Deny), ("c:inside".into(), Allow)],
            "the write inside the worktree was never asked about"
        );

        // The stream's transcript carries the question and the answer.
        let (_, transcript) = client.get("/api/runs/one/agents").await;
        let kinds: Vec<&str> = transcript["entries"].as_array().unwrap().iter().filter_map(|e| e["kind"].as_str()).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "approval_requested").count(), 2, "{kinds:?}");
        let resolved: Vec<&Value> = transcript["entries"].as_array().unwrap().iter().filter(|e| e["kind"] == "approval_resolved").collect();
        assert_eq!((resolved[0]["allowed"].as_bool(), resolved[0]["by"].as_str()), (Some(true), Some("person")));
        assert_eq!(resolved[1]["allowed"], false);

        // The policy changes at once and survives in the settings file; nothing else in it is lost.
        std::fs::create_dir_all(tmp.0.join(".taskmaster")).unwrap();
        std::fs::write(tmp.0.join(".taskmaster/config.json"), r#"{"commons":{"report":false},"approvals":{"mode":"risky","timeout_secs":30},"roles":{"worker":"grok","mid":"grok","master":"grok","reviewer":"grok","verifier":"grok"}}"#).unwrap();
        assert_eq!(client.post("/api/settings", json!({ "approvals": "sometimes" })).await.0, 400);
        assert_eq!(client.post("/api/settings", json!({ "approval_timeout_secs": 0 })).await.0, 400);
        let (status, changed) = client.post("/api/settings", json!({ "approvals": "never" })).await;
        assert_eq!((status, changed["approvals"]["mode"].as_str()), (200, Some("never")), "{changed}");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(tmp.0.join(".taskmaster/config.json")).unwrap()).unwrap();
        assert_eq!((saved["approvals"]["mode"].as_str(), saved["approvals"]["timeout_secs"].as_u64(), saved["commons"]["report"].as_bool()), (Some("never"), Some(30), Some(false)));
        asked.lock().unwrap().clear();
        let planned = serde_json::to_value(plan(&[("d", &[])])).unwrap();
        client.post("/api/runs", json!({ "id": "two", "plan": planned, "yes": true })).await;
        client.settled("two", "done").await;
        assert_eq!(
            asked.lock().unwrap().clone(),
            vec![("d:shell".into(), Deny), ("d:outside".into(), Deny), ("d:inside".into(), Allow)],
            "with `never`, nothing waits and the refusals stand"
        );

        // `always` asks about the write inside the worktree too, and a cancelled run refuses what waits.
        assert_eq!(client.post("/api/settings", json!({ "approvals": "always" })).await.0, 200);
        let planned = serde_json::to_value(plan(&[("e", &[])])).unwrap();
        client.post("/api/runs", json!({ "id": "three", "plan": planned, "yes": true })).await;
        pending(&client, "three", 1).await;
        asked.lock().unwrap().clear();
        assert_eq!(client.post("/api/runs/three/cancel", json!({})).await.0, 202);
        for _ in 0..400 {
            if client.get("/api/approvals?run=three").await.1["approvals"].as_array().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(client.get("/api/approvals").await.1["approvals"].as_array().unwrap().iter().all(|a| a["run"] != "three"));
    }

    #[tokio::test]
    async fn a_question_nobody_answers_is_refused_after_the_timeout() {
        use tm_harness::{Decision, PermissionRequest, ToolKind, WorktreeGate};
        let tmp = repo("approval-timeout");
        let live = Arc::new(crate::live::Live::new(Arc::new(crate::store::FileStore::in_project(&tmp.0))));
        live.approvals.set_policy(crate::approvals::Policy { mode: crate::approvals::Mode::Risky, timeout: Duration::from_millis(80) });
        let gate = crate::approvals::ApprovalGate::new(WorktreeGate::new(&tmp.0, vec![]), live.clone(), "r1".into(), json!({ "id": "w" }));
        let shell = PermissionRequest { tool: "Bash".into(), kind: ToolKind::Execute, paths: vec![], command: Some("ls".into()), url: None };
        assert_eq!(gate.decide_async(&shell).await, Decision::Deny);
        assert!(live.approvals.list(None).is_empty());
        let (_, entries) = live.read("r1", 0);
        assert_eq!(entries.last().unwrap()["by"], "timeout");
        // A reviewer's gate never asks: it may not run commands at all.
        let reviewer = crate::approvals::ApprovalGate::new(WorktreeGate::new(&tmp.0, vec![]).read_only(), live.clone(), "r1".into(), json!({}));
        assert_eq!(reviewer.decide_async(&shell).await, Decision::Deny);
        assert_eq!(live.read("r1", 0).0, 2, "no question was written for the reviewer");
        // A network fetch is risky too; a read outside the worktree is simply refused.
        let fetch = PermissionRequest { tool: "WebFetch".into(), kind: ToolKind::Fetch, paths: vec![], command: None, url: None };
        assert_eq!(gate.decide_async(&fetch).await, Decision::Deny);
        assert_eq!(live.read("r1", 0).1[2]["reason"], "fetches from the network");
        let read = PermissionRequest { tool: "Read".into(), kind: ToolKind::Read, paths: vec!["/etc/passwd".into()], command: None, url: None };
        let before = live.read("r1", 0).0;
        assert_eq!(gate.decide_async(&read).await, Decision::Deny);
        assert_eq!(live.read("r1", 0).0, before);
    }

    fn script(dir: &Path, name: &str, log: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho \"{name} $*\" >> {}\n", log.display())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_editor_comes_from_the_settings_then_visual_then_editor_even_off_the_path() {
        use crate::api::choose_editor;
        let tmp = repo("editors");
        let (bin, log) = (tmp.0.join("bin"), tmp.0.join("log"));
        let zed = script(&bin, "zed", &log);
        let mine = script(&bin, "mything", &log);
        let vim = script(&bin, "vim", &log);
        let cursor = script(&bin, "cursor", &log);
        let (zed, mine, vim, cursor) = (zed.display().to_string(), mine.display().to_string(), vim.display().to_string(), cursor.display().to_string());

        // The settings file wins, by path: a known editor keeps its way of taking a line.
        let chosen = choose_editor(&cursor, Some(&zed), Some(&mine)).unwrap();
        assert_eq!((chosen.id.as_str(), chosen.source), ("cursor", "settings"));
        assert_eq!(chosen.style, crate::api::Style::Goto);
        // Then $VISUAL.
        let chosen = choose_editor("", Some(&zed), Some(&mine)).unwrap();
        assert_eq!((chosen.id.as_str(), chosen.source), ("zed", "$VISUAL"));
        // A terminal editor cannot be opened from here, so $VISUAL is passed over for $EDITOR,
        // and an unknown program with flags is kept as it is, minus the wait flag.
        let chosen = choose_editor("", Some(&vim), Some(&format!("{mine} --reuse -w"))).unwrap();
        assert_eq!((chosen.id.as_str(), chosen.source, chosen.terminal), ("custom", "$EDITOR", false));
        assert_eq!(chosen.extra, vec!["--reuse"]);
        // A setting that names nothing installed is ignored rather than breaking Open.
        let chosen = choose_editor("/nonexistent/editor", Some(&zed), None).unwrap();
        assert_eq!(chosen.source, "$VISUAL");
        // An explicit terminal editor in the settings is kept, and flagged.
        assert!(choose_editor(&vim, None, None).unwrap().terminal);
        // Nothing set: whatever is installed, which is a program or the file manager, never an error.
        assert!(choose_editor("", None, Some("/nonexistent/x")).is_none_or(|c| matches!(c.source, "installed" | "files")));
    }

    #[tokio::test]
    async fn open_uses_the_editor_from_the_settings_and_remaps_worktree_paths() {
        let tmp = repo("api-paths-2");
        let project = tmp.0.canonicalize().unwrap();
        std::fs::create_dir_all(project.join("app/home")).unwrap();
        std::fs::write(project.join("app/home/page.tsx"), "in the project\n").unwrap();
        let (bin, log) = (project.join("bin"), project.join("editor.log"));
        let mine = script(&bin, "mything", &log);
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        // An agent reports a path in its worktree; it means the file in the project.
        let tree = crate::git::worktree_dir(&project, "run1", "page-home");
        std::fs::create_dir_all(tree.join("app/home")).unwrap();
        std::fs::write(tree.join("app/home/page.tsx"), "in the worktree\n").unwrap();
        std::fs::write(tree.join("only-here.ts"), "x\n").unwrap();
        let raw = tree.join("app/home/page.tsx").display().to_string();
        let only = tree.join("only-here.ts").display().to_string();
        let up = format!("{}/../../etc/passwd", tree.display());
        let (_, found) = client.post("/api/paths", json!({ "paths": [raw, only, up] })).await;
        assert_eq!(found["paths"][&raw], project.join("app/home/page.tsx").display().to_string().as_str(), "{found}");
        assert_eq!(found["relative"][&raw], "app/home/page.tsx");
        // Not merged yet: the worktree's own file, reported by its path in the project.
        assert_eq!(found["relative"][&only], "only-here.ts");
        assert!(found["paths"][&only].as_str().unwrap().ends_with("only-here.ts"));
        assert!(found["paths"][&up].is_null(), "{found}");
        let (status, preview) = client.get(&format!("/api/file?path={}", raw.replace('/', "%2F"))).await;
        assert_eq!((status, preview["content"].as_str(), preview["relative"].as_str()), (200, Some("in the project\n"), Some("app/home/page.tsx")));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hostname", tree.join("link")).unwrap();
            let link = tree.join("link").display().to_string();
            let (_, found) = client.post("/api/paths", json!({ "paths": [link] })).await;
            assert!(found["paths"].as_object().unwrap().values().all(|v| v.is_null()), "a link out of a worktree is not followed: {found}");
        }
        assert_eq!(
            crate::git::unroot_worktrees(&project, &format!("--- a/{0}/app/x.ts\n+++ b/{0}/app/x.ts\n", tree.display())),
            "--- a/app/x.ts\n+++ b/app/x.ts\n"
        );
        std::fs::remove_dir_all(&tree).unwrap();

        // The settings file's editor is used, and survives in the file.
        assert_eq!(client.post("/api/settings", json!({ "editor": "/nonexistent/ed" })).await.0, 400);
        let (status, saved) = client.post("/api/settings", json!({ "editor": mine.display().to_string() })).await;
        assert_eq!((status, saved["editor"].as_str()), (200, Some(mine.display().to_string().as_str())), "{saved}");
        let (_, editors) = client.get("/api/editors").await;
        assert_eq!((editors["default"].as_str(), editors["source"].as_str(), editors["override_found"].as_bool()), (Some("custom"), Some("settings"), Some(true)));
        assert!(editors["editors"].as_array().unwrap().iter().any(|e| e["id"] == "custom" && e["name"] == "mything"));
        let (status, opened) = client.post("/api/open", json!({ "path": "app/home/page.tsx", "line": 3 })).await;
        assert_eq!(status, 202, "{opened}");
        for _ in 0..200 {
            if log.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.starts_with("mything ") && logged.contains("app/home/page.tsx"), "{logged}");
        let config: Value = serde_json::from_str(&std::fs::read_to_string(project.join(".taskmaster/config.json")).unwrap()).unwrap();
        assert_eq!(config["editor"], mine.display().to_string().as_str());
        assert_eq!(client.post("/api/settings", json!({ "editor": "" })).await.1["editor"], "");
    }

    /// Whatever goes wrong under /api, the reply is JSON with a message in `error`.
    async fn raw(client: &Client, method: &str, path: &str, content_type: Option<&str>, body: &str) -> (u16, String, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(&client.addr).await.unwrap();
        let ct = content_type.map(|c| format!("content-type: {c}\r\n")).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {}\r\n{ct}content-length: {}\r\nconnection: close\r\n\r\n{body}",
            client.token.clone().unwrap_or_default(),
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        let (head, rest) = reply.split_once("\r\n\r\n").unwrap_or(("", &reply));
        let status = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
        (status, head.to_lowercase(), rest.to_string())
    }

    #[test]
    fn keys_credentials_and_git_internals_are_never_previewed_and_addresses_lose_their_passwords() {
        use crate::api::{redact, sensitive};
        for secret in [
            "/home/a/.ssh/id_rsa",
            "/home/a/.aws/credentials",
            "/p/.git/config",
            "/p/.env",
            "/p/app/.env.local",
            "/p/server.pem",
            "/p/deploy/prod.key",
            "/home/a/.config/gh/hosts.yml",
            "/home/a/.netrc",
            "/w/secrets.env",
            "/w/.TASKMASTER/../.ENV",
        ] {
            assert!(sensitive(Path::new(secret)), "{secret}");
        }
        for fine in ["/p/.env.example", "/p/src/main.rs", "/p/README.md", "/p/.github/workflows/ci.yml", "/p/environment.ts", "/p/keys.md"] {
            assert!(!sensitive(Path::new(fine)), "{fine}");
        }
        assert_eq!(
            redact("fatal: unable to access 'https://naren:ghp_abc123@github.com/o/r.git/': 403 and http://h.example/x"),
            "fatal: unable to access 'https://***@github.com/o/r.git/': 403 and http://h.example/x"
        );
        assert_eq!(redact("nothing to hide"), "nothing to hide");
    }

    #[tokio::test]
    async fn the_ui_endpoints_refuse_bad_input_with_json_errors_and_never_leave_the_project() {
        let tmp = repo("api-harden");
        let project = tmp.0.canonicalize().unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(project.join(".env"), "TOKEN=hunter2\n").unwrap();
        std::fs::write(project.join(".env.example"), "TOKEN=\n").unwrap();
        std::fs::write(project.join("big.txt"), vec![b'a'; 2 * 1024 * 1024]).unwrap();
        std::fs::write(project.join("bin.dat"), [0u8, 1, 2]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", project.join("link")).unwrap();
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        // A body that is not JSON, the wrong type, a missing field, a bad number, a wrong method: all JSON.
        for (method, path, ct, body, status) in [
            ("POST", "/api/open", Some("application/json"), "{not json", 400),
            ("POST", "/api/open", Some("application/json"), "{}", 422),
            ("POST", "/api/open", Some("text/plain"), "{}", 415),
            ("POST", "/api/open", None, "{}", 415),
            ("POST", "/api/paths", Some("application/json"), r#"{"paths":"x"}"#, 422),
            ("POST", "/api/settings", Some("application/json"), r#"{"approvals":7}"#, 422),
            ("GET", "/api/gh/pr/abc", None, "", 400),
            ("GET", "/api/gh/pr/-1", None, "", 400),
            ("DELETE", "/api/settings", None, "", 405),
            ("GET", "/api/nope", None, "", 404),
            ("POST", "/api/runs/x/notes", Some("application/json"), r#"{"to":"master"}"#, 422),
        ] {
            let (got, head, text) = raw(&client, method, path, ct, body).await;
            assert_eq!(got, status, "{method} {path}: {text}");
            assert!(head.contains("content-type: application/json"), "{method} {path}: {head}");
            let reply: Value = serde_json::from_str(&text).unwrap_or_else(|_| panic!("{method} {path}: {text}"));
            assert!(reply["error"].as_str().is_some_and(|e| !e.is_empty()), "{method} {path}: {reply}");
        }
        // Known failures keep one shape too.
        for (status, reply) in [
            client.get("/api/runs/nope").await,
            client.get("/api/file?path=nope.rs").await,
            client.get("/api/file").await,
            client.post("/api/open", json!({ "path": "nope.rs" })).await,
            client.post("/api/runs/nope/pr", json!({})).await,
            client.get("/api/runs/nope/diff").await,
            client.post("/api/settings", json!({ "approvals": "sometimes" })).await,
        ] {
            assert!(status >= 400 && reply["error"].as_str().is_some(), "{status}: {reply}");
        }

        // Nothing outside the project (or the home directory) is read, opened or reported as real.
        let up = "../".repeat(12) + "etc/passwd";
        for path in [
            "/etc/passwd".to_string(),
            "src/../../../../../../etc/passwd".to_string(),
            up.clone(),
            "~/../../etc/passwd".to_string(),
            "link".to_string(),
            "src".to_string(),
            "src/\0main.rs".to_string(),
            "x".repeat(5000),
        ] {
            let query = path.replace('\0', "%00").replace('/', "%2F");
            let (status, reply) = client.get(&format!("/api/file?path={query}")).await;
            assert_eq!(status, 404, "{path}: {reply}");
            let (status, reply) = client.post("/api/open", json!({ "path": path })).await;
            assert_eq!(status, 404, "{path}: {reply}");
            let (_, found) = client.post("/api/paths", json!({ "paths": [path.clone()] })).await;
            assert!(found["paths"][&path].is_null(), "{path}: {found}");
        }
        // A path with `..` that comes back inside is fine.
        assert_eq!(client.get("/api/file?path=src%2F..%2Fsrc%2Fmain.rs").await.0, 200);
        // Secrets inside the project are real but not shown; a template is.
        let (status, reply) = client.get("/api/file?path=.env").await;
        assert_eq!(status, 403, "{reply}");
        assert!(!reply.to_string().contains("hunter2"));
        assert_eq!(client.get("/api/file?path=.env.example").await.0, 200);
        // Only the start of a big file is read; a binary file has no text preview.
        let (status, big) = client.get("/api/file?path=big.txt").await;
        assert_eq!((status, big["truncated"].as_bool(), big["content"].as_str().unwrap().len()), (200, Some(true), 512 * 1024));
        assert_eq!(client.get("/api/file?path=bin.dat").await.0, 415);

        // Settings: bad values change nothing.
        let before = std::fs::read_to_string(project.join(".taskmaster/config.json")).unwrap();
        assert_eq!(client.post("/api/settings", json!({ "approval_timeout_secs": 999999 })).await.0, 400);
        assert_eq!(client.post("/api/settings", json!({ "editor": "x\nrm -rf" })).await.0, 400);
        assert_eq!(std::fs::read_to_string(project.join(".taskmaster/config.json")).unwrap(), before, "a refused change writes nothing");
        // The PR draft needs a finished run; the pull request lookup needs a number.
        assert_eq!(client.get("/api/gh/pr/99999999999999999999").await.0, 400);
        assert_eq!(client.get("/api/approvals?run=..").await.0, 400);
        let planned = serde_json::to_value(plan(&[("a", &[])])).unwrap();
        client.post("/api/runs", json!({ "id": "pr1", "plan": planned })).await;
        client.settled("pr1", "awaiting_approval").await;
        let (status, reply) = client.post("/api/runs/pr1/pr", json!({})).await;
        assert_eq!(status, 409, "{reply}");
        assert!(reply["error"].as_str().unwrap().contains("done"));
        assert_eq!(client.get("/api/runs/pr1/diff?scope=page").await.0, 409);
        // Review waits for a finished run.
        assert_eq!(client.post("/api/runs/pr1/review", json!({})).await.0, 409);
    }

    fn have_python() -> bool {
        std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// A stub dev server: Python's file server on `$PORT`, in the directory it was started in.
    const STUB: &str = "python3 -m http.server {port} --bind 127.0.0.1";

    async fn fetch(port: u16, path: &str) -> Option<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        stream.write_all(format!("GET {path} HTTP/1.0\r\nhost: x\r\n\r\n").as_bytes()).await.ok()?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.ok()?;
        reply.split_once("\r\n\r\n").map(|(_, body)| body.to_string())
    }

    async fn until<F: std::future::Future<Output = bool>>(what: &str, mut check: impl FnMut() -> F) {
        for _ in 0..400 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[test]
    fn the_dev_command_is_the_settings_or_the_projects_own_script() {
        use crate::preview::dev_command;
        let tmp = repo("preview-cmd");
        assert_eq!(dev_command(&tmp.0, "", 3000), None);
        assert_eq!(dev_command(&tmp.0, "serve --port {port}", 4000).as_deref(), Some("serve --port 4000"));
        // Installed packages, so no install step goes first (see the fallback test).
        std::fs::create_dir_all(tmp.0.join("node_modules")).unwrap();
        std::fs::write(tmp.0.join("package.json"), r#"{"scripts":{"dev":"next dev"},"dependencies":{"next":"15"}}"#).unwrap();
        assert_eq!(dev_command(&tmp.0, "", 3001).as_deref(), Some("npm run dev -- -p 3001 -H 127.0.0.1"));
        std::fs::write(tmp.0.join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(tmp.0.join("package.json"), r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"6"}}"#).unwrap();
        assert_eq!(dev_command(&tmp.0, "", 3002).as_deref(), Some("pnpm run dev -- --port 3002 --host 127.0.0.1 --strictPort"));
        std::fs::write(tmp.0.join("package.json"), r#"{"scripts":{"build":"x"}}"#).unwrap();
        assert_eq!(dev_command(&tmp.0, "", 3003), None);
        assert_eq!(dev_command(&tmp.0, "   ", 3003), None);
    }

    #[tokio::test]
    async fn a_run_gets_a_dev_server_that_is_tracked_and_stopped() {
        if !have_python() {
            eprintln!("python3 is not installed; skipping the preview test");
            return;
        }
        let tmp = seeded("preview");
        let world = World::new(Mode::Write);
        let auth = Arc::new(PairingAuth::new(Records::files(&tmp.0)).unwrap());
        let code = auth.code().to_string();
        let mut options = ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
        let w = world.clone();
        options.harness = Arc::new(move |_| Ok(Arc::new(Fake(w.clone())) as Arc<dyn Harness>));
        options.preview_command = STUB.into();
        choose_roles(&options.project);
        let server = Server::serve(options).await.unwrap();
        let mut client = Client { addr: server.url.trim_start_matches("http://").trim_end_matches('/').to_string(), token: None };
        client.token = Some(client.post("/api/pair", json!({ "code": code })).await.1["token"].as_str().unwrap().to_string());

        let planned = serde_json::to_value(plan(&[("a", &[])])).unwrap();
        client.post("/api/runs", json!({ "id": "pv", "plan": planned })).await;
        client.settled("pv", "awaiting_approval").await;
        let (_, before) = client.get("/api/runs/pv/preview").await;
        assert_eq!((before["available"].as_bool(), before["state"].as_str(), before["url"].is_null()), (Some(true), Some("none"), true), "{before}");
        // Nothing to show until it has been built.
        assert_eq!(client.post("/api/runs/pv/preview", json!({})).await.0, 409);
        assert_eq!(client.get("/api/runs/nope/preview").await.0, 404);
        client.post("/api/runs/pv/approve", json!({})).await;
        client.settled("pv", "done").await;

        // Started once, tracked, and it serves the project the run was merged into.
        let (status, started) = client.post("/api/runs/pv/preview", json!({})).await;
        assert_eq!(status, 202, "{started}");
        let port = started["port"].as_u64().unwrap() as u16;
        assert!(started["url"].as_str().unwrap().starts_with(&format!("http://127.0.0.1:{port}")));
        until("the preview to run", || async { client.get("/api/runs/pv/preview").await.1["state"] == "running" }).await;
        let again = client.post("/api/runs/pv/preview", json!({})).await.1;
        assert_eq!(again["port"].as_u64().unwrap() as u16, port, "a second start finds the first");
        let listing = fetch(port, "/").await.unwrap();
        assert!(listing.contains("app/"), "{listing}");

        // Stopping ends it, and the port goes quiet.
        let (_, stopped) = client.post("/api/runs/pv/preview/stop", json!({})).await;
        assert_eq!(stopped["state"], "stopped");
        until("the port to close", || async { fetch(port, "/").await.is_none() }).await;
        // A stopped preview can be started again, on a new port if need be.
        let (_, restarted) = client.post("/api/runs/pv/preview", json!({})).await;
        let port = restarted["port"].as_u64().unwrap() as u16;
        until("the second preview", || async { client.get("/api/runs/pv/preview").await.1["state"] == "running" }).await;

        // The daemon going away takes its previews with it.
        drop(server);
        until("the port to close after the daemon stops", || async { fetch(port, "/").await.is_none() }).await;
    }

    #[tokio::test]
    async fn a_run_still_on_its_branch_is_previewed_from_a_checkout_that_goes_with_it() {
        use crate::preview::Previews;
        if !have_python() {
            return;
        }
        let tmp = seeded("preview-branch");
        git(&tmp.0, &["checkout", "-q", "-b", "taskmaster/r9"]);
        std::fs::write(tmp.0.join("only-on-the-branch.txt"), "branch content\n").unwrap();
        git(&tmp.0, &["add", "."]);
        git(&tmp.0, &["commit", "-q", "-m", "branch work"]);
        git(&tmp.0, &["checkout", "-q", "main"]);
        let project = tmp.0.canonicalize().unwrap();
        assert!(!project.join("only-on-the-branch.txt").exists());

        let previews = Previews::new(project.clone(), STUB.into());
        let started = previews.start("r9", Some("taskmaster/r9")).await.unwrap();
        let tree = std::path::PathBuf::from(&started.dir);
        assert!(tree.join("only-on-the-branch.txt").is_file() && tree != project);
        until("the preview", || async { fetch(started.port, "/only-on-the-branch.txt").await.is_some_and(|b| b.contains("branch content")) }).await;
        previews.stop("r9");
        until("the checkout to be removed", || async { !tree.exists() }).await;
        assert_eq!(previews.status("r9").unwrap().state, "stopped");
        let worktrees = git_out(&project, &["worktree", "list"]);
        assert_eq!(worktrees.lines().count(), 1, "{worktrees}");
    }

    #[tokio::test]
    async fn a_dev_server_that_dies_is_reported_with_what_it_said() {
        use crate::preview::Previews;
        let tmp = repo("preview-fail");
        let project = tmp.0.canonicalize().unwrap();
        let previews = Previews::new(project.clone(), "echo cannot bind {port} >&2; exit 3".into());
        previews.start("r1", None).await.unwrap();
        until("the failure", || async { previews.status("r1").unwrap().state == "failed" }).await;
        let reason = previews.status("r1").unwrap().reason.unwrap();
        assert!(reason.contains("code 3") && reason.contains("cannot bind"), "{reason}");
        // No dev server and no page: the preview still opens, on a page listing the files.
        let none = Previews::new(project.clone(), String::new());
        assert!(none.available(), "no scripts means static files");
        let listed = none.start("r2", None).await.unwrap();
        assert_eq!((listed.state, listed.command.as_str()), ("running", "built-in static file server"));
        assert!(listed.reason.unwrap_or_default().contains("lists the project's files"));
        none.stop("r2");

        // A dev server that dies before it answers, in a project with a page: the files are
        // served instead, and the reason says why.
        std::fs::write(project.join("index.html"), "<h1>fallback</h1>").unwrap();
        let dying = Previews::new(project, "echo boom >&2; exit 4".into());
        dying.start("r3", None).await.unwrap();
        until("the static fallback", || async { dying.status("r3").is_some_and(|s| s.state == "running" && s.command.contains("static")) }).await;
        let fell = dying.status("r3").unwrap();
        assert!(fell.reason.as_deref().unwrap_or("").contains("serving the project's files instead"), "{fell:?}");
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", fell.port)).await.is_ok());
        dying.stop("r3");
        assert_eq!(dying.status("r3").unwrap().state, "stopped");
    }

    #[test]
    fn the_dev_command_falls_back_to_start_and_installs_missing_packages() {
        use crate::preview::dev_command;
        let tmp = repo("preview-scripts");
        let dir = tmp.0.as_path();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"start":"node server.js","test":"node --test"}}"#).unwrap();
        assert_eq!(dev_command(dir, "", 4000).as_deref(), Some("npm run start"));
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"^5"}}"#).unwrap();
        assert_eq!(
            dev_command(dir, "", 4000).as_deref(),
            Some("npm install --no-audit --no-fund --loglevel=error && npm run dev -- --port 4000 --host 127.0.0.1 --strictPort")
        );
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        assert_eq!(dev_command(dir, "", 4000).as_deref(), Some("npm run dev -- --port 4000 --host 127.0.0.1 --strictPort"));
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"node --test"}}"#).unwrap();
        assert_eq!(dev_command(dir, "", 4000), None, "no script that serves: the static server takes it");
        assert_eq!(dev_command(dir, "serve -p {port}", 4000).as_deref(), Some("serve -p 4000"));
    }

    #[tokio::test]
    async fn a_project_with_no_dev_script_is_served_as_static_files() {
        use crate::preview::Previews;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = repo("preview-static");
        let project = tmp.0.canonicalize().unwrap();
        std::fs::create_dir_all(project.join("app")).unwrap();
        std::fs::write(project.join("app/index.html"), "<h1>hi</h1>").unwrap();
        let previews = Previews::new(project, String::new());
        let started = previews.start("s1", None).await.unwrap();
        assert_eq!(started.state, "running");
        let get = |path: &'static str| async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", started.port)).await.unwrap();
            stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).await.unwrap();
            text
        };
        assert!(get("/app/index.html").await.contains("<h1>hi</h1>"));
        assert!(get("/.git/HEAD").await.starts_with("HTTP/1.1 404"));
        previews.stop("s1");
        assert_eq!(previews.status("s1").unwrap().state, "stopped");
    }

    /// `pgrp` from `/proc/<pid>/stat`, the field after state and ppid.
    fn process_group_of(pid: u32) -> Option<u32> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = text.rsplit_once(')')?.1;
        rest.split_whitespace().nth(2)?.parse().ok()
    }

    #[tokio::test]
    async fn stopping_a_preview_kills_the_whole_process_group() {
        use crate::preview::Previews;
        if !have_python() {
            return;
        }
        let tmp = repo("preview-group");
        let project = tmp.0.canonicalize().unwrap();
        let pidfile = project.join("server.pid");
        let script = project.join("serve.py");
        // The shell traps SIGTERM and the server ignores SIGHUP, so killing only the shell
        // would leave the server up. The server records its own pid and sits in the shell's group.
        std::fs::write(
            &script,
            "import os, signal\n\
             from http.server import ThreadingHTTPServer, SimpleHTTPRequestHandler\n\
             signal.signal(signal.SIGHUP, signal.SIG_IGN)\n\
             open(os.environ['PIDFILE'], 'w').write(str(os.getpid()))\n\
             port = int(os.environ['PORT'])\n\
             ThreadingHTTPServer(('127.0.0.1', port), SimpleHTTPRequestHandler).serve_forever()\n",
        )
        .unwrap();
        let command = format!("PIDFILE='{}' python3 '{}' & trap 'exit 0' TERM; wait", pidfile.display(), script.display());
        let previews = Previews::new(project, command);
        let started = previews.start("r", None).await.unwrap();
        until("the preview", || async { fetch(started.port, "/").await.is_some() }).await;
        let child: u32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let group = process_group_of(child).unwrap();
        assert_ne!(group, process_group_of(std::process::id()).unwrap(), "the dev server must not share the test's process group");
        previews.stop("r");
        until("the server process to die", || async { process_group_of(child).is_none() }).await;
        until("the port to close", || async { fetch(started.port, "/").await.is_none() }).await;
    }

    #[tokio::test]
    async fn dropping_the_preview_handle_stops_its_dev_server() {
        use crate::preview::Previews;
        if !have_python() {
            return;
        }
        let tmp = repo("preview-drop");
        let project = tmp.0.canonicalize().unwrap();
        let port = {
            let previews = Previews::new(project, STUB.into());
            let started = previews.start("r", None).await.unwrap();
            until("the preview to run", || async { previews.status("r").unwrap().state == "running" }).await;
            started.port
        };
        until("the port to close after the handle is dropped", || async { fetch(port, "/").await.is_none() }).await;
    }

    #[tokio::test]
    async fn a_second_start_returns_the_dev_server_already_running() {
        use crate::preview::Previews;
        if !have_python() {
            return;
        }
        let tmp = repo("preview-once");
        let project = tmp.0.canonicalize().unwrap();
        let marker = project.join("starts.txt");
        let command = format!("echo started >> '{}'; python3 -m http.server {{port}} --bind 127.0.0.1", marker.display());
        let previews = Arc::new(Previews::new(project, command));
        let (first, second) = tokio::join!(previews.start("run", None), previews.start("run", None));
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(first.port, second.port);
        assert_ne!(first.port, 0);
        assert_eq!(first.url, second.url);
        until("the preview to run", || async { previews.status("run").unwrap().state == "running" }).await;
        let started = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(started.lines().filter(|line| *line == "started").count(), 1, "{started}");
    }

    #[tokio::test]
    async fn two_mode_cancel_escalates_and_resumes_a_cancelled_run_keeping_passed_pages() {
        let tmp = seeded("api-two-mode-cancel");
        // Worker blocks on a barrier nobody else reaches, proving Force interrupts it immediately.
        let barrier = Arc::new(Barrier::new(2));
        let blocked_world = World::new(Mode::Parallel(Some(barrier)));
        let (_server, mut client, code) = server(&tmp, &blocked_world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        let planned = serde_json::to_value(plan(&[("a", &[]), ("b", &[])])).unwrap();
        let (status, made) = client.post("/api/runs", json!({ "id": "stop", "plan": planned, "yes": true, "parallel": 1 })).await;
        assert_eq!(status, 202, "{made}");

        // Wait until worker `a` is actively inside the blocked call.
        for _ in 0..200 {
            if blocked_world.inflight.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(blocked_world.inflight.load(Ordering::SeqCst) >= 1, "worker never started");

        // 1. First cancel with explicit graceful mode sets `cancelling: "graceful"` without killing `a`.
        let (status, first) = client.post("/api/runs/stop/cancel", json!({ "mode": "graceful" })).await;
        assert_eq!(status, 202, "{first}");
        assert_eq!(first["mode"], "graceful");
        let (_, draining) = client.get("/api/runs/stop").await;
        assert_eq!(draining["active"], true, "{draining}");
        assert_eq!(draining["cancelling"], "graceful", "{draining}");

        // 2. A repeat cancel (with empty body) auto-escalates to `force` and interrupts `a` immediately.
        let (status, second) = client.call("POST", "/api/runs/stop/cancel", None).await;
        assert_eq!(status, 202, "{second}");
        assert_eq!(second["mode"], "force");
        let cancelled = client.settled("stop", "failed").await;
        assert_eq!(cancelled["cancelled"], true, "{cancelled}");
        assert!(cancelled["cancelling"].is_null(), "{cancelled}");

        // 3. Now start a fresh server with Mode::Write on the same repo and resume the cancelled run!
        drop(_server);
        let write_world = World::new(Mode::Write);
        let (_server2, mut client2, code2) = server(&tmp, &write_world).await;
        let (_, paired2) = client2.post("/api/pair", json!({ "code": code2 })).await;
        client2.token = Some(paired2["token"].as_str().unwrap().to_string());

        let (status, resumed) = client2.post("/api/runs/stop/resume", json!({})).await;
        assert_eq!(status, 202, "{resumed}");
        let done = client2.settled("stop", "done").await;
        assert_eq!(done["cancelled"], false, "{done}");
        assert!(tmp.0.join("app/a/page.ts").is_file() && tmp.0.join("app/b/page.ts").is_file());
    }

    #[tokio::test]
    async fn studio_endpoints_atlas_doctor_policy_plan_edit_checkpoints_fork_and_redacted_bundle() {
        let tmp = seeded("studio-api");
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        // 1. Living Codebase Atlas
        let (status, atlas) = client.get("/api/atlas?refresh=1").await;
        assert_eq!(status, 200, "{atlas}");
        assert!(atlas["atlas"]["total_files"].as_u64().unwrap_or(0) >= 1, "{atlas}");
        assert!(tmp.0.join(".taskmaster/atlas.json").is_file());

        // 2. Preflight Doctor
        let (status, doc) = client.get("/api/doctor").await;
        assert_eq!(status, 200, "{doc}");
        assert!(doc["doctor"]["checks"].as_array().is_some_and(|c| !c.is_empty()), "{doc}");

        // 3. Policy-as-Code GET + POST (repo_file: true writes taskmaster.policy.toml)
        let (status, pol) = client.get("/api/policy").await;
        assert_eq!(status, 200, "{pol}");
        assert!(pol["policy"]["denied_globs"].as_array().is_some());

        let (status, saved_pol) = client
            .post(
                "/api/policy",
                json!({
                    "repo_file": true,
                    "policy": {
                        "protected_globs": ["src/auth/**"],
                        "denied_globs": [".env", ".env.*"],
                        "allowed_fetch_hosts": [],
                        "denied_fetch_hosts": ["evil.example"],
                        "max_run_cost_usd": 5.0,
                        "daily_budget_usd": 20.0,
                        "require_cross_family_review": true,
                        "redact_patterns": ["SUPER_SECRET_MARKER_999"]
                    }
                }),
            )
            .await;
        assert_eq!(status, 200, "{saved_pol}");
        assert_eq!(saved_pol["source"], "taskmaster.policy.toml");
        assert!(tmp.0.join("taskmaster.policy.toml").is_file());

        // Invalid negative cap is rejected.
        let (bad_status, _) = client
            .post(
                "/api/policy",
                json!({ "policy": { "max_run_cost_usd": -1.0 } }),
            )
            .await;
        assert_eq!(bad_status, 400);

        // 4. Create a run awaiting approval, edit plan interactively, then approve.
        let mut edited_plan = plan(&[("a", &[]), ("b", &[])]);
        let (status, created) = client
            .post("/api/runs", json!({ "id": "studio", "plan": edited_plan, "yes": false }))
            .await;
        assert_eq!(status, 202, "{created}");
        let awaiting = client.settled("studio", "awaiting_approval").await;
        assert_eq!(awaiting["phase"], "awaiting_approval");

        // Cyclic plan edit is rejected with 400.
        let mut cyclic = edited_plan.clone();
        cyclic.pages[0].depends_on = vec!["b".into()];
        cyclic.pages[1].depends_on = vec!["a".into()];
        let (cyc_status, _) = client.post("/api/runs/studio/plan", json!({ "plan": cyclic })).await;
        assert_eq!(cyc_status, 400);

        // Valid plan edit updates brief and dependency edge.
        edited_plan.pages[0].brief = "Build page a with custom SUPER_SECRET_MARKER_999 and sk-ant-api03-abcdefghijklmnopqrstuvwx".into();
        edited_plan.pages[1].depends_on = vec!["a".into()];
        let (edit_status, updated) = client.post("/api/runs/studio/plan", json!({ "plan": edited_plan })).await;
        assert_eq!(edit_status, 200, "{updated}");
        assert_eq!(updated["plan"]["pages"][1]["depends_on"], json!(["a"]));

        // Approve & let it complete.
        let (app_status, _) = client.post("/api/runs/studio/approve", json!({})).await;
        assert_eq!(app_status, 202);
        let done = client.settled("studio", "done").await;
        assert_eq!(done["phase"], "done");

        // 5. Checkpoints endpoint returns committed unit milestones.
        let (cp_status, cps) = client.get("/api/runs/studio/checkpoints").await;
        assert_eq!(cp_status, 200, "{cps}");
        assert!(
            cps["checkpoints"].as_array().is_some_and(|list| !list.is_empty()),
            "expected at least one checkpoint: {cps}"
        );

        // 6. Bundle endpoint exports tmrun/v1 with cache_economics and secret redaction!
        let (b_status, bundle) = client.get("/api/runs/studio/bundle").await;
        assert_eq!(b_status, 200, "{bundle}");
        assert_eq!(bundle["format"], "tmrun/v1");
        assert!(bundle["cache_economics"]["total_cost_usd"].is_number());
        let bundle_text = serde_json::to_string(&bundle).unwrap();
        assert!(!bundle_text.contains("SUPER_SECRET_MARKER_999"), "custom policy pattern leaked: {bundle_text}");
        assert!(!bundle_text.contains("sk-ant-api03-abcdefghijklmnopqrstuvwx"), "API key leaked: {bundle_text}");
        assert!(bundle_text.contains("•••"), "expected redaction marker in bundle");

        // 7. Fork endpoint creates and runs a forked run.
        let (f_status, forked) = client
            .post("/api/runs/studio/fork", json!({ "id": "studio-fork", "yes": true, "max_cost_usd": 2.0 }))
            .await;
        assert_eq!(f_status, 202, "{forked}");
        assert_eq!(forked["id"], "studio-fork");
        assert_eq!(forked["forked_from"], "studio");
        let fork_done = client.settled("studio-fork", "done").await;
        assert_eq!(fork_done["phase"], "done");
    }

    #[tokio::test]
    async fn ops_endpoints_and_idempotency_middleware_work_end_to_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = repo("ops-e2e");
        let world = World::new(Mode::Write);
        let (_server, mut client, code) = server(&tmp, &world).await;
        let (_, paired) = client.post("/api/pair", json!({ "code": code, "name": "ops" })).await;
        client.token = Some(paired["token"].as_str().unwrap().to_string());

        // 1. Prometheus /metrics endpoint (public text/plain)
        let (m_status, metrics_text) = client.text("/metrics").await;
        assert_eq!(m_status, 200);
        assert!(metrics_text.contains("taskmaster_active_runs"));

        // 2. Monthly spend + egress log + GC
        let (s_status, spend) = client.get("/api/metrics/spend?threshold=50").await;
        assert_eq!(s_status, 200, "{spend}");
        assert_eq!(spend["spend"]["alert_triggered"], false);

        let (e_status, egress) = client.get("/api/egress?limit=10").await;
        assert_eq!(e_status, 200, "{egress}");
        assert!(egress["entries"].is_array());

        let (gc_status, gc) = client.post("/api/gc", json!({ "max_runs": 10 })).await;
        assert_eq!(gc_status, 200, "{gc}");
        assert!(gc["gc"]["removed_runs"].is_array());

        // 3. Idempotency-Key deduplication on POST /api/schedules
        let post_with_key = |addr: String, token: String, key: &'static str, body: String| async move {
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            let req = format!(
                "POST /api/schedules HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nauthorization: Bearer {token}\r\nidempotency-key: {key}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).await.unwrap();
            let (head, rest) = reply.split_once("\r\n\r\n").unwrap_or(("", &reply));
            let status: u16 = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
            (status, serde_json::from_str::<Value>(rest).unwrap_or(Value::Null))
        };
        let token = client.token.clone().unwrap();
        let sched_body = json!({ "name": "Nightly audit", "schedule": "nightly", "prompt": "Run checks" }).to_string();
        let (st1, r1) = post_with_key(client.addr.clone(), token.clone(), "idem-sched-1", sched_body.clone()).await;
        let (st2, r2) = post_with_key(client.addr.clone(), token.clone(), "idem-sched-1", sched_body).await;
        assert_eq!(st1, 200, "{r1}");
        assert_eq!(st2, 200, "{r2}");
        assert_eq!(r1["schedule"]["id"], r2["schedule"]["id"]);
        let (_, listed_scheds) = client.get("/api/schedules").await;
        assert_eq!(listed_scheds["schedules"].as_array().unwrap().len(), 1);

        // 4. Remote runners & environments never leak tokens in API JSON
        let (r_status, runner) = client
            .post(
                "/api/runners",
                json!({ "name": "gpu-box", "url": "http://127.0.0.1:9999", "token": "super-secret-runner-tok" }),
            )
            .await;
        assert_eq!(r_status, 200, "{runner}");
        let runner_text = serde_json::to_string(&runner).unwrap();
        assert!(!runner_text.contains("super-secret-runner-tok"));
        assert_eq!(runner["runner"]["has_token"], true);

        let (env_status, env_res) = client
            .post(
                "/api/environments",
                json!({ "name": "staging-vm", "url": "http://127.0.0.1:9998", "token": "env-secret-tok" }),
            )
            .await;
        assert_eq!(env_status, 200, "{env_res}");
        assert!(!serde_json::to_string(&env_res).unwrap().contains("env-secret-tok"));

        // 5. User MCP servers keep secret_env out of API JSON
        let (mcp_status, mcp) = client
            .post(
                "/api/mcp",
                json!({
                    "name": "postgres",
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-postgres"],
                    "roles": ["worker"],
                    "secret_env": { "PGPASSWORD": "db-password-999" }
                }),
            )
            .await;
        assert_eq!(mcp_status, 200, "{mcp}");
        assert!(!serde_json::to_string(&mcp).unwrap().contains("db-password-999"));
        assert_eq!(mcp["server"]["secret_env_keys"], json!(["PGPASSWORD"]));

        // 6. Notifications config validation & save
        let (n_bad, _) = client
            .post("/api/notifications", json!({ "desktop": false, "webhook_url": "http://evil.example/hook" }))
            .await;
        assert_eq!(n_bad, 400);
        let (n_ok, notif) = client
            .post("/api/notifications", json!({ "desktop": true, "webhook_url": "https://hooks.example.com/tm", "on_approval": true, "on_limit": true, "on_finish": true }))
            .await;
        assert_eq!(n_ok, 200, "{notif}");

        // 7. Issue URL parser, attachments, stratified scorecard, and eval suite
        let (i_status, parsed) = client
            .post("/api/issues/parse", json!({ "url": "https://github.com/acme/app/issues/77" }))
            .await;
        assert_eq!(i_status, 200, "{parsed}");
        assert_eq!(parsed["issue"]["number"], 77);
        assert_eq!(parsed["closing_link"], "Closes #77 (https://github.com/acme/app/issues/77)");

        let (a_status, att) = client
            .post(
                "/api/runs/demo-att/attachments",
                json!({ "name": "mockup.png", "mime": "image/png", "data_base64": "iVBORw0KGgo=" }),
            )
            .await;
        assert_eq!(a_status, 200, "{att}");
        let (al_status, listed_att) = client.get("/api/runs/demo-att/attachments").await;
        assert_eq!(al_status, 200, "{listed_att}");
        assert_eq!(listed_att["attachments"].as_array().unwrap().len(), 1);

        let (sc_status, strat) = client.get("/api/scorecard/stratified").await;
        assert_eq!(sc_status, 200, "{strat}");
        assert!(strat["slices"].is_array());

        let (ev_status, suite) = client.get("/api/eval/suite").await;
        assert_eq!(ev_status, 200, "{suite}");
        assert!(suite["tasks"].is_array());
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

#[cfg(test)]
mod role_defaults {
    use serde_json::json;

    #[test]
    fn no_role_has_a_built_in_harness() {
        let roles = crate::Roles::default();
        assert_eq!(roles.missing().len(), 5);
        let settings: crate::settings::Settings = serde_json::from_str(r#"{"roles":{"master":"claude"}}"#).unwrap();
        assert_eq!((settings.roles.master.as_str(), settings.roles.worker.as_str()), ("claude", ""));
    }

    #[test]
    fn a_run_takes_the_request_over_the_saved_choice_and_names_what_is_missing() {
        let saved = json!({ "master": "claude", "worker": "grok" });
        let chosen = json!({ "worker": "codex", "mid": "opencode", "reviewer": "agy", "verifier": "agy" });
        let roles = crate::Roles::resolve(saved.as_object().unwrap(), chosen.as_object().unwrap()).unwrap();
        assert_eq!((roles.master.as_str(), roles.worker.as_str(), roles.mid.as_str()), ("claude", "codex", "opencode"));
        let err = crate::Roles::resolve(saved.as_object().unwrap(), &Default::default()).unwrap_err().to_string();
        assert!(err.contains("mid, reviewer, verifier"), "{err}");
    }

    #[test]
    fn default_worker_time_limit_is_unlimited() {
        assert_eq!(crate::settings::DEFAULT_AGENT_SECS, 0);
        let cfg = crate::Config::new(std::path::PathBuf::from("/tmp"));
        assert_eq!(cfg.timeout, crate::settings::NO_LIMIT);
    }

    #[test]
    fn plan_json_accepts_summary_workers_alias_and_per_worker_context() {
        let raw = r#"{
            "summary": "Build auth and billing in parallel",
            "workers": [
                {
                    "id": "auth",
                    "owned_glob": "src/auth/**",
                    "brief": "Implement JWT middleware and login route.",
                    "context": "Use argon2 for hashing and export requireAuth from src/auth/index.ts."
                },
                {
                    "id": "billing",
                    "owned_glob": "src/billing/**",
                    "brief": "Implement Stripe webhook handler.",
                    "context": "Reuse UserId from contracts/types.ts.",
                    "depends_on": ["auth"]
                }
            ]
        }"#;
        let (plan, _) = crate::reply::parse_plan(raw).unwrap();
        assert_eq!(plan.summary, "Build auth and billing in parallel");
        assert_eq!(plan.pages.len(), 2);
        assert_eq!(plan.pages[0].context, "Use argon2 for hashing and export requireAuth from src/auth/index.ts.");
        assert_eq!(plan.pages[1].context, "Reuse UserId from contracts/types.ts.");
        assert_eq!(plan.pages[1].depends_on, vec!["auth".to_string()]);
    }
}

#[tokio::test]
async fn direct_fast_path_skips_master_planning_for_scoped_edits_and_keeps_approval_gate() {
    let tmp = repo("fast-path");
    std::fs::create_dir_all(tmp.0.join("src")).unwrap();
    std::fs::write(tmp.0.join("src/greet.ts"), "export function hello(name: string) { return `Helo ${name}`; }\n").unwrap();
    git(&tmp.0, &["add", "."]);
    git(&tmp.0, &["commit", "-q", "-m", "seed existing repo"]);

    // With `yes: false`, the Direct Fast-Path synthesizes the plan with 0 planner calls
    // and still pauses at the approval gate.
    let world = World::new(Mode::Write);
    let paused = run(prompt_config(&tmp.0, "Fix the typo in src/greet.ts", false), &world).await.unwrap();
    assert!(matches!(paused, Outcome::NeedsApproval { .. }), "{paused:?}");
    assert!(planner_calls(&world).is_empty(), "direct fast-path must make 0 planner calls");

    // Resuming with `yes: true` runs 1 Code-Mode worker with the pre-computed repo map in
    // its shared prefix, runs deterministic checks, and merges cleanly.
    let again = World::new(Mode::Write);
    let outcome = run(prompt_config(&tmp.0, "Fix the typo in src/greet.ts", true), &again).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(planner_calls(&again).is_empty());
    let calls = again.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "expected exactly 1 worker call, got: {calls:?}");
    assert!(calls[0].contains("Repository map"), "shared prefix should include repo map: {}", calls[0]);
    assert!(calls[0].contains("src/greet.ts"), "repo map should list src/greet.ts: {}", calls[0]);
    assert!(calls[0].contains("Build unit `edit`"), "worker should run the direct edit unit: {}", calls[0]);
}

#[tokio::test]
async fn agent_mode_builds_with_one_agent_and_code_mode_always_plans() {
    let tmp = repo("agent-mode");
    std::fs::create_dir_all(tmp.0.join("src")).unwrap();
    std::fs::write(tmp.0.join("src/greet.ts"), "export function hello(name: string) { return `Helo ${name}`; }\n").unwrap();
    git(&tmp.0, &["add", "."]);
    git(&tmp.0, &["commit", "-q", "-m", "seed existing repo"]);

    // Agent mode: no planning call even for a request the fast path would not take.
    let world = World::new(Mode::Write);
    let mut cfg = prompt_config(&tmp.0, "Add a settings panel and wire it to the header and the footer", true);
    cfg.mode = crate::RunMode::Agent;
    let outcome = run(cfg, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });
    assert!(planner_calls(&world).is_empty(), "agent mode makes no planning call");
    let calls = world.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "one agent: {calls:?}");
    assert!(calls[0].contains("only agent") && calls[0].contains("live preview"), "{}", calls[0]);

    // Code mode: a scoped edit the fast path would take still goes to the master.
    let world = World::new(Mode::Write);
    let mut cfg = prompt_config(&tmp.0, "Fix the typo in src/greet.ts", true);
    cfg.run_id = "code-mode".into();
    cfg.mode = crate::RunMode::Code;
    let _ = run(cfg, &world).await;
    assert!(!planner_calls(&world).is_empty(), "code mode always plans");
}

#[tokio::test]
async fn force_cancel_aborts_a_blocked_worker_immediately_and_cleans_up() {
    let tmp = seeded("force-cancel");
    let barrier = Arc::new(Barrier::new(2));
    let world = World::new(Mode::Parallel(Some(barrier)));
    let (tx, rx) = watch::channel(crate::CancelMode::None);
    let mut cfg = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1);
    cfg.cancel_mode = Some(rx);

    let watcher_world = world.clone();
    tokio::spawn(async move {
        for _ in 0..200 {
            if watcher_world.inflight.load(Ordering::SeqCst) >= 1 {
                let _ = tx.send(crate::CancelMode::Force);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    let outcome = tokio::time::timeout(Duration::from_secs(10), drive(cfg, world.harnesses()))
        .await
        .expect("force cancel must abort blocked worker promptly")
        .unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: false });
    assert!(read(&run_directory(&tmp.0, "current").join("events.jsonl")).contains("cancelled"));
    assert!(!tmp.0.join("app/a/page.ts").exists());
    assert!(!tmp.0.join("app/b/page.ts").exists());
}

#[tokio::test]
async fn max_cost_usd_stops_the_run_cleanly_once_budget_is_reached() {
    let tmp = seeded("budget-cap");
    let world = World::new(Mode::Write);
    // Each Fake call costs $0.01; capping at $0.01 stops before starting page `b`.
    let mut cfg = config(&tmp.0, plan(&[("a", &[]), ("b", &[])]), true, 1);
    cfg.max_cost_usd = Some(0.01);
    let outcome = run(cfg, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: false });
    assert!(read(&run_directory(&tmp.0, "current").join("events.jsonl")).contains("cancelled"));
    let calls = world.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.contains("Build page `a`")), "{calls:?}");
    assert!(!calls.iter().any(|c| c.contains("Build page `b`")), "budget cap must prevent building page b: {calls:?}");
    assert!(git_out(&tmp.0, &["show", "taskmaster/current:app/a/page.ts"]).contains("\"a\""));
}

#[tokio::test]
async fn planner_research_writes_project_md_and_keeps_worker_shared_prefix_byte_identical() {
    let tmp = seeded("research-prefix");
    let world = World::new(Mode::Write);
    let mut p = plan(&[("a", &[]), ("b", &[])]);
    p.research = vec![tm_core::ResearchNote {
        decision: "Use React 19 useActionState for form mutations".into(),
        source: "https://react.dev/reference/react/useActionState".into(),
        version: "react@19".into(),
    }];
    let cfg = config(&tmp.0, p, true, 2);
    let outcome = run(cfg, &world).await.unwrap();
    assert_eq!(outcome, Outcome::Finished { ok: true });

    let project_md = read(&tmp.0.join("PROJECT.md"));
    assert!(project_md.contains("## Research"), "{project_md}");
    assert!(project_md.contains("https://react.dev/reference/react/useActionState"), "{project_md}");
    assert!(project_md.contains("react@19"), "{project_md}");

    let calls = world.calls.lock().unwrap().clone();
    let worker_a = calls.iter().find(|c| c.contains("Build page `a`")).expect("worker a brief");
    let worker_b = calls.iter().find(|c| c.contains("Build page `b`")).expect("worker b brief");
    let prefix_a = worker_a.split("--- Your task ---").next().unwrap();
    let prefix_b = worker_b.split("--- Your task ---").next().unwrap();
    assert_eq!(prefix_a, prefix_b, "shared prefix must be byte-identical across workers");
    assert!(prefix_a.contains("## Research"), "workers inherit research decisions via PROJECT.md");
    assert!(!prefix_a.contains("<untrusted_web_content"), "raw fetched web frames must never enter shared prefix");
}

#[tokio::test]
async fn web_access_api_persists_settings_vaults_api_key_and_serves_run_web_activity() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn call(addr: &str, token: Option<&str>, method: &str, path: &str, body: Option<serde_json::Value>) -> (u16, serde_json::Value) {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let body = body.map(|v| v.to_string()).unwrap_or_default();
        let auth = token.map(|t| format!("authorization: Bearer {t}\r\n")).unwrap_or_default();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{auth}\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        let (head, rest) = reply.split_once("\r\n\r\n").unwrap_or(("", &reply));
        let status: u16 = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
        (status, serde_json::from_str(rest).unwrap_or(serde_json::Value::Null))
    }

    let tmp = seeded("web-api");
    let auth = Arc::new(crate::auth::PairingAuth::new(Records::files(&tmp.0)).unwrap());
    let code = auth.code().to_string();
    let options = crate::api::ServerOptions::new(&tmp.0, "127.0.0.1:0".parse().unwrap(), auth);
    let server = crate::api::Server::serve(options).await.unwrap();
    let addr = server.url.trim_start_matches("http://").trim_end_matches('/').to_string();

    let (st, paired) = call(&addr, None, "POST", "/api/pair", Some(serde_json::json!({ "code": code }))).await;
    assert_eq!(st, 200);
    let token = paired["token"].as_str().unwrap().to_string();

    // 1. Update web access settings with an API key (must be vaulted, never returned)
    let (st, saved) = call(
        &addr,
        Some(&token),
        "POST",
        "/api/web-access",
        Some(serde_json::json!({
            "enabled": true,
            "provider": "brave",
            "api_key": "BSA-super-secret-brave-token-12345",
            "allowlist": ["docs.rs", "react.dev"],
            "planner_max_searches": 7,
            "planner_max_fetches": 10,
            "roles": {
                "planner": { "search": true, "fetch": true },
                "worker": { "search": true, "fetch": true },
                "reviewer": { "search": false, "fetch": true },
                "verifier": { "search": false, "fetch": false }
            }
        })),
    )
    .await;
    assert_eq!(st, 200, "{saved}");
    assert_eq!(saved["web_access"]["enabled"], true);
    assert_eq!(saved["web_access"]["provider"], "brave");
    assert_eq!(saved["web_access"]["planner_max_searches"], 7);
    assert_eq!(saved["has_api_key"], true);
    assert!(!saved.to_string().contains("BSA-super-secret-brave-token-12345"));

    // 2. GET /api/settings and /api/settings/export never contain the secret key
    let (st, settings) = call(&addr, Some(&token), "GET", "/api/settings", None).await;
    assert_eq!(st, 200);
    assert_eq!(settings["web_access"]["enabled"], true);
    assert_eq!(settings["web_access"]["has_api_key"], true);
    assert!(!settings.to_string().contains("BSA-super-secret-brave-token-12345"));

    let (st, exported) = call(&addr, Some(&token), "GET", "/api/settings/export", None).await;
    assert_eq!(st, 200);
    assert!(!exported.to_string().contains("BSA-super-secret-brave-token-12345"));

    // Clean up vaulted key after test
    let (st, cleared) = call(
        &addr,
        Some(&token),
        "POST",
        "/api/web-access",
        Some(serde_json::json!({ "provider": "brave", "clear_api_key": true })),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(cleared["has_api_key"], false);

    // 3. Record web activity in live transcript and read via GET /api/runs/{id}/web
    let live = crate::live::Live::new(Arc::new(crate::store::FileStore::in_project(&tmp.0)));
    live.push(
        "run-web-ui",
        &serde_json::json!({ "id": "master", "role": "planner" }),
        "web_search",
        serde_json::json!({
            "provider": "searxng",
            "query": "rust axum router",
            "hits": 1,
            "sources": [{ "title": "axum::Router", "url": "https://docs.rs/axum", "engine": "fixture" }],
            "injection_flagged": false
        }),
    );
    live.flush("run-web-ui");
    let (st, activity) = call(&addr, Some(&token), "GET", "/api/runs/run-web-ui/web", None).await;
    assert_eq!(st, 200);
    let items = activity["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["query"], "rust axum router");

    drop(server);
}

