//! The skills store, shared by every harness.
//!
//! ```text
//! GET  /api/skills                     the store's skills, then those found in the project and home
//! GET  /api/skills/search?q=           skills.sh results, marked when installed
//! GET  /api/skills/updates             store skills whose folder changed upstream
//! POST /api/skills/install             {repo: "owner/name", skill} from GitHub
//! POST /api/skills/{name}/enable       {enabled}
//! POST /api/skills/{name}/update
//! POST /api/skills/{name}/remove
//! POST /api/skills/{name}/import       copy a project or home skill into the store
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::{App, Reply, fail, ok};
use crate::skills::{Store, valid_name};

fn store(app: &App) -> Store {
    Store::new(app.skills_root.clone(), app.skills_fetch.clone())
}

/// Run blocking store work (files and the network) off the async threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, super::ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(fail(StatusCode::BAD_GATEWAY, format!("{err:#}"))),
        Err(err) => Err(fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())),
    }
}

fn named(name: &str) -> Result<(), super::ApiError> {
    if valid_name(name) { Ok(()) } else { Err(fail(StatusCode::BAD_REQUEST, format!("`{name}` is not a skill name"))) }
}

pub async fn list(State(app): State<Arc<App>>) -> Reply {
    let (store, project) = (store(&app), app.project());
    let skills = blocking(move || Ok(store.list(&project))).await?;
    ok(json!({ "skills": skills, "root": app.skills_root.display().to_string() }))
}

pub async fn search(State(app): State<Arc<App>>, Query(query): Query<HashMap<String, String>>) -> Reply {
    let q = query.get("q").cloned().unwrap_or_default();
    if q.trim().len() < 2 {
        return Err(fail(StatusCode::BAD_REQUEST, "type at least two characters to search"));
    }
    let store = store(&app);
    let hits = blocking(move || store.search(&q)).await?;
    ok(json!({ "hits": hits }))
}

pub async fn updates(State(app): State<Arc<App>>) -> Reply {
    let store = store(&app);
    let updates = blocking(move || store.updates()).await?;
    ok(json!({ "updates": updates }))
}

#[derive(Deserialize)]
pub struct InstallBody {
    repo: String,
    skill: String,
}

pub async fn install(State(app): State<Arc<App>>, Json(body): Json<InstallBody>) -> Reply {
    named(&body.skill)?;
    let store = store(&app);
    let skill = blocking(move || store.install(&body.repo, &body.skill)).await?;
    ok(json!(skill))
}

#[derive(Deserialize)]
pub struct EnableBody {
    enabled: bool,
}

pub async fn enable(State(app): State<Arc<App>>, UrlPath(name): UrlPath<String>, Json(body): Json<EnableBody>) -> Reply {
    named(&name)?;
    let store = store(&app);
    let skill = store.set_enabled(&name, body.enabled).map_err(|err| fail(StatusCode::NOT_FOUND, format!("{err:#}")))?;
    ok(json!(skill))
}

pub async fn update(State(app): State<Arc<App>>, UrlPath(name): UrlPath<String>) -> Reply {
    named(&name)?;
    let store = store(&app);
    let skill = blocking(move || store.update(&name)).await?;
    ok(json!(skill))
}

pub async fn remove(State(app): State<Arc<App>>, UrlPath(name): UrlPath<String>) -> Reply {
    named(&name)?;
    store(&app).remove(&name).map_err(|err| fail(StatusCode::NOT_FOUND, format!("{err:#}")))?;
    ok(json!({ "removed": name }))
}

pub async fn import(State(app): State<Arc<App>>, UrlPath(name): UrlPath<String>) -> Reply {
    named(&name)?;
    let (store, project) = (store(&app), app.project());
    let skill = store.import(&project, &name).map_err(|err| fail(StatusCode::NOT_FOUND, format!("{err:#}")))?;
    ok(json!(skill))
}
