//! Compiled Working Memory: 3-tier cache-conscious context projection for workers and repairs.
//!
//! Append-only conversation transcripts accumulate stale diffs, resolved diagnostics, and noisy
//! tool JSON across turns. This module projects a compact, deterministic Tier-2 state block
//! (active diff summary, deduplicated diagnostics, and a bounded Attempts Ledger) while leaving
//! Tier 0 (the shared prefix) untouched for maximum provider prompt-cache reuse.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Thresholds for triggering warm-session compaction on long repair loops.
pub const COMPACT_TURN_THRESHOLD: usize = 6;
pub const COMPACT_UNCACHED_TOKEN_THRESHOLD: u64 = 35_000;
pub const COMPACT_FAILED_REPAIRS_THRESHOLD: usize = 2;

/// One structured record of a prior build or repair attempt on a unit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttemptMemo {
    pub attempt: usize,
    pub tier: String,
    pub outcome: String,
    pub files: Vec<String>,
    pub diagnostics: Vec<String>,
}

/// Decides whether an open warm session has accumulated enough entropy that starting a clean
/// session hydrated with Tier 0 + Tier 1 + Tier 2 projection will outperform appending further.
pub fn should_compact(turns: usize, uncached_tokens: u64, failed_repairs: usize) -> bool {
    turns >= COMPACT_TURN_THRESHOLD
        || uncached_tokens >= COMPACT_UNCACHED_TOKEN_THRESHOLD
        || failed_repairs >= COMPACT_FAILED_REPAIRS_THRESHOLD
}

/// Normalize a diagnostic line for deduplication (trim whitespace, collapse runs of spaces).
fn normalize_diag(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Deduplicate diagnostics while preserving first-seen order and capping at `limit`.
pub fn dedup_diagnostics(raw: &[String], limit: usize) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for item in raw {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = normalize_diag(trimmed);
        if seen.insert(key) {
            out.push(trimmed.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// Render a compact Tier-2 Working Memory projection for a repair turn.
/// Keeps the last `max_attempts` memos, deduplicates diagnostics, and formats a crisp
/// "Do not repeat" ledger so the model focuses strictly on unresolved failures.
pub fn render_repair_projection(
    memos: &[AttemptMemo],
    active_diff_stat: Option<&str>,
    max_attempts: usize,
) -> String {
    if memos.is_empty() && active_diff_stat.map(str::trim).unwrap_or("").is_empty() {
        return String::new();
    }
    let mut out = String::from("## Compiled Working Memory (Live State)\n");

    if let Some(stat) = active_diff_stat.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("### Current diff\n```text\n");
        out.push_str(stat);
        out.push_str("\n```\n");
    }

    if !memos.is_empty() {
        let start = memos.len().saturating_sub(max_attempts.max(1));
        let slice = &memos[start..];
        out.push_str("### Attempts ledger (do not repeat failed approaches)\n");
        for memo in slice {
            let files = if memo.files.is_empty() {
                "no files".to_string()
            } else {
                memo.files.iter().take(6).cloned().collect::<Vec<_>>().join(", ")
            };
            let diags = dedup_diagnostics(&memo.diagnostics, 3);
            let reason = if diags.is_empty() {
                memo.outcome.clone()
            } else {
                diags.join(" | ")
            };
            out.push_str(&format!(
                "- Attempt #{} ({}) [{}]: {} — {}\n",
                memo.attempt, memo.tier, files, memo.outcome, reason
            ));
        }

        // Surface any recurring failure signatures across attempts explicitly.
        let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for memo in memos {
            let mut local = BTreeSet::new();
            for d in &memo.diagnostics {
                let norm = normalize_diag(d);
                if !norm.is_empty() && local.insert(norm.clone()) {
                    *counts.entry(norm).or_insert(0) += 1;
                }
            }
        }
        let recurring: Vec<&String> = counts.iter().filter(|(_, c)| **c >= 2).map(|(k, _)| k).take(3).collect();
        if !recurring.is_empty() {
            out.push_str("### Recurring errors across attempts\n");
            for err in recurring {
                out.push_str(&format!("- {err}\n"));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_triggers_on_entropy_or_repeated_repairs() {
        assert!(!should_compact(2, 10_000, 1));
        assert!(should_compact(COMPACT_TURN_THRESHOLD, 5_000, 0));
        assert!(should_compact(1, COMPACT_UNCACHED_TOKEN_THRESHOLD, 0));
        assert!(should_compact(2, 5_000, COMPACT_FAILED_REPAIRS_THRESHOLD));
    }

    #[test]
    fn projection_deduplicates_diagnostics_and_highlights_recurring_errors() {
        let memos = vec![
            AttemptMemo {
                attempt: 1,
                tier: "worker".into(),
                outcome: "check_failed".into(),
                files: vec!["src/a.ts".into()],
                diagnostics: vec![
                    "src/a.ts:4 TS2304: Cannot find name 'Foo'".into(),
                    "src/a.ts:4   TS2304:  Cannot find name 'Foo'".into(),
                ],
            },
            AttemptMemo {
                attempt: 2,
                tier: "mid".into(),
                outcome: "check_failed".into(),
                files: vec!["src/a.ts".into()],
                diagnostics: vec!["src/a.ts:4 TS2304: Cannot find name 'Foo'".into()],
            },
        ];
        let rendered = render_repair_projection(&memos, Some(" src/a.ts | 4 ++--"), 5);
        assert!(rendered.contains("Compiled Working Memory"), "{rendered}");
        assert!(rendered.contains("src/a.ts | 4 ++--"), "{rendered}");
        assert!(rendered.contains("Attempt #1 (worker)"), "{rendered}");
        assert!(rendered.contains("Attempt #2 (mid)"), "{rendered}");
        assert!(rendered.contains("Recurring errors across attempts"), "{rendered}");
        assert_eq!(
            rendered.matches("Attempt #1").count(),
            1,
            "deduped attempt line should appear once"
        );
    }
}
