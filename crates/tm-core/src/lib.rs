//! Domain types and the run state machine. No I/O lives in this crate.

pub mod citations;
pub mod docs;
pub mod errorography;
pub mod ledger;
pub mod plan;
pub mod playbooks;
pub mod run;
pub mod schema;
pub mod types;

pub use citations::{Citation, Citations, page_is_fresh};
pub use errorography::{Book, Mistake, PROMOTE_AT, ProjectNote, digest, project_lesson};
pub use ledger::{
    LedgerEntry, ModelScore, ModelScorecard, Prices, RECOMMEND_FIRST_PASS, Role, SCORE_MIN_UNITS, STRUGGLING_FIRST_PASS, Shape, Spend, StratifiedScore, Totals, auto_route, forecast,
    scorecard, stratified_scores, totals,
};
pub use plan::{
    CONTRACT_LINES, CONTRACT_LINES_SOFT, DraftEntry, PLAN_CONTRACT_LINES, Rejection, Rule, check_plan,
    effective_contract_phase, normalize_plan, plan_warnings, rejections, rules_text,
};
pub use playbooks::{Playbook, brief_hash, format_playbooks, match_playbooks};
pub use run::{Command, Event, PageState, Run, RunPhase, Tier, WorkerOutcome};
pub use schema::{SCHEMA_VERSION, events_header, migrate_status, unwrap_event_line};
pub use types::*;
