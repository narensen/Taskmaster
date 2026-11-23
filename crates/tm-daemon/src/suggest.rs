//! A5 (Labs, off by default): after a turn ends, one small call proposes the next message.
//! The composer shows it as its placeholder, and Tab fills it in.
//!
//! The call goes to the master's provider and model at low effort, reads nothing, and is
//! billed in the run's ledger like any other call.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_core::{LedgerEntry, Role};
use tm_harness::{Harness, HarnessEvent, WorkerTask, WorktreeGate};
use tokio::sync::mpsc;

/// The saved suggestion: `runs/<id>/suggest.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub text: String,
    pub at: String,
}

/// What the call is told about the run.
pub struct Context<'a> {
    pub asked: &'a str,
    pub outcome: &'a str,
    pub pages: String,
    /// The agents' last words, newest last.
    pub said: Vec<String>,
}

pub fn brief(ctx: &Context) -> String {
    let asked: String = ctx.asked.chars().take(600).collect();
    let mut said = ctx.said.join("\n");
    if said.chars().count() > 1200 {
        said = said.chars().rev().take(1200).collect::<Vec<_>>().into_iter().rev().collect();
    }
    format!(
        "A person is using a coding tool. A run they asked for just finished. Suggest the one message they are most likely to send next: a fix, a check, or the next small addition.\n\n\
         They asked: {asked}\n\
         Outcome: {}\n\
         Pages: {}\n\
         The agents' last words:\n{said}\n\n\
         Do not use any tools. Reply with the message only, in the person's voice, at most 20 words, with no quotes and nothing before or after it. If nothing is worth suggesting, reply NONE.",
        ctx.outcome, ctx.pages
    )
}

/// The suggestion from a reply, or `None` for an empty one, `NONE`, or a reply too long to
/// be one message.
pub fn clean(reply: &str) -> Option<String> {
    let line = reply.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '“' || c == '”').trim();
    if line.is_empty() || line.eq_ignore_ascii_case("none") || line.split_whitespace().count() > 40 {
        return None;
    }
    Some(line.chars().take(240).collect())
}

/// Ask `harness` for the suggestion. Returns it and the ledger row for the call.
pub async fn ask(harness: Arc<dyn Harness>, model: Option<String>, scratch: &Path, prompt: String) -> (Option<String>, Option<LedgerEntry>) {
    let task = WorkerTask {
        brief: prompt,
        cwd: scratch.to_path_buf(),
        model: model.clone(),
        effort: Some("low".into()),
        dictionary: None,
        timeout: Duration::from_secs(90),
        stall: None,
        read_only: true,
        code_mode: false,
        shell: None,
        permission_mode: None,
    };
    let gate = Arc::new(WorktreeGate::new(scratch, vec![]).read_only());
    let (tx, mut rx) = mpsc::channel::<HarnessEvent>(256);
    let collect = tokio::spawn(async move {
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            if let HarnessEvent::Text { text: chunk } = event {
                text.push_str(&chunk);
            }
        }
        text
    });
    let started = Instant::now();
    let summary = harness.run(task, gate, tx).await;
    let text = collect.await.unwrap_or_default();
    let Ok(summary) = summary else { return (None, None) };
    let entry = LedgerEntry {
        at: crate::time::rfc3339_now(),
        role: Role::Master,
        page: None,
        harness: harness.id().to_string(),
        model,
        input: summary.usage.input,
        output: summary.usage.output,
        cache_read: summary.usage.cache_read,
        cache_write: summary.usage.cache_write,
        cost_usd: summary.cost_usd,
        seconds: started.elapsed().as_secs_f64(),
        ok: summary.ok,
        refused: Vec::new(),
        agent: Some("suggest".into()),
        unit: None,
        priced_by: summary.cost_usd.map(|_| "provider".to_string()),
        brief_hash: None,
        difficulty: None,
        language: None,
    };
    (summary.ok.then(|| clean(&text)).flatten(), Some(entry))
}

/// The agents' last words in a transcript tail, newest last.
pub fn last_words(entries: &[Value], max: usize) -> Vec<String> {
    let mut said: Vec<String> = entries
        .iter()
        .rev()
        .filter(|e| e["kind"] == "text" && e["agent"]["id"] != "person" && e["agent"]["id"] != "runtime")
        .filter_map(|e| e["text"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string))
        .take(max)
        .collect();
    said.reverse();
    said
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_becomes_one_short_message_or_nothing() {
        assert_eq!(clean("\n\"Add a test for the empty cart.\"\n").as_deref(), Some("Add a test for the empty cart."));
        assert_eq!(clean("NONE"), None);
        assert_eq!(clean("   "), None);
        assert_eq!(clean(&"word ".repeat(60)), None, "a speech is not a suggestion");
        let b = brief(&Context { asked: "A todo app", outcome: "done", pages: "home passed".into(), said: vec!["Done.".into()] });
        assert!(b.contains("They asked: A todo app") && b.contains("Do not use any tools") && b.contains("reply NONE"));
    }
}
