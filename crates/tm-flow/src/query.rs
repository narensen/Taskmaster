//! Query API over `.flow/index.sqlite` and on-disk sources (`find`, `node`, `neighbors`, `source`).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Confidence, Edge, EdgeKind, Node, NodeStatus};
use crate::store::{FlowDb, read_meta};

/// Hard cap on nodes returned by any single query call.
pub const MAX_QUERY_NODES: usize = 40;
/// Hard cap on formatted characters returned by any single query call (~1.5k tokens).
pub const MAX_QUERY_CHARS: usize = 6_000;
/// Maximum BFS hop depth for `neighbors`.
pub const MAX_HOPS: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Both,
    Out,
    In,
}

impl Direction {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "out" | "outgoing" | "callees" => Direction::Out,
            "in" | "incoming" | "callers" => Direction::In,
            _ => Direction::Both,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindResult {
    pub query: String,
    pub nodes: Vec<Node>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeView {
    pub dir: String,
    pub kind: EdgeKind,
    pub conf: Confidence,
    pub peer_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub peer_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub peer_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_owner: Option<String>,
    pub peer_status: NodeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDetail {
    pub node: Node,
    pub incoming: Vec<EdgeView>,
    pub outgoing: Vec<EdgeView>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborsResult {
    pub root: String,
    pub k: usize,
    pub dir: Direction,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSlice {
    pub id: String,
    pub path: String,
    pub span: [u32; 2],
    pub code: String,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct QueryOutput {
    pub json: Value,
    pub text: String,
}

/// Query handle backed by `.flow/index.sqlite`.
pub struct FlowQuery<'a> {
    tree: &'a Path,
    db: FlowDb,
    moves: BTreeMap<String, String>,
}

impl<'a> FlowQuery<'a> {
    pub fn open(tree: &'a Path) -> anyhow::Result<Self> {
        let db = FlowDb::open_or_rebuild(tree)?;
        let moves = read_meta(tree)?.map(|m| m.moves).unwrap_or_default();
        Ok(Self { tree, db, moves })
    }

    fn resolve_id<'b>(&'b self, id: &'b str) -> &'b str {
        let mut cur = id;
        for _ in 0..8 {
            match self.moves.get(cur) {
                Some(next) if next != cur => cur = next.as_str(),
                _ => break,
            }
        }
        cur
    }

    /// Find nodes matching `query` by name, id, signature, docstring, or path.
    pub fn find(&self, query: &str, limit: usize) -> anyhow::Result<FindResult> {
        let cap = limit.clamp(1, MAX_QUERY_NODES);
        let mut nodes = self.db.search_nodes(query, cap + 1)?;
        let truncated = nodes.len() > cap;
        nodes.truncate(cap);
        Ok(FindResult {
            query: query.to_string(),
            nodes,
            truncated,
        })
    }

    /// Inspect one node by ID along with its incoming and outgoing edges (capped).
    pub fn node(&self, id: &str) -> anyhow::Result<Option<NodeDetail>> {
        let resolved = self.resolve_id(id);
        let Some(node) = self.db.get_node(resolved)? else {
            return Ok(None);
        };
        let mut raw_in = self.db.incoming_edges(&node.id)?;
        let mut raw_out = self.db.outgoing_edges(&node.id)?;
        let mut truncated = false;
        if raw_in.len() > MAX_QUERY_NODES {
            raw_in.truncate(MAX_QUERY_NODES);
            truncated = true;
        }
        if raw_out.len() > MAX_QUERY_NODES {
            raw_out.truncate(MAX_QUERY_NODES);
            truncated = true;
        }
        let incoming = raw_in
            .into_iter()
            .map(|e| self.edge_view("in", &e.from, e.kind, e.conf))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let outgoing = raw_out
            .into_iter()
            .map(|e| self.edge_view("out", &e.to, e.kind, e.conf))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Some(NodeDetail {
            node,
            incoming,
            outgoing,
            truncated,
        }))
    }

    fn edge_view(
        &self,
        dir: &str,
        peer_id: &str,
        kind: EdgeKind,
        conf: Confidence,
    ) -> anyhow::Result<EdgeView> {
        let peer = self.db.get_node(peer_id)?;
        Ok(match peer {
            Some(p) => EdgeView {
                dir: dir.to_string(),
                kind,
                conf,
                peer_id: p.id,
                peer_name: p.name,
                peer_path: p.path,
                peer_owner: p.owner,
                peer_status: p.status,
            },
            None => EdgeView {
                dir: dir.to_string(),
                kind,
                conf,
                peer_id: peer_id.to_string(),
                peer_name: String::new(),
                peer_path: String::new(),
                peer_owner: None,
                peer_status: NodeStatus::Planned,
            },
        })
    }

    /// Breadth-first k-hop neighborhood around `id`, optionally filtered by edge kinds and direction.
    pub fn neighbors(
        &self,
        id: &str,
        k: usize,
        kinds: Option<&[EdgeKind]>,
        dir: Direction,
        limit: usize,
    ) -> anyhow::Result<NeighborsResult> {
        let root_id = self.resolve_id(id).to_string();
        let hops = k.clamp(1, MAX_HOPS);
        let cap = limit.clamp(1, MAX_QUERY_NODES);
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut nodes_map: HashMap<String, Node> = HashMap::new();
        let mut edges_set: BTreeSet<Edge> = BTreeSet::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::new();
        let mut truncated = false;

        visited.insert(root_id.clone());
        if let Some(root_node) = self.db.get_node(&root_id)? {
            nodes_map.insert(root_id.clone(), root_node);
        }
        queue.push_back((root_id.clone(), 0));

        while let Some((cur, depth)) = queue.pop_front() {
            if depth >= hops {
                continue;
            }
            let mut next_edges: Vec<(Edge, String)> = Vec::new();
            if matches!(dir, Direction::Both | Direction::Out) {
                for e in self.db.outgoing_edges(&cur)? {
                    if kinds.is_none_or(|ks| ks.contains(&e.kind)) {
                        let peer = e.to.clone();
                        next_edges.push((e, peer));
                    }
                }
            }
            if matches!(dir, Direction::Both | Direction::In) {
                for e in self.db.incoming_edges(&cur)? {
                    if kinds.is_none_or(|ks| ks.contains(&e.kind)) {
                        let peer = e.from.clone();
                        next_edges.push((e, peer));
                    }
                }
            }
            for (edge, peer) in next_edges {
                if !visited.contains(&peer) {
                    if visited.len() >= cap {
                        truncated = true;
                        continue;
                    }
                    visited.insert(peer.clone());
                    if let Some(n) = self.db.get_node(&peer)? {
                        nodes_map.insert(peer.clone(), n);
                    }
                    queue.push_back((peer, depth + 1));
                }
                edges_set.insert(edge);
            }
        }

        let mut nodes: Vec<Node> = nodes_map.into_values().collect();
        nodes.sort_by(|a, b| {
            let a_root = a.id == root_id;
            let b_root = b.id == root_id;
            b_root
                .cmp(&a_root)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.span.cmp(&b.span))
                .then_with(|| a.id.cmp(&b.id))
        });
        let edges: Vec<Edge> = edges_set.into_iter().collect();

        Ok(NeighborsResult {
            root: root_id,
            k: hops,
            dir,
            nodes,
            edges,
            truncated,
        })
    }

    /// Return only the source lines covering `id`'s `[start, end]` span, capped at `MAX_QUERY_CHARS`.
    pub fn source(&self, id: &str) -> anyhow::Result<Option<SourceSlice>> {
        let resolved = self.resolve_id(id);
        let Some(node) = self.db.get_node(resolved)? else {
            return Ok(None);
        };
        if node.path.is_empty() || node.span[0] == 0 {
            return Ok(Some(SourceSlice {
                id: node.id,
                path: node.path,
                span: node.span,
                code: format!("(no source span on disk; status: {}, sig: {})", node.status.as_str(), node.sig),
                truncated: false,
            }));
        }
        let full = self.tree.join(&node.path);
        let text = std::fs::read_to_string(&full)?;
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            return Ok(Some(SourceSlice {
                id: node.id,
                path: node.path,
                span: node.span,
                code: String::new(),
                truncated: false,
            }));
        }
        let start = (node.span[0].saturating_sub(1) as usize).min(lines.len().saturating_sub(1));
        let end = (node.span[1] as usize).clamp(start + 1, lines.len());
        let raw = lines[start..end].join("\n");
        let (code, truncated) = clip_chars(&raw, MAX_QUERY_CHARS.saturating_sub(200));
        Ok(Some(SourceSlice {
            id: node.id,
            path: node.path,
            span: [start as u32 + 1, end as u32],
            code,
            truncated,
        }))
    }
}

/// Dispatch one `flow_query` / Code Mode `flow(op, args)` operation with strict caps.
pub fn execute_op(tree: &Path, op: &str, args: &Value) -> anyhow::Result<QueryOutput> {
    let q = FlowQuery::open(tree)?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(MAX_QUERY_NODES)
        .clamp(1, MAX_QUERY_NODES);

    match op.trim().to_ascii_lowercase().as_str() {
        "find" => {
            let query = args
                .get("query")
                .or_else(|| args.get("q"))
                .or_else(|| args.get("name"))
                .or_else(|| args.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let res = q.find(query, limit)?;
            let text = format_find(&res);
            Ok(QueryOutput {
                json: serde_json::to_value(&res)?,
                text,
            })
        }
        "node" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`node` requires a non-empty `id`"))?;
            let res = q.node(id)?;
            match res {
                Some(detail) => {
                    let text = format_node(&detail);
                    Ok(QueryOutput {
                        json: serde_json::to_value(&detail)?,
                        text,
                    })
                }
                None => Ok(QueryOutput {
                    json: json!(null),
                    text: format!("Node `{id}` not found."),
                }),
            }
        }
        "neighbors" | "callers" | "callees" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`{op}` requires a non-empty `id`"))?;
            let k = args
                .get("k")
                .or_else(|| args.get("depth"))
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(1);
            let op_lower = op.trim().to_ascii_lowercase();
            let dir = if op_lower == "callers" {
                Direction::In
            } else if op_lower == "callees" {
                Direction::Out
            } else {
                args.get("dir")
                    .and_then(|v| v.as_str())
                    .map(Direction::parse)
                    .unwrap_or(Direction::Both)
            };
            let parsed_kinds: Option<Vec<EdgeKind>> = if op_lower == "callers" || op_lower == "callees" {
                Some(vec![EdgeKind::Calls])
            } else {
                parse_kinds_arg(args.get("kinds"))
            };
            let res = q.neighbors(id, k, parsed_kinds.as_deref(), dir, limit)?;
            let text = format_neighbors(&res);
            Ok(QueryOutput {
                json: serde_json::to_value(&res)?,
                text,
            })
        }
        "source" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("`source` requires a non-empty `id`"))?;
            match q.source(id)? {
                Some(slice) => {
                    let text = format_source(&slice);
                    Ok(QueryOutput {
                        json: serde_json::to_value(&slice)?,
                        text,
                    })
                }
                None => Ok(QueryOutput {
                    json: json!(null),
                    text: format!("Node `{id}` not found."),
                }),
            }
        }
        other => bail!("unknown flow op `{other}` (use find, node, neighbors, or source)"),
    }
}

fn parse_kinds_arg(val: Option<&Value>) -> Option<Vec<EdgeKind>> {
    let val = val?;
    let mut out = Vec::new();
    match val {
        Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str()
                    && let Some(k) = EdgeKind::parse(s)
                {
                    out.push(k);
                }
            }
        }
        Value::String(s) => {
            for part in s.split(',') {
                if let Some(k) = EdgeKind::parse(part.trim()) {
                    out.push(k);
                }
            }
        }
        _ => {}
    }
    (!out.is_empty()).then_some(out)
}

pub fn format_find(res: &FindResult) -> String {
    if res.nodes.is_empty() {
        return format!("No flow nodes matched `{}`.", res.query);
    }
    let mut out = format!("find `{}` ({} match{}):\n", res.query, res.nodes.len(), if res.nodes.len() == 1 { "" } else { "es" });
    for n in &res.nodes {
        let owner = n.owner.as_deref().unwrap_or("-");
        let mut line = format!(
            "- {} [{}|{}|owner:{}] {}:{}",
            n.id,
            n.kind.as_str(),
            n.status.as_str(),
            owner,
            n.path,
            n.span[0]
        );
        if !n.sig.is_empty() {
            let _ = write!(line, " :: {}", n.sig);
        }
        if let Some(doc) = &n.doc {
            let _ = write!(line, " — {doc}");
        }
        line.push('\n');
        if out.len() + line.len() > MAX_QUERY_CHARS - 32 {
            out.push_str("... (truncated)\n");
            return out;
        }
        out.push_str(&line);
    }
    if res.truncated {
        out.push_str("... (more results capped at limit)\n");
    }
    out
}

pub fn format_node(detail: &NodeDetail) -> String {
    let n = &detail.node;
    let owner = n.owner.as_deref().unwrap_or("-");
    let mut out = format!(
        "{} ({}, {}, owner: {}, {}:{}-{})\n",
        n.id,
        n.kind.as_str(),
        n.status.as_str(),
        owner,
        n.path,
        n.span[0],
        n.span[1]
    );
    if !n.sig.is_empty() {
        let _ = writeln!(out, "sig: {}", n.sig);
    }
    if let Some(doc) = &n.doc {
        let _ = writeln!(out, "doc: {doc}");
    }
    if !detail.outgoing.is_empty() {
        out.push_str("out:\n");
        for e in &detail.outgoing {
            let line = format!(
                "  ▸ {} {} [{}|{}|owner:{}]\n",
                e.kind.as_str(),
                e.peer_id,
                e.conf.as_str(),
                e.peer_status.as_str(),
                e.peer_owner.as_deref().unwrap_or("-")
            );
            if out.len() + line.len() > MAX_QUERY_CHARS - 32 {
                out.push_str("... (truncated)\n");
                return out;
            }
            out.push_str(&line);
        }
    }
    if !detail.incoming.is_empty() {
        out.push_str("in:\n");
        for e in &detail.incoming {
            let line = format!(
                "  ◂ {} {} [{}|{}|owner:{}]\n",
                e.kind.as_str(),
                e.peer_id,
                e.conf.as_str(),
                e.peer_status.as_str(),
                e.peer_owner.as_deref().unwrap_or("-")
            );
            if out.len() + line.len() > MAX_QUERY_CHARS - 32 {
                out.push_str("... (truncated)\n");
                return out;
            }
            out.push_str(&line);
        }
    }
    if detail.truncated {
        out.push_str("... (edges capped)\n");
    }
    out
}

pub fn format_neighbors(res: &NeighborsResult) -> String {
    let mut out = format!(
        "neighbors of {} (k={}, {} nodes, {} edges):\n",
        res.root,
        res.k,
        res.nodes.len(),
        res.edges.len()
    );
    for n in &res.nodes {
        let line = format!(
            "- {} [{}|{}|owner:{}] {}:{}\n",
            n.id,
            n.kind.as_str(),
            n.status.as_str(),
            n.owner.as_deref().unwrap_or("-"),
            n.path,
            n.span[0]
        );
        if out.len() + line.len() > MAX_QUERY_CHARS - 32 {
            out.push_str("... (truncated)\n");
            return out;
        }
        out.push_str(&line);
    }
    for e in &res.edges {
        let line = format!(
            "  {} --{}[{}]--> {}\n",
            e.from,
            e.kind.as_str(),
            e.conf.as_str(),
            e.to
        );
        if out.len() + line.len() > MAX_QUERY_CHARS - 32 {
            out.push_str("... (truncated)\n");
            return out;
        }
        out.push_str(&line);
    }
    if res.truncated {
        out.push_str("... (more neighbors capped)\n");
    }
    out
}

pub fn format_source(slice: &SourceSlice) -> String {
    let mut out = format!(
        "# {} ({}:{}-{})\n{}\n",
        slice.id, slice.path, slice.span[0], slice.span[1], slice.code
    );
    if slice.truncated {
        out.push_str("... (source truncated to cap)\n");
    }
    let (capped, _) = clip_chars(&out, MAX_QUERY_CHARS);
    capped
}

fn clip_chars(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::build;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn fixture_repo(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tm-flow-query-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(
            dir.join("pkg/storage.py"),
            "class Storage:\n    \"\"\"Persists rows.\"\"\"\n    def save(self, row: dict) -> None:\n        pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pkg/engine.py"),
            "from pkg.storage import Storage\n\nclass Engine:\n    def __init__(self, store: Storage) -> None:\n        self.store = store\n\n    def commit(self, row: dict) -> None:\n        \"\"\"Write one row.\"\"\"\n        self.store.save(row)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pkg/cli.py"),
            "from pkg.engine import Engine\n\ndef main(eng: Engine) -> None:\n    eng.commit({'id': 1})\n",
        )
        .unwrap();
        build(&dir).unwrap();
        dir
    }

    #[test]
    fn find_node_neighbors_and_source_work_and_respect_caps() {
        let dir = fixture_repo("ops");
        let q = FlowQuery::open(&dir).unwrap();

        let found = q.find("commit", 10).unwrap();
        assert!(!found.nodes.is_empty());
        assert_eq!(found.nodes[0].id, "py:pkg/engine.py#Engine.commit");

        let detail = q.node("py:pkg/engine.py#Engine.commit").unwrap().unwrap();
        assert_eq!(detail.node.doc.as_deref(), Some("Write one row."));
        assert!(detail.outgoing.iter().any(|e| e.peer_id == "py:pkg/storage.py#Storage.save" && e.kind == EdgeKind::Calls));
        assert!(detail.incoming.iter().any(|e| e.peer_id == "py:pkg/cli.py#main" && e.kind == EdgeKind::Calls));

        let nbrs = q
            .neighbors("py:pkg/cli.py#main", 2, Some(&[EdgeKind::Calls]), Direction::Out, 10)
            .unwrap();
        let ids: Vec<&str> = nbrs.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"py:pkg/engine.py#Engine.commit"), "{ids:?}");
        assert!(ids.contains(&"py:pkg/storage.py#Storage.save"), "{ids:?}");

        // Source returns ONLY the method span, not the whole file.
        let src = q.source("py:pkg/engine.py#Engine.commit").unwrap().unwrap();
        assert!(src.code.contains("def commit"), "{}", src.code);
        assert!(src.code.contains("self.store.save(row)"), "{}", src.code);
        assert!(!src.code.contains("def __init__"), "must only return node span: {}", src.code);

        // Caps enforced on find/execute_op.
        let capped = execute_op(&dir, "find", &json!({ "query": "", "limit": 2 })).unwrap();
        assert_eq!(capped.json["nodes"].as_array().unwrap().len(), 2);
        assert_eq!(capped.json["truncated"], true);
        assert!(capped.text.len() <= MAX_QUERY_CHARS);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
