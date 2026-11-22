//! Connecting things to this daemon: phones and other computers, other machines, and
//! outside agents.
//!
//! ```text
//! GET  /api/connect               how this daemon can be reached: one route per address
//! POST /api/pairing               {label?, kind?, ttl_secs?} -> a one-time code and a link per route
//! GET  /api/devices               paired devices, with the caller marked `current`
//! POST /api/devices/{id}/rename   {label}
//! POST /api/devices/{id}/revoke   its token stops working at once
//! POST /api/devices/agent         {label} -> a token for an outside agent, shown once
//! POST /mcp                       the MCP server outside agents use (JSON-RPC over HTTP)
//! ```
//!
//! A link is `<route>/pair#code=<code>`: the code rides in the fragment, so it never reaches
//! a server log or a referrer, and the app removes it from the address bar once used.
//!
//! The MCP tools call the same handlers the web app does, so an agent gets the same checks,
//! the same refusals, and the same runs a person would.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{App, Reply, bearer, fail, ok};

/// One way to reach this daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// `this-computer`, `local-network`, `tailscale` or `network`.
    pub kind: &'static str,
    pub address: String,
    /// Ends in `/`.
    pub url: String,
}

/// What kind of address `ip` is, for a person choosing a link.
pub fn route_kind(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() => "this-computer",
        IpAddr::V4(v4) if super::is_tailscale(v4) => "tailscale",
        IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => "local-network",
        IpAddr::V6(v6) if v6.is_loopback() => "this-computer",
        _ => "network",
    }
}

/// [`lan_ipv4`], for the startup banner.
pub fn lan_route() -> Option<Ipv4Addr> {
    lan_ipv4()
}

/// The address other machines on the local network use for this one: the source address
/// of a route out. Connecting a UDP socket sends nothing.
fn lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// This machine's Tailscale IPv4 address and MagicDNS name, when Tailscale is running.
pub fn tailscale_identity() -> (Option<Ipv4Addr>, Option<String>) {
    let ip = super::tailscale_ipv4().ok();
    if ip.is_none() {
        return (None, None);
    }
    let dns = std::process::Command::new("tailscale")
        .args(["status", "--json", "--self", "--peers=false"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
        .and_then(|status| status["Self"]["DNSName"].as_str().map(|name| name.trim_end_matches('.').to_string()))
        .filter(|name| !name.is_empty());
    (ip, dns)
}

fn url_for(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V6(v6) => format!("http://[{v6}]:{port}/"),
        IpAddr::V4(v4) => format!("http://{v4}:{port}/"),
    }
}

/// Every route to a daemon listening on `bind`, the most local first. `lan` and
/// `tailscale` are what this machine has; they matter only when it listens on every address.
pub fn routes(bind: SocketAddr, lan: Option<Ipv4Addr>, tailscale: Option<Ipv4Addr>) -> Vec<Route> {
    let port = bind.port();
    let mut out = Vec::new();
    let mut add = |ip: IpAddr| {
        let url = url_for(ip, port);
        if !out.iter().any(|route: &Route| route.url == url) {
            out.push(Route { kind: route_kind(ip), address: format!("{ip}:{port}"), url });
        }
    };
    if bind.ip().is_unspecified() {
        add(IpAddr::V4(Ipv4Addr::LOCALHOST));
        if let Some(ip) = lan {
            add(IpAddr::V4(ip));
        }
        if let Some(ip) = tailscale {
            add(IpAddr::V4(ip));
        }
    } else {
        add(bind.ip());
    }
    out
}

/// The routes for this daemon now. Tailscale is asked only when it could be a route.
async fn current_routes(app: &App) -> (Vec<Route>, Value) {
    let bind = app.addr;
    let wide = bind.ip().is_unspecified();
    let tailnet_bound = matches!(bind.ip(), IpAddr::V4(ip) if super::is_tailscale(ip));
    let (ts_ip, ts_dns) = if wide || tailnet_bound {
        tokio::task::spawn_blocking(tailscale_identity).await.unwrap_or((None, None))
    } else {
        (None, None)
    };
    let lan = if wide { lan_ipv4() } else { None };
    let mut routes = routes(bind, lan, ts_ip);
    if let (Some(dns), true) = (&ts_dns, wide || tailnet_bound) {
        routes.push(Route { kind: "tailscale", address: format!("{dns}:{}", bind.port()), url: format!("http://{dns}:{}/", bind.port()) });
    }
    let tailscale = json!({ "running": ts_ip.is_some(), "ip": ts_ip.map(|ip| ip.to_string()), "dns": ts_dns });
    (routes, tailscale)
}

fn route_json(route: &Route) -> Value {
    json!({ "kind": route.kind, "address": route.address, "url": route.url })
}

pub async fn overview(State(app): State<Arc<App>>) -> Reply {
    let (routes, tailscale) = current_routes(&app).await;
    let only_here = routes.iter().all(|route| route.kind == "this-computer");
    let base = routes.first().map(|route| route.url.clone()).unwrap_or_default();
    ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "bind": app.addr.to_string(),
        "only_this_computer": only_here,
        "routes": routes.iter().map(route_json).collect::<Vec<_>>(),
        "tailscale": tailscale,
        "mcp_url": format!("{base}mcp"),
        "project": app.project().display().to_string(),
        "kinds": crate::auth::DEVICE_KINDS,
    }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct PairingBody {
    label: String,
    kind: String,
    ttl_secs: Option<u64>,
}

pub async fn create_pairing(State(app): State<Arc<App>>, body: Option<Json<PairingBody>>) -> Reply {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let ttl = body.ttl_secs.map(Duration::from_secs).unwrap_or(crate::auth::OFFER_TTL);
    let Some(offer) = app.auth.offer(&body.label, &body.kind, ttl)? else {
        return Err(fail(StatusCode::CONFLICT, "this daemon signs devices in with accounts, not pairing links"));
    };
    let (routes, _) = current_routes(&app).await;
    let links: Vec<Value> = routes
        .iter()
        .map(|route| json!({ "kind": route.kind, "address": route.address, "url": format!("{}pair#code={}", route.url, offer.code) }))
        .collect();
    crate::log::note(&format!("pairing link made for {}", if offer.label.is_empty() { "a device" } else { offer.label.as_str() }));
    ok(json!({ "code": offer.code, "label": offer.label, "kind": offer.kind, "expires_at": offer.expires_at, "links": links }))
}

/// The paired device behind this request.
fn caller(app: &App, headers: &HeaderMap) -> Option<String> {
    bearer(headers).and_then(|token| app.auth.identify(token)).map(|principal| principal.id)
}

pub async fn devices(State(app): State<Arc<App>>, headers: HeaderMap) -> Reply {
    let me = caller(&app, &headers);
    let mut list: Vec<Value> = app
        .auth
        .devices()
        .into_iter()
        .map(|device| {
            let current = me.as_deref() == Some(device.id.as_str());
            let mut value = json!(device);
            value["current"] = json!(current);
            value
        })
        .collect();
    // The most recently seen first; this device on top.
    list.sort_by(|a, b| {
        b["current"].as_bool().cmp(&a["current"].as_bool()).then_with(|| b["last_seen"].as_str().unwrap_or("").cmp(a["last_seen"].as_str().unwrap_or("")))
    });
    ok(json!({ "devices": list }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct RenameBody {
    label: String,
}

pub async fn rename_device(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<RenameBody>) -> Reply {
    if body.label.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "a device needs a name"));
    }
    if !app.auth.rename(&id, &body.label)? {
        return Err(fail(StatusCode::NOT_FOUND, "no such device"));
    }
    ok(json!({ "ok": true }))
}

pub async fn revoke_device(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    if !app.auth.revoke(&id)? {
        return Err(fail(StatusCode::NOT_FOUND, "no such device"));
    }
    crate::log::note(&format!("device {id} revoked"));
    ok(json!({ "ok": true }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct AgentBody {
    label: String,
}

pub async fn mint_agent(State(app): State<Arc<App>>, body: Option<Json<AgentBody>>) -> Reply {
    let label = body.map(|Json(body)| body.label).unwrap_or_default();
    let Some(token) = app.auth.mint(&label, "agent")? else {
        return Err(fail(StatusCode::CONFLICT, "this daemon cannot make agent tokens"));
    };
    let id = app.auth.identify(&token).map(|principal| principal.id).unwrap_or_default();
    let (routes, _) = current_routes(&app).await;
    let base = routes.first().map(|route| route.url.clone()).unwrap_or_default();
    // The name the device list keeps, which has no control characters, not what was sent.
    let named = app.auth.devices().into_iter().find(|device| device.id == id).map(|device| device.label).unwrap_or_default();
    crate::log::note(&format!("agent token made: {named}"));
    ok(json!({ "token": token, "id": id, "mcp_url": format!("{base}mcp") }))
}

// The MCP server for outside agents.

const PROTOCOL: &str = "2025-06-18";
/// How long `taskmaster_wait` may hold one call.
const WAIT_MAX: Duration = Duration::from_secs(600);

fn tools() -> Value {
    let run = json!({ "type": "string", "description": "The run id, such as run-1a2b3c4d" });
    json!([
        { "name": "taskmaster_list_runs",
          "description": "List this project's recent Taskmaster runs: id, phase, whether it is working or needs a person, and the first line of its request.",
          "inputSchema": { "type": "object", "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 50 } } } },
        { "name": "taskmaster_start_run",
          "description": "Start a Taskmaster run in the open project with the person's saved providers. mode: `agent` (one agent does it all), `code` (a master plans and parallel workers build), or `auto` (Taskmaster picks). With approve=false a planned run waits for approval; call taskmaster_approve or let the person approve in the app.",
          "inputSchema": { "type": "object", "required": ["prompt"], "properties": {
              "prompt": { "type": "string" },
              "mode": { "type": "string", "enum": ["auto", "code", "agent"] },
              "approve": { "type": "boolean", "description": "Build as soon as the plan is ready. Default false." } } } },
        { "name": "taskmaster_run_status",
          "description": "One run's phase, pages, spend, error and its link in the Taskmaster app.",
          "inputSchema": { "type": "object", "required": ["run"], "properties": { "run": run } } },
        { "name": "taskmaster_wait",
          "description": "Wait until a run stops working: done, failed, cancelled, or waiting on a person (a plan to approve or a risky tool call). Returns its status. timeout_secs defaults to 60, at most 600.",
          "inputSchema": { "type": "object", "required": ["run"], "properties": { "run": run, "timeout_secs": { "type": "integer", "minimum": 1, "maximum": 600 } } } },
        { "name": "taskmaster_read_run",
          "description": "What a run did, kept short: the request, the plan, each agent's latest words and edited files, errors, and notes. max_chars defaults to 6000.",
          "inputSchema": { "type": "object", "required": ["run"], "properties": { "run": run, "max_chars": { "type": "integer", "minimum": 500, "maximum": 40000 } } } },
        { "name": "taskmaster_send_note",
          "description": "Send a note to a run. While it works, the master reads it before the next worker. On a finished run it starts a new turn below the last one; mode picks who takes it (agent, code or auto).",
          "inputSchema": { "type": "object", "required": ["run", "text"], "properties": {
              "run": run, "text": { "type": "string" },
              "to": { "type": "string", "description": "master (default), workers, page:<id>, reviewer or verifier" },
              "mode": { "type": "string", "enum": ["auto", "code", "agent"] } } } },
        { "name": "taskmaster_approve",
          "description": "Approve a run's plan so its workers start building.",
          "inputSchema": { "type": "object", "required": ["run"], "properties": { "run": run } } },
        { "name": "taskmaster_cancel",
          "description": "Stop a run now. Pages that passed stay; it can be resumed in the app.",
          "inputSchema": { "type": "object", "required": ["run"], "properties": { "run": run } } },
        { "name": "taskmaster_preview",
          "description": "Start a run's live preview (its dev server, or the project's files) and return its address.",
          "inputSchema": { "type": "object", "required": ["run"], "properties": { "run": run } } },
    ])
}

/// The JSON body of a handler's reply, or its error sentence.
async fn body_of(reply: Reply) -> Result<Value, String> {
    match reply {
        Ok(response) => {
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024).await.map_err(|err| err.to_string())?;
            let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if status.is_success() { Ok(value) } else { Err(value["error"].as_str().unwrap_or("refused").to_string()) }
        }
        Err(err) => Err(err.1),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args[key].as_str().map(str::trim).filter(|value| !value.is_empty()).ok_or_else(|| format!("`{key}` is required"))
}

fn run_link(base: &str, id: &str) -> String {
    format!("{base}runs/{}", id)
}

/// A run as an outside agent needs it: short, and with where a person can look.
fn status_of(detail: &Value, base: &str) -> Value {
    let id = detail["id"].as_str().unwrap_or_default();
    let phase = detail["phase"].as_str().unwrap_or_default();
    let active = detail["active"].as_bool().unwrap_or(false);
    let waiting = detail["pending_approvals"].as_u64().unwrap_or(0);
    let state = if detail["cancelled"].as_bool().unwrap_or(false) {
        "cancelled"
    } else if waiting > 0 {
        "needs a person: a tool call waits for approval"
    } else if phase == "awaiting_approval" {
        "needs a person: the plan waits for approval"
    } else if active {
        "working"
    } else {
        phase
    };
    json!({
        "id": id,
        "state": state,
        "phase": phase,
        "active": active,
        "pages": detail["pages"].as_array().map(|pages| pages.iter().map(|p| json!({ "id": p["id"], "state": p["state"] })).collect::<Vec<_>>()),
        "error": detail["error"],
        "merged": detail["merged"],
        "branch": detail["branch"],
        "spent_usd": detail["spent"]["cost_usd"],
        "link": run_link(base, id),
    })
}

fn done_waiting(detail: &Value) -> bool {
    let active = detail["active"].as_bool().unwrap_or(false);
    let waiting = detail["pending_approvals"].as_u64().unwrap_or(0) > 0;
    !active || waiting || detail["phase"] == "awaiting_approval"
}

/// The last `max` characters of `text`, on a character boundary.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let skipped: String = text.chars().skip(count - max).collect();
    format!("…{skipped}")
}

/// A short account of a run: request, plan, each agent's latest words, notes.
fn summary(app: &App, id: &str, detail: &Value, max: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!("Run {id}: {}\n", detail["phase"].as_str().unwrap_or("")));
    if let Some(prompt) = detail["prompt"].as_str() {
        out.push_str(&format!("Request: {}\n", tail(prompt.trim(), 1200)));
    }
    if let Some(plan) = detail["plan"].as_object() {
        if let Some(summary) = plan.get("summary").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            out.push_str(&format!("Plan: {summary}\n"));
        }
        for page in plan.get("pages").and_then(Value::as_array).cloned().unwrap_or_default() {
            out.push_str(&format!("- unit {} owns {}\n", page["id"].as_str().unwrap_or(""), page["owned_glob"].as_str().unwrap_or("")));
        }
    }
    if let Some(error) = detail["error"].as_str() {
        out.push_str(&format!("Error: {error}\n"));
    }
    // Each agent's last words and the files it wrote.
    let (_, entries) = app.live.read(id, 0);
    let mut agents: Vec<(String, String, Vec<String>)> = Vec::new();
    for entry in &entries {
        let agent = entry["agent"]["id"].as_str().unwrap_or("");
        if agent.is_empty() || agent == "runtime" || agent == "person" {
            continue;
        }
        let at = match agents.iter().position(|(name, _, _)| name == agent) {
            Some(at) => at,
            None => {
                agents.push((agent.to_string(), String::new(), Vec::new()));
                agents.len() - 1
            }
        };
        match entry["kind"].as_str().unwrap_or("") {
            "start" => agents[at].1.clear(),
            "text" => agents[at].1.push_str(entry["text"].as_str().unwrap_or("")),
            "tool_call" => {
                let name = entry["name"].as_str().unwrap_or("").to_lowercase();
                let target = entry["target"].as_str().unwrap_or("").to_string();
                let writes = ["write", "edit", "patch", "create"].iter().any(|w| name.contains(w));
                if writes && !target.is_empty() && !agents[at].2.contains(&target) {
                    agents[at].2.push(target);
                }
            }
            _ => {}
        }
    }
    for (agent, said, files) in &agents {
        out.push_str(&format!("\n[{agent}]\n"));
        if !files.is_empty() {
            out.push_str(&format!("edited: {}\n", files.iter().take(20).cloned().collect::<Vec<_>>().join(", ")));
        }
        let said = said.trim();
        if !said.is_empty() {
            out.push_str(&tail(said, 800));
            out.push('\n');
        }
    }
    let notes = app.live.notes(id);
    if !notes.is_empty() {
        out.push_str("\nNotes:\n");
        for note in notes.iter().rev().take(10) {
            out.push_str(&format!("- to {}: {}\n", note.to, tail(note.text.trim(), 300)));
        }
    }
    tail(&out, max)
}

async fn call_tool(app: &Arc<App>, name: &str, args: &Value) -> Result<Value, String> {
    let (routes, _) = current_routes(app).await;
    let base = routes.first().map(|route| route.url.clone()).unwrap_or_default();
    let detail = |id: String| async move { body_of(super::show_run(State(app.clone()), UrlPath(id)).await).await };
    match name {
        "taskmaster_list_runs" => {
            let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
            let listed = body_of(super::list_runs(State(app.clone()), axum::extract::Query(super::RunsQuery::default())).await).await?;
            let mut runs: Vec<Value> = listed["runs"].as_array().cloned().unwrap_or_default();
            runs.sort_by(|a, b| b["last_at"].as_str().unwrap_or("").cmp(a["last_at"].as_str().unwrap_or("")));
            Ok(json!({ "runs": runs.iter().take(limit).map(|run| {
                let mut status = status_of(run, &base);
                status["request"] = run["prompt_head"].clone();
                status
            }).collect::<Vec<_>>() }))
        }
        "taskmaster_start_run" => {
            let prompt = arg(args, "prompt")?;
            // A goal's check runs on this machine outside the agents' sandbox, so only the
            // person sets one, in the app.
            if crate::goals::extract_goal(prompt).1.is_some() {
                return Err("a /goal line is set in the Taskmaster app, not by an outside agent; send the prompt without it".into());
            }
            let request = json!({
                "prompt": prompt,
                "mode": args["mode"].as_str().unwrap_or("auto"),
                "yes": args["approve"].as_bool().unwrap_or(false),
            });
            let body: super::NewRun = serde_json::from_value(request).map_err(|err| err.to_string())?;
            let started = body_of(super::create_run(State(app.clone()), Json(body)).await).await?;
            let id = started["id"].as_str().unwrap_or_default().to_string();
            Ok(json!({ "id": id, "link": run_link(&base, &id), "next": "Call taskmaster_wait, then taskmaster_read_run." }))
        }
        "taskmaster_run_status" => Ok(status_of(&detail(arg(args, "run")?.to_string()).await?, &base)),
        "taskmaster_wait" => {
            let id = arg(args, "run")?.to_string();
            let limit = Duration::from_secs(args["timeout_secs"].as_u64().unwrap_or(60)).min(WAIT_MAX);
            let started = Instant::now();
            loop {
                let now = detail(id.clone()).await?;
                if done_waiting(&now) || started.elapsed() >= limit {
                    let mut status = status_of(&now, &base);
                    status["waited_secs"] = json!(started.elapsed().as_secs());
                    status["timed_out"] = json!(!done_waiting(&now));
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
        "taskmaster_read_run" => {
            let id = arg(args, "run")?.to_string();
            let max = args["max_chars"].as_u64().unwrap_or(6000).clamp(500, 40_000) as usize;
            let now = detail(id.clone()).await?;
            Ok(json!({ "status": status_of(&now, &base), "summary": summary(app, &id, &now, max) }))
        }
        "taskmaster_send_note" => {
            let id = arg(args, "run")?.to_string();
            let body = json!({ "to": args["to"].as_str().unwrap_or("master"), "text": arg(args, "text")?, "mode": args["mode"] });
            let body: super::desk::NoteBody = serde_json::from_value(body).map_err(|err| err.to_string())?;
            let note = body_of(super::desk::add_note(State(app.clone()), UrlPath(id.clone()), Json(body)).await).await?;
            Ok(json!({ "note": note["id"], "status": status_of(&detail(id).await?, &base) }))
        }
        "taskmaster_approve" => {
            let id = arg(args, "run")?.to_string();
            body_of(super::approve_run(State(app.clone()), UrlPath(id.clone()), None).await).await?;
            Ok(status_of(&detail(id).await?, &base))
        }
        "taskmaster_cancel" => {
            let id = arg(args, "run")?.to_string();
            let body = axum::body::Bytes::from_static(br#"{"mode":"force"}"#);
            body_of(super::cancel_run(State(app.clone()), UrlPath(id.clone()), body).await).await?;
            Ok(json!({ "id": id, "cancelling": true }))
        }
        "taskmaster_preview" => {
            let id = arg(args, "run")?.to_string();
            let view = body_of(super::desk::start_preview(State(app.clone()), UrlPath(id)).await).await?;
            Ok(json!({ "state": view["state"], "url": view["url"], "note": view["reason"] }))
        }
        other => Err(format!("no tool `{other}`")),
    }
}

/// `POST /mcp`: JSON-RPC from an outside agent. The router's guard has checked the token.
pub async fn mcp(State(app): State<Arc<App>>, body: String) -> Response {
    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "not JSON" } }))).into_response();
    };
    let Some(id) = message.get("id").cloned() else {
        // A notification: nothing to answer.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = &message["params"];
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "taskmaster", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Taskmaster builds software with coding agents on this machine. Start a run with taskmaster_start_run, \
                follow it with taskmaster_wait, read it with taskmaster_read_run, and steer it with taskmaster_send_note. \
                A person can watch every run at the link each tool returns.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let called = call_tool(&app, name, &params["arguments"]).await;
            if name.starts_with("taskmaster_") {
                crate::log::note(&format!("outside agent called {name}{}", if called.is_err() { " (refused)" } else { "" }));
            }
            Ok(match called {
                Ok(value) => json!({
                    "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
                    "structuredContent": value,
                    "isError": false,
                }),
                Err(message) => json!({ "content": [{ "type": "text", "text": message }], "isError": true }),
            })
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };
    let reply = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    };
    Json(reply).into_response()
}

/// The skill that teaches an outside agent these tools (A10), as shipped in the repository.
pub const SKILL: &str = include_str!("../../../../skills/taskmaster/SKILL.md");

/// `GET /skill.md`: the skill, for `curl … -o ~/.claude/skills/taskmaster/SKILL.md`. It holds
/// no secret, so it needs no token.
pub async fn skill() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")], SKILL).into_response()
}

/// The tool names the MCP server offers, for the skill's own check.
#[cfg(test)]
pub fn tool_names() -> Vec<String> {
    tools().as_array().map(|list| list.iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// This server never pushes, so it offers no stream.
pub async fn mcp_no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_follow_the_bind_address() {
        let lan = Some(Ipv4Addr::new(192, 168, 1, 20));
        let ts = Some(Ipv4Addr::new(100, 101, 102, 103));
        let here = routes("127.0.0.1:8737".parse().unwrap(), lan, ts);
        assert_eq!(here.len(), 1);
        assert_eq!((here[0].kind, here[0].url.as_str()), ("this-computer", "http://127.0.0.1:8737/"));

        let wide = routes("0.0.0.0:8737".parse().unwrap(), lan, ts);
        let kinds: Vec<&str> = wide.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec!["this-computer", "local-network", "tailscale"]);
        assert_eq!(wide[1].url, "http://192.168.1.20:8737/");
        assert_eq!(wide[2].url, "http://100.101.102.103:8737/");

        let tailnet = routes("100.101.102.103:9000".parse().unwrap(), lan, ts);
        assert_eq!((tailnet.len(), tailnet[0].kind), (1, "tailscale"));
        assert_eq!(route_kind("8.8.8.8".parse().unwrap()), "network");
        assert_eq!(route_kind("10.1.2.3".parse().unwrap()), "local-network");
    }

    #[test]
    fn a_long_text_keeps_its_end() {
        assert_eq!(tail("abcdef", 10), "abcdef");
        assert_eq!(tail("abcdef", 3), "…def");
        assert_eq!(tail("ééééé", 2), "…éé");
    }
}
