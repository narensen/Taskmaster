//! Connections to GitHub, GitLab, Gitea (Forgejo uses the same API) and Bitbucket, and
//! the git remote of the open project.
//!
//! ```text
//! GET    /api/hosts                 every host, and who is signed in. Never a token.
//! POST   /api/hosts                 {kind, token, base?, user?} check the token, then keep it
//! DELETE /api/hosts/{id}            forget a saved token. A gh or glab sign-in is left as it is.
//! POST   /api/hosts/{id}/repos      {name, private?, push?} create the repository and, by default, push
//! GET    /api/git                   {repo, branch, detached, dirty, remotes}
//! POST   /api/git/remote            {name, url}  https without a token, or ssh, or git@
//! POST   /api/git/push              {remote?, branch?}
//! POST   /api/git/clone             {url, name?} into the workspace. The open project stays put.
//! ```
//!
//! A token is checked against the host before it is written, stored only in the home
//! directory, and sent to git as one request header. It is not written into the remote URL.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tm_core::valid_key;

use super::{App, Reply, fail, ok, workspace_child};
use crate::git;
use crate::hosts::{self, Kind, LiveFetch};

async fn blocked<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, super::ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(fail(status_for(&err), hosts::scrub(&err, ""))),
        Err(err) => Err(fail(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())),
    }
}

fn status_for(message: &str) -> StatusCode {
    let text = message.to_ascii_lowercase();
    if text.contains("not installed") || text.contains("failed to run git") || text.contains("not signed in") {
        StatusCode::FAILED_DEPENDENCY
    } else if text.contains("already") || text.contains("not a git repository") || text.contains("not on a branch") || text.contains("exists") || text.contains("has no remote") {
        StatusCode::CONFLICT
    } else if text.contains("has to")
        || text.contains("uses letters")
        || text.contains("paste the")
        || text.contains("unknown host")
        || text.contains("not a git address")
        || text.contains("not a branch")
        || text.contains("one line")
        || text.contains("needs the username")
        || text.contains("a token is")
        || text.contains("name this clone")
        || text.contains("a project name")
        || text.contains("a repository name")
        || text.contains("plain https")
        || text.contains("connect this host")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    }
}

fn state_of(project: &std::path::Path) -> Result<Value, String> {
    if !git::is_repo(project) {
        return Ok(json!({ "repo": false, "branch": null, "detached": false, "dirty": false, "remotes": [] }));
    }
    let branch = git::current_branch(project);
    let detached = branch.is_none() && git::has_head(project);
    let remotes = git::remotes(project).map_err(|err| crate::api::redact(&format!("{err:#}")))?;
    let remotes: Vec<Value> = remotes
        .into_iter()
        .map(|(name, url)| json!({ "name": name, "url": crate::api::redact(&url) }))
        .collect();
    Ok(json!({
        "repo": true,
        "branch": branch,
        "detached": detached,
        "dirty": git::dirty(project),
        "remotes": remotes,
    }))
}

fn basic_for(url: &str) -> Option<String> {
    let (user, token) = hosts::auth_for_url(&hosts::store_path(), url)?;
    Some(hosts::basic_secret(&user, &token))
}

fn name_from_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let name = trimmed.rsplit(['/', ':']).next().unwrap_or("");
    if valid_key(name) {
        Ok(name.to_string())
    } else {
        Err("name this clone with letters, digits, `_` and `-`".into())
    }
}

pub async fn list() -> Reply {
    let views = blocked(|| Ok(hosts::list_live(&hosts::store_path(), &LiveFetch))).await?;
    ok(json!({ "hosts": views.into_iter().map(|view| view.json()).collect::<Vec<_>>() }))
}

#[derive(Deserialize)]
pub(super) struct ConnectBody {
    kind: String,
    token: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    user: String,
}

pub async fn connect(Json(body): Json<ConnectBody>) -> Reply {
    let kind = Kind::parse(&body.kind).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "unknown host"))?;
    let token = body.token;
    let base = body.base;
    let user = body.user;
    let view = blocked(move || hosts::connect(&hosts::store_path(), &LiveFetch, kind, &token, &base, &user)).await?;
    ok(view.json())
}

pub async fn forget(UrlPath(id): UrlPath<String>) -> Reply {
    blocked(move || hosts::forget(&hosts::store_path(), &id)).await?;
    ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
pub(super) struct RepoBody {
    name: String,
    #[serde(default = "yes")]
    private: bool,
    #[serde(default = "yes")]
    push: bool,
}

fn yes() -> bool {
    true
}

pub async fn create_repo(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>, Json(body): Json<RepoBody>) -> Reply {
    let project = app.project();
    if !git::is_repo(&project) {
        return Err(fail(StatusCode::CONFLICT, "this project is not a git repository"));
    }
    let name = body.name.trim().to_string();
    hosts::check_name(&name).map_err(|err| fail(StatusCode::BAD_REQUEST, err))?;
    let private = body.private;
    let push = body.push;
    let made = blocked(move || create_on_host(&project, &id, &name, private, push)).await?;
    ok(made)
}

fn create_on_host(project: &std::path::Path, id: &str, name: &str, private: bool, push: bool) -> Result<Value, String> {
    let store = hosts::store_path();
    if let Some(saved) = hosts::saved_for(&store, id).map_err(|err| hosts::scrub(&err, ""))? {
        let token = saved.token.clone();
        let (created, saved) = hosts::create_with_token(&store, &LiveFetch, id, name, private).map_err(|err| hosts::scrub(&err, &token))?;
        git::ensure_remote(project, "origin", &created.clone_url).map_err(|err| hosts::scrub(&format!("{err:#}"), &token))?;
        let mut pushed = false;
        if push {
            let branch = git::current_branch(project).ok_or_else(|| "this checkout is not on a branch".to_string())?;
            git::check_branch_name(&branch)?;
            let user = hosts::git_user(saved.kind, &saved.user);
            let basic = hosts::basic_secret(&user, &saved.token);
            git::push(project, "origin", &branch, Some(&basic)).map_err(|err| hosts::scrub(&format!("{err:#}"), &token))?;
            pushed = true;
        }
        return Ok(json!({ "clone_url": created.clone_url, "html_url": created.html_url, "pushed": pushed }));
    }
    let (kind, base) = hosts::identify(id)?;
    if (kind == Kind::Github || kind == Kind::Gitlab) && base == kind.default_base() {
        let url = hosts::create_with_cli(kind, project, name, private)?;
        return Ok(json!({ "clone_url": Value::Null, "html_url": url, "pushed": true }));
    }
    Err("connect this host with a token first, or sign in with its CLI".into())
}

pub async fn git_state(State(app): State<Arc<App>>) -> Reply {
    let project = app.project();
    ok(blocked(move || state_of(&project)).await?)
}

#[derive(Deserialize)]
pub(super) struct RemoteBody {
    name: String,
    url: String,
}

pub async fn set_remote(State(app): State<Arc<App>>, Json(body): Json<RemoteBody>) -> Reply {
    let project = app.project();
    if !git::is_repo(&project) {
        return Err(fail(StatusCode::CONFLICT, "this project is not a git repository"));
    }
    let name = body.name.trim().to_string();
    let url = body.url.trim().to_string();
    git::check_remote_name(&name).map_err(|err| fail(StatusCode::BAD_REQUEST, err))?;
    git::check_remote_url(&url).map_err(|err| fail(StatusCode::BAD_REQUEST, err))?;
    let made = blocked(move || {
        git::ensure_remote(&project, &name, &url).map_err(|err| crate::api::redact(&format!("{err:#}")))?;
        state_of(&project)
    })
    .await?;
    ok(made)
}

#[derive(Deserialize, Default)]
pub(super) struct PushBody {
    #[serde(default)]
    remote: String,
    #[serde(default)]
    branch: String,
}

pub async fn push(State(app): State<Arc<App>>, Json(body): Json<PushBody>) -> Reply {
    let project = app.project();
    if !git::is_repo(&project) {
        return Err(fail(StatusCode::CONFLICT, "this project is not a git repository"));
    }
    let remote = if body.remote.trim().is_empty() { "origin".to_string() } else { body.remote.trim().to_string() };
    git::check_remote_name(&remote).map_err(|err| fail(StatusCode::BAD_REQUEST, err))?;
    let branch = if body.branch.trim().is_empty() {
        git::current_branch(&project).ok_or_else(|| fail(StatusCode::CONFLICT, "this checkout is not on a branch"))?
    } else {
        body.branch.trim().to_string()
    };
    git::check_branch_name(&branch).map_err(|err| fail(StatusCode::BAD_REQUEST, err))?;
    let remote_name = remote.clone();
    let branch_name = branch.clone();
    blocked(move || push_one(&project, &remote, &branch)).await?;
    ok(json!({ "remote": remote_name, "branch": branch_name }))
}

fn push_one(project: &std::path::Path, remote: &str, branch: &str) -> Result<(), String> {
    let url = git::remotes(project)
        .map_err(|err| crate::api::redact(&format!("{err:#}")))?
        .into_iter()
        .find(|(name, _)| name == remote)
        .map(|(_, url)| url)
        .ok_or_else(|| format!("this project has no remote `{remote}`"))?;
    let basic = basic_for(&url);
    let token = hosts::auth_for_url(&hosts::store_path(), &url).map(|(_, token)| token).unwrap_or_default();
    git::push(project, remote, branch, basic.as_deref()).map_err(|err| hosts::scrub(&format!("{err:#}"), &token))
}

#[derive(Deserialize)]
pub(super) struct CloneBody {
    url: String,
    #[serde(default)]
    name: String,
}

pub async fn clone(State(app): State<Arc<App>>, Json(body): Json<CloneBody>) -> Reply {
    let url = body.url.trim().to_string();
    git::check_remote_url(&url).map_err(|err| fail(StatusCode::BAD_REQUEST, err))?;
    let name = if body.name.trim().is_empty() { name_from_url(&url).map_err(|err| fail(StatusCode::BAD_REQUEST, err))? } else { body.name.trim().to_string() };
    let dest = workspace_child(&app.workspace, &name)?;
    if dest.exists() {
        return Err(fail(StatusCode::CONFLICT, format!("`{}` already exists", dest.display())));
    }
    let path = blocked(move || clone_into(&url, &dest)).await?;
    let folder = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
    ok(json!({ "path": path.display().to_string(), "name": folder }))
}

fn clone_into(url: &str, dest: &std::path::Path) -> Result<PathBuf, String> {
    if dest.exists() {
        return Err(format!("`{}` already exists", dest.display()));
    }
    let basic = basic_for(url);
    let token = hosts::auth_for_url(&hosts::store_path(), url).map(|(_, token)| token).unwrap_or_default();
    let cloned = git::clone_repo(url, dest, basic.as_deref());
    if cloned.is_err() && dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
    cloned.map_err(|err| hosts::scrub(&format!("{err:#}"), &token))?;
    dest.canonicalize().map_err(|err| format!("cannot open the clone: {err}"))
}
