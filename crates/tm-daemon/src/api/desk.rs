//! The parts of the API the web control surface uses beside runs: the agent transcript,
//! notes, the run's terminal, diffs, opening a file in an editor on this machine, a file
//! preview, completions, and the GitHub CLI.
//!
//! ```text
//! GET  /api/runs/{id}/agents      ?after=<n> -> {next, entries}: text, thoughts, tool calls, commands
//! GET  /api/runs/{id}/notes       notes queued for the run's agents
//! POST /api/runs/{id}/notes       {to, text}: the master reads it before any worker, reviewer or verifier
//! POST /api/runs/{id}/notes/{note}  {text} changes a note not yet delivered; {} removes it
//! POST /api/runs/{id}/terminal    {command}: run it in the project as the daemon's user
//! GET  /api/runs/{id}/diff        ?scope=run|page&page=<id>
//! POST /api/runs/{id}/pr          push the run's branch and open a draft pull request
//! GET  /api/runs/{id}/preview     the run's live preview: whether the project has a dev server, and its url
//! POST /api/runs/{id}/preview     start the run's dev server (once; it listens on this machine only)
//! POST /api/runs/{id}/preview/stop
//! GET  /api/approvals             ?run=<id> risky tool calls waiting for a person
//! POST /api/approvals/{id}/{approve|deny}   answer one
//! GET  /api/settings              where this daemon keeps things, the roles the person chose,
//!                                 and what this machine can run at once (`machine.max_parallel`)
//! POST /api/settings              {approvals?: always|risky|never, approval_timeout_secs?, editor?,
//!                                 roles?: {role: harness | ""}, models?, checker?, code_mode?, telemetry?}
//! GET  /api/settings/export       the settings file, with tokens removed
//! POST /api/settings/import       {settings: {...}} replace the saved choices
//! POST /api/settings/reset        forget role, editor, approval, checker, code mode and telemetry choices
//! GET  /api/logs                  the daemon log: path and the last lines
//! GET  /api/editors               the editors this machine can launch, and the default
//! POST /api/open                  {path, line?, column?, editor?, reveal?}
//! POST /api/paths                 {paths: [..]} -> {paths: {raw: full path or null}}
//! GET  /api/file                  ?path=<p> a file's text, for a read-only preview
//! GET  /api/files                 files git knows in the project
//! GET  /api/grep                  ?q=<text> lines in the project's files that contain it
//! GET  /api/gh                    whether gh is installed and signed in, and as whom
//! GET  /api/gh/pr/{number}        one pull request of the project's repository
//! ```
//!
//! A path is relative to the project, or starts with `~/` for this machine's home, or is
//! absolute. Only files under the project or the home directory are opened, previewed or
//! reported as real.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tm_core::{RunPhase, valid_key};

use super::{ApiError, App, Reply, accepted, fail, ok};

const PERSON: &str = "person";
/// How long a command typed into the terminal may run.
const TERMINAL_TIMEOUT: Duration = Duration::from_secs(300);
const KEEP: usize = 64 * 1024;
const PREVIEW_LIMIT: u64 = 512 * 1024;

/// The longest path or command-line value the API accepts.
const MAX_PATH: usize = 4096;

/// Files a preview never shows, wherever they are: keys, credentials, environment files
/// and git's own directory (whose config can hold a token in a remote's address).
pub fn sensitive(path: &Path) -> bool {
    const DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".docker", ".git", ".password-store", ".azure"];
    const NAMES: &[&str] = &[
        ".netrc", ".npmrc", ".pypirc", ".git-credentials", ".pgpass", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519", "credentials",
        "credentials.json", "secrets.env", "taskmaster-credentials.tgz",
    ];
    let parts: Vec<String> = path.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect();
    if parts.iter().any(|part| DIRS.contains(&part.as_str())) {
        return true;
    }
    // GitHub CLI's own login.
    if parts.windows(2).any(|pair| pair[0] == ".config" && pair[1] == "gh") {
        return true;
    }
    let Some(name) = parts.last() else { return false };
    if NAMES.contains(&name.as_str()) || [".pem", ".key", ".p12", ".pfx", ".keystore", ".kdbx"].iter().any(|ext| name.ends_with(ext)) {
        return true;
    }
    let template = [".example", ".sample", ".template", ".dist"].iter().any(|ext| name.ends_with(ext));
    (name == ".env" || name.starts_with(".env.") || name.ends_with(".env")) && !template
}

/// Hide a password or token in an address (`https://user:secret@host/..`) in text that is
/// about to be shown, as git and gh print the remote they could not reach.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail.find(|c: char| c.is_whitespace() || c == '/').unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(i) => {
                out.push_str("***@");
                rest = &tail[i + 1..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The file `raw` names, if it is a real file this daemon may show. A line suffix is
/// the caller's to strip. A path inside one of the project's run worktrees means the same
/// file in the project; while the run still has its worktree, a file that is not in the
/// project yet is found there.
pub fn resolve(project: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_PATH || raw.contains('\0') {
        return None;
    }
    let path = if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        project.join(raw.strip_prefix("./").unwrap_or(raw))
    };
    // `..` is resolved by the filesystem below, so a path cannot leave by it.
    let lexical = normalize(&path);
    if let Some((tree, inner)) = crate::git::in_worktree(project, &lexical) {
        let in_project = project.join(&inner).canonicalize().ok().filter(|full| full.is_file() && full.starts_with(project));
        let in_tree = || tree.join(&inner).canonicalize().ok().filter(|full| full.is_file() && full.starts_with(tree.canonicalize().unwrap_or(tree.clone())));
        return in_project.or_else(in_tree);
    }
    let full = path.canonicalize().ok()?;
    if !full.is_file() {
        return None;
    }
    let allowed = full.starts_with(project) || home().and_then(|h| h.canonicalize().ok()).is_some_and(|h| full.starts_with(h));
    allowed.then_some(full)
}

/// `.` and `..` folded away without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The project-relative form of a file `resolve` found, or its full path when it is outside the project.
pub fn relative(project: &Path, full: &Path) -> String {
    if let Ok(rest) = full.strip_prefix(project) {
        return rest.display().to_string();
    }
    if let Some((_, inner)) = crate::git::in_worktree(project, full) {
        return inner.display().to_string();
    }
    full.display().to_string()
}

// Agents and notes.

/// Output bodies larger than this leave the wire as `{id, bytes}` and load on demand (P5).
pub const LARGE_BODY_BYTES: usize = 2048;
/// Target byte budget for `?window=recent` cold-open responses (never splits a call).
const RECENT_WINDOW_BYTES: usize = 900 * 1024;
/// Throttle interval for per-row live command output SSE frames.
const CMD_OUTPUT_THROTTLE: Duration = Duration::from_millis(100);

/// Replace oversized `output` bodies (`text` or `diff` > 2 KiB) with a stub pointing to
/// `GET /api/runs/{id}/agents/{index}` so large command outputs never ride the main stream.
pub fn stub_large_output(entry: &Value, index: usize) -> Value {
    if entry["kind"].as_str() != Some("output") {
        return entry.clone();
    }
    let text_len = entry["text"].as_str().map(str::len).unwrap_or(0);
    let diff_len = entry["diff"].as_str().map(str::len).unwrap_or(0);
    if text_len <= LARGE_BODY_BYTES && diff_len <= LARGE_BODY_BYTES {
        return entry.clone();
    }
    let mut out = entry.clone();
    if let Some(obj) = out.as_object_mut() {
        if text_len > LARGE_BODY_BYTES {
            obj.remove("text");
            obj.insert("text_bytes".into(), json!(text_len));
        }
        if diff_len > LARGE_BODY_BYTES {
            obj.remove("diff");
            obj.insert("diff_bytes".into(), json!(diff_len));
        }
        obj.insert("bytes".into(), json!(text_len + diff_len));
        obj.insert("body_omitted".into(), json!(true));
        obj.insert("body_index".into(), json!(index));
    }
    out
}

fn call_key(entry: &Value) -> Option<String> {
    let id = entry["agent"]["id"].as_str()?;
    let call = entry["agent"]["call"].as_u64()?;
    if id.is_empty() || id == "runtime" || id == "person" {
        return None;
    }
    Some(format!("{id}#{call}"))
}

fn approx_entry_bytes(entry: &Value) -> usize {
    serde_json::to_string(entry).map(|s| s.len() + 2).unwrap_or(256)
}

fn summarize_older_calls(entries: &[Value], before: usize) -> Vec<Value> {
    struct Acc {
        agent: Value,
        call: u64,
        start_index: usize,
        end_index: usize,
        entries: usize,
        ok: Option<bool>,
        seconds: Option<f64>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, Acc> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate().take(before) {
        let Some(key) = call_key(entry) else { continue };
        let call_num = entry["agent"]["call"].as_u64().unwrap_or(0);
        let acc = by_key.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            Acc {
                agent: entry["agent"].clone(),
                call: call_num,
                start_index: idx,
                end_index: idx,
                entries: 0,
                ok: None,
                seconds: None,
            }
        });
        // Prefer the fuller tag from the call's `start` entry when present.
        if entry["kind"] == "start" {
            acc.agent = entry["agent"].clone();
        }
        acc.end_index = idx;
        acc.entries += 1;
        if entry["kind"] == "end" {
            acc.ok = entry["ok"].as_bool();
            acc.seconds = entry["seconds"].as_f64();
        }
    }
    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .map(|a| {
            json!({
                "agent": a.agent,
                "call": a.call,
                "start_index": a.start_index,
                "end_index": a.end_index,
                "entries": a.entries,
                "ok": a.ok,
                "seconds": a.seconds,
            })
        })
        .collect()
}

/// Choose a start index within `0..end` whose stubbed entries fit in `budget` bytes without
/// ever splitting an agent call across the boundary.
fn bounded_window_start(entries: &[Value], end: usize, budget: usize) -> usize {
    if end == 0 {
        return 0;
    }
    // Map each call to its earliest entry index so including any entry of a call pulls in its start.
    let mut call_first: HashMap<String, usize> = HashMap::new();
    for (i, e) in entries.iter().enumerate().take(end) {
        if let Some(k) = call_key(e) {
            call_first.entry(k).or_insert(i);
        }
    }
    let mut used = 0usize;
    let mut start = end;
    let mut i = end;
    while i > 0 {
        i -= 1;
        let group_start = call_key(&entries[i])
            .and_then(|k| call_first.get(&k).copied())
            .unwrap_or(i)
            .min(i);
        // Compute the byte cost of pulling in `[group_start..=i]` together.
        let mut chunk_bytes = 0usize;
        for (j, raw) in entries.iter().enumerate().take(i + 1).skip(group_start) {
            chunk_bytes += approx_entry_bytes(&stub_large_output(raw, j));
        }
        if used > 0 && used + chunk_bytes > budget {
            break;
        }
        used += chunk_bytes;
        start = group_start;
        i = group_start;
        if used >= budget {
            break;
        }
    }
    start
}

pub async fn agents(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    app.known(&id)?;
    let window = query.get("window").map(String::as_str);
    let before = query.get("before").and_then(|n| n.parse::<usize>().ok());

    if window == Some("recent") || before.is_some() {
        let (next, all) = app.live.read(&id, 0);
        let end = before.map(|b| b.min(all.len())).unwrap_or(all.len());
        let budget = query
            .get("bytes")
            .and_then(|n| n.parse::<usize>().ok())
            .map(|b| b.clamp(16 * 1024, RECENT_WINDOW_BYTES))
            .unwrap_or(RECENT_WINDOW_BYTES);
        let start = bounded_window_start(&all, end, budget);
        let older_calls = if start > 0 {
            summarize_older_calls(&all, start)
        } else {
            Vec::new()
        };
        let slice: Vec<Value> = all
            .iter()
            .enumerate()
            .take(end)
            .skip(start)
            .map(|(idx, e)| stub_large_output(e, idx))
            .collect();
        return ok(json!({
            "next": next,
            "window_start": start,
            "window_end": end,
            "has_older": start > 0,
            "older_calls": older_calls,
            "entries": slice,
        }));
    }

    let after = query.get("after").and_then(|n| n.parse().ok()).unwrap_or(0);
    let stub = query.get("stub").map(String::as_str) == Some("1");
    let (next, entries) = app.live.read(&id, after);
    let entries = if stub {
        entries
            .iter()
            .enumerate()
            .map(|(offset, e)| stub_large_output(e, after + offset))
            .collect()
    } else {
        entries
    };
    ok(json!({ "next": next, "entries": entries }))
}

/// On-demand full body of a single transcript entry (`GET /api/runs/{id}/agents/{index}`, P5).
pub async fn agent_entry(
    State(app): State<Arc<App>>,
    UrlPath((id, index)): UrlPath<(String, usize)>,
) -> Reply {
    app.known(&id)?;
    let Some(entry) = app.live.read_at(&id, index) else {
        return Err(fail(StatusCode::NOT_FOUND, "transcript entry not found"));
    };
    ok(json!({
        "index": index,
        "text": entry.get("text").cloned().unwrap_or(Value::Null),
        "diff": entry.get("diff").cloned().unwrap_or(Value::Null),
        "entry": entry,
    }))
}

/// Per-row throttled SSE stream for a running command's live output (P5).
/// Sends at most one frame every 100 ms; appends up to 16 KiB, then switches to tail replace.
pub async fn command_output_stream(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    app.known(&id)?;
    let tool_id = query
        .get("tool_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "missing tool_id"))?;
    let rx = app.live.subscribe_command_output(&id, &tool_id);
    struct StateFeed {
        app: Arc<App>,
        run: String,
        tool_id: String,
        rx: tokio::sync::watch::Receiver<u64>,
        sent_offset: usize,
        sent_seq: u64,
        done: bool,
    }
    let state = StateFeed {
        app,
        run: id,
        tool_id,
        rx,
        sent_offset: 0,
        sent_seq: 0,
        done: false,
    };
    let stream = stream::unfold(state, |mut st| async move {
        loop {
            if st.done {
                return None;
            }
            if let Some((seq, frame)) = st.app.live.command_output_frame(&st.run, &st.tool_id, st.sent_offset, st.sent_seq) {
                st.sent_seq = seq;
                st.sent_offset = match &frame {
                    crate::live::CommandOutputFrame::Append { next_offset, .. }
                    | crate::live::CommandOutputFrame::Replace { next_offset, .. } => *next_offset,
                };
                let data = serde_json::to_string(&frame).unwrap_or_default();
                tokio::time::sleep(CMD_OUTPUT_THROTTLE).await;
                return Some((Ok(SseEvent::default().event("output").data(data)), st));
            }
            if !st.app.is_active(&st.run) {
                st.done = true;
                return Some((Ok(SseEvent::default().event("done").data("{}")), st));
            }
            tokio::select! {
                changed = st.rx.changed() => {
                    if changed.is_err() {
                        st.done = true;
                        return Some((Ok(SseEvent::default().event("done").data("{}")), st));
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn notes(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    ok(json!({ "notes": app.live.notes(&id) }))
}

#[derive(Deserialize)]
pub struct NoteBody {
    to: String,
    text: String,
    /// On a finished run: `agent`, `code` or `auto` for the turn this note starts.
    #[serde(default)]
    mode: Option<String>,
    /// Queue until the active run settles instead of steering the running master (A1).
    #[serde(default)]
    queued: bool,
}

pub fn valid_recipient(to: &str) -> bool {
    matches!(to, "master" | "workers" | "reviewer" | "verifier") || to.strip_prefix("page:").or_else(|| to.strip_prefix("unit:")).is_some_and(valid_key)
}

pub async fn add_note(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<NoteBody>) -> Reply {
    app.known(&id)?;
    if !valid_recipient(&body.to) {
        return Err(fail(StatusCode::BAD_REQUEST, "a note goes to master, workers, page:<id>, reviewer or verifier"));
    }
    if body.text.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "a note needs text"));
    }
    let mode = body.mode.as_deref().map(str::trim).filter(|m| !m.is_empty());
    if let Some(mode) = mode
        && crate::RunMode::parse(mode).is_none()
    {
        return Err(fail(StatusCode::BAD_REQUEST, format!("unknown mode `{mode}`: use `auto`, `code` or `agent`")));
    }
    let note = app.live.add_note_full(&id, &body.to, body.text.trim(), mode, body.queued)?;
    app.live.notify_events(&id);
    app.wake_for_note(&id)?;
    ok(json!(note))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BatchCommentItem {
    file: String,
    line: u32,
    text: String,
    to: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BatchNoteItem {
    to: String,
    text: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct BatchNotesBody {
    /// Optional pre-grouped notes (`{ to, text }`).
    notes: Vec<BatchNoteItem>,
    /// Optional diff line comments (`{ file, line, text, to? }`) automatically grouped by owning worker.
    comments: Vec<BatchCommentItem>,
    mode: Option<String>,
    queued: bool,
}

/// O3: Batched diff review — groups comments by owning worker (one note per worker carrying every
/// line anchor) and wakes the run once so all comments become a single turn.
pub async fn add_notes_batch(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<BatchNotesBody>,
) -> Reply {
    let run = app.known(&id)?;
    let mode = body.mode.as_deref().map(str::trim).filter(|m| !m.is_empty());
    if let Some(mode) = mode
        && crate::RunMode::parse(mode).is_none()
    {
        return Err(fail(StatusCode::BAD_REQUEST, format!("unknown mode `{mode}`: use `auto`, `code` or `agent`")));
    }

    let mut to_send: Vec<(String, String)> = Vec::new();
    for item in body.notes {
        let to = item.to.trim();
        let text = item.text.trim();
        if !valid_recipient(to) {
            return Err(fail(StatusCode::BAD_REQUEST, format!("invalid note recipient `{to}`")));
        }
        if !text.is_empty() {
            to_send.push((to.to_string(), text.to_string()));
        }
    }

    if !body.comments.is_empty() {
        let mut grouped: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
        for c in body.comments {
            let text = c.text.trim();
            if text.is_empty() {
                continue;
            }
            let target = c
                .to
                .as_deref()
                .map(str::trim)
                .filter(|t| valid_recipient(t))
                .map(str::to_string)
                .unwrap_or_else(|| owner_for_file(&run, c.file.trim()));
            let anchor = if c.file.trim().is_empty() {
                text.to_string()
            } else if c.line > 0 {
                format!("- {}:{} — {}", c.file.trim(), c.line, text)
            } else {
                format!("- {} — {}", c.file.trim(), text)
            };
            grouped.entry(target).or_default().push(anchor);
        }
        for (target, lines) in grouped {
            let combined = format!("Review comments ({}):\n{}", lines.len(), lines.join("\n"));
            to_send.push((target, combined));
        }
    }

    if to_send.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "batch requires at least one note or comment"));
    }

    let mut created = Vec::with_capacity(to_send.len());
    for (to, text) in to_send {
        let note = app.live.add_note_full(&id, &to, &text, mode, body.queued)?;
        created.push(note);
    }
    app.live.notify_events(&id);
    app.wake_for_note(&id)?;
    ok(json!({ "count": created.len(), "notes": created }))
}

fn owner_for_file(run: &tm_core::Run, file: &str) -> String {
    if let Some(plan) = &run.plan {
        for page in &plan.pages {
            if let Ok(glob) = globset::Glob::new(&page.owned_glob)
                && glob.compile_matcher().is_match(file)
            {
                return plan.agent_id(page, None);
            }
        }
        for page in &plan.pages {
            let prefix = page.owned_glob.split('*').next().unwrap_or("").trim_end_matches('/');
            if !prefix.is_empty() && file.starts_with(prefix) {
                return plan.agent_id(page, None);
            }
        }
    }
    "master".to_string()
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct EditNote {
    text: Option<String>,
    queued: Option<bool>,
}

pub async fn edit_note(
    State(app): State<Arc<App>>,
    UrlPath((id, note)): UrlPath<(String, String)>,
    body: Option<Json<EditNote>>,
) -> Reply {
    app.known(&id)?;
    let (text, queued) = match body {
        Some(Json(b)) => (b.text.filter(|t| !t.trim().is_empty()), b.queued),
        None => (None, None),
    };
    if app.live.edit_note_full(&id, &note, text.as_deref(), queued)? {
        app.live.notify_events(&id);
        if queued == Some(false) {
            app.wake_for_note(&id)?;
        }
        ok(json!({ "notes": app.live.notes(&id) }))
    } else {
        Err(fail(StatusCode::CONFLICT, "that note was delivered or does not exist"))
    }
}

// The terminal.

#[derive(Deserialize)]
pub struct TerminalBody {
    command: String,
}

/// A value from a request, cut short before it goes into a message.
fn clip_text(text: &str) -> String {
    text.chars().take(200).collect()
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

/// A command a person typed. It runs in the project as the daemon's user, which is the
/// person who started the daemon. Page workers never reach this: their shell stays denied.
pub async fn terminal(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<TerminalBody>) -> Reply {
    app.known(&id)?;
    let command = body.command.trim().to_string();
    if command.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "type a command"));
    }
    let agent = json!({ "id": PERSON, "role": PERSON });
    let cwd = app.project().display().to_string();
    app.live.push(&id, &agent, "command", json!({ "command": command, "cwd": cwd, "typed": true }));
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(app.project())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let (code, text) = match tokio::time::timeout(TERMINAL_TIMEOUT, child).await {
        Ok(Ok(out)) => {
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            (out.status.code(), tail(text.trim_end()))
        }
        Ok(Err(err)) => (None, format!("could not start the command: {err}")),
        Err(_) => (None, format!("stopped after {} seconds", TERMINAL_TIMEOUT.as_secs())),
    };
    let ok_ = code == Some(0);
    app.live.push(&id, &agent, "output", json!({ "ok": ok_, "code": code, "text": text, "typed": true }));
    ok(json!({ "ok": ok_, "code": code, "output": text }))
}

// Diffs.

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|err| format!("cannot run git: {err}"))?;
    if !out.status.success() {
        return Err(redact(String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn diff(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    app.known(&id)?;
    let meta = app.records().meta(&id)?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "this run has not started building"))?;
    let (Some(base), Some(branch)) = (meta.base.clone(), meta.branch.clone()) else {
        return Err(fail(StatusCode::CONFLICT, "this run built in the project's own working tree, so it has no branch to compare"));
    };
    let scope = query.get("scope").map(String::as_str).unwrap_or("run");
    // After a merge the branch is gone; its last commit was kept.
    let head = run_head(&app, &id, &branch).ok_or_else(|| fail(StatusCode::CONFLICT, format!("branch `{branch}` is gone and its last commit was not kept")))?;
    let mut args: Vec<String> = vec!["diff".into(), "--no-color".into(), base.clone(), head];
    match scope {
        "run" => {}
        "page" => {
            let page = query.get("page").filter(|page| valid_key(page)).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "name a page"))?;
            let mut files: Vec<String> = app.records().changes(&id)?
                .into_iter()
                .filter(|change| change.page.as_deref() == Some(page.as_str()))
                .flat_map(|change| change.files)
                .collect();
            files.sort();
            files.dedup();
            if files.is_empty() {
                return ok(json!({ "scope": scope, "base": base, "branch": branch, "diff": "" }));
            }
            args.push("--".into());
            args.extend(files);
        }
        _ => return Err(fail(StatusCode::BAD_REQUEST, "scope is run or page; an attempt's diff is in its attempts")),
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let text = git(&app.project(), &refs).map_err(|err| fail(StatusCode::CONFLICT, err))?;
    let text = crate::git::unroot_worktrees(&app.project(), &text);
    ok(json!({ "scope": scope, "base": base, "branch": branch, "diff": text }))
}

/// Fast per-page `+added / -removed` line counts for a run branch using `git diff --numstat`.
pub async fn diff_stats(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    let Some(meta) = app.records().meta(&id)? else {
        return ok(json!({ "pages": {}, "total": { "added": 0, "removed": 0, "files": 0 } }));
    };
    let (Some(base), Some(branch)) = (meta.base.clone(), meta.branch.clone()) else {
        return ok(json!({ "pages": {}, "total": { "added": 0, "removed": 0, "files": 0 } }));
    };
    let Some(head) = run_head(&app, &id, &branch) else {
        return ok(json!({ "pages": {}, "total": { "added": 0, "removed": 0, "files": 0 } }));
    };
    let Ok(raw) = git(&app.project(), &["diff", "--numstat", &base, &head]) else {
        return ok(json!({ "pages": {}, "total": { "added": 0, "removed": 0, "files": 0 } }));
    };
    // Map each file path to the page that changed it (from change records, falling back to plan globs).
    let mut file_owner: HashMap<String, String> = HashMap::new();
    if let Ok(changes) = app.records().changes(&id) {
        for change in changes {
            if let Some(page) = change.page {
                for file in change.files {
                    file_owner.insert(file, page.clone());
                }
            }
        }
    }
    let owners = app
        .records()
        .events(&id)
        .ok()
        .and_then(|evs| tm_core::Run::replay(1, evs).plan)
        .and_then(|plan| tm_verify::ownership::Owners::from_plan(&plan).ok());
    let mut pages: HashMap<String, (u64, u64, u64)> = HashMap::new();
    let mut total_add = 0u64;
    let mut total_del = 0u64;
    let mut total_files = 0u64;
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let add = a.parse::<u64>().unwrap_or(0);
        let del = d.parse::<u64>().unwrap_or(0);
        total_add += add;
        total_del += del;
        total_files += 1;
        let owner = file_owner
            .get(path)
            .cloned()
            .or_else(|| owners.as_ref().and_then(|o| o.owner(path).cloned()));
        if let Some(page) = owner {
            let entry = pages.entry(page).or_default();
            entry.0 += add;
            entry.1 += del;
            entry.2 += 1;
        }
    }
    let pages_json: serde_json::Map<String, serde_json::Value> = pages
        .into_iter()
        .map(|(k, (added, removed, files))| (k, json!({ "added": added, "removed": removed, "files": files })))
        .collect();
    ok(json!({
        "pages": pages_json,
        "total": { "added": total_add, "removed": total_del, "files": total_files },
    }))
}

/// What to compare against: the run branch while it exists, else the commit it ended on.
pub fn run_head(app: &App, id: &str, branch: &str) -> Option<String> {
    if git(&app.project(), &["rev-parse", "--verify", "--quiet", &format!("{branch}^{{commit}}")]).is_ok() {
        return Some(branch.to_string());
    }
    app.live.tip(id)
}

// The GitHub CLI. `TASKMASTER_GH` names another program, for tests.

fn gh_program() -> String {
    std::env::var("TASKMASTER_GH").unwrap_or_else(|_| "gh".into())
}

fn gh(project: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new(gh_program())
        .args(args)
        .current_dir(project)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "gh is not installed".to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(redact(text.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `{installed, account}`. The account is `None` when gh is not signed in.
fn gh_account(project: &Path) -> Value {
    if which(&gh_program()).is_none() {
        return json!({ "installed": false, "account": null });
    }
    match gh(project, &["api", "user", "--jq", ".login"]) {
        Ok(login) if !login.trim().is_empty() => json!({ "installed": true, "account": login.trim() }),
        Ok(_) => json!({ "installed": true, "account": null }),
        Err(err) => json!({ "installed": true, "account": null, "error": err }),
    }
}

pub async fn gh_status(State(app): State<Arc<App>>) -> Reply {
    let project = app.project().clone();
    let status = tokio::task::spawn_blocking(move || gh_account(&project)).await.unwrap_or(Value::Null);
    ok(status)
}

pub async fn gh_pr(State(app): State<Arc<App>>, UrlPath(number): UrlPath<u64>) -> Reply {
    let project = app.project().clone();
    let found = tokio::task::spawn_blocking(move || {
        gh(&project, &["pr", "view", &number.to_string(), "--json", "number,title,url,state"])
    })
    .await
    .unwrap_or_else(|_| Err("gh stopped".into()));
    match found {
        Ok(text) => ok(serde_json::from_str(&text).unwrap_or(Value::Null)),
        Err(err) if err.contains("not installed") => Err(fail(StatusCode::FAILED_DEPENDENCY, err)),
        Err(err) => Err(fail(StatusCode::NOT_FOUND, err)),
    }
}

/// The title and body of a run's pull request, from its locked plan.
pub fn pr_text(id: &str, prompt: Option<&str>, plan: &tm_core::Plan) -> (String, String) {
    let first = prompt.and_then(|p| p.lines().map(str::trim).find(|l| !l.is_empty())).unwrap_or("");
    let pages: Vec<&str> = plan.pages.iter().map(|page| page.id.as_str()).collect();
    let mut title = if first.is_empty() { format!("Taskmaster {id}: {}", pages.join(", ")) } else { first.to_string() };
    if title.chars().count() > 72 {
        title = title.chars().take(71).collect::<String>() + "…";
    }
    let mut body = format!("Built by Taskmaster run `{id}` from its locked plan.\n\n");
    if !first.is_empty() {
        body.push_str(&format!("> {first}\n\n"));
    }
    body.push_str("| Page | Route | Owns | Cites |\n| --- | --- | --- | --- |\n");
    for page in &plan.pages {
        let cites = if page.cites.is_empty() { "none".to_string() } else { page.cites.join(", ") };
        body.push_str(&format!("| `{}` | `{}` | `{}` | {} |\n", page.id, page.route, page.owned_glob, cites));
    }
    if !plan.contracts.is_empty() {
        body.push_str("\nContracts:\n");
        for contract in &plan.contracts {
            body.push_str(&format!("- `{}`\n", contract.path));
        }
    }
    body.push_str(&format!("\nEstimate: about {} tokens, ${:.2}.\n", plan.estimate.tokens, plan.estimate.usd));
    (title, body)
}

pub async fn open_pr(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let run = app.known(&id)?;
    if run.phase != RunPhase::Done {
        return Err(fail(StatusCode::CONFLICT, "a pull request is drafted once the run is done"));
    }
    let plan = run.plan.clone().ok_or_else(|| fail(StatusCode::CONFLICT, "this run has no plan"))?;
    let meta = app.records().meta(&id)?;
    let branch = meta.and_then(|meta| meta.branch).ok_or_else(|| fail(StatusCode::CONFLICT, "this run has no branch"))?;
    let prompt = app.request_for(&id)?.prompt;
    let (title, body) = pr_text(&id, prompt.as_deref(), &plan);
    let head = run_head(&app, &id, &branch).ok_or_else(|| fail(StatusCode::CONFLICT, format!("branch `{branch}` is gone and its last commit was not kept")))?;
    let project = app.project().clone();
    let made = tokio::task::spawn_blocking(move || -> Result<String, String> {
        if gh_account(&project)["account"].is_null() {
            return Err("gh is not signed in on this machine".into());
        }
        // A merged run's branch was deleted locally; push the commit it ended on under its name.
        let refspec = if head == branch { branch.clone() } else { format!("{head}:refs/heads/{branch}") };
        git(&project, &["push", "-u", "origin", &refspec])?;
        let out = gh(&project, &["pr", "create", "--draft", "--head", &branch, "--title", &title, "--body", &body])?;
        Ok(out.lines().map(str::trim).rfind(|line| line.starts_with("http")).unwrap_or(out.trim()).to_string())
    })
    .await
    .unwrap_or_else(|_| Err("stopped".into()));
    match made {
        Ok(url) => {
            // A8: the pull request's merge settles the run.
            app.remember_pr(&id, &url);
            ok(json!({ "url": url }))
        }
        Err(err) => Err(fail(StatusCode::CONFLICT, err)),
    }
}

// Editors.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Style {
    /// `code -g file:line:column` and the editors built on it.
    Goto,
    /// `zed file:line:column`.
    Suffix,
    /// `idea --line L --column C file`.
    JetBrains,
    /// `program file`, for an editor this daemon knows nothing more about.
    Plain,
    /// The file manager, at the folder.
    Files,
}

pub struct Editor {
    pub id: &'static str,
    pub name: &'static str,
    pub command: &'static str,
    pub style: Style,
}

const fn editor(id: &'static str, name: &'static str, command: &'static str, style: Style) -> Editor {
    Editor { id, name, command, style }
}

pub const EDITORS: &[Editor] = &[
    editor("cursor", "Cursor", "cursor", Style::Goto),
    editor("trae", "Trae", "trae", Style::Goto),
    editor("kiro", "Kiro", "kiro", Style::Goto),
    editor("vscode", "VS Code", "code", Style::Goto),
    editor("vscode-insiders", "VS Code Insiders", "code-insiders", Style::Goto),
    editor("vscodium", "VSCodium", "codium", Style::Goto),
    editor("zed", "Zed", "zed", Style::Suffix),
    editor("antigravity", "Antigravity", "antigravity", Style::Goto),
    editor("idea", "IntelliJ IDEA", "idea", Style::JetBrains),
    editor("clion", "CLion", "clion", Style::JetBrains),
    editor("goland", "GoLand", "goland", Style::JetBrains),
    editor("pycharm", "PyCharm", "pycharm", Style::JetBrains),
    editor("rustrover", "RustRover", "rustrover", Style::JetBrains),
    editor("webstorm", "WebStorm", "webstorm", Style::JetBrains),
    editor("phpstorm", "PhpStorm", "phpstorm", Style::JetBrains),
    editor("rubymine", "RubyMine", "rubymine", Style::JetBrains),
    editor("rider", "Rider", "rider", Style::JetBrains),
    editor("datagrip", "DataGrip", "datagrip", Style::JetBrains),
    editor("dataspell", "DataSpell", "dataspell", Style::JetBrains),
    editor("aqua", "Aqua", "aqua", Style::JetBrains),
    editor("files", "File manager", if cfg!(target_os = "macos") { "open" } else { "xdg-open" }, Style::Files),
];

/// Where editors are usually installed that a daemon's own `PATH` may leave out.
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["/usr/local/bin", "/opt/homebrew/bin", "/snap/bin", "/var/lib/flatpak/exports/bin"]
        .iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = home() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join("bin"));
    }
    for app in [
        "/Applications/Cursor.app/Contents/Resources/app/bin",
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin",
        "/Applications/Zed.app/Contents/MacOS",
        "/Applications/VSCodium.app/Contents/Resources/app/bin",
    ] {
        dirs.push(PathBuf::from(app));
    }
    dirs
}

/// A program by name: on `PATH` first, then where editors usually are. A name with a
/// slash is a path (`~/` allowed).
pub fn which(program: &str) -> Option<PathBuf> {
    if let Some(rest) = program.strip_prefix("~/") {
        let path = home()?.join(rest);
        return path.is_file().then_some(path);
    }
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    let on_path = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    let names = tm_harness::sandbox::program_names(program);
    on_path.into_iter().chain(extra_dirs()).flat_map(|dir| names.iter().map(move |name| dir.join(name))).find(|path| path.is_file())
}

/// Editors that need a terminal of their own, which a daemon cannot give them.
const TERMINAL_EDITORS: &[&str] = &["vi", "vim", "nvim", "nano", "emacs", "pico", "micro", "hx", "helix", "kak", "ed", "joe", "mcedit"];

/// An editor to launch: where it is, how to give it a file, and why it was chosen.
#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub id: String,
    pub name: String,
    pub program: PathBuf,
    /// Arguments that came with it, such as `$EDITOR`'s `--reuse-window`.
    pub extra: Vec<String>,
    pub style: Style,
    pub terminal: bool,
    /// `settings`, `$VISUAL`, `$EDITOR`, `installed` or `files`.
    pub source: &'static str,
}

fn known(editor: &Editor, program: PathBuf, source: &'static str) -> Chosen {
    Chosen { id: editor.id.into(), name: editor.name.into(), program, extra: vec![], style: editor.style, terminal: false, source }
}

/// An editor named by the settings file, `$VISUAL` or `$EDITOR`: a known id (`cursor`), a
/// command line (`code --reuse-window`), or any program, by name or by path.
pub fn editor_from_value(value: &str, source: &'static str) -> Option<Chosen> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) || value.len() > 300 {
        return None;
    }
    if let Some(editor) = EDITORS.iter().find(|e| e.id == value) {
        return which(editor.command).map(|program| known(editor, program, source));
    }
    let mut words = value.split_whitespace();
    let program_word = words.next()?;
    // Waiting for the editor to close would hold the daemon's request open.
    let extra: Vec<String> = words.filter(|w| !matches!(*w, "-w" | "--wait" | "-W")).map(str::to_string).collect();
    let program = which(program_word)?;
    let base = program.file_name()?.to_string_lossy().into_owned();
    let stem = base.strip_suffix(".exe").unwrap_or(&base);
    if let Some(editor) = EDITORS.iter().find(|e| e.command == stem || e.id == stem) {
        let mut chosen = known(editor, program, source);
        chosen.extra = extra;
        return Some(chosen);
    }
    Some(Chosen {
        id: "custom".into(),
        name: stem.to_string(),
        program,
        extra,
        style: Style::Plain,
        terminal: TERMINAL_EDITORS.contains(&stem),
        source,
    })
}

/// The editor to use when a request names none: the settings file's, then `$VISUAL`, then
/// `$EDITOR` (these two only when a window can open from them), then the first installed of
/// Cursor, VS Code and Zed, then the file manager.
pub fn choose_editor(setting: &str, visual: Option<&str>, editor: Option<&str>) -> Option<Chosen> {
    if let Some(chosen) = editor_from_value(setting, "settings") {
        return Some(chosen);
    }
    for (value, source) in [(visual, "$VISUAL"), (editor, "$EDITOR")] {
        if let Some(chosen) = value.and_then(|v| editor_from_value(v, source)).filter(|c| !c.terminal) {
            return Some(chosen);
        }
    }
    for id in ["cursor", "vscode", "zed"] {
        if let Some(editor) = EDITORS.iter().find(|e| e.id == id)
            && let Some(program) = which(editor.command)
        {
            return Some(known(editor, program, "installed"));
        }
    }
    let files = EDITORS.iter().find(|e| e.id == "files")?;
    which(files.command).map(|program| known(files, program, "files"))
}

fn env_editor(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn current_editor(app: &App) -> Option<Chosen> {
    let setting = app.editor.lock().unwrap().clone();
    choose_editor(&setting, env_editor("VISUAL").as_deref(), env_editor("EDITOR").as_deref())
}

/// The arguments that open `file` at a line and column, or show it in its folder.
pub fn editor_args(style: Style, file: &Path, line: Option<u32>, column: Option<u32>, reveal: bool) -> Vec<String> {
    let path = file.display().to_string();
    let at = match (line, column) {
        (Some(line), Some(column)) => format!("{path}:{line}:{column}"),
        (Some(line), None) => format!("{path}:{line}"),
        _ => path.clone(),
    };
    if reveal || style == Style::Files {
        return if cfg!(target_os = "macos") {
            vec!["-R".into(), path]
        } else {
            vec![file.parent().map(|dir| dir.display().to_string()).unwrap_or(path)]
        };
    }
    match style {
        Style::Goto => vec!["-g".into(), at],
        Style::Suffix => vec![at],
        Style::JetBrains => {
            let mut args = Vec::new();
            if let Some(line) = line {
                args.extend(["--line".to_string(), line.to_string()]);
            }
            if let Some(column) = column {
                args.extend(["--column".to_string(), column.to_string()]);
            }
            args.push(path);
            args
        }
        Style::Plain => vec![path],
        Style::Files => unreachable!(),
    }
}

pub async fn editors(State(app): State<Arc<App>>) -> Reply {
    let mut list: Vec<Value> = EDITORS
        .iter()
        .map(|e| json!({ "id": e.id, "name": e.name, "command": e.command, "installed": which(e.command).is_some() }))
        .collect();
    let chosen = current_editor(&app);
    // An editor that came from the settings or the environment and is not one of the above.
    if let Some(custom) = chosen.as_ref().filter(|c| c.id == "custom") {
        list.push(json!({ "id": "custom", "name": custom.name, "command": custom.program.display().to_string(), "installed": true, "terminal": custom.terminal }));
    }
    let setting = app.editor.lock().unwrap().clone();
    ok(json!({
        "editors": list,
        "default": chosen.as_ref().map(|c| c.id.as_str()).unwrap_or("files"),
        "source": chosen.as_ref().map(|c| c.source),
        "override": setting,
        "override_found": setting.is_empty() || editor_from_value(&setting, "settings").is_some(),
        "env": { "VISUAL": env_editor("VISUAL"), "EDITOR": env_editor("EDITOR") },
    }))
}

#[derive(Deserialize)]
pub struct OpenBody {
    path: String,
    line: Option<u32>,
    column: Option<u32>,
    editor: Option<String>,
    #[serde(default)]
    reveal: bool,
}

fn failed_dependency(message: String, command: &str, path: &str) -> Reply {
    let reply = json!({ "error": message, "command": command, "path": path });
    Ok(axum::response::IntoResponse::into_response((StatusCode::FAILED_DEPENDENCY, Json(reply))))
}

/// Open a file in an editor on this machine, the one that holds the project.
pub async fn open(State(app): State<Arc<App>>, Json(body): Json<OpenBody>) -> Reply {
    let Some(file) = resolve(&app.project(), &body.path) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("{} is not a file here", clip_text(&body.path))));
    };
    let full = file.display().to_string();
    let wanted = body.editor.as_deref().filter(|id| !id.is_empty());
    let chosen = if body.reveal {
        let files = EDITORS.iter().find(|e| e.id == "files").expect("the file manager is listed");
        which(files.command).map(|program| known(files, program, "files"))
    } else {
        match wanted {
            None | Some("custom") => current_editor(&app),
            Some(id) => match EDITORS.iter().find(|e| e.id == id) {
                Some(editor) => which(editor.command).map(|program| known(editor, program, "request")),
                None => return Err(fail(StatusCode::BAD_REQUEST, format!("unknown editor `{id}`"))),
            },
        }
    };
    let Some(chosen) = chosen else {
        let name = match wanted {
            Some(id) => EDITORS.iter().find(|e| e.id == id).map(|e| e.command).unwrap_or(id),
            None => "an editor",
        };
        return failed_dependency(format!("`{name}` is not installed on this machine"), name, &full);
    };
    let command = chosen.program.display().to_string();
    if chosen.terminal {
        return failed_dependency(format!("`{}` runs in a terminal; choose a window editor in Settings", chosen.name), &command, &full);
    }
    let mut args = chosen.extra.clone();
    args.extend(editor_args(chosen.style, &file, body.line, body.column, body.reveal));
    let spawned = std::process::Command::new(&chosen.program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            // Reap it whenever it exits, so it does not linger.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            accepted(json!({ "command": command, "editor": chosen.id, "source": chosen.source, "args": args, "path": full }))
        }
        Err(err) => failed_dependency(format!("`{command}` did not start: {err}"), &command, &full),
    }
}

#[derive(Deserialize)]
pub struct PathsBody {
    paths: Vec<String>,
}

pub async fn paths(State(app): State<Arc<App>>, Json(body): Json<PathsBody>) -> Reply {
    let (mut out, mut rel) = (serde_json::Map::new(), serde_json::Map::new());
    for raw in body.paths.into_iter().take(500) {
        let found = resolve(&app.project(), &raw);
        rel.insert(raw.clone(), json!(found.as_ref().map(|path| relative(&app.project(), path))));
        out.insert(raw, json!(found.map(|path| path.display().to_string())));
    }
    ok(json!({ "paths": out, "relative": rel, "project": app.project().display().to_string() }))
}

pub async fn file(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let raw = query.get("path").cloned().unwrap_or_default();
    let Some(file) = resolve(&app.project(), &raw) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("{} is not a file here", clip_text(&raw))));
    };
    if sensitive(&file) {
        return Err(fail(StatusCode::FORBIDDEN, "keys, credentials, environment files and .git are never previewed"));
    }
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    // Only the part that is shown is read, however large the file is.
    let mut bytes = Vec::new();
    {
        use std::io::Read;
        std::fs::File::open(&file)
            .and_then(|f| f.take(PREVIEW_LIMIT).read_to_end(&mut bytes))
            .map_err(|err| fail(StatusCode::NOT_FOUND, err.to_string()))?;
    }
    let cut = &bytes[..];
    if cut.contains(&0) {
        return Err(fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a binary file has no text preview"));
    }
    ok(json!({
        "path": raw,
        "relative": relative(&app.project(), &file),
        "full": file.display().to_string(),
        "content": String::from_utf8_lossy(cut),
        "truncated": size > PREVIEW_LIMIT,
    }))
}

pub async fn files(State(app): State<Arc<App>>) -> Reply {
    let text = git(&app.project(), &["ls-files", "--cached", "--others", "--exclude-standard"]).unwrap_or_default();
    let list: Vec<&str> = text.lines().take(20_000).collect();
    ok(json!({ "files": list }))
}

/// Search the text of the project's files: tracked and untracked, ignoring what git
/// ignores, binary files and anything `sensitive` would hide.
pub async fn grep(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let q = query.get("q").map(|q| q.trim()).unwrap_or("");
    if q.is_empty() {
        return ok(json!({ "hits": [], "truncated": false }));
    }
    if q.len() > 200 {
        return Err(fail(StatusCode::BAD_REQUEST, "search for at most 200 characters"));
    }
    let project = app.project();
    let q = q.to_string();
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["grep", "--untracked", "--exclude-standard", "-I", "-n", "--column", "-i", "-F", "--no-color", "-e"])
            .arg(&q)
            .output()
    })
    .await
    .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))?
    .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot run git: {err}")))?;
    const LIMIT: usize = 400;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut hits = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        let mut parts = line.splitn(4, ':');
        let (Some(path), Some(n), Some(col), Some(body)) = (parts.next(), parts.next(), parts.next(), parts.next()) else { continue };
        if sensitive(Path::new(path)) {
            continue;
        }
        if hits.len() == LIMIT {
            truncated = true;
            break;
        }
        let body: String = body.chars().take(240).collect();
        hits.push(json!({ "path": path, "line": n.parse::<u32>().unwrap_or(0), "column": col.parse::<u32>().unwrap_or(0), "text": redact(&body) }));
    }
    ok(json!({ "hits": hits, "truncated": truncated }))
}

// The live preview.

fn preview_view(app: &App, id: &str) -> Value {
    let status = app.previews.status(id);
    json!({
        "available": app.previews.available(),
        "state": status.as_ref().map(|s| s.state).unwrap_or("none"),
        "url": status.as_ref().filter(|s| matches!(s.state, "starting" | "running")).map(|s| s.url.clone()),
        "port": status.as_ref().map(|s| s.port),
        "command": status.as_ref().map(|s| s.command.clone()),
        "reason": status.as_ref().and_then(|s| s.reason.clone()),
        "output": status.as_ref().filter(|s| s.state == "failed").map(|_| app.previews.output(id)),
        "started": status.as_ref().map(|s| s.started.clone()),
    })
}

pub async fn preview(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    ok(preview_view(&app, &id))
}

pub async fn start_preview(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let run = app.known(&id)?;
    let meta = app.records().meta(&id)?;
    if meta.is_none() || matches!(run.phase, RunPhase::Planning | RunPhase::AwaitingApproval) && run.pages.values().all(|p| p.state != tm_core::PageState::Passed) {
        return Err(fail(StatusCode::CONFLICT, "there is nothing to preview until the run has started building"));
    }
    // A run still on its branch is previewed from that branch (a detached checkout, so it
    // works while the run builds: it shows what has passed so far); a merged one from the
    // project.
    let branch = meta.and_then(|m| m.branch).filter(|branch| crate::git::branch_exists(&app.project(), branch));
    match app.previews.start(&id, branch.as_deref()).await {
        Ok(_) => accepted(preview_view(&app, &id)),
        Err(err) => Err(fail(StatusCode::CONFLICT, format!("{err:#}"))),
    }
}

pub async fn stop_preview(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    app.previews.stop(&id);
    ok(preview_view(&app, &id))
}

/// O2: Same-origin proxy for the run's running preview server so Design Mode can inspect,
/// highlight, and capture DOM elements inside the iframe without cross-origin restrictions.
pub async fn preview_proxy_root(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    preview_proxy_inner(app, id, String::new(), query, headers).await
}

pub async fn preview_proxy_path(
    State(app): State<Arc<App>>,
    UrlPath((id, path)): UrlPath<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    preview_proxy_inner(app, id, path, query, headers).await
}

async fn preview_proxy_inner(
    app: Arc<App>,
    id: String,
    rel_path: String,
    query: HashMap<String, String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let token = super::bearer(&headers).or(query.get("token").map(String::as_str));
    // Allow tokenless asset sub-requests when the request comes from our same-origin proxy Referer.
    let referer_ok = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|r| r.contains(&format!("/api/runs/{id}/preview/proxy")));
    if !referer_ok && token.and_then(|t| app.auth.identify(t)).is_none() {
        return fail(StatusCode::UNAUTHORIZED, "a token is required").into_response();
    }
    if app.known(&id).is_err() {
        return fail(StatusCode::NOT_FOUND, "run not found").into_response();
    }
    let Some(status) = app.previews.status(&id).filter(|s| s.state == "running") else {
        return fail(StatusCode::CONFLICT, "the preview is not running").into_response();
    };
    let port = status.port;
    let target_path = if rel_path.is_empty() {
        "/".to_string()
    } else if rel_path.starts_with('/') {
        rel_path
    } else {
        format!("/{rel_path}")
    };
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return fail(StatusCode::BAD_GATEWAY, "could not reach the preview server").into_response(),
    };
    let req = format!(
        "GET {target_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).await.is_err() {
        return fail(StatusCode::BAD_GATEWAY, "preview write failed").into_response();
    }
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw)).await;
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return fail(StatusCode::BAD_GATEWAY, "malformed preview response").into_response();
    };
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut body = raw[split + 4..].to_vec();
    let mut status_code = StatusCode::OK;
    let mut content_type = "text/html; charset=utf-8".to_string();
    let mut chunked = false;
    for (idx, line) in head.lines().enumerate() {
        if idx == 0 {
            if let Some(code) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u16>().ok()) {
                status_code = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-type") {
                content_type = v.trim().to_string();
            } else if k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    if chunked {
        body = decode_chunked(&body);
    }
    if content_type.to_ascii_lowercase().starts_with("text/html") {
        let html = String::from_utf8_lossy(&body);
        if !html.contains("<base ") {
            let base = format!("<base href=\"/api/runs/{id}/preview/proxy/\">");
            let rewritten = if let Some(pos) = html.to_ascii_lowercase().find("<head>") {
                format!("{}{}{}", &html[..pos + 6], base, &html[pos + 6..])
            } else {
                format!("{base}{html}")
            };
            body = rewritten.into_bytes();
        }
    }
    (status_code, [(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
}

fn decode_chunked(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < input.len() {
        let Some(crlf) = input[i..].windows(2).position(|w| w == b"\r\n") else { break };
        let size_str = String::from_utf8_lossy(&input[i..i + crlf]);
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else { break };
        i += crlf + 2;
        if size == 0 || i + size > input.len() {
            break;
        }
        out.extend_from_slice(&input[i..i + size]);
        i += size + 2;
    }
    out
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct PreviewCaptureBody {
    selector: String,
    outer_html: String,
    #[serde(default)]
    styles: HashMap<String, String>,
    #[serde(default)]
    path: String,
    #[serde(default)]
    source_file: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    screenshot_data_url: Option<String>,
}

/// O2: Receive a Design Mode element capture from the preview iframe, resolve its owning worker
/// (from `source_file`, `.map` source maps, route, and `owned_glob`), and return the capture chip.
pub async fn preview_capture(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<PreviewCaptureBody>,
) -> Reply {
    let run = app.known(&id)?;
    let selector = body.selector.trim().chars().take(240).collect::<String>();
    if selector.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "selector is required"));
    }
    let outer_html: String = body.outer_html.trim().chars().take(800).collect();
    let source_file = body
        .source_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| resolve_source_map_file(&app.project(), &body.path));
    let (owner, page) = resolve_preview_owner(
        &run,
        source_file.as_deref(),
        body.path.trim(),
        body.text.as_deref().unwrap_or(""),
    );
    ok(json!({
        "owner": owner,
        "page": page,
        "source_file": source_file,
        "selector": selector,
        "outer_html": outer_html,
        "styles": body.styles,
        "path": body.path,
        "screenshot_data_url": body.screenshot_data_url,
    }))
}

fn resolve_source_map_file(project: &Path, route_path: &str) -> Option<String> {
    let clean = route_path.trim_start_matches('/');
    if clean.is_empty() {
        return None;
    }
    let candidate = project.join(format!("{clean}.map"));
    let raw = std::fs::read_to_string(candidate).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed["sources"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches("../").trim_start_matches("./").to_string())
}

fn resolve_preview_owner(
    run: &tm_core::Run,
    source_file: Option<&str>,
    route_path: &str,
    _text: &str,
) -> (String, Option<String>) {
    let Some(plan) = &run.plan else {
        return ("master".to_string(), None);
    };
    if let Some(sf) = source_file {
        for page in &plan.pages {
            if let Ok(glob) = globset::Glob::new(&page.owned_glob)
                && glob.compile_matcher().is_match(sf)
            {
                return (plan.agent_id(page, None), Some(page.id.clone()));
            }
        }
    }
    let normalized_route = if route_path.is_empty() { "/" } else { route_path };
    for page in &plan.pages {
        if page.route == normalized_route || (!page.route.is_empty() && normalized_route.starts_with(&page.route) && page.route != "/") {
            return (plan.agent_id(page, None), Some(page.id.clone()));
        }
    }
    let seg = normalized_route.trim_matches('/').split('/').next().unwrap_or("");
    if !seg.is_empty()
        && let Some(page) = plan.pages.iter().find(|p| p.id.eq_ignore_ascii_case(seg))
    {
        return (plan.agent_id(page, None), Some(page.id.clone()));
    }
    if let Some(first) = plan.pages.first() {
        return (plan.agent_id(first, None), Some(first.id.clone()));
    }
    ("master".to_string(), None)
}

// Approvals.

pub async fn approvals(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let run = query.get("run").map(String::as_str);
    if let Some(run) = run
        && !valid_key(run)
    {
        return Err(fail(StatusCode::BAD_REQUEST, "run is not a run id"));
    }
    let policy = app.live.approvals.policy();
    ok(json!({ "approvals": app.live.approvals.list(run), "mode": policy.mode.id(), "timeout_secs": policy.timeout.as_secs() }))
}

pub async fn decide(State(app): State<Arc<App>>, UrlPath((id, decision)): UrlPath<(String, String)>) -> Reply {
    let allow = match decision.as_str() {
        "approve" => true,
        "deny" => false,
        _ => return Err(fail(StatusCode::NOT_FOUND, "answer with approve or deny")),
    };
    match app.live.approvals.answer(&id, allow, "person") {
        Some(view) => {
            if let Some(run) = view["run"].as_str() {
                app.live.notify_events(run);
            }
            ok(json!({ "id": id, "allowed": allow, "approval": view }))
        }
        None => Err(fail(StatusCode::CONFLICT, "that question was already answered, timed out, or its run ended")),
    }
}

// Project worktree config (`taskmaster.yaml`, O1).

fn project_config_view(project: &Path) -> Value {
    let path = project.join(crate::project_config::CONFIG_FILE);
    match crate::project_config::load(project) {
        Ok(Some(loaded)) => json!({
            "exists": true,
            "path": path.display().to_string(),
            "raw": loaded.raw,
            "config": loaded.config,
            "hash": loaded.hash,
            "has_commands": loaded.has_commands,
            "trusted": loaded.trusted,
        }),
        Ok(None) => json!({
            "exists": false,
            "path": path.display().to_string(),
            "trusted": true,
            "has_commands": false,
        }),
        Err(err) => json!({
            "exists": path.is_file(),
            "path": path.display().to_string(),
            "trusted": false,
            "has_commands": true,
            "error": format!("{err:#}"),
        }),
    }
}

pub async fn project_config(State(app): State<Arc<App>>) -> Reply {
    ok(project_config_view(&app.project()))
}

#[derive(Deserialize, Default)]
pub struct TrustProjectBody {
    pub hash: Option<String>,
}

pub async fn trust_project_config(
    State(app): State<Arc<App>>,
    body: Option<Json<TrustProjectBody>>,
) -> Reply {
    let project = app.project();
    let Some(loaded) = crate::project_config::load(&project)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?
    else {
        return Err(fail(StatusCode::NOT_FOUND, "taskmaster.yaml does not exist in this project"));
    };
    if let Some(expected) = body.and_then(|Json(b)| b.hash)
        && expected != loaded.hash
    {
        return Err(fail(StatusCode::CONFLICT, "taskmaster.yaml changed since it was read; review and approve again"));
    }
    crate::project_config::trust(&project, &loaded.hash)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(project_config_view(&project))
}

#[derive(Deserialize, Default)]
pub struct GenerateProjectBody {
    #[serde(default)]
    pub overwrite: bool,
}

pub async fn generate_project_config(
    State(app): State<Arc<App>>,
    body: Option<Json<GenerateProjectBody>>,
) -> Reply {
    let project = app.project();
    let path = project.join(crate::project_config::CONFIG_FILE);
    let overwrite = body.map(|Json(b)| b.overwrite).unwrap_or(false);
    if path.is_file() && !overwrite {
        return Err(fail(StatusCode::CONFLICT, "taskmaster.yaml already exists"));
    }
    let content = crate::project_config::generate_from_project(&project);
    std::fs::write(&path, &content)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot write taskmaster.yaml: {err}")))?;
    if let Ok(Some(loaded)) = crate::project_config::load(&project)
        && loaded.has_commands
    {
        let _ = crate::project_config::trust(&project, &loaded.hash);
    }
    ok(project_config_view(&project))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SettingsBody {
    approvals: Option<String>,
    approval_timeout_secs: Option<u64>,
    /// O8: let the master answer a worker's low-risk question first.
    approvals_master_answers: Option<bool>,
    /// An editor id, a program name or path, or empty to go back to `$VISUAL`, `$EDITOR` and what is installed.
    editor: Option<String>,
    /// Role choices for new runs: any of `{worker, mid, master, reviewer, verifier}`, each a
    /// harness id. Roles left out keep what was saved; an empty string forgets a choice.
    roles: Option<serde_json::Map<String, Value>>,
    /// Model per role; `null` is the provider's default.
    models: Option<crate::Models>,
    /// The mid-tier checker on or off.
    checker: Option<bool>,
    /// Code Mode on or off.
    code_mode: Option<bool>,
    /// The workers' sandbox: `{shell?, package_indexes?}`.
    sandbox: Option<SandboxBody>,
    /// Anonymous counts. Off until set.
    telemetry: Option<bool>,
    /// Seconds one agent call may run; 0 is no limit.
    agent_timeout_secs: Option<u64>,
    /// Seconds an idle warm session is kept before detaching (O7); 0 disables idle release.
    warm_idle_secs: Option<u64>,
    /// Opt-in OAuth usage polling (O4). Off by default.
    usage_oauth_polling: Option<bool>,
    /// A8: minutes a merged run stays out before it settles itself.
    settle_after_merge_mins: Option<u32>,
    /// A8: days with no activity before a finished run settles itself; 0 is off.
    settle_idle_days: Option<u32>,
    /// A5 (Labs): suggest the next message after a turn.
    labs_suggest_next: Option<bool>,
}

#[derive(Deserialize)]
pub struct SandboxBody {
    shell: Option<bool>,
    package_indexes: Option<bool>,
}

/// Change the approval policy now and keep it in the settings file that is in use (or
/// the project's, when there is none), leaving every other part of that file alone.
pub async fn update_settings(State(app): State<Arc<App>>, Json(body): Json<SettingsBody>) -> Reply {
    let mut policy = app.live.approvals.policy();
    let mut changes = serde_json::Map::new();
    if let Some(mode) = &body.approvals {
        policy.mode = crate::approvals::Mode::parse(mode)
            .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "approvals is always, risky or never"))?;
        changes.insert("mode".into(), json!(policy.mode.id()));
    }
    if let Some(secs) = body.approval_timeout_secs {
        if !(1..=86_400).contains(&secs) {
            return Err(fail(StatusCode::BAD_REQUEST, "approval_timeout_secs is between 1 and 86400"));
        }
        policy.timeout = Duration::from_secs(secs);
        changes.insert("timeout_secs".into(), json!(secs));
    }
    if let Some(on) = body.approvals_master_answers {
        changes.insert("master_answers".into(), json!(on));
    }
    if let Some(editor) = &body.editor {
        let editor = editor.trim();
        if !editor.is_empty() && editor_from_value(editor, "settings").is_none() {
            return Err(fail(StatusCode::BAD_REQUEST, format!("`{editor}` is not an editor installed on this machine")));
        }
        save_setting(&app.project(), "editor", json!(editor)).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
        *app.editor.lock().unwrap() = editor.to_string();
    }
    if body.roles.is_some() || body.models.is_some() {
        let mut saved = crate::settings::Settings::load(&app.project()).unwrap_or_default();
        let mut roles = crate::settings::Settings::saved_roles(&app.project());
        if let Some(chosen) = body.roles {
            for (role, value) in chosen {
                if !crate::Roles::NAMES.contains(&role.as_str()) {
                    return Err(fail(StatusCode::BAD_REQUEST, format!("`{role}` is not a role: use one of {}", crate::Roles::NAMES.join(", "))));
                }
                let id = value.as_str().unwrap_or("").trim().to_string();
                if id.is_empty() {
                    roles.remove(&role);
                    continue;
                }
                if !tm_harness::HARNESSES.contains(&id.as_str()) {
                    return Err(fail(StatusCode::BAD_REQUEST, format!("unknown harness `{id}`: use one of {}", tm_harness::HARNESSES.join(", "))));
                }
                roles.insert(role, json!(id));
            }
        }
        if let Some(models) = body.models {
            saved.models = models;
        }
        // The roles object is written whole, so a forgotten choice is gone from the file.
        set_setting(&app.project(), "roles", Value::Object(roles))
            .and_then(|()| save_setting(&app.project(), "models", json!(saved.models)))
            .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(sandbox) = &body.sandbox {
        let mut changes = serde_json::Map::new();
        if let Some(on) = sandbox.shell {
            changes.insert("shell".into(), json!(on));
        }
        if let Some(on) = sandbox.package_indexes {
            changes.insert("package_indexes".into(), json!(on));
        }
        save_setting(&app.project(), "sandbox", Value::Object(changes)).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(on) = body.code_mode {
        save_setting(&app.project(), "code_mode", json!({ "enabled": on })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(on) = body.checker {
        save_setting(&app.project(), "checker", json!({ "enabled": on })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(on) = body.telemetry {
        save_setting(&app.project(), "telemetry", json!({ "enabled": on })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(secs) = body.agent_timeout_secs {
        if secs != 0 && !(60..=24 * 3600).contains(&secs) {
            return Err(fail(StatusCode::BAD_REQUEST, "the agent time limit is 0 (no limit), or 60 to 86400 seconds"));
        }
        save_setting(&app.project(), "agents", json!({ "timeout_secs": secs })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(secs) = body.warm_idle_secs {
        if secs > 24 * 3600 {
            return Err(fail(StatusCode::BAD_REQUEST, "warm_idle_secs is at most 86400"));
        }
        save_setting(&app.project(), "agents", json!({ "warm_idle_secs": secs })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(on) = body.labs_suggest_next {
        save_setting(&app.project(), "labs", json!({ "suggest_next": on })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(mins) = body.settle_after_merge_mins {
        if mins > 30 * 24 * 60 {
            return Err(fail(StatusCode::BAD_REQUEST, "settle_after_merge_mins is at most 43200 (30 days)"));
        }
        save_setting(&app.project(), "settle", json!({ "after_merge_mins": mins })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(days) = body.settle_idle_days {
        if days > 365 {
            return Err(fail(StatusCode::BAD_REQUEST, "settle_idle_days is at most 365"));
        }
        save_setting(&app.project(), "settle", json!({ "idle_days": days })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if let Some(on) = body.usage_oauth_polling {
        save_setting(&app.project(), "usage", json!({ "oauth_polling": on })).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    if !changes.is_empty() {
        save_setting(&app.project(), "approvals", Value::Object(changes)).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
        app.live.approvals.set_policy(policy);
    }
    settings(State(app)).await
}

/// The settings file in use: the project's if it has one, else the home directory's, else
/// the project's, to be created.
fn settings_file(project: &Path) -> PathBuf {
    let project_file = project.join(".taskmaster/config.json");
    [Some(project_file.clone()), home().map(|h| h.join(".taskmaster/config.json"))]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .unwrap_or(project_file)
}

/// Put `value` at `key` of the settings file, replacing what was there.
pub(super) fn set_setting(project: &Path, key: &str, value: Value) -> anyhow::Result<()> {
    write_setting(project, key, value, false)
}

/// Merge `values` into the object at `key` of the settings file.
pub(super) fn save_setting(project: &Path, key: &str, values: Value) -> anyhow::Result<()> {
    write_setting(project, key, values, true)
}

fn write_setting(project: &Path, key: &str, values: Value, merge: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    let file = settings_file(project);
    let mut root: Value = match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not a settings file", file.display()))?,
        Err(_) => json!({}),
    };
    let object = root.as_object_mut().with_context(|| format!("{} is not a settings file", file.display()))?;
    match (object.get_mut(key), values) {
        (Some(Value::Object(existing)), Value::Object(new)) if merge => existing.extend(new),
        (_, values) => {
            object.insert(key.into(), values);
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = file.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_string_pretty(&root)? + "\n")?;
    std::fs::rename(&temp, &file)?;
    Ok(())
}

pub async fn settings(State(app): State<Arc<App>>) -> Reply {
    let project_config = app.project().join(".taskmaster/config.json");
    let home_config = home().map(|h| h.join(".taskmaster/config.json"));
    let config = [Some(project_config.clone()), home_config]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .map(|path| path.display().to_string());
    let role_defaults = crate::settings::Settings::load(&app.project()).unwrap_or_default();
    ok(json!({
        "project": app.project().display().to_string(),
        "workspace": app.workspace.display().to_string(),
        "branch": crate::git::current_branch(&app.project()),
        "dictionary": app.dictionary().display().to_string(),
        "commons": app.commons().display().to_string(),
        "commons_remote": app.remote.is_some(),
        "config": config,
        "config_default": project_config.display().to_string(),
        "store": if app.store.is_some() { "sqlite" } else { "files" },
        // Only what the person chose. A role missing here has no provider yet.
        "roles": crate::settings::Settings::saved_roles(&app.project()),
        "models": role_defaults.models,
        "checker": { "enabled": role_defaults.checker.enabled },
        "code_mode": { "enabled": role_defaults.code_mode.enabled },
        "sandbox": {
            "shell": role_defaults.sandbox.shell,
            "package_indexes": role_defaults.sandbox.package_indexes,
            // Whether this machine can give a worker a sandboxed shell at all.
            "available": tm_harness::sandbox::bwrap_usable(),
        },
        "telemetry": { "enabled": role_defaults.telemetry.enabled },
        "agents": {
            "timeout_secs": role_defaults.agents.timeout_secs,
            "warm_idle_secs": role_defaults.agents.warm_idle_secs,
        },
        "usage": {
            "oauth_polling": role_defaults.usage.oauth_polling,
        },
        "labs": { "suggest_next": role_defaults.labs.suggest_next },
        "settle": {
            "after_merge_mins": role_defaults.settle.after_merge_mins,
            "idle_days": role_defaults.settle.idle_days,
        },
        "web_access": {
            "enabled": role_defaults.web_access.enabled,
            "provider": role_defaults.web_access.provider_norm(),
            "endpoint": role_defaults.web_access.endpoint,
            "has_api_key": crate::web_access::load_api_key(&crate::web_access::default_vault(), &role_defaults.web_access.provider_norm()).is_some(),
            "roles": role_defaults.web_access.roles,
            "allowlist": role_defaults.web_access.allowlist,
            "planner_max_searches": role_defaults.web_access.planner_max_searches,
            "planner_max_fetches": role_defaults.web_access.planner_max_fetches,
            "max_fetch_bytes": role_defaults.web_access.max_fetch_bytes,
            "cache_ttl_secs": role_defaults.web_access.cache_ttl_secs,
            "cache_max_mb": role_defaults.web_access.cache_max_mb,
        },
        "editor": *app.editor.lock().unwrap(),
        "approvals": {
            "mode": app.live.approvals.policy().mode.id(),
            "timeout_secs": app.live.approvals.policy().timeout.as_secs(),
            "master_answers": role_defaults.approvals.master_answers,
        },
        "machine": machine(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

const SETTING_KEYS: &[&str] = &[
    "store", "auth", "commons", "approvals", "preview", "search", "editor", "roles", "models", "checker", "code_mode", "telemetry", "agents", "sandbox", "web_access",
];

/// Tokens are not part of an export. Host connections live in another file and stay there.
fn redact_export(value: &mut Value) {
    if let Some(commons) = value.get_mut("commons").and_then(Value::as_object_mut)
        && commons.get("token").and_then(Value::as_str).is_some_and(|token| !token.is_empty())
    {
        commons.insert("token".into(), json!(""));
    }
    if let Some(web) = value.get_mut("web_access").and_then(Value::as_object_mut) {
        web.remove("api_key");
    }
}

pub async fn export_settings(State(app): State<Arc<App>>) -> Reply {
    let file = settings_file(&app.project());
    let mut value = std::fs::read_to_string(&file).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_else(|| json!({}));
    if !value.is_object() {
        value = json!({});
    }
    redact_export(&mut value);
    ok(json!({
        "settings": value,
        "note": "Tokens are left out. Host connections are kept in their own file and are not here.",
    }))
}

pub async fn import_settings(State(app): State<Arc<App>>, Json(body): Json<Value>) -> Reply {
    let incoming = body.get("settings").cloned().unwrap_or(body);
    let Some(object) = incoming.as_object() else {
        return Err(fail(StatusCode::BAD_REQUEST, "settings import is a JSON object"));
    };
    for key in object.keys() {
        if !SETTING_KEYS.contains(&key.as_str()) {
            return Err(fail(StatusCode::BAD_REQUEST, format!("`{key}` is not a settings field")));
        }
    }
    if let Some(mode) = object.get("approvals").and_then(|v| v.get("mode")).and_then(|v| v.as_str())
        && crate::approvals::Mode::parse(mode).is_none()
    {
        return Err(fail(StatusCode::BAD_REQUEST, "approvals.mode is always, risky or never"));
    }
    if let Some(roles) = object.get("roles").and_then(Value::as_object) {
        for (role, value) in roles {
            if !crate::Roles::NAMES.contains(&role.as_str()) {
                return Err(fail(StatusCode::BAD_REQUEST, format!("`{role}` is not a role")));
            }
            let id = value.as_str().unwrap_or("").trim();
            if !id.is_empty() && !tm_harness::HARNESSES.contains(&id) {
                return Err(fail(StatusCode::BAD_REQUEST, format!("unknown harness `{id}`")));
            }
        }
    }
    for (key, value) in object {
        set_setting(&app.project(), key, value.clone()).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    apply_live_settings(&app)?;
    settings(State(app)).await
}

pub async fn reset_settings(State(app): State<Arc<App>>) -> Reply {
    let project = app.project();
    let defaults = [
        ("roles", json!({})),
        ("models", json!({})),
        ("editor", json!("")),
        ("approvals", json!({ "mode": "never", "timeout_secs": 600 })),
        ("checker", json!({ "enabled": false })),
        ("code_mode", json!({ "enabled": true })),
        ("telemetry", json!({ "enabled": false })),
        ("agents", json!({ "timeout_secs": crate::settings::DEFAULT_AGENT_SECS })),
        ("sandbox", json!({ "shell": true, "package_indexes": true })),
        ("web_access", serde_json::to_value(crate::web_access::WebAccessSettings::default()).unwrap_or_else(|_| json!({}))),
    ];
    for (key, value) in defaults {
        set_setting(&project, key, value).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    apply_live_settings(&app)?;
    crate::log::note("settings reset");
    settings(State(app)).await
}

fn apply_live_settings(app: &App) -> Result<(), ApiError> {
    let loaded = crate::settings::Settings::load(&app.project()).unwrap_or_default();
    let policy = loaded.approval_policy().map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    app.live.approvals.set_policy(policy);
    *app.editor.lock().unwrap() = loaded.editor;
    Ok(())
}

pub async fn logs(State(_app): State<Arc<App>>) -> Reply {
    let (path, lines) = crate::log::tail(200);
    ok(json!({ "path": path, "lines": lines }))
}

/// What this machine can run at once. Every worker is a local agent CLI with its own
/// process tree, so the ceiling is the smaller of the cores and the free memory, at about
/// one gigabyte a worker.
pub fn machine() -> Value {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let free_gb = free_memory_bytes().map(|bytes| bytes / (1 << 30));
    let by_memory = free_gb.map(|gb| gb as usize).unwrap_or(cpus);
    let max_parallel = cpus.min(by_memory).clamp(1, 32);
    json!({ "cpus": cpus, "free_memory_gb": free_gb, "max_parallel": max_parallel })
}

fn free_memory_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|line| line.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_export_drops_a_commons_token() {
        let mut value = json!({
            "commons": { "token": "secret-token", "remote": "https://commons.example" },
            "roles": { "master": "grok" }
        });
        redact_export(&mut value);
        assert_eq!(value["commons"]["token"], "");
        assert_eq!(value["commons"]["remote"], "https://commons.example");
        assert_eq!(value["roles"]["master"], "grok");
    }

    #[test]
    fn editors_open_at_the_line_and_column() {
        let file = Path::new("/p/src/main.rs");
        assert_eq!(editor_args(Style::Goto, file, Some(42), Some(5), false), vec!["-g", "/p/src/main.rs:42:5"]);
        assert_eq!(editor_args(Style::Goto, file, None, None, false), vec!["-g", "/p/src/main.rs"]);
        assert_eq!(editor_args(Style::Suffix, file, Some(42), None, false), vec!["/p/src/main.rs:42"]);
        assert_eq!(
            editor_args(Style::JetBrains, file, Some(42), Some(5), false),
            vec!["--line", "42", "--column", "5", "/p/src/main.rs"]
        );
        if !cfg!(target_os = "macos") {
            assert_eq!(editor_args(Style::Goto, file, Some(1), None, true), vec!["/p/src"]);
        }
        assert_eq!(EDITORS.len(), 21);
    }

    #[test]
    fn only_real_files_under_the_project_resolve() {
        let dir = std::env::temp_dir().join(format!("tm-desk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let project = dir.canonicalize().unwrap();
        assert_eq!(resolve(&project, "src/main.rs"), Some(project.join("src/main.rs")));
        assert_eq!(resolve(&project, "./src/main.rs"), Some(project.join("src/main.rs")));
        assert_eq!(resolve(&project, "src/missing.rs"), None);
        assert_eq!(resolve(&project, "src"), None);
        assert_eq!(resolve(&project, "/etc/hostname"), None);
        assert!(valid_recipient("page:a") && valid_recipient("workers") && !valid_recipient("page:../x"));
    }

    #[test]
    fn a_pull_request_is_written_from_the_plan() {
        let plan = tm_core::Plan {
            contracts: vec![tm_core::ContractFile { path: "app/types.ts".into(), content: "x".into() }],
            pages: vec![tm_core::PagePlan {
                id: "home".into(),
                route: "/".into(),
                owned_glob: "app/home/**".into(),
                cites: vec!["theme".into()],
                brief: "b".into(),
                ..Default::default()
            }],
            estimate: tm_core::Estimate { tokens: 1000, usd: 0.5, ..Default::default() },
            ..Default::default()
        };
        let (title, body) = pr_text("r1", Some("\nA bookshop with a cart\nmore"), &plan);
        assert_eq!(title, "A bookshop with a cart");
        assert!(body.contains("| `home` | `/` | `app/home/**` | theme |") && body.contains("`app/types.ts`"), "{body}");
    }
}
