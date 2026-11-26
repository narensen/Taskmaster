//! Production operations & workflow endpoints (Phases 2–5):
//! Prometheus metrics, monthly spend alerts, egress audit, retention GC, remote runner
//! mesh, environments, user MCP servers, notifications, scheduled runs, issue intake,
//! run attachments, PR CI/review sync, mid-flight Contract RFCs, stratified scorecard,
//! and eval suite inspection.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tm_core::Event;

use super::{App, Reply, fail, ok};

/// `GET /metrics` — Prometheus text format observability endpoint (Item 3.8).
pub(super) async fn prometheus_metrics(State(app): State<Arc<App>>) -> Response {
    let records = app.records();
    let active_runs = app.active.lock().unwrap().len();
    let body = crate::metrics::render_prometheus(&records, active_runs);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}

/// `GET /api/metrics/spend?threshold=<usd>` — monthly project spend with alert check (Item 3.8).
pub(super) async fn monthly_spend(
    State(app): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    let threshold = query
        .get("threshold")
        .and_then(|v| v.parse::<f64>().ok())
        .or_else(|| crate::policy::Policy::load(&app.project()).daily_budget_usd.map(|d| d * 30.0));
    let report = crate::metrics::monthly_spend(&app.records(), threshold);
    ok(json!({ "spend": report }))
}

/// `GET /api/egress?limit=100` — recent daemon-side network egress audit entries (Item 3.2).
pub(super) async fn egress_log(
    State(app): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100)
        .min(500);
    let entries = crate::egress::recent(&app.project(), limit);
    ok(json!({ "entries": entries }))
}

#[derive(Deserialize, Default)]
pub(super) struct GcBody {
    max_runs: Option<usize>,
    max_age_days: Option<u64>,
    prune_worktrees: Option<bool>,
}

/// `POST /api/gc` — run retention garbage collection on finished runs & stale worktrees (Item 3.6).
pub(super) async fn run_gc(
    State(app): State<Arc<App>>,
    body: Option<Json<GcBody>>,
) -> Reply {
    let req = body.map(|b| b.0).unwrap_or_default();
    let defaults = crate::retention::RetentionPolicy::default();
    let policy = crate::retention::RetentionPolicy {
        max_runs: req.max_runs.unwrap_or(defaults.max_runs),
        max_age_days: req.max_age_days.unwrap_or(defaults.max_age_days),
        prune_worktrees: req.prune_worktrees.unwrap_or(defaults.prune_worktrees),
    };
    let active: BTreeSet<String> = app.active.lock().unwrap().keys().cloned().collect();
    let project = app.project();
    let report = crate::retention::run_gc(&project, &policy, &active)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({ "gc": report }))
}

// --- Remote Runner Mesh (Item 3.13) & Environments (Item 4.8) ---

#[derive(Deserialize)]
pub(super) struct NodeBody {
    #[serde(default)]
    id: String,
    name: String,
    url: String,
    #[serde(default)]
    labels: Vec<String>,
    max_parallel: Option<usize>,
    enabled: Option<bool>,
    token: Option<String>,
}

pub(super) async fn list_runners(State(app): State<Arc<App>>) -> Reply {
    let nodes = crate::mesh::list_nodes(&app.project(), "runners.json");
    ok(json!({ "runners": nodes }))
}

pub(super) async fn upsert_runner(State(app): State<Arc<App>>, Json(body): Json<NodeBody>) -> Reply {
    let vault = crate::mesh::default_vault();
    let node = crate::mesh::RemoteNode {
        id: body.id,
        name: body.name,
        url: body.url,
        kind: "runner".into(),
        labels: body.labels,
        max_parallel: body.max_parallel.unwrap_or(4),
        enabled: body.enabled.unwrap_or(true),
        has_token: false,
    };
    let saved = crate::mesh::upsert_node(&app.project(), "runners.json", &vault, node, body.token.as_deref())
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "runner": saved }))
}

pub(super) async fn remove_runner(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let vault = crate::mesh::default_vault();
    let removed = crate::mesh::remove_node(&app.project(), "runners.json", &vault, &id)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({ "removed": removed }))
}

pub(super) async fn probe_runner(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let nodes = crate::mesh::list_nodes(&app.project(), "runners.json");
    let Some(node) = nodes.into_iter().find(|n| n.id == id) else {
        return Err(fail(StatusCode::NOT_FOUND, format!("runner `{id}` not found")));
    };
    let health = crate::mesh::probe_node(&node).await;
    ok(json!({ "health": health }))
}

pub(super) async fn list_environments(State(app): State<Arc<App>>) -> Reply {
    let envs = crate::mesh::list_nodes(&app.project(), "environments.json");
    ok(json!({ "environments": envs }))
}

pub(super) async fn upsert_environment(State(app): State<Arc<App>>, Json(body): Json<NodeBody>) -> Reply {
    let vault = crate::mesh::default_vault();
    let node = crate::mesh::RemoteNode {
        id: body.id,
        name: body.name,
        url: body.url,
        kind: "environment".into(),
        labels: body.labels,
        max_parallel: body.max_parallel.unwrap_or(4),
        enabled: body.enabled.unwrap_or(true),
        has_token: false,
    };
    let saved = crate::mesh::upsert_node(&app.project(), "environments.json", &vault, node, body.token.as_deref())
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "environment": saved }))
}

pub(super) async fn remove_environment(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let vault = crate::mesh::default_vault();
    let removed = crate::mesh::remove_node(&app.project(), "environments.json", &vault, &id)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({ "removed": removed }))
}

// --- User MCP servers (Item 4.4) ---

#[derive(Deserialize)]
pub(super) struct McpBody {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    secret_env: BTreeMap<String, String>,
    #[serde(default)]
    roles: Vec<String>,
    enabled: Option<bool>,
    require_approval: Option<bool>,
}

pub(super) async fn list_mcp(State(app): State<Arc<App>>) -> Reply {
    ok(json!({ "servers": crate::mcp_user::list(&app.project()) }))
}

pub(super) async fn upsert_mcp(State(app): State<Arc<App>>, Json(body): Json<McpBody>) -> Reply {
    let vault = crate::mesh::default_vault();
    let srv = crate::mcp_user::UserMcpServer {
        name: body.name,
        command: body.command,
        args: body.args,
        env: body.env,
        secret_env_keys: vec![],
        roles: body.roles,
        enabled: body.enabled.unwrap_or(true),
        require_approval: body.require_approval.unwrap_or(true),
    };
    let saved = crate::mcp_user::upsert(&app.project(), &vault, srv, &body.secret_env)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "server": saved }))
}

pub(super) async fn remove_mcp(State(app): State<Arc<App>>, UrlPath(name): UrlPath<String>) -> Reply {
    let vault = crate::mesh::default_vault();
    let removed = crate::mcp_user::remove(&app.project(), &vault, &name)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({ "removed": removed }))
}

// --- Notifications (Item 4.5) ---

pub(super) async fn get_notifications(State(app): State<Arc<App>>) -> Reply {
    ok(json!({ "notifications": crate::notify::load(&app.project()) }))
}

pub(super) async fn update_notifications(
    State(app): State<Arc<App>>,
    Json(cfg): Json<crate::notify::NotificationConfig>,
) -> Reply {
    crate::notify::save(&app.project(), &cfg)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "notifications": cfg }))
}

pub(super) async fn test_notification(State(app): State<Arc<App>>) -> Reply {
    let cfg = crate::notify::load(&app.project());
    let sent = crate::notify::dispatch(
        &cfg,
        crate::notify::NotifyKind::RunFinished,
        "test",
        "Taskmaster test notification",
        "Notifications are connected.",
    )
    .await
    .map_err(|err| fail(StatusCode::BAD_GATEWAY, format!("{err:#}")))?;
    ok(json!({ "sent": sent }))
}

// --- Scheduled runs (Item 4.6) ---

#[derive(Deserialize)]
pub(super) struct ScheduleBody {
    #[serde(default)]
    id: String,
    name: String,
    schedule: String,
    prompt: String,
    enabled: Option<bool>,
    auto_approve: Option<bool>,
    max_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
pub(super) struct ToggleBody {
    enabled: bool,
}

pub(super) async fn list_schedules(State(app): State<Arc<App>>) -> Reply {
    ok(json!({ "schedules": crate::scheduler::list(&app.project()) }))
}

pub(super) async fn upsert_schedule(State(app): State<Arc<App>>, Json(body): Json<ScheduleBody>) -> Reply {
    let item = crate::scheduler::ScheduledRun {
        id: body.id,
        name: body.name,
        schedule: body.schedule,
        prompt: body.prompt,
        enabled: body.enabled.unwrap_or(true),
        auto_approve: body.auto_approve.unwrap_or(false),
        max_cost_usd: body.max_cost_usd,
        last_run_at: None,
        last_run_id: None,
    };
    let saved = crate::scheduler::upsert(&app.project(), item)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "schedule": saved }))
}

pub(super) async fn toggle_schedule(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ToggleBody>,
) -> Reply {
    let updated = crate::scheduler::toggle(&app.project(), &id, body.enabled)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let Some(schedule) = updated else {
        return Err(fail(StatusCode::NOT_FOUND, format!("schedule `{id}` not found")));
    };
    ok(json!({ "schedule": schedule }))
}

pub(super) async fn remove_schedule(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    let removed = crate::scheduler::remove(&app.project(), &id)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({ "removed": removed }))
}

// --- Issue intake (Item 4.2) ---

#[derive(Deserialize)]
pub(super) struct IssueParseBody {
    url: String,
    #[serde(default)]
    fetch: bool,
}

pub(super) async fn parse_issue(Json(body): Json<IssueParseBody>) -> Reply {
    let parsed = crate::issues::parse_issue_url(&body.url)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    if body.fetch {
        let details = crate::issues::fetch_issue(&body.url)
            .await
            .map_err(|err| fail(StatusCode::BAD_GATEWAY, format!("{err:#}")))?;
        let prompt = crate::issues::issue_to_prompt(&details);
        let closing = crate::issues::closing_link(&details.issue);
        return ok(json!({
            "issue": details,
            "prompt": prompt,
            "closing_link": closing,
        }));
    }
    let closing = crate::issues::closing_link(&parsed);
    ok(json!({ "issue": parsed, "closing_link": closing }))
}

// --- Attachments (Item 4.3) ---

#[derive(Deserialize)]
pub(super) struct AttachmentBody {
    name: String,
    #[serde(default)]
    mime: String,
    data_base64: String,
}

pub(super) async fn list_attachments(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Reply {
    ok(json!({ "attachments": crate::attachments::list(&app.project(), &id) }))
}

pub(super) async fn add_attachment(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<AttachmentBody>,
) -> Reply {
    let bytes = crate::attachments::decode_base64(&body.data_base64)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    let saved = crate::attachments::save(&app.project(), &id, &body.name, &body.mime, &bytes)
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(json!({ "attachment": saved }))
}

// --- PR CI loop & review comment sync (Item 4.1) ---

#[derive(Deserialize, Default)]
pub(super) struct PrSyncBody {
    pr: Option<String>,
    #[serde(default)]
    checks: Vec<crate::pr_loop::CiCheck>,
    #[serde(default)]
    comments: Vec<crate::pr_loop::PrReviewComment>,
}

pub(super) async fn pr_status(
    State(app): State<Arc<App>>,
    UrlPath(_id): UrlPath<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    let project = app.project();
    let pr = query.get("pr").cloned();
    let checks = tokio::task::spawn_blocking(move || crate::pr_loop::fetch_gh_checks(&project, pr.as_deref()))
        .await
        .unwrap_or_default();
    ok(json!({ "checks": checks }))
}

pub(super) async fn pr_sync(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<PrSyncBody>>,
) -> Reply {
    let req = body.map(|b| b.0).unwrap_or_default();
    let records = app.records();
    let events = records
        .events(&id)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let plan = events
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            Event::PlanReady(p) => Some(p),
            _ => None,
        })
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("run `{id}` has no plan")))?;

    let checks = if req.checks.is_empty() {
        let project = app.project();
        let pr = req.pr.clone();
        tokio::task::spawn_blocking(move || crate::pr_loop::fetch_gh_checks(&project, pr.as_deref()))
            .await
            .unwrap_or_default()
    } else {
        req.checks
    };

    let queued = crate::pr_loop::attribute_pr_feedback(&plan, &checks, &req.comments);
    for note in &queued {
        let _ = app.live.add_note(&id, &note.target, &note.text);
    }
    let failing_checks = checks
        .iter()
        .filter(|c| matches!(c.state.to_ascii_lowercase().as_str(), "fail" | "failed" | "failure" | "error"))
        .count();
    let report = crate::pr_loop::PrSyncReport {
        checks,
        failing_checks,
        comments_synced: req.comments.len(),
        queued_notes: queued,
    };
    ok(json!({ "sync": report }))
}

// --- Dynamic Mid-Flight Contract RFC evaluation (Item 2.12) ---

#[derive(Deserialize)]
pub(super) struct ContractProposeBody {
    path: String,
    proposed_content: String,
}

pub(super) async fn propose_contract(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ContractProposeBody>,
) -> Reply {
    let records = app.records();
    let events = records
        .events(&id)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let plan = events
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            Event::PlanReady(p) => Some(p),
            _ => None,
        })
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, format!("run `{id}` has no plan")))?;

    let old = plan
        .contracts
        .iter()
        .find(|c| c.path == body.path)
        .map(|c| c.content.clone())
        .unwrap_or_default();
    let mut imports = BTreeMap::new();
    for p in &plan.pages {
        let mut refs = p.cites.clone();
        refs.push(p.brief.clone());
        imports.insert(p.id.clone(), refs);
    }
    let eval = crate::gates::evaluate_contract_proposal(&body.path, &old, &body.proposed_content, &imports);
    ok(json!({ "proposal": eval }))
}

// --- Stratified Scorecard & Auto-Routing (Item 5.3) ---

pub(super) async fn stratified_scorecard(
    State(app): State<Arc<App>>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    let records = app.records();
    let slices = records
        .stratified_scores(crate::store::SCORECARD_RUNS)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let diff = query.get("difficulty").map(String::as_str);
    let lang = query.get("language").map(String::as_str);
    let recommended = records
        .auto_route(crate::store::SCORECARD_RUNS, diff, lang)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({
        "slices": slices,
        "auto_route": recommended,
    }))
}

// --- Eval suite listing (Item 5.1) ---

pub(super) async fn eval_suite(State(app): State<Arc<App>>) -> Reply {
    let suite_dir = app.project().join("bench/suite");
    let tasks: Vec<Value> = if suite_dir.is_dir() {
        crate::eval_suite::load_suite(&suite_dir)
            .unwrap_or_default()
            .into_iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "category": t.category,
                    "language": t.language,
                    "difficulty": t.difficulty,
                    "prompt": t.prompt,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    ok(json!({ "tasks": tasks }))
}

// --- Phase 1: Web Access settings, cache management, and per-run web activity ---

#[derive(Deserialize, Default)]
pub(super) struct WebAccessUpdateBody {
    enabled: Option<bool>,
    provider: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    clear_api_key: Option<bool>,
    roles: Option<crate::web_access::WebRoleMatrix>,
    allowlist: Option<Vec<String>>,
    planner_max_searches: Option<usize>,
    planner_max_fetches: Option<usize>,
    worker_max_fetches: Option<usize>,
    native_search_max_planner: Option<usize>,
    native_search_max_worker: Option<usize>,
    max_fetch_bytes: Option<usize>,
    cache_ttl_secs: Option<u64>,
    cache_max_mb: Option<u64>,
}

fn web_access_payload(project: &std::path::Path) -> Value {
    use crate::secrets::SecretVault;
    let loaded = crate::settings::Settings::load(project).unwrap_or_default();
    let web = loaded.web_access;
    let provider = web.provider_norm();
    let vault = crate::web_access::default_vault();
    let has_api_key = crate::web_access::load_api_key(&vault, &provider).is_some();
    let providers_with_keys: Vec<&str> = crate::web_access::WebAccessSettings::PROVIDERS
        .iter()
        .copied()
        .filter(|p| crate::web_access::load_api_key(&vault, p).is_some())
        .collect();
    let cache = crate::web_access::WebCache::new(
        crate::web_access::WebCache::default_dir(),
        web.cache_ttl_secs,
        web.cache_max_mb.saturating_mul(1024 * 1024),
    );
    let derived: Vec<String> = crate::web_access::derive_project_allowlist(project, &web.allowlist)
        .into_iter()
        .collect();
    json!({
        "web_access": web,
        "has_api_key": has_api_key,
        "providers_with_keys": providers_with_keys,
        "vault_backend": vault.backend_name(),
        "derived_allowlist": derived,
        "advisory_hosts": crate::web_access::ADVISORY_HOSTS,
        "providers": crate::web_access::WebAccessSettings::PROVIDERS,
        "cache": cache.stats(),
    })
}

/// `GET /api/web-access` — active Phase 1 web access settings, vault status, and cache stats.
pub(super) async fn get_web_access(State(app): State<Arc<App>>) -> Reply {
    ok(web_access_payload(&app.project()))
}

/// `POST /api/web-access` — update Phase 1 web access settings; API keys go only to the OS keychain.
pub(super) async fn update_web_access(
    State(app): State<Arc<App>>,
    Json(body): Json<WebAccessUpdateBody>,
) -> Reply {
    let project = app.project();
    let mut web = crate::settings::Settings::load(&project).unwrap_or_default().web_access;
    if let Some(enabled) = body.enabled {
        web.enabled = enabled;
    }
    if let Some(provider) = body.provider {
        let p = provider.trim().to_ascii_lowercase();
        if !crate::web_access::WebAccessSettings::PROVIDERS.contains(&p.as_str()) {
            return Err(fail(
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown web provider `{p}`: use one of {}",
                    crate::web_access::WebAccessSettings::PROVIDERS.join(", ")
                ),
            ));
        }
        web.provider = p;
    }
    if let Some(endpoint) = body.endpoint {
        web.endpoint = endpoint.trim().to_string();
    }
    if let Some(roles) = body.roles {
        web.roles = roles;
    }
    if let Some(allowlist) = body.allowlist {
        web.allowlist = allowlist
            .into_iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
    }
    if let Some(n) = body.planner_max_searches {
        web.planner_max_searches = n.min(100);
    }
    if let Some(n) = body.planner_max_fetches {
        web.planner_max_fetches = n.min(100);
    }
    if let Some(n) = body.worker_max_fetches {
        web.worker_max_fetches = n.min(100);
    }
    if let Some(n) = body.native_search_max_planner {
        web.native_search_max_planner = n.min(100);
    }
    if let Some(n) = body.native_search_max_worker {
        web.native_search_max_worker = n.min(100);
    }
    if let Some(b) = body.max_fetch_bytes {
        web.max_fetch_bytes = b.clamp(16 * 1024, 20 * 1024 * 1024);
    }
    if let Some(ttl) = body.cache_ttl_secs {
        web.cache_ttl_secs = ttl;
    }
    if let Some(mb) = body.cache_max_mb {
        web.cache_max_mb = mb.clamp(1, 4096);
    }

    let vault = crate::web_access::default_vault();
    let provider_id = web.provider_norm();
    if body.clear_api_key == Some(true) {
        crate::web_access::save_api_key(&vault, &provider_id, "")
            .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    } else if let Some(key) = body.api_key.as_deref()
        && !key.trim().is_empty()
    {
        crate::web_access::save_api_key(&vault, &provider_id, key)
            .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }

    let value = serde_json::to_value(&web)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    super::desk::set_setting(&project, "web_access", value)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;

    ok(web_access_payload(&project))
}

/// `POST /api/web-access/cache/clear` — empty the content-addressed web fetch cache.
pub(super) async fn clear_web_cache(State(app): State<Arc<App>>) -> Reply {
    let loaded = crate::settings::Settings::load(&app.project()).unwrap_or_default().web_access;
    let cache = crate::web_access::WebCache::new(
        crate::web_access::WebCache::default_dir(),
        loaded.cache_ttl_secs,
        loaded.cache_max_mb.saturating_mul(1024 * 1024),
    );
    let removed = cache
        .clear()
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    ok(json!({ "removed": removed, "cache": cache.stats() }))
}

/// `GET /api/runs/{id}/web` — each agent's web searches, fetches, and local docs lookups (Item 1.7).
pub(super) async fn run_web_activity(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let (_, entries) = app.live.read(&id, 0);
    let items = crate::web_access::summarize_run_web_activity(&entries);
    ok(json!({ "items": items }))
}

