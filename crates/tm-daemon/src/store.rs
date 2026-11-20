//! Where the daemon keeps what it must not lose.
//!
//! [`Store`] is five operations on named records. [`FileStore`] keeps them as files under
//! `.taskmaster/` in the project, which is the only implementation today. A database is a
//! second implementation of the same trait: nothing above this file opens a path.
//!
//! [`Records`] is the typed view the rest of the daemon uses. The key layout is:
//!
//! ```text
//! runs/<id>/events.jsonl    the run. Replaying it rebuilds the state
//! runs/<id>/changes.jsonl   what was committed, and by which model
//! runs/<id>/ledger.jsonl    what every agent call cost
//! runs/<id>/inflight.json   commands started but not finished
//! runs/<id>/meta.json       settings a resumed run keeps: parallelism, branch, template
//! runs/<id>/request.json    how the run was asked for, so it can be approved later
//! runs/<id>/status.json     the outcome and the totals
//! runs/<id>/merged/<page>   a page merged but not yet recorded in the event log
//! errorography.json         recurring mistakes, per model
//! citations.json            the Dictionary hashes each page was built against
//! evals/<id>.json           Taskmaster against one model
//! reviews/jobs/<id>.json    reviews started through the API
//! devices.json              hashes of the tokens issued to paired devices
//! ```

use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_core::{Book, ChangeEntry, Citations, Command, Event, LedgerEntry, Plan};

use crate::RunRequest;

pub trait Store: Send + Sync {
    /// `None` when nothing is stored under `key`.
    fn read(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>>;
    /// Replace the record. A reader sees the old bytes or the new ones, never a mix.
    fn write(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()>;
    /// Add one line to a record that is a list of lines.
    fn append(&self, key: &str, line: &str) -> anyhow::Result<()>;
    /// Removing a record that does not exist is not an error.
    fn remove(&self, key: &str) -> anyhow::Result<()>;
    /// The names directly under `prefix`, sorted.
    fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>>;
}

/// Records as files under one directory.
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FileStore { root: root.into() }
    }

    /// The store every project has: `.taskmaster/` inside it.
    pub fn in_project(project: &Path) -> Self {
        FileStore::new(project.join(".taskmaster"))
    }

    fn path(&self, key: &str) -> anyhow::Result<PathBuf> {
        let rel = Path::new(key);
        let safe = !key.is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_)));
        if !safe {
            bail!("`{key}` is not a record name");
        }
        Ok(self.root.join(rel))
    }
}

impl Store for FileStore {
    fn read(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let path = self.path(key)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    fn write(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    fn append(&self, key: &str, line: &str) -> anyhow::Result<()> {
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // One write call, so a line is not interleaved with another writer's line.
        file.write_all(format!("{}\n", line.trim_end_matches('\n')).as_bytes())?;
        Ok(())
    }

    fn remove(&self, key: &str) -> anyhow::Result<()> {
        let path = self.path(key)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot remove {}", path.display())),
        }
    }

    fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let dir = self.path(prefix)?;
        let Ok(read) = fs::read_dir(&dir) else { return Ok(Vec::new()) };
        let mut names: Vec<String> =
            read.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        Ok(names)
    }
}

/// Settings fixed when a run starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub parallel: usize,
    /// The branch the run builds on. `None` builds in the project's own working tree,
    /// which is what a repository with no commit yet has to do.
    #[serde(default)]
    pub branch: Option<String>,
    /// The commit the branch started from.
    #[serde(default)]
    pub base: Option<String>,
    /// The template whose files are written when the contracts are locked.
    #[serde(default)]
    pub template: Option<String>,
    /// Every tier was the same harness and model when the run started: no escalation
    /// ladder (see `Run::focused`).
    #[serde(default)]
    pub focused: bool,
}

/// One go at building or repairing a page, kept so the next attempt knows what was tried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    pub at: String,
    pub tier: tm_core::Tier,
    /// The harness and model, such as `agy gemini-flash`.
    pub model: String,
    /// `merged`, `discarded`, `flagged` or `blocked`.
    pub outcome: String,
    /// The agent that made it (`unit:a/part` for a part of a split unit). Older records have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Seconds the attempt ran, and its fresh tokens.
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub tokens: u64,
    /// What it was asked to fix, if this was a repair.
    #[serde(default)]
    pub fixing: Option<String>,
    /// Why it was discarded.
    #[serde(default)]
    pub failure: Option<String>,
    pub files: Vec<String>,
    /// The agent's own last words, shortened.
    pub said: String,
    /// The change itself, when it was discarded and so is not in the tree.
    #[serde(default)]
    pub diff: Option<String>,
}

/// The daemon's records, by name and type.
#[derive(Clone)]
pub struct Records {
    store: Arc<dyn Store>,
}

impl Records {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Records { store }
    }

    pub fn files(project: &Path) -> Self {
        Records::new(Arc::new(FileStore::in_project(project)))
    }

    // One run.

    pub fn run_ids(&self) -> anyhow::Result<Vec<String>> {
        self.store.list("runs")
    }

    /// The agents' transcript of a run, one JSON value per line.
    pub fn transcript(&self, run: &str) -> anyhow::Result<Vec<Value>> {
        self.lines(&format!("runs/{run}/agents.jsonl"))
    }

    pub fn events(&self, run: &str) -> anyhow::Result<Vec<Event>> {
        self.lines(&format!("runs/{run}/events.jsonl"))
    }

    pub fn save_events(&self, run: &str, events: &[Event]) -> anyhow::Result<()> {
        self.save_lines(&format!("runs/{run}/events.jsonl"), events)
    }

    pub fn changes(&self, run: &str) -> anyhow::Result<Vec<ChangeEntry>> {
        self.lines(&format!("runs/{run}/changes.jsonl"))
    }

    pub fn save_changes(&self, run: &str, changes: &[ChangeEntry]) -> anyhow::Result<()> {
        self.save_lines(&format!("runs/{run}/changes.jsonl"), changes)
    }

    /// `None` when the run stopped cleanly or never started a command.
    pub fn inflight(&self, run: &str) -> anyhow::Result<Option<Vec<Command>>> {
        let key = format!("runs/{run}/inflight.json");
        let Some(bytes) = self.store.read(&key)? else { return Ok(None) };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("cannot read {key}"))?))
    }

    pub fn save_inflight(&self, run: &str, commands: &[Command]) -> anyhow::Result<()> {
        let key = format!("runs/{run}/inflight.json");
        if commands.is_empty() {
            return self.store.remove(&key);
        }
        self.save(&key, &commands)
    }

    pub fn meta(&self, run: &str) -> anyhow::Result<Option<Meta>> {
        self.load(&format!("runs/{run}/meta.json"))
    }

    pub fn save_meta(&self, run: &str, meta: &Meta) -> anyhow::Result<()> {
        self.save(&format!("runs/{run}/meta.json"), meta)
    }

    pub fn request(&self, run: &str) -> anyhow::Result<Option<RunRequest>> {
        self.load(&format!("runs/{run}/request.json"))
    }

    pub fn save_request(&self, run: &str, request: &RunRequest) -> anyhow::Result<()> {
        self.save(&format!("runs/{run}/request.json"), request)
    }

    pub fn status(&self, run: &str) -> anyhow::Result<Option<Value>> {
        self.load(&format!("runs/{run}/status.json"))
    }

    pub fn save_status(&self, run: &str, status: &Value) -> anyhow::Result<()> {
        self.save(&format!("runs/{run}/status.json"), status)
    }

    pub fn save_plan(&self, run: &str, plan: &Plan) -> anyhow::Result<()> {
        self.save(&format!("runs/{run}/plan.json"), plan)
    }

    /// A damaged line is skipped: losing one price must not stop a run from resuming.
    pub fn ledger(&self, run: &str) -> anyhow::Result<Vec<LedgerEntry>> {
        let Some(bytes) = self.store.read(&format!("runs/{run}/ledger.jsonl"))? else { return Ok(Vec::new()) };
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
    }

    pub fn charge(&self, run: &str, entry: &LedgerEntry) -> anyhow::Result<()> {
        self.store.append(&format!("runs/{run}/ledger.jsonl"), &serde_json::to_string(entry)?)
    }

    pub fn merged(&self, run: &str, page: &str) -> bool {
        matches!(self.store.read(&format!("runs/{run}/merged/{page}")), Ok(Some(_)))
    }

    pub fn mark_merged(&self, run: &str, page: &str, commit: &str) -> anyhow::Result<()> {
        self.store.write(&format!("runs/{run}/merged/{page}"), commit.as_bytes())
    }

    pub fn clear_merged(&self, run: &str, page: &str) {
        let _ = self.store.remove(&format!("runs/{run}/merged/{page}"));
    }

    /// Every attempt at a page so far, oldest first.
    pub fn attempts(&self, run: &str, page: &str) -> anyhow::Result<Vec<Attempt>> {
        let Some(bytes) = self.store.read(&format!("runs/{run}/attempts/{page}.jsonl"))? else { return Ok(Vec::new()) };
        Ok(String::from_utf8_lossy(&bytes).lines().filter_map(|line| serde_json::from_str(line).ok()).collect())
    }

    pub fn note_attempt(&self, run: &str, page: &str, attempt: &Attempt) -> anyhow::Result<()> {
        self.store.append(&format!("runs/{run}/attempts/{page}.jsonl"), &serde_json::to_string(attempt)?)
    }

    /// What a page looked like in the browser, as PNG.
    pub fn screenshot(&self, run: &str, page: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.store.read(&format!("runs/{run}/screens/{page}.png"))
    }

    pub fn save_screenshot(&self, run: &str, page: &str, png: &[u8]) -> anyhow::Result<()> {
        self.store.write(&format!("runs/{run}/screens/{page}.png"), png)
    }

    /// The pages that have a screenshot.
    pub fn screenshots(&self, run: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .store
            .list(&format!("runs/{run}/screens"))?
            .into_iter()
            .filter_map(|name| name.strip_suffix(".png").map(str::to_string))
            .collect())
    }

    // The project.

    pub fn book(&self) -> anyhow::Result<Book> {
        Ok(self.load("errorography.json")?.unwrap_or_default())
    }

    pub fn save_book(&self, book: &Book) -> anyhow::Result<()> {
        self.save("errorography.json", book)
    }

    pub fn citations(&self) -> anyhow::Result<Citations> {
        Ok(self.load("citations.json")?.unwrap_or_default())
    }

    pub fn save_citations(&self, citations: &Citations) -> anyhow::Result<()> {
        self.save("citations.json", citations)
    }

    pub fn eval(&self, id: &str) -> anyhow::Result<Option<Value>> {
        self.load(&format!("evals/{}.json", flat(id)))
    }

    pub fn save_eval(&self, id: &str, report: &impl Serialize) -> anyhow::Result<()> {
        self.save(&format!("evals/{}.json", flat(id)), report)
    }

    pub fn review_ids(&self) -> anyhow::Result<Vec<String>> {
        let names = self.store.list("reviews/jobs")?;
        Ok(names.into_iter().filter_map(|name| name.strip_suffix(".json").map(str::to_string)).collect())
    }

    pub fn review(&self, id: &str) -> anyhow::Result<Option<Value>> {
        self.load(&format!("reviews/jobs/{}.json", flat(id)))
    }

    pub fn save_review(&self, id: &str, job: &Value) -> anyhow::Result<()> {
        self.save(&format!("reviews/jobs/{}.json", flat(id)), job)
    }

    pub fn devices(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.load("devices.json")?.unwrap_or_default())
    }

    pub fn save_devices(&self, hashes: &[String]) -> anyhow::Result<()> {
        self.save("devices.json", &hashes)
    }

    fn load<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        let Some(bytes) = self.store.read(key)? else { return Ok(None) };
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&bytes).with_context(|| format!("cannot read {key}"))?))
    }

    fn save(&self, key: &str, value: &impl Serialize) -> anyhow::Result<()> {
        self.store.write(key, &serde_json::to_vec_pretty(value)?)
    }

    fn lines<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Vec<T>> {
        let Some(bytes) = self.store.read(key)? else { return Ok(Vec::new()) };
        let text = String::from_utf8_lossy(&bytes);
        let mut out = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            out.push(serde_json::from_str(line).with_context(|| format!("{key}:{}", index + 1))?);
        }
        Ok(out)
    }

    fn save_lines<T: Serialize>(&self, key: &str, items: &[T]) -> anyhow::Result<()> {
        let mut body = String::new();
        for item in items {
            body.push_str(&serde_json::to_string(item)?);
            body.push('\n');
        }
        self.store.write(key, body.as_bytes())
    }
}

fn flat(id: &str) -> String {
    id.replace(['/', '\\'], "_")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A second implementation, to show nothing above the trait depends on files.
    #[derive(Default)]
    pub(crate) struct Memory(pub(crate) Mutex<BTreeMap<String, Vec<u8>>>);

    impl Store for Memory {
        fn read(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
        fn write(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
            self.0.lock().unwrap().insert(key.into(), bytes.to_vec());
            Ok(())
        }
        fn append(&self, key: &str, line: &str) -> anyhow::Result<()> {
            let mut map = self.0.lock().unwrap();
            let body = map.entry(key.into()).or_default();
            body.extend_from_slice(line.as_bytes());
            body.push(b'\n');
            Ok(())
        }
        fn remove(&self, key: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
        fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
            let start = format!("{prefix}/");
            let mut names: Vec<String> = self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter_map(|key| key.strip_prefix(&start))
                .map(|rest| rest.split('/').next().unwrap_or(rest).to_string())
                .collect();
            names.dedup();
            Ok(names)
        }
    }

    pub(crate) fn exercise(records: &Records) {
        assert!(records.events("r1").unwrap().is_empty());
        assert!(records.inflight("r1").unwrap().is_none());
        records.save_events("r1", &[Event::PlanApproved, Event::Cancelled]).unwrap();
        records.save_inflight("r1", &[Command::RunVerify]).unwrap();
        records.save_meta("r1", &Meta { parallel: 3, branch: None, base: None, template: None, focused: false }).unwrap();
        records.mark_merged("r1", "home", "abc").unwrap();
        assert_eq!(records.events("r1").unwrap(), vec![Event::PlanApproved, Event::Cancelled]);
        assert_eq!(records.inflight("r1").unwrap(), Some(vec![Command::RunVerify]));
        assert_eq!(records.meta("r1").unwrap().unwrap().parallel, 3);
        assert!(records.merged("r1", "home"));
        records.clear_merged("r1", "home");
        assert!(!records.merged("r1", "home"));
        records.save_inflight("r1", &[]).unwrap();
        assert!(records.inflight("r1").unwrap().is_none());
        assert_eq!(records.run_ids().unwrap(), vec!["r1".to_string()]);
        let mut book = Book::default();
        book.record("agy", "forgets the route param");
        records.save_book(&book).unwrap();
        assert_eq!(records.book().unwrap(), book);
    }

    #[test]
    fn files_and_memory_hold_the_same_records() {
        let dir = std::env::temp_dir().join(format!("tm-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        exercise(&Records::new(Arc::new(FileStore::new(&dir))));
        assert!(dir.join("runs/r1/events.jsonl").is_file());
        exercise(&Records::new(Arc::new(Memory::default())));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_name_cannot_leave_the_store() {
        let store = FileStore::new("/tmp/tm-store-names");
        assert!(store.read("../etc/passwd").is_err());
        assert!(store.write("/etc/passwd", b"x").is_err());
        assert!(store.read("").is_err());
    }
}
