//! Domain types and the run state machine. No I/O lives in this crate.

pub mod run;
pub mod types;

pub use run::{Command, Event, PageState, Run, RunPhase, Tier, WorkerOutcome};
pub use types::*;
