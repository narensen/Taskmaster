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
    /// The agent that made the call, as the transcript names it: `master`, `page:<id>`, or
    /// `unit:<id>/<part>` for one part of a split unit. Older rows have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The part of a split unit the call built, when it was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Where `cost_usd` came from: `provider` when the harness reported it, `prices` when it
    /// was worked out from the price table. Missing when there is no price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priced_by: Option<String>,
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
    /// Which agent this call was: the key of [`Totals::by_agent`]. A builder row names its
    /// own agent, so the parts of a split unit are counted apart.
    pub fn agent(&self) -> String {
        if let Some(agent) = self.agent.as_ref().filter(|a| !a.is_empty())
            && matches!(self.role, Role::Worker | Role::Mid | Role::Master)
            && self.page.is_some()
        {
            return agent.clone();
        }
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

/// Dollars per million tokens for one model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Prices {
    pub input: f64,
    /// Input served from the provider's cache. Missing means the same as `input`.
    #[serde(default)]
    pub cached_input: Option<f64>,
    pub output: f64,
}

impl Prices {
    /// What a call with these counts costs. Uncached input, cached input and output are
    /// priced apart, as providers bill them.
    pub fn cost(&self, input: u64, output: u64, cache_read: u64) -> f64 {
        let per = |n: u64, price: f64| n as f64 * price / 1_000_000.0;
        per(input, self.input) + per(cache_read, self.cached_input.unwrap_or(self.input)) + per(output, self.output)
    }
}

/// What a plan asks for, as far as cost goes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Shape {
    /// Units planned so far (with a sketch, the first phase's).
    pub units: usize,
    /// Whether they are pages of a web app, which are smaller than modules with tests.
    pub web: bool,
    /// Phases in the sketch; 0 or 1 is a run planned whole.
    pub phases: usize,
    /// Bytes of locked contracts, which every builder carries in its prompt.
    pub contract_bytes: usize,
}

/// Fresh tokens and cache reads one unit's builder uses before this project has history,
/// from the November 2026 benchmark runs: a module with its tests, and a web page.
const DEFAULT_MODULE: (u64, u64) = (110_000, 330_000);
const DEFAULT_PAGE: (u64, u64) = (40_000, 120_000);
/// Planning one later phase of a sketch.
const DEFAULT_PHASE_PLAN: (u64, u64) = (25_000, 50_000);
/// Builder calls per unit: the first attempt and the repairs a unit needs on average.
const CALLS_PER_UNIT: f64 = 1.3;
/// Of a call's fresh tokens, the share that is output when only the total is known.
const OUTPUT_SHARE: f64 = 0.1;

/// What building a plan of `shape` should cost, from what build calls cost in `history`,
/// or from defaults sized to the plan when there is none. Fresh tokens and cache reads are
/// estimated apart; dollars come from past priced calls, else from `prices`, else stay
/// unpriced. Later phases of a sketch are assumed to be as big as the first. The second
/// value is how many past calls the forecast rests on.
pub fn forecast(history: &[LedgerEntry], shape: &Shape, prices: Option<Prices>) -> (Estimate, usize) {
    let builds: Vec<&LedgerEntry> = history.iter().filter(|e| matches!(e.role, Role::Worker | Role::Mid | Role::Master) && e.page.is_some()).collect();
    let phases = shape.phases.max(1);
    let units = shape.units.max(1) * phases;
    let calls = units as f64 * CALLS_PER_UNIT;
    let (per_fresh, per_cached) = if builds.is_empty() {
        let (fresh, cached) = if shape.web { DEFAULT_PAGE } else { DEFAULT_MODULE };
        (fresh as f64 / CALLS_PER_UNIT, cached as f64 / CALLS_PER_UNIT)
    } else {
        let n = builds.len() as f64;
        (builds.iter().map(|e| (e.input + e.output) as f64).sum::<f64>() / n, builds.iter().map(|e| e.cache_read as f64).sum::<f64>() / n)
    };
    // Every builder carries the contracts in its prompt; after the first, from the cache.
    let contracts = (shape.contract_bytes / 4) as f64 * calls;
    let later = phases.saturating_sub(1) as f64;
    let fresh = per_fresh * calls + DEFAULT_PHASE_PLAN.0 as f64 * later;
    let cached = per_cached * calls + contracts + DEFAULT_PHASE_PLAN.1 as f64 * later;
    let priced: Vec<f64> = builds.iter().filter_map(|e| e.cost_usd).collect();
    let (usd, priced) = if !priced.is_empty() {
        let per_token = priced.iter().sum::<f64>()
            / builds.iter().filter(|e| e.cost_usd.is_some()).map(|e| (e.input + e.output + e.cache_read).max(1) as f64).sum::<f64>();
        (per_token * (fresh + cached), true)
    } else if let Some(prices) = prices {
        let output = (fresh * OUTPUT_SHARE) as u64;
        (prices.cost(fresh as u64 - output, output, cached as u64), true)
    } else {
        (0.0, false)
    };
    (Estimate { tokens: fresh as u64, usd, cache_read: cached as u64, priced }, builds.len())
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
            agent: None,
            unit: None,
            priced_by: None,
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
    fn a_forecast_is_sized_to_the_plan_and_counts_cache_reads_apart() {
        let module = Shape { units: 1, web: false, phases: 0, contract_bytes: 0 };
        let (one, basis) = forecast(&[], &module, None);
        assert_eq!(basis, 0);
        assert!((100_000..=120_000).contains(&one.tokens), "{one:?}");
        assert!(one.cache_read > one.tokens && !one.priced && one.usd == 0.0);
        // Three units, a sketch of two phases and 8 KB of contracts cost more, in both counts.
        let big = Shape { units: 3, web: false, phases: 2, contract_bytes: 8_000 };
        let (more, _) = forecast(&[], &big, None);
        assert!(more.tokens > 6 * one.tokens && more.cache_read > 6 * one.cache_read, "{more:?}");
        // A web page is smaller than a module with its tests.
        let (page, _) = forecast(&[], &Shape { web: true, ..module }, None);
        assert!(page.tokens < one.tokens);
        // With prices, the estimate is in dollars: uncached, cached and output apart.
        let grok = Prices { input: 2.0, cached_input: Some(0.5), output: 6.0 };
        let (priced, _) = forecast(&[], &module, Some(grok));
        assert!(priced.priced && priced.usd > 0.1, "{priced:?}");
        assert!((grok.cost(1_000_000, 0, 1_000_000) - 2.5).abs() < 1e-9);
        // History wins over defaults: what past build calls used, fresh and cached, and paid.
        let mut build = entry(Role::Worker, "agy", 9_000, 1_000, Some(0.02));
        build.page = Some("a".into());
        build.cache_read = 40_000;
        let (seen, basis) = forecast(&[build.clone(), build], &module, None);
        assert_eq!(basis, 2);
        assert_eq!(seen.tokens, 13_000);
        assert_eq!(seen.cache_read, 52_000);
        assert!(seen.priced && (seen.usd - 0.026).abs() < 1e-6, "{seen:?}");
    }

    #[test]
    fn the_parts_of_a_split_unit_are_counted_apart() {
        let mut list = entry(Role::Worker, "grok", 10, 1, None);
        list.page = Some("books".into());
        list.agent = Some("unit:books/list".into());
        list.unit = Some("list".into());
        let mut styles = list.clone();
        styles.agent = Some("unit:books/styles".into());
        styles.unit = Some("styles".into());
        let sum = totals(&[list, styles]);
        assert_eq!(sum.by_agent.keys().collect::<Vec<_>>(), ["unit:books/list", "unit:books/styles"]);
    }
}
