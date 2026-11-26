//! `.flow` — deterministic, parser-built code knowledge graph for Taskmaster.
//!
//! Stores one sorted, deterministic JSON shard per source file under `.flow/files/<path>.json`,
//! graph metadata in `.flow/meta.json`, planner seeds in `.flow/plan.json`, verified test
//! environment facts in `.flow/env.json`, and a derived gitignored SQLite index at
//! `.flow/index.sqlite`.

pub mod env;
pub mod model;
pub mod parse;
pub mod query;
pub mod render;
pub mod resolve;
pub mod store;
pub mod update;

pub use env::{EnvFacts, read_env, write_env};
pub use model::{
    BUILDER_VERSION, Confidence, Edge, EdgeKind, FLOW_DIR, FLOW_GITIGNORE_CONTENT, Meta, Node,
    NodeKind, NodeStatus, PlanSeed, SCHEMA_VERSION, Shard, UnitSeed, body_hash, file_node_id,
    git_blob_sha, is_flow_path, lang_prefix, sig_hash, symbol_node_id, unit_node_id,
};
pub use query::{
    Direction, EdgeView, FindResult, FlowQuery, MAX_BATCH_CHARS, MAX_BATCH_QUERIES,
    MAX_QUERY_CHARS, MAX_QUERY_NODES, NeighborsResult, NodeDetail, QueryOutput, SourceSlice,
    execute_batch, execute_op,
};
pub use render::{
    DEFAULT_UNIT_MAP_BUDGET, PLANNER_REPO_SUMMARY_BUDGET, REPAIR_UNIT_MAP_BUDGET,
    is_integrator_unit, preinjected_context, repo_summary, unit_map,
};
pub use store::FlowDb;
pub use update::{
    BuildReport, SeedReport, UpdateReport, build, discover_source_files, ensure_fresh,
    seed_from_plan, update_files, verify_all,
};
