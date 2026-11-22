//! Provider rate-limit and usage tracking before limits bite (O4).
//!
//! Tracks 5-hour and 7-day rolling utilization windows and reset times per provider,
//! populated from each CLI's own structured events (`rate_limit_event`, Codex `rate_limits`,
//! ACP `session/update` usage). OAuth polling is opt-in and off by default.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tm_harness::RateLimitReport;

/// Utilization percentage at or above which a provider triggers a warning and is skipped by Auto.
pub const WARNING_THRESHOLD_PCT: f64 = 80.0;

/// Normalize harness ids (`"claude-code"` -> `"claude"`, etc.) so CLI events, `Roles`, and UI agree.
pub fn canonical_provider(id: &str) -> String {
    let trimmed = id.trim().to_ascii_lowercase();
    match trimmed.as_str() {
        "claude-code" | "claude_code" | "anthropic" => "claude".into(),
        "openai" | "codex-cli" => "codex".into(),
        "antigravity" => "agy".into(),
        "grok-build" | "grok_build" => "grok".into(),
        _ => trimmed,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WindowUsage {
    /// Utilization percentage 0.0..=100.0.
    pub pct: f64,
    /// Reset timestamp or CLI-reported label, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_5h: Option<WindowUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_7d: Option<WindowUsage>,
    /// Where this reading came from (`"claude-cli"`, `"codex-cli"`, `"acp"`, `"event"`, `"oauth"`).
    pub source: String,
    pub updated_at: String,
    /// True when either the 5-hour or 7-day window is at or above [`WARNING_THRESHOLD_PCT`].
    pub warning: bool,
}

impl ProviderUsage {
    pub fn max_pct(&self) -> f64 {
        let a = self.window_5h.as_ref().map_or(0.0, |w| w.pct);
        let b = self.window_7d.as_ref().map_or(0.0, |w| w.pct);
        a.max(b)
    }

    pub fn is_near_limit(&self) -> bool {
        self.max_pct() >= WARNING_THRESHOLD_PCT
    }
}

/// Thread-safe in-memory tracker of provider usage windows, shared across `Live` and `App`.
#[derive(Debug, Clone, Default)]
pub struct UsageTracker {
    inner: Arc<Mutex<BTreeMap<String, ProviderUsage>>>,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a CLI rate-limit report for `provider`, merging any present windows.
    pub fn record_report(&self, provider: &str, report: &RateLimitReport) {
        if report.is_empty() {
            return;
        }
        let key = canonical_provider(provider);
        if key.is_empty() {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(key.clone()).or_insert_with(|| ProviderUsage {
            provider: key,
            window_5h: None,
            window_7d: None,
            source: "cli".into(),
            updated_at: String::new(),
            warning: false,
        });
        if let Some(pct) = report.window_5h_pct {
            entry.window_5h = Some(WindowUsage {
                pct: pct.clamp(0.0, 100.0),
                resets_at: report
                    .reset_5h
                    .clone()
                    .or_else(|| entry.window_5h.as_ref().and_then(|w| w.resets_at.clone())),
            });
        }
        if let Some(pct) = report.window_7d_pct {
            entry.window_7d = Some(WindowUsage {
                pct: pct.clamp(0.0, 100.0),
                resets_at: report
                    .reset_7d
                    .clone()
                    .or_else(|| entry.window_7d.as_ref().and_then(|w| w.resets_at.clone())),
            });
        }
        if let Some(src) = &report.source {
            entry.source = src.clone();
        }
        entry.updated_at = rfc3339_now();
        entry.warning = entry.is_near_limit();
    }

    /// Snapshot of all tracked providers, sorted by provider id.
    pub fn snapshot(&self) -> Vec<ProviderUsage> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    /// Canonical provider ids currently at or above [`WARNING_THRESHOLD_PCT`].
    pub fn near_limit(&self) -> BTreeSet<String> {
        self.inner
            .lock()
            .unwrap()
            .values()
            .filter(|u| u.is_near_limit())
            .map(|u| u.provider.clone())
            .collect()
    }

    /// Clear tracked usage (used in tests).
    #[cfg(test)]
    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Compact UTC ISO-8601 without pulling extra crates.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_flags_warning_at_80_pct_and_merges_windows() {
        let tracker = UsageTracker::new();
        tracker.record_report(
            "claude-code",
            &RateLimitReport {
                window_5h_pct: Some(52.0),
                reset_5h: Some("2026-11-22T22:00:00Z".into()),
                window_7d_pct: None,
                reset_7d: None,
                source: Some("claude-cli".into()),
            },
        );
        assert!(tracker.near_limit().is_empty());

        tracker.record_report(
            "claude",
            &RateLimitReport {
                window_5h_pct: None,
                reset_5h: None,
                window_7d_pct: Some(85.0),
                reset_7d: Some("2026-11-28T00:00:00Z".into()),
                source: Some("claude-cli".into()),
            },
        );
        let snap = tracker.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].provider, "claude");
        assert_eq!(snap[0].window_5h.as_ref().unwrap().pct, 52.0);
        assert_eq!(snap[0].window_7d.as_ref().unwrap().pct, 85.0);
        assert!(snap[0].warning);
        assert!(tracker.near_limit().contains("claude"));
    }
}
