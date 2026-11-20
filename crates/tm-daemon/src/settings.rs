//! The settings file: where records are kept, who may sign in, and which Commons is shared.
//!
//! `.taskmaster/config.json` in the project, or `~/.taskmaster/config.json`. Every part
//! has a default, so a missing file changes nothing. `config.example.json` in the
//! repository shows every field with a placeholder. A placeholder is treated as "not set
//! yet": the daemon says so and carries on with the default, so the example file can be
//! copied as it is and filled in one part at a time.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::Deserialize;
use tm_dictionary::CommonsRemote;

use crate::auth::{AccountAuth, AnyAuth, Auth, PairingAuth};
use crate::commons::HttpCommons;
use crate::sqlite::SqliteStore;
use crate::store::{Records, Store};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub store: StoreSettings,
    pub auth: AuthSettings,
    pub commons: CommonsSettings,
    pub approvals: ApprovalSettings,
    pub preview: PreviewSettings,
    pub search: SearchSettings,
    /// The editor `Open` uses: one of the ids in `GET /api/editors`, or the name of a
    /// program on this machine. Empty means `$VISUAL`, then `$EDITOR`, then whatever is
    /// installed.
    pub editor: String,
    /// The harness for each role when a run does not say. Empty until the person chooses.
    pub roles: crate::Roles,
    /// The model for each role when a run does not say; none means the provider's default.
    pub models: crate::Models,
    pub checker: CheckerSettings,
    pub code_mode: CodeModeSettings,
    /// Off until a person turns it on. See `telemetry.rs`.
    pub telemetry: TelemetrySettings,
    /// How long one agent call may run.
    pub agents: AgentSettings,
    /// What a worker's sandbox lets it do.
    pub sandbox: SandboxSettings,
    /// Dollars per million tokens, by model id or harness id, for calls whose harness
    /// reports no price. Grok's are built in (see [`built_in_prices`]).
    pub prices: std::collections::BTreeMap<String, tm_core::Prices>,
}

/// Prices Taskmaster knows without a settings file: Grok Build's, as the benchmark's grader
/// counts them ($2/M input, $0.50/M cached input, $6/M output).
pub fn built_in_prices() -> std::collections::BTreeMap<String, tm_core::Prices> {
    let grok = tm_core::Prices { input: 2.0, cached_input: Some(0.5), output: 6.0 };
    ["grok", "grok-build"].iter().map(|k| (k.to_string(), grok)).collect()
}

/// The price table a run uses: the built-in prices, then the settings file's over them.
pub fn price_table(settings: &Settings) -> std::collections::BTreeMap<String, tm_core::Prices> {
    let mut table = built_in_prices();
    table.extend(settings.prices.clone());
    table
}

/// A builder's shell. It is only ever given inside the bubblewrap sandbox (or Codex's own),
/// where it can write nothing but the worktree and the run's scratch and caches.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SandboxSettings {
    /// Builders get a shell, so they can run their tests and install what those need.
    pub shell: bool,
    /// Fetch tools may reach the package indexes (PyPI, npm, crates.io), and nothing else.
    pub package_indexes: bool,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        SandboxSettings { shell: true, package_indexes: true }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Seconds one agent call may run before it is stopped. 0 means no limit.
    pub timeout_secs: u64,
}

impl Default for AgentSettings {
    fn default() -> Self {
        AgentSettings { timeout_secs: DEFAULT_AGENT_SECS }
    }
}

/// Ten minutes, the limit a fresh install has.
pub const DEFAULT_AGENT_SECS: u64 = 600;

/// What "no limit" becomes: long enough never to matter, short enough to add to a clock.
pub const NO_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// The limit for `secs`, where 0 means none.
pub fn agent_limit(secs: u64) -> std::time::Duration {
    if secs == 0 { NO_LIMIT } else { std::time::Duration::from_secs(secs) }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TelemetrySettings {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CodeModeSettings {
    /// Workers get `run_code`, and Claude Code runs its native code mode (see `code.rs`).
    pub enabled: bool,
    /// Seconds one `run_code` script may run; 0 keeps the default (600). A script that runs
    /// the tests needs room.
    pub script_secs: u64,
    /// Seconds one `sh()` or `test()` call inside a script may run; 0 keeps the default (300).
    pub sh_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CheckerSettings {
    /// Give each finished page a second look from the mid model (see `checker.rs`).
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ApprovalSettings {
    /// `never` (the default: shell commands, network fetches and writes outside the
    /// worktree are refused), `risky` (ask before those), or `always` (ask before every
    /// tool call).
    pub mode: String,
    /// How long a question waits for an answer before it is refused.
    pub timeout_secs: u64,
}

impl Default for ApprovalSettings {
    fn default() -> Self {
        ApprovalSettings { mode: "never".into(), timeout_secs: 600 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchSettings {
    /// The JSON search endpoint; `{q}` stands for the query. `TASKMASTER_SEARCH_URL` overrides it.
    pub endpoint: String,
}

impl Default for SearchSettings {
    fn default() -> Self {
        SearchSettings { endpoint: "http://127.0.0.1:8888/search?format=json&q={q}".into() }
    }
}

impl Settings {
    /// The search endpoint: the environment first, then the settings file.
    pub fn search_endpoint(&self) -> String {
        match std::env::var("TASKMASTER_SEARCH_URL") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => self.search.endpoint.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PreviewSettings {
    /// The command that starts a dev server, in place of the one the project's
    /// `package.json` names. It gets the port as `$PORT`.
    pub command: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StoreSettings {
    /// `files` or `sqlite`.
    pub kind: String,
    /// The database file, relative to the project. Used by `sqlite`.
    pub path: String,
}

impl Default for StoreSettings {
    fn default() -> Self {
        StoreSettings { kind: "files".into(), path: ".taskmaster/taskmaster.db".into() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AuthSettings {
    /// The Commons service, whose sign-in tokens are accepted alongside paired devices.
    pub issuer: String,
    /// The accounts that may sign in here: `github:<login>`, `google:<email>`, or an
    /// email. Anyone can have an account with the service, so nobody is let in by default.
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CommonsSettings {
    /// The shared Commons: the Commons service, or a daemon acting as a hub.
    pub remote: String,
    /// A token made on the service's account page. Pulling works without one.
    pub token: String,
    /// Offer entries this machine promotes. Off unless set: an entry is project code.
    pub share: bool,
    /// Say how Commons entries did in this machine's runs, which is how their pass rates
    /// are earned. Only the entry and pass or fail are sent.
    pub report: bool,
}

impl Default for CommonsSettings {
    fn default() -> Self {
        CommonsSettings { remote: String::new(), token: String::new(), share: false, report: true }
    }
}

/// A value still as the example file has it.
pub fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || value.contains("example.invalid") || value.contains("replace-me")
}

impl Settings {
    /// The project's file if it has one, otherwise the one in the home directory.
    pub fn load(project: &Path) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".taskmaster/config.json"));
        for path in [Some(project.join(".taskmaster/config.json")), home].into_iter().flatten() {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            return serde_json::from_str(&text).with_context(|| format!("{} is not a settings file", path.display()));
        }
        Ok(Settings::default())
    }

    /// The role choices the person saved, and only those: no role is filled in for them.
    pub fn saved_roles(project: &Path) -> serde_json::Map<String, serde_json::Value> {
        let home = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".taskmaster/config.json"));
        for path in [Some(project.join(".taskmaster/config.json")), home].into_iter().flatten() {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(&text) else { return Default::default() };
            let Some(serde_json::Value::Object(roles)) = root.get("roles") else { return Default::default() };
            return roles
                .iter()
                .filter(|(role, value)| crate::Roles::NAMES.contains(&role.as_str()) && value.as_str().is_some_and(|v| !v.trim().is_empty()))
                .map(|(role, value)| (role.clone(), value.clone()))
                .collect();
        }
        Default::default()
    }

    pub fn approval_policy(&self) -> anyhow::Result<crate::approvals::Policy> {
        let mode = crate::approvals::Mode::parse(&self.approvals.mode)
            .with_context(|| format!("approvals.mode `{}` is not always, risky or never", self.approvals.mode))?;
        Ok(crate::approvals::Policy { mode, timeout: std::time::Duration::from_secs(self.approvals.timeout_secs.max(1)) })
    }

    /// `None` keeps the records as files under `.taskmaster/`.
    pub fn store(&self, project: &Path) -> anyhow::Result<Option<Arc<dyn Store>>> {
        match self.store.kind.as_str() {
            "files" | "" => Ok(None),
            "sqlite" => Ok(Some(Arc::new(SqliteStore::open(&project.join(&self.store.path))?))),
            other => bail!("unknown store `{other}` in the settings file: use `files` or `sqlite`"),
        }
    }

    pub fn records(&self, project: &Path) -> anyhow::Result<Records> {
        Ok(match self.store(project)? {
            Some(store) => Records::new(store),
            None => Records::files(project),
        })
    }

    /// Pairing always works. Accounts are added when the sign-in service is filled in.
    /// The second value is the pairing code to print.
    pub fn auth(&self, records: Records) -> anyhow::Result<(Arc<dyn Auth>, String)> {
        let pairing = Arc::new(PairingAuth::new(records)?);
        let code = pairing.code().to_string();
        if is_placeholder(&self.auth.issuer) {
            if !self.auth.issuer.trim().is_empty() {
                eprintln!("taskmaster: account sign-in is still a placeholder in the settings file; pairing only");
            }
            return Ok((pairing, code));
        }
        let issuer = self.auth.issuer.trim();
        if !issuer.starts_with("https://") && !issuer.starts_with("http://localhost") && !issuer.starts_with("http://127.0.0.1") {
            bail!("the sign-in service in the settings file must be https://");
        }
        let allow: Vec<String> = self.auth.allow.iter().filter(|entry| !is_placeholder(entry)).cloned().collect();
        if allow.is_empty() {
            eprintln!("taskmaster: `auth.allow` in the settings file is empty, so no account can sign in here");
        }
        let accounts = Arc::new(AccountAuth::new(issuer, allow));
        Ok((Arc::new(AnyAuth(vec![pairing, accounts])), code))
    }

    /// `None` keeps the Commons on this machine.
    pub fn remote(&self) -> Option<Arc<dyn CommonsRemote>> {
        if is_placeholder(&self.commons.remote) {
            if !self.commons.remote.trim().is_empty() {
                eprintln!("taskmaster: the shared Commons is still a placeholder in the settings file; using the local one");
            }
            return None;
        }
        let token = if is_placeholder(&self.commons.token) { "" } else { self.commons.token.trim() };
        if token.is_empty() && self.commons.share {
            eprintln!("taskmaster: `commons.share` is on but there is no token, so nothing will be shared");
        }
        let remote = HttpCommons::new(self.commons.remote.trim(), token)
            .sharing(self.commons.share && !token.is_empty())
            .reporting(self.commons.report);
        Some(Arc::new(remote))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_file_is_all_placeholders_and_changes_nothing() {
        let example = include_str!("../../../config.example.json");
        let settings: Settings = serde_json::from_str(example).unwrap();
        assert!(is_placeholder(&settings.auth.issuer) && settings.auth.allow.iter().all(|a| is_placeholder(a)));
        assert!(!settings.commons.share && settings.commons.report);
        assert!(is_placeholder(&settings.commons.remote) && is_placeholder(&settings.commons.token));
        assert!(settings.remote().is_none());
        let dir = std::env::temp_dir().join(format!("tm-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(settings.store(&dir).unwrap().is_none());
        let (auth, code) = settings.auth(Records::files(&dir)).unwrap();
        assert_eq!(auth.describe()["methods"], serde_json::json!(["pairing"]));
        assert_eq!(code.len(), 8);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filled_in_settings_turn_each_part_on() {
        let dir = std::env::temp_dir().join(format!("tm-settings-on-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".taskmaster")).unwrap();
        std::fs::write(
            dir.join(".taskmaster/config.json"),
            r#"{"store":{"kind":"sqlite"},
                "auth":{"issuer":"http://127.0.0.1:9","allow":["github:naren"]},
                "commons":{"remote":"http://127.0.0.1:9","token":"abc","share":true}}"#,
        )
        .unwrap();
        let settings = Settings::load(&dir).unwrap();
        let records = settings.records(&dir).unwrap();
        records.save_meta("r", &crate::store::Meta { parallel: 1, branch: None, base: None, template: None, focused: false }).unwrap();
        assert!(dir.join(".taskmaster/taskmaster.db").is_file());
        assert!(!dir.join(".taskmaster/runs").exists());
        let (auth, _) = settings.auth(records).unwrap();
        assert_eq!(auth.describe()["methods"], serde_json::json!(["pairing", "account"]));
        assert!(settings.remote().is_some());
        assert!(settings.commons.share && settings.commons.report);
        let plain = Settings { auth: AuthSettings { issuer: "http://auth.test".into(), allow: vec![] }, ..Settings::default() };
        assert!(plain.auth(Records::files(&dir)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
