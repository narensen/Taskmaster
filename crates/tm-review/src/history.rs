//! The record of past reviews, kept in the reviewed repository.
//!
//! A reviewer is shown what earlier reviews said about the same files: a finding that was
//! rejected should not be raised again, and one that was confirmed may have come back.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::HISTORY_FILE;
use crate::findings::Finding;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The verifier agreed it is real.
    Confirmed,
    /// The verifier disproved it.
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judged {
    #[serde(flatten)]
    pub finding: Finding,
    pub outcome: Outcome,
    /// The verifier's reason, when it rejected the finding.
    pub why: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub at: String,
    pub base: String,
    pub head: String,
    pub reviewer: String,
    pub verifier: String,
    pub findings: Vec<Judged>,
}

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn open(repo: &Path) -> Self {
        History { path: repo.join(HISTORY_FILE) }
    }

    pub fn append(&self, record: &Record) -> anyhow::Result<()> {
        let dir = self.path.parent().context("history file has no parent directory")?;
        std::fs::create_dir_all(dir)?;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(record)?)?;
        Ok(())
    }

    /// Every record, oldest first. A line that does not parse is skipped, so one damaged
    /// line cannot hide the rest of the history.
    pub fn records(&self) -> Vec<Record> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
    }
}

/// What earlier reviews found in `files`, newest first, for the reviewer to read.
pub fn render(records: &[Record], files: &[String], limit: usize) -> String {
    let mut out = String::from(
        "# Earlier reviews of these files\n\nDo not raise a rejected finding again unless the code \
         changed. Check whether a confirmed one has come back.\n",
    );
    let mut shown = 0;
    for record in records.iter().rev() {
        for judged in record.findings.iter().filter(|j| files.contains(&j.finding.file)) {
            if shown == limit {
                return out;
            }
            shown += 1;
            let head: String = record.head.chars().take(10).collect();
            let outcome = match judged.outcome {
                Outcome::Confirmed => "CONFIRMED".to_string(),
                Outcome::Rejected => {
                    format!("REJECTED ({})", judged.why.as_deref().unwrap_or("no reason recorded"))
                }
            };
            let f = &judged.finding;
            out.push_str(&format!(
                "\n- {} at `{head}`: `{}`:{} {} — {outcome}\n",
                record.at, f.file, f.line, f.failure
            ));
        }
    }
    if shown == 0 {
        out.push_str("\nNo earlier review found anything in these files.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Kind;

    fn record(at: &str, findings: &[(&str, Outcome)]) -> Record {
        Record {
            at: at.into(),
            base: "b".repeat(40),
            head: "h".repeat(40),
            reviewer: "claude-code".into(),
            verifier: "antigravity".into(),
            findings: findings
                .iter()
                .map(|(file, outcome)| Judged {
                    finding: Finding {
                        kind: Kind::Bug,
                        file: file.to_string(),
                        line: 7,
                        failure: format!("failure in {file} at {at}"),
                    },
                    outcome: *outcome,
                    why: (*outcome == Outcome::Rejected).then(|| "already handled".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn records_survive_a_round_trip_and_a_damaged_line() {
        let repo = std::env::temp_dir().join(format!("tm-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        let history = History::open(&repo);
        assert!(history.records().is_empty());
        let first = record("t1", &[("a.ts", Outcome::Confirmed)]);
        history.append(&first).unwrap();
        std::fs::OpenOptions::new().append(true).open(repo.join(HISTORY_FILE)).unwrap().write_all(b"{broken\n").unwrap();
        let second = record("t2", &[("b.ts", Outcome::Rejected)]);
        history.append(&second).unwrap();
        assert_eq!(history.records(), vec![first, second]);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn shows_only_the_files_under_review_newest_first() {
        let records = [
            record("t1", &[("a.ts", Outcome::Confirmed), ("other.ts", Outcome::Confirmed)]),
            record("t2", &[("a.ts", Outcome::Rejected)]),
        ];
        let doc = render(&records, &["a.ts".into()], 10);
        assert!(!doc.contains("other.ts"));
        assert!(doc.find("at t2").unwrap() < doc.find("at t1").unwrap());
        assert!(doc.contains("REJECTED (already handled)") && doc.contains("CONFIRMED"));
        assert_eq!(render(&records, &["a.ts".into()], 1).matches("\n- ").count(), 1);
        assert!(render(&records, &["new.ts".into()], 10).contains("No earlier review"));
    }
}
