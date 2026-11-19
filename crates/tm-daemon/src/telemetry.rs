//! Anonymous counts. Off until the settings file says otherwise.
//!
//! A prompt, a path, and a file are never included. When it is on, the daemon writes
//! one line to its log. It sends that count only when `TASKMASTER_TELEMETRY_URL` is an
//! `https://` address. With no address, nothing leaves the machine.

use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::settings::Settings;

pub fn record(project: &Path, kind: &str) {
    let settings = Settings::load(project).unwrap_or_default();
    if !settings.telemetry.enabled {
        return;
    }
    let kind = kind.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect::<String>();
    if kind.is_empty() {
        return;
    }
    crate::log::note(&format!("telemetry {kind}"));
    let Ok(url) = std::env::var("TASKMASTER_TELEMETRY_URL") else { return };
    if !url.starts_with("https://") || url.len() > 300 {
        return;
    }
    let body = json!({ "kind": kind, "version": env!("CARGO_PKG_VERSION") }).to_string();
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(5)).build();
        let _ = agent.post(&url).set("content-type", "application/json").set("user-agent", "taskmaster").send_string(&body);
    });
}
