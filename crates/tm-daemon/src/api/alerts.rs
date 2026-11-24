//! Phone notifications: Web Push subscriptions, ntfy, and alerts when a run needs a person.
//!
//! ```text
//! GET  /api/push                  VAPID public key, subscriptions, and ntfy configuration
//! POST /api/push/subscribe        {endpoint, p256dh, auth, label?} save a browser subscription
//! POST /api/push/unsubscribe      {endpoint} forget a browser subscription
//! POST /api/push/ntfy             {on, server?} turn ntfy on with a private topic, or off
//! POST /api/push/test             send one test notification to every configured channel
//! ```

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, App, Reply, fail, ok};
use crate::push::{Note, Push, Subscription};

fn push_of(app: &App) -> Result<Arc<Push>, ApiError> {
    app.push
        .clone()
        .ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "phone notifications are off on this daemon"))
}

fn view_json(push: Option<&Push>) -> Value {
    match push {
        Some(push) => {
            let mut view = push.view();
            view["enabled"] = json!(true);
            view
        }
        None => json!({
            "enabled": false,
            "public_key": "",
            "subscriptions": [],
            "ntfy": Value::Null,
        }),
    }
}

impl App {
    /// Forward every approval question raised by a run to configured phone notification channels.
    pub(super) fn spawn_alerts(self: &Arc<Self>) {
        if self.push.is_none() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
        self.live.set_alerts(tx);
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some((run_id, question)) = rx.recv().await {
                let Some(app) = weak.upgrade() else { return };
                let Some(push) = app.push.clone() else { continue };
                let tool = question["tool"].as_str().unwrap_or("tool").trim();
                let reason = question["reason"].as_str().unwrap_or("").trim();
                let body = if reason.is_empty() {
                    format!("Needs approval for {tool}.")
                } else {
                    let short: String = reason.chars().take(140).collect();
                    format!("Needs approval for {tool}: {short}")
                };
                let note = Note {
                    title: format!("Taskmaster · {run_id}"),
                    body,
                    path: format!("/runs/{run_id}"),
                    tag: format!("run-{run_id}"),
                };
                let base = super::connect::phone_base(&app).await;
                let sent = push.send(&note, &base).await;
                for err in sent.errors {
                    crate::log::note(&format!("phone notification failed: {err}"));
                }
            }
        });
    }
}

/// `GET /api/push`: VAPID public key, saved browser subscriptions, and ntfy topic.
pub(super) async fn push_view(State(app): State<Arc<App>>) -> Reply {
    ok(view_json(app.push.as_deref()))
}

#[derive(Deserialize)]
pub(super) struct SubscribeBody {
    endpoint: String,
    p256dh: String,
    auth: String,
    #[serde(default)]
    label: String,
}

/// `POST /api/push/subscribe`: save a browser's Web Push subscription.
pub(super) async fn subscribe(State(app): State<Arc<App>>, Json(body): Json<SubscribeBody>) -> Reply {
    let push = push_of(&app)?;
    push.subscribe(Subscription {
        endpoint: body.endpoint,
        p256dh: body.p256dh,
        auth: body.auth,
        label: body.label,
        added: String::new(),
    })
    .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(view_json(Some(&push)))
}

#[derive(Deserialize)]
pub(super) struct UnsubscribeBody {
    endpoint: String,
}

/// `POST /api/push/unsubscribe`: remove a saved Web Push subscription.
pub(super) async fn unsubscribe(State(app): State<Arc<App>>, Json(body): Json<UnsubscribeBody>) -> Reply {
    let push = push_of(&app)?;
    let removed = push
        .unsubscribe(&body.endpoint)
        .map_err(|err| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    let mut view = view_json(Some(&push));
    view["removed"] = json!(removed);
    ok(view)
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct NtfyBody {
    on: bool,
    server: Option<String>,
}

/// `POST /api/push/ntfy`: enable ntfy with a freshly generated private topic, or disable it.
pub(super) async fn set_ntfy(State(app): State<Arc<App>>, Json(body): Json<NtfyBody>) -> Reply {
    let push = push_of(&app)?;
    push.set_ntfy(body.on, body.server.as_deref())
        .map_err(|err| fail(StatusCode::BAD_REQUEST, format!("{err:#}")))?;
    ok(view_json(Some(&push)))
}

/// `POST /api/push/test`: send one test notification to every configured channel.
pub(super) async fn test(State(app): State<Arc<App>>) -> Reply {
    let push = push_of(&app)?;
    let base = super::connect::phone_base(&app).await;
    let note = Note {
        title: "Taskmaster".into(),
        body: "Notifications from this daemon reach this device.".into(),
        path: "/connect".into(),
        tag: "taskmaster-test".into(),
    };
    let sent = push.send(&note, &base).await;
    ok(json!(sent))
}
