//! Checks that run the code. They cost no tokens.
//!
//! [`run_steps`] runs the project's own build and tests: the `build` and `test` scripts
//! in `package.json`, `cargo test` for a Rust project, and `pytest` where a Python
//! project has its configuration. [`browse`] starts a built Next.js app, opens every
//! page's route in headless Chrome over the DevTools protocol, and reports uncaught
//! errors, console errors, failed loads and error statuses. It keeps a screenshot of each
//! page.
//!
//! [`attribute`] sends each line of output to the page whose file or route it names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use crate::checks::Timing;
use tm_core::PageId;
use tm_verify::Owners;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_tungstenite::tungstenite::Message;

/// Output kept from a failed step: the end, where the errors are.
const KEEP: usize = 6000;

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub step: String,
    pub output: String,
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// What to run in `tree`, in order. A step whose tool is missing is left out.
pub fn steps(tree: &Path) -> Vec<Step> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(tree.join("package.json"))
        && let Ok(package) = serde_json::from_str::<Value>(&text)
        && tree.join("node_modules").exists()
    {
        let manager = if tree.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if tree.join("yarn.lock").is_file() {
            "yarn"
        } else if tree.join("bun.lockb").is_file() || tree.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        if on_path(manager) {
            for script in ["build", "test"] {
                let Some(body) = package["scripts"][script].as_str() else { continue };
                // What `npm init` writes: a test script that only fails.
                if body.contains("no test specified") {
                    continue;
                }
                out.push(Step { name: script.into(), program: manager.into(), args: vec!["run".into(), script.into()] });
            }
        }
    }
    if tree.join("Cargo.toml").is_file() && on_path("cargo") {
        out.push(Step { name: "test".into(), program: "cargo".into(), args: vec!["test".into(), "--quiet".into()] });
    }
    let python = ["pytest.ini", "pyproject.toml", "setup.cfg", "tox.ini"].iter().any(|f| tree.join(f).is_file())
        && (tree.join("tests").is_dir() || tree.join("test").is_dir());
    if python && on_path("pytest") {
        out.push(Step { name: "test".into(), program: "pytest".into(), args: vec!["-q".into(), "-x".into()] });
    }
    out
}

fn tail(text: &str) -> String {
    if text.len() <= KEEP {
        return text.to_string();
    }
    let mut start = text.len() - KEEP;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…\n{}", &text[start..])
}

/// Run the steps in order and stop at the first that fails: tests after a failed build
/// say nothing new.
pub async fn run_steps(tree: &Path, steps: &[Step], timeout: Duration) -> (Vec<Timing>, Option<Failure>) {
    let mut timings = Vec::new();
    for step in steps {
        let began = Instant::now();
        eprintln!("taskmaster: running `{} {}`", step.program, step.args.join(" "));
        let child = Command::new(&step.program)
            .args(&step.args)
            .current_dir(tree)
            .env("CI", "1")
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .env("FORCE_COLOR", "0")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                timings.push(Timing::new(&step.name, began.elapsed(), "failed"));
                return (timings, Some(Failure { step: step.name.clone(), output: format!("could not start {}: {err}", step.program) }));
            }
        };
        let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let read = async {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let _ = tokio::join!(out.read_to_end(&mut a), err.read_to_end(&mut b));
            let status = child.wait().await;
            (status, a, b)
        };
        match tokio::time::timeout(timeout, read).await {
            Ok((Ok(status), a, b)) if status.success() => {
                let _ = (a, b);
                timings.push(Timing::new(&step.name, began.elapsed(), "ok"));
            }
            Ok((_, a, b)) => {
                let text = format!("{}\n{}", String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
                timings.push(Timing::new(&step.name, began.elapsed(), "failed"));
                return (timings, Some(Failure { step: step.name.clone(), output: tail(text.trim()) }));
            }
            Err(_) => {
                timings.push(Timing::new(&step.name, began.elapsed(), "timeout"));
                return (
                    timings,
                    Some(Failure {
                        step: step.name.clone(),
                        output: format!("`{} {}` did not finish in {} seconds", step.program, step.args.join(" "), timeout.as_secs()),
                    }),
                );
            }
        }
    }
    (timings, None)
}

/// Paths in output, as `./app/x/page.tsx:12:5`, `app/x/page.tsx(12,5)` or `src/x.rs:3`.
pub fn paths_in(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in line.split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '(' | ')' | ',' | '<' | '>')) {
        let word = word.trim_start_matches("./").trim_end_matches(|c: char| c == ':' || c == '.' || c.is_ascii_digit());
        let word = word.split(':').next().unwrap_or("");
        let has_ext = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".css", ".rs", ".py"].iter().any(|ext| word.ends_with(ext));
        if has_ext && !word.starts_with('/') && !word.contains("node_modules") && !word.contains("://") {
            out.push(word.to_string());
        }
    }
    // A route group or segment in brackets is split off by the separators above.
    if let Some(start) = line.find("./") {
        let rest = &line[start + 2..];
        let end = rest.find(|c: char| c.is_whitespace() || c == ':' || c == '\'' || c == '"').unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.contains('.') && !out.iter().any(|have| have == candidate) {
            out.push(candidate.to_string());
        }
    }
    out
}

/// Which page each line of `output` is about, by a file it names or the route it names
/// (`prerendering page "/books"`). Lines that name no page are returned apart.
pub fn attribute(output: &str, owners: &Owners, routes: &[(PageId, String)]) -> (BTreeMap<PageId, String>, String) {
    let mut pages: BTreeMap<PageId, String> = BTreeMap::new();
    let mut rest = String::new();
    let mut current: Option<PageId> = None;
    for line in output.lines() {
        let by_file = paths_in(line).iter().find_map(|path| owners.owner(path).cloned());
        let by_route = routes.iter().find_map(|(page, route)| {
            let quoted = [format!("\"{route}\""), format!("'{route}'"), format!("page {route}"), format!("route {route}")];
            quoted.iter().any(|q| line.contains(q.as_str())).then(|| page.clone())
        });
        if let Some(page) = by_file.or(by_route) {
            current = Some(page);
        } else if line.trim().is_empty() {
            current = None;
        }
        match &current {
            Some(page) => {
                let text = pages.entry(page.clone()).or_default();
                text.push_str(line);
                text.push('\n');
            }
            None => {
                rest.push_str(line);
                rest.push('\n');
            }
        }
    }
    (pages, rest.trim().to_string())
}

// The browser.

/// A Chrome or Chromium to drive: `$TASKMASTER_CHROME`, or one on `PATH`.
pub fn chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TASKMASTER_CHROME").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    let paths = std::env::var_os("PATH")?;
    for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"] {
        for dir in std::env::split_paths(&paths) {
            let path = dir.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Whether `tree` is a Next.js app that has been built.
pub fn is_built_next(tree: &Path) -> bool {
    let next = tree.join("node_modules/.bin/next");
    let package = std::fs::read_to_string(tree.join("package.json")).unwrap_or_default();
    next.exists() && package.contains("\"next\"") && tree.join(".next/BUILD_ID").is_file()
}

#[derive(Debug, Default)]
pub struct Browsed {
    /// Errors per page, ready to hand to the page's owner.
    pub pages: BTreeMap<PageId, String>,
    pub screenshots: Vec<(PageId, Vec<u8>)>,
    /// What could not be checked, and why.
    pub notes: Vec<String>,
}

/// A route with its dynamic segments filled in, and whether it had any. `[id]` becomes
/// `1` and an optional catch-all is dropped. A dynamic page may answer 404 for a made-up
/// id, so that is not held against it.
pub fn sample_path(route: &str) -> (String, bool) {
    let mut dynamic = false;
    let mut parts = Vec::new();
    for segment in route.split('/') {
        if segment.starts_with("[[") {
            dynamic = true;
            continue;
        }
        if segment.starts_with('[') {
            dynamic = true;
            parts.push("1".to_string());
        } else if !(segment.starts_with('(') && segment.ends_with(')')) {
            parts.push(segment.to_string());
        }
    }
    let path = parts.join("/");
    let path = if path.starts_with('/') { path } else { format!("/{path}") };
    (path.replace("//", "/"), dynamic)
}

struct Server(tokio::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn free_port() -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}

/// Start the built app and wait until it answers.
async fn start_app(tree: &Path) -> anyhow::Result<(Server, u16)> {
    let port = free_port().await?;
    let mut child = Command::new(tree.join("node_modules/.bin/next"))
        .args(["start", "-p", &port.to_string(), "-H", "127.0.0.1"])
        .current_dir(tree)
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not start the app with `next start`")?;
    let stderr = child.stderr.take();
    let server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok((server, port));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut said = String::new();
    if let Some(mut stderr) = stderr {
        let _ = tokio::time::timeout(Duration::from_secs(1), stderr.read_to_string(&mut said)).await;
    }
    bail!("the app did not start in 60 seconds: {}", tail(said.trim()))
}

struct Browser {
    _process: tokio::process::Child,
    _profile: PathBuf,
    socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
    events: Vec<Value>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self._process.start_kill();
        let _ = std::fs::remove_dir_all(&self._profile);
    }
}

impl Browser {
    async fn launch(chrome: &Path) -> anyhow::Result<Browser> {
        let profile = std::env::temp_dir().join(format!("tm-chrome-{}-{}", std::process::id(), free_port().await?));
        let mut command = Command::new(chrome);
        command.args([
            "--headless=new",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--hide-scrollbars",
            "--window-size=1280,800",
            "--remote-debugging-port=0",
            "--remote-allow-origins=*",
        ]);
        command.arg(format!("--user-data-dir={}", profile.display()));
        // Chrome refuses its own sandbox as root, which is how containers run.
        if std::env::var("USER").as_deref() == Ok("root") {
            command.arg("--no-sandbox");
        }
        command.arg("about:blank");
        let mut process =
            command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true).spawn().context("could not start Chrome")?;
        let mut lines = BufReader::new(process.stderr.take().context("no stderr")?).lines();
        let url = tokio::time::timeout(Duration::from_secs(20), async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(at) = line.find("ws://") {
                    return Some(line[at..].trim().to_string());
                }
            }
            None
        })
        .await
        .ok()
        .flatten()
        .context("Chrome did not open its DevTools port")?;
        // Keep reading so Chrome never blocks on a full pipe.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str()).await.context("cannot talk to Chrome")?;
        Ok(Browser { _process: process, _profile: profile, socket, next_id: 0, events: Vec::new() })
    }

    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.socket.send(Message::Text(message.to_string().into())).await?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let next = tokio::time::timeout(left, self.socket.next()).await.with_context(|| format!("Chrome did not answer {method}"))?;
            let Some(message) = next else { bail!("Chrome closed the connection") };
            let Message::Text(text) = message? else { continue };
            let value: Value = serde_json::from_str(&text)?;
            if value["id"] == id {
                if let Some(error) = value.get("error") {
                    bail!("Chrome refused {method}: {error}");
                }
                return Ok(value["result"].clone());
            }
            if value.get("method").is_some() {
                self.events.push(value);
            }
        }
    }

    /// Collect events until `done` sees one, or the time is up.
    async fn wait_for(&mut self, within: Duration, done: impl Fn(&Value) -> bool) {
        if self.events.iter().any(&done) {
            return;
        }
        let deadline = Instant::now() + within;
        while let Ok(Some(Ok(message))) = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), self.socket.next()).await {
            let Message::Text(text) = message else { continue };
            let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
            let finished = done(&value);
            self.events.push(value);
            if finished {
                return;
            }
        }
    }
}

/// What went wrong on one page, from the events Chrome sent for its session.
fn problems(events: &[Value], session: &str, url: &str, dynamic: bool) -> Vec<String> {
    let mut out = Vec::new();
    for event in events.iter().filter(|e| e["sessionId"] == session) {
        let params = &event["params"];
        match event["method"].as_str().unwrap_or("") {
            "Network.responseReceived" if params["type"] == "Document" => {
                let status = params["response"]["status"].as_u64().unwrap_or(0);
                if status >= 400 && !(status == 404 && dynamic) {
                    out.push(format!("the page answered HTTP {status}"));
                }
            }
            "Network.loadingFailed" if params["type"] == "Document" => {
                out.push(format!("the page did not load: {}", params["errorText"].as_str().unwrap_or("unknown error")));
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let text = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("an exception");
                let at = details["url"].as_str().map(|u| format!(" at {u}:{}", details["lineNumber"])).unwrap_or_default();
                // The message and the first frame; the rest is the framework.
                out.push(format!("uncaught error{at}: {}", text.lines().take(2).collect::<Vec<_>>().join("\n  ")));
            }
            "Runtime.consoleAPICalled" if params["type"] == "error" => {
                let args: Vec<String> = params["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| a["value"].as_str().map(str::to_string).or(a["description"].as_str().map(str::to_string)).unwrap_or_default())
                    .collect();
                out.push(format!("console error: {}", args.join(" ").lines().take(6).collect::<Vec<_>>().join("\n  ")));
            }
            "Log.entryAdded" if params["entry"]["level"] == "error" => {
                let entry = &params["entry"];
                let source = entry["url"].as_str().unwrap_or("");
                // The page's own status is reported above. A browser asks for a favicon by
                // itself, and no page asked for one.
                let about_page = source.trim_end_matches('/') == url.trim_end_matches('/');
                if about_page || source.ends_with("/favicon.ico") {
                    continue;
                }
                let at = entry["url"].as_str().map(|u| format!(" ({u})")).unwrap_or_default();
                out.push(format!("browser error: {}{at}", entry["text"].as_str().unwrap_or("")));
            }
            _ => {}
        }
    }
    out.dedup();
    out
}

/// Open each page in Chrome. `None` when this is neither a built Next.js app nor a static
/// site, or there is no Chrome; the reason is printed.
pub async fn browse(tree: &Path, routes: &[(PageId, String)]) -> anyhow::Result<Option<Browsed>> {
    let next = is_built_next(tree);
    let static_start = if next || !crate::static_site::has_no_scripts(tree) { None } else { crate::static_site::start_page(tree) };
    if !next && static_start.is_none() {
        return Ok(None);
    }
    let Some(chrome) = chrome() else {
        eprintln!("taskmaster: no Chrome or Chromium found, so pages were not opened in a browser (set TASKMASTER_CHROME)");
        return Ok(None);
    };
    let mut browsed = Browsed::default();
    // A static site is served from here; a Next.js app is started.
    let (_server, port, _site) = if static_start.is_some() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let site = tokio::spawn(crate::static_site::serve(listener, tree.to_path_buf()));
        (None, port, Some(AbortOnDrop(site)))
    } else {
        match start_app(tree).await {
            Ok((server, port)) => (Some(server), port, None),
            Err(err) => {
                browsed.notes.push(format!("{err:#}"));
                return Ok(Some(browsed));
            }
        }
    };
    let mut browser = Browser::launch(&chrome).await?;
    // A static site has no routes of its own: its start page stands for the first page.
    let static_routes: Vec<(PageId, String)> = match static_start {
        Some(start) => routes.first().map(|(page, _)| (page.clone(), format!("/{start}"))).into_iter().collect(),
        None => Vec::new(),
    };
    let routes = if static_start.is_some() { &static_routes[..] } else { routes };
    for (page, route) in routes {
        let (path, dynamic) = sample_path(route);
        let url = format!("http://127.0.0.1:{port}{path}");
        let target = browser.call("Target.createTarget", json!({ "url": "about:blank" }), None).await?;
        let target_id = target["targetId"].as_str().context("no target")?.to_string();
        let attached = browser.call("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None).await?;
        let session = attached["sessionId"].as_str().context("no session")?.to_string();
        for domain in ["Page.enable", "Runtime.enable", "Log.enable", "Network.enable"] {
            browser.call(domain, json!({}), Some(&session)).await?;
        }
        browser.events.clear();
        browser.call("Page.navigate", json!({ "url": url }), Some(&session)).await?;
        let mine = session.clone();
        browser.wait_for(Duration::from_secs(20), move |e| e["sessionId"] == mine.as_str() && e["method"] == "Page.loadEventFired").await;
        // Errors from hydration and effects come just after the load.
        browser.wait_for(Duration::from_millis(1500), |_| false).await;
        if let Ok(shot) = browser.call("Page.captureScreenshot", json!({ "format": "png" }), Some(&session)).await
            && let Some(data) = shot["data"].as_str()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
        {
            browsed.screenshots.push((page.clone(), bytes));
        }
        let found = problems(&browser.events, &session, &url, dynamic);
        if !found.is_empty() {
            let text = found.iter().map(|p| format!("In the browser at {path}: {p}")).collect::<Vec<_>>().join("\n");
            browsed.pages.insert(page.clone(), text);
        }
        let _ = browser.call("Target.closeTarget", json!({ "targetId": target_id }), None).await;
    }
    Ok(Some(browsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{Estimate, PagePlan, Plan};

    fn owners() -> Owners {
        let page = |id: &str, glob: &str| PagePlan {
            id: id.into(),
            route: format!("/{id}"),
            owned_glob: glob.into(),
            cites: vec![],
            brief: String::new(),
            done: String::new(),
            units: vec![],
        };
        Owners::from_plan(&Plan {
            contracts: vec![],
            pages: vec![page("book", "app/books/**"), page("cart", "app/cart/**")],
            estimate: Estimate { tokens: 0, usd: 0.0 },
        })
        .unwrap()
    }

    #[test]
    fn build_output_goes_to_the_page_whose_file_or_route_it_names() {
        let output = "Creating an optimized production build ...\n\
                      Failed to compile.\n\
                      \n\
                      ./app/books/[id]/page.tsx:12:5\n\
                      Type error: Property 'title' does not exist on type 'Book'.\n\
                      \n\
                      Error occurred prerendering page \"/cart\". Read more\n\
                      TypeError: Cannot read properties of undefined (reading 'map')\n\
                      \n\
                      next.config.mjs is invalid";
        let routes = vec![("book".to_string(), "/books/[id]".to_string()), ("cart".to_string(), "/cart".to_string())];
        let (pages, rest) = attribute(output, &owners(), &routes);
        assert!(pages["book"].contains("Property 'title'"), "{pages:?}");
        assert!(pages["cart"].contains("reading 'map'"), "{pages:?}");
        assert!(rest.contains("next.config.mjs is invalid") && !rest.contains("title"), "{rest}");
        let (none, _) = attribute("src/app/books/page.tsx(3,1): error", &owners(), &routes);
        assert!(none.is_empty(), "a path outside every page's files goes to nobody");
    }

    #[test]
    fn a_dynamic_route_is_opened_with_a_sample_id() {
        assert_eq!(sample_path("/books/[id]"), ("/books/1".into(), true));
        assert_eq!(sample_path("/"), ("/".into(), false));
        assert_eq!(sample_path("/docs/[[...slug]]"), ("/docs".into(), true));
        assert_eq!(sample_path("/(shop)/cart"), ("/cart".into(), false));
    }

    #[test]
    fn browser_events_become_problems_for_that_page_only() {
        let s = "S1";
        let events = vec![
            json!({ "sessionId": s, "method": "Network.responseReceived", "params": { "type": "Document", "response": { "status": 500 } } }),
            json!({ "sessionId": s, "method": "Runtime.exceptionThrown", "params": { "exceptionDetails": { "text": "Uncaught", "url": "http://x/app.js", "lineNumber": 3, "exception": { "description": "TypeError: books.map is not a function" } } } }),
            json!({ "sessionId": s, "method": "Runtime.consoleAPICalled", "params": { "type": "error", "args": [{ "value": "Warning: Each child needs a key" }] } }),
            json!({ "sessionId": s, "method": "Runtime.consoleAPICalled", "params": { "type": "log", "args": [{ "value": "fine" }] } }),
            json!({ "sessionId": "other", "method": "Runtime.exceptionThrown", "params": { "exceptionDetails": { "text": "elsewhere" } } }),
        ];
        let found = problems(&events, s, "http://x/books", false);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("HTTP 500") && found[1].contains("books.map") && found[2].contains("key"));

        let missing = vec![
            json!({ "sessionId": s, "method": "Network.responseReceived", "params": { "type": "Document", "response": { "status": 404 } } }),
            json!({ "sessionId": s, "method": "Log.entryAdded", "params": { "entry": { "level": "error", "text": "Failed to load resource: 404", "url": "http://x/books/1" } } }),
        ];
        assert!(problems(&missing, s, "http://x/books/1", true).is_empty(), "a made-up id may be missing");
        assert_eq!(problems(&missing, s, "http://x/books/1", false), vec!["the page answered HTTP 404".to_string()]);
        let favicon = vec![json!({ "sessionId": s, "method": "Log.entryAdded", "params": { "entry": { "level": "error", "text": "404", "url": "http://x/favicon.ico" } } })];
        assert!(problems(&favicon, s, "http://x/", false).is_empty());
    }

    #[tokio::test]
    async fn the_projects_own_scripts_run_and_a_failure_keeps_its_output() {
        let dir = std::env::temp_dir().join(format!("tm-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = Step { name: "build".into(), program: "sh".into(), args: vec!["-c".into(), "echo built".into()] };
        let bad = Step { name: "test".into(), program: "sh".into(), args: vec!["-c".into(), "echo 'app/cart/page.tsx:1 boom' >&2; exit 1".into()] };
        let never = Step { name: "after".into(), program: "sh".into(), args: vec!["-c".into(), "touch ran".into()] };
        let (timed, failure) = run_steps(&dir, &[ok, bad, never], Duration::from_secs(10)).await;
        let failure = failure.unwrap();
        assert_eq!(timed.iter().map(|t| (t.name.as_str(), t.status)).collect::<Vec<_>>(), vec![("build", "ok"), ("test", "failed")]);
        assert_eq!(failure.step, "test");
        assert!(failure.output.contains("boom"));
        assert!(!dir.join("ran").exists(), "nothing runs after a failure");
        let slow = Step { name: "build".into(), program: "sleep".into(), args: vec!["5".into()] };
        let (timed, slow_failure) = run_steps(&dir, &[slow], Duration::from_millis(200)).await;
        assert!(slow_failure.unwrap().output.contains("did not finish"));
        assert_eq!(timed[0].status, "timeout");

        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"next build","test":"echo \"Error: no test specified\" && exit 1"}}"#).unwrap();
        assert!(steps(&dir).is_empty(), "nothing is installed yet");
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let found = steps(&dir);
        if on_path("npm") {
            assert_eq!(found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["build"]);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Opens a real app in Chrome when `TM_BROWSER_TEST_APP` names a built Next.js app with
    /// the pages `/`, `/broken` (throws after hydration), `/noisy` (logs an error) and
    /// `/books/[id]` (404 for an unknown id). Skipped otherwise.
    #[tokio::test]
    async fn a_real_app_is_opened_and_each_page_gets_its_own_problems() {
        let Some(app) = std::env::var_os("TM_BROWSER_TEST_APP").map(PathBuf::from) else {
            eprintln!("TM_BROWSER_TEST_APP is not set; skipping");
            return;
        };
        let routes: Vec<(PageId, String)> = [("home", "/"), ("broken", "/broken"), ("noisy", "/noisy"), ("book", "/books/[id]"), ("gone", "/gone")]
            .iter()
            .map(|(p, r)| (p.to_string(), r.to_string()))
            .collect();
        let browsed = browse(&app, &routes).await.unwrap().expect("a built app and a Chrome");
        assert!(browsed.notes.is_empty(), "{:?}", browsed.notes);
        assert_eq!(browsed.screenshots.len(), 5);
        assert!(browsed.screenshots.iter().all(|(_, png)| png.starts_with(b"\x89PNG")));
        assert!(!browsed.pages.contains_key("home"), "{:?}", browsed.pages);
        assert!(!browsed.pages.contains_key("book"), "a made-up id may be missing: {:?}", browsed.pages);
        assert!(browsed.pages["broken"].contains("map"), "{:?}", browsed.pages);
        assert!(browsed.pages["noisy"].contains("cart total is NaN"), "{:?}", browsed.pages);
        assert!(browsed.pages["gone"].contains("HTTP 404"), "{:?}", browsed.pages);
        if let Some(dir) = std::env::var_os("TM_BROWSER_TEST_SHOTS") {
            for (page, png) in &browsed.screenshots {
                std::fs::write(PathBuf::from(&dir).join(format!("{page}.png")), png).unwrap();
            }
        }
        for (page, text) in &browsed.pages {
            eprintln!("{page}:\n{text}");
        }
    }
}
