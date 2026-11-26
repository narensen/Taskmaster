//! Local `.flow` code-graph MCP extension (`flow_query`).
//!
//! Kept separate from [`crate::web_access::WebTools`] so local graph queries do not
//! live inside the web/SSRF module.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde_json::{Value, json};
use tm_dictionary::{Extension, Grant};

use crate::live::Live;
use crate::web_access::{WebRole, mcp_err, mcp_ok};

/// Daemon MCP extension exposing `flow_query` against the active repository tree's `.flow/` graph.
pub struct FlowTools {
    flow_root: RwLock<PathBuf>,
    live: Arc<Live>,
}

impl FlowTools {
    pub fn new(project: PathBuf, live: Arc<Live>) -> Arc<Self> {
        Arc::new(Self {
            flow_root: RwLock::new(project),
            live,
        })
    }

    /// Update the active tree root used by `flow_query` (e.g. the run's branch worktree).
    pub fn set_flow_root(&self, root: PathBuf) {
        *self.flow_root.write().unwrap() = root;
    }

    async fn handle_flow_query(&self, grant: &Grant, args: &Value) -> Value {
        let agent = WebRole::agent_tag(grant);
        let raw_op = args["op"].as_str().unwrap_or("").trim();
        let op = if raw_op.is_empty() && args.get("queries").and_then(Value::as_array).is_some() {
            "batch".to_string()
        } else {
            raw_op.to_string()
        };
        if op.is_empty() {
            return mcp_err("flow_query requires `op`: one of `find`, `node`, `neighbors`, `source`, or `batch` (with `queries`).");
        }
        let batch_len = args.get("queries").and_then(Value::as_array).map(Vec::len);
        let root = self.flow_root.read().unwrap().clone();
        let args_owned = args.clone();
        let op_for_task = op.clone();
        let result = tokio::task::spawn_blocking(move || {
            if !root.join(tm_flow::FLOW_DIR).is_dir() {
                let _ = tm_flow::build(&root);
            }
            tm_flow::execute_op(&root, &op_for_task, &args_owned)
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let mut entry = json!({
                    "op": op,
                    "id": args.get("id").and_then(|v| v.as_str()),
                    "query": args.get("query").and_then(|v| v.as_str()),
                    "reply_bytes": output.text.len(),
                });
                if let Some(n) = batch_len {
                    entry["batch_len"] = json!(n);
                }
                self.live.push(&grant.run, &agent, "flow_query", entry);
                mcp_ok(&output.text)
            }
            Ok(Err(err)) => mcp_err(&format!("flow_query failed: {err:#}")),
            Err(err) => mcp_err(&format!("flow_query task failed: {err}")),
        }
    }
}

#[async_trait::async_trait]
impl Extension for FlowTools {
    fn tools(&self, _grant: &Grant) -> Vec<Value> {
        vec![json!({
            "name": "flow_query",
            "description": "Query the deterministic .flow code graph instead of reading whole files. Ops: `find` (search symbols), `node` (signature/doc/edges), `neighbors` (k-hop subgraph), `source` (full symbol body span), `batch` (run up to 10 ops in `queries` in one call). `node` and `source` also accept `ids: [...]`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "op": { "type": "string", "enum": ["find", "node", "neighbors", "source", "batch"], "description": "Operation to run (defaults to 'batch' when `queries` is set)" },
                    "query": { "type": "string", "description": "Search text for op='find'" },
                    "id": { "type": "string", "description": "Node id (e.g. py:pkg/mod.py#Class.method) for op='node'|'neighbors'|'source'" },
                    "ids": { "type": "array", "items": { "type": "string" }, "description": "Multiple node ids for op='node'|'source' in one call" },
                    "queries": { "type": "array", "items": { "type": "object" }, "description": "Array of sub-op objects ({op, id?, query?, k?, kinds?, dir?, limit?}) for op='batch'" },
                    "k": { "type": "integer", "description": "Hop depth (1-3, default 1) for op='neighbors'" },
                    "kinds": { "type": "array", "items": { "type": "string" }, "description": "Optional edge kind filter (imports, calls, inherits, implements, owns, contract_of, tests)" },
                    "dir": { "type": "string", "enum": ["both", "out", "in"], "description": "Edge direction for op='neighbors' (default 'both')" },
                    "limit": { "type": "integer", "description": "Max nodes returned (1-40, default 40)" }
                }
            }
        })]
    }

    async fn call(&self, grant: &Grant, name: &str, args: &Value) -> Option<Value> {
        match tm_dictionary::strip_dict_prefix(name) {
            "flow_query" => Some(self.handle_flow_query(grant, args).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileStore;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tm-flow-tools-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn flow_query_mcp_tool_serves_find_node_neighbors_source_with_caps_for_all_roles() {
        let dir = temp_project("flow-query");
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(
            dir.join("pkg/store.py"),
            "class Store:\n    \"\"\"Persists items.\"\"\"\n    def put(self, key: str) -> None:\n        pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pkg/app.py"),
            "from pkg.store import Store\n\ndef handle(s: Store) -> None:\n    \"\"\"Handle request.\"\"\"\n    s.put('k')\n",
        )
        .unwrap();
        tm_flow::build(&dir).unwrap();

        let store = Arc::new(FileStore::in_project(&dir));
        let live = Arc::new(Live::new(store));
        let tools = FlowTools::new(dir.clone(), live);

        let worker = Grant { run: "r-fq".into(), page: "app".into(), unit: String::new() };
        let reviewer = Grant { run: "r-fq".into(), page: "reviewer".into(), unit: String::new() };
        for g in [&worker, &reviewer] {
            let listed = tools.tools(g);
            let names: Vec<&str> = listed.iter().filter_map(|t| t["name"].as_str()).collect();
            assert_eq!(names, vec!["flow_query"], "FlowTools must expose only flow_query for {:?}", g.page);
        }

        let found = tools
            .call(&reviewer, "flow_query", &json!({ "op": "find", "query": "handle" }))
            .await
            .unwrap();
        assert_eq!(found["isError"], false, "{found}");
        let ftext = found["content"][0]["text"].as_str().unwrap();
        assert!(ftext.contains("py:pkg/app.py#handle"), "{ftext}");

        let node = tools
            .call(&worker, "mcp_dictionary_flow_query", &json!({ "op": "node", "id": "py:pkg/app.py#handle" }))
            .await
            .unwrap();
        assert_eq!(node["isError"], false, "{node}");
        let ntext = node["content"][0]["text"].as_str().unwrap();
        assert!(ntext.contains("py:pkg/store.py#Store.put"), "{ntext}");

        let nbrs = tools
            .call(&worker, "flow_query", &json!({ "op": "neighbors", "id": "py:pkg/app.py#handle", "k": 1, "kinds": ["calls"], "dir": "out" }))
            .await
            .unwrap();
        assert_eq!(nbrs["isError"], false, "{nbrs}");
        assert!(nbrs["content"][0]["text"].as_str().unwrap().contains("Store.put"));

        let src = tools
            .call(&reviewer, "flow_query", &json!({ "op": "source", "id": "py:pkg/store.py#Store.put" }))
            .await
            .unwrap();
        assert_eq!(src["isError"], false, "{src}");
        let stext = src["content"][0]["text"].as_str().unwrap();
        assert!(stext.contains("def put(self, key: str)"), "{stext}");
        assert!(!stext.contains("class Store:"), "source must slice only the node span: {stext}");
        assert!(stext.len() <= tm_flow::MAX_QUERY_CHARS);

        // Batched op runs multiple queries in a single MCP call (both explicit op='batch' and omitted op).
        let batched = tools
            .call(
                &worker,
                "flow_query",
                &json!({
                    "queries": [
                        { "op": "find", "query": "Store" },
                        { "op": "source", "id": "py:pkg/app.py#handle" }
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(batched["isError"], false, "{batched}");
        let btext = batched["content"][0]["text"].as_str().unwrap();
        assert!(btext.contains("py:pkg/store.py#Store"), "{btext}");
        assert!(btext.contains("s.put('k')"), "batched source must preserve full body: {btext}");

        // Non-flow tools return None so other MCP extensions handle them.
        assert!(tools.call(&worker, "docs", &json!({ "package": "foo" })).await.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
