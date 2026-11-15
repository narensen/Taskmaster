//! The Dictionary: an immutable, content-addressed store plus a read-only MCP front end.
//! The Commons is a second store workers search before inventing a contract.

pub mod catalog;
pub mod mcp;
pub mod store;

pub use catalog::Catalog;
pub use mcp::{CiteRecord, Grant, McpState, router};
pub use store::LocalStore;
