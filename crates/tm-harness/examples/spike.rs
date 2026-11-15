//! Run one task through a harness and print what it did.
//!
//! ```text
//! cargo run -p tm-harness --example spike -- <claude|agy> <cwd> <prompt> [model] [dict-url token]
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use tm_harness::acp::AcpHarness;
use tm_harness::claude::ClaudeCode;
use tm_harness::{DictionaryAccess, Harness, HarnessEvent, WorkerTask, WorktreeGate};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [which, cwd, prompt, rest @ ..] = args.as_slice() else {
        bail!("usage: spike <claude|agy> <cwd> <prompt> [model] [dict-url token]");
    };
    let harness: Box<dyn Harness> = match which.as_str() {
        "claude" => Box::new(ClaudeCode::default()),
        "agy" => Box::new(AcpHarness::antigravity()?),
        other => bail!("unknown harness `{other}`"),
    };
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
    let gate = Arc::new(
        WorktreeGate::new(&cwd, vec!["mcp_dict_".into()])
            .with_readable(format!("{home}/.gemini")),
    );
    let task = WorkerTask {
        brief: prompt.clone(),
        cwd,
        model,
        dictionary,
        timeout: Duration::from_secs(240),
    };

    let (tx, mut rx) = mpsc::channel(64);
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                HarnessEvent::ToolCall { name, input, .. } => {
                    let input: String = input.to_string().chars().take(120).collect();
                    println!("tool   {name} {input}");
                }
                HarnessEvent::PermissionDecided { tool, allowed } => {
                    println!("gate   {} {tool}", if allowed { "ALLOW" } else { "DENY " });
                }
                HarnessEvent::Text { .. } | HarnessEvent::Thought { .. } => {}
            }
        }
    });
    let summary = harness.run(task, gate, tx).await?;
    printer.await?;
    println!("result {}", serde_json::to_string(&summary)?);
    Ok(())
}
