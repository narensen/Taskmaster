//! A9: switch a role's provider on a run that is not working, with a handoff.
//!
//! The new provider cannot resume the old one's session, so the next agent gets a compact
//! handoff instead: the plan, the decisions locked in it, what the run changed, what is
//! still failing, and the agents' last words. It is written when the switch is made and
//! read once, by the first agent of the next turn.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_core::{Event, Plan, Run, WorkerOutcome};

/// `runs/<id>/handoff.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Handoff {
    /// Seat → `harness` or `harness · model`, before and after the switch.
    pub from: BTreeMap<String, String>,
    pub to: BTreeMap<String, String>,
    pub text: String,
    pub at: String,
    /// Set once an agent was given it.
    #[serde(default)]
    pub used: bool,
}

pub struct Facts<'a> {
    pub plan: Option<&'a Plan>,
    pub run: &'a Run,
    pub events: &'a [Event],
    pub diff_stat: String,
    pub error: Option<&'a str>,
    /// The agents' last words, newest last.
    pub said: Vec<String>,
}

/// The handoff text, kept to a few thousand characters.
pub fn compile(facts: &Facts, from: &BTreeMap<String, String>, to: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let switched: Vec<String> = to
        .iter()
        .map(|(seat, now)| format!("{seat}: {} → {now}", from.get(seat).map(String::as_str).unwrap_or("its default")))
        .collect();
    out.push_str(&format!("Providers were switched on this run ({}). You start fresh; this is what the earlier agents left.\n", switched.join("; ")));

    if let Some(plan) = facts.plan {
        out.push_str("\nPlan:\n");
        for page in plan.pages.iter().take(20) {
            let state = facts.run.pages.get(&page.id).map(|s| format!("{:?}", s.state).to_lowercase()).unwrap_or_else(|| "not built".into());
            let route = if page.route.trim().is_empty() { String::new() } else { format!(" {}", page.route.trim()) };
            out.push_str(&format!("- {}{route} owns {} ({state})\n", page.id, page.owned_glob));
        }
        if !plan.contracts.is_empty() {
            out.push_str("\nDecisions locked in the shared contracts (keep them):\n");
            for contract in plan.contracts.iter().take(12) {
                out.push_str(&format!("- {}\n", contract.path));
            }
        }
    }

    let stat = facts.diff_stat.trim();
    if !stat.is_empty() {
        let stat: String = stat.lines().rev().take(25).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        out.push_str(&format!("\nWhat the run changed:\n{stat}\n"));
    }

    let failures = open_failures(facts.events, facts.run);
    if !failures.is_empty() || facts.error.is_some() {
        out.push_str("\nStill failing:\n");
        if let Some(error) = facts.error.map(str::trim).filter(|e| !e.is_empty()) {
            out.push_str(&format!("- the run: {}\n", error.chars().take(400).collect::<String>()));
        }
        for line in failures {
            out.push_str(&format!("- {line}\n"));
        }
    }

    if !facts.said.is_empty() {
        out.push_str("\nThe earlier agents' last words:\n");
        for words in &facts.said {
            let words: String = words.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect();
            out.push_str(&format!("- {words}\n"));
        }
    }
    out.chars().take(6000).collect()
}

/// Pages that have not passed, with the last errors their checks gave.
fn open_failures(events: &[Event], run: &Run) -> Vec<String> {
    let mut last: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for event in events {
        if let Event::WorkerFinished { page, outcome } = event {
            match outcome {
                WorkerOutcome::CheckFailed { errors } => {
                    let lines: Vec<String> = errors.lines().map(str::to_string).collect();
                    last.insert(page.clone(), crate::memory::dedup_diagnostics(&lines, 3));
                }
                WorkerOutcome::Blocked { reason } => {
                    last.insert(page.clone(), vec![reason.clone()]);
                }
                WorkerOutcome::Passed => {
                    last.remove(page);
                }
                _ => {}
            }
        }
    }
    run.pages
        .iter()
        .filter(|(_, state)| state.state != tm_core::PageState::Passed)
        .map(|(page, _)| match last.get(page) {
            Some(errors) if !errors.is_empty() => format!("{page}: {}", errors.join(" | ").chars().take(400).collect::<String>()),
            _ => format!("{page}: not passed"),
        })
        .take(12)
        .collect()
}

/// `git diff --stat` of what the run changed: its branch against where it started, or its
/// merge commit once merged.
pub fn diff_stat(project: &Path, status: &Value, base: Option<&str>, branch: Option<&str>) -> String {
    let range = match (status["merge_commit"].as_str(), base, branch) {
        (Some(merge), _, _) => format!("{merge}^1..{merge}"),
        (None, Some(base), Some(branch)) => format!("{base}..{branch}"),
        _ => return String::new(),
    };
    std::process::Command::new("git")
        .args(["diff", "--stat", "--stat-width=100", &range])
        .current_dir(project)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// The pending handoff's text, marked used so only the first agent of the turn gets it.
pub fn take(records: &crate::store::Records, run: &str) -> Option<String> {
    let mut handoff = records.handoff(run).ok().flatten().filter(|h| !h.used)?;
    handoff.used = true;
    records.save_handoff(run, &handoff).ok()?;
    Some(handoff.text)
}

/// The handoff for a brief: the text once, then never again.
pub fn brief_section(text: &str) -> String {
    format!("\n\nHandoff from the earlier provider:\n{}\n", text.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_names_the_switch_the_plan_the_failures_and_the_last_words() {
        let run = Run::replay_with(1, false, vec![]);
        let from = BTreeMap::from([("worker".to_string(), "claude".to_string())]);
        let to = BTreeMap::from([("worker".to_string(), "codex · gpt-5".to_string())]);
        let facts = Facts {
            plan: None,
            run: &run,
            events: &[],
            diff_stat: " app/a/page.tsx | 12 ++++++\n 1 file changed, 12 insertions(+)\n".into(),
            error: Some("the build failed: Cannot find module './theme'"),
            said: vec!["I could not find the theme file.".into()],
        };
        let text = compile(&facts, &from, &to);
        assert!(text.contains("worker: claude → codex · gpt-5"), "{text}");
        assert!(text.contains("1 file changed"));
        assert!(text.contains("Cannot find module './theme'"));
        assert!(text.contains("I could not find the theme file."));
        assert!(brief_section(&text).starts_with("\n\nHandoff from the earlier provider:\n"));
    }
}
