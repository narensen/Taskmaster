//! Domain types and the run state machine. No I/O lives in this crate.

pub mod citations;
pub mod docs;
pub mod errorography;
pub mod ledger;
pub mod plan;
pub mod run;
pub mod types;

pub use citations::{Citation, Citations, page_is_fresh};
pub use errorography::{Book, Mistake, PROMOTE_AT, ProjectNote, digest, project_lesson};
pub use ledger::{
    LedgerEntry, ModelScore, ModelScorecard, Prices, RECOMMEND_FIRST_PASS, Role, SCORE_MIN_UNITS, STRUGGLING_FIRST_PASS, Shape, Spend, Totals, forecast,
    scorecard, totals,
};
pub use plan::{DraftEntry, Rejection, Rule, check_plan, normalize_plan, rejections, rules_text};
pub use run::{Command, Event, PageState, Run, RunPhase, Tier, WorkerOutcome};
pub use types::*;
