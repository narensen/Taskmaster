//! O5: race one request across providers, compare the runs, merge the one picked.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tm_core::RunPhase;

use super::{App, Decision, Reply, accepted, fail, mode_of, ok};
use crate::race::{Entrant, Race};
use crate::{Models, Roles, RunRequest};

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct NewRace {
    prompt: String,
    /// Two or three harness ids; each runs the whole request on its own.
    providers: Vec<String>,
    /// Harness id → model; a provider with none uses its default.
    models: std::collections::BTreeMap<String, String>,
    mode: Option<String>,
    yes: bool,
    parallel: Option<usize>,
    timeout_secs: Option<u64>,
    template: Option<String>,
    max_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
pub struct Pick {
    run: String,
}

/// `POST /api/races`: start the same request once per provider, each held on its branch.
pub(super) async fn create_race(State(app): State<Arc<App>>, Json(body): Json<NewRace>) -> Reply {
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "a race needs a prompt"));
    }
    let mut providers: Vec<String> = Vec::new();
    for p in &body.providers {
        let p = p.trim().to_string();
        if !tm_harness::HARNESSES.contains(&p.as_str()) {
            return Err(fail(StatusCode::BAD_REQUEST, format!("`{p}` is not a provider Taskmaster drives")));
        }
        if !providers.contains(&p) {
            providers.push(p);
        }
    }
    if !(crate::race::MIN..=crate::race::MAX).contains(&providers.len()) {
        return Err(fail(StatusCode::BAD_REQUEST, "a race takes two or three different providers"));
    }
    let project = app.project();
    if !crate::git::has_head(&project) {
        return Err(fail(StatusCode::CONFLICT, "a race needs a commit to branch from: commit the project first"));
    }
    let mode = mode_of(body.mode.as_deref())?;
    let id = format!("race-{}", crate::secret::random_hex(3).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?);
    let mut entrants = Vec::new();
    let mut requests = Vec::new();
    for provider in &providers {
        let roles = Roles {
            worker: provider.clone(),
            mid: provider.clone(),
            master: provider.clone(),
            reviewer: provider.clone(),
            verifier: provider.clone(),
        };
        let model = body.models.get(provider).map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        let models: Models = serde_json::from_value(json!({
            "worker": model, "mid": model, "master": model, "reviewer": model, "verifier": model,
        }))
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
        app.check_models_cached(&roles, &models).await.map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
        let run = crate::race::run_id(&id, provider);
        entrants.push(Entrant { run: run.clone(), provider: provider.clone(), model: model.clone() });
        requests.push((
            run,
            RunRequest {
                prompt: Some(prompt.clone()),
                plan: None,
                roles,
                models,
                parallel: body.parallel,
                timeout_secs: body.timeout_secs,
                template: body.template.clone(),
                max_cost_usd: body.max_cost_usd,
                mode,
                permission_mode: None,
                goal: None,
                hold: true,
                race: Some(id.clone()),
            },
        ));
    }
    let race = Race { id: id.clone(), prompt, created: crate::time::rfc3339_now(), runs: entrants, winner: None, picked_at: None };
    let records = app.records();
    records.save_race(&race).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    for (run, request) in requests {
        records.save_request(&run, &request).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
        let decision = if body.yes { Decision::Approve } else { Decision::Continue };
        app.start(&run, request, decision)?;
    }
    accepted(json!({ "id": id, "runs": race.runs }))
}

fn compare(app: &App, race: &Race) -> Value {
    let project = app.project();
    let records = app.records();
    let rows: Vec<Value> = race
        .runs
        .iter()
        .map(|entrant| {
            let view = app.replay(&entrant.run).ok().flatten().and_then(|run| app.view(&entrant.run, &run).ok()).unwrap_or_else(|| json!({}));
            let meta = records.meta(&entrant.run).ok().flatten();
            let branch = meta.as_ref().and_then(|m| m.branch.clone());
            let diff = crate::race::diff_size(&project, meta.as_ref().and_then(|m| m.base.as_deref()), branch.as_deref());
            crate::race::compare_row(entrant, &view, diff)
        })
        .collect();
    json!({ "race": race, "runs": rows })
}

/// `GET /api/races/{id}`: the entrants side by side.
pub(super) async fn show_race(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let race = app.records().race(&id).ok().flatten().ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no race `{id}`")))?;
    let app2 = app.clone();
    let body = tokio::task::spawn_blocking(move || compare(&app2, &race)).await.map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))?;
    ok(body)
}

/// `POST /api/races/{id}/pick`: merge the run picked; remove the others' branches and
/// worktrees, and archive them.
pub(super) async fn pick_winner(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<Pick>) -> Reply {
    let records = app.records();
    let mut race = records.race(&id).ok().flatten().ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("no race `{id}`")))?;
    if let Some(winner) = &race.winner {
        return Err(fail(StatusCode::CONFLICT, format!("this race was decided: {winner} was merged")));
    }
    if !race.runs.iter().any(|e| e.run == body.run) {
        return Err(fail(StatusCode::BAD_REQUEST, format!("`{}` is not in this race", body.run)));
    }
    if race.runs.iter().any(|e| app.is_active(&e.run)) {
        return Err(fail(StatusCode::CONFLICT, "a run of this race is still working: wait for it, or stop it"));
    }
    let winner = app.known(&body.run)?;
    if winner.phase != RunPhase::Done {
        return Err(fail(StatusCode::CONFLICT, "pick a run that finished: this one did not pass"));
    }
    let project = app.project();
    let (live, run) = (app.live.clone(), body.run.clone());
    let merged = {
        let (project, records) = (project.clone(), records.clone());
        tokio::task::spawn_blocking(move || crate::race::merge_winner(&project, &records, &live, &run))
            .await
            .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))?
    };
    let commit = merged.map_err(|err| fail(StatusCode::CONFLICT, format!("{err:#}")))?;
    for entrant in race.runs.iter().filter(|e| e.run != body.run) {
        app.previews.stop(&entrant.run);
        crate::race::clear_loser(&project, &records, &entrant.run);
    }
    race.winner = Some(body.run.clone());
    race.picked_at = Some(crate::time::rfc3339_now());
    records.save_race(&race).map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    for entrant in &race.runs {
        app.summary_cache.lock().unwrap().remove(&(project.clone(), entrant.run.clone()));
        app.live.notify_events(&entrant.run);
    }
    crate::log::note(&format!("race {id}: merged {} ({commit})", body.run));
    let app2 = app.clone();
    let body = tokio::task::spawn_blocking(move || compare(&app2, &race)).await.map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))?;
    ok(body)
}
