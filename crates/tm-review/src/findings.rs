//! Findings, and reading them out of what an agent wrote.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bug,
    Security,
    /// Breaks one of the project's own review rules.
    Rule,
    /// Produced by a linter (clippy, eslint, ruff), not by a reviewer.
    Lint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: Kind,
    pub file: String,
    pub line: u32,
    /// What input or state goes wrong, and what happens.
    pub failure: String,
}

impl Finding {
    /// A finding needs a place and a concrete failure, or nobody can check it.
    pub fn is_actionable(&self) -> bool {
        !self.file.trim().is_empty() && self.line > 0 && self.failure.trim().len() >= 10
    }
}

/// Read a reviewer's reply. `None` means the reply contained no findings list at all, which
/// is a failed review, not a clean one. An empty list is a clean review.
pub fn parse_findings(reply: &str, default_kind: Kind) -> Option<Vec<Finding>> {
    let array = last_json(reply, '[', ']')?;
    let items = array.as_array()?;
    Some(
        items
            .iter()
            .filter_map(|item| {
                let kind = match item["kind"].as_str() {
                    Some("security") => Kind::Security,
                    Some("rule") => Kind::Rule,
                    Some("bug") => Kind::Bug,
                    Some("lint") => Kind::Lint,
                    _ => default_kind,
                };
                // Models write the line as a number or as a string.
                let line = item["line"]
                    .as_u64()
                    .or_else(|| item["line"].as_str().and_then(|l| l.trim().parse().ok()))?;
                Some(Finding {
                    kind,
                    file: item["file"].as_str()?.trim().trim_start_matches("./").to_string(),
                    line: u32::try_from(line).ok()?,
                    failure: item["failure"].as_str()?.trim().to_string(),
                })
            })
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub real: bool,
    pub why: Option<String>,
}

/// Read a verifier's reply. No readable verdict counts as "not confirmed".
pub fn parse_verdict(reply: &str) -> Verdict {
    let parsed = last_json(reply, '{', '}').and_then(|v| {
        Some(Verdict { real: v["real"].as_bool()?, why: v["why"].as_str().map(str::to_string) })
    });
    parsed.unwrap_or(Verdict { real: false, why: Some("the verifier gave no readable verdict".into()) })
}

/// The last complete JSON value in `text` that starts with `open` and ends with `close`.
/// Agents often wrap their answer in prose or a code fence.
fn last_json(text: &str, open: char, close: char) -> Option<Value> {
    let end = text.rfind(close)?;
    let slice = &text[..=end];
    // The earliest opening bracket that yields valid JSON through `end` is the whole value;
    // earlier brackets belong to the prose around it.
    slice
        .char_indices()
        .filter(|(_, c)| *c == open)
        .find_map(|(start, _)| serde_json::from_str::<Value>(&slice[start..]).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_findings_from_a_fenced_reply_with_prose() {
        let reply = "I read the files [twice].\n```json\n[\n {\"file\": \"./lib/money.ts\", \"line\": \"12\", \
                     \"failure\": \"total(0.1, 0.2) returns 0.30000000000000004\"},\n {\"file\": \"a.ts\", \
                     \"line\": 3, \"kind\": \"rule\", \"failure\": \"default export breaks the src rule\"}\n]\n```\nDone.";
        let found = parse_findings(reply, Kind::Bug).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].file.as_str(), found[0].line, found[0].kind), ("lib/money.ts", 12, Kind::Bug));
        assert_eq!(found[1].kind, Kind::Rule);
    }

    #[test]
    fn an_empty_list_is_clean_but_no_list_is_a_failed_review() {
        assert_eq!(parse_findings("Nothing wrong.\n[]", Kind::Bug), Some(vec![]));
        assert_eq!(parse_findings("I could not read the files.", Kind::Bug), None);
    }

    #[test]
    fn malformed_items_are_skipped_and_vague_ones_are_not_actionable() {
        let reply = r#"[{"file": "a.ts"}, {"file": "a.ts", "line": 0, "failure": "bad"}, {"file": "a.ts", "line": -4, "failure": "x"}]"#;
        let found = parse_findings(reply, Kind::Security).unwrap();
        assert_eq!(found.len(), 1);
        assert!(!found[0].is_actionable());
        assert_eq!(found[0].kind, Kind::Security);
    }

    #[test]
    fn reads_verdicts_and_treats_silence_as_not_confirmed() {
        assert!(parse_verdict("Checked. {\"real\": true}").real);
        let no = parse_verdict("```json\n{\"real\": false, \"why\": \"line 12 already rounds\"}\n```");
        assert_eq!((no.real, no.why.as_deref()), (false, Some("line 12 already rounds")));
        assert!(!parse_verdict("It looks real to me.").real);
        assert!(!parse_verdict("{\"real\": \"yes\"}").real);
    }
}
