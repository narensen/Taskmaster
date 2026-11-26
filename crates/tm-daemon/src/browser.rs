//! Chrome over the DevTools protocol, and the browser tool workers use to look at their
//! own page before they say they are done.
//!
//! Each page being built gets its own site: its worktree served on a port of its own,
//! by the built-in static server or by the project's dev script, and its own tab. The tab
//! is held to that site's origin twice over: every request the page makes is paused and
//! anything for another origin is failed (Fetch domain), and the tools only take a path
//! on the site. The permission gate allows the tool on that origin only.
//!
//! Tools, served beside the Dictionary under the page's token: `browser_open`,
//! `browser_click`, `browser_type`, `browser_screenshot` and `browser_console`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tm_dictionary::{Extension, Grant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_tungstenite::tungstenite::Message;

use crate::runtime::{free_port, problems};

pub struct Browser {
    process: tokio::process::Child,
    profile: PathBuf,
    socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
    pub events: Vec<Value>,
    /// Sessions held to one origin, and what each was stopped from loading.
    guards: HashMap<String, String>,
    blocked: HashMap<String, Vec<String>>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

impl Browser {
    pub async fn launch(chrome: &Path) -> anyhow::Result<Browser> {
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
        Ok(Browser { process, profile, socket, next_id: 0, events: Vec::new(), guards: HashMap::new(), blocked: HashMap::new() })
    }

    async fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> anyhow::Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.socket.send(Message::Text(message.to_string().into())).await?;
        Ok(id)
    }

    pub async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> anyhow::Result<Value> {
        let id = self.send(method, params, session).await?;
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
                self.event(value).await;
            }
        }
    }

    /// Collect events until `done` sees one, or the time is up.
    pub async fn wait_for(&mut self, within: Duration, done: impl Fn(&Value) -> bool) {
        if self.events.iter().any(&done) {
            return;
        }
        let deadline = Instant::now() + within;
        while let Ok(Some(Ok(message))) = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), self.socket.next()).await {
            let Message::Text(text) = message else { continue };
            let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
            if value.get("method").is_none() {
                continue;
            }
            let finished = done(&value);
            self.event(value).await;
            if finished {
                return;
            }
        }
    }

    /// Keep an event, answering a paused request at once: this origin goes on, any other
    /// is failed and noted.
    async fn event(&mut self, value: Value) {
        if value["method"] == "Fetch.requestPaused" {
            let session = value["sessionId"].as_str().unwrap_or_default().to_string();
            let id = value["params"]["requestId"].clone();
            let url = value["params"]["request"]["url"].as_str().unwrap_or_default().to_string();
            let allowed = match self.guards.get(&session) {
                Some(origin) => allowed_url(origin, &url),
                None => true,
            };
            let _ = if allowed {
                self.send("Fetch.continueRequest", json!({ "requestId": id }), Some(&session)).await
            } else {
                self.blocked.entry(session.clone()).or_default().push(url);
                self.send("Fetch.failRequest", json!({ "requestId": id, "errorReason": "BlockedByClient" }), Some(&session)).await
            };
            return;
        }
        self.events.push(value);
    }

    /// Hold a session to one origin: every other request it makes fails.
    pub async fn guard(&mut self, session: &str, origin: &str) -> anyhow::Result<()> {
        self.guards.insert(session.to_string(), origin.to_string());
        self.call("Fetch.enable", json!({ "patterns": [{ "urlPattern": "*" }] }), Some(session)).await?;
        Ok(())
    }

    fn take_blocked(&mut self, session: &str) -> Vec<String> {
        self.blocked.remove(session).unwrap_or_default()
    }

    fn forget(&mut self, session: &str) {
        self.events.retain(|e| e["sessionId"] != session);
    }
}

/// Whether a page held to `origin` may load `url`.
pub fn allowed_url(origin: &str, url: &str) -> bool {
    if url.starts_with("data:") || url.starts_with("blob:") || url == "about:blank" {
        return true;
    }
    tm_harness::gate::same_origin(origin, url) && !url.starts_with('/')
}

/// How a page's site is served.
enum Served {
    /// The port is held (by the listener) until the dev script starts on the first visit.
    Reserved(#[allow(dead_code)] TcpListener),
    Static(tokio::task::AbortHandle),
    /// The dev script; dropping the guard stops it and what it started.
    Dev(#[allow(dead_code)] tokio::process::Child, #[allow(dead_code)] tm_harness::GroupGuard),
    Failed(String),
}

impl Drop for Served {
    fn drop(&mut self) {
        if let Served::Static(task) = self {
            task.abort();
        }
    }
}

struct Site {
    root: PathBuf,
    origin: String,
    port: u16,
    served: tokio::sync::Mutex<Served>,
    tab: tokio::sync::Mutex<Option<(String, String)>>,
    /// The address last opened, for `browser_console`.
    last: Mutex<String>,
}

/// Every page's site and tab for one run. Chrome starts on the first tool call.
pub struct PageBrowsers {
    chrome: PathBuf,
    project: PathBuf,
    browser: tokio::sync::Mutex<Option<Browser>>,
    sites: Mutex<HashMap<String, Arc<Site>>>,
}

const TOOL_NAMES: [&str; 5] = ["browser_open", "browser_click", "browser_type", "browser_screenshot", "browser_console"];

impl PageBrowsers {
    pub fn new(chrome: PathBuf, project: PathBuf) -> Arc<PageBrowsers> {
        Arc::new(PageBrowsers { chrome, project, browser: tokio::sync::Mutex::new(None), sites: Mutex::new(HashMap::new()) })
    }

    /// Serve `root` for `page` on a port of its own, and return its origin. A page that is
    /// attached again (a repair, in a new worktree) gets a new site.
    pub fn attach(self: &Arc<Self>, page: &str, root: &Path) -> anyhow::Result<String> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let port = listener.local_addr()?.port();
        let origin = format!("http://127.0.0.1:{port}");
        let served = if crate::preview::dev_command(root, "", port).is_some() {
            Served::Reserved(listener)
        } else {
            let task = tokio::spawn(crate::static_site::serve(listener, root.to_path_buf()));
            Served::Static(task.abort_handle())
        };
        let site = Arc::new(Site {
            root: root.to_path_buf(),
            origin: origin.clone(),
            port,
            served: tokio::sync::Mutex::new(served),
            tab: tokio::sync::Mutex::new(None),
            last: Mutex::new(String::new()),
        });
        let old = self.sites.lock().unwrap().insert(page.to_string(), site);
        if let Some(old) = old {
            let this = self.clone();
            tokio::spawn(async move { this.close(old).await });
        }
        Ok(origin)
    }

    /// Stop `page`'s site and close its tab.
    pub fn detach(self: &Arc<Self>, page: &str) {
        let old = self.sites.lock().unwrap().remove(page);
        if let Some(old) = old {
            // The dev server's link to the project's packages is not the agent's work: take
            // it out before the worktree is judged.
            let link = old.root.join("node_modules");
            if std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = std::fs::remove_file(&link);
            }
            let this = self.clone();
            tokio::spawn(async move { this.close(old).await });
        }
    }

    /// A screenshot of `page`'s tab as the worker left it, when it opened one.
    pub async fn snapshot(&self, page: &str) -> Option<Vec<u8>> {
        use base64::Engine;
        let site = self.site(page)?;
        let session = site.tab.lock().await.as_ref().map(|(_, session)| session.clone())?;
        let mut slot = self.browser.lock().await;
        let browser = slot.as_mut()?;
        let shot = browser.call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 60 }), Some(&session)).await.ok()?;
        base64::engine::general_purpose::STANDARD.decode(shot["data"].as_str()?).ok()
    }

    /// Whether `page` has a site now.
    #[cfg(test)]
    pub fn attached(&self, page: &str) -> bool {
        self.sites.lock().unwrap().contains_key(page)
    }

    async fn close(&self, site: Arc<Site>) {
        let tab = site.tab.lock().await.take();
        if let Some((target, session)) = tab
            && let Some(browser) = self.browser.lock().await.as_mut()
        {
            browser.guards.remove(&session);
            browser.forget(&session);
            let _ = browser.call("Target.closeTarget", json!({ "targetId": target }), None).await;
        }
    }

    fn site(&self, page: &str) -> Option<Arc<Site>> {
        self.sites.lock().unwrap().get(page).cloned()
    }

    /// Start the dev script on the site's port if it is not running yet.
    async fn ready(&self, site: &Site) -> anyhow::Result<()> {
        let mut served = site.served.lock().await;
        match &*served {
            Served::Static(_) | Served::Dev(..) => return Ok(()),
            Served::Failed(why) => bail!("the page's dev server did not start: {why}"),
            Served::Reserved(_) => {}
        }
        let command = crate::preview::dev_command(&site.root, "", site.port).context("no dev script")?;
        crate::git::borrow_node_modules(&self.project, &site.root);
        // The dev server binds the port itself.
        *served = Served::Failed("it was starting".into());
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&site.root)
            .env("PORT", site.port.to_string())
            .env("HOST", "127.0.0.1")
            .env("BROWSER", "none")
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, guard) = tm_harness::spawn_grouped(&mut cmd).with_context(|| format!("could not start `{command}`"))?;
        let log = Arc::new(Mutex::new(String::new()));
        for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>), child.stderr.take().map(|p| Box::new(p) as _)]
            .into_iter()
            .flatten()
        {
            let log = log.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut log = log.lock().unwrap();
                    if log.len() < 16 * 1024 {
                        log.push_str(&line);
                        log.push('\n');
                    }
                }
            });
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if tokio::net::TcpStream::connect(("127.0.0.1", site.port)).await.is_ok() {
                *served = Served::Dev(child, guard);
                return Ok(());
            }
            if let Ok(Some(status)) = child.try_wait() {
                let said = log.lock().unwrap().lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                let why = format!("`{command}` exited ({status}): {said}");
                *served = Served::Failed(why.clone());
                bail!("{why}");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        *served = Served::Failed(format!("`{command}` did not answer within 60 seconds"));
        bail!("`{command}` did not answer within 60 seconds")
    }

    /// The page's tab, opened and held to its origin on first use.
    async fn tab<'a>(&self, site: &Site, browser: &'a mut Option<Browser>) -> anyhow::Result<(&'a mut Browser, String)> {
        if browser.is_none() {
            *browser = Some(Browser::launch(&self.chrome).await?);
        }
        let browser = browser.as_mut().expect("launched above");
        let mut tab = site.tab.lock().await;
        if let Some((_, session)) = tab.as_ref() {
            return Ok((browser, session.clone()));
        }
        let target = browser.call("Target.createTarget", json!({ "url": "about:blank" }), None).await?;
        let target = target["targetId"].as_str().context("no target")?.to_string();
        let attached = browser.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None).await?;
        let session = attached["sessionId"].as_str().context("no session")?.to_string();
        browser.guard(&session, &site.origin).await?;
        for domain in ["Page.enable", "Runtime.enable", "Log.enable", "Network.enable"] {
            browser.call(domain, json!({}), Some(&session)).await?;
        }
        *tab = Some((target, session.clone()));
        Ok((browser, session))
    }

    async fn run(&self, site: &Site, name: &str, args: &Value) -> anyhow::Result<Value> {
        if name == "browser_open" {
            self.ready(site).await?;
        }
        let mut slot = self.browser.lock().await;
        let (browser, session) = self.tab(site, &mut slot).await?;
        match name {
            "browser_open" => {
                let path = args["path"].as_str().or_else(|| args["url"].as_str()).unwrap_or("/");
                let url = site_url(&site.origin, path)?;
                browser.forget(&session);
                browser.take_blocked(&session);
                browser.call("Page.navigate", json!({ "url": url }), Some(&session)).await?;
                let mine = session.clone();
                browser.wait_for(Duration::from_secs(20), move |e| e["sessionId"] == mine.as_str() && e["method"] == "Page.loadEventFired").await;
                browser.wait_for(Duration::from_millis(800), |_| false).await;
                *site.last.lock().unwrap() = url.clone();
                let status = browser
                    .events
                    .iter()
                    .rev()
                    .find(|e| e["sessionId"] == session.as_str() && e["method"] == "Network.responseReceived" && e["params"]["type"] == "Document")
                    .and_then(|e| e["params"]["response"]["status"].as_u64());
                let page = evaluate(browser, &session, PAGE_SUMMARY).await?;
                let errors = problems(&browser.events, &session, &url, false).len() + browser.blocked.get(&session).map_or(0, Vec::len);
                Ok(text(format!(
                    "Opened {url}{}\nTitle: {}\n{}\n\n{} problem{} so far; browser_console lists them.",
                    status.map(|s| format!(" (HTTP {s})")).unwrap_or_default(),
                    page["title"].as_str().unwrap_or(""),
                    page["text"].as_str().unwrap_or(""),
                    errors,
                    if errors == 1 { "" } else { "s" },
                )))
            }
            "browser_click" => {
                let found = evaluate(browser, &session, &find_script(args, "text")?).await?;
                let (Some(x), Some(y)) = (found["x"].as_f64(), found["y"].as_f64()) else {
                    return Ok(error(found["error"].as_str().unwrap_or("nothing matched").to_string()));
                };
                for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
                    browser
                        .call("Input.dispatchMouseEvent", json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }), Some(&session))
                        .await?;
                }
                browser.wait_for(Duration::from_millis(600), |_| false).await;
                let now = evaluate(browser, &session, "({ url: location.href })").await?;
                Ok(text(format!("Clicked {}. Now at {}.", found["label"].as_str().unwrap_or("the element"), now["url"].as_str().unwrap_or(""))))
            }
            "browser_type" => {
                let typed = args["text"].as_str().context("`text` is required")?;
                let found = evaluate(browser, &session, &find_script(args, "label")?.replace("/*FOCUS*/", "el.focus(); if (el.select) el.select();")).await?;
                if found["x"].is_null() {
                    return Ok(error(found["error"].as_str().unwrap_or("nothing matched").to_string()));
                }
                browser.call("Input.insertText", json!({ "text": typed }), Some(&session)).await?;
                if args["submit"].as_bool() == Some(true) {
                    for kind in ["keyDown", "keyUp"] {
                        browser
                            .call(
                                "Input.dispatchKeyEvent",
                                json!({ "type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": if kind == "keyDown" { "\r" } else { "" } }),
                                Some(&session),
                            )
                            .await?;
                    }
                }
                browser.wait_for(Duration::from_millis(400), |_| false).await;
                Ok(text(format!("Typed into {}.", found["label"].as_str().unwrap_or("the field"))))
            }
            "browser_screenshot" => {
                let full = args["full_page"].as_bool() == Some(true);
                let shot = browser
                    .call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": 60, "captureBeyondViewport": full }), Some(&session))
                    .await?;
                let data = shot["data"].as_str().context("Chrome returned no image")?;
                Ok(json!({ "content": [{ "type": "image", "data": data, "mimeType": "image/jpeg" }], "isError": false }))
            }
            "browser_console" => {
                browser.wait_for(Duration::from_millis(200), |_| false).await;
                let url = site.last.lock().unwrap().clone();
                let mut found = problems(&browser.events, &session, &url, false);
                for blocked in browser.blocked.get(&session).into_iter().flatten() {
                    found.push(format!("blocked a request to another origin: {blocked}"));
                }
                Ok(text(if found.is_empty() { "No console errors, failed loads or blocked requests.".to_string() } else { found.join("\n") }))
            }
            other => bail!("unknown browser tool `{other}`"),
        }
    }
}

const PAGE_SUMMARY: &str = r#"(() => {
  const text = (document.body ? document.body.innerText : "").replace(/\n{3,}/g, "\n\n").slice(0, 1500);
  const seen = new Set();
  const controls = [...document.querySelectorAll("a[href], button, input, select, textarea, [role=button]")]
    .filter((el) => el.offsetParent !== null)
    .slice(0, 30)
    .map((el) => {
      const label = (el.getAttribute("aria-label") || el.innerText || el.value || el.placeholder || el.name || el.id || "").trim().slice(0, 40);
      const tag = el.tagName.toLowerCase() + (el.type ? `[${el.type}]` : "");
      return `${tag} "${label}"`;
    })
    .filter((line) => !seen.has(line) && seen.add(line));
  return { title: document.title, text: `Text:\n${text}\n\nControls:\n${controls.join("\n") || "none"}` };
})()"#;

/// A script that finds the element `args` names (`selector`, or visible `text`) and returns
/// its centre, after scrolling it into view.
/// `key` is the argument that names the element by its text: `text` for a click, `label`
/// for typing (where `text` is what is typed).
fn find_script(args: &Value, key: &str) -> anyhow::Result<String> {
    let selector = args["selector"].as_str();
    let wanted = args[key].as_str().or(args["label"].as_str());
    if selector.is_none() && wanted.is_none() {
        bail!("give `selector` or `{key}`");
    }
    Ok(format!(
        r#"(() => {{
  const selector = {selector};
  const wanted = {wanted};
  let el = null;
  if (selector) el = document.querySelector(selector);
  else {{
    const all = [...document.querySelectorAll("a, button, input, select, textarea, label, [role=button], [role=link], [tabindex]")];
    const norm = (s) => (s || "").replace(/\s+/g, " ").trim().toLowerCase();
    const w = norm(wanted);
    el = all.find((e) => norm(e.innerText || e.value || e.getAttribute("aria-label") || e.placeholder) === w)
      || all.find((e) => norm(e.innerText || e.value || e.getAttribute("aria-label") || e.placeholder).includes(w));
  }}
  if (!el) return {{ error: selector ? `no element matches ${{selector}}` : `nothing says "${{wanted}}"` }};
  el.scrollIntoView({{ block: "center" }});
  /*FOCUS*/
  const r = el.getBoundingClientRect();
  return {{ x: r.left + r.width / 2, y: r.top + r.height / 2, label: `${{el.tagName.toLowerCase()}} "${{(el.innerText || el.value || el.placeholder || "").trim().slice(0, 40)}}"` }};
}})()"#,
        selector = serde_json::to_string(&selector)?,
        wanted = serde_json::to_string(&wanted)?,
    ))
}

async fn evaluate(browser: &mut Browser, session: &str, script: &str) -> anyhow::Result<Value> {
    let result = browser.call("Runtime.evaluate", json!({ "expression": script, "returnByValue": true, "awaitPromise": true }), Some(session)).await?;
    if let Some(details) = result.get("exceptionDetails") {
        bail!("the page threw: {}", details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("an error"));
    }
    Ok(result["result"]["value"].clone())
}

/// The address on `origin` for `path`. A full address must be on the origin.
pub fn site_url(origin: &str, path: &str) -> anyhow::Result<String> {
    if path.starts_with('/') && !path.starts_with("//") {
        return Ok(format!("{origin}{path}"));
    }
    if tm_harness::gate::same_origin(origin, path) {
        return Ok(path.to_string());
    }
    bail!("only this page's preview can be opened: give a path such as `/` (it is served at {origin})")
}

fn text(body: String) -> Value {
    json!({ "content": [{ "type": "text", "text": body }], "isError": false })
}

fn error(body: String) -> Value {
    json!({ "content": [{ "type": "text", "text": body }], "isError": true })
}

#[async_trait::async_trait]
impl Extension for PageBrowsers {
    fn tools(&self, grant: &Grant) -> Vec<Value> {
        let Some(site) = self.site(&grant.page) else { return Vec::new() };
        let on = format!("Only {} (this page's own preview) can be opened.", site.origin);
        let path = json!({ "type": "object", "properties": { "path": { "type": "string", "description": "A path on the preview, such as /" } }, "required": ["path"] });
        let target = |extra: Value| {
            let mut props = json!({
                "selector": { "type": "string", "description": "A CSS selector" },
                "text": { "type": "string", "description": "Visible text or label of the element, when there is no selector" },
            });
            if let (Some(props), Some(extra)) = (props.as_object_mut(), extra.as_object()) {
                props.extend(extra.clone());
            }
            json!({ "type": "object", "properties": props })
        };
        vec![
            json!({ "name": "browser_open", "description": format!("Open a path of the page you are building in a real browser. Returns its title, visible text and controls. {on}"), "inputSchema": path }),
            json!({ "name": "browser_click", "description": "Click an element on the open page, found by CSS selector or by its visible text.", "inputSchema": target(json!({})) }),
            json!({ "name": "browser_type", "description": "Type into a field on the open page. `submit` presses Enter after.", "inputSchema": target(json!({ "text": { "type": "string", "description": "What to type" }, "label": { "type": "string", "description": "Visible label or placeholder of the field, when there is no selector" }, "submit": { "type": "boolean" } })) }),
            json!({ "name": "browser_screenshot", "description": "A screenshot of the open page, to check how it looks.", "inputSchema": { "type": "object", "properties": { "full_page": { "type": "boolean" } } } }),
            json!({ "name": "browser_console", "description": "Console errors, uncaught exceptions, failed loads and blocked requests since the page was opened.", "inputSchema": { "type": "object", "properties": {} } }),
        ]
    }

    async fn call(&self, grant: &Grant, name: &str, args: &Value) -> Option<Value> {
        let canonical = crate::code::strip_dict_prefix(name);
        if !TOOL_NAMES.contains(&canonical) {
            return None;
        }
        let Some(site) = self.site(&grant.page) else {
            return Some(error("there is no preview for this agent".into()));
        };
        Some(match self.run(&site, canonical, args).await {
            Ok(value) => value,
            Err(err) => error(format!("{err:#}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>Pantry</title></head><body>
<h1 id="count">0 jars</h1>
<input id="name" placeholder="Jar name">
<button onclick="document.getElementById('count').textContent = document.getElementById('name').value + ' added'; console.error('shelf is full')">Add jar</button>
<img src="https://images.example.com/jar.png" alt="">
</body></html>"#;

    fn text_of(reply: &Value) -> String {
        reply["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    #[tokio::test]
    async fn a_worker_opens_clicks_types_and_reads_errors_on_its_own_page_only() {
        let Some(chrome) = crate::runtime::chrome() else {
            eprintln!("no Chrome here; the browser tool is not exercised");
            return;
        };
        let dir = std::env::temp_dir().join(format!("tm-browser-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), PAGE).unwrap();
        let browsers = PageBrowsers::new(chrome, dir.clone());
        let mine = Grant { run: "r".into(), page: "pantry".into(), unit: String::new() };
        let other = Grant { run: "r".into(), page: "elsewhere".into(), unit: String::new() };
        let origin = browsers.attach("pantry", &dir).unwrap();
        assert!(browsers.attached("pantry"));
        assert_eq!(browsers.tools(&mine).len(), 5);
        assert!(browsers.tools(&other).is_empty(), "a page with no preview gets no browser");
        assert!(browsers.call(&mine, "cite", &json!({})).await.is_none(), "not a browser tool");

        let opened = browsers.call(&mine, "browser_open", &json!({ "path": "/index.html" })).await.unwrap();
        let said = text_of(&opened);
        assert!(said.contains("Title: Pantry") && said.contains("0 jars") && said.contains("button[submit] \"Add jar\""), "{said}");
        assert!(said.contains(&origin));

        let typed = browsers.call(&mine, "browser_type", &json!({ "label": "Jar name", "text": "Quince" })).await.unwrap();
        assert_eq!(typed["isError"], false, "{typed}");
        let clicked = browsers.call(&mine, "browser_click", &json!({ "text": "Add jar" })).await.unwrap();
        assert_eq!(clicked["isError"], false, "{clicked}");
        let reread = browsers.call(&mine, "browser_open", &json!({ "path": "/index.html" })).await.unwrap();
        assert!(text_of(&reread).contains("0 jars"), "opening again reloads the page");
        browsers.call(&mine, "browser_type", &json!({ "selector": "#name", "text": "Fig" })).await.unwrap();
        browsers.call(&mine, "browser_click", &json!({ "selector": "button" })).await.unwrap();
        let console = text_of(&browsers.call(&mine, "browser_console", &json!({})).await.unwrap());
        assert!(console.contains("shelf is full"), "{console}");
        assert!(console.contains("blocked a request to another origin: https://images.example.com/jar.png"), "{console}");

        let shot = browsers.call(&mine, "browser_screenshot", &json!({})).await.unwrap();
        assert_eq!(shot["content"][0]["mimeType"], "image/jpeg");
        assert!(shot["content"][0]["data"].as_str().unwrap().len() > 1000);

        for elsewhere in ["https://example.com/", "http://127.0.0.1:1/", "//example.com/x"] {
            let refused = browsers.call(&mine, "browser_open", &json!({ "path": elsewhere })).await.unwrap();
            assert_eq!(refused["isError"], true, "{elsewhere}");
            assert!(text_of(&refused).contains("only this page's preview"), "{refused}");
        }
        let missing = browsers.call(&mine, "browser_click", &json!({ "text": "No such button" })).await.unwrap();
        assert_eq!(missing["isError"], true);

        browsers.detach("pantry");
        assert!(!browsers.attached("pantry"));
        assert_eq!(browsers.call(&mine, "browser_open", &json!({ "path": "/" })).await.unwrap()["isError"], true);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn only_the_site_s_own_addresses_open() {
        let origin = "http://127.0.0.1:5000";
        assert_eq!(site_url(origin, "/cart").unwrap(), "http://127.0.0.1:5000/cart");
        assert_eq!(site_url(origin, "http://127.0.0.1:5000/x?y").unwrap(), "http://127.0.0.1:5000/x?y");
        for bad in ["https://example.com/", "//example.com", "http://127.0.0.1:8737/api/runs", "http://127.0.0.1:5000@evil.test/", "file:///etc/passwd", "javascript:alert(1)"] {
            assert!(site_url(origin, bad).is_err(), "{bad}");
        }
        assert!(allowed_url(origin, "http://127.0.0.1:5000/app.js"));
        assert!(allowed_url(origin, "data:image/png;base64,AAAA"));
        assert!(!allowed_url(origin, "https://fonts.example.com/a.woff2"));
        assert!(!allowed_url(origin, "http://localhost:5000/"));
        assert!(!allowed_url(origin, "/relative"));
    }
}
