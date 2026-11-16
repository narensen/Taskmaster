//! What a run spent. One entry for every agent the daemon started.
//!
//! Only some harnesses report a price. A call without one still counts its tokens, and
//! [`Totals::unpriced_calls`] says how many calls the dollar total leaves out.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::run::Tier;
use crate::types::PageId;

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
    /// The part of `tokens` the provider served from its cache, at a reduced rate.
    pub cache_read: u64,
    pub cost_usd: f64,
    /// Calls with no reported price. `cost_usd` does not include them.
    pub unpriced_calls: u64,
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
        for slot in [out.by_role.entry(entry.role.name().into()).or_default(), out.by_model.entry(model).or_default()] {
            slot.calls += 1;
            slot.tokens += tokens;
            slot.cache_read += entry.cache_read;
            slot.cost_usd += entry.cost_usd.unwrap_or(0.0);
            slot.unpriced_calls += u64::from(entry.cost_usd.is_none());
        }
    }
    out
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
        assert_eq!(sum.by_role["worker"], Spend { calls: 2, tokens: 615, cache_read: 0, cost_usd: 0.0, unpriced_calls: 2 });
        assert_eq!(sum.by_model["claude"].tokens, 150);
        assert_eq!(Role::builder(Tier::Mid), Role::Mid);
    }
}
