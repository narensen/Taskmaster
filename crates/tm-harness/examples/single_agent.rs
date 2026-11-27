//! The baseline for scripts/bench.sh: one agent builds the whole product from the prompt,
//! the way a single-agent harness would, under the same confinement as a Taskmaster
//! worker (files in its directory, no shell). Prints one JSON line with the time and the
//! four-way usage.
//!
//! ```text
//! cargo run --release -p tm-harness --example single_agent -- <harness> <dir> <prompt-file> [model]
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use tm_harness::{HarnessEvent, WorkerTask, WorktreeGate};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let harness = args.next().expect("a harness");
    let dir = std::path::PathBuf::from(args.next().expect("a directory"));
    let prompt = std::fs::read_to_string(args.next().expect("a prompt file"))?;
    let model = args.next();
    let task = WorkerTask {
        brief: format!("Build this product in the current directory, completely, and say done when every file is written.\n\n{prompt}"),
        cwd: dir.clone(),
        model,
        effort: None,
        dictionary: None,
        timeout: Duration::from_secs(40 * 60),
        stall: None,
        read_only: false,
        code_mode: false,
        shell: None,
        permission_mode: None,
        native_search: false,
        hold: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<HarnessEvent>(1024);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let started = Instant::now();
    let result = tm_harness::harness(&harness)?.run(task, Arc::new(WorktreeGate::new(&dir, vec![])), tx).await;
    let seconds = started.elapsed().as_secs_f64();
    drain.await?;
    let (ok, usage, error) = match result {
        Ok(summary) => (summary.ok, summary.usage, None),
        Err(err) => (false, Default::default(), Some(format!("{err:#}"))),
    };
    println!(
        "{}",
        serde_json::json!({ "seconds": (seconds * 10.0).round() / 10.0, "ok": ok, "calls": 1, "input": usage.input, "output": usage.output,
            "cache_read": usage.cache_read, "cache_write": usage.cache_write, "error": error })
    );
    Ok(())
}
