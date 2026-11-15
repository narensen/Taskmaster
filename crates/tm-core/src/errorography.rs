//! Mistakes, recorded separately for each model.
//!
//! A warning or a fix learned for one model is never shown to another. An entry is
//! promoted once that model has made the same mistake [`PROMOTE_AT`] times. Promoted
//! entries are the ones a later prompt for that model is warned about, and the ones
//! whose recorded fix is handed back as an auto-fix.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Times one model must repeat a mistake before it is promoted.
pub const PROMOTE_AT: u64 = 3;

/// One recurring failure for a single model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mistake {
    pub signature: String,
    /// The latest concrete failure text.
    pub sample: String,
    pub count: u64,
    /// What worked the last time this model got past the failure.
    #[serde(default)]
    pub fix: Option<String>,
}

impl Mistake {
    pub fn promoted(&self) -> bool {
        self.count >= PROMOTE_AT
    }
}

/// Every model's mistakes. The map key is the model byline, such as `agy` or `agy haiku`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Book {
    pub models: BTreeMap<String, Vec<Mistake>>,
}

impl Book {
    /// Count one failure against `model` only.
    pub fn record(&mut self, model: &str, failure: &str) {
        let signature = signature(failure);
        if signature.is_empty() {
            return;
        }
        let sample = clip(failure.trim(), 400);
        let list = self.models.entry(model.to_string()).or_default();
        if let Some(found) = list.iter_mut().find(|mistake| mistake.signature == signature) {
            found.count += 1;
            found.sample = sample;
            return;
        }
        list.push(Mistake { signature, sample, count: 1, fix: None });
    }

    /// Remember a fix for a failure this model already has on record.
    /// Short replies are not a fix.
    pub fn learn_fix(&mut self, model: &str, failure: &str, fix: &str) {
        let fix = fix.trim();
        if fix.chars().count() < 20 {
            return;
        }
        let signature = signature(failure);
        let Some(list) = self.models.get_mut(model) else { return };
        if let Some(found) = list.iter_mut().find(|mistake| mistake.signature == signature) {
            found.fix = Some(clip(fix, 400));
        }
    }

    /// Promoted mistakes for this model, and no others.
    pub fn warnings(&self, model: &str) -> Vec<&Mistake> {
        self.models
            .get(model)
            .map(|list| list.iter().filter(|mistake| mistake.promoted()).collect())
            .unwrap_or_default()
    }

    /// The recorded fix when this model has a promoted mistake matching `failure`.
    pub fn fix_for(&self, model: &str, failure: &str) -> Option<&str> {
        let signature = signature(failure);
        self.warnings(model)
            .into_iter()
            .find(|mistake| mistake.signature == signature)
            .and_then(|mistake| mistake.fix.as_deref())
    }
}

/// Group the same failure across files and line numbers.
pub fn signature(failure: &str) -> String {
    let mut out = String::new();
    for token in failure.to_lowercase().split_whitespace() {
        let bare = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if bare.is_empty() || bare.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if bare.contains('/') || bare.contains('\\') {
            continue;
        }
        let bare = match bare.rsplit_once(':') {
            Some((name, line)) if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) => name,
            _ => bare,
        };
        if is_filename(bare) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(bare);
        if out.len() >= 180 {
            break;
        }
    }
    out
}

fn is_filename(token: &str) -> bool {
    let Some((name, ext)) = token.rsplit_once('.') else { return false };
    !name.is_empty()
        && !ext.is_empty()
        && ext.len() <= 4
        && ext.chars().all(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_failure_groups_across_files_and_lines() {
        let a = signature("review: app/a/page.ts:12 empty id renders a blank header for guests");
        let b = signature("review: app/b/page.ts:40 empty id renders a blank header for guests");
        assert_eq!(a, b);
        assert_ne!(a, signature("forgets the route param"));
    }

    #[test]
    fn warnings_and_fixes_stay_with_one_model() {
        let mut book = Book::default();
        for _ in 0..PROMOTE_AT {
            book.record("agy", "review: app/a/page.ts:1 empty id renders a blank header");
        }
        book.record("claude", "review: app/a/page.ts:1 empty id renders a blank header");
        book.learn_fix(
            "agy",
            "review: app/a/page.ts:1 empty id renders a blank header",
            "Read the route before rendering the header.",
        );
        assert_eq!(book.warnings("agy").len(), 1);
        assert!(book.warnings("claude").is_empty());
        assert!(book.warnings("grok").is_empty());
        assert_eq!(
            book.fix_for("agy", "review: app/b/page.ts:9 empty id renders a blank header"),
            Some("Read the route before rendering the header.")
        );
        assert_eq!(book.fix_for("claude", "review: app/a/page.ts:1 empty id renders a blank header"), None);
    }

    #[test]
    fn a_short_reply_is_not_stored_as_a_fix() {
        let mut book = Book::default();
        book.record("agy", "worker produced no changes under its owned files");
        book.learn_fix("agy", "worker produced no changes under its owned files", "done");
        assert_eq!(book.models["agy"][0].fix, None);
    }
}
