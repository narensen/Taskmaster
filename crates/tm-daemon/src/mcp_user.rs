//! User-configured MCP servers offered per role under the approval policy (Item 4.4).
//!
//! Server definitions live in `.taskmaster/mcp_servers.json`. Any secret environment variables
//! (e.g. Sentry/Linear/DB tokens) are stored in [`crate::secrets::OsKeychain`] rather than
//! written into the JSON file or returned to the browser.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::secrets::SecretVault;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMcpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Non-secret environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Names of secret environment variables whose values live in the OS keychain.
    #[serde(default)]
    pub secret_env_keys: Vec<String>,
    /// Roles allowed to see this server (`planner`, `worker`, `mid`, `master`, `reviewer`, `verifier`).
    /// Empty means all roles except `verifier`.
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub require_approval: bool,
}

fn default_true() -> bool {
    true
}

fn store_path(project: &Path) -> PathBuf {
    project.join(".taskmaster").join("mcp_servers.json")
}

pub fn list(project: &Path) -> Vec<UserMcpServer> {
    let Ok(bytes) = std::fs::read(store_path(project)) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_all(project: &Path, servers: &[UserMcpServer]) -> anyhow::Result<()> {
    let path = store_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(servers)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Add or update a user MCP server, saving any provided `secret_env` entries into `vault`.
pub fn upsert(
    project: &Path,
    vault: &dyn SecretVault,
    mut server: UserMcpServer,
    secret_env: &BTreeMap<String, String>,
) -> anyhow::Result<UserMcpServer> {
    let name = server.name.trim().to_string();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')) {
        anyhow::bail!("MCP server name must be alphanumeric with `-` or `_`");
    }
    if server.command.trim().is_empty() {
        anyhow::bail!("MCP server command cannot be empty");
    }
    server.name = name.clone();
    for (k, v) in secret_env {
        if !v.trim().is_empty() {
            vault.set("taskmaster-mcp", &format!("{name}:{k}"), v)?;
            if !server.secret_env_keys.contains(k) {
                server.secret_env_keys.push(k.clone());
            }
        }
    }
    server.secret_env_keys.sort();
    server.secret_env_keys.dedup();
    let mut list = list(project);
    if let Some(existing) = list.iter_mut().find(|s| s.name == server.name) {
        *existing = server.clone();
    } else {
        list.push(server.clone());
    }
    save_all(project, &list)?;
    Ok(server)
}

pub fn remove(project: &Path, vault: &dyn SecretVault, name: &str) -> anyhow::Result<bool> {
    let mut servers = list(project);
    let before = servers.len();
    if let Some(existing) = servers.iter().find(|s| s.name == name) {
        for k in &existing.secret_env_keys {
            let _ = vault.delete("taskmaster-mcp", &format!("{name}:{k}"));
        }
    }
    servers.retain(|s| s.name != name);
    save_all(project, &servers)?;
    Ok(servers.len() != before)
}

/// Filter enabled MCP servers permitted for `role`.
pub fn servers_for_role(project: &Path, role: &str) -> Vec<UserMcpServer> {
    let role_l = role.trim().to_ascii_lowercase();
    list(project)
        .into_iter()
        .filter(|s| {
            if !s.enabled {
                return false;
            }
            if s.roles.is_empty() {
                role_l != "verifier"
            } else {
                s.roles.iter().any(|r| r.eq_ignore_ascii_case(&role_l))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::OsKeychain;

    #[test]
    fn filters_mcp_servers_by_role_and_keeps_secret_env_out_of_json() {
        let dir = std::env::temp_dir().join(format!("tm-mcp-user-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = OsKeychain::new(dir.join("vault.json"));

        let srv = UserMcpServer {
            name: "sentry".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@sentry/mcp-server".into()],
            env: BTreeMap::from([("SENTRY_ORG".into(), "acme".into())]),
            secret_env_keys: vec![],
            roles: vec!["planner".into(), "reviewer".into()],
            enabled: true,
            require_approval: true,
        };
        let secrets = BTreeMap::from([("SENTRY_AUTH_TOKEN".into(), "sntrys_fake_secret_999".into())]);
        let saved = upsert(&dir, &vault, srv, &secrets).unwrap();
        assert_eq!(saved.secret_env_keys, vec!["SENTRY_AUTH_TOKEN"]);

        let raw = std::fs::read_to_string(dir.join(".taskmaster/mcp_servers.json")).unwrap();
        assert!(!raw.contains("sntrys_fake_secret_999"));
        assert_eq!(servers_for_role(&dir, "planner").len(), 1);
        assert_eq!(servers_for_role(&dir, "worker").len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
