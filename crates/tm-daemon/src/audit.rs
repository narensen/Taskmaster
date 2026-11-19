//! The free UI checks every built page gets in headless Chrome, after the smoke check has
//! read its errors. They cost no tokens and find what a person would see at once:
//!
//! - `h1`: no visible `<h1>`;
//! - `label`: a form control or an icon-only button with no label;
//! - `controls`: a native checkbox stretched by a text-input rule, left at the browser's
//!   13px, or a control on the browser's default 13.3px font;
//! - `focus`: an element the keyboard reaches that does not change when focused;
//! - `narrow`: sideways scroll at 360px wide;
//! - `empty_state`: the empty-state message still shown after an item is added;
//! - `blank_input`: a blank entry added from the page's own form;
//! - `html_escape`: text from that form put on the page as HTML;
//! - `storage`: an uncaught error, or a blank page, once the page's saved data is corrupt.
//!
//! The last four use the page's own add form when it has one, and leave the page's saved
//! data as they found it.

use std::time::Duration;

use anyhow::bail;
use serde_json::{Value, json};

use crate::browser::Browser;

const SCRIPT: &str = include_str!("audit.js");

/// Every check, in the order a score lists them.
pub const CHECKS: [&str; 9] = ["h1", "label", "controls", "focus", "narrow", "empty_state", "blank_input", "html_escape", "storage"];

/// Checks that need the page's add form; without one they are not run.
pub const FORM_CHECKS: [&str; 3] = ["empty_state", "blank_input", "html_escape"];

/// The width a phone is checked at.
pub const NARROW: u32 = 360;

/// One problem: the check that found it and the sentence for the page's owner.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub check: String,
    pub text: String,
}

/// What one page's audit found, and whether the page had a form to try.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Audit {
    pub found: Vec<Finding>,
    pub probed: bool,
    /// Whether the page saved anything, so the storage check ran.
    pub stored: bool,
}

async fn eval(browser: &mut Browser, session: &str, expression: &str) -> anyhow::Result<Value> {
    let out = browser
        .call("Runtime.evaluate", json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }), Some(session))
        .await?;
    if let Some(details) = out.get("exceptionDetails") {
        let what = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("an exception");
        bail!("the audit script failed: {}", what.lines().next().unwrap_or(what));
    }
    Ok(out["result"]["value"].clone())
}

fn findings(value: &Value) -> Vec<Finding> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| Some(Finding { check: pair[0].as_str()?.to_string(), text: pair[1].as_str()?.to_string() }))
        .collect()
}

async fn press_tab(browser: &mut Browser, session: &str) -> anyhow::Result<()> {
    for kind in ["keyDown", "keyUp"] {
        browser
            .call(
                "Input.dispatchKeyEvent",
                json!({ "type": kind, "key": "Tab", "code": "Tab", "windowsVirtualKeyCode": 9, "nativeVirtualKeyCode": 9 }),
                Some(session),
            )
            .await?;
    }
    Ok(())
}

async fn pause(browser: &mut Browser, ms: u64) {
    browser.wait_for(Duration::from_millis(ms), |_| false).await;
}

/// Run every check on the page `session` has open. Call it after the page's own errors
/// have been read: the event log is cleared for the reload.
pub async fn audit(browser: &mut Browser, session: &str) -> anyhow::Result<Audit> {
    let _ = browser.call("Emulation.setFocusEmulationEnabled", json!({ "enabled": true }), Some(session)).await;
    eval(browser, session, SCRIPT).await?;
    let mut audit = Audit::default();

    // Focus first, before anything else on the page changes.
    eval(browser, session, "__tmAudit.still(true)").await?;
    for _ in 0..4 {
        press_tab(browser, session).await?;
        let step = eval(browser, session, "__tmAudit.focused()").await?;
        if step["visible"] == json!(false) {
            let name = step["name"].as_str().unwrap_or("an element");
            audit.found.push(Finding {
                check: "focus".into(),
                text: format!("{name} shows no change when the keyboard focuses it: give it a visible :focus-visible style (an outline or a ring)"),
            });
            break;
        }
    }
    eval(browser, session, "__tmAudit.still(false)").await?;

    // The add form, then the look of the page with an item in it.
    let probe = eval(browser, session, "__tmAudit.probe()").await?;
    audit.probed = probe["ran"] == json!(true);
    audit.found.extend(findings(&probe["found"]));
    let scan = eval(browser, session, "__tmAudit.scan()").await?;
    audit.found.extend(findings(&scan));

    browser
        .call("Emulation.setDeviceMetricsOverride", json!({ "width": NARROW, "height": 740, "deviceScaleFactor": 1, "mobile": false }), Some(session))
        .await?;
    pause(browser, 300).await;
    let narrow = eval(browser, session, "__tmAudit.narrow()").await;
    browser.call("Emulation.clearDeviceMetricsOverride", json!({}), Some(session)).await?;
    audit.found.extend(findings(&narrow?));

    // Last, the page's saved data, corrupted, then put back for the next page.
    let saved = eval(browser, session, "__tmAudit.corrupt()").await?;
    if saved.as_object().is_some_and(|keys| !keys.is_empty()) {
        audit.stored = true;
        browser.events.clear();
        browser.call("Page.reload", json!({}), Some(session)).await?;
        let mine = session.to_string();
        browser.wait_for(Duration::from_secs(10), move |e| e["sessionId"] == mine.as_str() && e["method"] == "Page.loadEventFired").await;
        pause(browser, 800).await;
        let thrown: Vec<String> = browser
            .events
            .iter()
            .filter(|e| e["sessionId"] == session && e["method"] == "Runtime.exceptionThrown")
            .map(|e| {
                let details = &e["params"]["exceptionDetails"];
                let text = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("an exception");
                text.lines().next().unwrap_or(text).to_string()
            })
            .collect();
        let blank = eval(browser, session, "(document.body ? document.body.innerText : '').trim().length").await?.as_u64() == Some(0);
        if let Some(first) = thrown.first() {
            audit.found.push(Finding {
                check: "storage".into(),
                text: format!("with its saved localStorage data corrupted, the page threw `{first}`: wrap JSON.parse of saved data in try/catch and start from empty"),
            });
        } else if blank {
            audit.found.push(Finding {
                check: "storage".into(),
                text: "with its saved localStorage data corrupted, the page came up blank: wrap JSON.parse of saved data in try/catch and start from empty".into(),
            });
        }
        eval(browser, session, SCRIPT).await?;
        eval(browser, session, &format!("__tmAudit.restore({saved})")).await?;
    }
    Ok(audit)
}

/// Score a built project the way its run's verify step sees it: the smoke check's own
/// errors, then every UI check, each passed, failed, or not run (null). One JSON object, for
/// `scripts/bench.sh`. Only the start page of a static site, or `/` of a built app, is opened.
pub async fn score(tree: &std::path::Path) -> anyhow::Result<Value> {
    let routes = vec![("app".to_string(), "/".to_string())];
    let Some(browsed) = crate::runtime::browse(tree, &routes).await? else {
        bail!("{} is neither a static site nor a built Next.js app, or there is no Chrome", tree.display());
    };
    let audit = browsed.audits.values().next().cloned().unwrap_or_default();
    let errors = browsed.pages.values().next().cloned();
    let mut checks = serde_json::Map::new();
    checks.insert("errors".into(), json!(errors.is_none()));
    for check in CHECKS {
        let ran = (!FORM_CHECKS.contains(&check) || audit.probed) && (check != "storage" || audit.stored);
        let failed = audit.found.iter().any(|f| f.check == check);
        checks.insert(check.into(), if ran { json!(!failed) } else { Value::Null });
    }
    let passed = checks.values().filter(|v| **v == json!(true)).count();
    let of = checks.values().filter(|v| !v.is_null()).count();
    let mut problems: Vec<String> = errors.into_iter().collect();
    problems.extend(audit.found.iter().map(|f| format!("{}: {}", f.check, f.text)));
    Ok(json!({ "passed": passed, "of": of, "checks": checks, "problems": problems, "notes": browsed.notes }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page with every defect the audit looks for.
    const BAD: &str = r#"<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body { font: 16px system-ui; }
  input { width: 100%; height: 44px; }
  *:focus { outline: none; }
  .empty { display: block; }
  .wide { width: 900px; }
</style></head><body>
<h2>Tasks</h2>
<form id="f"><input id="t"><button>Add</button></form>
<p class="empty" id="e">Nothing to do yet</p>
<ul id="l"></ul>
<div class="wide">wide</div>
<script>
  let list = JSON.parse(localStorage.getItem("tasks") || "[]");
  const l = document.getElementById("l");
  const draw = () => { l.innerHTML = list.map((t) => `<li><input type="checkbox"> ${t}</li>`).join(""); };
  draw();
  document.getElementById("f").addEventListener("submit", (e) => {
    e.preventDefault();
    list.push(document.getElementById("t").value);
    localStorage.setItem("tasks", JSON.stringify(list));
    draw();
  });
</script></body></html>"#;

    /// The same app done well.
    const GOOD: &str = r#"<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  body { font: 16px system-ui; margin: 0 auto; max-width: 640px; padding: 16px; }
  input, button { font: inherit; min-height: 44px; }
  input[type=checkbox] { width: 20px; height: 20px; min-height: 0; accent-color: #333; }
  :focus-visible { outline: 2px solid #333; outline-offset: 2px; }
  [hidden] { display: none !important; }
</style></head><body>
<h1>Tasks</h1>
<form id="f"><label for="t">New task</label> <input id="t"><button>Add</button></form>
<p id="e">Nothing to do yet</p>
<ul id="l"></ul>
<script>
  let list = [];
  try { list = JSON.parse(localStorage.getItem("tasks") || "[]"); } catch { list = []; }
  if (!Array.isArray(list)) list = [];
  const l = document.getElementById("l");
  const draw = () => {
    l.replaceChildren(...list.map((t, i) => {
      const li = document.createElement("li");
      const box = document.createElement("input");
      box.type = "checkbox";
      box.setAttribute("aria-label", "Done");
      const span = document.createElement("span");
      span.textContent = t;
      li.append(box, " ", span);
      return li;
    }));
    document.getElementById("e").hidden = list.length > 0;
  };
  draw();
  document.getElementById("f").addEventListener("submit", (e) => {
    e.preventDefault();
    const text = document.getElementById("t").value.trim();
    if (!text) return;
    list.push(text);
    localStorage.setItem("tasks", JSON.stringify(list));
    draw();
  });
</script></body></html>"#;

    async fn audit_page(html: &str) -> Option<Audit> {
        let chrome = crate::runtime::chrome()?;
        let dir = std::env::temp_dir().join(format!("tm-audit-{}-{}", std::process::id(), html.len()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), html).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let site = tokio::spawn(crate::static_site::serve(listener, dir.clone()));
        let mut browser = Browser::launch(&chrome).await.unwrap();
        let target = browser.call("Target.createTarget", json!({ "url": "about:blank" }), None).await.unwrap();
        let attached = browser.call("Target.attachToTarget", json!({ "targetId": target["targetId"], "flatten": true }), None).await.unwrap();
        let session = attached["sessionId"].as_str().unwrap().to_string();
        for domain in ["Page.enable", "Runtime.enable"] {
            browser.call(domain, json!({}), Some(&session)).await.unwrap();
        }
        browser.call("Page.navigate", json!({ "url": format!("http://127.0.0.1:{port}/index.html") }), Some(&session)).await.unwrap();
        let mine = session.clone();
        browser.wait_for(Duration::from_secs(10), move |e| e["sessionId"] == mine.as_str() && e["method"] == "Page.loadEventFired").await;
        // Something saved, so the storage check has data to corrupt.
        eval(&mut browser, &session, "localStorage.setItem('tasks', JSON.stringify(['first']));").await.unwrap();
        browser.events.clear();
        browser.call("Page.reload", json!({}), Some(&session)).await.unwrap();
        let mine = session.clone();
        browser.wait_for(Duration::from_secs(10), move |e| e["sessionId"] == mine.as_str() && e["method"] == "Page.loadEventFired").await;
        let found = audit(&mut browser, &session).await.unwrap();
        site.abort();
        let _ = std::fs::remove_dir_all(&dir);
        Some(found)
    }

    #[tokio::test]
    async fn a_page_with_every_defect_fails_every_check() {
        let Some(audit) = audit_page(BAD).await else {
            eprintln!("no Chrome here; skipped");
            return;
        };
        let checks: Vec<&str> = audit.found.iter().map(|f| f.check.as_str()).collect();
        for check in CHECKS {
            assert!(checks.contains(&check), "{check} not found in {:#?}", audit.found);
        }
        assert!(audit.probed && audit.stored);
    }

    #[tokio::test]
    async fn the_same_app_done_well_passes() {
        let Some(audit) = audit_page(GOOD).await else {
            eprintln!("no Chrome here; skipped");
            return;
        };
        assert!(audit.found.is_empty(), "{:#?}", audit.found);
        assert!(audit.probed && audit.stored);
    }
}
