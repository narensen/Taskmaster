//! The parts of the API the web control surface uses beside runs: the agent transcript,
//! notes, the run's terminal, diffs, opening a file in an editor on this machine, a file
//! preview, completions, and the GitHub CLI.
//!
//! ```text
//! GET  /api/runs/{id}/agents      ?after=<n> -> {next, entries}: text, thoughts, tool calls, commands
//! GET  /api/runs/{id}/notes       notes queued for the run's agents
//! POST /api/runs/{id}/notes       {to, text}: to is master, workers, page:<id>, reviewer or verifier
//! POST /api/runs/{id}/notes/{note}  {text} changes a note not yet delivered; {} removes it
//! POST /api/runs/{id}/terminal    {command}: run it in the project as the daemon's user
//! GET  /api/runs/{id}/diff        ?scope=run|page&page=<id>
//! POST /api/runs/{id}/pr          push the run's branch and open a draft pull request
//! GET  /api/settings              where this daemon keeps things
//! GET  /api/editors               the editors this machine can launch, and the default
//! POST /api/open                  {path, line?, column?, editor?, reveal?}
//! POST /api/paths                 {paths: [..]} -> {paths: {raw: full path or null}}
//! GET  /api/file                  ?path=<p> a file's text, for a read-only preview
//! GET  /api/files                 files git knows in the project
//! GET  /api/skills                SKILL.md names under .agents/skills and .claude/skills
//! GET  /api/gh                    whether gh is installed and signed in, and as whom
//! GET  /api/gh/pr/{number}        one pull request of the project's repository
//! ```
//!
//! A path is relative to the project, or starts with `~/` for this machine's home, or is
//! absolute. Only files under the project or the home directory are opened, previewed or
//! reported as real.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tm_core::{RunPhase, valid_key};

use super::{App, Reply, accepted, fail, ok};

const PERSON: &str = "person";
/// How long a command typed into the terminal may run.
const TERMINAL_TIMEOUT: Duration = Duration::from_secs(300);
const KEEP: usize = 64 * 1024;
const PREVIEW_LIMIT: u64 = 512 * 1024;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The file `raw` names, if it is a real file this daemon may show. A line suffix is
/// the caller's to strip.
pub fn resolve(project: &Path, raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let path = if let Some(rest) = raw.strip_prefix("~/") {
        home()?.join(rest)
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        project.join(raw.strip_prefix("./").unwrap_or(raw))
    };
    let full = path.canonicalize().ok()?;
    if !full.is_file() {
        return None;
    }
    let allowed = full.starts_with(project) || home().and_then(|h| h.canonicalize().ok()).is_some_and(|h| full.starts_with(h));
    allowed.then_some(full)
}

// Agents and notes.

pub async fn agents(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    app.known(&id)?;
    let after = query.get("after").and_then(|n| n.parse().ok()).unwrap_or(0);
    let (next, entries) = app.live.read(&id, after);
    ok(json!({ "next": next, "entries": entries }))
}

pub async fn notes(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    ok(json!({ "notes": app.live.notes(&id) }))
}

#[derive(Deserialize)]
pub struct NoteBody {
    to: String,
    text: String,
}

pub fn valid_recipient(to: &str) -> bool {
    matches!(to, "master" | "workers" | "reviewer" | "verifier") || to.strip_prefix("page:").is_some_and(valid_key)
}

pub async fn add_note(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<NoteBody>) -> Reply {
    app.known(&id)?;
    if !valid_recipient(&body.to) {
        return Err(fail(StatusCode::BAD_REQUEST, "a note goes to master, workers, page:<id>, reviewer or verifier"));
    }
    if body.text.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "a note needs text"));
    }
    ok(json!(app.live.add_note(&id, &body.to, body.text.trim())?))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct EditNote {
    text: Option<String>,
}

pub async fn edit_note(
    State(app): State<Arc<App>>,
    UrlPath((id, note)): UrlPath<(String, String)>,
    body: Option<Json<EditNote>>,
) -> Reply {
    app.known(&id)?;
    let text = body.and_then(|Json(body)| body.text).filter(|text| !text.trim().is_empty());
    if app.live.edit_note(&id, &note, text.as_deref())? {
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
    let cwd = app.project.display().to_string();
    app.live.push(&id, &agent, "command", json!({ "command": command, "cwd": cwd, "typed": true }));
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&app.project)
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
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn diff(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    app.known(&id)?;
    let meta = app.records.meta(&id)?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "this run has not started building"))?;
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
            let mut files: Vec<String> = app
                .records
                .changes(&id)?
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
    let text = git(&app.project, &refs).map_err(|err| fail(StatusCode::CONFLICT, err))?;
    ok(json!({ "scope": scope, "base": base, "branch": branch, "diff": text }))
}

/// What to compare against: the run branch while it exists, else the commit it ended on.
fn run_head(app: &App, id: &str, branch: &str) -> Option<String> {
    if git(&app.project, &["rev-parse", "--verify", "--quiet", &format!("{branch}^{{commit}}")]).is_ok() {
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
        return Err(text.trim().to_string());
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
    let project = app.project.clone();
    let status = tokio::task::spawn_blocking(move || gh_account(&project)).await.unwrap_or(Value::Null);
    ok(status)
}

pub async fn gh_pr(State(app): State<Arc<App>>, UrlPath(number): UrlPath<u64>) -> Reply {
    let project = app.project.clone();
    let found = tokio::task::spawn_blocking(move || {
        gh(&project, &["pr", "view", &number.to_string(), "--json", "number,title,url,state"])
    })
    .await
    .unwrap_or_else(|_| Err("gh stopped".into()));
    match found {
        Ok(text) => ok(serde_json::from_str(&text).unwrap_or(Value::Null)),
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
    let meta = app.records.meta(&id)?;
    let branch = meta.and_then(|meta| meta.branch).ok_or_else(|| fail(StatusCode::CONFLICT, "this run has no branch"))?;
    let prompt = app.request_for(&id)?.prompt;
    let (title, body) = pr_text(&id, prompt.as_deref(), &plan);
    let head = run_head(&app, &id, &branch).ok_or_else(|| fail(StatusCode::CONFLICT, format!("branch `{branch}` is gone and its last commit was not kept")))?;
    let project = app.project.clone();
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
        Ok(url) => ok(json!({ "url": url })),
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

pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(program)).find(|path| path.is_file())
}

/// The first installed of Cursor, VS Code and Zed, then the file manager.
pub fn default_editor() -> &'static str {
    ["cursor", "vscode", "zed"]
        .into_iter()
        .find(|id| EDITORS.iter().any(|e| e.id == *id && which(e.command).is_some()))
        .unwrap_or("files")
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
        Style::Files => unreachable!(),
    }
}

pub async fn editors() -> Reply {
    let list: Vec<Value> = EDITORS
        .iter()
        .map(|e| json!({ "id": e.id, "name": e.name, "command": e.command, "installed": which(e.command).is_some() }))
        .collect();
    ok(json!({ "editors": list, "default": default_editor() }))
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

/// Open a file in an editor on this machine, the one that holds the project.
pub async fn open(State(app): State<Arc<App>>, Json(body): Json<OpenBody>) -> Reply {
    let Some(file) = resolve(&app.project, &body.path) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("{} is not a file here", body.path)));
    };
    let wanted = if body.reveal { "files".to_string() } else { body.editor.clone().unwrap_or_else(|| default_editor().into()) };
    let Some(editor) = EDITORS.iter().find(|e| e.id == wanted) else {
        return Err(fail(StatusCode::BAD_REQUEST, format!("unknown editor `{wanted}`")));
    };
    let full = file.display().to_string();
    let Some(program) = which(editor.command) else {
        let reply = json!({ "error": format!("`{}` is not installed on this machine", editor.command), "command": editor.command, "path": full });
        return Ok(axum::response::IntoResponse::into_response((StatusCode::FAILED_DEPENDENCY, Json(reply))));
    };
    let args = editor_args(editor.style, &file, body.line, body.column, body.reveal);
    let spawned = std::process::Command::new(&program)
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
            accepted(json!({ "command": editor.command, "args": args, "path": full }))
        }
        Err(err) => {
            let reply = json!({ "error": format!("`{}` did not start: {err}", editor.command), "command": editor.command, "path": full });
            Ok(axum::response::IntoResponse::into_response((StatusCode::FAILED_DEPENDENCY, Json(reply))))
        }
    }
}

#[derive(Deserialize)]
pub struct PathsBody {
    paths: Vec<String>,
}

pub async fn paths(State(app): State<Arc<App>>, Json(body): Json<PathsBody>) -> Reply {
    let mut out = serde_json::Map::new();
    for raw in body.paths.into_iter().take(500) {
        let full = resolve(&app.project, &raw).map(|path| path.display().to_string());
        out.insert(raw, json!(full));
    }
    ok(json!({ "paths": out, "project": app.project.display().to_string() }))
}

pub async fn file(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let raw = query.get("path").cloned().unwrap_or_default();
    let Some(file) = resolve(&app.project, &raw) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("{raw} is not a file here")));
    };
    let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    let bytes = std::fs::read(&file).map_err(|err| fail(StatusCode::NOT_FOUND, err.to_string()))?;
    let cut = &bytes[..bytes.len().min(PREVIEW_LIMIT as usize)];
    if cut.contains(&0) {
        return Err(fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "a binary file has no text preview"));
    }
    ok(json!({
        "path": raw,
        "full": file.display().to_string(),
        "content": String::from_utf8_lossy(cut),
        "truncated": size > PREVIEW_LIMIT,
    }))
}

pub async fn files(State(app): State<Arc<App>>) -> Reply {
    let text = git(&app.project, &["ls-files", "--cached", "--others", "--exclude-standard"]).unwrap_or_default();
    let list: Vec<&str> = text.lines().take(20_000).collect();
    ok(json!({ "files": list }))
}

pub async fn skills(State(app): State<Arc<App>>) -> Reply {
    let mut found = Vec::new();
    for root in [".agents/skills", ".claude/skills"] {
        let Ok(dirs) = std::fs::read_dir(app.project.join(root)) else { continue };
        for dir in dirs.flatten() {
            let file = dir.path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&file) else { continue };
            let named = text
                .lines()
                .take(20)
                .find_map(|line| line.strip_prefix("name:").map(|n| n.trim().trim_matches('"').to_string()))
                .filter(|n| !n.is_empty());
            let name = named.unwrap_or_else(|| dir.file_name().to_string_lossy().into_owned());
            let path = format!("{root}/{}/SKILL.md", dir.file_name().to_string_lossy());
            found.push(json!({ "name": name, "path": path }));
        }
    }
    found.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    ok(json!({ "skills": found }))
}

pub async fn settings(State(app): State<Arc<App>>) -> Reply {
    let project_config = app.project.join(".taskmaster/config.json");
    let home_config = home().map(|h| h.join(".taskmaster/config.json"));
    let config = [Some(project_config.clone()), home_config]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .map(|path| path.display().to_string());
    ok(json!({
        "project": app.project.display().to_string(),
        "dictionary": app.dictionary.display().to_string(),
        "commons": app.commons.display().to_string(),
        "commons_remote": app.remote.is_some(),
        "config": config,
        "config_default": project_config.display().to_string(),
        "store": if app.store.is_some() { "sqlite" } else { "files" },
        "roles": crate::Roles::default(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            }],
            estimate: tm_core::Estimate { tokens: 1000, usd: 0.5 },
        };
        let (title, body) = pr_text("r1", Some("\nA bookshop with a cart\nmore"), &plan);
        assert_eq!(title, "A bookshop with a cart");
        assert!(body.contains("| `home` | `/` | `app/home/**` | theme |") && body.contains("`app/types.ts`"), "{body}");
    }
}
