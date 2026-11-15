//! Read-only MCP server (streamable HTTP, JSON responses). Tools: `cite` and `index`.
//! Every call is authenticated by a run-scoped bearer token and recorded, which is what
//! builds the citation graph from actual use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::store::LocalStore;

#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub run: String,
    pub page: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CiteRecord {
    pub grant: Grant,
    pub key: String,
    pub found: bool,
}

pub struct McpState {
    store: LocalStore,
    tokens: Mutex<HashMap<String, Grant>>,
    cites: Mutex<Vec<CiteRecord>>,
}

impl McpState {
    pub fn new(store: LocalStore) -> Arc<Self> {
        Arc::new(McpState { store, tokens: Mutex::default(), cites: Mutex::default() })
    }

    pub fn grant(&self, token: &str, grant: Grant) {
        self.tokens.lock().unwrap().insert(token.into(), grant);
    }

    pub fn revoke(&self, token: &str) {
        self.tokens.lock().unwrap().remove(token);
    }

    pub fn cites(&self) -> Vec<CiteRecord> {
        self.cites.lock().unwrap().clone()
    }
}

pub fn router(state: Arc<McpState>) -> Router {
    Router::new().route("/mcp", post(handle).get(no_stream)).with_state(state)
}

/// This server never pushes, so it offers no SSE stream.
async fn no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn handle(State(state): State<Arc<McpState>>, headers: HeaderMap, body: String) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let grant = token.and_then(|t| state.tokens.lock().unwrap().get(t).cloned());
    let Some(grant) = grant else { return StatusCode::UNAUTHORIZED.into_response() };
    let Ok(msg) = serde_json::from_str::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(id) = msg.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response(); // notification
    };
    let reply = match dispatch(&state, &grant, &msg) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    };
    Json(reply).into_response()
}

fn dispatch(state: &McpState, grant: &Grant, msg: &Value) -> Result<Value, (i32, String)> {
    let params = &msg["params"];
    match msg["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"],
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "taskmaster-dictionary", "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": [
            { "name": "cite", "description": "Read a Dictionary entry by key. Read-only.",
              "inputSchema": { "type": "object", "properties": { "key": { "type": "string" } },
                               "required": ["key"] } },
            { "name": "index", "description": "List every key with its one-line summary.",
              "inputSchema": { "type": "object", "properties": {} } },
        ]})),
        "tools/call" => call_tool(state, grant, params),
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

fn call_tool(state: &McpState, grant: &Grant, params: &Value) -> Result<Value, (i32, String)> {
    let text = |s: String, is_error: bool| json!({ "content": [{ "type": "text", "text": s }], "isError": is_error });
    match params["name"].as_str().unwrap_or_default() {
        "cite" => {
            let key = params["arguments"]["key"].as_str().unwrap_or_default();
            let entry = state.store.get(key).map_err(|e| (-32602, e.to_string()))?;
            state.cites.lock().unwrap().push(CiteRecord {
                grant: grant.clone(),
                key: key.into(),
                found: entry.is_some(),
            });
            Ok(match entry {
                Some(e) => text(e.content, false),
                None => text(format!("no such key: {key}"), true),
            })
        }
        "index" => {
            let index = state.store.index().map_err(|e| (-32603, e.to_string()))?;
            Ok(text(serde_json::to_string(&index).unwrap(), false))
        }
        other => Err((-32602, format!("unknown tool: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tm_core::EntryKind;
    use tower::ServiceExt;

    async fn rpc(app: &Router, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        let mut req = Request::post("/mcp").header("content-type", "application/json");
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn setup() -> (Arc<McpState>, Router) {
        let dir = std::env::temp_dir().join(format!("tm-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = LocalStore::open(dir).unwrap();
        store.publish("greeting_contract", EntryKind::Hint, "greeting", "export const G = 1;", "x").unwrap();
        let state = McpState::new(store);
        state.grant("tok-a", Grant { run: "r1".into(), page: "dashboard".into() });
        let app = router(state.clone());
        (state, app)
    }

    fn call(key: &str) -> Value {
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "cite", "arguments": { "key": key } } })
    }

    #[tokio::test]
    async fn cite_returns_content_and_is_logged_per_page() {
        let (state, app) = setup();
        let (status, reply) = rpc(&app, Some("tok-a"), call("greeting_contract")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reply["result"]["content"][0]["text"], "export const G = 1;");
        let (_, miss) = rpc(&app, Some("tok-a"), call("nope")).await;
        assert_eq!(miss["result"]["isError"], true);
        let cites = state.cites();
        assert_eq!(cites.len(), 2);
        assert_eq!(cites[0].grant.page, "dashboard");
        assert!(cites[0].found && !cites[1].found);
    }

    #[tokio::test]
    async fn rejects_missing_unknown_and_revoked_tokens() {
        let (state, app) = setup();
        assert_eq!(rpc(&app, None, call("greeting_contract")).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(rpc(&app, Some("bad"), call("greeting_contract")).await.0, StatusCode::UNAUTHORIZED);
        state.revoke("tok-a");
        assert_eq!(rpc(&app, Some("tok-a"), call("greeting_contract")).await.0, StatusCode::UNAUTHORIZED);
        assert!(state.cites().is_empty());
    }

    #[tokio::test]
    async fn lists_tools_and_answers_the_handshake() {
        let (_, app) = setup();
        let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                           "params": { "protocolVersion": "2025-11-25" } });
        let (_, r) = rpc(&app, Some("tok-a"), init).await;
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        let (_, r) = rpc(&app, Some("tok-a"), json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 2);
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(rpc(&app, Some("tok-a"), note).await.0, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn path_traversal_keys_are_refused() {
        let (_, app) = setup();
        let (_, r) = rpc(&app, Some("tok-a"), call("../../etc/passwd")).await;
        assert!(r["error"].is_object());
    }
}
