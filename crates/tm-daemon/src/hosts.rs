//! Connections to GitHub, GitLab, Gitea (Forgejo speaks the same API), and Bitbucket.
//!
//! A signed-in `gh` or `glab` is used when it is there. Otherwise a token saved under
//! the home directory is used. The token is checked against the host before it is
//! kept, and it is never sent back to the browser or written into a git remote.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const KINDS: [Kind; 4] = [Kind::Github, Kind::Gitlab, Kind::Gitea, Kind::Bitbucket];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Github,
    Gitlab,
    Gitea,
    Bitbucket,
}

impl Kind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "github" => Some(Kind::Github),
            "gitlab" => Some(Kind::Gitlab),
            "gitea" => Some(Kind::Gitea),
            "bitbucket" => Some(Kind::Bitbucket),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Github => "github",
            Kind::Gitlab => "gitlab",
            Kind::Gitea => "gitea",
            Kind::Bitbucket => "bitbucket",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Github => "GitHub",
            Kind::Gitlab => "GitLab",
            Kind::Gitea => "Gitea",
            Kind::Bitbucket => "Bitbucket",
        }
    }

    /// The public site, when the person does not name another.
    pub fn default_base(self) -> &'static str {
        match self {
            Kind::Github => "https://github.com",
            Kind::Gitlab => "https://gitlab.com",
            Kind::Gitea => "https://gitea.com",
            Kind::Bitbucket => "https://bitbucket.org",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    kind: String,
    base: String,
    token: String,
    #[serde(default)]
    user: String,
}

#[derive(Clone, Debug)]
pub struct Saved {
    pub kind: Kind,
    pub base: String,
    pub token: String,
    pub user: String,
}

#[derive(Clone, Debug)]
pub struct HostView {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub base: String,
    pub account: Option<String>,
    pub connected: bool,
    pub via: Option<String>,
    pub error: Option<String>,
}

impl HostView {
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind,
            "name": self.name,
            "base": self.base,
            "account": self.account,
            "connected": self.connected,
            "via": self.via,
            "error": self.error,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Created {
    pub clone_url: String,
    pub html_url: String,
}

pub trait Fetch {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, String>;
    fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String>;
}

/// The file that holds tokens. `TASKMASTER_HOSTS` overrides it, for tests.
pub fn store_path() -> PathBuf {
    std::env::var_os("TASKMASTER_HOSTS").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        home.join(".taskmaster").join("hosts.json")
    })
}

pub fn normalize_base(kind: Kind, base: &str) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    let base = if base.is_empty() { kind.default_base().to_string() } else { base.to_string() };
    if !base.starts_with("https://") {
        return Err("the host address has to start with https://".into());
    }
    let rest = &base["https://".len()..];
    if rest.is_empty() || rest.contains(['@', '?', '#', ' ', '\\']) || rest.contains("//") {
        return Err("the host address is not a plain https address".into());
    }
    if kind == Kind::Bitbucket && base != kind.default_base() {
        return Err("Bitbucket Cloud is at https://bitbucket.org".into());
    }
    Ok(base)
}

pub fn check_token(token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.len() < 8 || token.len() > 256 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("a token is 8 to 256 characters, with no spaces".into());
    }
    Ok(())
}

pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 100
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        || name.starts_with('.')
    {
        return Err("a repository name uses letters, digits, `.`, `_` and `-`".into());
    }
    Ok(())
}

fn id_of(kind: Kind, base: &str) -> String {
    if base == kind.default_base() {
        kind.as_str().to_string()
    } else {
        let host = base.trim_start_matches("https://");
        format!("{}:{host}", kind.as_str())
    }
}

pub fn identify(id: &str) -> Result<(Kind, String), String> {
    parse_id(id)
}

fn parse_id(id: &str) -> Result<(Kind, String), String> {
    let (kind_text, host) = match id.split_once(':') {
        Some((kind, host)) => (kind, Some(host)),
        None => (id, None),
    };
    let kind = Kind::parse(kind_text).ok_or_else(|| "unknown host".to_string())?;
    let base = match host {
        Some(host) if !host.is_empty() && !host.contains('/') => format!("https://{host}"),
        Some(_) => return Err("unknown host".into()),
        None => kind.default_base().to_string(),
    };
    Ok((kind, normalize_base(kind, &base)?))
}

pub fn account_of(kind: Kind, body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let key = match kind {
        Kind::Github | Kind::Gitea => "login",
        Kind::Gitlab | Kind::Bitbucket => "username",
    };
    let account = value.get(key)?.as_str()?.trim();
    (!account.is_empty()).then(|| account.to_string())
}

fn auth_headers(kind: Kind, token: &str, user: &str) -> Result<Vec<(String, String)>, String> {
    let mut headers = vec![("user-agent".into(), "taskmaster".into()), ("accept".into(), "application/json".into())];
    match kind {
        Kind::Github => {
            headers.push(("authorization".into(), format!("Bearer {token}")));
            headers.push(("x-github-api-version".into(), "2022-11-28".into()));
        }
        Kind::Gitlab => headers.push(("private-token".into(), token.to_string())),
        Kind::Gitea => headers.push(("authorization".into(), format!("token {token}"))),
        Kind::Bitbucket => {
            if user.is_empty() {
                return Err("Bitbucket needs the username that owns the app password".into());
            }
            let raw = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
            headers.push(("authorization".into(), format!("Basic {raw}")));
        }
    }
    Ok(headers)
}

/// GitHub.com uses `api.github.com`. A GitHub Enterprise host uses `{base}/api/v3`.
fn github_api(base: &str) -> String {
    if base == Kind::Github.default_base() { "https://api.github.com".into() } else { format!("{base}/api/v3") }
}

fn user_url(kind: Kind, base: &str) -> String {
    match kind {
        Kind::Github => format!("{}/user", github_api(base)),
        Kind::Gitlab => format!("{base}/api/v4/user"),
        Kind::Gitea => format!("{base}/api/v1/user"),
        Kind::Bitbucket => "https://api.bitbucket.org/2.0/user".into(),
    }
}

fn header_refs(headers: &[(String, String)]) -> Vec<(&str, &str)> {
    headers.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect()
}

/// What creating a repository asks of the host, before it is sent.
pub fn create_call(kind: Kind, base: &str, name: &str, private: bool, user: &str) -> Result<(String, Value), String> {
    check_name(name)?;
    let (url, body) = match kind {
        Kind::Github => (
            format!("{}/user/repos", github_api(base)),
            json!({ "name": name, "private": private }),
        ),
        Kind::Gitlab => (
            format!("{base}/api/v4/projects"),
            json!({ "name": name, "visibility": if private { "private" } else { "public" } }),
        ),
        Kind::Gitea => (
            format!("{base}/api/v1/user/repos"),
            json!({ "name": name, "private": private }),
        ),
        Kind::Bitbucket => {
            if user.is_empty() {
                return Err("Bitbucket needs the username that owns the app password".into());
            }
            (
                format!("https://api.bitbucket.org/2.0/repositories/{user}/{name}"),
                json!({ "scm": "git", "is_private": private }),
            )
        }
    };
    Ok((url, body))
}

pub fn created_from(kind: Kind, body: &str) -> Result<Created, String> {
    let value: Value = serde_json::from_str(body).map_err(|_| "the host did not return a repository".to_string())?;
    let pick = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let (clone_url, html_url) = match kind {
        Kind::Github | Kind::Gitea => (pick("clone_url"), pick("html_url")),
        Kind::Gitlab => (pick("http_url_to_repo"), pick("web_url")),
        Kind::Bitbucket => {
            let clones = value.pointer("/links/clone").and_then(|v| v.as_array());
            let https = clones.and_then(|list| {
                list.iter().find(|item| item.get("name").and_then(|n| n.as_str()) == Some("https"))
            });
            let clone_url = https.and_then(|item| item.get("href")).and_then(|v| v.as_str()).map(str::to_string);
            let html = value.pointer("/links/html/href").and_then(|v| v.as_str()).map(str::to_string);
            (clone_url, html)
        }
    };
    match (clone_url, html_url) {
        (Some(clone_url), Some(html_url)) if clone_url.starts_with("https://") && html_url.starts_with("https://") => {
            Ok(Created { clone_url, html_url })
        }
        _ => Err("the host did not return a repository address".into()),
    }
}

/// `user:token` as Basic auth, for one git request. The caller scrubs it from errors.
pub fn basic_secret(user: &str, token: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"))
}

/// The git username a host expects beside the token.
pub fn git_user(kind: Kind, account: &str) -> String {
    match kind {
        Kind::Github => "x-access-token".into(),
        Kind::Gitlab => "oauth2".into(),
        Kind::Gitea => {
            if account.is_empty() {
                "token".into()
            } else {
                account.to_string()
            }
        }
        Kind::Bitbucket => account.to_string(),
    }
}

fn load(path: &Path) -> Result<Vec<Saved>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path).map_err(|err| format!("cannot read saved hosts: {err}"))?;
    let records: Vec<Record> = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| serde_json::from_value(value.get("connections")?.clone()).ok())
        .ok_or_else(|| "saved hosts are not readable".to_string())?;
    let mut saved = Vec::new();
    for record in records {
        let Some(kind) = Kind::parse(&record.kind) else { continue };
        let Ok(base) = normalize_base(kind, &record.base) else { continue };
        if check_token(&record.token).is_err() {
            continue;
        }
        saved.push(Saved { kind, base, token: record.token, user: record.user });
    }
    Ok(saved)
}

fn store(path: &Path, saved: &[Saved]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let connections: Vec<Record> = saved
        .iter()
        .map(|item| Record { kind: item.kind.as_str().into(), base: item.base.clone(), token: item.token.clone(), user: item.user.clone() })
        .collect();
    let text = serde_json::to_string_pretty(&json!({ "connections": connections })).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("json.tmp");
    {
        let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(&tmp).map_err(|err| format!("cannot write hosts: {err}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        file.write_all(text.as_bytes()).map_err(|err| format!("cannot write hosts: {err}"))?;
    }
    fs::rename(&tmp, path).map_err(|err| format!("cannot write hosts: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

fn run_cli(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    if !which(program) && Path::new(program).is_relative() {
        return Err(format!("{program} is not installed"));
    }
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let mut child = command.spawn().map_err(|_| format!("{program} is not installed"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > Duration::from_secs(20) => {
                let _ = child.kill();
                return Err(format!("{program} did not answer"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(err) => return Err(format!("{program} stopped: {err}")),
        }
    };
    let out = child.wait_with_output().map_err(|err| format!("{program} stopped: {err}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if !status.success() {
        return Err(crate::api::redact(text.trim()));
    }
    Ok(text)
}

/// The account a signed-in CLI can see, for the public site of that host.
pub fn cli_account(kind: Kind) -> Option<String> {
    match kind {
        Kind::Github => {
            let program = std::env::var("TASKMASTER_GH").unwrap_or_else(|_| "gh".into());
            cli_github(&program)
        }
        Kind::Gitlab => cli_gitlab(),
        Kind::Gitea | Kind::Bitbucket => None,
    }
}

fn cli_github(program: &str) -> Option<String> {
    let text = run_cli(program, &["api", "user", "--jq", ".login"], None).ok()?;
    let login = text.trim();
    (!login.is_empty() && !login.contains(' ') && login.len() < 80).then(|| login.to_string())
}

fn cli_gitlab() -> Option<String> {
    let program = std::env::var("TASKMASTER_GLAB").unwrap_or_else(|_| "glab".into());
    let text = run_cli(&program, &["api", "user"], None).ok()?;
    account_of(Kind::Gitlab, text.trim())
}

fn live_cli(kind: Kind) -> Option<String> {
    cli_account(kind)
}

fn probe(fetch: &dyn Fetch, saved: &Saved) -> Result<String, String> {
    let headers = auth_headers(saved.kind, &saved.token, &saved.user)?;
    let refs = header_refs(&headers);
    let body = fetch.get(&user_url(saved.kind, &saved.base), &refs)?;
    account_of(saved.kind, &body).ok_or_else(|| "the host did not name an account".to_string())
}

fn blank(kind: Kind, base: &str) -> HostView {
    HostView {
        id: id_of(kind, base),
        kind: kind.as_str().into(),
        name: kind.label().into(),
        base: base.into(),
        account: None,
        connected: false,
        via: None,
        error: None,
    }
}

/// Every host the browser can connect, including ones that are not connected yet.
pub fn list(path: &Path, fetch: &dyn Fetch, cli: &dyn Fn(Kind) -> Option<String>) -> Vec<HostView> {
    let saved = load(path).unwrap_or_default();
    let mut views = Vec::new();
    for kind in KINDS {
        let base = kind.default_base();
        let mut view = blank(kind, base);
        if let Some(account) = cli(kind) {
            view.account = Some(account);
            view.connected = true;
            view.via = Some("cli".into());
        } else if let Some(item) = saved.iter().find(|item| item.kind == kind && item.base == base) {
            match probe(fetch, item) {
                Ok(account) => {
                    view.account = Some(account);
                    view.connected = true;
                    view.via = Some("token".into());
                }
                Err(err) => view.error = Some(scrub(&err, &item.token)),
            }
        }
        views.push(view);
    }
    for item in &saved {
        if views.iter().any(|view| view.base == item.base && view.kind == item.kind.as_str()) {
            continue;
        }
        let mut view = blank(item.kind, &item.base);
        match probe(fetch, item) {
            Ok(account) => {
                view.account = Some(account);
                view.connected = true;
                view.via = Some("token".into());
            }
            Err(err) => view.error = Some(scrub(&err, &item.token)),
        }
        views.push(view);
    }
    views
}

pub fn list_live(path: &Path, fetch: &dyn Fetch) -> Vec<HostView> {
    list(path, fetch, &live_cli)
}

pub fn connect(path: &Path, fetch: &dyn Fetch, kind: Kind, token: &str, base: &str, user: &str) -> Result<HostView, String> {
    check_token(token)?;
    let base = normalize_base(kind, base)?;
    let user = user.trim().to_string();
    if kind == Kind::Bitbucket && (user.is_empty() || user.len() > 64 || user.chars().any(|c| c.is_whitespace())) {
        return Err("Bitbucket needs the username that owns the app password".into());
    }
    let mut saved_one = Saved { kind, base: base.clone(), token: token.trim().to_string(), user: user.clone() };
    let account = probe(fetch, &saved_one)?;
    if saved_one.user.is_empty() {
        saved_one.user = account.clone();
    }
    let mut all = load(path).unwrap_or_default();
    all.retain(|item| !(item.kind == kind && item.base == base));
    all.push(saved_one);
    store(path, &all)?;
    Ok(HostView {
        id: id_of(kind, &base),
        kind: kind.as_str().into(),
        name: kind.label().into(),
        base,
        account: Some(account),
        connected: true,
        via: Some("token".into()),
        error: None,
    })
}

pub fn forget(path: &Path, id: &str) -> Result<(), String> {
    let (kind, base) = parse_id(id)?;
    let mut all = load(path)?;
    let before = all.len();
    all.retain(|item| !(item.kind == kind && item.base == base));
    if all.len() == before {
        return Err("that host has no saved token".into());
    }
    store(path, &all)
}

pub fn saved_for(path: &Path, id: &str) -> Result<Option<Saved>, String> {
    let (kind, base) = parse_id(id)?;
    Ok(load(path)?.into_iter().find(|item| item.kind == kind && item.base == base))
}

/// Create the repository with a saved token. The caller pushes.
pub fn create_with_token(path: &Path, fetch: &dyn Fetch, id: &str, name: &str, private: bool) -> Result<(Created, Saved), String> {
    let saved = saved_for(path, id)?.ok_or_else(|| "connect this host with a token first, or sign in with its CLI".to_string())?;
    let (url, body) = create_call(saved.kind, &saved.base, name, private, &saved.user)?;
    let headers = auth_headers(saved.kind, &saved.token, &saved.user)?;
    let refs = header_refs(&headers);
    let reply = fetch.post(&url, &refs, &body.to_string()).map_err(|err| scrub(&err, &saved.token))?;
    let created = created_from(saved.kind, &reply).map_err(|err| scrub(&err, &saved.token))?;
    Ok((created, saved))
}

/// Create and push with `gh` or `glab` when that CLI is signed in on the public site.
pub fn create_with_cli(kind: Kind, repo: &Path, name: &str, private: bool) -> Result<String, String> {
    check_name(name)?;
    if kind == Kind::Github {
        let program = std::env::var("TASKMASTER_GH").unwrap_or_else(|_| "gh".into());
        if cli_github(&program).is_none() {
            return Err("gh is not signed in".into());
        }
        let privacy = if private { "--private" } else { "--public" };
        let text = run_cli(&program, &["repo", "create", name, privacy, "--source", ".", "--remote", "origin", "--push"], Some(repo))?;
        return Ok(text.lines().find(|line| line.starts_with("https://")).unwrap_or("").trim().to_string());
    }
    if kind == Kind::Gitlab {
        if cli_gitlab().is_none() {
            return Err("glab is not signed in".into());
        }
        let program = std::env::var("TASKMASTER_GLAB").unwrap_or_else(|_| "glab".into());
        let privacy = if private { "--private" } else { "--public" };
        let text = run_cli(&program, &["repo", "create", name, privacy, "--defaultBranch", "main"], Some(repo))?;
        let url = text.lines().find(|line| line.contains("https://")).unwrap_or("").trim().to_string();
        crate::git::push(repo, "origin", "main", None).map_err(|err| crate::api::redact(&format!("{err:#}")))?;
        return Ok(url);
    }
    Err("this host is connected with a token, not a CLI".into())
}

/// A saved token that can authenticate `url`, if the address is one we know.
pub fn auth_for_url(path: &Path, url: &str) -> Option<(String, String)> {
    let host = url_host(url)?;
    let saved = load(path).ok()?;
    let item = saved.into_iter().find(|item| item.base.trim_start_matches("https://").eq_ignore_ascii_case(host))?;
    let user = git_user(item.kind, if item.user.is_empty() { "" } else { &item.user });
    let user = if item.kind == Kind::Bitbucket { item.user.clone() } else { user };
    if user.is_empty() {
        return None;
    }
    Some((user, item.token))
}

pub fn url_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit('@').next()?;
    (!host.is_empty()).then_some(host)
}

pub fn scrub(text: &str, token: &str) -> String {
    let mut out = crate::api::redact(text);
    if token.len() >= 8 {
        out = out.replace(token, "***");
    }
    out
}

pub struct LiveFetch;

impl Fetch for LiveFetch {
    fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
        call("GET", url, headers, None)
    }

    fn post(&self, url: &str, headers: &[(&str, &str)], body: &str) -> Result<String, String> {
        call("POST", url, headers, Some(body))
    }
}

fn call(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&str>) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).build();
    let mut request = match method {
        "POST" => agent.post(url),
        _ => agent.get(url),
    };
    for (key, value) in headers {
        request = request.set(key, value);
    }
    let result = if let Some(body) = body { request.send_string(body) } else { request.call() };
    match result {
        Ok(response) => response.into_string().map_err(|err| format!("the host sent a reply that could not be read: {err}")),
        Err(ureq::Error::Status(code, response)) => {
            let text = response.into_string().unwrap_or_default();
            let text = clip(&text, 240);
            Err(format!("the host answered {code}: {text}"))
        }
        Err(err) => Err(format!("could not reach the host: {err}")),
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Script {
        get: String,
        post: String,
    }

    impl Fetch for Script {
        fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
            assert!(url.starts_with("https://"), "{url}");
            assert!(headers.iter().any(|(key, value)| {
                let key = key.to_ascii_lowercase();
                (key == "authorization" || key == "private-token") && value.contains("secret-token-value")
            }));
            Ok(self.get.clone())
        }

        fn post(&self, url: &str, _headers: &[(&str, &str)], body: &str) -> Result<String, String> {
            assert!(url.starts_with("https://"), "{url}");
            assert!(body.contains("shop"));
            Ok(self.post.clone())
        }
    }

    fn dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tm-hosts-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path.join("hosts.json")
    }

    #[test]
    fn a_token_is_checked_then_kept_and_never_shown() {
        let path = dir("keep");
        let fetch = Script { get: r#"{"login":"ada"}"#.into(), post: String::new() };
        let view = connect(&path, &fetch, Kind::Github, "secret-token-value", "", "").unwrap();
        assert_eq!(view.account.as_deref(), Some("ada"));
        assert_eq!(view.via.as_deref(), Some("token"));
        let shown = view.json().to_string();
        assert!(!shown.contains("secret-token-value"));
        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("secret-token-value"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "hosts file is readable by others: {mode:o}");
        }
        let listed = list(&path, &fetch, &|_| None);
        let github = listed.iter().find(|view| view.kind == "github").unwrap();
        assert_eq!(github.account.as_deref(), Some("ada"));
        assert!(!github.json().to_string().contains("secret-token-value"));
        forget(&path, "github").unwrap();
        assert!(saved_for(&path, "github").unwrap().is_none());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn each_host_names_its_account_and_its_clone_address() {
        assert_eq!(account_of(Kind::Gitlab, r#"{"username":"ada"}"#).as_deref(), Some("ada"));
        assert_eq!(account_of(Kind::Gitea, r#"{"login":"ada"}"#).as_deref(), Some("ada"));
        assert_eq!(account_of(Kind::Bitbucket, r#"{"username":"ada"}"#).as_deref(), Some("ada"));
        let github = created_from(Kind::Github, r#"{"clone_url":"https://github.com/ada/shop.git","html_url":"https://github.com/ada/shop"}"#).unwrap();
        assert_eq!(github.clone_url, "https://github.com/ada/shop.git");
        let gitlab = created_from(
            Kind::Gitlab,
            r#"{"http_url_to_repo":"https://gitlab.com/ada/shop.git","web_url":"https://gitlab.com/ada/shop"}"#,
        )
        .unwrap();
        assert!(gitlab.html_url.ends_with("/shop"));
        let bucket = created_from(
            Kind::Bitbucket,
            r#"{"links":{"clone":[{"name":"https","href":"https://bitbucket.org/ada/shop.git"}],"html":{"href":"https://bitbucket.org/ada/shop"}}}"#,
        )
        .unwrap();
        assert!(bucket.clone_url.contains("bitbucket.org"));
        assert!(check_token("short").is_err());
        assert!(normalize_base(Kind::Gitlab, "http://gitlab.com").is_err());
        assert!(normalize_base(Kind::Bitbucket, "https://bitbucket.example.com").is_err());
        assert_eq!(normalize_base(Kind::Gitea, "https://code.example.com/").unwrap(), "https://code.example.com");
        assert_eq!(identify("github").unwrap().1, "https://github.com");
        assert_eq!(identify("gitea:code.example.com").unwrap().1, "https://code.example.com");
        let (public, _) = create_call(Kind::Github, "https://github.com", "shop", true, "").unwrap();
        assert_eq!(public, "https://api.github.com/user/repos");
        let (enterprise, _) = create_call(Kind::Github, "https://git.example.com", "shop", true, "").unwrap();
        assert_eq!(enterprise, "https://git.example.com/api/v3/user/repos");
        assert_eq!(scrub("token secret-token-value failed", "secret-token-value"), "token *** failed");
        let hidden = scrub("https://ada:secret-token-value@github.com/a", "secret-token-value");
        assert!(hidden.contains("***@"));
        assert!(!hidden.contains("secret-token-value"));
    }

    #[test]
    fn creating_a_repository_uses_the_saved_token() {
        let path = dir("create");
        let fetch = Script {
            get: r#"{"username":"ada"}"#.into(),
            post: r#"{"http_url_to_repo":"https://gitlab.com/ada/shop.git","web_url":"https://gitlab.com/ada/shop"}"#.into(),
        };
        connect(&path, &fetch, Kind::Gitlab, "secret-token-value", "", "").unwrap();
        let (created, saved) = create_with_token(&path, &fetch, "gitlab", "shop", true).unwrap();
        assert_eq!(created.clone_url, "https://gitlab.com/ada/shop.git");
        assert_eq!(saved.kind, Kind::Gitlab);
        assert_eq!(git_user(Kind::Gitlab, ""), "oauth2");
        assert_eq!(git_user(Kind::Github, ""), "x-access-token");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
