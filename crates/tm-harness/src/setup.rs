//! Installing, updating and signing in to each harness CLI, and the runtimes projects need.
//!
//! Every command here is a constant. Nothing a person types becomes part of one: the
//! daemon picks a [`Step`] list by provider id and action, and the only free text it passes
//! on is what a person types into a running sign-in screen.

use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::providers::{Provider, spec};
use crate::sandbox::resolve_binary;

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// What a person can ask Taskmaster to do for a provider or a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Install,
    Update,
    Login,
}

impl Action {
    pub fn parse(name: &str) -> Option<Action> {
        Some(match name {
            "install" => Action::Install,
            "update" => Action::Update,
            "login" => Action::Login,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Action::Install => "install",
            Action::Update => "update",
            Action::Login => "login",
        }
    }
}

/// One program the daemon runs for a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What the job shows while this step runs.
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
    /// Run on a terminal, so a sign-in screen draws and reads keys. Output is the same.
    pub terminal: bool,
}

impl Step {
    fn script(label: &str, script: &str) -> Step {
        Step { label: label.into(), program: "/bin/sh".into(), args: vec!["-c".into(), script.into()], terminal: false }
    }

    fn cli(label: &str, binary: &str, args: &[&str], terminal: bool) -> Step {
        let program = resolve_binary(binary).map(|p| p.display().to_string()).unwrap_or_else(|_| binary.to_string());
        Step { label: label.into(), program, args: args.iter().map(|a| a.to_string()).collect(), terminal }
    }

    pub fn command_line(&self) -> String {
        if self.program == "/bin/sh" && self.args.first().map(String::as_str) == Some("-c") {
            return self.args.get(1).cloned().unwrap_or_default();
        }
        let name = std::path::Path::new(&self.program).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        std::iter::once(name).chain(self.args.iter().cloned()).collect::<Vec<_>>().join(" ")
    }
}

/// A language runtime that project builds, checks, skills and MCP servers start.
pub struct Runtime {
    pub id: &'static str,
    pub name: &'static str,
    pub binary: &'static str,
    pub version_args: &'static [&'static str],
    pub install: &'static str,
    /// What it is for, in one line.
    pub used_for: &'static str,
}

/// Node from nodejs.org: the current LTS line's tarball for this machine, checked against
/// the release's published SHA-256 sums, unpacked under the user's data directory, and
/// linked into `~/.local/bin`.
const NODE_INSTALL: &str = r#"set -eu
case "$(uname -s)" in Linux) os=linux ;; Darwin) os=darwin ;; *) echo "unsupported system $(uname -s)"; exit 1 ;; esac
case "$(uname -m)" in x86_64|amd64) arch=x64 ;; aarch64|arm64) arch=arm64 ;; *) echo "unsupported machine $(uname -m)"; exit 1 ;; esac
base=https://nodejs.org/dist/latest-v24.x
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$base/SHASUMS256.txt" -o "$tmp/sums"
file=$(grep -o "node-v[0-9.]*-$os-$arch.tar.gz" "$tmp/sums" | head -n 1)
[ -n "$file" ] || { echo "no Node build for $os-$arch"; exit 1; }
echo "downloading $file"
curl -fL --progress-bar "$base/$file" -o "$tmp/$file"
want=$(grep " $file\$" "$tmp/sums" | cut -d ' ' -f 1)
if command -v sha256sum >/dev/null; then have=$(sha256sum "$tmp/$file" | cut -d ' ' -f 1); else have=$(shasum -a 256 "$tmp/$file" | cut -d ' ' -f 1); fi
[ "$want" = "$have" ] || { echo "checksum mismatch for $file"; exit 1; }
echo "checksum ok"
dest="${XDG_DATA_HOME:-$HOME/.local/share}/taskmaster/runtimes/node"
rm -rf "$dest" && mkdir -p "$dest" "$HOME/.local/bin"
tar -xzf "$tmp/$file" -C "$dest" --strip-components 1
for bin in node npm npx corepack; do ln -sf "$dest/bin/$bin" "$HOME/.local/bin/$bin"; done
"$dest/bin/node" --version"#;

pub const RUNTIMES: [Runtime; 3] = [
    Runtime {
        id: "node",
        name: "Node.js",
        binary: "node",
        version_args: &["--version"],
        install: NODE_INSTALL,
        used_for: "JavaScript builds, dev servers, npx MCP servers",
    },
    Runtime {
        id: "bun",
        name: "Bun",
        binary: "bun",
        version_args: &["--version"],
        install: "curl -fsSL https://bun.sh/install | bash",
        used_for: "Bun projects and skills that call bun",
    },
    Runtime {
        id: "python",
        name: "Python",
        binary: "python3",
        version_args: &["--version"],
        install: "set -eu\ncommand -v uv >/dev/null || curl -LsSf https://astral.sh/uv/install.sh | sh\n\"$(command -v uv || echo \"$HOME/.local/bin/uv\")\" python install --default",
        used_for: "Python projects, ruff, skills with Python scripts",
    },
];

pub fn runtime(id: &str) -> Option<&'static Runtime> {
    RUNTIMES.iter().find(|r| r.id == id)
}

/// The steps that carry out `action` for the provider `id`. `None` for an unknown
/// provider, or an action that does not apply in this state (updating what is missing).
pub fn provider_steps(id: &str, action: Action) -> Option<Vec<Step>> {
    let spec = spec(id)?;
    let installed = resolve_binary(spec.binary).is_ok();
    Some(match action {
        Action::Install => {
            let mut steps: Vec<Step> = spec
                .needs
                .iter()
                .filter_map(|need| runtime(need))
                .filter(|rt| resolve_binary(rt.binary).is_err())
                .map(|rt| Step::script(&format!("Installing {}", rt.name), rt.install))
                .collect();
            steps.push(Step::script(&format!("Installing {}", spec.name), spec.install));
            steps.push(Step::cli("Checking the install", spec.binary, spec.version, false));
            steps
        }
        Action::Update if installed => {
            vec![Step::cli(&format!("Updating {}", spec.name), spec.binary, spec.update, true), Step::cli("Checking the version", spec.binary, spec.version, false)]
        }
        Action::Login if installed => vec![Step::cli(&format!("Signing in to {}", spec.name), spec.binary, spec.login_args, true)],
        Action::Update | Action::Login => return None,
    })
}

/// The steps that install a runtime. `None` for an unknown one.
pub fn runtime_steps(id: &str) -> Option<Vec<Step>> {
    let rt = runtime(id)?;
    Some(vec![Step::script(&format!("Installing {}", rt.name), rt.install), Step::cli("Checking the install", rt.binary, rt.version_args, false)])
}

/// Where installers put programs. A job runs with these in front of `PATH`, so a step can
/// use what an earlier one installed.
pub fn install_dirs() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return Vec::new() };
    [".local/bin", ".bun/bin", ".opencode/bin", ".grok/bin", ".codex/bin", ".npm-global/bin"].iter().map(|d| home.join(d)).collect()
}

/// `PATH` for a job: the install directories, then the daemon's own.
pub fn job_path() -> std::ffi::OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let dirs = install_dirs().into_iter().chain(std::env::split_paths(&current));
    std::env::join_paths(dirs).unwrap_or(current)
}

/// One provider's install, version, sign-in and update state.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderStatus {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub path: Option<String>,
    pub version: Option<String>,
    /// `None` when the CLI cannot say without starting a session.
    pub signed_in: Option<bool>,
    pub account: Option<String>,
    pub latest: Option<String>,
    pub update_available: Option<bool>,
    /// Runtimes its installer needs that are missing.
    pub missing_runtimes: Vec<String>,
    /// What could not be read, one line each.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RuntimeStatus {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub version: Option<String>,
    pub used_for: String,
}

pub async fn runtime_statuses() -> Vec<RuntimeStatus> {
    let mut out = Vec::new();
    for rt in &RUNTIMES {
        let found = resolve_binary(rt.binary).ok();
        let version = match &found {
            Some(path) => probe(&path.display().to_string(), rt.version_args).await.ok().and_then(|text| parse_version(&text)),
            None => None,
        };
        out.push(RuntimeStatus { id: rt.id.into(), name: rt.name.into(), installed: found.is_some(), version, used_for: rt.used_for.into() });
    }
    out
}

pub async fn provider_statuses() -> Vec<ProviderStatus> {
    let ids = crate::HARNESSES;
    let (a, b, c, d, e) =
        tokio::join!(provider_status(ids[0]), provider_status(ids[1]), provider_status(ids[2]), provider_status(ids[3]), provider_status(ids[4]));
    vec![a, b, c, d, e]
}

pub async fn provider_status(id: &str) -> ProviderStatus {
    let Some(spec) = spec(id) else {
        return ProviderStatus { id: id.into(), name: id.into(), notes: vec![format!("unknown harness `{id}`")], ..Default::default() };
    };
    let mut status = ProviderStatus { id: spec.id.into(), name: spec.name.into(), ..Default::default() };
    status.missing_runtimes =
        spec.needs.iter().filter_map(|n| runtime(n)).filter(|rt| resolve_binary(rt.binary).is_err()).map(|rt| rt.id.to_string()).collect();
    let Ok(path) = resolve_binary(spec.binary) else { return status };
    let program = path.display().to_string();
    status.installed = true;
    status.path = Some(program.clone());
    let (version, auth, latest) = tokio::join!(probe(&program, spec.version), auth_status(spec, &program), latest_version(spec, &program));
    match version {
        Ok(text) => status.version = parse_version(&text),
        Err(err) => status.notes.push(format!("version: {err}")),
    }
    match auth {
        Ok(auth) => {
            status.signed_in = auth.signed_in;
            status.account = auth.account;
        }
        Err(err) => status.notes.push(format!("sign-in: {err}")),
    }
    match latest {
        Ok(latest) => status.latest = latest,
        Err(err) => status.notes.push(format!("latest version: {err}")),
    }
    if let (Some(have), Some(latest)) = (&status.version, &status.latest) {
        status.update_available = Some(newer(latest, have));
    }
    status
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Auth {
    pub signed_in: Option<bool>,
    pub account: Option<String>,
}

/// How each CLI says whether it is signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthProbe {
    /// `claude auth status`: JSON with `loggedIn` and `email`.
    ClaudeJson,
    /// `codex login status`: `Logged in using ...` or `Not logged in`.
    CodexText,
    /// `grok models` opens with `You are logged in with <site>.`
    GrokModels,
    /// `opencode auth list`: one line per stored credential.
    OpencodeList,
    /// `agy models` lists models only for a signed-in user.
    ListsModels,
}

async fn auth_status(spec: &Provider, program: &str) -> anyhow::Result<Auth> {
    let text = probe_any(program, spec.status).await;
    Ok(match spec.auth {
        AuthProbe::ClaudeJson => parse_claude_auth(&text?),
        AuthProbe::CodexText => parse_codex_auth(&text?),
        AuthProbe::GrokModels => parse_grok_auth(&text?),
        AuthProbe::OpencodeList => parse_opencode_auth(&text?),
        AuthProbe::ListsModels => match text {
            Ok(text) if text.lines().any(|l| l.contains('\t')) => Auth { signed_in: Some(true), account: None },
            Ok(_) => Auth { signed_in: Some(false), account: None },
            Err(_) => Auth { signed_in: Some(false), account: None },
        },
    })
}

pub fn parse_claude_auth(text: &str) -> Auth {
    let Some(value) = json_in(text) else { return Auth::default() };
    let signed_in = value.get("loggedIn").and_then(Value::as_bool);
    let account = value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| value.get("orgName").and_then(Value::as_str).map(str::to_string))
        .filter(|_| signed_in == Some(true));
    let plan = value.get("subscriptionType").and_then(Value::as_str);
    let account = match (account, plan) {
        (Some(who), Some(plan)) => Some(format!("{who} ({plan})")),
        (who, _) => who,
    };
    Auth { signed_in, account }
}

pub fn parse_codex_auth(text: &str) -> Auth {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let lower = line.to_ascii_lowercase();
    if lower.starts_with("not logged in") {
        return Auth { signed_in: Some(false), account: None };
    }
    if let Some(how) = line.strip_prefix("Logged in using ") {
        return Auth { signed_in: Some(true), account: Some(how.trim().to_string()) };
    }
    if lower.starts_with("logged in") {
        return Auth { signed_in: Some(true), account: None };
    }
    Auth::default()
}

pub fn parse_grok_auth(text: &str) -> Auth {
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("You are logged in with ") {
            return Auth { signed_in: Some(true), account: Some(rest.trim_end_matches('.').to_string()) };
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("not logged in") || lower.contains("grok login") {
            return Auth { signed_in: Some(false), account: None };
        }
    }
    Auth::default()
}

/// `opencode auth list`: a header, then a line per stored credential such as
/// `●  Anthropic oauth`, and a count line.
pub fn parse_opencode_auth(text: &str) -> Auth {
    let lower = text.to_ascii_lowercase();
    if lower.contains("no authenticated") || lower.contains("no credentials") || lower.contains(" 0 credentials") {
        return Auth { signed_in: Some(false), account: None };
    }
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['┌', '│', '└', '●', '○', '◇', '◆', '*', '-']).trim();
        let lower = line.to_ascii_lowercase();
        if line.is_empty() || lower.starts_with("credentials") || lower.contains("environment") || lower.ends_with("credentials") || lower.contains("~/") {
            continue;
        }
        if let Some(name) = line.split_whitespace().next()
            && name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        {
            names.push(name.to_string());
        }
    }
    if names.is_empty() {
        return Auth { signed_in: Some(false), account: None };
    }
    names.dedup();
    Auth { signed_in: Some(true), account: Some(names.join(", ")) }
}

/// Where the newest published version is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latest {
    Npm(&'static str),
    /// `grok update --check --json`.
    GrokCheck,
    Unknown,
}

const LATEST_TTL: Duration = Duration::from_secs(600);

/// The newest version of each provider, and when it was read.
type LatestCache = std::sync::Mutex<std::collections::HashMap<&'static str, (std::time::Instant, Option<String>)>>;

fn latest_cache() -> &'static LatestCache {
    static CACHE: std::sync::OnceLock<LatestCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Forget the cached newest versions, after an update.
pub fn forget_latest() {
    latest_cache().lock().unwrap().clear();
}

async fn latest_version(spec: &Provider, program: &str) -> anyhow::Result<Option<String>> {
    if let Some((at, value)) = latest_cache().lock().unwrap().get(spec.id)
        && at.elapsed() < LATEST_TTL
    {
        return Ok(value.clone());
    }
    let value = match spec.latest {
        Latest::Npm(package) => {
            let url = format!("https://registry.npmjs.org/{package}/latest");
            let text = probe("curl", &["-fsSL", "--max-time", "10", &url]).await?;
            json_in(&text).and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string))
        }
        Latest::GrokCheck => {
            let text = probe(program, &["update", "--check", "--json"]).await?;
            json_in(&text).and_then(|v| v.get("latestVersion").and_then(Value::as_str).map(str::to_string))
        }
        Latest::Unknown => None,
    };
    latest_cache().lock().unwrap().insert(spec.id, (std::time::Instant::now(), value.clone()));
    Ok(value)
}

/// The first `x.y.z` in a version line: `2.1.325 (Claude Code)`, `codex-cli 0.169.0`, `v24.11.0`.
pub fn parse_version(text: &str) -> Option<String> {
    for word in text.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',') {
        let word = word.trim_start_matches('v');
        let core: String = word.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if core.split('.').filter(|p| !p.is_empty()).count() >= 2 && core.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return Some(core.trim_end_matches('.').to_string());
        }
    }
    None
}

/// Whether `latest` is a later dotted version than `have`.
pub fn newer(latest: &str, have: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect() };
    let (a, b) = (parts(latest), parts(have));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn json_in(text: &str) -> Option<Value> {
    let text = text.trim();
    serde_json::from_str(text).ok().or_else(|| {
        let (start, end) = (text.find('{')?, text.rfind('}')?);
        serde_json::from_str(&text[start..=end]).ok()
    })
}

/// Run a short read-only command and return its output when it succeeds.
async fn probe(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let text = run_probe(program, args).await?;
    match text {
        (true, out) => Ok(out),
        (false, out) => anyhow::bail!("{}", out.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed").chars().take(160).collect::<String>()),
    }
}

/// Like [`probe`], but a failing command's output is still returned: `codex login status`
/// exits 1 when signed out.
async fn probe_any(program: &str, args: &[&str]) -> anyhow::Result<String> {
    Ok(run_probe(program, args).await?.1)
}

async fn run_probe(program: &str, args: &[&str]) -> anyhow::Result<(bool, String)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).env("PATH", job_path()).env("NO_COLOR", "1").stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("{program} took longer than {}s", PROBE_TIMEOUT.as_secs()))??;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_each_cli_s_own_line() {
        assert_eq!(parse_version("2.1.325 (Claude Code)").as_deref(), Some("2.1.325"));
        assert_eq!(parse_version("codex-cli 0.169.0").as_deref(), Some("0.169.0"));
        assert_eq!(parse_version("grok 1.0.77 (50ca579fa996) [stable]").as_deref(), Some("1.0.77"));
        assert_eq!(parse_version("v24.11.0\n").as_deref(), Some("24.11.0"));
        assert_eq!(parse_version("Python 3.12.3").as_deref(), Some("3.12.3"));
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn a_later_version_is_an_update() {
        assert!(newer("1.0.88", "1.0.77"));
        assert!(newer("2.0.0", "1.99.99"));
        assert!(newer("1.10.0", "1.9.0"));
        assert!(!newer("1.0.77", "1.0.77"));
        assert!(!newer("1.0.7", "1.0.77"));
    }

    #[test]
    fn claude_names_the_signed_in_account_and_plan() {
        let auth = parse_claude_auth(r#"{"loggedIn": true, "email": "someone@example.com", "subscriptionType": "pro"}"#);
        assert_eq!(auth, Auth { signed_in: Some(true), account: Some("someone@example.com (pro)".into()) });
        let out = parse_claude_auth(r#"{"loggedIn": false, "email": "old@example.com"}"#);
        assert_eq!(out, Auth { signed_in: Some(false), account: None });
        assert_eq!(parse_claude_auth("Not JSON"), Auth::default());
    }

    #[test]
    fn codex_grok_and_opencode_say_it_in_text() {
        assert_eq!(parse_codex_auth("Logged in using ChatGPT\n"), Auth { signed_in: Some(true), account: Some("ChatGPT".into()) });
        assert_eq!(parse_codex_auth("Not logged in\n"), Auth { signed_in: Some(false), account: None });
        assert_eq!(
            parse_grok_auth("You are logged in with grok.com.\nDefault model: x\n"),
            Auth { signed_in: Some(true), account: Some("grok.com".into()) }
        );
        assert_eq!(parse_grok_auth("Error: not logged in. Run `grok login`."), Auth { signed_in: Some(false), account: None });
        let listed = "┌  Credentials ~/.local/share/opencode/auth.json\n│\n●  Anthropic oauth\n│\n●  OpenAI api\n│\n└  2 credentials\n";
        assert_eq!(parse_opencode_auth(listed), Auth { signed_in: Some(true), account: Some("Anthropic, OpenAI".into()) });
        assert_eq!(parse_opencode_auth("┌  Credentials ~/.local/share/opencode/auth.json\n│\n└  0 credentials\n").signed_in, Some(false));
        assert_eq!(parse_opencode_auth("No authenticated integrations\n"), Auth { signed_in: Some(false), account: None });
    }

    #[test]
    fn every_provider_has_steps_for_each_action_and_nothing_else_runs() {
        for id in crate::HARNESSES {
            let install = provider_steps(id, Action::Install).unwrap();
            assert!(install.iter().any(|s| s.label.starts_with("Installing")), "{id}");
            assert_eq!(install.last().unwrap().label, "Checking the install");
        }
        assert!(provider_steps("rm -rf /", Action::Install).is_none());
        assert!(Action::parse("exec").is_none());
        assert!(runtime_steps("perl").is_none());
        for rt in &RUNTIMES {
            assert_eq!(runtime_steps(rt.id).unwrap().len(), 2);
        }
    }

    #[test]
    fn node_is_checked_against_its_published_sums() {
        assert!(NODE_INSTALL.contains("SHASUMS256.txt"));
        assert!(NODE_INSTALL.contains("checksum mismatch"));
    }

    #[test]
    fn a_step_shows_the_command_a_person_would_type() {
        let step = Step::script("x", "curl -fsSL https://example.com | bash");
        assert_eq!(step.command_line(), "curl -fsSL https://example.com | bash");
        let step = Step { label: "x".into(), program: "/home/u/.local/bin/codex".into(), args: vec!["login".into(), "--device-auth".into()], terminal: true };
        assert_eq!(step.command_line(), "codex login --device-auth");
    }
}
