//! Terminals a person opens from the app: a login shell on a pseudo-terminal in the
//! project, kept alive while the drawer that shows it is hidden.
//!
//! ```text
//! GET  /api/terminals                 the open terminals
//! POST /api/terminals                 {cols?, rows?} -> a new terminal in the project
//! GET  /api/terminals/{id}/stream     ?from=<offset>&token=<t> output as server-sent events
//! POST /api/terminals/{id}/input      {data}: what was typed, as text
//! POST /api/terminals/{id}/resize     {cols, rows}
//! POST /api/terminals/{id}/close      end the shell and everything it started
//! ```
//!
//! Output is kept as raw bytes in a ring of `SCROLLBACK` bytes. A `data` event carries
//! `{at, b64}`, the offset of its first byte and the bytes in base64, so a client that
//! reconnects with `from` gets what it missed and nothing twice. `exit` ends the stream.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use base64::Engine;
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tm_harness::GroupGuard;
use tm_harness::pty::{self, PtyWriter};
use tokio::sync::watch;

use super::{App, ApiError, Reply, bearer, fail, ok};

const SCROLLBACK: usize = 512 * 1024;
/// A person rarely needs more; a runaway script opening shells is stopped here.
const MAX_OPEN: usize = 12;

#[derive(Default)]
pub struct Terminals {
    open: Mutex<HashMap<String, Arc<Term>>>,
    next: Mutex<u64>,
}

struct Output {
    bytes: VecDeque<u8>,
    /// Offset just past the last byte ever written.
    end: u64,
    exit: Option<Option<i32>>,
}

pub struct Term {
    id: String,
    cwd: PathBuf,
    shell: String,
    started: String,
    writer: PtyWriter,
    output: Mutex<Output>,
    changed: watch::Sender<u64>,
    guard: Mutex<Option<GroupGuard>>,
}

impl Term {
    fn view(&self) -> Value {
        let out = self.output.lock().unwrap();
        json!({
            "id": self.id,
            "cwd": self.cwd.display().to_string(),
            "shell": self.shell,
            "started": self.started,
            "alive": out.exit.is_none(),
            "code": out.exit.flatten(),
        })
    }

    fn push(&self, chunk: &[u8]) {
        let mut out = self.output.lock().unwrap();
        out.bytes.extend(chunk);
        let over = out.bytes.len().saturating_sub(SCROLLBACK);
        out.bytes.drain(..over);
        out.end += chunk.len() as u64;
        let end = out.end;
        drop(out);
        self.changed.send_replace(end);
    }

    /// Bytes from `from` on, with the offset they start at (later than `from` when the
    /// ring has dropped the start).
    fn since(&self, from: u64) -> (u64, Vec<u8>, Option<Option<i32>>) {
        let out = self.output.lock().unwrap();
        let start = out.end - out.bytes.len() as u64;
        let at = from.clamp(start, out.end);
        let skip = (at - start) as usize;
        (at, out.bytes.iter().skip(skip).copied().collect(), out.exit)
    }
}

impl Terminals {
    fn get(&self, id: &str) -> Result<Arc<Term>, ApiError> {
        self.open.lock().unwrap().get(id).cloned().ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no terminal `{id}`")))
    }
}

fn shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|path| !path.is_empty() && std::path::Path::new(path).is_file())
        .or_else(|| ["/bin/bash", "/bin/zsh", "/bin/sh"].iter().find(|p| std::path::Path::new(p).is_file()).map(|p| p.to_string()))
        .unwrap_or_else(|| "/bin/sh".into())
}

pub async fn list(State(app): State<Arc<App>>) -> Reply {
    let open = app.terms.open.lock().unwrap();
    let mut list: Vec<Value> = open.values().map(|t| t.view()).collect();
    list.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    ok(json!({ "terminals": list }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct OpenBody {
    cols: Option<u16>,
    rows: Option<u16>,
}

pub async fn open(State(app): State<Arc<App>>, body: Option<Json<OpenBody>>) -> Reply {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    if !pty::AVAILABLE {
        return Err(fail(StatusCode::NOT_IMPLEMENTED, pty::UNAVAILABLE));
    }
    if app.terms.open.lock().unwrap().len() >= MAX_OPEN {
        return Err(fail(StatusCode::CONFLICT, format!("{MAX_OPEN} terminals are open; close one first")));
    }
    let cwd = app.project();
    let shell = shell();
    let rows = body.rows.unwrap_or(24).clamp(2, 500);
    let cols = body.cols.unwrap_or(80).clamp(10, 1000);
    let env = [("TERM", "xterm-256color"), ("COLORTERM", "truecolor"), ("TASKMASTER_TERMINAL", "1")];
    let child = pty::spawn(std::path::Path::new(&shell), &["-l"], &cwd, env, rows, cols)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let id = {
        let mut next = app.terms.next.lock().unwrap();
        *next += 1;
        format!("term-{}", *next)
    };
    let (changed, _) = watch::channel(0);
    let pty::PtyChild { mut child, guard, writer, mut output } = child;
    let term = Arc::new(Term {
        id: id.clone(),
        cwd,
        shell,
        started: crate::time::rfc3339_now(),
        writer,
        output: Mutex::new(Output { bytes: VecDeque::new(), end: 0, exit: None }),
        changed,
        guard: Mutex::new(Some(guard)),
    });
    app.terms.open.lock().unwrap().insert(id.clone(), term.clone());
    let reading = term.clone();
    tokio::spawn(async move {
        while let Some(chunk) = output.recv().await {
            reading.push(&chunk);
        }
        let code = child.wait().await.ok().and_then(|status| status.code());
        reading.output.lock().unwrap().exit = Some(code);
        reading.guard.lock().unwrap().take();
        let end = reading.output.lock().unwrap().end;
        reading.changed.send_replace(end + 1);
    });
    ok(term.view())
}

#[derive(Deserialize)]
pub struct InputBody {
    data: String,
}

pub async fn input(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<InputBody>) -> Reply {
    let term = app.terms.get(&id)?;
    if body.data.len() > 64 * 1024 {
        return Err(fail(StatusCode::BAD_REQUEST, "send at most 64 KiB at a time"));
    }
    term.writer.write(body.data.as_bytes()).map_err(|err| fail(StatusCode::GONE, format!("the shell has ended: {err}")))?;
    ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
pub struct ResizeBody {
    cols: u16,
    rows: u16,
}

pub async fn resize(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<ResizeBody>) -> Reply {
    let term = app.terms.get(&id)?;
    term.writer
        .resize(body.rows.clamp(2, 500), body.cols.clamp(10, 1000))
        .map_err(|err| fail(StatusCode::GONE, format!("the shell has ended: {err}")))?;
    ok(json!({ "ok": true }))
}

pub async fn close(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let term = app.terms.get(&id)?;
    // Dropping the guard ends the shell's whole process group.
    term.guard.lock().unwrap().take();
    app.terms.open.lock().unwrap().remove(&id);
    ok(json!({ "closed": id }))
}

pub async fn stream(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let token = bearer(&headers).or(query.get("token").map(String::as_str));
    if token.and_then(|token| app.auth.identify(token)).is_none() {
        return Err(fail(StatusCode::UNAUTHORIZED, "a token is required"));
    }
    let term = app.terms.get(&id)?;
    let from = query.get("from").and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    let changed = term.changed.subscribe();
    let events = stream::unfold((term, changed, from, false), |(term, mut changed, from, over)| async move {
        if over {
            return None;
        }
        loop {
            let (at, bytes, exit) = term.since(from);
            if !bytes.is_empty() {
                let next = at + bytes.len() as u64;
                let data = json!({ "at": at, "b64": base64::engine::general_purpose::STANDARD.encode(&bytes) });
                let event = SseEvent::default().event("data").id(next.to_string()).data(data.to_string());
                return Some((Ok(event), (term, changed, next, false)));
            }
            if let Some(code) = exit {
                let event = SseEvent::default().event("exit").data(json!({ "code": code }).to_string());
                return Some((Ok(event), (term, changed, from, true)));
            }
            // A frame's worth of output goes as one event rather than one per read.
            let _ = tokio::time::timeout(Duration::from_secs(15), changed.changed()).await;
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}
