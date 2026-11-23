//! A9: switch the providers of a run that is not working, with a handoff to the next agent.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{App, Reply, fail, ok};
use crate::handoff::{Facts, Handoff};
use crate::{Models, Roles};

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct SwitchBody {
    /// Seat → harness id, for the seats that change.
    roles: serde_json::Map<String, Value>,
    /// Seat → model, for the seats that change; an empty string is the provider's default.
    models: serde_json::Map<String, Value>,
}

fn label(roles: &Roles, models: &Models, seat: &str) -> String {
    let harness = roles.get(seat).to_string();
    match serde_json::to_value(models).ok().and_then(|m| m[seat].as_str().map(str::to_string)) {
        Some(model) if !model.is_empty() => format!("{harness} · {model}"),
        _ => harness,
    }
}

/// `POST /api/runs/{id}/roles`: give some seats to another provider (or model). The run must
/// not be working. The next turn's first agent gets a handoff instead of a session.
pub(super) async fn switch_roles(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<SwitchBody>) -> Reply {
    let run = app.known(&id)?;
    if app.is_active(&id) {
        return Err(fail(StatusCode::CONFLICT, "the run is working: stop it, or wait for this turn to end, then switch"));
    }
    if body.roles.is_empty() && body.models.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "name a seat to switch: roles or models"));
    }
    for seat in body.roles.keys().chain(body.models.keys()) {
        if !Roles::NAMES.contains(&seat.as_str()) {
            return Err(fail(StatusCode::BAD_REQUEST, format!("`{seat}` is not a seat: use master, mid, worker, reviewer or verifier")));
        }
    }
    let bad = |err: anyhow::Error| fail(StatusCode::BAD_REQUEST, format!("{err:#}"));
    let mut request = app.request_for(&id).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let before_roles = request.roles.clone();
    let before_models = request.models.clone();
    let saved_roles = serde_json::to_value(&request.roles).ok().and_then(|v| v.as_object().cloned()).unwrap_or_default();
    let roles = Roles::resolve(&saved_roles, &body.roles).map_err(bad)?;
    for seat in body.roles.keys() {
        let name = roles.get(seat);
        if !tm_harness::HARNESSES.contains(&name) && !name.eq_ignore_ascii_case("auto") {
            return Err(fail(StatusCode::BAD_REQUEST, format!("`{name}` is not a provider Taskmaster drives")));
        }
    }
    // A seat that changes provider and names no model takes the new provider's default.
    let mut models_value = serde_json::to_value(&request.models).unwrap_or_else(|_| json!({}));
    for seat in body.roles.keys() {
        if roles.get(seat) != before_roles.get(seat) && !body.models.contains_key(seat) {
            models_value[seat] = Value::Null;
        }
    }
    for (seat, model) in &body.models {
        models_value[seat] = match model.as_str().map(str::trim) {
            Some("") | None => Value::Null,
            Some(name) => json!(name),
        };
    }
    let models: Models = serde_json::from_value(models_value).map_err(|err| bad(err.into()))?;
    app.check_models_cached(&roles, &models).await.map_err(bad)?;

    let mut from = BTreeMap::new();
    let mut to = BTreeMap::new();
    for seat in Roles::NAMES {
        let (was, now) = (label(&before_roles, &before_models, seat), label(&roles, &models, seat));
        if was != now {
            from.insert(seat.to_string(), was);
            to.insert(seat.to_string(), now);
        }
    }
    if to.is_empty() {
        return ok(json!({ "roles": roles, "models": models, "handoff": null }));
    }
    request.roles = roles.clone();
    request.models = models.clone();

    let records = app.records();
    let project = app.project();
    let events = records.events(&id).unwrap_or_default();
    let status = records.status(&id).ok().flatten().unwrap_or_default();
    let meta = records.meta(&id).ok().flatten();
    let (base, branch) = (meta.as_ref().and_then(|m| m.base.clone()), meta.as_ref().and_then(|m| m.branch.clone()));
    let diff_stat = tokio::task::spawn_blocking({
        let (project, status) = (project.clone(), status.clone());
        move || crate::handoff::diff_stat(&project, &status, base.as_deref(), branch.as_deref())
    })
    .await
    .unwrap_or_default();
    let (total, _) = app.live.read(&id, usize::MAX);
    let (_, tail) = app.live.read(&id, total.saturating_sub(300));
    let text = crate::handoff::compile(
        &Facts {
            plan: run.plan.as_ref(),
            run: &run,
            events: &events,
            diff_stat,
            error: status["error"].as_str(),
            said: crate::suggest::last_words(&tail, 5),
        },
        &from,
        &to,
    );
    let handoff = Handoff { from, to, text, at: crate::time::rfc3339_now(), used: false };
    let internal = |err: anyhow::Error| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"));
    records.save_request(&id, &request).map_err(internal)?;
    records.save_handoff(&id, &handoff).map_err(internal)?;
    // A switch is a decision in the run's history: the stream shows it as a daemon line.
    let switched: Vec<String> = handoff.to.iter().map(|(seat, now)| format!("{seat} → {now}")).collect();
    app.live.push(
        &id,
        &json!({ "id": "runtime", "role": "runtime" }),
        "switch",
        json!({ "text": format!("providers switched: {}; the next agent gets a handoff", switched.join(", ")), "ok": true, "to": handoff.to }),
    );
    app.summary_cache.lock().unwrap().remove(&(project, id.clone()));
    app.live.notify_events(&id);
    ok(json!({ "roles": roles, "models": models, "handoff": handoff }))
}
