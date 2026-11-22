//! Run one task through a harness and print what it did.
//!
//! ```text
//! cargo run -p tm-harness --example spike -- <harness> <cwd> <prompt> [model] [dict-url token]
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tm_harness::{DictionaryAccess, HARNESS_STATE_DIRS, HarnessEvent, WorkerTask, WorktreeGate};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [which, cwd, prompt, rest @ ..] = args.as_slice() else {
        bail!("usage: spike <harness> <cwd> <prompt> [model] [dict-url token]");
    };
    let harness = tm_harness::harness(which)?;
    let model = rest.first().filter(|m| !m.is_empty()).cloned();
    let dictionary = match rest.get(1..) {
        Some([url, token]) => Some(DictionaryAccess {
            name: "dict".into(),
            url: url.clone(),
            bearer_token: token.clone(),
        }),
        _ => None,
    };
    let cwd = std::fs::canonicalize(cwd).with_context(|| format!("no such directory: {cwd}"))?;
    let home = std::env::var("HOME")?;
    let mut gate = WorktreeGate::new(&cwd, vec![]);
    for dir in HARNESS_STATE_DIRS {
        gate = gate.with_readable(format!("{home}/{dir}"));
    }
    let gate = Arc::new(gate);
    let task = WorkerTask {
        brief: prompt.clone(),
        cwd,
        model,
        effort: None,
        dictionary,
        timeout: Duration::from_secs(240),
        stall: None,
        read_only: false,
        code_mode: false,
        shell: None,
    };

    let (tx, mut rx) = mpsc::channel(64);
    let printer = tokio::spawn(async move {
        let mut said = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                HarnessEvent::ToolCall { name, input, .. } => {
                    let input: String = input.to_string().chars().take(120).collect();
                    println!("tool   {name} {input}");
                }
                HarnessEvent::PermissionDecided { tool, allowed } => {
                    println!("gate   {} {tool}", if allowed { "ALLOW" } else { "DENY " });
                }
                HarnessEvent::Text { text } => said.push_str(&text),
                HarnessEvent::Thought { .. }
                | HarnessEvent::SessionStarted { .. }
                | HarnessEvent::RateLimit(_) => {}
                HarnessEvent::ToolResult { id, text, diff } => {
                    let text: String = text.chars().take(120).collect();
                    println!("result {id} {text}");
                    if let Some(diff) = diff {
                        let diff: String = diff.chars().take(120).collect();
                        println!("diff   {diff}");
                    }
                }
            }
        }
        let said: String = said.trim().chars().take(600).collect();
        println!("said   {said}");
    });
    let summary = harness.run(task, gate, tx).await?;
    printer.await?;
    println!("result {}", serde_json::to_string(&summary)?);
    Ok(())
}
