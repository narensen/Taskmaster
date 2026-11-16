//! The HTTP API a frontend talks to.
//!
//! ```text
//! GET  /                          the phone page
//! GET  /api/health                no token needed
//! POST /api/pair                  {code} -> {token}
//!
//! GET  /api/runs                  every run, with its phase, pages and spend
//! POST /api/runs                  {prompt | plan, id?, yes?, roles?, models?, parallel?, timeout_secs?}
//! GET  /api/runs/{id}             one run, with its plan and how it was asked for
//! GET  /api/runs/{id}/events      ?after=<n> -> {next, events}
//! GET  /api/runs/{id}/ledger      what each agent call cost
//! GET  /api/runs/{id}/changes     what was committed, and by which model
//! POST /api/runs/{id}/approve     build the plan that is waiting
//! POST /api/runs/{id}/reject      {prompt?} drop the plan, and plan again when a prompt is given
//! POST /api/runs/{id}/resume      carry on a run that stopped part way
//! POST /api/runs/{id}/cancel
//!
//! GET  /api/providers             ?harness=<h> installed harnesses and their models
//! GET  /api/dictionary            ?q=<text> Commons first, then the project
//! GET  /api/dictionary/{key}
//! GET  /api/errorography          recurring mistakes, per model
//! GET  /api/reviews               reviews started here
//! POST /api/reviews               {target?, base?, head?, pr?, dirty?, reviewer?, verifier?, ...}
//! GET  /api/reviews/{id}
//! ```
//!
//! Everything under `/api` except `health` and `pair` needs `Authorization: Bearer <token>`.
//! One run builds at a time, because a run commits into the project's working tree.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::extract::{Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tm_core::{Plan, Run, RunPhase, totals, valid_key};
use tm_dictionary::{Catalog, CommonsRemote};
use tm_harness::{HARNESSES, Harness, provider_report, provider_reports};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

use crate::auth::Auth;
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
        if let Some(active) = self.app.active.lock().unwrap().as_ref() {
            let _ = active.cancel.send(true);
        }
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Active {
    run: String,
    cancel: watch::Sender<bool>,
}

struct App {
    project: PathBuf,
    records: Records,
    auth: Arc<dyn Auth>,
    store: Option<Arc<dyn Store>>,
    remote: Option<Arc<dyn CommonsRemote>>,
    dictionary: PathBuf,
    commons: PathBuf,
    harness: HarnessFactory,
    active: Mutex<Option<Active>>,
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
        let app = Arc::new(App {
            dictionary: options.dictionary.unwrap_or_else(|| project.join(".taskmaster/dictionary")),
            commons: options.commons.unwrap_or_else(|| project.join(".taskmaster/commons")),
            project,
            records,
            auth: options.auth,
            store: options.store,
            remote: options.remote,
            harness: options.harness,
            active: Mutex::new(None),
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
        Ok(Server { url: format!("http://{addr}/"), shutdown: Some(tx), task: Some(task), app })
    }
}

fn router(app: Arc<App>) -> Router {
    let private = Router::new()
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(show_run))
        .route("/api/runs/{id}/events", get(run_events))
        .route("/api/runs/{id}/ledger", get(run_ledger))
        .route("/api/runs/{id}/changes", get(run_changes))
        .route("/api/runs/{id}/approve", post(approve_run))
        .route("/api/runs/{id}/reject", post(reject_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/providers", get(providers))
        .route("/api/dictionary", get(search_dictionary))
        .route("/api/dictionary/{key}", get(show_entry))
        .route("/api/errorography", get(errorography))
        .route("/api/reviews", get(list_reviews).post(create_review))
        .route("/api/reviews/{id}", get(show_review))
        .route_layer(middleware::from_fn_with_state(app.clone(), guard));
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/health", get(health))
        .route("/api/pair", post(pair))
        .merge(private)
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

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
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
    fn active_run(&self) -> Option<String> {
        self.active.lock().unwrap().as_ref().map(|active| active.run.clone())
    }

    fn replay(&self, id: &str) -> anyhow::Result<Option<Run>> {
        let events = self.records.events(id)?;
        if events.is_empty() && self.records.request(id)?.is_none() {
            return Ok(None);
        }
        Ok(Some(Run::replay(1, events)))
    }

    fn known(&self, id: &str) -> Result<Run, ApiError> {
        if !valid_key(id) {
            return Err(fail(StatusCode::NOT_FOUND, "no such run"));
        }
        self.replay(id)?.ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such run"))
    }

    fn view(&self, id: &str, run: &Run) -> anyhow::Result<Value> {
        let status = self.records.status(id)?.unwrap_or(Value::Null);
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
        Ok(json!({
            "id": id,
            "phase": run.phase,
            "active": self.active_run().as_deref() == Some(id),
            "error": status.get("error"),
            "pages": pages,
            "estimate": run.plan.as_ref().map(|plan| &plan.estimate),
            "spent": totals(&self.records.ledger(id)?),
            "review_rounds": run.review_rounds,
            "amendments": run.amendments,
        }))
    }

    /// Start driving `id` in the background. Only one run drives at a time.
    fn start(self: &Arc<Self>, id: &str, request: RunRequest, decision: Decision) -> Result<(), ApiError> {
        let mut active = self.active.lock().unwrap();
        if let Some(running) = active.as_ref() {
            return Err(fail(StatusCode::CONFLICT, format!("run `{}` is in progress", running.run)));
        }
        let harnesses = Harnesses::from_roles(&request.roles, &*self.harness)
            .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
        let (cancel, cancelled) = watch::channel(matches!(decision, Decision::Cancel));
        let mut config = Config::new(&self.project);
        config.run_id = id.to_string();
        config.plan = request.plan;
        config.prompt = request.prompt;
        config.yes = matches!(decision, Decision::Approve);
        config.reject = matches!(decision, Decision::Reject);
        config.parallel = request.parallel.unwrap_or(config.parallel).max(1);
        if let Some(secs) = request.timeout_secs {
            config.timeout = Duration::from_secs(secs.max(1));
        }
        config.models = request.models;
        config.dictionary = Some(self.dictionary.clone());
        config.commons = Some(self.commons.clone());
        config.store = self.store.clone();
        config.remote = self.remote.clone();
        config.cancel = cancelled;
        *active = Some(Active { run: id.to_string(), cancel });
        drop(active);

        let app = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let result = drive(config, harnesses).await;
            if let Err(err) = &result {
                eprintln!("taskmaster: run {id}: {err:#}");
            }
            app.record_outcome(&id, result);
            *app.active.lock().unwrap() = None;
        });
        Ok(())
    }

    /// An error is kept with the run, so a client that was not connected still sees it.
    fn record_outcome(&self, id: &str, result: anyhow::Result<Outcome>) {
        let mut status = self.records.status(id).ok().flatten().unwrap_or_else(|| json!({}));
        let Some(fields) = status.as_object_mut() else { return };
        match result {
            Ok(_) => {
                if fields.remove("error").is_none() {
                    return;
                }
            }
            Err(err) => {
                fields.insert("error".into(), json!(format!("{err:#}")));
            }
        }
        let _ = self.records.save_status(id, &status);
    }

    fn request_for(&self, id: &str) -> anyhow::Result<RunRequest> {
        Ok(self.records.request(id)?.unwrap_or_default())
    }
}

async fn list_runs(State(app): State<Arc<App>>) -> Reply {
    let mut runs = Vec::new();
    for id in app.records.run_ids()? {
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
    roles: Roles,
    models: Models,
    parallel: Option<usize>,
    timeout_secs: Option<u64>,
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
    check_models(&body.roles, &body.models).await.map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    let request = RunRequest {
        prompt,
        plan: body.plan,
        roles: body.roles,
        models: body.models,
        parallel: body.parallel,
        timeout_secs: body.timeout_secs,
    };
    let decision = if body.yes { Decision::Approve } else { Decision::Continue };
    app.start(&id, request.clone(), decision)?;
    app.records.save_request(&id, &request)?;
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
    let events = app.records.events(&id)?;
    let after = query.get("after").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0).min(events.len());
    ok(json!({ "next": events.len(), "events": &events[after..] }))
}

async fn run_ledger(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    let entries = app.records.ledger(&id)?;
    ok(json!({ "totals": totals(&entries), "entries": entries }))
}

async fn run_changes(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    app.known(&id)?;
    ok(json!({ "changes": app.records.changes(&id)? }))
}

fn waiting(run: &Run) -> Result<(), ApiError> {
    if run.phase == RunPhase::AwaitingApproval {
        return Ok(());
    }
    Err(fail(StatusCode::CONFLICT, "this run has no plan waiting for approval"))
}

async fn approve_run(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    waiting(&app.known(&id)?)?;
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
    app.records.save_request(&id, &request)?;
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
    {
        let active = app.active.lock().unwrap();
        if let Some(active) = active.as_ref().filter(|active| active.run == id) {
            let _ = active.cancel.send(true);
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

async fn search_dictionary(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let catalog = Catalog::open(&app.dictionary, &app.commons)?;
    let found = catalog.search(query.get("q").map(String::as_str).unwrap_or(""))?;
    ok(json!({ "entries": found }))
}

async fn show_entry(State(app): State<Arc<App>>, UrlPath(key): UrlPath<String>) -> Reply {
    if !valid_key(&key) {
        return Err(fail(StatusCode::NOT_FOUND, "no such entry"));
    }
    match Catalog::open(&app.dictionary, &app.commons)?.resolve(&key)? {
        Some(entry) => ok(json!(entry)),
        None => Err(fail(StatusCode::NOT_FOUND, "no such entry")),
    }
}

async fn errorography(State(app): State<Arc<App>>) -> Reply {
    ok(json!(app.records.book()?))
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
        for id in self.records.review_ids().unwrap_or_default() {
            let Ok(Some(mut job)) = self.records.review(&id) else { continue };
            if job["status"] == "running" {
                job["status"] = json!("failed");
                job["error"] = json!("the daemon stopped before this review finished");
                let _ = self.records.save_review(&id, &job);
            }
        }
    }
}

async fn create_review(State(app): State<Arc<App>>, body: Option<Json<NewReview>>) -> Reply {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let defaults = Roles::default();
    let reviewer_name = body.reviewer.unwrap_or(defaults.reviewer);
    let verifier_name = body.verifier.unwrap_or(defaults.verifier);
    let bad = |err: anyhow::Error| fail(StatusCode::BAD_REQUEST, format!("{err:#}"));
    let reviewer = (app.harness)(&reviewer_name).map_err(bad)?;
    let verifier = (app.harness)(&verifier_name).map_err(bad)?;
    let target = body.target.unwrap_or_else(|| app.project.to_string_lossy().into_owned());
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
    });
    app.records.save_review(&id, &job)?;
    let (records, job_id) = (app.records.clone(), id.clone());
    tokio::spawn(async move {
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
    });
    accepted(json!({ "id": id }))
}

async fn list_reviews(State(app): State<Arc<App>>) -> Reply {
    let mut jobs = Vec::new();
    for id in app.records.review_ids()? {
        let Some(job) = app.records.review(&id)? else { continue };
        let confirmed = job["report"]["findings"]
            .as_array()
            .map(|list| list.iter().filter(|finding| finding["outcome"] == "confirmed").count());
        jobs.push(json!({
            "id": job["id"],
            "status": job["status"],
            "at": job["at"],
            "target": job["target"],
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
    match app.records.review(&id)? {
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
