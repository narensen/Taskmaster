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
        records.save_meta("r", &crate::store::Meta { parallel: 1, branch: None, base: None, template: None }).unwrap();
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
