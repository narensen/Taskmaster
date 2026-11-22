//! Desktop and webhook notifications for approvals, limits, and finished runs (Item 4.5).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    pub desktop: bool,
    /// Optional HTTPS webhook URL (localhost permitted in tests).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    pub on_approval: bool,
    pub on_limit: bool,
    pub on_finish: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            desktop: false,
            webhook_url: None,
            on_approval: true,
            on_limit: true,
            on_finish: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    ApprovalWaiting,
    LimitHit,
    RunFinished,
}

impl NotifyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApprovalWaiting => "approval_waiting",
            Self::LimitHit => "limit_hit",
            Self::RunFinished => "run_finished",
        }
    }
}

fn config_path(project: &Path) -> PathBuf {
    project.join(".taskmaster").join("notifications.json")
}

pub fn load(project: &Path) -> NotificationConfig {
    std::fs::read(config_path(project))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(project: &Path, cfg: &NotificationConfig) -> anyhow::Result<()> {
    if let Some(ref url) = cfg.webhook_url {
        let u = url.trim();
        if !(u.is_empty()
            || u.starts_with("https://")
            || u.starts_with("http://127.0.0.1")
            || u.starts_with("http://localhost"))
        {
            anyhow::bail!("webhook_url must start with https://");
        }
    }
    let path = config_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Dispatch desktop and/or webhook notification if enabled for `kind`.
pub async fn dispatch(
    cfg: &NotificationConfig,
    kind: NotifyKind,
    run_id: &str,
    title: &str,
    detail: &str,
) -> anyhow::Result<bool> {
    let enabled = match kind {
        NotifyKind::ApprovalWaiting => cfg.on_approval,
        NotifyKind::LimitHit => cfg.on_limit,
        NotifyKind::RunFinished => cfg.on_finish,
    };
    if !enabled {
        return Ok(false);
    }
    let mut sent = false;
    if cfg.desktop {
        send_desktop(title, detail);
        sent = true;
    }
    if let Some(url) = cfg.webhook_url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        let payload = json!({
            "event": kind.as_str(),
            "run_id": run_id,
            "title": title,
            "detail": crate::secrets::redact_text(detail),
            "at": crate::time::rfc3339_now(),
        });
        let url = url.to_string();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
            agent.post(&url).send_json(payload).map_err(|err| match err {
                ureq::Error::Status(code, _) => anyhow::anyhow!("webhook returned HTTP {code}"),
                ureq::Error::Transport(t) => anyhow::anyhow!("webhook request failed: {t}"),
            })?;
            Ok(())
        })
        .await??;
        sent = true;
    }
    Ok(sent)
}

fn send_desktop(title: &str, detail: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("notify-send")
            .args([title, detail])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {:?} with title {:?}",
            detail, title
        );
        let _ = Command::new("osascript")
            .args(["-e", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn webhook_receives_redacted_notification_payload() {
        let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
        let slot = received.clone();
        let app = Router::new().route(
            "/hook",
            post(move |Json(body): Json<serde_json::Value>| async move {
                slot.lock().unwrap().push(body);
                "ok"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = NotificationConfig {
            desktop: false,
            webhook_url: Some(format!("http://{addr}/hook")),
            on_approval: true,
            on_limit: true,
            on_finish: true,
        };
        let sent = dispatch(
            &cfg,
            NotifyKind::RunFinished,
            "run-7",
            "Run finished",
            "Completed with token Bearer secret_token_1234567890",
        )
        .await
        .unwrap();
        assert!(sent);

        let items = received.lock().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["event"], "run_finished");
        assert_eq!(items[0]["run_id"], "run-7");
        assert!(!items[0]["detail"].as_str().unwrap().contains("secret_token_1234567890"));
        srv.abort();
    }
}
