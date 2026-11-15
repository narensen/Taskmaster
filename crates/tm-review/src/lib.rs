//! Code review for any git repository.
//!
//! A review never depends on the conversation that produced the code. It gets its own
//! checkout of the commit under review, and everything a reviewer needs is written into
//! that checkout as files: how the code fits together, what changed, the project's review
//! rules, and what earlier reviews of the same files found.

pub mod brief;
pub mod context;
pub mod findings;
pub mod git;
pub mod history;
pub mod rules;

/// Directory inside the review checkout that holds the context files.
pub const CONTEXT_DIR: &str = ".taskmaster-review";
/// Project-wide rules file, written by the project's owners.
pub const ROOT_RULES: &str = ".taskmaster/REVIEW.md";
/// Rules for one directory and everything below it.
pub const NESTED_RULES: &str = "REVIEW.md";
/// Where review records are appended, inside the reviewed repository.
pub const HISTORY_FILE: &str = ".taskmaster/reviews/history.jsonl";
