//! Scheduled recurring runs (nightly dependency bumps, flaky-test sweeps, docs refresh) (Item 4.6).
//!
//! Persists schedules in `.taskmaster/schedules.json` and supports presets (`hourly`,
//! `nightly`, `weekly`) as well as standard 5-field cron expressions (`min hour dom month dow`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledRun {
    pub id: String,
    pub name: String,
    /// `hourly`, `nightly`, `weekly`, or a 5-field cron expression (`0 2 * * *`).
    pub schedule: String,
    pub prompt: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<String>,
}

fn default_true() -> bool {
    true
}

fn store_path(project: &Path) -> PathBuf {
    project.join(".taskmaster").join("schedules.json")
}

pub fn list(project: &Path) -> Vec<ScheduledRun> {
    let Ok(bytes) = std::fs::read(store_path(project)) else { return Vec::new() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_all(project: &Path, items: &[ScheduledRun]) -> anyhow::Result<()> {
    let path = store_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(items)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Expand preset keywords into standard 5-field cron strings and validate syntax.
pub fn normalize_cron(expr: &str) -> anyhow::Result<String> {
    let trimmed = expr.trim();
    let expanded = match trimmed.to_ascii_lowercase().as_str() {
        "hourly" | "@hourly" => "0 * * * *",
        "nightly" | "daily" | "@daily" => "0 2 * * *",
        "weekly" | "@weekly" => "0 3 * * 1",
        _ => trimmed,
    };
    let fields: Vec<&str> = expanded.split_whitespace().collect();
    if fields.len() != 5 {
        anyhow::bail!("schedule must be `hourly`, `nightly`, `weekly`, or a 5-field cron expression");
    }
    for (idx, f) in fields.iter().enumerate() {
        let (min, max) = match idx {
            0 => (0u32, 59u32),
            1 => (0, 23),
            2 => (1, 31),
            3 => (1, 12),
            _ => (0, 6),
        };
        if !valid_cron_field(f, min, max) {
            anyhow::bail!("invalid cron field `{f}` at position {}", idx + 1);
        }
    }
    Ok(expanded.to_string())
}

fn valid_cron_field(field: &str, min: u32, max: u32) -> bool {
    for part in field.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => {
                let Ok(step_val) = s.parse::<u32>() else { return false };
                if step_val == 0 {
                    return false;
                }
                (b, Some(step_val))
            }
            None => (part, None),
        };
        let _ = step;
        if base == "*" {
            continue;
        }
        if let Some((a, b)) = base.split_once('-') {
            let (Ok(lo), Ok(hi)) = (a.parse::<u32>(), b.parse::<u32>()) else { return false };
            if lo < min || hi > max || lo > hi {
                return false;
            }
        } else {
            let Ok(v) = base.parse::<u32>() else { return false };
            if v < min || v > max {
                return false;
            }
        }
    }
    true
}

/// Evaluate whether a normalized 5-field cron expression matches `(min, hour, dom, month, dow)`.
pub fn matches_cron(expr: &str, min: u32, hour: u32, dom: u32, month: u32, dow: u32) -> bool {
    let Ok(norm) = normalize_cron(expr) else { return false };
    let parts: Vec<&str> = norm.split_whitespace().collect();
    let vals = [min, hour, dom, month, dow];
    parts.iter().zip(vals).all(|(f, v)| field_matches(f, v))
}

fn field_matches(field: &str, value: u32) -> bool {
    for part in field.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => (b, s.parse::<u32>().ok().filter(|&n| n > 0)),
            None => (part, None),
        };
        let in_range = if base == "*" {
            true
        } else if let Some((a, b)) = base.split_once('-') {
            a.parse::<u32>().ok().zip(b.parse::<u32>().ok()).is_some_and(|(lo, hi)| value >= lo && value <= hi)
        } else {
            base.parse::<u32>().ok() == Some(value)
        };
        if !in_range {
            continue;
        }
        if let Some(step) = step {
            let start = base.split('-').next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            if (value.saturating_sub(start)).is_multiple_of(step) {
                return true;
            }
        } else {
            return true;
        }
    }
    false
}

pub fn upsert(project: &Path, mut item: ScheduledRun) -> anyhow::Result<ScheduledRun> {
    if item.name.trim().is_empty() {
        anyhow::bail!("schedule name cannot be empty");
    }
    if item.prompt.trim().is_empty() {
        anyhow::bail!("schedule prompt cannot be empty");
    }
    item.schedule = normalize_cron(&item.schedule)?;
    if item.id.trim().is_empty() {
        item.id = crate::secret::random_hex(5)?;
    }
    let mut items = list(project);
    if let Some(existing) = items.iter_mut().find(|s| s.id == item.id) {
        *existing = item.clone();
    } else {
        items.push(item.clone());
    }
    save_all(project, &items)?;
    Ok(item)
}

pub fn toggle(project: &Path, id: &str, enabled: bool) -> anyhow::Result<Option<ScheduledRun>> {
    let mut items = list(project);
    let mut updated = None;
    for item in &mut items {
        if item.id == id {
            item.enabled = enabled;
            updated = Some(item.clone());
        }
    }
    if updated.is_some() {
        save_all(project, &items)?;
    }
    Ok(updated)
}

pub fn remove(project: &Path, id: &str) -> anyhow::Result<bool> {
    let mut items = list(project);
    let before = items.len();
    items.retain(|s| s.id != id);
    save_all(project, &items)?;
    Ok(items.len() != before)
}

pub fn mark_ran(project: &Path, id: &str, run_id: &str) -> anyhow::Result<()> {
    let mut items = list(project);
    for item in &mut items {
        if item.id == id {
            item.last_run_at = Some(crate::time::rfc3339_now());
            item.last_run_id = Some(run_id.to_string());
        }
    }
    save_all(project, &items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_matching_and_schedule_crud_work() {
        assert_eq!(normalize_cron("nightly").unwrap(), "0 2 * * *");
        assert!(matches_cron("nightly", 0, 2, 15, 6, 3));
        assert!(!matches_cron("nightly", 1, 2, 15, 6, 3));
        assert!(matches_cron("*/15 9-17 * * 1-5", 30, 14, 1, 1, 3));
        assert!(!matches_cron("*/15 9-17 * * 1-5", 30, 14, 1, 1, 0));

        let dir = std::env::temp_dir().join(format!("tm-sched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let saved = upsert(
            &dir,
            ScheduledRun {
                id: String::new(),
                name: "Nightly flaky sweep".into(),
                schedule: "nightly".into(),
                prompt: "Run the test suite and fix flaky tests.".into(),
                enabled: true,
                auto_approve: false,
                max_cost_usd: Some(2.0),
                last_run_at: None,
                last_run_id: None,
            },
        )
        .unwrap();
        assert_eq!(list(&dir).len(), 1);
        let toggled = toggle(&dir, &saved.id, false).unwrap().unwrap();
        assert!(!toggled.enabled);
        assert!(remove(&dir, &saved.id).unwrap());
        assert!(list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
