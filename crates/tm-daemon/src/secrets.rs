//! OS keychain storage and universal runtime secret redaction (Item 3.1).
//!
//! Host tokens and API keys are stored in the OS keychain (`secret-tool` on Linux
//! Secret Service, `security` on macOS Keychain) whenever available, falling back cleanly
//! to a mode-`0600` file on headless machines without a desktop keyring. Any registered
//! secret value plus common token patterns (`sk-…`, `ghp_…`, `Bearer …`) are scrubbed to
//! `•••` across transcripts, logs, terminal output, and exports.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, RwLock};

use serde_json::Value;

static RUNTIME_SECRETS: LazyLock<RwLock<BTreeSet<String>>> = LazyLock::new(|| RwLock::new(BTreeSet::new()));

/// Register a known secret value in memory so every transcript, log, and export scrubs it.
pub fn register_secret(secret: &str) {
    let trimmed = secret.trim();
    if trimmed.len() >= 6 {
        RUNTIME_SECRETS.write().unwrap().insert(trimmed.to_string());
    }
}

/// Snapshot of registered runtime secrets.
pub fn registered_secrets() -> Vec<String> {
    RUNTIME_SECRETS.read().unwrap().iter().cloned().collect()
}

/// Scrub all registered runtime secrets and common token patterns from `input`.
pub fn redact_text(input: &str) -> String {
    let extras = registered_secrets();
    crate::policy::redact_text(input, &extras)
}

/// Scrub all registered runtime secrets and common token patterns in-place inside JSON.
pub fn redact_json(value: &mut Value) {
    let extras = registered_secrets();
    crate::policy::redact_json(value, &extras);
}

/// Abstraction over secret storage (OS keychain or mode-0600 file fallback).
pub trait SecretVault: Send + Sync {
    fn backend_name(&self) -> &'static str;
    fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()>;
    fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<String>>;
    fn delete(&self, service: &str, account: &str) -> anyhow::Result<()>;
}

/// Attempts the platform keychain (`secret-tool` on Linux, `security` on macOS) and falls
/// back transparently to a mode-0600 JSON map on headless environments.
pub struct OsKeychain {
    fallback_file: PathBuf,
}

impl OsKeychain {
    pub fn new(fallback_file: PathBuf) -> Self {
        Self { fallback_file }
    }

    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join(".taskmaster").join("vault.json")
    }

    fn has_secret_tool() -> bool {
        #[cfg(target_os = "linux")]
        {
            Command::new("secret-tool")
                .arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    fn has_macos_security() -> bool {
        #[cfg(target_os = "macos")]
        {
            Command::new("security")
                .arg("-h")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    fn load_fallback(&self) -> std::collections::BTreeMap<String, String> {
        std::fs::read(&self.fallback_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_fallback(&self, map: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
        if let Some(parent) = self.fallback_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(map)?;
        write_private(&self.fallback_file, &bytes)
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

impl SecretVault for OsKeychain {
    fn backend_name(&self) -> &'static str {
        if Self::has_secret_tool() {
            "linux-secret-service"
        } else if Self::has_macos_security() {
            "macos-keychain"
        } else {
            "file-0600"
        }
    }

    fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        register_secret(secret);
        if Self::has_secret_tool() {
            let child = Command::new("secret-tool")
                .args(["store", "--label", &format!("Taskmaster {service}:{account}"), "service", service, "account", account])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = child {
                if let Some(mut stdin) = child.stdin.take() {
                    use std::io::Write;
                    let _ = stdin.write_all(secret.as_bytes());
                }
                if child.wait().is_ok_and(|s| s.success()) {
                    return Ok(());
                }
            }
        }
        if Self::has_macos_security() {
            let ok = Command::new("security")
                .args(["add-generic-password", "-U", "-s", service, "-a", account, "-w", secret])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                return Ok(());
            }
        }
        let mut map = self.load_fallback();
        map.insert(format!("{service}:{account}"), secret.to_string());
        self.save_fallback(&map)
    }

    fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<String>> {
        if Self::has_secret_tool()
            && let Ok(out) = Command::new("secret-tool")
                .args(["lookup", "service", service, "account", account])
                .output()
            && out.status.success()
        {
            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !val.is_empty() {
                register_secret(&val);
                return Ok(Some(val));
            }
        }
        if Self::has_macos_security()
            && let Ok(out) = Command::new("security")
                .args(["find-generic-password", "-w", "-s", service, "-a", account])
                .output()
            && out.status.success()
        {
            let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !val.is_empty() {
                register_secret(&val);
                return Ok(Some(val));
            }
        }
        let map = self.load_fallback();
        let val = map.get(&format!("{service}:{account}")).cloned();
        if let Some(ref s) = val {
            register_secret(s);
        }
        Ok(val)
    }

    fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
        if Self::has_secret_tool() {
            let _ = Command::new("secret-tool")
                .args(["clear", "service", service, "account", account])
                .status();
        }
        if Self::has_macos_security() {
            let _ = Command::new("security")
                .args(["delete-generic-password", "-s", service, "-a", account])
                .status();
        }
        let mut map = self.load_fallback();
        if map.remove(&format!("{service}:{account}")).is_some() {
            self.save_fallback(&map)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_secret_is_redacted_in_text_and_json_and_stored_with_0600() {
        let dir = std::env::temp_dir().join(format!("tm-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vault_path = dir.join("vault.json");
        let vault = OsKeychain::new(vault_path.clone());
        let fake_val = "tm_test_synthetic_secret_value_998877";
        vault.save_fallback(&std::collections::BTreeMap::from([("s:a".into(), fake_val.into())])).unwrap();
        register_secret(fake_val);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&vault_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let scrubbed = redact_text(&format!("printed {fake_val} and Bearer eyJhbGciOiJIUzI1NiJ9.abcdef"));
        assert!(!scrubbed.contains(fake_val));
        assert!(scrubbed.contains("•••"));

        let mut val = serde_json::json!({ "text": format!("leaked {fake_val}") });
        redact_json(&mut val);
        assert_eq!(val["text"], "leaked •••");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
