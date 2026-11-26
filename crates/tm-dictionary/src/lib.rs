//! The Dictionary: an immutable, content-addressed store plus a read-only MCP front end.
//! The Commons is a second store workers search before inventing a contract.

pub mod catalog;
pub mod mcp;
pub mod remote;
pub mod store;

pub use catalog::Catalog;
pub use mcp::{CiteRecord, Extension, Grant, McpReplyRecord, McpState, ReplyObserver, ToolPolicy, router};
pub use remote::CommonsRemote;
pub use store::{LocalStore, matches_hash};
