//! Installing, updating and signing in to harness CLIs from the app.
//!
//! ```text
//! GET  /api/setup                              every provider's and runtime's state, and the latest jobs
//! POST /api/setup/providers/{id}/{action}      install | update | login; returns the job
//! POST /api/setup/runtimes/{id}/install        node | bun | python
//! GET  /api/setup/jobs/{id}                    one job: steps, output, sign-in address and code
//! GET  /api/setup/jobs/{id}/stream             the same, sent again on every change (server-sent events)
//! POST /api/setup/jobs/{id}/input              {text?, key?} typed into a running sign-in screen
//! POST /api/setup/jobs/{id}/cancel
//! ```
//!
//! The commands come from `tm_harness::setup`; a request only names a provider or runtime
//! and an action from a fixed set. `stream`, like a run's stream, takes `?token=`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::json;
use tm_harness::pty::Key;
use tm_harness::setup::Action;

use super::{App, ApiError, Reply, accepted, bearer, fail, ok};
use crate::jobs::{MAX_INPUT, StartError, Target};

pub async fn overview(State(app): State<Arc<App>>) -> Reply {
    let (providers, runtimes) = tokio::join!(tm_harness::setup::provider_statuses(), tm_harness::setup::runtime_statuses());
    ok(json!({ "providers": providers, "runtimes": runtimes, "jobs": app.jobs.latest() }))
}

fn started(result: Result<crate::jobs::JobView, StartError>) -> Reply {
    match result {
        Ok(view) => accepted(json!(view)),
        Err(StartError::NotApplicable(why)) => Err(fail(StatusCode::CONFLICT, why)),
        Err(StartError::Busy(id)) => Err(ApiError(StatusCode::CONFLICT, format!("another job for this is still running: {id}"))),
    }
}

pub async fn provider_action(State(app): State<Arc<App>>, UrlPath((id, action)): UrlPath<(String, String)>) -> Reply {
    if !tm_harness::HARNESSES.contains(&id.as_str()) {
        return Err(fail(StatusCode::NOT_FOUND, format!("no provider `{id}`")));
    }
    let Some(action) = Action::parse(&action) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("no action `{action}`: use install, update or login")));
    };
    app.forget_provider(&id);
    started(app.jobs.start(Target::Provider(&id), action))
}

pub async fn runtime_install(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    if tm_harness::setup::runtime(&id).is_none() {
        return Err(fail(StatusCode::NOT_FOUND, format!("no runtime `{id}`")));
    }
    started(app.jobs.start(Target::Runtime(&id), Action::Install))
}

pub async fn job(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let job = app.jobs.get(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no job `{id}`")))?;
    ok(json!(job.view()))
}

#[derive(Deserialize)]
pub struct InputBody {
    text: Option<String>,
    key: Option<String>,
}

pub async fn input(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<InputBody>) -> Reply {
    let job = app.jobs.get(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no job `{id}`")))?;
    let view = job.view();
    if view.action != Action::Login {
        return Err(fail(StatusCode::CONFLICT, "only a sign-in reads what is typed"));
    }
    let text = body.text.as_deref().filter(|t| !t.is_empty());
    if text.is_some_and(|t| t.len() > MAX_INPUT || t.chars().any(|c| c.is_control())) {
        return Err(fail(StatusCode::BAD_REQUEST, format!("type at most {MAX_INPUT} characters, with no control keys")));
    }
    let key = match body.key.as_deref() {
        None => None,
        Some(name) => Some(Key::parse(name).ok_or_else(|| fail(StatusCode::BAD_REQUEST, format!("no key `{name}`")))?),
    };
    if text.is_none() && key.is_none() {
        return Err(fail(StatusCode::BAD_REQUEST, "send text, a key, or both"));
    }
    job.input(text, key).map_err(|err| fail(StatusCode::CONFLICT, format!("{err:#}")))?;
    ok(json!({ "ok": true }))
}

pub async fn cancel(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let job = app.jobs.get(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no job `{id}`")))?;
    job.cancel();
    ok(json!(job.view()))
}

pub async fn stream_job(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let token = bearer(&headers).or(query.get("token").map(String::as_str));
    if token.and_then(|token| app.auth.identify(token)).is_none() {
        return Err(fail(StatusCode::UNAUTHORIZED, "a token is required"));
    }
    let job = app.jobs.get(&id).ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no job `{id}`")))?;
    let changed = job.subscribe();
    // The state now, then again after each change, until the job has finished.
    let events = stream::unfold((job, changed, true, false), |(job, mut changed, first, over)| async move {
        if over {
            return None;
        }
        if !first {
            // At most ten frames a second: a progress bar redraws far more often.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = tokio::time::timeout(Duration::from_secs(15), changed.changed()).await;
        }
        let view = job.view();
        let finished = view.state != "running";
        let event = SseEvent::default().event("job").data(serde_json::to_string(&view).unwrap_or_default());
        Some((Ok(event), (job, changed, false, finished)))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}
