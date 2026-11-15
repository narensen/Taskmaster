//! Serve a Dictionary directory over MCP on 127.0.0.1:<port> and print every cite.
//! `cargo run -p tm-dictionary --example serve -- <dir> <port> <token>`
use tm_dictionary::{Grant, LocalStore, McpState, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [dir, port, token] = a.as_slice() else { anyhow::bail!("usage: serve <dir> <port> <token>") };
    let store = LocalStore::open(dir)?;
    if store.get("greeting_contract")?.is_none() {
        store.publish("greeting_contract", tm_core::EntryKind::Hint, "greeting constant",
            "export const GREETING = 'taskmaster-spike-7Q';", "next@15")?;
    }
    let state = McpState::new(store);
    state.grant(token, Grant { run: "demo".into(), page: "demo".into() });
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    let s = state.clone();
    tokio::spawn(async move {
        let mut seen = 0;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let c = s.cites();
            for r in &c[seen..] { println!("CITE {} found={}", r.key, r.found); }
            seen = c.len();
        }
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}
