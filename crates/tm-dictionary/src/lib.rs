//! The Dictionary: an immutable, content-addressed store plus a read-only MCP front end.

pub mod mcp;
pub mod store;

pub use mcp::{CiteRecord, Grant, McpState, router};
pub use store::LocalStore;
