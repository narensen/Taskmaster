//! On-disk `.flow/` shard storage and derived SQLite index (`.flow/index.sqlite`).
//!
//! Per-file JSON shards under `.flow/files/<path>.json` (plus `.flow/meta.json` and
//! `.flow/plan.json`) are the source of truth. `.flow/index.sqlite` is a gitignored,
//! derived cache rebuilt automatically whenever any shard or metadata file changes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::model::{
    Confidence, Edge, EdgeKind, FLOW_DIR, FLOW_GITIGNORE_CONTENT, Meta, Node, NodeKind,
    NodeStatus, PlanSeed, SCHEMA_VERSION, Shard, normalize_rel_path,
};

pub const META_FILE: &str = "meta.json";
pub const PLAN_FILE: &str = "plan.json";
pub const FILES_DIR: &str = "files";
pub const SQLITE_FILE: &str = "index.sqlite";

pub fn flow_dir(root: &Path) -> PathBuf {
    root.join(FLOW_DIR)
}

pub fn files_dir(root: &Path) -> PathBuf {
    flow_dir(root).join(FILES_DIR)
}

pub fn meta_path(root: &Path) -> PathBuf {
    flow_dir(root).join(META_FILE)
}

pub fn plan_path(root: &Path) -> PathBuf {
    flow_dir(root).join(PLAN_FILE)
}

pub fn sqlite_path(root: &Path) -> PathBuf {
    flow_dir(root).join(SQLITE_FILE)
}

pub fn shard_path(root: &Path, rel_path: &str) -> PathBuf {
    let norm = normalize_rel_path(rel_path);
    files_dir(root).join(format!("{norm}.json"))
}

/// Ensure `.flow/`, `.flow/files/`, and `.flow/.gitignore` exist on disk.
pub fn ensure_flow_dir(root: &Path) -> anyhow::Result<()> {
    let dir = flow_dir(root);
    std::fs::create_dir_all(dir.join(FILES_DIR))?;
    let gi = dir.join(".gitignore");
    if std::fs::read_to_string(&gi).ok().as_deref() != Some(FLOW_GITIGNORE_CONTENT) {
        std::fs::write(&gi, FLOW_GITIGNORE_CONTENT)
            .with_context(|| format!("cannot write {}", gi.display()))?;
    }
    Ok(())
}

/// Write a shard deterministically. Returns `Ok(true)` if the file was created or changed,
/// or `Ok(false)` if identical bytes were already on disk (leaving its mtime intact).
pub fn write_shard(root: &Path, shard: &Shard) -> anyhow::Result<bool> {
    ensure_flow_dir(root)?;
    let dest = shard_path(root, &shard.path);
    let rendered = shard.to_deterministic_json()?;
    if std::fs::read_to_string(&dest).ok().as_deref() == Some(rendered.as_str()) {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, rendered).with_context(|| format!("cannot write {}", dest.display()))?;
    Ok(true)
}

/// Read one shard by repo-relative source path.
pub fn read_shard(root: &Path, rel_path: &str) -> anyhow::Result<Option<Shard>> {
    let path = shard_path(root, rel_path);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let shard: Shard = serde_json::from_str(&raw)
        .with_context(|| format!("invalid shard JSON in {}", path.display()))?;
    Ok(Some(shard))
}

/// Remove a deleted source file's shard and prune empty parent directories under `.flow/files/`.
pub fn remove_shard(root: &Path, rel_path: &str) -> anyhow::Result<bool> {
    let path = shard_path(root, rel_path);
    if !path.is_file() {
        return Ok(false);
    }
    std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    let stop = files_dir(root);
    let mut cur = path.parent().map(Path::to_path_buf);
    while let Some(dir) = cur {
        if dir == stop || !dir.starts_with(&stop) {
            break;
        }
        let is_empty = std::fs::read_dir(&dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            break;
        }
        let _ = std::fs::remove_dir(&dir);
        cur = dir.parent().map(Path::to_path_buf);
    }
    Ok(true)
}

/// Read all shards under `.flow/files/` keyed by repo-relative source path.
pub fn read_all_shards(root: &Path) -> anyhow::Result<BTreeMap<String, Shard>> {
    let base = files_dir(root);
    let mut out = BTreeMap::new();
    if !base.is_dir() {
        return Ok(out);
    }
    let mut paths = Vec::new();
    collect_json_files(&base, &mut paths)?;
    paths.sort();
    for file in paths {
        let raw = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        let shard: Shard = serde_json::from_str(&raw)
            .with_context(|| format!("invalid shard JSON in {}", file.display()))?;
        out.insert(shard.path.clone(), shard);
    }
    Ok(out)
}

pub fn read_meta(root: &Path) -> anyhow::Result<Option<Meta>> {
    let path = meta_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let meta: Meta = serde_json::from_str(&raw)
        .with_context(|| format!("invalid meta JSON in {}", path.display()))?;
    Ok(Some(meta))
}

pub fn write_meta(root: &Path, meta: &Meta) -> anyhow::Result<bool> {
    ensure_flow_dir(root)?;
    let path = meta_path(root);
    let rendered = meta.to_deterministic_json()?;
    if std::fs::read_to_string(&path).ok().as_deref() == Some(rendered.as_str()) {
        return Ok(false);
    }
    std::fs::write(&path, rendered).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

pub fn read_plan_seed(root: &Path) -> anyhow::Result<Option<PlanSeed>> {
    let path = plan_path(root);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let seed: PlanSeed = serde_json::from_str(&raw)
        .with_context(|| format!("invalid plan JSON in {}", path.display()))?;
    Ok(Some(seed))
}

pub fn write_plan_seed(root: &Path, seed: &PlanSeed) -> anyhow::Result<bool> {
    ensure_flow_dir(root)?;
    let path = plan_path(root);
    let rendered = seed.to_deterministic_json()?;
    if std::fs::read_to_string(&path).ok().as_deref() == Some(rendered.as_str()) {
        return Ok(false);
    }
    std::fs::write(&path, rendered).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

fn collect_json_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_json_files(&path, out)?;
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
    Ok(())
}

/// Cheap deterministic fingerprint over `.flow/meta.json`, `.flow/plan.json`, and every
/// shard's relative path, byte length, and modification timestamp.
pub fn shards_fingerprint(root: &Path) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(format!("schema:{SCHEMA_VERSION}\n").as_bytes());
    for file in [meta_path(root), plan_path(root)] {
        hash_file_stat(&file, &file, &mut hasher);
    }
    let base = files_dir(root);
    if base.is_dir() {
        let mut files = Vec::new();
        collect_json_files(&base, &mut files)?;
        files.sort();
        for f in files {
            hash_file_stat(&base, &f, &mut hasher);
        }
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

fn hash_file_stat(base: &Path, path: &Path, hasher: &mut Sha256) {
    let rel = path.strip_prefix(base).unwrap_or(path).to_string_lossy();
    hasher.update(rel.as_bytes());
    hasher.update(b":");
    if let Ok(meta) = std::fs::metadata(path) {
        hasher.update(meta.len().to_le_bytes());
        if let Ok(modified) = meta.modified()
            && let Ok(dur) = modified.duration_since(UNIX_EPOCH)
        {
            hasher.update(dur.as_nanos().to_le_bytes());
        }
    } else {
        hasher.update(b"missing");
    }
    hasher.update(b"\n");
}

/// Derived SQLite index backed by `.flow/index.sqlite`.
pub struct FlowDb {
    conn: Connection,
}

impl FlowDb {
    /// Open `.flow/index.sqlite`, rebuilding it automatically if missing, corrupt, or stale
    /// relative to the JSON shards / `meta.json` / `plan.json`.
    pub fn open_or_rebuild(root: &Path) -> anyhow::Result<Self> {
        ensure_flow_dir(root)?;
        let fp = shards_fingerprint(root)?;
        let db_path = sqlite_path(root);
        if db_path.is_file()
            && let Ok(conn) = open_connection(&db_path)
            && is_index_fresh(&conn, &fp)
        {
            return Ok(FlowDb { conn });
        }
        Self::rebuild(root)
    }

    /// Unconditionally rebuild `.flow/index.sqlite` from `.flow/files/**/*.json` + `plan.json`.
    pub fn rebuild(root: &Path) -> anyhow::Result<Self> {
        ensure_flow_dir(root)?;
        let fp = shards_fingerprint(root)?;
        let shards = read_all_shards(root)?;
        let plan = read_plan_seed(root)?;
        let db_path = sqlite_path(root);

        // Rebuild cleanly: recreate tables in a single transaction (removing corrupt files if needed).
        let mut conn = match open_connection(&db_path) {
            Ok(c) => c,
            Err(_) => {
                let _ = std::fs::remove_file(&db_path);
                let _ = std::fs::remove_file(db_path.with_extension("sqlite-wal"));
                let _ = std::fs::remove_file(db_path.with_extension("sqlite-shm"));
                open_connection(&db_path)?
            }
        };

        let tx = conn.transaction()?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS edges;
             DROP TABLE IF EXISTS nodes;
             DROP TABLE IF EXISTS shards;
             DROP TABLE IF EXISTS index_meta;
             CREATE TABLE index_meta (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE shards (
                 path TEXT PRIMARY KEY,
                 lang TEXT NOT NULL,
                 blob TEXT NOT NULL,
                 owner TEXT
             );
             CREATE TABLE nodes (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 name TEXT NOT NULL,
                 path TEXT NOT NULL,
                 parent TEXT,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 sig TEXT NOT NULL,
                 sig_hash TEXT NOT NULL,
                 body_hash TEXT NOT NULL,
                 exported INTEGER NOT NULL,
                 doc TEXT,
                 owner TEXT,
                 status TEXT NOT NULL
             );
             CREATE INDEX idx_nodes_name ON nodes(name COLLATE NOCASE);
             CREATE INDEX idx_nodes_path ON nodes(path);
             CREATE INDEX idx_nodes_owner ON nodes(owner);
             CREATE INDEX idx_nodes_parent ON nodes(parent);
             CREATE TABLE edges (
                 from_id TEXT NOT NULL,
                 to_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 conf TEXT NOT NULL,
                 PRIMARY KEY (from_id, to_id, kind, conf)
             );
             CREATE INDEX idx_edges_from ON edges(from_id);
             CREATE INDEX idx_edges_to ON edges(to_id);
             CREATE INDEX idx_edges_kind ON edges(kind);",
        )?;

        {
            let mut ins_shard = tx.prepare(
                "INSERT OR REPLACE INTO shards (path, lang, blob, owner) VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut ins_node = tx.prepare(
                "INSERT OR REPLACE INTO nodes \
                 (id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            let mut ins_edge = tx.prepare(
                "INSERT OR IGNORE INTO edges (from_id, to_id, kind, conf) VALUES (?1, ?2, ?3, ?4)",
            )?;

            for shard in shards.values() {
                ins_shard.execute(params![shard.path, shard.lang, shard.blob, shard.owner])?;
                for n in &shard.nodes {
                    ins_node.execute(params![
                        n.id,
                        n.kind.as_str(),
                        n.name,
                        n.path,
                        n.parent,
                        n.span[0] as i64,
                        n.span[1] as i64,
                        n.sig,
                        n.sig_hash,
                        n.body_hash,
                        if n.exported { 1i64 } else { 0i64 },
                        n.doc,
                        n.owner,
                        n.status.as_str(),
                    ])?;
                }
                for e in &shard.edges {
                    ins_edge.execute(params![e.from, e.to, e.kind.as_str(), e.conf.as_str()])?;
                }
            }
        }

        if let Some(seed) = &plan {
            // Planned/unit nodes use INSERT OR IGNORE so any built shard node with the same ID wins.
            let mut ins_planned = tx.prepare(
                "INSERT OR IGNORE INTO nodes \
                 (id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            let mut ins_edge = tx.prepare(
                "INSERT OR IGNORE INTO edges (from_id, to_id, kind, conf) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for n in &seed.nodes {
                ins_planned.execute(params![
                    n.id,
                    n.kind.as_str(),
                    n.name,
                    n.path,
                    n.parent,
                    n.span[0] as i64,
                    n.span[1] as i64,
                    n.sig,
                    n.sig_hash,
                    n.body_hash,
                    if n.exported { 1i64 } else { 0i64 },
                    n.doc,
                    n.owner,
                    n.status.as_str(),
                ])?;
            }
            for e in &seed.edges {
                ins_edge.execute(params![e.from, e.to, e.kind.as_str(), e.conf.as_str()])?;
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('schema_version', ?1)",
            params![SCHEMA_VERSION.to_string()],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('fingerprint', ?1)",
            params![fp],
        )?;
        tx.commit()?;
        Ok(FlowDb { conn })
    }

    pub fn get_node(&self, id: &str) -> anyhow::Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status \
                 FROM nodes WHERE id = ?1",
                params![id],
                row_to_node,
            )
            .optional()
            .map_err(Into::into)
    }

    /// Exact (case-insensitive) name lookup.
    pub fn find_by_name(&self, name: &str, limit: usize) -> anyhow::Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status \
             FROM nodes WHERE lower(name) = lower(?1) \
             ORDER BY exported DESC, path ASC, start_line ASC, id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![name, limit as i64], row_to_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Ranked search across node name, id, signature, docstring, and path.
    pub fn search_nodes(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Node>> {
        let q = query.trim();
        if q.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status \
                 FROM nodes WHERE kind != 'file' \
                 ORDER BY exported DESC, path ASC, start_line ASC, id ASC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], row_to_node)?;
            return rows.collect::<Result<Vec<_>, _>>().map_err(Into::into);
        }
        let lower = q.to_ascii_lowercase();
        let like = format!("%{lower}%");
        let prefix = format!("{lower}%");
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status, \
                CASE \
                  WHEN lower(name) = ?1 THEN 0 \
                  WHEN lower(id) = ?1 THEN 1 \
                  WHEN lower(name) LIKE ?2 THEN 2 \
                  WHEN lower(name) LIKE ?3 THEN 3 \
                  WHEN lower(sig) LIKE ?3 THEN 4 \
                  WHEN lower(coalesce(doc, '')) LIKE ?3 THEN 5 \
                  ELSE 6 \
                END AS rank \
             FROM nodes \
             WHERE lower(name) LIKE ?3 \
                OR lower(id) LIKE ?3 \
                OR lower(sig) LIKE ?3 \
                OR lower(coalesce(doc, '')) LIKE ?3 \
                OR lower(path) LIKE ?3 \
             ORDER BY rank ASC, exported DESC, path ASC, start_line ASC, id ASC \
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![lower, prefix, like, limit as i64], row_to_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn outgoing_edges(&self, from_id: &str) -> anyhow::Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind, conf FROM edges WHERE from_id = ?1 \
             ORDER BY kind ASC, conf ASC, to_id ASC",
        )?;
        let rows = stmt.query_map(params![from_id], row_to_edge)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn incoming_edges(&self, to_id: &str) -> anyhow::Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind, conf FROM edges WHERE to_id = ?1 \
             ORDER BY kind ASC, conf ASC, from_id ASC",
        )?;
        let rows = stmt.query_map(params![to_id], row_to_edge)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn nodes_by_owner(&self, owner: &str) -> anyhow::Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status \
             FROM nodes WHERE owner = ?1 \
             ORDER BY path ASC, start_line ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![owner], row_to_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn nodes_by_path(&self, path: &str) -> anyhow::Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status \
             FROM nodes WHERE path = ?1 \
             ORDER BY start_line ASC, end_line ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![path], row_to_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn all_edges(&self) -> anyhow::Result<Vec<Edge>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, kind, conf FROM edges ORDER BY from_id ASC, kind ASC, to_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_edge)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn all_nodes(&self) -> anyhow::Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, path, parent, start_line, end_line, sig, sig_hash, body_hash, exported, doc, owner, status \
             FROM nodes ORDER BY path ASC, start_line ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_node)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn node_count(&self) -> anyhow::Result<usize> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }

    pub fn edge_count(&self) -> anyhow::Result<usize> {
        let n: i64 = self.conn.query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))?;
        Ok(n.max(0) as usize)
    }
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

fn is_index_fresh(conn: &Connection, expected_fp: &str) -> bool {
    let schema: Option<String> = conn
        .query_row("SELECT value FROM index_meta WHERE key = 'schema_version'", [], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    if schema.as_deref() != Some(SCHEMA_VERSION.to_string().as_str()) {
        return false;
    }
    let fp: Option<String> = conn
        .query_row("SELECT value FROM index_meta WHERE key = 'fingerprint'", [], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    fp.as_deref() == Some(expected_fp)
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let kind_str: String = row.get(1)?;
    let status_str: String = row.get(13)?;
    let exported: i64 = row.get(10)?;
    Ok(Node {
        id: row.get(0)?,
        kind: NodeKind::parse(&kind_str).unwrap_or(NodeKind::Function),
        name: row.get(2)?,
        path: row.get(3)?,
        parent: row.get(4)?,
        span: [row.get::<_, i64>(5)? as u32, row.get::<_, i64>(6)? as u32],
        sig: row.get(7)?,
        sig_hash: row.get(8)?,
        body_hash: row.get(9)?,
        exported: exported != 0,
        doc: row.get(11)?,
        owner: row.get(12)?,
        status: NodeStatus::parse(&status_str).unwrap_or(NodeStatus::Built),
    })
}

fn row_to_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    let kind_str: String = row.get(2)?;
    let conf_str: String = row.get(3)?;
    Ok(Edge {
        from: row.get(0)?,
        to: row.get(1)?,
        kind: EdgeKind::parse(&kind_str).unwrap_or(EdgeKind::Calls),
        conf: Confidence::parse(&conf_str).unwrap_or(Confidence::Name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{body_hash, git_blob_sha, sig_hash};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tm-flow-store-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn shards_write_read_remove_and_sqlite_rebuilds_when_stale() {
        let dir = temp_dir("sqlite");
        let shard = Shard {
            path: "pkg/math.py".into(),
            lang: "python".into(),
            blob: git_blob_sha(b"def add(a, b): return a + b\n"),
            owner: Some("math".into()),
            nodes: vec![Node {
                id: "py:pkg/math.py#add".into(),
                kind: NodeKind::Function,
                name: "add".into(),
                path: "pkg/math.py".into(),
                parent: None,
                span: [1, 1],
                sig: "def add(a, b)".into(),
                sig_hash: sig_hash("def add(a, b)"),
                body_hash: body_hash("def add(a, b): return a + b"),
                exported: true,
                doc: Some("Adds two numbers.".into()),
                owner: Some("math".into()),
                status: NodeStatus::Built,
            }],
            edges: vec![],
        };

        assert!(write_shard(&dir, &shard).unwrap());
        assert!(!write_shard(&dir, &shard).unwrap(), "unchanged shard must not be rewritten");
        assert!(dir.join(".flow/.gitignore").is_file());
        let gi = std::fs::read_to_string(dir.join(".flow/.gitignore")).unwrap();
        assert!(gi.contains("index.sqlite") && gi.contains("index.sqlite-wal") && gi.contains("index.sqlite-shm"));

        let db = FlowDb::open_or_rebuild(&dir).unwrap();
        assert_eq!(db.node_count().unwrap(), 1);
        let found = db.find_by_name("add", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "py:pkg/math.py#add");
        drop(db);

        // Write a second shard -> SQLite fingerprint changes and automatically rebuilds!
        let shard2 = Shard {
            path: "pkg/app.py".into(),
            lang: "python".into(),
            blob: git_blob_sha(b"def main(): return add(1, 2)\n"),
            owner: Some("app".into()),
            nodes: vec![Node {
                id: "py:pkg/app.py#main".into(),
                kind: NodeKind::Function,
                name: "main".into(),
                path: "pkg/app.py".into(),
                parent: None,
                span: [1, 1],
                sig: "def main()".into(),
                sig_hash: sig_hash("def main()"),
                body_hash: body_hash("def main(): return add(1, 2)"),
                exported: true,
                doc: None,
                owner: Some("app".into()),
                status: NodeStatus::Built,
            }],
            edges: vec![Edge {
                from: "py:pkg/app.py#main".into(),
                to: "py:pkg/math.py#add".into(),
                kind: EdgeKind::Calls,
                conf: Confidence::Import,
            }],
        };
        assert!(write_shard(&dir, &shard2).unwrap());

        let db2 = FlowDb::open_or_rebuild(&dir).unwrap();
        assert_eq!(db2.node_count().unwrap(), 2);
        assert_eq!(db2.edge_count().unwrap(), 1);
        assert_eq!(db2.incoming_edges("py:pkg/math.py#add").unwrap().len(), 1);
        drop(db2);

        // Corrupt SQLite file -> auto-rebuilds cleanly.
        std::fs::write(sqlite_path(&dir), b"corrupt sqlite bytes").unwrap();
        let db3 = FlowDb::open_or_rebuild(&dir).unwrap();
        assert_eq!(db3.node_count().unwrap(), 2);
        drop(db3);

        // Remove shard -> parent dir pruned and rebuild removes node.
        assert!(remove_shard(&dir, "pkg/app.py").unwrap());
        let db4 = FlowDb::open_or_rebuild(&dir).unwrap();
        assert_eq!(db4.node_count().unwrap(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
