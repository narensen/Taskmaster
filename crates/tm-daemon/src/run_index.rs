//! Search inside runs (A7): an inverted index of what agents said, what the person asked,
//! and the commands the daemon ran, per run of a project.
//!
//! Built incrementally. Each run keeps the byte offset of `agents.jsonl` it has read, so a
//! query costs one `stat` per idle run and reads only the lines added since the last query
//! for a working one. Words of three letters or more are kept; a word split across two
//! streamed chunks of the same call is joined. A hit keeps where its line is in the file,
//! so a snippet reads one line, not the transcript.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::store::Store;

/// The fields of an entry that hold words worth finding.
const FIELDS: [&str; 4] = ["text", "command", "error", "reason"];
/// Entry kinds that are indexed. Tool output and diffs are left out: they are large and
/// mostly the project's own files, which the files search finds.
const KINDS: [&str; 8] = ["text", "thought", "turn", "status", "goal", "error", "command", "approval_requested"];
const MIN_WORD: usize = 3;
const MAX_WORD: usize = 40;

#[derive(Default)]
pub struct RunIndex {
    runs: Mutex<HashMap<(PathBuf, String), Postings>>,
}

#[derive(Default)]
struct Postings {
    /// Bytes of `agents.jsonl` read so far, and how many entries they held.
    offset: u64,
    entries: u32,
    /// The turn the next entry belongs to (the first is 1).
    turn: u32,
    agents: Vec<String>,
    hits: Vec<Hit>,
    words: BTreeMap<Box<str>, Vec<u32>>,
    /// The unfinished word at the end of a call's last streamed chunk.
    carry: HashMap<String, String>,
    last_at: String,
}

#[derive(Clone, Copy)]
struct Hit {
    entry: u32,
    agent: u16,
    turn: u16,
    offset: u64,
    len: u32,
}

/// One match, for the palette.
pub struct Found {
    pub run: String,
    pub entry: u32,
    pub agent: String,
    pub turn: u32,
    pub kind: String,
    pub snippet: String,
    pub at: String,
}

impl Found {
    pub fn to_json(&self) -> Value {
        json!({
            "run": self.run,
            "entry": self.entry,
            "agent": self.agent,
            "turn": self.turn,
            "kind": self.kind,
            "snippet": self.snippet,
            "at": self.at,
        })
    }
}

/// The words of `text`, lowercased, three letters or more.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= MIN_WORD)
        .map(|w| w.chars().take(MAX_WORD).collect::<String>().to_lowercase())
}

/// A query's terms, and whether its last one is still being typed (a prefix).
fn terms(query: &str) -> (Vec<String>, bool) {
    let list: Vec<String> = words(query).collect();
    let typing = query.chars().last().is_some_and(char::is_alphanumeric);
    (list, typing)
}

impl Postings {
    fn agent(&mut self, id: &str) -> u16 {
        match self.agents.iter().position(|a| a == id) {
            Some(at) => at as u16,
            None => {
                self.agents.push(id.to_string());
                (self.agents.len() - 1).min(u16::MAX as usize) as u16
            }
        }
    }

    fn add(&mut self, entry: &Value, offset: u64, len: u32) {
        let index = self.entries;
        self.entries += 1;
        let kind = entry["kind"].as_str().unwrap_or_default();
        let id = entry["agent"]["id"].as_str().unwrap_or("run");
        if kind == "turn" && id == "person" {
            self.turn = entry["turn"].as_u64().map(|t| t as u32).unwrap_or(self.turn + 1);
        }
        if let Some(at) = entry["at"].as_str() {
            self.last_at = at.to_string();
        }
        if !KINDS.contains(&kind) {
            return;
        }
        let mut found: Vec<String> = Vec::new();
        for field in FIELDS {
            if let Some(text) = entry[field].as_str() {
                found.extend(words(text));
            }
        }
        // A word cut between two chunks of the same call's stream is joined.
        if matches!(kind, "text" | "thought")
            && let Some(text) = entry["text"].as_str()
        {
            let key = format!("{id}#{}#{kind}", entry["agent"]["call"].as_u64().unwrap_or(0));
            let before = self.carry.remove(&key).unwrap_or_default();
            let head: String = text.chars().take_while(|c| c.is_alphanumeric()).collect();
            if !before.is_empty() && !head.is_empty() {
                let joined = format!("{before}{head}");
                if joined.chars().count() >= MIN_WORD {
                    found.push(joined.chars().take(MAX_WORD).collect::<String>().to_lowercase());
                }
            }
            if text.chars().last().is_some_and(char::is_alphanumeric) {
                // A chunk that is all one word continues the word before it.
                let tail: String = if head.chars().count() == text.chars().count() {
                    format!("{before}{text}")
                } else {
                    let rev: Vec<char> = text.chars().rev().take_while(|c| c.is_alphanumeric()).collect();
                    rev.into_iter().rev().collect()
                };
                self.carry.insert(key, tail.chars().take(MAX_WORD).collect());
            }
        }
        if found.is_empty() {
            return;
        }
        found.sort_unstable();
        found.dedup();
        let agent = self.agent(id);
        let hit = self.hits.len() as u32;
        self.hits.push(Hit { entry: index, agent, turn: self.turn.min(u16::MAX as u32) as u16, offset, len });
        for word in found {
            self.words.entry(word.into_boxed_str()).or_default().push(hit);
        }
    }

    /// Read what was appended since the last call. Returns false when the record is gone.
    fn catch_up(&mut self, store: &dyn Store, key: &str) -> bool {
        let Ok(Some((len, fresh))) = store.read_from(key, self.offset) else { return false };
        if len < self.offset {
            // The record was rewritten (a rewind, a restore): start again.
            *self = Postings::default();
            return self.catch_up(store, key);
        }
        // Only whole lines: the writer may be part way through the last one.
        let whole = fresh.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        let mut at = self.offset;
        for line in fresh[..whole].split_inclusive(|b| *b == b'\n') {
            let start = at;
            at += line.len() as u64;
            let body = line.strip_suffix(b"\n").unwrap_or(line);
            if body.is_empty() {
                continue;
            }
            // Count entries as `Live::read` does: a line that does not parse is not one.
            if let Ok(entry) = serde_json::from_slice::<Value>(body) {
                self.add(&entry, start, body.len() as u32);
            }
        }
        self.offset += whole as u64;
        true
    }

    /// The hits holding every term; the last term may be a prefix while it is typed.
    fn matches(&self, terms: &[String], typing: bool) -> Vec<u32> {
        let mut sets: Vec<Vec<u32>> = Vec::new();
        for (i, term) in terms.iter().enumerate() {
            let mut set: Vec<u32> = if typing && i + 1 == terms.len() {
                self.words
                    .range::<str, _>((std::ops::Bound::Included(term.as_str()), std::ops::Bound::Unbounded))
                    .take_while(|(word, _)| word.starts_with(term.as_str()))
                    .flat_map(|(_, hits)| hits.iter().copied())
                    .collect()
            } else {
                self.words.get(term.as_str()).cloned().unwrap_or_default()
            };
            set.sort_unstable();
            set.dedup();
            sets.push(set);
        }
        sets.sort_by_key(Vec::len);
        let Some((first, rest)) = sets.split_first() else { return Vec::new() };
        first.iter().copied().filter(|hit| rest.iter().all(|set| set.binary_search(hit).is_ok())).collect()
    }
}

impl RunIndex {
    /// Search the project's runs. Up to `per_run` hits from each, `limit` in all, the runs
    /// with the latest activity first and each run's latest hits first.
    pub fn search(&self, project: &Path, store: &dyn Store, runs: &[String], query: &str, limit: usize, per_run: usize) -> Vec<Found> {
        let (terms, typing) = terms(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let mut index = self.runs.lock().unwrap();
        // Forget runs that are gone.
        index.retain(|(p, run), _| p != project || runs.contains(run));
        let mut found: Vec<(String, Vec<Found>)> = Vec::new();
        for run in runs {
            let key = format!("runs/{run}/agents.jsonl");
            let postings = index.entry((project.to_path_buf(), run.clone())).or_insert_with(|| Postings { turn: 1, ..Postings::default() });
            if postings.turn == 0 {
                postings.turn = 1;
            }
            if !postings.catch_up(store, &key) {
                continue;
            }
            let hits = postings.matches(&terms, typing);
            if hits.is_empty() {
                continue;
            }
            let mut list = Vec::new();
            for &hit in hits.iter().rev().take(per_run) {
                let hit = postings.hits[hit as usize];
                let Some(entry) = store.read_range(&key, hit.offset, hit.len as usize).ok().flatten().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) else {
                    continue;
                };
                list.push(Found {
                    run: run.clone(),
                    entry: hit.entry,
                    agent: postings.agents.get(hit.agent as usize).cloned().unwrap_or_default(),
                    turn: hit.turn as u32,
                    kind: entry["kind"].as_str().unwrap_or_default().to_string(),
                    snippet: snippet(&entry, &terms),
                    at: entry["at"].as_str().unwrap_or_default().to_string(),
                });
            }
            found.push((postings.last_at.clone(), list));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0));
        found.into_iter().flat_map(|(_, list)| list).take(limit).collect()
    }
}

/// About 140 characters of the entry's words around the first term found.
fn snippet(entry: &Value, terms: &[String]) -> String {
    let text = FIELDS.iter().filter_map(|f| entry[*f].as_str()).collect::<Vec<_>>().join(" ");
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    // Lowercasing can change byte lengths; find the term in chars, not bytes.
    let chars: Vec<char> = flat.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let at = terms
        .iter()
        .filter_map(|term| {
            let t: Vec<char> = term.chars().collect();
            lower_chars.windows(t.len().max(1)).position(|w| w == t.as_slice())
        })
        .min()
        .unwrap_or(0)
        .min(chars.len());
    let start = at.saturating_sub(50);
    let end = (at + 90).min(chars.len());
    let mut out: String = chars[start..end].iter().collect();
    if start > 0 {
        out = format!("…{}", out.trim_start());
    }
    if end < chars.len() {
        out = format!("{}…", out.trim_end());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileStore;

    fn line(store: &FileStore, run: &str, entry: Value) {
        store.append(&format!("runs/{run}/agents.jsonl"), &entry.to_string()).unwrap();
    }

    #[test]
    fn finds_words_across_runs_incrementally_with_prefixes_and_split_chunks() {
        let dir = std::env::temp_dir().join(format!("tm-run-index-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileStore::in_project(&dir);
        let worker = json!({ "id": "page:a", "role": "worker", "call": 1 });
        line(&store, "r1", json!({ "agent": worker, "kind": "start", "at": "2026-11-01T00:00:00Z" }));
        line(&store, "r1", json!({ "agent": worker, "kind": "text", "text": "Wired the checkout flo", "at": "2026-11-01T00:00:01Z" }));
        line(&store, "r1", json!({ "agent": { "id": "page:a", "call": 1 }, "kind": "text", "text": "w to the payment form.", "at": "2026-11-01T00:00:02Z" }));
        line(&store, "r2", json!({ "agent": { "id": "master", "call": 1 }, "kind": "text", "text": "Plan: a pricing table.", "at": "2026-11-02T00:00:00Z" }));
        let index = RunIndex::default();
        let runs = vec!["r1".to_string(), "r2".to_string()];

        let hits = index.search(&dir, &store, &runs, "payment", 10, 3);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].run.as_str(), hits[0].entry, hits[0].agent.as_str(), hits[0].turn), ("r1", 2, "page:a", 1));
        assert!(hits[0].snippet.contains("payment form"), "{}", hits[0].snippet);
        // A word split between two streamed chunks is found whole, but a thought is not
        // joined to the text after it.
        assert_eq!(index.search(&dir, &store, &runs, "flow", 10, 3).len(), 1);
        line(&store, "r2", json!({ "agent": { "id": "master", "call": 2 }, "kind": "thought", "text": "pondering", "at": "2026-11-02T00:00:01Z" }));
        line(&store, "r2", json!({ "agent": { "id": "master", "call": 2 }, "kind": "text", "text": "done", "at": "2026-11-02T00:00:02Z" }));
        assert!(index.search(&dir, &store, &runs, "ponderingdone", 10, 3).is_empty());
        // The last word is a prefix while it is typed; all words must match.
        assert_eq!(index.search(&dir, &store, &runs, "checko", 10, 3).len(), 1);
        assert!(index.search(&dir, &store, &runs, "checkout pricing", 10, 3).is_empty());
        assert!(index.search(&dir, &store, &runs, "ab", 10, 3).is_empty(), "short words are not searched");

        // A new turn and a new line are picked up on the next query, without a rebuild.
        line(&store, "r1", json!({ "agent": { "id": "person", "role": "person" }, "kind": "turn", "turn": 2, "text": "make the payment button bigger", "at": "2026-11-03T00:00:00Z" }));
        let hits = index.search(&dir, &store, &runs, "payment", 10, 3);
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].entry, hits[0].turn, hits[0].agent.as_str()), (3, 2, "person"), "latest first");
        // Runs with the latest activity come first.
        let hits = index.search(&dir, &store, &runs, "pricing", 10, 3);
        assert_eq!(hits[0].run, "r2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
