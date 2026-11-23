//! Run one small task through Claude Code's interactive code mode and print what came back.
//!
//! ```text
//! cargo run -p tm-harness --example code_mode_probe -- [model]
//! ```

use std::sync::Arc;
use std::time::Duration;

use tm_harness::{Harness, WorkerTask, WorktreeGate};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("tm-code-mode-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let task = WorkerTask {
        brief: "Create the file notes/hello.txt containing the single line `hello from code mode`, then reply DONE.".into(),
        cwd: dir.clone(),
        model: std::env::args().nth(1),
        effort: None,
        dictionary: None,
        timeout: Duration::from_secs(240),
        stall: None,
        read_only: false,
        code_mode: true,
        shell: None,
        permission_mode: None,
        hold: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            println!("event: {}", serde_json::to_string(&event).unwrap_or_default().chars().take(160).collect::<String>());
        }
    });
    let started = std::time::Instant::now();
    let summary = tm_harness::claude::ClaudeCode::default().run(task, Arc::new(WorktreeGate::new(&dir, vec![])), tx).await;
    printer.await?;
    println!("after {:.1}s: {summary:?}", started.elapsed().as_secs_f64());
    println!("file: {:?}", std::fs::read_to_string(dir.join("notes/hello.txt")).ok());
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
