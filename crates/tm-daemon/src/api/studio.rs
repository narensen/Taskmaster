//! Studio endpoints: Living Codebase Atlas, Preflight Doctor, Policy-as-Code,
//! Interactive Plan editing, Checkpoint Time-Travel, Run Forking, and
//! secret-redacted `.tmrun.json` Flight Recorder bundles.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Command;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tm_core::{Event, Plan, RunPhase, totals, valid_key};

use super::{App, Decision, Reply, accepted, fail, ok};
use crate::policy::{Policy, redact_json, render_policy_toml};
use crate::secret::random_hex;
use crate::time::rfc3339_now;

/// `GET /api/atlas?refresh=1` — build or return cached Living Codebase Atlas.
pub(super) async fn atlas(
    State(app): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    let refresh = query
        .get("refresh")
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let project = app.project();
    let result = tokio::task::spawn_blocking(move || crate::atlas::build_or_load(&project, refresh))
        .await
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("atlas task failed: {err}")))?;
    ok(json!({ "atlas": result }))
}

/// `GET /api/doctor` — sub-second Preflight readiness check for the open project.
pub(super) async fn doctor(State(app): State<Arc<App>>) -> Reply {
    let project = app.project();
    let report = tokio::task::spawn_blocking(move || crate::doctor::run(&project))
        .await
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("doctor task failed: {err}")))?;
    ok(json!({ "doctor": report }))
}

/// `GET /api/policy` — active Policy-as-Code rules, source path, and rendered TOML.
pub(super) async fn get_policy(State(app): State<Arc<App>>) -> Reply {
    let project = app.project();
    let policy = Policy::load(&project);
    let source = Policy::source(&project);
    let toml = render_policy_toml(&policy);
    ok(json!({
        "policy": policy,
        "source": source,
        "toml": toml,
    }))
}

#[derive(Deserialize)]
pub(super) struct SavePolicyBody {
    policy: Policy,
    #[serde(default)]
    repo_file: bool,
}

/// `POST /api/policy` — validate and save Policy-as-Code rules.
pub(super) async fn save_policy(
    State(app): State<Arc<App>>,
    Json(body): Json<SavePolicyBody>,
) -> Reply {
    if let Some(cap) = body.policy.max_run_cost_usd
        && (!cap.is_finite() || cap <= 0.0)
    {
        return Err(fail(StatusCode::BAD_REQUEST, "max_run_cost_usd must be a positive number"));
    }
    if let Some(cap) = body.policy.daily_budget_usd
        && (!cap.is_finite() || cap <= 0.0)
    {
        return Err(fail(StatusCode::BAD_REQUEST, "daily_budget_usd must be a positive number"));
    }
    let project = app.project();
    let source = body
        .policy
        .save(&project, body.repo_file)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    let toml = render_policy_toml(&body.policy);
    ok(json!({
        "ok": true,
        "policy": body.policy,
        "source": source,
        "toml": toml,
    }))
}

#[derive(Deserialize)]
pub(super) struct UpdatePlanBody {
    plan: Plan,
}

/// Validate a human-edited plan before saving it during `awaiting_approval`.
fn validate_edited_plan(plan: &Plan) -> Result<(), String> {
    if plan.pages.is_empty() {
        return Err("plan must contain at least one unit".into());
    }
    let mut ids = HashSet::new();
    for page in &plan.pages {
        if !valid_key(&page.id) {
            return Err(format!("invalid unit id `{}`", page.id));
        }
        if !ids.insert(page.id.as_str()) {
            return Err(format!("duplicate unit id `{}`", page.id));
        }
        if page.brief.trim().is_empty() {
            return Err(format!("unit `{}` has an empty brief", page.id));
        }
        let glob = page.owned_glob.trim();
        if glob.is_empty() || glob.starts_with('/') || glob.contains("..") {
            return Err(format!("unit `{}` has an invalid owned_glob `{}`", page.id, page.owned_glob));
        }
    }
    // Verify dependency references and acyclicity.
    let deps_map: HashMap<&str, &[String]> =
        plan.pages.iter().map(|p| (p.id.as_str(), p.depends_on.as_slice())).collect();
    for page in &plan.pages {
        for dep in &page.depends_on {
            if dep == &page.id {
                return Err(format!("unit `{}` cannot depend on itself", page.id));
            }
            if !ids.contains(dep.as_str()) {
                return Err(format!("unit `{}` depends on unknown unit `{dep}`", page.id));
            }
        }
    }
    fn has_cycle<'a>(
        node: &'a str,
        deps: &HashMap<&'a str, &'a [String]>,
        state: &mut HashMap<&'a str, u8>,
    ) -> bool {
        match state.get(node).copied().unwrap_or(0) {
            1 => return true,
            2 => return false,
            _ => {}
        }
        state.insert(node, 1);
        if let Some(list) = deps.get(node) {
            for dep in *list {
                if has_cycle(dep.as_str(), deps, state) {
                    return true;
                }
            }
        }
        state.insert(node, 2);
        false
    }
    let mut state = HashMap::new();
    for page in &plan.pages {
        if has_cycle(page.id.as_str(), &deps_map, &mut state) {
            return Err(format!("dependency cycle involving unit `{}`", page.id));
        }
    }
    let mut contract_paths = HashSet::new();
    for contract in &plan.contracts {
        let p = contract.path.trim();
        if p.is_empty() || p.starts_with('/') || p.contains("..") {
            return Err(format!("invalid contract path `{}`", contract.path));
        }
        if !contract_paths.insert(p) {
            return Err(format!("duplicate contract path `{}`", contract.path));
        }
    }
    Ok(())
}

/// `POST /api/runs/{id}/plan` — edit the plan interactively while awaiting approval.
pub(super) async fn update_plan(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<UpdatePlanBody>,
) -> Reply {
    let run = app.known(&id)?;
    if app.is_active(&id) {
        return Err(fail(StatusCode::CONFLICT, "cannot edit a plan while the run is active"));
    }
    if run.phase != RunPhase::AwaitingApproval {
        return Err(fail(
            StatusCode::CONFLICT,
            "plan can only be edited while the run is awaiting approval",
        ));
    }
    validate_edited_plan(&body.plan).map_err(|msg| fail(StatusCode::BAD_REQUEST, msg))?;

    let records = app.records();
    records.save_plan(&id, &body.plan)?;
    let mut events = records.events(&id)?;
    events.push(Event::PlanReady(body.plan.clone()));
    records.save_events(&id, &events)?;
    app.live.notify_events(&id);
    if let Some(mut req) = records.request(&id)?
        && req.plan.is_some()
    {
        req.plan = Some(body.plan.clone());
        records.save_request(&id, &req)?;
    }
    app.summary_cache.lock().unwrap().remove(&(app.project(), id.clone()));

    let updated = app.known(&id)?;
    let mut view = app.view(&id, &updated)?;
    let request = app.request_for(&id)?;
    view["plan"] = json!(updated.plan);
    view["prompt"] = json!(request.prompt);
    view["roles"] = json!(request.roles);
    view["models"] = json!(request.models);
    ok(view)
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Checkpoint {
    pub sha: String,
    pub short_sha: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

fn collect_checkpoints(app: &App, id: &str) -> Vec<Checkpoint> {
    let project = app.project();
    let records = app.records();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Checkpoint> = Vec::new();

    // 1. Committed changes recorded in changes.jsonl (richest metadata: page + model).
    if let Ok(changes) = records.changes(id) {
        for entry in changes {
            let Some(commit) = entry.commit.filter(|c| !c.is_empty()) else {
                continue;
            };
            if !seen.insert(commit.clone()) {
                continue;
            }
            let short_sha = commit.chars().take(8).collect();
            let label = if entry.summary.trim().is_empty() {
                match &entry.page {
                    Some(page) => format!("Unit `{page}` committed"),
                    None => "Change committed".into(),
                }
            } else {
                entry.summary.clone()
            };
            out.push(Checkpoint {
                sha: commit,
                short_sha,
                kind: "unit".into(),
                label,
                ref_name: None,
                page: entry.page,
                model: entry.model,
                at: if entry.at.is_empty() { None } else { Some(entry.at) },
            });
        }
    }

    // 2. Git checkpoint refs under `refs/taskmaster/{id}/*`.
    let prefix = format!("refs/taskmaster/{id}/");
    if let Ok(output) = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(objectname)\t%(refname)\t%(creatordate:iso-strict)\t%(subject)",
            &prefix,
        ])
        .current_dir(&project)
        .output()
        && output.status.success()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let sha = parts[0].trim().to_string();
            let ref_name = parts[1].trim().to_string();
            let at = parts.get(2).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let subject = parts.get(3).map(|s| s.trim()).unwrap_or("");
            if sha.is_empty() {
                continue;
            }
            let tail = ref_name.strip_prefix(&prefix).unwrap_or(&ref_name);
            let short_sha: String = sha.chars().take(8).collect();
            if seen.insert(sha.clone()) {
                out.push(Checkpoint {
                    sha,
                    short_sha,
                    kind: "checkpoint".into(),
                    label: if subject.is_empty() { tail.to_string() } else { subject.to_string() },
                    ref_name: Some(ref_name),
                    page: None,
                    model: None,
                    at,
                });
            }
        }
    }

    // 3. Branch commits between base and branch (if available in meta/status).
    let meta = records.meta(id).ok().flatten();
    let status = records.status(id).ok().flatten();
    let base = meta
        .as_ref()
        .and_then(|m| m.base.clone())
        .or_else(|| status.as_ref().and_then(|s| s.get("base")?.as_str().map(String::from)));
    let branch = meta
        .as_ref()
        .and_then(|m| m.branch.clone())
        .or_else(|| status.as_ref().and_then(|s| s.get("branch")?.as_str().map(String::from)));

    if let (Some(base_sha), Some(branch_name)) = (base, branch) {
        let range = format!("{base_sha}..{branch_name}");
        if let Ok(output) = Command::new("git")
            .args(["log", "--reverse", "--format=%H\t%cI\t%s", &range])
            .current_dir(&project)
            .output()
            && output.status.success()
        {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.is_empty() {
                    continue;
                }
                let sha = parts[0].trim().to_string();
                if sha.is_empty() || !seen.insert(sha.clone()) {
                    continue;
                }
                let at = parts.get(1).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
                let subject = parts.get(2).map(|s| s.trim()).unwrap_or("Commit");
                let short_sha = sha.chars().take(8).collect();
                out.push(Checkpoint {
                    sha,
                    short_sha,
                    kind: "commit".into(),
                    label: subject.to_string(),
                    ref_name: Some(branch_name.clone()),
                    page: None,
                    model: None,
                    at,
                });
            }
        }
    }

    out
}

/// `GET /api/runs/{id}/checkpoints` — list git checkpoints & committed milestones for time-travel.
pub(super) async fn checkpoints(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    app.known(&id)?;
    let list = collect_checkpoints(&app, &id);
    ok(json!({ "checkpoints": list }))
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct ForkRunBody {
    pub id: Option<String>,
    pub prompt: Option<String>,
    pub plan: Option<Plan>,
    pub yes: Option<bool>,
    pub parallel: Option<usize>,
    pub max_cost_usd: Option<f64>,
    pub note: Option<String>,
}

/// `POST /api/runs/{id}/fork` — fork a run into a fresh run id with optional plan/prompt/budget overrides.
pub(super) async fn fork_run(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ForkRunBody>,
) -> Reply {
    let source_run = app.known(&id)?;
    let mut req = app.request_for(&id)?;

    let new_id = match body.id {
        Some(custom) => custom,
        None => format!("run-{}", random_hex(4)?),
    };
    if !valid_key(&new_id) {
        return Err(fail(StatusCode::BAD_REQUEST, "a run id uses letters, digits, `_` and `-`"));
    }
    if app.replay(&new_id)?.is_some() {
        return Err(fail(StatusCode::CONFLICT, format!("run `{new_id}` already exists")));
    }

    if let Some(plan) = body.plan {
        validate_edited_plan(&plan).map_err(|msg| fail(StatusCode::BAD_REQUEST, msg))?;
        req.plan = Some(plan);
        req.prompt = None;
    } else if let Some(prompt) = body.prompt.filter(|p| !p.trim().is_empty()) {
        req.prompt = Some(prompt);
        req.plan = None;
    } else if req.prompt.is_none() && req.plan.is_none() {
        if let Some(plan) = source_run.plan {
            req.plan = Some(plan);
        } else {
            return Err(fail(StatusCode::BAD_REQUEST, "source run has neither a prompt nor a plan to fork"));
        }
    }

    if let Some(parallel) = body.parallel {
        req.parallel = Some(parallel.max(1));
    }
    if let Some(cap) = body.max_cost_usd {
        if !cap.is_finite() || cap <= 0.0 {
            return Err(fail(StatusCode::BAD_REQUEST, "max_cost_usd must be positive"));
        }
        req.max_cost_usd = Some(cap);
    }

    let decision = if body.yes.unwrap_or(false) { Decision::Approve } else { Decision::Continue };
    app.start(&new_id, req.clone(), decision)?;
    app.records().save_request(&new_id, &req)?;
    if let Some(note) = body.note.filter(|n| !n.trim().is_empty()) {
        let _ = app.live.add_note(&new_id, "all", &note);
    }
    accepted(json!({ "id": new_id, "forked_from": id }))
}

/// `GET /api/runs/{id}/bundle` — secret-redacted portable `.tmrun.json` Flight Recorder bundle.
pub(super) async fn bundle(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let run = app.known(&id)?;
    let records = app.records();
    let view = app.view(&id, &run)?;
    let events = records.events(&id)?;
    let ledger = records.ledger(&id)?;
    let changes = records.changes(&id)?;
    let transcript = records.transcript(&id)?;
    let checkpoints = collect_checkpoints(&app, &id);

    // Compute cache economics & per-role breakdown.
    let spent = totals(&ledger);
    let total_input = spent.input + spent.cache_read + spent.cache_write;
    let cache_hit_rate = if total_input > 0 {
        (spent.cache_read as f64) / (total_input as f64)
    } else {
        0.0
    };
    let mut by_role: BTreeMap<String, Value> = BTreeMap::new();
    for entry in &ledger {
        let slot = by_role.entry(entry.role.name().to_string()).or_insert_with(|| {
            json!({
                "calls": 0u64,
                "cost_usd": 0.0f64,
                "input": 0u64,
                "cache_read": 0u64,
                "cache_write": 0u64,
                "output": 0u64,
                "seconds": 0.0f64,
            })
        });
        if let Some(obj) = slot.as_object_mut() {
            obj["calls"] = json!(obj["calls"].as_u64().unwrap_or(0) + 1);
            obj["cost_usd"] = json!(obj["cost_usd"].as_f64().unwrap_or(0.0) + entry.cost_usd.unwrap_or(0.0));
            obj["input"] = json!(obj["input"].as_u64().unwrap_or(0) + entry.input);
            obj["cache_read"] = json!(obj["cache_read"].as_u64().unwrap_or(0) + entry.cache_read);
            obj["cache_write"] = json!(obj["cache_write"].as_u64().unwrap_or(0) + entry.cache_write);
            obj["output"] = json!(obj["output"].as_u64().unwrap_or(0) + entry.output);
            obj["seconds"] = json!(obj["seconds"].as_f64().unwrap_or(0.0) + entry.seconds);
        }
    }

    let mut raw = json!({
        "format": "tmrun/v1",
        "exported_at": rfc3339_now(),
        "run": view,
        "plan": run.plan,
        "events": events,
        "ledger": ledger,
        "changes": changes,
        "transcript": transcript,
        "checkpoints": checkpoints,
        "cache_economics": {
            "total_input_tokens": total_input,
            "fresh_input_tokens": spent.input,
            "cached_read_tokens": spent.cache_read,
            "cache_write_tokens": spent.cache_write,
            "output_tokens": spent.output,
            "cache_hit_rate": cache_hit_rate,
            "total_cost_usd": spent.cost_usd,
            "by_role": by_role,
        }
    });

    let policy = Policy::load(&app.project());
    redact_json(&mut raw, &policy.redact_patterns);
    ok(raw)
}
