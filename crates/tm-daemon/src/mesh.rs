//! Remote Runner Mesh & Environment registry (Items 3.13 & 4.8).
//!
//! Manages remote headless `tm-daemon` nodes (over HTTPS / Tailscale / mTLS) for worktree
//! offloading (3.13) and remote daemon environment switching (4.8). Pairing tokens are stored
//! through [`crate::secrets::OsKeychain`] and are NEVER returned in API JSON responses.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::secrets::{OsKeychain, SecretVault};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNode {
    pub id: String,
    pub name: String,
    /// `https://host:port` or `http://127.0.0.1:port` (for local tests).
    pub url: String,
    /// `runner` (offload worktrees) or `environment` (switchable remote daemon).
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_slots")]
    pub max_parallel: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Always computed dynamically — never contains the secret token itself.
    #[serde(default)]
    pub has_token: bool,
}

fn default_kind() -> String {
    "runner".into()
}
fn default_slots() -> usize {
    4
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeHealth {
    pub id: String,
    pub reachable: bool,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn store_path(project: &Path, filename: &str) -> PathBuf {
    project.join(".taskmaster").join(filename)
}

pub fn list_nodes(project: &Path, filename: &str) -> Vec<RemoteNode> {
    let path = store_path(project, filename);
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_nodes(project: &Path, filename: &str, nodes: &[RemoteNode]) -> anyhow::Result<()> {
    let path = store_path(project, filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(nodes)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn upsert_node(
    project: &Path,
    filename: &str,
    vault: &dyn SecretVault,
    mut node: RemoteNode,
    token: Option<&str>,
) -> anyhow::Result<RemoteNode> {
    if node.name.trim().is_empty() {
        anyhow::bail!("node name cannot be empty");
    }
    let url = node.url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost")) {
        anyhow::bail!("remote URL must use https:// (or localhost for testing)");
    }
    node.url = url.trim_end_matches('/').to_string();
    if node.id.trim().is_empty() {
        node.id = crate::secret::random_hex(5)?;
    }
    if let Some(tok) = token.map(str::trim).filter(|t| !t.is_empty()) {
        vault.set("taskmaster-node", &node.id, tok)?;
        node.has_token = true;
    } else {
        node.has_token = vault.get("taskmaster-node", &node.id)?.is_some();
    }
    let mut nodes = list_nodes(project, filename);
    if let Some(existing) = nodes.iter_mut().find(|n| n.id == node.id) {
        *existing = node.clone();
    } else {
        nodes.push(node.clone());
    }
    save_nodes(project, filename, &nodes)?;
    Ok(node)
}

pub fn remove_node(
    project: &Path,
    filename: &str,
    vault: &dyn SecretVault,
    id: &str,
) -> anyhow::Result<bool> {
    let mut nodes = list_nodes(project, filename);
    let before = nodes.len();
    nodes.retain(|n| n.id != id);
    let _ = vault.delete("taskmaster-node", id);
    save_nodes(project, filename, &nodes)?;
    Ok(nodes.len() != before)
}

/// Probe a remote `tm-daemon` node's `/api/health` endpoint.
pub async fn probe_node(node: &RemoteNode) -> NodeHealth {
    let start = Instant::now();
    let id = node.id.clone();
    let url = format!("{}/api/health", node.url.trim_end_matches('/'));
    let outcome = tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(3)).build();
        match agent.get(&url).call() {
            Ok(resp) => {
                let body: serde_json::Value = resp.into_json().unwrap_or_default();
                Ok(body["version"].as_str().map(str::to_string))
            }
            Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
            Err(ureq::Error::Transport(err)) => Err(err.to_string()),
        }
    })
    .await;
    let latency_ms = start.elapsed().as_millis() as u64;
    match outcome {
        Ok(Ok(version)) => NodeHealth {
            id,
            reachable: true,
            latency_ms,
            version,
            error: None,
        },
        Ok(Err(err)) => NodeHealth {
            id,
            reachable: false,
            latency_ms,
            version: None,
            error: Some(err),
        },
        Err(err) => NodeHealth {
            id,
            reachable: false,
            latency_ms,
            version: None,
            error: Some(err.to_string()),
        },
    }
}

pub fn default_vault() -> OsKeychain {
    OsKeychain::new(OsKeychain::default_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_upsert_stores_token_in_vault_and_never_serializes_it() {
        let dir = std::env::temp_dir().join(format!("tm-mesh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = OsKeychain::new(dir.join("vault.json"));

        let saved = upsert_node(
            &dir,
            "runners.json",
            &vault,
            RemoteNode {
                id: "gpu-1".into(),
                name: "Linux Build Box".into(),
                url: "https://build.ts.net:7420".into(),
                kind: "runner".into(),
                labels: vec!["linux".into(), "rust".into()],
                max_parallel: 8,
                enabled: true,
                has_token: false,
            },
            Some("secret-pairing-token-123456"),
        )
        .unwrap();
        assert!(saved.has_token);

        let raw = std::fs::read_to_string(dir.join(".taskmaster/runners.json")).unwrap();
        assert!(!raw.contains("secret-pairing-token-123456"));
        assert_eq!(list_nodes(&dir, "runners.json").len(), 1);
        assert!(remove_node(&dir, "runners.json", &vault, "gpu-1").unwrap());
        assert!(list_nodes(&dir, "runners.json").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
