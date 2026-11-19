//! What a run spent. One entry for every agent the daemon started.
//!
//! Only some harnesses report a price. A call without one still counts its tokens, and
//! [`Totals::unpriced_calls`] says how many calls the dollar total leaves out.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::run::Tier;
use crate::types::{Estimate, PageId};

/// Why the agent was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Worker,
    Mid,
    Master,
    Arbiter,
    Reviewer,
    Security,
    Verifier,
    /// The mid-tier checker's second look at a finished page.
    Checker,
}

impl Role {
    /// The role that builds a page on this tier of the ladder.
    pub fn builder(tier: Tier) -> Role {
        match tier {
            Tier::Worker => Role::Worker,
            Tier::Mid => Role::Mid,
            Tier::Master => Role::Master,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Worker => "worker",
            Role::Mid => "mid",
            Role::Master => "master",
            Role::Arbiter => "arbiter",
            Role::Reviewer => "reviewer",
            Role::Security => "security",
            Role::Verifier => "verifier",
            Role::Checker => "checker",
        }
    }
}

/// One agent call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub at: String,
    pub role: Role,
    #[serde(default)]
    pub page: Option<PageId>,
    pub harness: String,
    #[serde(default)]
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    /// `None` when the harness did not report a price.
    #[serde(default)]
    pub cost_usd: Option<f64>,
    pub seconds: f64,
    /// Whether the harness finished its turn.
    pub ok: bool,
    /// Tool calls the gate refused in this call, each with what it asked for. A call can
    /// finish its turn (`ok`) and still have had calls refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
}

impl LedgerEntry {
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub calls: u64,
    /// Every token, cache reads included.
    pub tokens: u64,
    /// Fresh input: what the model read for the first time.
    #[serde(default)]
    pub input: u64,
    /// What the model wrote. The number a harness can shrink by asking for less.
    #[serde(default)]
    pub output: u64,
    /// The part of `tokens` the provider served from its cache, at a reduced rate.
    pub cache_read: u64,
    /// Input written into the provider's cache, for later calls to read.
    #[serde(default)]
    pub cache_write: u64,
    pub cost_usd: f64,
    /// Calls with no reported price. `cost_usd` does not include them.
    pub unpriced_calls: u64,
    #[serde(default)]
    pub seconds: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Every token, cache reads included.
    pub tokens: u64,
    /// Tokens the models read or wrote for the first time: `tokens` without `cache_read`.
    /// A harness re-sends its own prompt on every step and the provider serves that from
    /// a cache, so this is the number that reflects the work.
    pub fresh: u64,
    /// The sum of the prices that were reported.
    pub cost_usd: f64,
    /// Calls with no reported price. `cost_usd` does not include them.
    pub unpriced_calls: u64,
    pub seconds: f64,
    pub by_role: BTreeMap<String, Spend>,
    /// Keyed by harness and model, such as `agy gemini-flash`.
    pub by_model: BTreeMap<String, Spend>,
    /// Keyed by agent: `master` for the planner, `page:<id>` for a page's builders, and
    /// `<role> page:<id>` for a reviewer or verifier of one page.
    #[serde(default)]
    pub by_agent: BTreeMap<String, Spend>,
}

impl LedgerEntry {
    /// Which agent this call was: the key of [`Totals::by_agent`].
    pub fn agent(&self) -> String {
        match (&self.page, self.role) {
            (None, Role::Planner) => "master".to_string(),
            (None, role) => role.name().to_string(),
            (Some(page), Role::Worker | Role::Mid | Role::Master) => format!("page:{page}"),
            (Some(page), role) => format!("{} page:{page}", role.name()),
        }
    }
}

pub fn totals(entries: &[LedgerEntry]) -> Totals {
    let mut out = Totals::default();
    for entry in entries {
        let tokens = entry.tokens();
        out.calls += 1;
        out.input += entry.input;
        out.output += entry.output;
        out.cache_read += entry.cache_read;
        out.cache_write += entry.cache_write;
        out.tokens += tokens;
        out.fresh += tokens - entry.cache_read;
        out.seconds += entry.seconds;
        match entry.cost_usd {
            Some(cost) => out.cost_usd += cost,
            None => out.unpriced_calls += 1,
        }
        let model = match &entry.model {
            Some(model) => format!("{} {model}", entry.harness),
            None => entry.harness.clone(),
        };
        for slot in [
            out.by_role.entry(entry.role.name().into()).or_default(),
            out.by_model.entry(model.clone()).or_default(),
            out.by_agent.entry(entry.agent()).or_default(),
        ] {
            slot.calls += 1;
            slot.tokens += tokens;
            slot.input += entry.input;
            slot.output += entry.output;
            slot.cache_read += entry.cache_read;
            slot.cache_write += entry.cache_write;
            slot.cost_usd += entry.cost_usd.unwrap_or(0.0);
            slot.unpriced_calls += u64::from(entry.cost_usd.is_none());
            slot.seconds += entry.seconds;
        }
    }
    out
}

/// What one build call and one review call are assumed to cost before this project has
/// any history: fresh tokens, and dollars. They come from one small measured run.
const DEFAULT_BUILD: (u64, f64) = (17_000, 0.0);
const DEFAULT_REVIEW: (u64, f64) = (500, 0.05);

/// What building `pages` pages and running `reviews` reviewers should cost, from what
/// such calls cost in `history`. Tokens are fresh tokens: cache reads are left out, as
/// they are in the summary a run prints. One attempt per page is assumed, so repairs
/// come on top. The second value is how many past calls the forecast rests on.
pub fn forecast(history: &[LedgerEntry], pages: usize, reviews: usize) -> (Estimate, usize) {
    let mean = |roles: &[Role], default: (u64, f64)| -> ((u64, f64), usize) {
        let calls: Vec<&LedgerEntry> = history.iter().filter(|entry| roles.contains(&entry.role)).collect();
        if calls.is_empty() {
            return (default, 0);
        }
        let n = calls.len() as u64;
        let tokens = calls.iter().map(|entry| entry.tokens() - entry.cache_read).sum::<u64>() / n;
        let usd = calls.iter().filter_map(|entry| entry.cost_usd).sum::<f64>() / n as f64;
        ((tokens, usd), calls.len())
    };
    let (build, built) = mean(&[Role::Worker, Role::Mid, Role::Master], DEFAULT_BUILD);
    let (review, reviewed) = mean(&[Role::Reviewer, Role::Security, Role::Verifier], DEFAULT_REVIEW);
    let estimate = Estimate {
        tokens: build.0 * pages as u64 + review.0 * reviews as u64,
        usd: build.1 * pages as f64 + review.1 * reviews as f64,
    };
    (estimate, built + reviewed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(role: Role, harness: &str, input: u64, output: u64, cost: Option<f64>) -> LedgerEntry {
        LedgerEntry {
            at: String::new(),
            role,
            page: None,
            harness: harness.into(),
            model: None,
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            cost_usd: cost,
            seconds: 1.5,
            ok: true,
            refused: Vec::new(),
        }
    }

    #[test]
    fn totals_split_by_role_and_count_unpriced_calls() {
        let entries = vec![
            entry(Role::Planner, "claude", 100, 50, Some(0.25)),
            entry(Role::Worker, "agy", 400, 200, None),
            entry(Role::Worker, "agy", 10, 5, None),
        ];
        let sum = totals(&entries);
        assert_eq!(sum.calls, 3);
        assert_eq!(sum.tokens, 765);
        assert_eq!(sum.fresh, 765);
        let mut cached = entry(Role::Reviewer, "claude", 8, 400, Some(0.05));
        cached.cache_read = 50_000;
        let sum = totals(&[cached]);
        assert_eq!((sum.tokens, sum.fresh, sum.cache_read), (50_408, 408, 50_000));
        assert_eq!(sum.by_role["reviewer"].cache_read, 50_000);
        let sum = totals(&entries);
        assert_eq!(sum.cost_usd, 0.25);
        assert_eq!(sum.unpriced_calls, 2);
        assert_eq!(sum.by_role["worker"].calls, 2);
        assert_eq!(sum.by_role["worker"].tokens, 615);
        assert_eq!((sum.by_role["worker"].cache_read, sum.by_role["worker"].unpriced_calls), (0, 2));
        assert_eq!(sum.by_role["worker"].input + sum.by_role["worker"].output, 615);
        assert_eq!(sum.by_model["claude"].tokens, 150);
        assert_eq!(Role::builder(Tier::Mid), Role::Mid);
    }

    #[test]
    fn every_agent_s_tokens_are_split_four_ways() {
        let mut plan = entry(Role::Planner, "claude", 1200, 900, Some(0.1));
        plan.cache_write = 3000;
        let mut first = entry(Role::Worker, "grok", 700, 300, None);
        first.page = Some("cart".into());
        first.cache_read = 9000;
        let mut repair = entry(Role::Mid, "claude", 100, 50, None);
        repair.page = Some("cart".into());
        let mut review = entry(Role::Reviewer, "codex", 40, 20, None);
        review.page = Some("cart".into());
        let sum = totals(&[plan, first, repair, review]);
        let agents: Vec<&str> = sum.by_agent.keys().map(String::as_str).collect();
        assert_eq!(agents, ["master", "page:cart", "reviewer page:cart"]);
        let cart = &sum.by_agent["page:cart"];
        assert_eq!((cart.calls, cart.input, cart.output, cart.cache_read, cart.cache_write), (2, 800, 350, 9000, 0));
        assert_eq!(sum.by_agent["master"].cache_write, 3000);
        assert_eq!((sum.input, sum.output, sum.cache_read, sum.cache_write), (2040, 1270, 9000, 3000));
        assert_eq!(sum.by_model["grok"].output, 300);
        assert_eq!(sum.by_agent["page:cart"].seconds, 3.0);
        // A ledger written before the split still reads.
        let old: Spend = serde_json::from_str(r#"{"calls":1,"tokens":5,"cache_read":0,"cost_usd":0.0,"unpriced_calls":1}"#).unwrap();
        assert_eq!((old.input, old.output), (0, 0));
    }

    #[test]
    fn a_forecast_uses_what_past_calls_cost_and_leaves_cache_reads_out() {
        let (fresh, basis) = forecast(&[], 3, 2);
        assert_eq!((fresh.tokens, basis), (3 * 17_000 + 2 * 500, 0));
        let mut build = entry(Role::Worker, "agy", 9_000, 1_000, Some(0.02));
        build.cache_read = 400_000;
        let review = entry(Role::Reviewer, "claude", 100, 300, Some(0.04));
        let (seen, basis) = forecast(&[build.clone(), build, review], 3, 2);
        assert_eq!(seen.tokens, 3 * 10_000 + 2 * 400);
        assert!((seen.usd - (3.0 * 0.02 + 2.0 * 0.04)).abs() < 1e-9);
        assert_eq!(basis, 3);
    }
}
