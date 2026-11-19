//! Score a built project with the free UI checks, for scripts/bench.sh: one JSON line with
//! each check passed, failed or not run, and what failed.
//!
//! ```text
//! cargo run --release -p tm-daemon --example ui_score -- <project dir>
//! ```

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).context("usage: ui_score <project dir>")?;
    let score = tm_daemon::audit::score(std::path::Path::new(&dir)).await?;
    println!("{score}");
    Ok(())
}
