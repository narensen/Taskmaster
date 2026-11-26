//! `.flow` — deterministic, parser-built code knowledge graph for Taskmaster.
//!
//! Stores one sorted, deterministic JSON shard per source file under `.flow/files/<path>.json`,
//! graph metadata in `.flow/meta.json`, planner seeds in `.flow/plan.json`, verified test
//! environment facts in `.flow/env.json`, and a derived gitignored SQLite index at
//! `.flow/index.sqlite`.

pub mod env;
pub mod model;
pub mod parse;
pub mod resolve;

pub use env::{EnvFacts, read_env, write_env};
pub use model::{
    BUILDER_VERSION, Confidence, Edge, EdgeKind, FLOW_DIR, FLOW_GITIGNORE_CONTENT, Meta, Node,
    NodeKind, NodeStatus, PlanSeed, SCHEMA_VERSION, Shard, UnitSeed, body_hash, file_node_id,
    git_blob_sha, is_flow_path, lang_prefix, sig_hash, symbol_node_id, unit_node_id,
};
