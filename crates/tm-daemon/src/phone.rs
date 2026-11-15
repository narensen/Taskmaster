//! Phone UI on the tailnet.
//!
//! The listener binds to a Tailscale address, so the only path to it is the tailnet.
//! A phone pairs by posting the code printed on this machine and then polls run status.

use std::collections::BTreeSet;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tm_core::{Event, Run};

use crate::run_directory;

const PAGE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Taskmaster</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; background: #000; color: #fff; font: 18px/1.4 ui-sans-serif, system-ui, sans-serif; }
  main { max-width: 32rem; margin: 0 auto; padding: 1.5rem 1.25rem 3rem; }
  h1 { font-size: 1.6rem; font-weight: 650; letter-spacing: -0.03em; margin: 0 0 0.25rem; }
  p { color: #c8c8c8; }
  form, section { display: grid; gap: 0.75rem; }
  input { font: inherit; padding: 0.8rem 0.9rem; border-radius: 0.7rem; border: 1px solid #333; background: #111; color: #fff; letter-spacing: 0.2em; text-transform: uppercase; }
  button { font: inherit; padding: 0.85rem 1rem; border: 0; border-radius: 0.7rem; background: #fff; color: #000; font-weight: 650; }
  .run { border-top: 1px solid #222; padding: 0.9rem 0; }
  .phase { font-size: 1.15rem; }
  .bad { color: #ffb4b4; }
  .ok { color: #b6f5c8; }
  .muted { color: #9a9a9a; font-size: 0.9rem; }
</style>
</head>
<body>
<main>
  <h1>Taskmaster</h1>
  <p id="lead">Pair this phone with the machine running the daemon.</p>
  <form id="pair">
    <input id="code" name="code" autocomplete="one-time-code" autocapitalize="characters" placeholder="Pairing code" required>
    <button type="submit">Pair</button>
  </form>
  <section id="status" hidden></section>
</main>
<script>
const form = document.getElementById("pair");
const status = document.getElementById("status");
const lead = document.getElementById("lead");
const key = "taskmaster-phone-token";
function line(parent, text, className) {
  const p = document.createElement("p");
  p.textContent = text;
  if (className) p.className = className;
  parent.appendChild(p);
}
async function load() {
  const token = localStorage.getItem(key);
  if (!token) return;
  const response = await fetch("/status", { headers: { authorization: "Bearer " + token } });
  if (response.status === 401) { localStorage.removeItem(key); return; }
  const body = await response.json();
  form.hidden = true;
  status.hidden = false;
  status.replaceChildren();
  lead.textContent = "This phone is paired.";
  if (!body.runs.length) line(status, "No runs yet.", "muted");
  for (const run of body.runs) {
    const block = document.createElement("div");
    block.className = "run";
    line(block, run.id + "  " + run.phase, "phase " + (run.ok ? "ok" : "bad"));
    for (const page of run.pages) line(block, page.id + "  " + page.state, "muted");
    status.appendChild(block);
  }
}
form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const code = document.getElementById("code").value.trim();
  const response = await fetch("/pair", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ code }) });
  if (!response.ok) { lead.textContent = "That code was refused."; return; }
  const body = await response.json();
  localStorage.setItem(key, body.token);
  load();
});
load();
setInterval(load, 3000);
</script>
</body>
</html>
"#;

struct Phone {
    project: PathBuf,
    code: String,
    tokens: Mutex<BTreeSet<String>>,
}

/// A paired listener. Dropping it stops the server.
pub struct PhoneEndpoint {
    pub url: String,
    pub code: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for PhoneEndpoint {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl PhoneEndpoint {
    pub async fn serve(project: impl Into<PathBuf>, bind: SocketAddr) -> anyhow::Result<Self> {
        let project = project.into();
        let code = pairing_code()?;
        let listener = TcpListener::bind(bind).await.with_context(|| format!("cannot bind {bind}"))?;
        let addr = listener.local_addr().context("phone listener has no address")?;
        let state = Arc::new(Phone { project, code: code.clone(), tokens: Mutex::new(BTreeSet::new()) });
        let app = Router::new()
            .route("/", get(page))
            .route("/pair", post(pair))
            .route("/status", get(status))
            .with_state(state);
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        Ok(PhoneEndpoint { url: format!("http://{addr}/"), code, shutdown: Some(tx), task: Some(task) })
    }
}

/// The IPv4 address Tailscale gives this machine. It is inside 100.64.0.0/10.
pub fn tailscale_ipv4() -> anyhow::Result<Ipv4Addr> {
    let output = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .context("tailscale is not installed")?;
    if !output.status.success() {
        bail!("tailscale is not running");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Ok(ip) = line.trim().parse::<Ipv4Addr>() else { continue };
        if is_tailscale(ip) {
            return Ok(ip);
        }
    }
    bail!("tailscale has no IPv4 address in 100.64.0.0/10");
}

pub fn is_tailscale(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
}

async fn pair(State(phone): State<Arc<Phone>>, Json(body): Json<PairBody>) -> Response {
    if body.code.trim() != phone.code {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let token = match device_token() {
        Ok(token) => token,
        Err(err) => return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    };
    phone.tokens.lock().unwrap().insert(token.clone());
    if let Err(err) = remember(&phone.project, &token) {
        return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response();
    }
    Json(json!({ "token": token })).into_response()
}

async fn status(State(phone): State<Arc<Phone>>, headers: HeaderMap) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let allowed = token.is_some_and(|token| phone.tokens.lock().unwrap().contains(token));
    if !allowed {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({ "runs": runs(&phone.project) })).into_response()
}

use axum::response::Response;

fn runs(project: &Path) -> Vec<Value> {
    let dir = project.join(".taskmaster/runs");
    let Ok(read) = fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        let run_dir = run_directory(project, &id);
        let events = fs::read_to_string(run_dir.join("events.jsonl")).unwrap_or_default();
        let parsed: Vec<Event> = events.lines().filter_map(|line| serde_json::from_str(line).ok()).collect();
        let run = Run::replay(1, parsed);
        let phase = format!("{:?}", run.phase).to_lowercase();
        let status: Value = fs::read_to_string(run_dir.join("status.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        let pages: Vec<Value> = run
            .pages
            .iter()
            .map(|(id, page)| json!({ "id": id, "state": format!("{:?}", page.state).to_lowercase() }))
            .collect();
        out.push(json!({
            "id": id,
            "phase": status["phase"].as_str().unwrap_or(&phase),
            "ok": status["ok"].as_bool().unwrap_or(false),
            "pages": pages,
        }));
    }
    out.sort_by(|a, b| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")));
    out
}

fn remember(project: &Path, token: &str) -> anyhow::Result<()> {
    let path = project.join(".taskmaster/devices.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tokens: Vec<String> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    tokens.push(token.to_string());
    let body = serde_json::to_vec_pretty(&tokens)?;
    fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn pairing_code() -> anyhow::Result<String> {
    let bytes = random_bytes(4)?;
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut n = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let mut out = String::new();
    for _ in 0..6 {
        out.push(ALPHABET[(n & 31) as usize] as char);
        n >>= 5;
    }
    Ok(out)
}

fn device_token() -> anyhow::Result<String> {
    Ok(random_bytes(32)?.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn random_bytes(n: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = vec![0u8; n];
    let mut file = fs::File::open("/dev/urandom").context("cannot read /dev/urandom")?;
    use std::io::Read;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_range_is_the_cgnat_block() {
        assert!(is_tailscale("100.64.0.1".parse().unwrap()));
        assert!(is_tailscale("100.127.255.255".parse().unwrap()));
        assert!(!is_tailscale("100.63.0.1".parse().unwrap()));
        assert!(!is_tailscale("127.0.0.1".parse().unwrap()));
        assert!(!is_tailscale("10.0.0.1".parse().unwrap()));
    }

    #[tokio::test]
    async fn pairing_then_status() {
        let dir = std::env::temp_dir().join(format!("tm-phone-{}-pair", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".taskmaster/runs/current")).unwrap();
        fs::write(dir.join(".taskmaster/runs/current/events.jsonl"), "{\"plan_ready\":null}\n").ok();
        fs::write(
            dir.join(".taskmaster/runs/current/status.json"),
            r#"{"ok":true,"phase":"done"}"#,
        )
        .unwrap();
        let endpoint = PhoneEndpoint::serve(&dir, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let refused = http(&endpoint.url, "POST", "/pair", None, r#"{"code":"WRONG"}"#).await;
        assert_eq!(refused.0, 401, "{}", refused.1);
        let paired = http(&endpoint.url, "POST", "/pair", None, &format!("{{\"code\":\"{}\"}}", endpoint.code)).await;
        assert_eq!(paired.0, 200, "{}", paired.1);
        let token = serde_json::from_str::<Value>(&paired.1).unwrap()["token"].as_str().unwrap().to_string();
        let open = http(&endpoint.url, "GET", "/status", None, "").await;
        assert_eq!(open.0, 401, "{}", open.1);
        let body = http(&endpoint.url, "GET", "/status", Some(&token), "").await;
        assert_eq!(body.0, 200, "{}", body.1);
        assert!(body.1.contains("current"));
        assert!(body.1.contains("done"));
        let _ = fs::remove_dir_all(&dir);
    }

    async fn http(base: &str, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = base.trim_start_matches("http://").trim_end_matches('/');
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token.map(|token| format!("authorization: Bearer {token}\r\n")).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{auth}\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).await.unwrap();
        let (head, rest) = buf.split_once("\r\n\r\n").unwrap_or(("", &buf));
        let status: u16 = head.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
        (status, rest.to_string())
    }
}
