//! What each agent says and does while a run goes, and the notes a person queued for them.
//!
//! The run's log (`events`) records decisions. This records the conversation around them:
//! an agent's text and thoughts as they arrive, every tool call with the gate's answer, the
//! commands the runtime checks ran, and what a person typed into the run's terminal. Each
//! line of the record `runs/<run>/agents.jsonl` is one entry; the API numbers them from 1.
//! It is kept in the run's store beside its log, and only appended to, so a reader in
//! another process sees the same entries.
//!
//! Notes are the record `runs/<run>/notes.json`. A note is addressed to `master`,
//! `workers`, `page:<id>`, `reviewer` or `verifier`. The master reads every undelivered
//! note before any other agent does, and the engine marks it delivered then. The person
//! can edit or remove it until then.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::store::Store;
use crate::time::rfc3339_now;

#[derive(Default)]
struct Cached {
    /// Bytes of the file already parsed.
    offset: u64,
    entries: Vec<Value>,
}

pub struct Live {
    store: Arc<dyn Store>,
    cache: Mutex<HashMap<String, Cached>>,
    write: Mutex<()>,
    calls: AtomicU64,
    wake: broadcast::Sender<String>,
    /// Risky calls waiting for a person. Shared with every agent call of every run.
    pub approvals: Arc<crate::approvals::Approvals>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub to: String,
    pub text: String,
    pub at: String,
    #[serde(default)]
    pub delivered: bool,
}

impl Live {
    pub fn new(store: Arc<dyn Store>) -> Self {
        let (wake, _) = broadcast::channel(512);
        Live {
            store,
            cache: Mutex::new(HashMap::new()),
            write: Mutex::new(()),
            calls: AtomicU64::new(0),
            wake,
            approvals: Arc::default(),
        }
    }

    /// Subscribe to immediate notifications whenever a run receives a new transcript entry.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.wake.subscribe()
    }

    fn key(run: &str, name: &str) -> String {
        format!("runs/{run}/{name}")
    }

    /// A number for one agent call, so entries from parallel calls of one agent group.
    pub fn next_call(&self) -> u64 {
        self.calls.fetch_add(1, Ordering::Relaxed) + 1 + std::process::id() as u64 * 1_000_000
    }

    /// Append one entry. A failure to write is reported and otherwise ignored: the
    /// transcript is a view, and a run must not stop because of it.
    pub fn push(&self, run: &str, agent: &Value, kind: &str, mut fields: Value) {
        if !fields.is_object() {
            fields = json!({});
        }
        crate::secrets::redact_json(&mut fields);
        let object = fields.as_object_mut().expect("an object");
        object.insert("at".into(), json!(rfc3339_now()));
        object.insert("agent".into(), agent.clone());
        object.insert("kind".into(), json!(kind));
        let line = serde_json::to_string(&fields).unwrap_or_default();
        {
            let _guard = self.write.lock().unwrap();
            if let Err(err) = self.store.append(&Self::key(run, "agents.jsonl"), &line) {
                eprintln!("taskmaster: the agent transcript of {run} was not written: {err}");
                return;
            }
        }
        let _ = self.wake.send(run.to_string());
    }

    /// Every entry after the first `after`, and how many there are in all.
    pub fn read(&self, run: &str, after: usize) -> (usize, Vec<Value>) {
        let mut cache = self.cache.lock().unwrap();
        let cached = cache.entry(run.to_string()).or_default();
        if let Ok(Some((len, fresh))) = self.store.read_from(&Self::key(run, "agents.jsonl"), cached.offset) {
            if len < cached.offset {
                *cached = Cached::default();
            }
            if !fresh.is_empty() {
                // Only whole lines: a writer may be part way through the last one.
                let whole = fresh.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
                for line in fresh[..whole].split(|b| *b == b'\n') {
                    if line.is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(line) {
                        cached.entries.push(value);
                    }
                }
                cached.offset += whole as u64;
            }
        }
        let total = cached.entries.len();
        let from = after.min(total);
        (total, cached.entries[from..].to_vec())
    }

    /// The run branch's last commit, kept when the branch is deleted after a merge, so
    /// its diff and a later pull request can still find what the run built.
    pub fn set_tip(&self, run: &str, sha: &str) -> anyhow::Result<()> {
        self.store.write(&Self::key(run, "tip"), sha.trim().as_bytes())
    }

    pub fn tip(&self, run: &str) -> Option<String> {
        let bytes = self.store.read(&Self::key(run, "tip")).ok().flatten()?;
        let sha = String::from_utf8(bytes).ok()?.trim().to_string();
        (!sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some(sha)
    }

    pub fn notes(&self, run: &str) -> Vec<Note> {
        self.store
            .read(&Self::key(run, "notes.json"))
            .ok()
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save_notes(&self, run: &str, notes: &[Note]) -> anyhow::Result<()> {
        self.store.write(&Self::key(run, "notes.json"), &serde_json::to_vec_pretty(notes)?)
    }

    /// Hold the note lock across a check-then-append, so two wakes cannot both start a run.
    pub fn write_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.write.lock().unwrap()
    }

    pub fn add_note(&self, run: &str, to: &str, text: &str) -> anyhow::Result<Note> {
        let _guard = self.write.lock().unwrap();
        let mut notes = self.notes(run);
        let note = Note {
            id: crate::secret::random_hex(4)?,
            to: to.to_string(),
            text: text.to_string(),
            at: rfc3339_now(),
            delivered: false,
        };
        notes.push(note.clone());
        self.save_notes(run, &notes)?;
        Ok(note)
    }

    /// Change or remove a note that has not been delivered. `None` removes it.
    pub fn edit_note(&self, run: &str, id: &str, text: Option<&str>) -> anyhow::Result<bool> {
        let _guard = self.write.lock().unwrap();
        let mut notes = self.notes(run);
        let Some(index) = notes.iter().position(|note| note.id == id && !note.delivered) else {
            return Ok(false);
        };
        match text {
            Some(text) => notes[index].text = text.to_string(),
            None => {
                notes.remove(index);
            }
        }
        self.save_notes(run, &notes)?;
        Ok(true)
    }

    /// Undelivered notes matching `pred`, marked delivered. A note is read once.
    pub fn take_fresh_where(&self, run: &str, pred: impl Fn(&Note) -> bool) -> Vec<Note> {
        let _guard = self.write.lock().unwrap();
        let mut notes = self.notes(run);
        let mut out = Vec::new();
        let mut changed = false;
        for note in notes.iter_mut() {
            if note.delivered || !pred(note) {
                continue;
            }
            note.delivered = true;
            changed = true;
            out.push(note.clone());
        }
        if changed {
            let _ = self.save_notes(run, &notes);
        }
        out
    }

    /// True when an undelivered note matches `pred`.
    pub fn has_pending_where(&self, run: &str, pred: impl Fn(&Note) -> bool) -> bool {
        self.notes(run).iter().any(|note| !note.delivered && pred(note))
    }

    /// The undelivered notes for `to`, marked delivered. `workers` notes are kept for
    /// every later worker as well, so they are returned whether delivered or not.
    pub fn take_notes(&self, run: &str, to: &[&str]) -> Vec<String> {
        let _guard = self.write.lock().unwrap();
        let mut notes = self.notes(run);
        let mut out = Vec::new();
        for note in notes.iter_mut() {
            if !to.contains(&note.to.as_str()) {
                continue;
            }
            if note.delivered && note.to != "workers" {
                continue;
            }
            note.delivered = true;
            out.push(note.text.clone());
        }
        if !out.is_empty() {
            let _ = self.save_notes(run, &notes);
        }
        out
    }
}

/// What a tool call is aimed at: a file, a command, a pattern or an address.
pub fn tool_target(input: &Value) -> Option<String> {
    for key in ["file_path", "path", "filePath", "notebook_path", "command", "cmd", "url", "pattern", "query", "glob"] {
        if let Some(text) = input.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    input.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileStore;
    use std::path::PathBuf;

    fn tmp(name: &str) -> Arc<dyn Store> {
        let dir: PathBuf = std::env::temp_dir().join(format!("tm-live-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(FileStore::new(dir))
    }

    #[test]
    fn entries_are_numbered_and_read_in_order_across_instances() {
        let dir = tmp("read");
        let live = Live::new(dir.clone());
        let agent = json!({ "id": "master" });
        live.push("r", &agent, "text", json!({ "text": "a" }));
        live.push("r", &agent, "thought", json!({ "text": "b" }));
        assert_eq!(live.read("r", 0).0, 2);
        live.push("r", &agent, "text", json!({ "text": "c" }));
        let (total, rest) = live.read("r", 2);
        assert_eq!((total, rest.len()), (3, 1));
        assert_eq!(rest[0]["text"], "c");
        assert_eq!(rest[0]["agent"]["id"], "master");
        // Another reader, as another process would be, sees the same entries.
        assert_eq!(Live::new(dir).read("r", 0).0, 3);
        assert_eq!(live.read("none", 0).0, 0);
    }

    #[test]
    fn a_merged_branch_tip_is_kept_and_only_a_sha_reads_back() {
        let live = Live::new(tmp("tip"));
        assert_eq!(live.tip("r"), None);
        live.set_tip("r", "797ee50aa\n").unwrap();
        assert_eq!(live.tip("r").as_deref(), Some("797ee50aa"));
        live.set_tip("r", "main; rm -rf /").unwrap();
        assert_eq!(live.tip("r"), None);
    }

    #[test]
    fn notes_are_delivered_once_except_to_every_worker() {
        let dir = tmp("notes");
        let live = Live::new(dir.clone());
        let a = live.add_note("r", "page:a", "fix the header").unwrap();
        live.add_note("r", "workers", "use tabs").unwrap();
        let gone = live.add_note("r", "master", "never mind").unwrap();
        assert!(live.edit_note("r", &gone.id, None).unwrap());
        assert!(live.edit_note("r", &a.id, Some("fix the footer")).unwrap());
        assert_eq!(live.take_notes("r", &["page:a", "workers"]), vec!["fix the footer", "use tabs"]);
        assert_eq!(live.take_notes("r", &["page:a", "workers"]), vec!["use tabs"]);
        assert!(live.take_notes("r", &["master"]).is_empty());
        // A delivered note can no longer be changed.
        assert!(!live.edit_note("r", &a.id, Some("x")).unwrap());
    }

    #[test]
    fn a_tool_call_target_is_the_file_or_the_command() {
        assert_eq!(tool_target(&json!({ "file_path": "app/a.ts", "content": "x" })).as_deref(), Some("app/a.ts"));
        assert_eq!(tool_target(&json!({ "command": "ls" })).as_deref(), Some("ls"));
        assert_eq!(tool_target(&json!({ "other": 1 })), None);
    }

    #[tokio::test]
    async fn subscribe_wakes_immediately_on_push() {
        let live = Live::new(tmp("wake"));
        let mut rx = live.subscribe();
        live.push("run-42", &json!({ "id": "master" }), "thought", json!({ "text": "planning" }));
        let woken = rx.recv().await.unwrap();
        assert_eq!(woken, "run-42");
    }

    #[test]
    fn read_from_only_reads_appended_tail_bytes() {
        let store = tmp("tail");
        store.append("a.jsonl", r#"{"n":1}"#).unwrap();
        let (len1, b1) = store.read_from("a.jsonl", 0).unwrap().unwrap();
        assert_eq!(b1, br#"{"n":1}"#.iter().copied().chain([b'\n']).collect::<Vec<_>>());
        // Caught up: zero bytes read.
        let (len_same, b_empty) = store.read_from("a.jsonl", len1).unwrap().unwrap();
        assert_eq!(len_same, len1);
        assert!(b_empty.is_empty());
        // Appending another line reads ONLY the newly appended bytes.
        store.append("a.jsonl", r#"{"n":2}"#).unwrap();
        let (len2, b2) = store.read_from("a.jsonl", len1).unwrap().unwrap();
        assert!(len2 > len1);
        assert_eq!(b2, br#"{"n":2}"#.iter().copied().chain([b'\n']).collect::<Vec<_>>());
    }
}
