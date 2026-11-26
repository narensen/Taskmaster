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
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};

use crate::store::Store;
use crate::time::rfc3339_now;

/// Maximum number of runs kept in memory in [`Live::cache`] (P4).
pub const MAX_CACHED_RUNS: usize = 8;
/// Cached runs idle longer than this are evicted on the next read (P4).
const CACHE_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
/// Per-run writer flush interval (P3).
const WRITER_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// Per-run writer byte threshold before an immediate flush (P3).
const WRITER_FLUSH_BYTES: usize = 64 * 1024;
/// How long a held split-secret carry waits with no follow-up delta before flushing.
const CARRY_IDLE_FLUSH: Duration = Duration::from_millis(250);
/// How long an idle per-run writer thread waits before exiting cleanly.
const WRITER_IDLE_EXIT: Duration = Duration::from_secs(5);

/// Per-run monotonic sequence counters for `events.jsonl` and `agents.jsonl` (P1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunSeq {
    pub events: u64,
    pub agents: u64,
}

struct Cached {
    /// Bytes of the file already parsed.
    offset: u64,
    entries: Vec<Value>,
    last_used: Instant,
    use_seq: u64,
}

impl Default for Cached {
    fn default() -> Self {
        Self {
            offset: 0,
            entries: Vec::new(),
            last_used: Instant::now(),
            use_seq: 0,
        }
    }
}

enum WriterCmd {
    Entry {
        seq: u64,
        agent: Value,
        kind: String,
        fields: Value,
    },
    Flush {
        ack: mpsc::SyncSender<()>,
    },
}

struct WriterHandle {
    tx: mpsc::Sender<WriterCmd>,
    enqueued: Arc<AtomicU64>,
    flushed: Arc<AtomicU64>,
}

struct CarryEntry {
    agent: Value,
    text: String,
    at: Instant,
}

struct SignalHub {
    signals: Mutex<HashMap<String, watch::Sender<RunSeq>>>,
    wake: broadcast::Sender<String>,
}

impl SignalHub {
    fn new() -> Self {
        let (wake, _) = broadcast::channel(512);
        Self {
            signals: Mutex::new(HashMap::new()),
            wake,
        }
    }

    fn watch(&self, run: &str) -> watch::Receiver<RunSeq> {
        let mut map = self.signals.lock().unwrap();
        let tx = map
            .entry(run.to_string())
            .or_insert_with(|| watch::channel(RunSeq::default()).0);
        tx.subscribe()
    }

    fn notify_agents(&self, run: &str) {
        {
            let mut map = self.signals.lock().unwrap();
            let tx = map
                .entry(run.to_string())
                .or_insert_with(|| watch::channel(RunSeq::default()).0);
            tx.send_modify(|seq| seq.agents = seq.agents.saturating_add(1));
        }
        let _ = self.wake.send(run.to_string());
    }

    fn notify_events(&self, run: &str) {
        let mut map = self.signals.lock().unwrap();
        let tx = map
            .entry(run.to_string())
            .or_insert_with(|| watch::channel(RunSeq::default()).0);
        tx.send_modify(|seq| seq.events = seq.events.saturating_add(1));
    }
}

pub struct Live {
    store: Arc<dyn Store>,
    cache: Mutex<HashMap<String, Cached>>,
    cache_seq: AtomicU64,
    /// Serializes note read-modify-writes and turn wakeups without ever blocking transcript pushes.
    notes_lock: Mutex<()>,
    writers: Mutex<HashMap<String, WriterHandle>>,
    calls: AtomicU64,
    hub: Arc<SignalHub>,
    cmd_output: CommandOutputHub,
    /// Risky calls waiting for a person. Shared with every agent call of every run.
    pub approvals: Arc<crate::approvals::Approvals>,
    /// Provider rate-limit / usage window tracker populated from CLI reports (O4).
    pub usage: crate::usage::UsageTracker,
    /// Where an `approval_requested` entry is also sent, for a notification to the person.
    alerts: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<(String, Value)>>,
}

/// Cap on live per-row command output before switching from append frames to tail replace (P5).
pub const CMD_OUTPUT_CAP: usize = 16 * 1024;
const MAX_CMD_BUFFERS: usize = 64;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CommandOutputFrame {
    Append {
        chunk: String,
        next_offset: usize,
        bytes: usize,
    },
    Replace {
        text: String,
        next_offset: usize,
        bytes: usize,
        truncated: bool,
    },
}

struct CmdOutputBuf {
    text: String,
    total_bytes: usize,
    truncated: bool,
    seq: u64,
    tx: watch::Sender<u64>,
    last_used: Instant,
}

#[derive(Default)]
struct CommandOutputHub {
    channels: Mutex<HashMap<(String, String), CmdOutputBuf>>,
}

impl CommandOutputHub {
    fn push(&self, run: &str, tool_id: &str, chunk: &str) {
        if chunk.is_empty() || tool_id.is_empty() {
            return;
        }
        let mut map = self.channels.lock().unwrap();
        if map.len() >= MAX_CMD_BUFFERS
            && !map.contains_key(&(run.to_string(), tool_id.to_string()))
            && let Some(oldest) = map.iter().min_by_key(|(_, b)| b.last_used).map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        let entry = map.entry((run.to_string(), tool_id.to_string())).or_insert_with(|| {
            let (tx, _) = watch::channel(0u64);
            CmdOutputBuf {
                text: String::new(),
                total_bytes: 0,
                truncated: false,
                seq: 0,
                tx,
                last_used: Instant::now(),
            }
        });
        entry.text.push_str(chunk);
        entry.total_bytes = entry.total_bytes.saturating_add(chunk.len());
        if entry.text.len() > CMD_OUTPUT_CAP {
            let mut cut = entry.text.len() - CMD_OUTPUT_CAP;
            while cut < entry.text.len() && !entry.text.is_char_boundary(cut) {
                cut += 1;
            }
            entry.text.drain(..cut);
            entry.truncated = true;
        }
        entry.seq = entry.seq.saturating_add(1);
        entry.last_used = Instant::now();
        let seq = entry.seq;
        let _ = entry.tx.send(seq);
    }

    fn subscribe(&self, run: &str, tool_id: &str) -> watch::Receiver<u64> {
        let mut map = self.channels.lock().unwrap();
        let entry = map.entry((run.to_string(), tool_id.to_string())).or_insert_with(|| {
            let (tx, _) = watch::channel(0u64);
            CmdOutputBuf {
                text: String::new(),
                total_bytes: 0,
                truncated: false,
                seq: 0,
                tx,
                last_used: Instant::now(),
            }
        });
        entry.last_used = Instant::now();
        entry.tx.subscribe()
    }

    fn frame(&self, run: &str, tool_id: &str, sent_offset: usize, sent_seq: u64) -> Option<(u64, CommandOutputFrame)> {
        let map = self.channels.lock().unwrap();
        let buf = map.get(&(run.to_string(), tool_id.to_string()))?;
        if buf.seq == sent_seq && sent_offset >= buf.text.len() {
            return None;
        }
        if !buf.truncated && sent_offset <= buf.text.len() && buf.text.is_char_boundary(sent_offset) {
            let chunk = buf.text[sent_offset..].to_string();
            if chunk.is_empty() {
                return None;
            }
            Some((
                buf.seq,
                CommandOutputFrame::Append {
                    chunk,
                    next_offset: buf.text.len(),
                    bytes: buf.total_bytes,
                },
            ))
        } else {
            Some((
                buf.seq,
                CommandOutputFrame::Replace {
                    text: buf.text.clone(),
                    next_offset: buf.text.len(),
                    bytes: buf.total_bytes,
                    truncated: buf.truncated,
                },
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub to: String,
    pub text: String,
    pub at: String,
    #[serde(default)]
    pub delivered: bool,
    /// Who should take it when it starts a new turn on a finished run: `agent`, `code` or
    /// `auto`. `None` follows the run's own mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// True when the note should wait until the active run settles and open the next turn
    /// together with other queued notes instead of steering the running master (A1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
}

impl Live {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Live {
            store,
            cache: Mutex::new(HashMap::new()),
            cache_seq: AtomicU64::new(0),
            notes_lock: Mutex::new(()),
            writers: Mutex::new(HashMap::new()),
            calls: AtomicU64::new(0),
            hub: Arc::new(SignalHub::new()),
            cmd_output: CommandOutputHub::default(),
            approvals: Arc::default(),
            usage: crate::usage::UsageTracker::new(),
            alerts: std::sync::OnceLock::new(),
        }
    }

    /// Send every question put to a person to `tx` as well (run, question), for notifications.
    pub fn set_alerts(&self, tx: tokio::sync::mpsc::UnboundedSender<(String, Value)>) {
        let _ = self.alerts.set(tx);
    }

    /// Subscribe to per-run sequence notifications for `run` only (P1).
    pub fn watch(&self, run: &str) -> watch::Receiver<RunSeq> {
        self.hub.watch(run)
    }

    /// Notify watchers of `run` that `events.jsonl` or active state changed.
    pub fn notify_events(&self, run: &str) {
        self.hub.notify_events(run);
    }

    /// Notify watchers of `run` that `agents.jsonl` changed.
    pub fn notify_agents(&self, run: &str) {
        self.hub.notify_agents(run);
    }

    /// Subscribe to immediate notifications whenever a run receives a new transcript entry.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.hub.wake.subscribe()
    }

    /// How many runs are currently held in the transcript cache.
    pub fn cached_runs(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    fn key(run: &str, name: &str) -> String {
        format!("runs/{run}/{name}")
    }

    /// A number for one agent call, so entries from parallel calls of one agent group.
    pub fn next_call(&self) -> u64 {
        self.calls.fetch_add(1, Ordering::Relaxed) + 1 + std::process::id() as u64 * 1_000_000
    }

    fn spawn_writer(&self, run: &str) -> WriterHandle {
        let (tx, rx) = mpsc::channel::<WriterCmd>();
        let enqueued = Arc::new(AtomicU64::new(0));
        let flushed = Arc::new(AtomicU64::new(0));
        let store = self.store.clone();
        let hub = self.hub.clone();
        let run_id = run.to_string();
        let flushed_bg = flushed.clone();
        let _ = std::thread::Builder::new()
            .name(format!("tm-live-{}", &run_id[..run_id.len().min(16)]))
            .spawn(move || run_writer_loop(run_id, store, hub, rx, flushed_bg));
        WriterHandle { tx, enqueued, flushed }
    }

    /// Stream a live command output chunk into the per-row hub (P5) without writing it to the main SSE stream.
    pub fn push_command_output(&self, run: &str, tool_id: &str, chunk: &str) {
        self.cmd_output.push(run, tool_id, chunk);
    }

    pub fn subscribe_command_output(&self, run: &str, tool_id: &str) -> watch::Receiver<u64> {
        self.cmd_output.subscribe(run, tool_id)
    }

    pub fn command_output_frame(
        &self,
        run: &str,
        tool_id: &str,
        sent_offset: usize,
        sent_seq: u64,
    ) -> Option<(u64, CommandOutputFrame)> {
        self.cmd_output.frame(run, tool_id, sent_offset, sent_seq)
    }

    /// Append one entry via this run's buffered writer. Never blocks the caller on disk I/O
    /// and never acquires the notes lock.
    pub fn push(&self, run: &str, agent: &Value, kind: &str, fields: Value) {
        if kind == "approval_requested"
            && let Some(tx) = self.alerts.get()
        {
            let _ = tx.send((run.to_string(), fields.clone()));
        }
        if kind == "output"
            && let (Some(tool_id), Some(text)) = (fields["id"].as_str(), fields["text"].as_str())
            && !tool_id.is_empty()
            && !text.is_empty()
        {
            self.cmd_output.push(run, tool_id, text);
        }
        let mut writers = self.writers.lock().unwrap();
        for _ in 0..2 {
            if !writers.contains_key(run) {
                let handle = self.spawn_writer(run);
                writers.insert(run.to_string(), handle);
            }
            let handle = writers.get(run).expect("just inserted");
            let seq = handle.enqueued.fetch_add(1, Ordering::SeqCst) + 1;
            let cmd = WriterCmd::Entry {
                seq,
                agent: agent.clone(),
                kind: kind.to_string(),
                fields: fields.clone(),
            };
            if handle.tx.send(cmd).is_ok() {
                return;
            }
            // The idle writer exited; respawn once and resend.
            writers.remove(run);
        }
    }

    /// [`Live::push`] for a caller that already holds [`Live::write_lock`].
    pub fn push_held(&self, _held: &std::sync::MutexGuard<'_, ()>, run: &str, agent: &Value, kind: &str, fields: Value) {
        self.push(run, agent, kind, fields);
    }

    /// Ensure all entries pushed to `run` so far have been flushed to the store.
    pub fn flush(&self, run: &str) {
        let tx = {
            let writers = self.writers.lock().unwrap();
            let Some(handle) = writers.get(run) else {
                return;
            };
            if handle.enqueued.load(Ordering::SeqCst) <= handle.flushed.load(Ordering::SeqCst) {
                return;
            }
            handle.tx.clone()
        };
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if tx.send(WriterCmd::Flush { ack: ack_tx }).is_ok() {
            let _ = ack_rx.recv();
        }
    }

    /// End every agent call of `run` that started and has no `end`, marked stopped. An agent
    /// aborted by a cancel, a crash or a stalled driver never writes its own, and a reader
    /// would otherwise show it working forever. Returns how many calls it closed.
    pub fn close_open_calls(&self, run: &str, why: &str) -> usize {
        let (_, entries) = self.read(run, 0);
        let mut open: Vec<(String, Value)> = Vec::new();
        for entry in &entries {
            let agent = &entry["agent"];
            let id = agent["id"].as_str().unwrap_or("");
            // Only real calls carry a call number; run-level lines (cancelling, budget) do not.
            let Some(call) = agent["call"].as_u64() else { continue };
            if id.is_empty() || id == "runtime" || id == "person" {
                continue;
            }
            let key = format!("{id}#{call}");
            if entry["kind"] == "end" {
                open.retain(|(k, _)| *k != key);
            } else if !open.iter().any(|(k, _)| *k == key) {
                open.push((key, agent.clone()));
            }
        }
        for (_, agent) in &open {
            self.push(run, agent, "end", json!({ "ok": false, "stopped": true, "error": why }));
        }
        if !open.is_empty() {
            self.flush(run);
        }
        open.len()
    }

    /// Every entry after the first `after`, and how many there are in all.
    pub fn read(&self, run: &str, after: usize) -> (usize, Vec<Value>) {
        self.flush(run);
        let now = Instant::now();
        let seq = self.cache_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut cache = self.cache.lock().unwrap();
        let cached = cache.entry(run.to_string()).or_default();
        cached.last_used = now;
        cached.use_seq = seq;
        if let Ok(Some((len, fresh))) = self.store.read_from(&Self::key(run, "agents.jsonl"), cached.offset) {
            if len < cached.offset {
                *cached = Cached {
                    offset: 0,
                    entries: Vec::new(),
                    last_used: now,
                    use_seq: seq,
                };
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
        let result = cached.entries[from..].to_vec();

        // Evict stale and least-recently-used runs (P4).
        cache.retain(|k, entry| k == run || now.saturating_duration_since(entry.last_used) <= CACHE_IDLE_TTL);
        while cache.len() > MAX_CACHED_RUNS {
            let victim = cache
                .iter()
                .filter(|(k, _)| k.as_str() != run)
                .min_by_key(|(_, entry)| entry.use_seq)
                .map(|(k, _)| k.clone());
            match victim {
                Some(key) => {
                    cache.remove(&key);
                }
                None => break,
            }
        }

        (total, result)
    }

    /// Read one transcript entry by index without cloning the rest of the transcript (P5).
    pub fn read_at(&self, run: &str, index: usize) -> Option<Value> {
        let _ = self.read(run, usize::MAX);
        let cache = self.cache.lock().unwrap();
        cache.get(run).and_then(|c| c.entries.get(index).cloned())
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
        self.notes_lock.lock().unwrap()
    }

    pub fn add_note(&self, run: &str, to: &str, text: &str) -> anyhow::Result<Note> {
        self.add_note_as(run, to, text, None)
    }

    /// [`Live::add_note`] with the mode a new turn should run in (see [`Note::mode`]).
    pub fn add_note_as(&self, run: &str, to: &str, text: &str, mode: Option<&str>) -> anyhow::Result<Note> {
        self.add_note_full(run, to, text, mode, false)
    }

    /// [`Live::add_note`] with an optional turn mode and `queued` flag (A1).
    pub fn add_note_full(
        &self,
        run: &str,
        to: &str,
        text: &str,
        mode: Option<&str>,
        queued: bool,
    ) -> anyhow::Result<Note> {
        let _guard = self.notes_lock.lock().unwrap();
        let mut notes = self.notes(run);
        let note = Note {
            id: crate::secret::random_hex(4)?,
            to: to.to_string(),
            text: text.to_string(),
            at: rfc3339_now(),
            delivered: false,
            mode: mode.map(str::trim).filter(|m| !m.is_empty()).map(str::to_string),
            queued,
        };
        notes.push(note.clone());
        self.save_notes(run, &notes)?;
        Ok(note)
    }

    /// Change or remove a note that has not been delivered. `None` removes it.
    pub fn edit_note(&self, run: &str, id: &str, text: Option<&str>) -> anyhow::Result<bool> {
        self.edit_note_full(run, id, text, None)
    }

    /// Change text and/or `queued` flag on an undelivered note; if both are `None`, removes it.
    pub fn edit_note_full(
        &self,
        run: &str,
        id: &str,
        text: Option<&str>,
        queued: Option<bool>,
    ) -> anyhow::Result<bool> {
        let _guard = self.notes_lock.lock().unwrap();
        let mut notes = self.notes(run);
        let Some(index) = notes.iter().position(|note| note.id == id && !note.delivered) else {
            return Ok(false);
        };
        if text.is_none() && queued.is_none() {
            notes.remove(index);
        } else {
            if let Some(text) = text {
                notes[index].text = text.to_string();
            }
            if let Some(q) = queued {
                notes[index].queued = q;
            }
        }
        self.save_notes(run, &notes)?;
        Ok(true)
    }

    /// Promote all undelivered queued notes into active notes so a newly opened FollowUp turn reads them together.
    pub fn unqueue_all(&self, run: &str) {
        let guard = self.notes_lock.lock().unwrap();
        self.unqueue_all_held(&guard, run);
    }

    /// [`Live::unqueue_all`] when the caller already holds [`Live::write_lock`].
    pub fn unqueue_all_held(&self, _held: &std::sync::MutexGuard<'_, ()>, run: &str) {
        let mut notes = self.notes(run);
        let mut changed = false;
        for note in notes.iter_mut() {
            if !note.delivered && note.queued {
                note.queued = false;
                changed = true;
            }
        }
        if changed {
            let _ = self.save_notes(run, &notes);
        }
    }

    /// Undelivered notes matching `pred`, marked delivered. A note is read once.
    pub fn take_fresh_where(&self, run: &str, pred: impl Fn(&Note) -> bool) -> Vec<Note> {
        let guard = self.notes_lock.lock().unwrap();
        self.take_fresh_held(&guard, run, pred)
    }

    /// [`Live::take_fresh_where`] for a caller that already holds [`Live::write_lock`].
    pub fn take_fresh_held(&self, _held: &std::sync::MutexGuard<'_, ()>, run: &str, pred: impl Fn(&Note) -> bool) -> Vec<Note> {
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

    /// Put notes back as undelivered, for a turn that did not start after all.
    pub fn return_held(&self, _held: &std::sync::MutexGuard<'_, ()>, run: &str, ids: &[String]) {
        let mut notes = self.notes(run);
        let mut changed = false;
        for note in notes.iter_mut().filter(|note| ids.contains(&note.id)) {
            changed |= note.delivered;
            note.delivered = false;
        }
        if changed {
            let _ = self.save_notes(run, &notes);
        }
    }

    /// Remove notes by id (used when rewinding a run to an earlier turn, A2).
    pub fn remove_notes(&self, run: &str, ids: &[String]) -> anyhow::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let _guard = self.notes_lock.lock().unwrap();
        let mut notes = self.notes(run);
        let before = notes.len();
        notes.retain(|n| !ids.contains(&n.id));
        if notes.len() != before {
            self.save_notes(run, &notes)?;
        }
        Ok(())
    }

    /// Truncate `agents.jsonl` to the first `keep` entries and reset the in-memory cache (A2).
    pub fn truncate_transcript(&self, run: &str, keep: usize) -> anyhow::Result<usize> {
        self.flush(run);
        let (_, entries) = self.read(run, 0);
        let dropped = entries.len().saturating_sub(keep);
        let mut buf = Vec::new();
        for entry in entries.iter().take(keep) {
            let mut stripped = entry.clone();
            if let Some(obj) = stripped.as_object_mut() {
                obj.remove("index");
            }
            serde_json::to_writer(&mut buf, &stripped)?;
            buf.push(b'\n');
        }
        self.store.write(&Self::key(run, "agents.jsonl"), &buf)?;
        self.cache.lock().unwrap().remove(run);
        self.hub.notify_agents(run);
        Ok(dropped)
    }

    /// True when an undelivered note matches `pred`.
    pub fn has_pending_where(&self, run: &str, pred: impl Fn(&Note) -> bool) -> bool {
        self.notes(run).iter().any(|note| !note.delivered && pred(note))
    }

    /// The undelivered notes for `to`, marked delivered. `workers` notes are kept for
    /// every later worker as well, so they are returned whether delivered or not.
    /// Queued notes are held back until the current turn finishes (A1).
    pub fn take_notes(&self, run: &str, to: &[&str]) -> Vec<String> {
        let _guard = self.notes_lock.lock().unwrap();
        let mut notes = self.notes(run);
        let mut out = Vec::new();
        for note in notes.iter_mut() {
            if note.queued || !to.contains(&note.to.as_str()) {
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

use std::collections::HashSet;

/// Coalesce consecutive text/thought deltas up to this size before forcing an entry.
const COALESCE_MAX_BYTES: usize = 16 * 1024;

struct CoalescedDelta {
    agent: Value,
    text: String,
    started_at: Instant,
}

struct RunWriterState {
    run: String,
    key: String,
    store: Arc<dyn Store>,
    hub: Arc<SignalHub>,
    flushed_seq: Arc<AtomicU64>,
    seen_seq: u64,
    out: Option<BufWriter<Box<dyn Write + Send>>>,
    opened: bool,
    fallback_batch: Vec<String>,
    buffered_bytes: usize,
    pending_lines: usize,
    /// Calls whose full `agent` tag has already been written on `start` (P2).
    announced: HashSet<(String, u64)>,
    /// Coalescing buffer for consecutive `text`/`thought` deltas within the 50ms window (P2).
    coalesce: HashMap<(String, u64, String), CoalescedDelta>,
    /// Split-secret carry window held across delta emissions (P3).
    carry: HashMap<(String, u64, String), CarryEntry>,
}

impl RunWriterState {
    fn new(run: String, store: Arc<dyn Store>, hub: Arc<SignalHub>, flushed_seq: Arc<AtomicU64>) -> Self {
        let key = Live::key(&run, "agents.jsonl");
        Self {
            run,
            key,
            store,
            hub,
            flushed_seq,
            seen_seq: 0,
            out: None,
            opened: false,
            fallback_batch: Vec::new(),
            buffered_bytes: 0,
            pending_lines: 0,
            announced: HashSet::new(),
            coalesce: HashMap::new(),
            carry: HashMap::new(),
        }
    }

    fn ensure_open(&mut self) {
        if self.opened {
            return;
        }
        self.opened = true;
        match self.store.open_append(&self.key) {
            Ok(Some(writer)) => {
                self.out = Some(BufWriter::with_capacity(WRITER_FLUSH_BYTES, writer));
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("taskmaster: the agent transcript of {} could not be opened: {err}", self.run);
            }
        }
    }

    fn compact_agent(&mut self, agent: &Value, kind: &str) -> Value {
        let id = agent["id"].as_str().unwrap_or("");
        let call = agent["call"].as_u64().unwrap_or(0);
        if call == 0 || id.is_empty() || id == "runtime" || id == "person" {
            return agent.clone();
        }
        let key = (id.to_string(), call);
        if kind == "start" {
            self.announced.insert(key);
            return agent.clone();
        }
        if kind == "released" {
            return agent.clone();
        }
        if self.announced.contains(&key) {
            json!({ "id": id, "call": call })
        } else {
            agent.clone()
        }
    }

    fn write_line(&mut self, agent: &Value, kind: &str, mut fields: Value) {
        if !fields.is_object() {
            fields = json!({});
        }
        crate::secrets::redact_json(&mut fields);
        let written_agent = self.compact_agent(agent, kind);
        let object = fields.as_object_mut().expect("an object");
        object.insert("at".into(), json!(rfc3339_now()));
        object.insert("agent".into(), written_agent);
        object.insert("kind".into(), json!(kind));
        if kind == "start" {
            object.insert("v".into(), json!(2));
        }
        let line = serde_json::to_string(&fields).unwrap_or_default();
        self.ensure_open();
        if let Some(writer) = self.out.as_mut() {
            if let Err(err) = writer.write_all(line.as_bytes()).and_then(|_| writer.write_all(b"\n")) {
                eprintln!("taskmaster: the agent transcript of {} was not written: {err}", self.run);
                self.out = None;
                self.opened = false;
                return;
            }
        } else {
            self.fallback_batch.push(line.clone());
        }
        self.buffered_bytes += line.len() + 1;
        self.pending_lines += 1;
    }

    fn emit_text_with_carry(&mut self, key: (String, u64, String), agent: &Value, delta: &str) {
        let combined = match self.carry.remove(&key) {
            Some(prev) => format!("{}{delta}", prev.text),
            None => delta.to_string(),
        };
        let kind = key.2.clone();
        if let Some(split) = crate::secrets::potential_secret_tail_start(&combined) {
            let tail = combined[split..].to_string();
            self.carry.insert(key, CarryEntry { agent: agent.clone(), text: tail, at: Instant::now() });
            if split == 0 {
                return;
            }
            self.write_line(agent, &kind, json!({ "text": &combined[..split] }));
        } else {
            self.write_line(agent, &kind, json!({ "text": combined }));
        }
    }

    fn flush_coalesced_key(&mut self, key: &(String, u64, String)) {
        if let Some(buf) = self.coalesce.remove(key)
            && !buf.text.is_empty()
        {
            self.emit_text_with_carry(key.clone(), &buf.agent, &buf.text);
        }
    }

    fn flush_coalesced_for(&mut self, agent_id: &str, call: u64) {
        for kind in ["text", "thought"] {
            let key = (agent_id.to_string(), call, kind.to_string());
            self.flush_coalesced_key(&key);
        }
    }

    fn flush_all_coalesced(&mut self) {
        let mut items: Vec<((String, u64, String), CoalescedDelta)> = self.coalesce.drain().collect();
        items.sort_by_key(|(_, v)| v.started_at);
        for (key, buf) in items {
            if !buf.text.is_empty() {
                self.emit_text_with_carry(key, &buf.agent, &buf.text);
            }
        }
    }

    fn flush_carry_for(&mut self, agent_id: &str, call: u64) {
        for kind in ["text", "thought"] {
            if let Some(held) = self.carry.remove(&(agent_id.to_string(), call, kind.to_string()))
                && !held.text.is_empty()
            {
                self.write_line(&held.agent, kind, json!({ "text": held.text }));
            }
        }
    }

    fn flush_stale_carry(&mut self, force: bool) {
        let now = Instant::now();
        let keys: Vec<(String, u64, String)> = self
            .carry
            .iter()
            .filter(|(_, entry)| force || now.saturating_duration_since(entry.at) >= CARRY_IDLE_FLUSH)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            if let Some(held) = self.carry.remove(&key)
                && !held.text.is_empty()
            {
                self.write_line(&held.agent, &key.2, json!({ "text": held.text }));
            }
        }
    }

    fn handle_entry(&mut self, seq: u64, agent: Value, kind: String, fields: Value) {
        self.seen_seq = self.seen_seq.max(seq);
        let agent_id = agent["id"].as_str().unwrap_or("").to_string();
        let call = agent["call"].as_u64().unwrap_or(0);

        let discrete = kind_is_discrete(&kind);
        let only_text = !discrete
            && fields.as_object().is_some_and(|m| m.len() == 1 && m.get("text").and_then(Value::as_str).is_some());

        if only_text {
            let delta = fields["text"].as_str().unwrap_or("");
            // Switching between text and thought for the same call preserves ordering.
            let other_kind = if kind == "text" { "thought" } else { "text" };
            self.flush_coalesced_key(&(agent_id.clone(), call, other_kind.to_string()));

            let key = (agent_id, call, kind);
            let flush_now = {
                let entry = self.coalesce.entry(key.clone()).or_insert_with(|| CoalescedDelta {
                    agent: agent.clone(),
                    text: String::new(),
                    started_at: Instant::now(),
                });
                entry.text.push_str(delta);
                entry.text.len() >= COALESCE_MAX_BYTES
            };
            if flush_now {
                self.flush_coalesced_key(&key);
            }
        } else {
            self.flush_coalesced_for(&agent_id, call);
            self.flush_carry_for(&agent_id, call);
            self.write_line(&agent, &kind, fields);
        }

        if discrete || self.buffered_bytes >= WRITER_FLUSH_BYTES {
            self.flush_disk();
        }
    }

    fn flush_disk(&mut self) {
        self.flush_all_coalesced();
        if self.pending_lines > 0 {
            if let Some(writer) = self.out.as_mut() {
                if let Err(err) = writer.flush() {
                    eprintln!("taskmaster: the agent transcript of {} was not flushed: {err}", self.run);
                    self.out = None;
                    self.opened = false;
                }
            } else if !self.fallback_batch.is_empty() {
                let batch = std::mem::take(&mut self.fallback_batch);
                if let Err(err) = self.store.append_batch(&self.key, &batch) {
                    eprintln!("taskmaster: the agent transcript of {} was not written: {err}", self.run);
                }
            }
            self.buffered_bytes = 0;
            self.pending_lines = 0;
            self.hub.notify_agents(&self.run);
        }
        self.flushed_seq.store(self.seen_seq, Ordering::SeqCst);
    }
}

fn kind_is_discrete(kind: &str) -> bool {
    kind != "text" && kind != "thought"
}

fn run_writer_loop(
    run: String,
    store: Arc<dyn Store>,
    hub: Arc<SignalHub>,
    rx: mpsc::Receiver<WriterCmd>,
    flushed_seq: Arc<AtomicU64>,
) {
    let mut state = RunWriterState::new(run, store, hub, flushed_seq);
    loop {
        let timeout = if state.pending_lines > 0 || !state.coalesce.is_empty() {
            WRITER_FLUSH_INTERVAL
        } else if !state.carry.is_empty() {
            CARRY_IDLE_FLUSH
        } else {
            WRITER_IDLE_EXIT
        };
        match rx.recv_timeout(timeout) {
            Ok(cmd) => {
                let mut acks = Vec::new();
                match cmd {
                    WriterCmd::Entry { seq, agent, kind, fields } => state.handle_entry(seq, agent, kind, fields),
                    WriterCmd::Flush { ack } => acks.push(ack),
                }
                // Drain any already-queued entries so sequential pushes batch, coalesce, and catch split secrets.
                while let Ok(more) = rx.try_recv() {
                    match more {
                        WriterCmd::Entry { seq, agent, kind, fields } => state.handle_entry(seq, agent, kind, fields),
                        WriterCmd::Flush { ack } => acks.push(ack),
                    }
                }
                if !acks.is_empty() {
                    state.flush_disk();
                    for ack in acks {
                        let _ = ack.send(());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if state.pending_lines > 0 || !state.coalesce.is_empty() {
                    state.flush_disk();
                } else if !state.carry.is_empty() {
                    state.flush_stale_carry(false);
                    state.flush_disk();
                } else {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.flush_all_coalesced();
                state.flush_stale_carry(true);
                state.flush_disk();
                break;
            }
        }
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
        assert_eq!(b1, br#"{"n":1}"#.iter().copied().chain(*b"\n").collect::<Vec<_>>());
        // Caught up: zero bytes read.
        let (len_same, b_empty) = store.read_from("a.jsonl", len1).unwrap().unwrap();
        assert_eq!(len_same, len1);
        assert!(b_empty.is_empty());
        // Appending another line reads ONLY the newly appended bytes.
        store.append("a.jsonl", r#"{"n":2}"#).unwrap();
        let (len2, b2) = store.read_from("a.jsonl", len1).unwrap().unwrap();
        assert!(len2 > len1);
        assert_eq!(b2, br#"{"n":2}"#.iter().copied().chain(*b"\n").collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn watch_only_wakes_for_its_own_run_and_does_not_read_others() {
        struct CountingStore {
            inner: Arc<dyn Store>,
            reads: Arc<Mutex<HashMap<String, usize>>>,
        }
        impl Store for CountingStore {
            fn read(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
                *self.reads.lock().unwrap().entry(key.to_string()).or_default() += 1;
                self.inner.read(key)
            }
            fn read_from(&self, key: &str, offset: u64) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
                *self.reads.lock().unwrap().entry(key.to_string()).or_default() += 1;
                self.inner.read_from(key, offset)
            }
            fn write(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
                self.inner.write(key, bytes)
            }
            fn append(&self, key: &str, line: &str) -> anyhow::Result<()> {
                self.inner.append(key, line)
            }
            fn append_batch(&self, key: &str, lines: &[String]) -> anyhow::Result<()> {
                self.inner.append_batch(key, lines)
            }
            fn open_append(&self, key: &str) -> anyhow::Result<Option<Box<dyn Write + Send>>> {
                self.inner.open_append(key)
            }
            fn remove(&self, key: &str) -> anyhow::Result<()> {
                self.inner.remove(key)
            }
            fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
                self.inner.list(prefix)
            }
            fn stat(&self, key: &str) -> Option<(u64, u128)> {
                self.inner.stat(key)
            }
        }

        let reads = Arc::new(Mutex::new(HashMap::new()));
        let store: Arc<dyn Store> = Arc::new(CountingStore { inner: tmp("p1-watch"), reads: reads.clone() });
        let live = Live::new(store);
        let mut rx_a = live.watch("run-a");
        let _ = live.read("run-a", 0);
        reads.lock().unwrap().clear();

        let agent = json!({ "id": "worker", "call": 1 });
        for i in 0..1000 {
            live.push("run-b", &agent, "text", json!({ "text": format!("chunk {i} ") }));
        }
        live.flush("run-b");

        // Watching run-a must never wake or read run-a's files because of run-b's traffic.
        let woke = tokio::time::timeout(Duration::from_millis(80), rx_a.changed()).await;
        assert!(woke.is_err(), "run-a watcher woke on run-b traffic");
        assert_eq!(reads.lock().unwrap().get("runs/run-a/agents.jsonl").copied().unwrap_or(0), 0);
        assert_eq!(reads.lock().unwrap().get("runs/run-a/events.jsonl").copied().unwrap_or(0), 0);
    }

    #[test]
    fn a_secret_split_across_two_text_deltas_is_redacted() {
        let live = Live::new(tmp("p3-split-secret"));
        let agent = json!({ "id": "worker", "call": 7 });
        live.push("r1", &agent, "text", json!({ "text": "token=sk-ant-123456" }));
        live.push("r1", &agent, "text", json!({ "text": "7890abcdef done" }));
        let (_, entries) = live.read("r1", 0);
        let combined: String = entries.iter().filter_map(|e| e["text"].as_str()).collect();
        assert!(combined.contains("•••"), "expected redaction in `{combined}`");
        assert!(!combined.contains("sk-ant-"), "raw prefix leaked in `{combined}`");
        assert!(!combined.contains("7890abcdef"), "raw suffix leaked in `{combined}`");

        // Also when separated by a flush between deltas.
        let agent2 = json!({ "id": "worker", "call": 8 });
        live.push("r1", &agent2, "text", json!({ "text": "Authorization: Bearer abcdef" }));
        live.flush("r1");
        live.push("r1", &agent2, "text", json!({ "text": "1234567890 ok" }));
        live.push("r1", &agent2, "end", json!({ "ok": true }));
        let (_, entries) = live.read("r1", 0);
        let all: String = entries.iter().map(Value::to_string).collect();
        assert!(!all.contains("abcdef1234567890"), "bearer token leaked across flush: {all}");
        assert!(all.contains("Bearer •••"), "expected bearer redaction: {all}");
    }

    #[test]
    fn concurrent_pushes_do_not_block_notes_and_keep_high_throughput() {
        let live = Arc::new(Live::new(tmp("p3-throughput")));
        let started = Instant::now();
        let mut handles = Vec::new();
        for t in 0..8 {
            let live = live.clone();
            handles.push(std::thread::spawn(move || {
                let agent = json!({ "id": format!("page:{t}"), "call": t as u64 + 1 });
                for i in 0..1250 {
                    live.push("busy", &agent, "text", json!({ "text": format!("line {i} ok ") }));
                }
            }));
        }
        // Notes lock is independent of transcript pushes.
        let note = live.add_note("busy", "master", "check layout").unwrap();
        assert_eq!(note.text, "check layout");
        for h in handles {
            h.join().unwrap();
        }
        live.flush("busy");
        let elapsed = started.elapsed();
        let (total, entries) = live.read("busy", 0);
        // 10,000 rapid text deltas across 8 workers coalesce into far fewer entries without losing text.
        assert!(total > 0 && total < 10_000, "expected coalescing, got {total} entries");
        let bytes: usize = entries.iter().filter_map(|e| e["text"].as_str().map(str::len)).sum();
        assert!(bytes >= 80_000, "expected all text preserved, got {bytes} bytes");
        assert!(elapsed < Duration::from_secs(10), "10,000 pushes took {elapsed:?}");
    }

    #[test]
    fn compact_tags_and_coalescing_shrink_transcript_and_old_format_still_reads() {
        let store = tmp("p2-compact");
        let live = Live::new(store.clone());
        let full_tag = json!({
            "id": "page:home",
            "call": 42,
            "role": "worker",
            "harness": "claude",
            "model": "claude-sonnet-4-6",
            "page": "home",
            "unit": "layout",
            "owned_glob": "web/src/pages/Home.tsx",
            "worktree": "/tmp/example-project/.taskmaster/worktrees/page-home-layout",
            "branch": "tm/run-1/page-home"
        });
        live.push("r1", &full_tag, "start", json!({ "brief": "Build home page" }));
        for i in 0..50 {
            live.push("r1", &full_tag, "thought", json!({ "text": format!("step {i} checking layout; ") }));
        }
        live.push("r1", &full_tag, "end", json!({ "ok": true }));
        live.flush("r1");

        let raw = store.read("runs/r1/agents.jsonl").unwrap().unwrap();
        let (total, entries) = live.read("r1", 0);
        assert!(total <= 10, "expected coalesced entries, got {total}");
        assert_eq!(entries[0]["kind"], "start");
        assert_eq!(entries[0]["v"], 2);
        assert_eq!(entries[0]["agent"]["harness"], "claude");
        // Subsequent entries carry compact {id, call} only.
        let thought = entries.iter().find(|e| e["kind"] == "thought").unwrap();
        assert_eq!(thought["agent"], json!({ "id": "page:home", "call": 42 }));
        let end = entries.iter().find(|e| e["kind"] == "end").unwrap();
        assert_eq!(end["agent"], json!({ "id": "page:home", "call": 42 }));
        // Compare against uncoalesced old-format size (~50 * (340B tag + text)).
        let old_estimate = 50 * (full_tag.to_string().len() + 80);
        assert!(raw.len() * 3 < old_estimate, "expected >=3x smaller: new={} old~={}", raw.len(), old_estimate);

        // Old-format transcripts (v1, full agent tag on every line, no `v` field) still read cleanly.
        let legacy_line = json!({
            "at": "2026-01-01T00:00:00Z",
            "agent": full_tag,
            "kind": "thought",
            "text": "legacy thought"
        })
        .to_string();
        store.append("runs/legacy/agents.jsonl", &legacy_line).unwrap();
        let (ltotal, lentries) = live.read("legacy", 0);
        assert_eq!(ltotal, 1);
        assert_eq!(lentries[0]["agent"]["harness"], "claude");
        assert_eq!(lentries[0]["text"], "legacy thought");
    }

    #[test]
    fn transcript_cache_evicts_oldest_runs_past_cap() {
        let live = Live::new(tmp("p4-lru"));
        let agent = json!({ "id": "master", "call": 1 });
        for i in 0..20 {
            let run = format!("run-{i:02}");
            live.push(&run, &agent, "text", json!({ "text": "hello" }));
            let (total, _) = live.read(&run, 0);
            assert_eq!(total, 1);
            assert!(live.cached_runs() <= MAX_CACHED_RUNS, "cache grew to {}", live.cached_runs());
        }
        assert_eq!(live.cached_runs(), MAX_CACHED_RUNS);
    }
}
