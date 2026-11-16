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
    /// The sign-in service whose tokens are accepted, alongside paired devices.
    pub issuer: String,
    /// The secret that service signs with.
    pub secret: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CommonsSettings {
    /// A Taskmaster daemon that holds the shared Commons, such as `https://commons.example`.
    pub remote: String,
    /// A token that daemon issued.
    pub token: String,
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
        if is_placeholder(&self.auth.issuer) || is_placeholder(&self.auth.secret) {
            if !self.auth.issuer.trim().is_empty() || !self.auth.secret.trim().is_empty() {
                eprintln!("taskmaster: account sign-in is still a placeholder in the settings file; pairing only");
            }
            return Ok((pairing, code));
        }
        if self.auth.secret.len() < 32 {
            bail!("the sign-in secret in the settings file is shorter than 32 characters");
        }
        let accounts = Arc::new(AccountAuth::new(self.auth.issuer.trim(), &self.auth.secret));
        Ok((Arc::new(AnyAuth(vec![pairing, accounts])), code))
    }

    /// `None` keeps the Commons on this machine.
    pub fn remote(&self) -> Option<Arc<dyn CommonsRemote>> {
        if is_placeholder(&self.commons.remote) || is_placeholder(&self.commons.token) {
            if !self.commons.remote.trim().is_empty() {
                eprintln!("taskmaster: the shared Commons is still a placeholder in the settings file; using the local one");
            }
            return None;
        }
        Some(Arc::new(HttpCommons::new(self.commons.remote.trim(), self.commons.token.trim())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_file_is_all_placeholders_and_changes_nothing() {
        let example = include_str!("../../../config.example.json");
        let settings: Settings = serde_json::from_str(example).unwrap();
        assert!(is_placeholder(&settings.auth.issuer) && is_placeholder(&settings.auth.secret));
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
                "auth":{"issuer":"https://auth.test","secret":"0123456789abcdef0123456789abcdef"},
                "commons":{"remote":"http://127.0.0.1:9","token":"abc"}}"#,
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
        let short = Settings { auth: AuthSettings { issuer: "https://auth.test".into(), secret: "short".into() }, ..Settings::default() };
        assert!(short.auth(Records::files(&dir)).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
