//! The HTTP API a frontend talks to.
//!
//! ```text
//! GET  /                          the phone page
//! GET  /api/health                no token needed
//! POST /api/pair                  {code} -> {token}
//!
//! GET  /api/runs                  every run, with its phase, pages and spend
//! POST /api/runs                  {prompt | plan, id?, yes?, roles?, models?, parallel?, timeout_secs?, template?}
//!                                 a role left out takes the saved choice; with neither, the run is refused
//! GET  /api/runs/{id}             one run, with its plan and how it was asked for
//! GET  /api/runs/{id}/events      ?after=<n> -> {next, events}
//! GET  /api/runs/{id}/stream      the same events as they happen (server-sent events)
//! GET  /api/runs/{id}/ledger      what each agent call cost
//! GET  /api/runs/{id}/changes     what was committed, and by which model
//! GET  /api/runs/{id}/attempts/{page}  every attempt at a page: who, what, and why it was not kept
//! GET  /api/runs/{id}/screens     pages with a screenshot from the browser check
//! GET  /api/runs/{id}/screens/{page}   the screenshot, as PNG
//! POST /api/runs/{id}/approve     build the plan that is waiting
//! POST /api/runs/{id}/reject      {prompt?} drop the plan, and plan again when a prompt is given
//! POST /api/runs/{id}/resume      carry on a run that stopped part way
//! POST /api/runs/{id}/cancel
//!
//! GET  /api/providers             ?harness=<h> installed harnesses and their models
//! GET  /api/templates             the built-in templates; a directory path works as well
//! GET  /api/dictionary            ?q=<text> Commons first, then the project
//! GET  /api/dictionary/{key}
//! GET  /api/errorography          recurring mistakes, per model
//! GET  /api/reviews               reviews started here
//! POST /api/reviews               {target?, base?, head?, pr?, dirty?, reviewer?, verifier?, ...}
//! GET  /api/reviews/{id}
//! POST /api/reviews/{id}/cancel
//! POST /api/runs/{id}/review     one combined review of a finished run's diff; returns {id}
//! GET  /api/commons/entries       this daemon's Commons, for another daemon to pull
//! POST /api/commons/entries       an entry another daemon promoted
//! GET  /api/projects              the workspace's git repositories, the open one first
//! POST /api/projects              {name, prompt?, ...} a new project, and its first run
//! POST /api/projects/open         {path} open another repository of the workspace
//!
//! GET  /api/hosts                 GitHub, GitLab, Gitea and Bitbucket, and who is signed in
//! POST /api/hosts                 {kind, token, base?, user?} check a token, then keep it
//! DELETE /api/hosts/{id}          forget a saved token (a CLI sign-in stays)
//! POST /api/hosts/{id}/repos      {name, private?, push?} create a repository and push
//! GET  /api/git                   branch, dirty state, remotes
//! POST /api/git/remote            {name, url}
//! POST /api/git/push              {remote?, branch?}
//! POST /api/git/clone             {url, name?} into the workspace, without switching to it
//! ```
//!
//! Installing, updating and signing in to harness CLIs is in `api/setup.rs`, and the
//! skills store in `api/skills.rs`. Terminals a person opens (a shell on a PTY in the
//! project) are in `api/terms.rs`.
//!
//! The control surface's own routes (the agent transcript, notes, the terminal, diffs,
//! opening files in an editor, completions, gh) are listed in `api/desk.rs`.
//!
//! `health` also says how to sign in: with a pairing code, an account, or either. When a
//! built frontend is given (`web`), every path that is not under `/api` serves it.
//!
//! Everything under `/api` except `health` and `pair` needs `Authorization: Bearer <token>`.
//! A browser's `EventSource` cannot send that header, so `stream` also takes `?token=`.
//!
//! Each run builds on its own branch, so several can run at once. The exception is a
//! repository with no commit yet: its first run builds in place and runs alone.
//!
//! `stream` sends an `event` for each entry in the run's log, numbered from 1, and then an
//! `idle` when nothing is driving the run, with its phase. The stream ends there: nothing
//! more can happen until someone approves, resumes or cancels. `?after=<n>` or a
//! `Last-Event-ID` header skips what the client already has.
//!
//! `stream` also sends an `agent` for each entry of the agent transcript (an agent's text,
//! thoughts and tool calls with the gate's answer, tagged with the agent), without an id so
//! the log's numbering is unchanged. `?agents_after=<n>` skips entries the client has.
//! `approve` takes `{note?}`: a note to every worker that builds after it.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use futures_util::stream::{self, Stream};
use tm_core::{DictTier, DictionaryEntry, EntryStats, Event, Plan, Run, RunPhase, totals, valid_key};
use tm_dictionary::{Catalog, CommonsRemote, LocalStore};
use tm_harness::{HARNESSES, Harness, provider_report, provider_reports};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

mod desk;
mod hosts;
mod setup;
mod skills;
mod terms;

pub(crate) use desk::redact;

#[cfg(test)]
pub(crate) use desk::{Style, choose_editor, sensitive};

use crate::auth::Auth;
use crate::live::Live;
use crate::secret::random_hex;
use crate::store::{Records, Store};
use crate::time::rfc3339_now;
use crate::{Config, Harnesses, Models, Outcome, Roles, RunRequest, check_models, drive};

const PAGE: &str = include_str!("phone.html");

/// Turns a harness name into its adapter. Tests pass a fake here.
pub type HarnessFactory = Arc<dyn Fn(&str) -> anyhow::Result<Arc<dyn Harness>> + Send + Sync>;

pub struct ServerOptions {
    pub project: PathBuf,
    pub bind: SocketAddr,
    pub auth: Arc<dyn Auth>,
    /// `None` is `.taskmaster/` in the project.
    pub store: Option<Arc<dyn Store>>,
    pub remote: Option<Arc<dyn CommonsRemote>>,
    pub dictionary: Option<PathBuf>,
    pub commons: Option<PathBuf>,
    pub harness: HarnessFactory,
    /// A built frontend: a directory with an `index.html`. `None` serves the small
    /// built-in page.
    pub web: Option<PathBuf>,
    /// When agents' risky tool calls wait for a person. The default asks nothing.
    pub approvals: crate::approvals::Policy,
    /// The settings file's `editor`: an editor id or a program name. Empty finds one.
    pub editor: String,
    /// The settings file's `preview.command`. Empty reads the project's `package.json`.
    pub preview_command: String,
    /// Chooses the commands of install, update and sign-in jobs. Tests pass their own.
    pub setup_planner: Option<crate::jobs::Planner>,
    /// The skills store shared by every harness.
    pub skills: PathBuf,
    /// Where skills are searched for and fetched from. Tests pass canned answers.
    pub skills_fetch: Arc<dyn crate::skills::Fetch>,
}

impl ServerOptions {
    pub fn new(project: impl Into<PathBuf>, bind: SocketAddr, auth: Arc<dyn Auth>) -> Self {
        ServerOptions {
            project: project.into(),
            bind,
            auth,
            store: None,
            remote: None,
            dictionary: None,
            commons: None,
            harness: Arc::new(tm_harness::harness),
            web: None,
            approvals: crate::approvals::Policy::default(),
            editor: String::new(),
            preview_command: String::new(),
            setup_planner: None,
            skills: crate::skills::default_root(),
            skills_fetch: Arc::new(crate::skills::Web),
        }
    }
}

/// A running API. Dropping it stops the listener; a run in progress is cancelled.
pub struct Server {
    pub url: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    app: Arc<App>,
}

impl Drop for Server {
    fn drop(&mut self) {
        crate::log::note("shutting down");
        for active in self.app.active.lock().unwrap().values() {
            let _ = active.cancel.send(true);
        }
        self.app.live.approvals.refuse_all();
        self.app.previews.stop_all();
        self.app.jobs.stop_all();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Active {
    cancel: watch::Sender<bool>,
    /// The run commits into the project's own working tree, so it must run alone.
    in_place: bool,
}

struct App {
    /// Parent of the first project this daemon opened; new projects are created here.
    workspace: PathBuf,
    project_slot: Mutex<PathBuf>,
    records_slot: Mutex<Records>,
    auth: Arc<dyn Auth>,
    store: Option<Arc<dyn Store>>,
    remote: Option<Arc<dyn CommonsRemote>>,
    dictionary_slot: Mutex<PathBuf>,
    commons_slot: Mutex<PathBuf>,
    harness: HarnessFactory,
    web: Option<PathBuf>,
    active: Mutex<HashMap<String, Active>>,
    live: Arc<Live>,
    /// The editor override; the settings file's value until `POST /api/settings` changes it.
    editor: Mutex<String>,
    previews: crate::preview::Previews,
    /// Running review jobs, so cancelling one (or its run) can stop the agents.
    reviews: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    jobs: Arc<crate::jobs::Jobs>,
    terms: terms::Terminals,
    skills_root: PathBuf,
    skills_fetch: Arc<dyn crate::skills::Fetch>,
}

impl Server {
    pub async fn serve(options: ServerOptions) -> anyhow::Result<Self> {
        let project = options
            .project
            .canonicalize()
            .with_context(|| format!("cannot open {}", options.project.display()))?;
        let records = match &options.store {
            Some(store) => Records::new(store.clone()),
            None => Records::files(&project),
        };
        let listener =
            TcpListener::bind(options.bind).await.with_context(|| format!("cannot bind {}", options.bind))?;
        let addr = listener.local_addr().context("the API listener has no address")?;
        let live_store: Arc<dyn Store> = match &options.store {
            Some(store) => store.clone(),
            None => Arc::new(crate::store::FileStore::in_project(&project)),
        };
        let live = Arc::new(Live::new(live_store));
        live.approvals.set_policy(options.approvals);
        let project_for_previews = project.clone();
        let workspace = project.parent().unwrap_or(&project).to_path_buf();
        let dictionary = options.dictionary.unwrap_or_else(|| project.join(".taskmaster/dictionary"));
        let commons = options.commons.unwrap_or_else(|| project.join(".taskmaster/commons"));
        let app = Arc::new(App {
            workspace,
            dictionary_slot: Mutex::new(dictionary),
            commons_slot: Mutex::new(commons),
            project_slot: Mutex::new(project),
            records_slot: Mutex::new(records),
            auth: options.auth,
            store: options.store,
            remote: options.remote,
            harness: options.harness,
            web: options.web.filter(|dir| dir.join("index.html").is_file()),
            active: Mutex::new(HashMap::new()),
            live,
            editor: Mutex::new(options.editor),
            previews: crate::preview::Previews::new(project_for_previews, options.preview_command),
            reviews: Mutex::new(HashMap::new()),
            jobs: Arc::new(crate::jobs::Jobs::new(options.setup_planner.unwrap_or_else(crate::jobs::default_planner))),
            terms: terms::Terminals::default(),
            skills_root: options.skills,
            skills_fetch: options.skills_fetch,
        });
        app.close_interrupted_reviews();
        let (tx, rx) = oneshot::channel();
        let router = router(app.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        let url = format!("http://{addr}/");
        crate::log::note(&format!("serving {url}"));
        Ok(Server { url, shutdown: Some(tx), task: Some(task), app })
    }
}

fn router(app: Arc<App>) -> Router {
    let private = Router::new()
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(show_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/ledger", get(run_ledger))
        .route("/api/runs/{id}/changes", get(run_changes))
        .route("/api/runs/{id}/attempts/{page}", get(run_attempts))
        .route("/api/runs/{id}/screens", get(run_screens))
        .route("/api/runs/{id}/screens/{page}", get(run_screen))
        .route("/api/runs/{id}/approve", post(approve_run))
        .route("/api/runs/{id}/reject", post(reject_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/search", get(web_search))
        .route("/api/providers", get(providers))
        .route("/api/templates", get(templates))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/open", post(open_project))
        .route("/api/dictionary", get(search_dictionary))
        .route("/api/dictionary/{key}", get(show_entry))
        .route("/api/errorography", get(errorography))
        .route("/api/reviews", get(list_reviews).post(create_review))
        .route("/api/reviews/{id}", get(show_review))
        .route("/api/reviews/{id}/cancel", post(cancel_review))
        .route("/api/runs/{id}/review", post(review_run))
        .route("/api/commons/entries", get(commons_entries).post(commons_add))
        .route("/api/runs/{id}/agents", get(desk::agents))
        .route("/api/runs/{id}/notes", get(desk::notes).post(desk::add_note))
        .route("/api/runs/{id}/notes/{note}", post(desk::edit_note))
        .route("/api/runs/{id}/terminal", post(desk::terminal))
        .route("/api/runs/{id}/diff", get(desk::diff))
        .route("/api/runs/{id}/pr", post(desk::open_pr))
        .route("/api/runs/{id}/preview", get(desk::preview).post(desk::start_preview))
        .route("/api/runs/{id}/preview/stop", post(desk::stop_preview))
        .route("/api/approvals", get(desk::approvals))
        .route("/api/approvals/{id}/{decision}", post(desk::decide))
        .route("/api/settings", get(desk::settings).post(desk::update_settings))
        .route("/api/settings/export", get(desk::export_settings))
        .route("/api/settings/import", post(desk::import_settings))
        .route("/api/settings/reset", post(desk::reset_settings))
        .route("/api/logs", get(desk::logs))
        .route("/api/editors", get(desk::editors))
        .route("/api/open", post(desk::open))
        .route("/api/paths", post(desk::paths))
        .route("/api/file", get(desk::file))
        .route("/api/files", get(desk::files))
        .route("/api/grep", get(desk::grep))
        .route("/api/terminals", get(terms::list).post(terms::open))
        .route("/api/terminals/{id}/input", post(terms::input))
        .route("/api/terminals/{id}/resize", post(terms::resize))
        .route("/api/terminals/{id}/close", post(terms::close))
        .route("/api/skills", get(skills::list))
        .route("/api/skills/search", get(skills::search))
        .route("/api/skills/updates", get(skills::updates))
        .route("/api/skills/install", post(skills::install))
        .route("/api/skills/{name}/enable", post(skills::enable))
        .route("/api/skills/{name}/update", post(skills::update))
        .route("/api/skills/{name}/remove", post(skills::remove))
        .route("/api/skills/{name}/import", post(skills::import))
        .route("/api/gh", get(desk::gh_status))
        .route("/api/gh/pr/{number}", get(desk::gh_pr))
        .route("/api/hosts", get(hosts::list).post(hosts::connect))
        .route("/api/hosts/{id}", axum::routing::delete(hosts::forget))
        .route("/api/hosts/{id}/repos", post(hosts::create_repo))
        .route("/api/git", get(hosts::git_state))
        .route("/api/git/remote", post(hosts::set_remote))
        .route("/api/git/push", post(hosts::push))
        .route("/api/git/clone", post(hosts::clone))
        .route("/api/setup", get(setup::overview))
        .route("/api/setup/providers/{id}/{action}", post(setup::provider_action))
        .route("/api/setup/runtimes/{id}/install", post(setup::runtime_install))
        .route("/api/setup/jobs/{id}", get(setup::job))
        .route("/api/setup/jobs/{id}/input", post(setup::input))
        .route("/api/setup/jobs/{id}/cancel", post(setup::cancel))
        .route_layer(middleware::from_fn_with_state(app.clone(), guard));
    Router::new()
        .route("/api/runs/{id}/stream", get(run_stream))
        .route("/api/setup/jobs/{id}/stream", get(setup::stream_job))
        .route("/api/terminals/{id}/stream", get(terms::stream))
        .route("/api/health", get(health))
        .route("/api/pair", post(pair))
        .merge(private)
        .fallback(frontend)
        .layer(middleware::from_fn(json_errors))
        .layer(middleware::from_fn(cors))
        .with_state(app)
}

// Access.

async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    match bearer(request.headers()).and_then(|token| app.auth.identify(token)) {
        Some(_) => next.run(request).await,
        None => fail(StatusCode::UNAUTHORIZED, "a token is required").into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// A frontend served from somewhere else (a dev server, a desktop or phone shell) may
/// call the API. Access is by bearer token, not cookies, so any origin is safe to allow.
async fn cors(request: Request, next: Next) -> Response {
    let preflight = request.method() == Method::OPTIONS;
    let mut response = if preflight { StatusCode::NO_CONTENT.into_response() } else { next.run(request).await };
    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
    response
}

/// Every error under `/api` is `{"error": "<what went wrong>"}`, including the ones the
/// framework makes before a handler runs (a body that is not JSON, a number that is not
/// one, a method the route does not take).
async fn json_errors(request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let response = next.run(request).await;
    if !api || !(response.status().is_client_error() || response.status().is_server_error()) {
        return response;
    }
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if json {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if text.is_empty() { parts.status.canonical_reason().unwrap_or("error").to_lowercase() } else { text };
    let mut headers = parts.headers;
    headers.remove(header::CONTENT_LENGTH);
    headers.remove(header::CONTENT_TYPE);
    let mut reply = (parts.status, Json(json!({ "error": message }))).into_response();
    reply.headers_mut().extend(headers);
    reply
}

struct ApiError(StatusCode, String);

fn fail(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError(status, message.into())
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

type Reply = Result<Response, ApiError>;

fn ok(body: Value) -> Reply {
    Ok(Json(body).into_response())
}

fn accepted(body: Value) -> Reply {
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

async fn health(State(app): State<Arc<App>>) -> Json<Value> {
    // `terminal` is false where this build has no terminal (Windows, for now).
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION"), "auth": app.auth.describe(), "terminal": tm_harness::pty::AVAILABLE }))
}

/// Everything that is not the API: the built frontend, or the small built-in page.
async fn frontend(State(app): State<Arc<App>>, method: Method, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if method != Method::GET || path == "api" || path.starts_with("api/") {
        return fail(StatusCode::NOT_FOUND, "no such route").into_response();
    }
    let Some(web) = &app.web else {
        return if path.is_empty() { Html(PAGE).into_response() } else { StatusCode::NOT_FOUND.into_response() };
    };
    let rel = std::path::Path::new(path);
    let inside = rel.components().all(|part| matches!(part, std::path::Component::Normal(_)));
    let file = web.join(rel);
    if path.starts_with("assets/") && !(inside && file.is_file()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // A path the app routes itself, such as `/runs/web`, gets the app.
    let (file, cache) = if inside && !path.is_empty() && file.is_file() {
        (file, if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" })
    } else {
        (web.join("index.html"), "no-cache")
    };
    let Ok(bytes) = tokio::fs::read(&file).await else { return StatusCode::NOT_FOUND.into_response() };
    let kind = match file.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "txt" => "text/plain; charset=utf-8",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, cache)], bytes).into_response()
}

#[derive(Deserialize)]
struct PairBody {
    code: String,
}

async fn pair(State(app): State<Arc<App>>, Json(body): Json<PairBody>) -> Reply {
    match app.auth.enroll(&body.code) {
        Ok(Some(token)) => ok(json!({ "token": token })),
        Ok(None) => Err(fail(StatusCode::UNAUTHORIZED, "that code was refused")),
        Err(err) => Err(fail(StatusCode::TOO_MANY_REQUESTS, format!("{err:#}"))),
    }
}

// Runs.

enum Decision {
    /// Carry on from wherever the run is.
    Continue,
    Approve,
    Reject,
    Cancel,
}

impl App {
    fn project(&self) -> PathBuf {
        self.project_slot.lock().unwrap().clone()
    }

    fn records(&self) -> Records {
        self.records_slot.lock().unwrap().clone()
    }

    fn dictionary(&self) -> PathBuf {
        self.dictionary_slot.lock().unwrap().clone()
    }

    fn commons(&self) -> PathBuf {
        self.commons_slot.lock().unwrap().clone()
    }

    /// Open `path` as the current project. Existing run records stay in the previous
    /// project; this daemon now lists and drives the new one.
    fn use_project(&self, path: PathBuf) -> Result<(), ApiError> {
        if !self.active.lock().unwrap().is_empty() {
            return Err(fail(
                StatusCode::CONFLICT,
                "a run is in progress; wait for it to finish before opening another project",
            ));
        }
        if !self.reviews.lock().unwrap().is_empty() {
            return Err(fail(
                StatusCode::CONFLICT,
                "a review is in progress; wait for it to finish before opening another project",
            ));
        }
        if self.store.is_some() {
            return Err(fail(
                StatusCode::CONFLICT,
                "this daemon keeps records in a shared store, so it cannot open another project",
            ));
        }
        self.previews.stop_all();
        let records = Records::files(&path);
        *self.dictionary_slot.lock().unwrap() = path.join(".taskmaster/dictionary");
        *self.commons_slot.lock().unwrap() = path.join(".taskmaster/commons");
        *self.records_slot.lock().unwrap() = records;
        *self.project_slot.lock().unwrap() = path.clone();
        self.previews.retarget(path);
        Ok(())
    }

    fn is_active(&self, id: &str) -> bool {
        self.active.lock().unwrap().contains_key(id)
    }

    fn replay(&self, id: &str) -> anyhow::Result<Option<Run>> {
        let events = self.records().events(id)?;
        if events.is_empty() && self.records().request(id)?.is_none() {
            return Ok(None);
        }
        Ok(Some(Run::replay_with(self.parallel(id), self.focused(id), events)))
    }

    /// Replay with the parallelism the run was built with, as the engine does: with 1, a
    /// page that finished out of order (fresh pages report in any order) is dropped and
    /// the run looks stuck in building.
    fn parallel(&self, id: &str) -> usize {
        self.records().meta(id).ok().flatten().map(|meta| meta.parallel.max(1)).unwrap_or(1)
    }

    fn focused(&self, id: &str) -> bool {
        self.records().meta(id).ok().flatten().is_some_and(|meta| meta.focused)
    }

    fn known(&self, id: &str) -> Result<Run, ApiError> {
        if !valid_key(id) {
            return Err(fail(StatusCode::NOT_FOUND, "no such run"));
        }
        self.replay(id)?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such run"))
    }

    fn view(&self, id: &str, run: &Run) -> anyhow::Result<Value> {
        let status = self.records().status(id)?.unwrap_or(Value::Null);
        let routes: HashMap<&str, &str> = run
            .plan
            .iter()
            .flat_map(|plan| plan.pages.iter().map(|page| (page.id.as_str(), page.route.as_str())))
            .collect();
        let pages: Vec<Value> = run
            .pages
            .iter()
            .map(|(page, state)| {
                json!({
                    "id": page,
                    "route": routes.get(page.as_str()),
                    "state": state.state,
                    "tier": state.tier,
                    "repairs": state.repairs,
                })
            })
            .collect();
        // A cancelled run ends in the failed phase; the log says which it was.
        let cancelled = run.phase == RunPhase::Failed && self.records().events(id)?.last() == Some(&tm_core::Event::Cancelled);
        Ok(json!({
            "id": id,
            "phase": run.phase,
            "cancelled": cancelled,
            "active": self.is_active(id),
            "pending_approvals": self.live.approvals.count(id),
            "error": status.get("error"),
            "limit": status.get("limit"),
            "branch": status.get("branch"),
            "merged": status.get("merged"),
            "base": status.get("base"),
            "merge_commit": status.get("merge_commit"),
            "pages": pages,
            "estimate": run.plan.as_ref().map(|plan| &plan.estimate),
            "spent": totals(&self.records().ledger(id)?),
            "review_rounds": run.review_rounds,
            "amendments": run.amendments,
            "phase_at": run.phase_at,
            "next_phase": run.next_phase,
        }))
    }

    /// Start driving `id` in the background.
    fn start(self: &Arc<Self>, id: &str, request: RunRequest, decision: Decision) -> Result<(), ApiError> {
        let mut active = self.active.lock().unwrap();
        if active.contains_key(id) {
            return Err(fail(StatusCode::CONFLICT, format!("run `{id}` is in progress")));
        }
        let in_place = match self.records().meta(id)? {
            Some(meta) => meta.branch.is_none(),
            None => !crate::git::has_head(&self.project()),
        };
        if !active.is_empty() && (in_place || active.values().any(|other| other.in_place)) {
            return Err(fail(
                StatusCode::CONFLICT,
                "a run is building in the project's own working tree, because the repository had no \
                 commit to branch from; wait for it to finish",
            ));
        }
        let harnesses = Harnesses::from_roles(&request.roles, &*self.harness)
            .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
        let (cancel, cancelled) = watch::channel(matches!(decision, Decision::Cancel));
        let mut config = Config::new(self.project());
        config.skills = Some(self.skills_root.clone());
        let saved = crate::settings::Settings::load(&self.project()).unwrap_or_default();
        config.checker = saved.checker.enabled;
        config.code_mode = saved.code_mode.enabled;
        config.code_script_secs = saved.code_mode.script_secs;
        config.code_sh_secs = saved.code_mode.sh_secs;
        config.worker_shell = saved.sandbox.shell;
        config.package_indexes = saved.sandbox.package_indexes;
        config.prices = crate::settings::price_table(&saved);
        config.timeout = crate::settings::agent_limit(saved.agents.timeout_secs);
        config.run_id = id.to_string();
        config.plan = request.plan;
        config.prompt = request.prompt;
        config.yes = matches!(decision, Decision::Approve);
        config.reject = matches!(decision, Decision::Reject);
        config.parallel = request.parallel.unwrap_or(config.parallel).max(1);
        if let Some(secs) = request.timeout_secs {
            config.timeout = crate::settings::agent_limit(secs);
        }
        config.models = request.models;
        config.template = request.template;
        config.dictionary = Some(self.dictionary().clone());
        config.commons = Some(self.commons().clone());
        config.store = self.store.clone();
        config.remote = self.remote.clone();
        config.cancel = cancelled;
        config.live = Some(self.live.clone());
        self.live.approvals.reopen(id);
        // The run is about to change what it built, so a preview of the old build goes.
        self.previews.stop(id);
        active.insert(id.to_string(), Active { cancel, in_place });
        drop(active);

        let app = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let result = drive(config, harnesses).await;
            if let Err(err) = &result {
                eprintln!("taskmaster: run {id}: {err:#}");
                let line = format!("{err:#}").lines().next().unwrap_or("run failed").chars().take(200).collect::<String>();
                crate::log::note(&format!("run {id}: {line}"));
            }
            crate::telemetry::record(&app.project(), if result.is_ok() { "run_finished" } else { "run_failed" });
            app.record_outcome(&id, result);
            app.active.lock().unwrap().remove(&id);
            // A note can land in the moment the driver stops. Pick it up once the run is idle.
            if let Err(err) = app.wake_for_note(&id) {
                eprintln!("taskmaster: run {id}: a note was not picked up: {}", err.1);
            }
        });
        Ok(())
    }

    /// A note on a finished run starts the master again. The event is written before the
    /// driver starts, so a crash mid-follow-up resumes the pages instead of stopping.
    fn wake_for_note(self: &Arc<Self>, id: &str) -> Result<(), ApiError> {
        let _notes = self.live.write_lock();
        if self.is_active(id) {
            return Ok(());
        }
        let run = match self.replay(id) {
            Ok(Some(run)) => run,
            _ => return Ok(()),
        };
        if !matches!(run.phase, RunPhase::Done | RunPhase::Failed) || run.pages.is_empty() {
            return Ok(());
        }
        if !self.live.has_pending_where(id, |_| true) {
            return Ok(());
        }
        let mut events = self.records().events(id)?;
        let appended = !matches!(events.last(), Some(Event::FollowUp));
        if appended {
            events.push(Event::FollowUp);
            self.records().save_events(id, &events)?;
        }
        // The lock is held through start so a second note cannot start a second driver.
        match self.start(id, self.request_for(id)?, Decision::Continue) {
            Ok(()) => Ok(()),
            Err(err) => {
                if appended {
                    events.pop();
                    let _ = self.records().save_events(id, &events);
                }
                Err(err)
            }
        }
    }

    /// An error is kept with the run, so a client that was not connected still sees it.
    fn record_outcome(&self, id: &str, result: anyhow::Result<Outcome>) {
        let mut status = self.records().status(id).ok().flatten().unwrap_or_else(|| json!({}));
        let Some(fields) = status.as_object_mut() else { return };
        match result {
            Ok(_) => {
                let error_gone = fields.remove("error").is_none();
                let limit_gone = fields.remove("limit").is_none();
                if error_gone && limit_gone {
                    return;
                }
            }
            Err(err) => {
                if let Some(limit) = err.downcast_ref::<crate::reply::ReportedLimit>() {
                    fields.insert("error".into(), json!(limit.sentence()));
                    fields.insert("limit".into(), json!(limit));
                    if let Ok(mut book) = self.records().book() {
                        book.record(&limit.provider, &limit.sentence());
                        let _ = self.records().save_book(&book);
                    }
                } else {
                    fields.insert("error".into(), json!(format!("{err:#}")));
                    fields.remove("limit");
                }
            }
        }
        let _ = self.records().save_status(id, &status);
    }

    fn request_for(&self, id: &str) -> anyhow::Result<RunRequest> {
        Ok(self.records().request(id)?.unwrap_or_default())
    }
}

async fn list_runs(State(app): State<Arc<App>>) -> Reply {
    let mut runs = Vec::new();
    for id in app.records().run_ids()? {
        if let Some(run) = app.replay(&id)? {
            runs.push(app.view(&id, &run)?);
        }
    }
    ok(json!({ "runs": runs }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NewRun {
    id: Option<String>,
    prompt: Option<String>,
    plan: Option<Plan>,
    /// Build as soon as the plan is ready, without waiting for approval.
    yes: bool,
    /// Any role left out comes from the settings file's role defaults.
    roles: serde_json::Map<String, Value>,
    models: serde_json::Map<String, Value>,
    parallel: Option<usize>,
    timeout_secs: Option<u64>,
    template: Option<String>,
}

/// The saved defaults with the request's own choices laid over them.
fn with_defaults<T: serde::Serialize + serde::de::DeserializeOwned>(saved: &T, chosen: serde_json::Map<String, Value>) -> anyhow::Result<T> {
    let mut merged = serde_json::to_value(saved)?;
    if let Some(object) = merged.as_object_mut() {
        object.extend(chosen);
    }
    Ok(serde_json::from_value(merged)?)
}

async fn create_run(State(app): State<Arc<App>>, Json(body): Json<NewRun>) -> Reply {
    let id = match body.id {
        Some(id) => id,
        None => format!("run-{}", random_hex(4)?),
    };
    if !valid_key(&id) {
        return Err(fail(StatusCode::BAD_REQUEST, "a run id uses letters, digits, `_` and `-`"));
    }
    if app.replay(&id)?.is_some() {
        return Err(fail(StatusCode::CONFLICT, format!("run `{id}` already exists")));
    }
    let prompt = body.prompt.filter(|text| !text.trim().is_empty());
    if prompt.is_some() == body.plan.is_some() {
        return Err(fail(StatusCode::BAD_REQUEST, "send a prompt or a plan, and not both"));
    }
    let saved = crate::settings::Settings::load(&app.project()).unwrap_or_default();
    let bad_request = |err: anyhow::Error| fail(StatusCode::BAD_REQUEST, format!("{err:#}"));
    // A request that names roles but no models does not inherit the saved models, which belong to the saved harnesses.
    let models: Models = if body.roles.is_empty() { with_defaults(&saved.models, body.models).map_err(bad_request)? } else { with_defaults(&Models::default(), body.models).map_err(bad_request)? };
    let roles = Roles::resolve(&crate::settings::Settings::saved_roles(&app.project()), &body.roles).map_err(bad_request)?;
    check_models(&roles, &models).await.map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    if let Some(name) = &body.template {
        crate::template::load(name).map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    }
    let request = RunRequest {
        prompt,
        plan: body.plan,
        roles,
        models,
        parallel: body.parallel,
        timeout_secs: body.timeout_secs,
        template: body.template,
    };
    let decision = if body.yes { Decision::Approve } else { Decision::Continue };
    app.start(&id, request.clone(), decision)?;
    app.records().save_request(&id, &request)?;
    accepted(json!({ "id": id }))
}

async fn show_run(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let run = app.known(&id)?;
    let mut view = app.view(&id, &run)?;
    let request = app.request_for(&id)?;
    view["plan"] = json!(run.plan);
    view["prompt"] = json!(request.prompt);
    view["roles"] = json!(request.roles);
    view["models"] = json!(request.models);
    ok(view)
}

async fn run_events(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    app.known(&id)?;
    let events = app.records().events(&id)?;
    let after = query.get("after").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0).min(events.len());
    ok(json!({ "next": events.len(), "events": &events[after..] }))
}

struct Feed {
    app: Arc<App>,
    id: String,
    /// How many of the run's events the client already has.
    seen: usize,
    /// How many agent transcript entries it has.
    agents_seen: usize,
    ready: VecDeque<SseEvent>,
    over: bool,
}

async fn run_stream(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let token = bearer(&headers).or(query.get("token").map(String::as_str));
    if token.and_then(|token| app.auth.identify(token)).is_none() {
        return Err(fail(StatusCode::UNAUTHORIZED, "a token is required"));
    }
    app.known(&id)?;
    let resume = headers.get("last-event-id").and_then(|value| value.to_str().ok()).map(str::to_string);
    let seen = resume.or_else(|| query.get("after").cloned()).and_then(|n| n.parse().ok()).unwrap_or(0);
    let agents_seen = query.get("agents_after").and_then(|n| n.parse().ok()).unwrap_or(0);
    let feed = Feed { app, id, seen, agents_seen, ready: VecDeque::new(), over: false };
    let events = stream::unfold(feed, |mut feed| async move {
        loop {
            if let Some(event) = feed.ready.pop_front() {
                return Some((Ok(event), feed));
            }
            if feed.over {
                return None;
            }
            // Read "is it running" before the log: anything written while it ran is then
            // in the log we read, so the last events are not missed.
            let active = feed.app.is_active(&feed.id);
            let events = feed.app.records().events(&feed.id).unwrap_or_default();
            let (agents_total, said) = feed.app.live.read(&feed.id, feed.agents_seen);
            for entry in &said {
                feed.ready.push_back(SseEvent::default().event("agent").data(entry.to_string()));
            }
            feed.agents_seen = agents_total.max(feed.agents_seen);
            if !said.is_empty() {
                continue;
            }
            if events.len() > feed.seen {
                for (index, event) in events.iter().enumerate().skip(feed.seen) {
                    let body = serde_json::to_string(event).unwrap_or_default();
                    feed.ready.push_back(SseEvent::default().event("event").id((index + 1).to_string()).data(body));
                }
                feed.seen = events.len();
            } else if !active {
                let phase = Run::replay_with(feed.app.parallel(&feed.id), feed.app.focused(&feed.id), events).phase;
                feed.ready.push_back(SseEvent::default().event("idle").data(json!({ "phase": phase }).to_string()));
                feed.over = true;
            } else {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

async fn run_ledger(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    let entries = app.records().ledger(&id)?;
    ok(json!({ "totals": totals(&entries), "entries": entries }))
}

async fn run_changes(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    ok(json!({ "changes": app.records().changes(&id)? }))
}

async fn run_attempts(State(app): State<Arc<App>>, UrlPath((id, page)): UrlPath<(String, String)>) -> Reply {
    app.known(&id)?;
    if !valid_key(&page) {
        return Err(fail(StatusCode::NOT_FOUND, "no such page"));
    }
    ok(json!({ "attempts": app.records().attempts(&id, &page)? }))
}

async fn run_screens(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    ok(json!({ "pages": app.records().screenshots(&id)? }))
}

async fn run_screen(State(app): State<Arc<App>>, UrlPath((id, page)): UrlPath<(String, String)>) -> Reply {
    app.known(&id)?;
    if !valid_key(&page) {
        return Err(fail(StatusCode::NOT_FOUND, "no such page"));
    }
    match app.records().screenshot(&id, &page)? {
        Some(png) => Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-cache")], png).into_response()),
        None => Err(fail(StatusCode::NOT_FOUND, "no screenshot of this page")),
    }
}

fn waiting(run: &Run) -> Result<(), ApiError> {
    if run.phase == RunPhase::AwaitingApproval {
        return Ok(());
    }
    Err(fail(StatusCode::CONFLICT, "this run has no plan waiting for approval"))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApproveBody {
    /// A note the master reads before any worker starts.
    note: Option<String>,
}

async fn approve_run(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<ApproveBody>>,
) -> Reply {
    waiting(&app.known(&id)?)?;
    if let Some(note) = body.and_then(|Json(body)| body.note).filter(|text| !text.trim().is_empty()) {
        app.live.add_note(&id, "master", note.trim())?;
    }
    app.start(&id, app.request_for(&id)?, Decision::Approve)?;
    accepted(json!({ "id": id }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RejectBody {
    /// What to plan instead. Without it the run goes back to planning and waits.
    prompt: Option<String>,
}

async fn reject_run(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<RejectBody>>,
) -> Reply {
    waiting(&app.known(&id)?)?;
    let prompt = body.and_then(|Json(body)| body.prompt).filter(|text| !text.trim().is_empty());
    let mut request = app.request_for(&id)?;
    request.plan = None;
    request.prompt = prompt;
    app.start(&id, request.clone(), Decision::Reject)?;
    app.records().save_request(&id, &request)?;
    accepted(json!({ "id": id }))
}

async fn resume_run(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let run = app.known(&id)?;
    if matches!(run.phase, RunPhase::Done | RunPhase::Failed) {
        return Err(fail(StatusCode::CONFLICT, "this run is over"));
    }
    app.start(&id, app.request_for(&id)?, Decision::Continue)?;
    accepted(json!({ "id": id }))
}

async fn cancel_run(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let run = app.known(&id)?;
    app.stop_run_reviews(&id);
    {
        let active = app.active.lock().unwrap();
        if let Some(active) = active.get(&id) {
            let _ = active.cancel.send(true);
            app.live.approvals.refuse_run(&id);
            app.previews.stop(&id);
            return accepted(json!({ "id": id }));
        }
    }
    if matches!(run.phase, RunPhase::Done | RunPhase::Failed) {
        return Err(fail(StatusCode::CONFLICT, "this run is over"));
    }
    // Not running: drive it once with the cancel flag set, so the log records the end.
    let mut request = app.request_for(&id)?;
    (request.plan, request.prompt) = (None, None);
    app.start(&id, request, Decision::Cancel)?;
    accepted(json!({ "id": id }))
}

// What the project knows.

async fn providers(Query(query): Query<HashMap<String, String>>) -> Reply {
    let reports = match query.get("harness") {
        Some(id) if HARNESSES.contains(&id.as_str()) => vec![provider_report(id).await],
        Some(id) => return Err(fail(StatusCode::BAD_REQUEST, format!("unknown harness `{id}`"))),
        None => provider_reports().await,
    };
    ok(json!({ "providers": reports }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NewProject {
    /// A single path segment: letters, digits, `_` and `-`.
    name: String,
    prompt: Option<String>,
    plan: Option<Plan>,
    yes: bool,
    roles: serde_json::Map<String, Value>,
    models: serde_json::Map<String, Value>,
    parallel: Option<usize>,
    timeout_secs: Option<u64>,
    template: Option<String>,
    /// Open the new project in this daemon. Defaults to true when a prompt or plan is sent.
    switch: Option<bool>,
}

fn workspace_child(workspace: &std::path::Path, name: &str) -> Result<PathBuf, ApiError> {
    if !valid_key(name) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "a project name uses letters, digits, `_` and `-`, up to 64 characters",
        ));
    }
    let dest = workspace.join(name);
    if dest.parent() != Some(workspace) {
        return Err(fail(StatusCode::BAD_REQUEST, "a project stays in the workspace"));
    }
    Ok(dest)
}

/// The projects in the workspace: its folders that are git repositories, the open one first.
async fn list_projects(State(app): State<Arc<App>>) -> Reply {
    let current = app.project();
    let mut list: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&app.workspace) {
        for entry in entries.flatten().take(2000) {
            let path = entry.path();
            if !path.is_dir() || !path.join(".git").exists() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = path.canonicalize().unwrap_or(path);
            list.push(json!({ "name": name, "path": path.display().to_string(), "current": path == current }));
        }
    }
    if !list.iter().any(|p| p["current"] == true) {
        let name = current.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        list.push(json!({ "name": name, "path": current.display().to_string(), "current": true }));
    }
    list.sort_by(|a, b| b["current"].as_bool().cmp(&a["current"].as_bool()).then(a["name"].as_str().cmp(&b["name"].as_str())));
    ok(json!({ "projects": list, "workspace": app.workspace.display().to_string() }))
}

#[derive(Deserialize)]
struct OpenProject {
    path: String,
}

/// Open another project of the workspace, or the one open already.
async fn open_project(State(app): State<Arc<App>>, Json(body): Json<OpenProject>) -> Reply {
    let path = PathBuf::from(body.path.trim())
        .canonicalize()
        .map_err(|_| fail(StatusCode::NOT_FOUND, format!("`{}` is not a folder on this machine", body.path)))?;
    if path == app.project() {
        return ok(json!({ "project": path.display().to_string() }));
    }
    let workspace = app.workspace.canonicalize().unwrap_or_else(|_| app.workspace.clone());
    if path.parent() != Some(workspace.as_path()) || !path.join(".git").exists() {
        return Err(fail(StatusCode::BAD_REQUEST, "open a git repository that sits in the workspace"));
    }
    app.use_project(path.clone())?;
    ok(json!({ "project": path.display().to_string() }))
}

/// Create a new project folder and repository under the workspace (the current
/// project's parent). With a prompt or a plan, the daemon opens that project and
/// starts its first run.
async fn create_project(State(app): State<Arc<App>>, Json(body): Json<NewProject>) -> Reply {
    let dest = workspace_child(&app.workspace, body.name.trim())?;
    if dest.exists() {
        let empty = dest.is_dir() && dest.read_dir().map(|mut e| e.next().is_none()).unwrap_or(false);
        if !empty {
            return Err(fail(StatusCode::CONFLICT, format!("`{}` already exists", dest.display())));
        }
    } else {
        std::fs::create_dir_all(&dest).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot create {}: {err}", dest.display())))?;
    }
    let from = app.project();
    crate::git::init_repo(&dest, Some(&from)).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let path = dest.canonicalize().map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let prompt = body.prompt.filter(|text| !text.trim().is_empty());
    let switch = body.switch.unwrap_or(prompt.is_some() || body.plan.is_some());
    if !switch {
        return ok(json!({ "project": path.display().to_string() }));
    }
    app.use_project(path.clone())?;
    if prompt.is_none() && body.plan.is_none() {
        return ok(json!({ "project": path.display().to_string() }));
    }
    let id = format!("run-{}", random_hex(4)?);
    let saved = crate::settings::Settings::load(&path).unwrap_or_default();
    let bad_request = |err: anyhow::Error| fail(StatusCode::BAD_REQUEST, format!("{err:#}"));
    let models: Models = if body.roles.is_empty() {
        with_defaults(&saved.models, body.models).map_err(bad_request)?
    } else {
        with_defaults(&Models::default(), body.models).map_err(bad_request)?
    };
    let roles = Roles::resolve(&crate::settings::Settings::saved_roles(&path), &body.roles).map_err(bad_request)?;
    check_models(&roles, &models).await.map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    if let Some(name) = &body.template {
        crate::template::load(name).map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    }
    let request = RunRequest {
        prompt,
        plan: body.plan,
        roles,
        models,
        parallel: body.parallel,
        timeout_secs: body.timeout_secs,
        template: body.template,
    };
    let decision = if body.yes { Decision::Approve } else { Decision::Continue };
    app.start(&id, request.clone(), decision)?;
    app.records().save_request(&id, &request)?;
    accepted(json!({ "id": id, "project": path.display().to_string() }))
}

async fn templates() -> Json<Value> {
    Json(json!({ "templates": crate::template::BUILT_IN }))
}

async fn web_search(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let q = query.get("q").map(|q| q.trim().to_string()).unwrap_or_default();
    if q.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "q is required"));
    }
    let endpoint = match query.get("endpoint").map(|e| e.trim()).filter(|e| !e.is_empty()) {
        Some(e) => e.to_string(),
        None => crate::settings::Settings::load(&app.project()).unwrap_or_default().search_endpoint(),
    };
    let url = crate::search::build_url(&endpoint, &q);
    let shown = endpoint.split('?').next().unwrap_or("").to_string();
    let fetched = tokio::task::spawn_blocking(move || {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(6))
            .build()
            .get(&url)
            .set("accept", "application/json")
            .call()
            .map_err(|e| e.to_string())?
            .into_json::<Value>()
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match fetched {
        Ok(value) => ok(json!({ "endpoint": shown, "results": crate::search::parse(&value) })),
        Err(e) => Err(fail(StatusCode::BAD_GATEWAY, format!("the search endpoint {shown} did not answer: {e}"))),
    }
}

async fn search_dictionary(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let catalog = Catalog::open(app.dictionary(), app.commons())?;
    let found = catalog.search(query.get("q").map(String::as_str).unwrap_or(""))?;
    ok(json!({ "entries": found }))
}

async fn show_entry(State(app): State<Arc<App>>, UrlPath(key): UrlPath<String>) -> Reply {
    if !valid_key(&key) {
        return Err(fail(StatusCode::NOT_FOUND, "no such entry"));
    }
    match Catalog::open(app.dictionary(), app.commons())?.resolve(&key)? {
        Some(entry) => ok(json!(entry)),
        None => Err(fail(StatusCode::NOT_FOUND, "no such entry")),
    }
}

async fn errorography(State(app): State<Arc<App>>) -> Reply {
    ok(json!(app.records().book()?))
}

// The Commons, for other daemons.

async fn commons_entries(State(app): State<Arc<App>>) -> Reply {
    ok(json!({ "entries": LocalStore::open(app.commons())?.entries()? }))
}

async fn commons_add(State(app): State<Arc<App>>, Json(mut entry): Json<DictionaryEntry>) -> Reply {
    let commons = LocalStore::open(app.commons())?;
    if !valid_key(&entry.key) {
        return Err(fail(StatusCode::BAD_REQUEST, "an entry key uses letters, digits, `_` and `-`"));
    }
    if !tm_dictionary::matches_hash(&entry) {
        return Err(fail(StatusCode::BAD_REQUEST, format!("entry {} does not match its hash", entry.key)));
    }
    // The same content is already here, with the pass and fail counts made here.
    if commons.get(&entry.key)?.is_some_and(|have| have.hash == entry.hash) {
        return ok(json!({ "key": entry.key, "hash": entry.hash, "added": false }));
    }
    // Counts are earned on this hub, not taken from whoever sent the entry.
    entry.tier = DictTier::Commons;
    entry.stats = Some(EntryStats { reuses: 0, pass_rate: 1.0, passes: 0, fails: 0 });
    // `put` refuses content that does not match its hash.
    commons.put(&entry).map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "key": entry.key, "hash": entry.hash, "added": true }))
}

// Reviews.

#[derive(Deserialize, Default)]
#[serde(default)]
struct NewReview {
    /// A path, a repository URL or a request URL. The project when left out.
    target: Option<String>,
    base: Option<String>,
    head: Option<String>,
    pr: Option<u64>,
    dirty: bool,
    install_lints: bool,
    reviewer: Option<String>,
    verifier: Option<String>,
    reviewer_model: Option<String>,
    verifier_model: Option<String>,
}

impl App {
    /// A review that was running when the daemon stopped will never finish.
    fn close_interrupted_reviews(&self) {
        for id in self.records().review_ids().unwrap_or_default() {
            let Ok(Some(mut job)) = self.records().review(&id) else { continue };
            if job["status"] == "running" {
                job["status"] = json!("failed");
                job["error"] = json!("the daemon stopped before this review finished");
                let _ = self.records().save_review(&id, &job);
            }
        }
    }
}

async fn create_review(State(app): State<Arc<App>>, body: Option<Json<NewReview>>) -> Reply {
    start_review(&app, body.map(|Json(body)| body).unwrap_or_default(), None)
}

/// Review a finished run's whole diff, once, on request. A run never reviews itself.
async fn review_run(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let run = app.known(&id)?;
    if !matches!(run.phase, RunPhase::Done | RunPhase::Failed) {
        return Err(fail(StatusCode::CONFLICT, "review a run once it has finished"));
    }
    let meta = app.records().meta(&id)?.ok_or_else(|| fail(StatusCode::CONFLICT, "this run has not started building"))?;
    let (Some(base), Some(branch)) = (meta.base, meta.branch) else {
        return Err(fail(StatusCode::CONFLICT, "this run built in the project's own working tree, so it has no branch to review"));
    };
    let head = desk::run_head(&app, &id, &branch)
        .ok_or_else(|| fail(StatusCode::CONFLICT, format!("branch `{branch}` is gone and its last commit was not kept")))?;
    let body = NewReview { base: Some(base), head: Some(head), ..Default::default() };
    start_review(&app, body, Some(id))
}

async fn cancel_review(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    if !valid_key(&id) {
        return Err(fail(StatusCode::NOT_FOUND, "no such review"));
    }
    let Some(mut job) = app.records().review(&id)? else { return Err(fail(StatusCode::NOT_FOUND, "no such review")) };
    if job["status"] != "running" {
        return Err(fail(StatusCode::CONFLICT, "this review is over"));
    }
    app.stop_review(&id);
    job["status"] = json!("failed");
    job["error"] = json!("cancelled");
    app.records().save_review(&id, &job)?;
    accepted(json!({ "id": id }))
}

impl App {
    /// Abort a review job. Its agents die with it: each runs in its own process group that
    /// is killed when the task's future is dropped.
    fn stop_run_reviews(&self, run: &str) {
        for id in self.records().review_ids().unwrap_or_default() {
            let Ok(Some(mut job)) = self.records().review(&id) else { continue };
            if job["run"] == run && job["status"] == "running" {
                self.stop_review(&id);
                job["status"] = json!("failed");
                job["error"] = json!("cancelled");
                let _ = self.records().save_review(&id, &job);
            }
        }
    }

    fn stop_review(&self, id: &str) {
        if let Some(handle) = self.reviews.lock().unwrap().remove(id) {
            handle.abort();
        }
    }
}

fn start_review(app: &Arc<App>, body: NewReview, run: Option<String>) -> Reply {
    let defaults = crate::settings::Settings::load(&app.project()).unwrap_or_default().roles;
    let reviewer_name = body.reviewer.filter(|n| !n.is_empty()).unwrap_or(defaults.reviewer);
    let verifier_name = body.verifier.filter(|n| !n.is_empty()).unwrap_or(defaults.verifier);
    if reviewer_name.is_empty() || verifier_name.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "choose a provider for the reviewer and verifier roles first"));
    }
    let bad = |err: anyhow::Error| fail(StatusCode::BAD_REQUEST, format!("{err:#}"));
    let reviewer = (app.harness)(&reviewer_name).map_err(bad)?;
    let verifier = (app.harness)(&verifier_name).map_err(bad)?;
    let target = body.target.unwrap_or_else(|| app.project().to_string_lossy().into_owned());
    let mut options = tm_review::run::Options::new(target.clone());
    options.base = body.base;
    options.head = body.head;
    options.pr = body.pr;
    options.dirty = body.dirty;
    options.install_lints = body.install_lints;
    options.reviewer_model = body.reviewer_model;
    options.verifier_model = body.verifier_model;

    let id = format!("review-{}", random_hex(4)?);
    let mut job = json!({
        "id": id,
        "status": "running",
        "at": rfc3339_now(),
        "target": target,
        "reviewer": reviewer_name,
        "verifier": verifier_name,
        "run": run,
    });
    app.records().save_review(&id, &job)?;
    let (records, job_id, tracker) = (app.records().clone(), id.clone(), app.clone());
    let handle = tokio::spawn(async move {
        match tm_review::run::run(&options, reviewer, verifier).await {
            Ok(report) => {
                job["status"] = json!("done");
                job["text"] = json!(report.render());
                job["report"] = json!(report);
            }
            Err(err) => {
                job["status"] = json!("failed");
                job["error"] = json!(format!("{err:#}"));
            }
        }
        if let Err(err) = records.save_review(&job_id, &job) {
            eprintln!("taskmaster: review {job_id} was not recorded: {err:#}");
        }
        tracker.reviews.lock().unwrap().remove(&job_id);
    });
    app.reviews.lock().unwrap().insert(id.clone(), handle.abort_handle());
    accepted(json!({ "id": id }))
}

async fn list_reviews(State(app): State<Arc<App>>) -> Reply {
    let mut jobs = Vec::new();
    for id in app.records().review_ids()? {
        let Some(job) = app.records().review(&id)? else { continue };
        let confirmed = job["report"]["findings"]
            .as_array()
            .map(|list| list.iter().filter(|finding| finding["outcome"] == "confirmed").count());
        jobs.push(json!({
            "id": job["id"],
            "status": job["status"],
            "at": job["at"],
            "target": job["target"],
            "run": job["run"],
            "confirmed": confirmed,
        }));
    }
    jobs.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));
    ok(json!({ "reviews": jobs }))
}

async fn show_review(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    if !valid_key(&id) {
        return Err(fail(StatusCode::NOT_FOUND, "no such review"));
    }
    match app.records().review(&id)? {
        Some(job) => ok(job),
        None => Err(fail(StatusCode::NOT_FOUND, "no such review")),
    }
}

// The tailnet.

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
}
