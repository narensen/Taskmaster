//! The service's routes.
//!
//! ```text
//! GET  /                              what the Commons is
//! GET  /signin                        ?next=<path>
//! GET  /auth/{google|github}/start    ?next=<path> | ?redirect_uri=<a Taskmaster>
//! GET  /auth/{google|github}/callback
//! GET  /authorize                     ?redirect_uri=<a Taskmaster>/signed-in
//! POST /authorize                     hands that Taskmaster a sign-in token
//! POST /signout
//! GET  /account                       tokens for your Taskmasters, and what you shared
//! POST /account/tokens                {label}
//! POST /account/tokens/{id}/revoke
//! GET  /entries                       ?q=
//! POST /entries/{key}/{hash}/report   {reason}
//! GET  /moderate                      the weekly review, for moderators
//! POST /moderate/entries/{key}/{hash} {decision: approve | reject, note?}
//! POST /moderate/reports/{id}         {action: withdraw | ban | dismiss}
//! GET  /.well-known/jwks.json         the key that signs sign-in tokens
//! GET  /privacy                       what is kept, and why
//! GET  /terms                         the rules for using the Commons
//! GET  /healthz
//!
//! GET  /api/commons/entries           ?stack=<next@15,react@19> served entries, and withdrawn ones
//! POST /api/commons/entries           an entry to review               (token)
//! POST /api/commons/usage             {key, hash, passed}              (token)
//! POST /api/commons/reports           {key, hash, reason}              (token)
//! GET  /api/me                                                         (token)
//! ```
//!
//! Pages use a session cookie, and each form carries a token tied to that session. The
//! API takes `Authorization: Bearer tmc_…`, a token made on the account page.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tm_core::{DictionaryEntry, valid_key};
use tm_token::{Claims, DAEMON_AUDIENCE, Signer};

use crate::config::Config;
use crate::db::{Db, Status, Submission, User, best_per_key, suits};
use crate::oauth::Provider;
use crate::pages::{self, Viewer};
use crate::seal::Sealer;

/// How long a daemon sign-in token lasts. Signing in again is two clicks.
const DAEMON_TOKEN_DAYS: u64 = 30;
const SESSION_DAYS: u64 = 30;
const MAX_CONTENT: usize = 64 * 1024;

pub struct App {
    pub config: Config,
    pub db: Arc<dyn Db>,
    pub signer: Signer,
    pub sealer: Sealer,
    pub http: reqwest::Client,
}

impl App {
    pub fn new(config: Config, db: Arc<dyn Db>) -> anyhow::Result<App> {
        let signer = Signer::from_seed(&config.signing_key)?;
        let sealer = Sealer::new(&config.session_secret);
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(App { config, db, signer, sealer, http })
    }

    fn is_moderator(&self, user: &User) -> bool {
        !user.banned && claims_for(user, "", 0).allowed_by(&self.config.moderators)
    }
}

fn claims_for(user: &User, issuer: &str, exp: u64) -> Claims {
    Claims {
        iss: issuer.into(),
        sub: user.sub(),
        aud: DAEMON_AUDIENCE.into(),
        iat: tm_token::now(),
        exp,
        login: user.login.clone(),
        email: user.email.clone(),
        name: user.name.clone(),
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/signin", get(signin))
        .route("/auth/{provider}", get(start))
        .route("/auth/{provider}/start", get(start))
        .route("/auth/{provider}/callback", get(callback))
        .route("/authorize", get(authorize).post(authorize_confirm))
        .route("/signout", post(signout))
        .route("/account", get(account))
        .route("/account/tokens", post(make_token))
        .route("/account/tokens/{id}/revoke", post(revoke_token))
        .route("/entries", get(entries_page))
        .route("/entries/{key}/{hash}/report", post(report_page))
        .route("/moderate", get(moderate))
        .route("/moderate/entries/{key}/{hash}", post(decide))
        .route("/moderate/reports/{id}", post(settle_report))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/privacy", get(privacy))
        .route("/terms", get(terms))
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/commons/entries", get(api_entries).post(api_submit))
        .route("/api/commons/usage", post(api_usage))
        .route("/api/commons/reports", post(api_report))
        .route("/api/me", get(api_me))
        .with_state(app)
}

// Cookies and sessions.

const SESSION: &str = "tm_session";
const FLOW: &str = "tm_signin";

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

fn set_cookie(app: &App, name: &str, value: &str, max_age: u64) -> HeaderValue {
    let secure = if app.config.secure_cookies() { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .unwrap_or(HeaderValue::from_static(""))
}

struct Session {
    user: User,
    raw: String,
}

async fn session(app: &App, headers: &HeaderMap) -> Option<Session> {
    let raw = cookie(headers, SESSION)?;
    let value = app.sealer.open("session", &raw)?;
    let user = app.db.user(value["uid"].as_i64()?).await.ok()??;
    (!user.banned).then_some(Session { user, raw })
}

fn viewer<'a>(app: &App, session: Option<&'a Session>) -> Viewer<'a> {
    Viewer {
        user: session.map(|s| &s.user),
        moderator: session.is_some_and(|s| app.is_moderator(&s.user)),
        csrf: session.map(|s| app.sealer.csrf(&s.raw)).unwrap_or_default(),
    }
}

fn page(app: &App, session: Option<&Session>, title: &str, body: &str) -> Response {
    Html(pages::layout(title, &viewer(app, session), body)).into_response()
}

fn problem(app: &App, session: Option<&Session>, status: StatusCode, title: &str, text: &str) -> Response {
    (status, page(app, session, title, &pages::message(title, text))).into_response()
}

fn see_other(location: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, location.to_string())]).into_response()
}

/// The signed-in person, if the form came from one of this service's own pages.
async fn form_session(app: &App, headers: &HeaderMap, form: &HashMap<String, String>) -> Option<Session> {
    let session = session(app, headers).await?;
    let sent = form.get("csrf")?;
    same(sent, &app.sealer.csrf(&session.raw)).then_some(session)
}

/// A path on this service, never another site.
fn local_path(next: Option<&String>) -> String {
    match next {
        // Browsers drop tabs and newlines from a URL, so `/\t/evil.example` would become
        // `//evil.example`: no control characters at all.
        Some(path) if path.starts_with('/') && !path.starts_with("//") && !path.contains('\\') && !path.chars().any(char::is_control) => {
            path.clone()
        }
        _ => "/account".into(),
    }
}

/// Where a Taskmaster wants its sign-in token: `<origin>/signed-in`. Plain HTTP is
/// accepted only for this machine, a private network, or a tailnet, where daemons live.
pub fn daemon_redirect(raw: &str) -> Option<url::Url> {
    let url = url::Url::parse(raw).ok()?;
    if url.path() != "/signed-in" || url.fragment().is_some() || url.query().is_some() {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.scheme() {
        "https" => Some(url),
        "http" => {
            let local = match url.host()? {
                url::Host::Domain(name) => name == "localhost" || name.ends_with(".ts.net") || name.ends_with(".local"),
                url::Host::Ipv4(ip) => {
                    let [a, b, ..] = ip.octets();
                    ip.is_loopback() || ip.is_private() || (a == 100 && (64..128).contains(&b))
                }
                url::Host::Ipv6(ip) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
            };
            local.then_some(url)
        }
        _ => None,
    }
}

/// Equality that does not stop at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).expect("/dev/urandom is readable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fingerprint(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

// Pages.

async fn home(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let session = session(&app, &headers).await;
    let counts = app.db.counts().await.unwrap_or_default();
    page(&app, session.as_ref(), "The Commons", &pages::home(&counts, session.is_some(), &Provider::enabled(&app.config)))
}

async fn privacy(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let session = session(&app, &headers).await;
    page(&app, session.as_ref(), "Privacy", &pages::privacy())
}

async fn terms(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let session = session(&app, &headers).await;
    page(&app, session.as_ref(), "Terms", &pages::terms())
}

async fn signin(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let next = local_path(q.get("next"));
    if session(&app, &headers).await.is_some() {
        return see_other(&next);
    }
    page(&app, None, "Sign in", &pages::signin(&Provider::enabled(&app.config), &next))
}

async fn start(State(app): State<Arc<App>>, Path(provider): Path<String>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(provider) = Provider::parse(&provider).filter(|p| p.client(&app.config).is_some()) else {
        return problem(&app, None, StatusCode::NOT_FOUND, "Not available", "That way of signing in is not set up here.");
    };
    let redirect = match q.get("redirect_uri") {
        Some(raw) => match daemon_redirect(raw) {
            Some(url) => Some(url.to_string()),
            None => return problem(&app, None, StatusCode::BAD_REQUEST, "Not a Taskmaster", "That address cannot receive a sign-in."),
        },
        None => None,
    };
    let state = random_hex(16);
    let flow = app.sealer.seal(
        "signin",
        &json!({ "state": state, "provider": provider.id(), "next": local_path(q.get("next")), "redirect": redirect, "exp": tm_token::now() + 600 }),
    );
    let Some(url) = provider.authorize_url(&app.config, &state) else {
        return problem(&app, None, StatusCode::INTERNAL_SERVER_ERROR, "Sign-in is misconfigured", "The provider address is not valid.");
    };
    let mut response = see_other(&url);
    response.headers_mut().append(header::SET_COOKIE, set_cookie(&app, FLOW, &flow, 600));
    response
}

/// Whatever happens, a sign-in in progress is used up by its callback, and nothing about
/// the reply may be cached.
async fn callback(
    State(app): State<Arc<App>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let mut response = finish_sign_in(&app, &provider, &headers, &q).await;
    response.headers_mut().append(header::SET_COOKIE, set_cookie(&app, FLOW, "", 0));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn finish_sign_in(app: &App, provider: &str, headers: &HeaderMap, q: &HashMap<String, String>) -> Response {
    let Some(provider) = Provider::parse(provider) else {
        return problem(app, None, StatusCode::NOT_FOUND, "Not available", "That way of signing in is not set up here.");
    };
    if q.contains_key("error") {
        return problem(app, None, StatusCode::BAD_REQUEST, "Signed out", "The sign-in was cancelled.");
    }
    let flow = cookie(headers, FLOW).and_then(|raw| app.sealer.open("signin", &raw));
    let Some(flow) = flow.filter(|f| f["provider"] == provider.id() && q.get("state").is_some_and(|s| f["state"].as_str().is_some_and(|expected| same(expected, s)))) else {
        return problem(app, None, StatusCode::BAD_REQUEST, "Start again", "This sign-in has expired or was not started here.");
    };
    let Some(code) = q.get("code") else {
        return problem(app, None, StatusCode::BAD_REQUEST, "Start again", "The provider sent no code.");
    };
    let identity = match provider.identify(&app.http, &app.config, code).await {
        Ok(identity) => identity,
        Err(err) => {
            eprintln!("tm-commons: {} sign-in failed: {err:#}", provider.label());
            return problem(app, None, StatusCode::BAD_GATEWAY, "Could not sign in", &format!("{} did not confirm who you are. Try again.", provider.label()));
        }
    };
    let user = match app.db.upsert_user(&identity).await {
        Ok(user) => user,
        Err(err) => {
            eprintln!("tm-commons: cannot save a user: {err:#}");
            return problem(app, None, StatusCode::INTERNAL_SERVER_ERROR, "Could not sign in", "Something went wrong on our side.");
        }
    };
    if user.banned {
        return problem(app, None, StatusCode::FORBIDDEN, "Banned", "This account may not use the Commons.");
    }
    let sealed = app.sealer.seal("session", &json!({ "uid": user.id, "exp": tm_token::now() + SESSION_DAYS * 86_400 }));
    let next = match flow["redirect"].as_str() {
        Some(redirect) => format!("/authorize?redirect_uri={}", pages::enc(redirect)),
        None => local_path(flow["next"].as_str().map(str::to_string).as_ref()),
    };
    let mut response = see_other(&next);
    response.headers_mut().append(header::SET_COOKIE, set_cookie(app, SESSION, &sealed, SESSION_DAYS * 86_400));
    response
}

async fn authorize(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let session = session(&app, &headers).await;
    let Some(url) = q.get("redirect_uri").and_then(|raw| daemon_redirect(raw)) else {
        return problem(&app, session.as_ref(), StatusCode::BAD_REQUEST, "Not a Taskmaster", "That address cannot receive a sign-in.");
    };
    let destination = url.origin().ascii_serialization();
    let csrf = session.as_ref().map(|s| app.sealer.csrf(&s.raw)).unwrap_or_default();
    let body = pages::authorize(&destination, url.as_str(), session.as_ref().map(|s| &s.user), &Provider::enabled(&app.config), &csrf);
    page(&app, session.as_ref(), "Sign in to your Taskmaster", &body)
}

async fn authorize_confirm(State(app): State<Arc<App>>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    let Some(session) = form_session(&app, &headers, &form).await else {
        return problem(&app, None, StatusCode::FORBIDDEN, "Start again", "Open the sign-in from your Taskmaster again.");
    };
    let Some(url) = form.get("redirect_uri").and_then(|raw| daemon_redirect(raw)) else {
        return problem(&app, Some(&session), StatusCode::BAD_REQUEST, "Not a Taskmaster", "That address cannot receive a sign-in.");
    };
    let claims = claims_for(&session.user, &app.config.url, tm_token::now() + DAEMON_TOKEN_DAYS * 86_400);
    see_other(&format!("{url}#token={}", app.signer.sign(&claims)))
}

async fn signout(State(app): State<Arc<App>>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    if form_session(&app, &headers, &form).await.is_none() {
        return see_other("/");
    }
    let mut response = see_other("/");
    response.headers_mut().append(header::SET_COOKIE, set_cookie(&app, SESSION, "", 0));
    response
}

async fn account_page(app: &App, session: &Session, new_token: Option<&str>) -> Response {
    let tokens = app.db.tokens(session.user.id).await.unwrap_or_default();
    let mine = app.db.submissions_by(session.user.id).await.unwrap_or_default();
    let body = pages::account(&session.user, &tokens, &mine, &app.sealer.csrf(&session.raw), &app.config.url, new_token);
    page(app, Some(session), "Your account", &body)
}

async fn account(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    match session(&app, &headers).await {
        Some(session) => account_page(&app, &session, None).await,
        None => see_other("/signin?next=%2Faccount"),
    }
}

async fn make_token(State(app): State<Arc<App>>, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    let Some(session) = form_session(&app, &headers, &form).await else { return see_other("/signin?next=%2Faccount") };
    let label: String = form.get("label").map(|l| l.trim().chars().take(60).collect()).unwrap_or_default();
    if label.is_empty() {
        return see_other("/account");
    }
    let token = format!("tmc_{}", random_hex(24));
    if let Err(err) = app.db.add_token(session.user.id, &fingerprint(&token), &label).await {
        eprintln!("tm-commons: cannot save a token: {err:#}");
        return problem(&app, Some(&session), StatusCode::INTERNAL_SERVER_ERROR, "No token", "Something went wrong on our side.");
    }
    // Shown once, in this reply, and never stored.
    let mut response = account_page(&app, &session, Some(&token)).await;
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn revoke_token(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(session) = form_session(&app, &headers, &form).await {
        let _ = app.db.revoke_token(session.user.id, &id).await;
    }
    see_other("/account")
}

async fn entries_page(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let session = session(&app, &headers).await;
    let query = q.get("q").map(|s| s.trim().to_lowercase()).unwrap_or_default();
    let served: Vec<String> = match app.db.submissions(Status::Approved).await {
        Ok(all) => best_per_key(all).into_iter().map(|e| e.hash).collect(),
        Err(_) => Vec::new(),
    };
    let mut list: Vec<Submission> = app
        .db
        .submissions(Status::Approved)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|e| served.contains(&e.hash))
        .filter(|e| query.is_empty() || e.key.to_lowercase().contains(&query) || e.summary.to_lowercase().contains(&query))
        .collect();
    list.sort_by(|a, b| b.passes.cmp(&a.passes).then(a.key.cmp(&b.key)));
    let csrf = session.as_ref().map(|s| app.sealer.csrf(&s.raw));
    page(&app, session.as_ref(), "Entries", &pages::entries(&list, &query, csrf.as_deref()))
}

async fn report_page(
    State(app): State<Arc<App>>,
    Path((key, hash)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let Some(session) = form_session(&app, &headers, &form).await else { return see_other("/signin?next=%2Fentries") };
    let reason: String = form.get("reason").map(|r| r.trim().chars().take(1000).collect()).unwrap_or_default();
    if reason.is_empty() {
        return see_other("/entries");
    }
    if app.db.report(&key, &hash, session.user.id, &reason).await.unwrap_or(false) {
        return problem(&app, Some(&session), StatusCode::OK, "Thank you", "A moderator will look at it in this week's review.");
    }
    problem(&app, Some(&session), StatusCode::NOT_FOUND, "No such entry", "It may have been withdrawn already.")
}

async fn moderator(app: &App, headers: &HeaderMap) -> Result<Session, Box<Response>> {
    match session(app, headers).await {
        Some(session) if app.is_moderator(&session.user) => Ok(session),
        Some(session) => Err(Box::new(problem(app, Some(&session), StatusCode::FORBIDDEN, "Moderators only", "Your account is not on the moderator list."))),
        None => Err(Box::new(see_other("/signin?next=%2Fmoderate"))),
    }
}

async fn moderate(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let session = match moderator(&app, &headers).await {
        Ok(session) => session,
        Err(response) => return *response,
    };
    let pending = app.db.submissions(Status::Pending).await.unwrap_or_default();
    let mut reports = Vec::new();
    for report in app.db.open_reports().await.unwrap_or_default() {
        let entry = app.db.submission(&report.key, &report.hash).await.ok().flatten();
        reports.push((report, entry));
    }
    let struggling: Vec<Submission> = app
        .db
        .submissions(Status::Approved)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.fails > e.passes && e.users >= 2)
        .collect();
    let csrf = app.sealer.csrf(&session.raw);
    let body = pages::moderate(&pages::Queue { pending: &pending, reports: &reports, struggling: &struggling, csrf: &csrf });
    page(&app, Some(&session), "Moderate", &body)
}

async fn decide(
    State(app): State<Arc<App>>,
    Path((key, hash)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let session = match moderator(&app, &headers).await {
        Ok(session) if form.get("csrf").is_some_and(|c| *c == app.sealer.csrf(&session.raw)) => session,
        Ok(session) => return problem(&app, Some(&session), StatusCode::FORBIDDEN, "Start again", "Reload the page and try again."),
        Err(response) => return *response,
    };
    let status = match form.get("decision").map(String::as_str) {
        Some("approve") => Status::Approved,
        Some("reject") => Status::Rejected,
        _ => return see_other("/moderate"),
    };
    let note = form.get("note").map(|n| n.trim()).filter(|n| !n.is_empty());
    let _ = app.db.review(&key, &hash, status, session.user.id, note).await;
    see_other("/moderate")
}

async fn settle_report(
    State(app): State<Arc<App>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let session = match moderator(&app, &headers).await {
        Ok(session) if form.get("csrf").is_some_and(|c| *c == app.sealer.csrf(&session.raw)) => session,
        Ok(session) => return problem(&app, Some(&session), StatusCode::FORBIDDEN, "Start again", "Reload the page and try again."),
        Err(response) => return *response,
    };
    let reports = app.db.open_reports().await.unwrap_or_default();
    let Some(report) = reports.iter().find(|r| r.id == id) else { return see_other("/moderate") };
    let action = form.get("action").map(String::as_str).unwrap_or("");
    if action == "withdraw" || action == "ban" {
        let note = format!("withdrawn after a report: {}", report.reason.chars().take(200).collect::<String>());
        let _ = app.db.review(&report.key, &report.hash, Status::Rejected, session.user.id, Some(&note)).await;
    }
    if action == "ban"
        && let Ok(Some(entry)) = app.db.submission(&report.key, &report.hash).await
        && entry.author != session.user.id
    {
        let _ = app.db.set_banned(entry.author, true).await;
    }
    if matches!(action, "withdraw" | "ban" | "dismiss") {
        let _ = app.db.resolve_report(id).await;
    }
    see_other("/moderate")
}

async fn jwks(State(app): State<Arc<App>>) -> Response {
    ([(header::CACHE_CONTROL, "public, max-age=300")], Json(app.signer.jwks())).into_response()
}

// The API.

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        eprintln!("tm-commons: {err:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "something went wrong on our side".into())
    }
}

fn fail(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError(status, message.into())
}

async fn caller(app: &App, headers: &HeaderMap) -> Result<User, ApiError> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.starts_with("tmc_"))
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, format!("send a token made at {}/account", app.config.url)))?;
    match app.db.token_user(&fingerprint(token.trim())).await? {
        Some(user) if user.banned => Err(fail(StatusCode::FORBIDDEN, "this account may not use the Commons")),
        Some(user) => Ok(user),
        None => Err(fail(StatusCode::UNAUTHORIZED, "that token was revoked or never existed")),
    }
}

async fn api_entries(State(app): State<Arc<App>>, Query(q): Query<HashMap<String, String>>) -> Result<Response, ApiError> {
    let stack = q.get("stack").map(String::as_str);
    let entries: Vec<DictionaryEntry> =
        best_per_key(app.db.submissions(Status::Approved).await?).into_iter().filter(|e| suits(&e.stack, stack)).collect();
    // A daemon deletes these if it pulled them before they were withdrawn.
    let withdrawn: Vec<Value> =
        app.db.submissions(Status::Rejected).await?.into_iter().map(|e| json!({ "key": e.key, "hash": e.hash })).collect();
    Ok(([(header::CACHE_CONTROL, "public, max-age=60")], Json(json!({ "entries": entries, "withdrawn": withdrawn }))).into_response())
}

async fn api_submit(State(app): State<Arc<App>>, headers: HeaderMap, Json(entry): Json<DictionaryEntry>) -> Result<Response, ApiError> {
    let user = caller(&app, &headers).await?;
    if !valid_key(&entry.key) {
        return Err(fail(StatusCode::BAD_REQUEST, "an entry key uses letters, digits, `_` and `-`"));
    }
    if fingerprint(&entry.content) != entry.hash {
        return Err(fail(StatusCode::BAD_REQUEST, "the content does not match its hash"));
    }
    if entry.content.len() > MAX_CONTENT || entry.summary.chars().count() > 300 || entry.stack.len() > 200 {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, "an entry is at most 64 KiB, with a summary of 300 characters"));
    }
    if entry.summary.trim().is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "an entry needs a summary"));
    }
    let found = crate::scan::credentials(&format!("{}\n{}", entry.summary, entry.content));
    if !found.is_empty() {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, format!("it looks like it contains {}; nothing was shared", found.join(", "))));
    }
    if let Some(have) = app.db.submission(&entry.key, &entry.hash).await? {
        return Ok(Json(json!({ "key": entry.key, "hash": entry.hash, "status": have.status })).into_response());
    }
    if app.db.submitted_since_yesterday(user.id).await? >= app.config.daily_limit {
        return Err(fail(StatusCode::TOO_MANY_REQUESTS, "that is enough entries for one day"));
    }
    app.db.submit(&entry, user.id).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "key": entry.key, "hash": entry.hash, "status": "pending" }))).into_response())
}

#[derive(Deserialize)]
struct UsageBody {
    key: String,
    hash: String,
    passed: bool,
}

async fn api_usage(State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<UsageBody>) -> Result<Response, ApiError> {
    let user = caller(&app, &headers).await?;
    if app.db.record_usage(&body.key, &body.hash, user.id, body.passed).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(fail(StatusCode::NOT_FOUND, "that entry is not served here"))
    }
}

#[derive(Deserialize)]
struct ReportBody {
    key: String,
    hash: String,
    reason: String,
}

async fn api_report(State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<ReportBody>) -> Result<Response, ApiError> {
    let user = caller(&app, &headers).await?;
    let reason: String = body.reason.trim().chars().take(1000).collect();
    if reason.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "say what is wrong"));
    }
    if app.db.report(&body.key, &body.hash, user.id, &reason).await? {
        Ok(StatusCode::CREATED.into_response())
    } else {
        Err(fail(StatusCode::NOT_FOUND, "no such entry"))
    }
}

async fn api_me(State(app): State<Arc<App>>, headers: HeaderMap) -> Result<Response, ApiError> {
    let user = caller(&app, &headers).await?;
    Ok(Json(json!({
        "sub": user.sub(),
        "handle": user.handle(),
        "allow": pages::allow_entry(&user),
        "moderator": app.is_moderator(&user),
    }))
    .into_response())
}
