//! The daemon's API with a scripted harness behind it, for working on the web UI.
//! No model is started: every agent is a script that streams text, thoughts and tool
//! calls with a pause between them, so planning, building and review can be watched.
//!
//! ```text
//! cargo run -p tm-daemon --example ui_fixture -- <project> <port> [web dir]
//! ```
//!
//! It prints the pairing code. `FIXTURE_DELAY_MS` sets the pause (default 350).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tm_daemon::{PairingAuth, Records, Server, ServerOptions};
use tm_harness::{Decision, Harness, HarnessEvent, PermissionGate, PermissionRequest, ToolKind, RunSummary, Usage, WorkerTask};
use tokio::sync::mpsc;

/// Create this file to make the next plan carry a page that asks for two risky tool calls.
const ASK_FLAG: [&str; 1] = ["/workspace/t3setup/fixture-ask-tools"];

struct Scripted {
    id: &'static str,
    cart: Arc<AtomicUsize>,
    reviews: Arc<AtomicUsize>,
}

fn pause() -> Duration {
    Duration::from_millis(std::env::var("FIXTURE_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(350))
}

async fn say(events: &mpsc::Sender<HarnessEvent>, event: HarnessEvent) {
    tokio::time::sleep(pause()).await;
    let _ = events.send(event).await;
}

fn text(t: &str) -> HarnessEvent {
    HarnessEvent::Text { text: t.into() }
}

fn thought(t: &str) -> HarnessEvent {
    HarnessEvent::Thought { text: t.into() }
}

async fn tool(events: &mpsc::Sender<HarnessEvent>, name: &str, input: serde_json::Value, allowed: bool) {
    say(events, HarnessEvent::ToolCall { id: format!("call-{name}"), name: name.into(), input }).await;
    say(events, HarnessEvent::PermissionDecided { tool: name.into(), allowed }).await;
}

const PLAN: &str = r##"{"contracts":[{"path":"contracts/types.ts","content":"export type Book = { id: string; title: string; price: number };\n"}],
"pages":[
 {"id":"home","route":"/","owned_glob":"app/home/**","cites":["shop-theme"],"brief":"The front page: a hero and three featured books."},
 {"id":"books","route":"/books","owned_glob":"app/books/**","cites":["shop-theme"],"brief":"A searchable list of books.","units":[{"id":"list","owned_glob":"app/books/page.tsx","brief":"The list component."},{"id":"styles","owned_glob":"app/books/styles.css","brief":"Its styles."}]},
 {"id":"cart","route":"/cart","owned_glob":"app/cart/**","cites":["shop-theme"],"brief":"The cart with a total and a checkout button."}],
"estimate":{"tokens":0,"usd":0},
"dictionary":[{"key":"shop-theme","kind":"hint","summary":"black and white theme","content":"export const theme = { ink: \"#000\", paper: \"#fff\" };\n","stack":"next"}]}"##;

#[async_trait]
impl Harness for Scripted {
    fn id(&self) -> &'static str {
        self.id
    }

    async fn run(
        &self,
        task: WorkerTask,
        gate: Arc<dyn PermissionGate>,
        events: mpsc::Sender<HarnessEvent>,
    ) -> anyhow::Result<RunSummary> {
        let brief = task.brief.clone();
        // A prompt that says so stops before any plan, so the UI can show a typed limit.
        if brief.contains("USAGE LIMIT") {
            say(&events, text("You've hit your usage limit. Resets 6:30 PM.")).await;
        } else if brief.contains("Plan this product and lock its contracts.") || brief.contains("Plan this small product quickly.") || brief.contains("Repair this plan.") {
            say(&events, thought("The request names three pages. I will look for a theme in the Dictionary first.")).await;
            tool(&events, "Read", json!({ "file_path": "README.md" }), true).await;
            tool(&events, "dictionary__search", json!({ "query": "theme" }), true).await;
            for chunk in [
                "Three pages share one contract, ",
                "`contracts/types.ts`. ",
                "The home page lives in app/home/page.tsx and the list in app/books/page.tsx:1; ",
                "both read src/main.rs:2:5 for nothing at all, ",
                "which is only here to show a path word.\n\n",
            ] {
                say(&events, text(chunk)).await;
            }
            let plan = if ASK_FLAG.iter().any(|f| std::fs::remove_file(f).is_ok()) {
                PLAN.replace(r#"{"id":"books","route":"/books","owned_glob":"app/books/**""#, r#"{"id":"shelf","route":"/shelf","owned_glob":"app/shelf/**""#).replace("app/books/", "app/shelf/").replace("A searchable list of books.", "A shelf of books (asks the person).")
            } else {
                PLAN.to_string()
            };
            for part in plan.as_bytes().chunks(160) {
                say(&events, text(&String::from_utf8_lossy(part))).await;
            }
        } else if let Some(id) = brief.split("Build page `").nth(1).and_then(|rest| rest.split('`').next()) {
            let id = id.to_string();
            say(&events, thought(&format!("Page `{id}` owns app/{id}/**. I read the contract first."))).await;
            tool(&events, "Read", json!({ "file_path": "contracts/types.ts" }), true).await;
            tool(&events, "Bash", json!({ "command": "npm test" }), false).await;
            // `FIXTURE_STREAM=<n>` streams n thought and n text fragments, a few words each, the
            // way a real harness sends its tokens, so the run view can be profiled under load.
            let fragments: usize = std::env::var("FIXTURE_STREAM").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            for i in 0..fragments {
                say(&events, thought(&format!("weighing step {i} of `{id}`, "))).await;
                say(&events, text(&format!("line {i}: checked app/{id}/page.tsx:{} and moved on. ", i % 40 + 1))).await;
            }
            // A page whose brief says it asks the person waits on two risky calls (see the plan).
            if brief.contains("(asks the person)") && !brief.contains("You are unit `styles`") {
                let shell = PermissionRequest { tool: "Bash".into(), kind: ToolKind::Execute, paths: vec![], command: Some("npm install left-pad".into()), url: None };
                let outside = PermissionRequest { tool: "Write".into(), kind: ToolKind::Edit, paths: vec!["/etc/hosts".into()], command: None, url: None };
                for request in [shell, outside] {
                    let allowed = gate.decide_async(&request).await == Decision::Allow;
                    let _ = events.send(HarnessEvent::PermissionDecided { tool: request.tool, allowed }).await;
                }
            }
            let first_cart = id == "cart" && self.cart.fetch_add(1, Ordering::SeqCst) == 0;
            let styles = brief.contains("You are unit `styles`");
            let file = if styles { format!("app/{id}/styles.css") } else { format!("app/{id}/page.tsx") };
            let body = if styles { "main { padding: 1rem; }\n".to_string() } else { format!(
                "import type {{ Book }} from \"../../contracts/types\";\n\nexport default function Page() {{\n  const books: Book[] = [];\n  return <main>{id}: {{books.length}} books</main>;\n}}\n"
            ) };
            std::fs::create_dir_all(task.cwd.join(format!("app/{id}")))?;
            std::fs::write(task.cwd.join(&file), body)?;
            tool(&events, "Write", json!({ "file_path": file, "content": "…" }), true).await;
            if first_cart {
                // The first try at the cart edits a file it does not own, so it fails.
                std::fs::create_dir_all(task.cwd.join("app/home"))?;
                std::fs::write(task.cwd.join("app/home/page.tsx"), "broken\n")?;
                tool(&events, "Edit", json!({ "file_path": "app/home/page.tsx" }), true).await;
            }
            say(&events, text(&format!("Wrote {file}:3 against contracts/types.ts#L1. "))).await;
            say(&events, text("The key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWx was in an env file; it stays masked.\n")).await;
            say(&events, text("Done.")).await;
        } else if brief.contains("You are reviewing a code change") {
            // The on-demand review: one combined reviewer, slow enough to see "in progress".
            say(&events, thought("Reading the whole diff once.")).await;
            tool(&events, "Read", json!({ "file_path": "app/home/page.tsx" }), true).await;
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            say(&events, text("[{\"file\":\"app/home/page.tsx\",\"line\":5,\"failure\":\"the count is not labelled\"}]")).await;
        } else if brief.contains("reviewing one connection") || brief.contains("security problems") {
            say(&events, thought("Following each import from page to contract.")).await;
            tool(&events, "Read", json!({ "file_path": "app/home/page.tsx" }), true).await;
            let first = self.reviews.fetch_add(1, Ordering::SeqCst) == 0;
            say(&events, text("Checked app/home/page.tsx:5 against contracts/types.ts.\n")).await;
            if first && !brief.contains("security problems") {
                say(&events, text("[{\"page\":\"home\",\"file\":\"app/home/page.tsx\",\"line\":5,\"failure\":\"the count is not labelled\"}]")).await;
            } else {
                say(&events, text("[]")).await;
            }
        } else if brief.contains("Decide whether it is real") {
            tool(&events, "Read", json!({ "file_path": "app/home/page.tsx" }), true).await;
            say(&events, text("app/home/page.tsx:5 renders a number with a word next to it. ")).await;
            say(&events, text("{\"real\": true}")).await;
        } else {
            say(&events, text("Nothing to do.")).await;
        }
        // agy reports no usage; grok reports tokens but no price (subscription CLI).
        // The rest report all four counts; a plan writes more than a page, a review less.
        let output = if brief.contains("Plan this") { 1900 } else if brief.contains("Build page") { 640 } else { 180 };
        let usage = if self.id == "agy" {
            Usage::default()
        } else {
            let claude = self.id == "claude";
            Usage { input: 600 + brief.len() as u64 / 4, output, cache_read: 4200, cache_write: if claude { 2600 } else { 0 } }
        };
        let cost = if self.id == "agy" || self.id == "grok" { None } else { Some(0.012) };
        Ok(RunSummary { ok: true, usage, cost_usd: cost, denied: 1, freed: None, requests: Some(1) })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let project = PathBuf::from(args.next().expect("a project directory"));
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8737);
    let web = args.next().map(PathBuf::from);
    let auth = Arc::new(PairingAuth::new(Records::files(&project))?);
    println!("code {}", auth.code());
    let mut options = ServerOptions::new(&project, ([127, 0, 0, 1], port).into(), auth);
    options.web = web;
    // A harmless dev server for the live preview: a static file server on the given port.
    options.preview_command = std::env::var("FIXTURE_PREVIEW").unwrap_or_else(|_| "python3 -m http.server {port} --bind 127.0.0.1".into());
    let (cart, reviews) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    options.harness = Arc::new(move |name: &str| {
        let id: &'static str = match name {
            "claude" => "claude",
            "agy" => "agy",
            "grok" => "grok",
            "opencode" => "opencode",
            "codex" => "codex",
            other => anyhow::bail!("unknown harness `{other}`"),
        };
        Ok(Arc::new(Scripted { id, cart: cart.clone(), reviews: reviews.clone() }) as Arc<dyn Harness>)
    });
    let server = Server::serve(options).await?;
    println!("api {}", server.url);
    tokio::signal::ctrl_c().await?;
    Ok(())
}
