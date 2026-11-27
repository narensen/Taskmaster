//! Mistakes, recorded separately for each model, and project notes shared by all of them.
//!
//! A warning or a fix learned for one model is never shown to another. An entry is
//! promoted once that model has made the same mistake [`PROMOTE_AT`] times. Promoted
//! entries are the ones a later prompt for that model is warned about, and the ones
//! whose recorded fix is handed back as an auto-fix.
//!
//! A check failure that names its tool and code (`tsc: TS2304: ...`, `ruff: F401: ...`,
//! `clippy: E0308: ...`, `eslint: no-unused-vars: ...`) is counted once per code, whatever
//! the identifiers and files in the message. Anything else is grouped by its words.

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

/// A repo-wide build, test, or environment discovery shared across ALL models immediately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectNote {
    pub signature: String,
    pub note: String,
    #[serde(default = "one")]
    pub count: u64,
}

fn one() -> u64 {
    1
}

/// Every model's mistakes plus shared project-wide memory. The map key in `models` is the
/// model byline, such as `agy` or `agy haiku`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Book {
    pub models: BTreeMap<String, Vec<Mistake>>,
    /// Project-level discoveries (toolchain/test commands, env gotchas, verified build fixes)
    /// shared across every model from the very first run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_notes: Vec<ProjectNote>,
}

/// Maximum number of shared project notes kept in the prompt prefix.
pub const MAX_PROJECT_NOTES: usize = 12;

impl Book {
    /// Record a project-wide lesson shared across all models (promoted immediately on first
    /// hit). `key` names the quirk, such as the failing step and its first line of output, so
    /// the same quirk fixed in other words is one note.
    pub fn note_project(&mut self, key: &str, note: &str) {
        let note = note.trim();
        if note.chars().count() < 16 {
            return;
        }
        let sig = signature(key);
        if sig.is_empty() {
            return;
        }
        let clipped = clip(note, 280);
        if let Some(found) = self.project_notes.iter_mut().find(|n| n.signature == sig) {
            found.count += 1;
            found.note = clipped;
            return;
        }
        self.project_notes.push(ProjectNote { signature: sig, note: clipped, count: 1 });
        if self.project_notes.len() > MAX_PROJECT_NOTES * 2 {
            self.project_notes.sort_by_key(|a| std::cmp::Reverse(a.count));
            self.project_notes.truncate(MAX_PROJECT_NOTES);
        }
    }

    /// Top shared project notes for prompt injection (highest frequency first).
    pub fn project_memories(&self) -> Vec<&ProjectNote> {
        let mut list: Vec<&ProjectNote> = self.project_notes.iter().collect();
        list.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.signature.cmp(&b.signature)));
        list.truncate(MAX_PROJECT_NOTES);
        list
    }

    /// Count one failure against `model` only: once per error code it names, or once by
    /// its words when it names none.
    pub fn record(&mut self, model: &str, failure: &str) {
        for (signature, sample) in classify(failure) {
            let list = self.models.entry(model.to_string()).or_default();
            if let Some(found) = list.iter_mut().find(|mistake| mistake.signature == signature) {
                found.count += 1;
                found.sample = sample;
                continue;
            }
            list.push(Mistake { signature, sample, count: 1, fix: None });
        }
    }

    /// Remember a fix for a failure this model already has on record.
    /// Short replies are not a fix.
    pub fn learn_fix(&mut self, model: &str, failure: &str, fix: &str) {
        let fix = fix.trim();
        if fix.chars().count() < 20 {
            return;
        }
        let Some(list) = self.models.get_mut(model) else { return };
        for (signature, _) in classify(failure) {
            if let Some(found) = list.iter_mut().find(|mistake| mistake.signature == signature) {
                found.fix = Some(clip(fix, 400));
            }
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
        let warnings = self.warnings(model);
        classify(failure).into_iter().find_map(|(signature, _)| {
            warnings.iter().find(|mistake| mistake.signature == signature).and_then(|mistake| mistake.fix.as_deref())
        })
    }
}

/// A project lesson from a failure that is now fixed: the key and the note for
/// [`Book::note_project`]. Only a failed build or test step (``` `npm test` failed: ```) is a
/// lesson about the repository; a type error or a review finding is the model's own.
pub fn project_lesson(failure: &str, fix: &str) -> Option<(String, String)> {
    let mut lines = failure.lines().map(str::trim);
    let step = lines.by_ref().find_map(|line| line.strip_prefix('`')?.strip_suffix("` failed:"))?;
    let first = lines.find(|line| !line.is_empty()).map(|line| clip(line, 120)).unwrap_or_default();
    let said: String = fix.lines().map(str::trim).filter(|line| !line.is_empty()).take(2).collect::<Vec<_>>().join(" ");
    if said.chars().count() < 20 {
        return None;
    }
    let key = format!("{step} {first}");
    let note = if first.is_empty() { format!("`{step}` failed; fixed by: {}", clip(&said, 200)) } else { format!("`{step}` failed with `{first}`; fixed by: {}", clip(&said, 200)) };
    Some((key, note))
}

/// Coded lines a repair is shown for any one error code.
pub const DIGEST_PER_CODE: usize = 3;
/// Coded lines a repair is shown in all.
pub const DIGEST_CODED: usize = 40;

/// Check output as a repair is shown it: the same line once, at most [`DIGEST_PER_CODE`]
/// examples of each error code and [`DIGEST_CODED`] coded lines in all, then a count of what
/// was left out. Lines that name no code (test output, a stack trace) are kept as they are.
pub fn digest(errors: &str) -> String {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut per_code: BTreeMap<String, usize> = BTreeMap::new();
    let mut left_out: BTreeMap<String, usize> = BTreeMap::new();
    let mut coded = 0;
    let mut out = String::new();
    for line in errors.lines() {
        let Some((code, _)) = error_code(line) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        if !seen.insert(line.trim()) {
            continue;
        }
        let shown = per_code.entry(code.clone()).or_default();
        if *shown >= DIGEST_PER_CODE || coded >= DIGEST_CODED {
            *left_out.entry(code).or_default() += 1;
            continue;
        }
        *shown += 1;
        coded += 1;
        out.push_str(line);
        out.push('\n');
    }
    if !left_out.is_empty() {
        let total: usize = left_out.values().sum();
        let mut counts: Vec<(String, usize)> = left_out.into_iter().collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let list: Vec<String> = counts.iter().map(|(code, n)| format!("{code} ×{n}")).collect();
        out.push_str(&format!("…and {total} more like these, not shown: {}. Fix the cause, and they go too.\n", list.join(", ")));
    }
    if !errors.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Check tools whose failures read `<tool>: <code>: <message>`.
const CODED_TOOLS: [&str; 4] = ["tsc", "eslint", "ruff", "clippy"];

/// The most codes one failure is counted under.
const MAX_CODES: usize = 8;

/// What one failure is counted as: `(signature, sample)` per error code it names, in the
/// order they first appear, or its word [`signature`] when it names none.
pub fn classify(failure: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in failure.lines() {
        let Some((code, at)) = error_code(line) else { continue };
        if out.iter().any(|(sig, _)| *sig == code) {
            continue;
        }
        out.push((code, clip(line[at..].trim(), 400)));
        if out.len() == MAX_CODES {
            break;
        }
    }
    if out.is_empty() {
        let signature = signature(failure);
        if !signature.is_empty() {
            out.push((signature, clip(failure.trim(), 400)));
        }
    }
    out
}

/// `tsc TS2304` for `app/a.ts:3 tsc: TS2304: Cannot find name 'x'.`, with the byte offset the
/// tool's name starts at, so the sample can leave the file and line out.
fn error_code(line: &str) -> Option<(String, usize)> {
    let parts: Vec<&str> = line.split(": ").collect();
    let mut offset = 0;
    for pair in parts.windows(2) {
        let (head, code) = (pair[0], pair[1].trim());
        let word = head.rsplit(' ').next().unwrap_or(head);
        let named = CODED_TOOLS.iter().copied().find(|t| *t == word).map(|tool| (tool, head.len() - word.len()));
        // `src/lib.rs:4 clippy error outside the diff, in a file this change can break: E0308: ...`
        let found = named.or_else(|| head.find("clippy error ").map(|at| ("clippy", at)));
        if let Some((tool, at)) = found
            && is_code(code)
        {
            return Some((format!("{tool} {code}"), offset + at));
        }
        offset += head.len() + 2;
    }
    None
}

/// An error code or rule id: `TS2304`, `E0308`, `F401`, `clippy::needless_return`,
/// `no-unused-vars`, `@typescript-eslint/no-explicit-any`.
fn is_code(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.chars().any(|c| c.is_ascii_alphabetic())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | ':' | '@' | '.'))
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
    fn check_failures_are_counted_by_their_code_not_their_words() {
        let mut book = Book::default();
        book.record("agy", "app/a/page.ts:3 tsc: TS2304: Cannot find name 'user'.");
        book.record("agy", "app/b/page.ts:9 tsc: TS2304: Cannot find name 'session'.\napp/b/page.ts:12 ruff: F401: `os` imported but unused");
        book.record(
            "agy",
            "src/lib.rs:4 clippy error outside the diff, in a file this change can break: E0308: mismatched types\nsrc/a.rs:1 tsc: TS2304: Cannot find name 'cart'.",
        );
        let list = &book.models["agy"];
        let ts = list.iter().find(|m| m.signature == "tsc TS2304").expect("one entry for the code");
        assert_eq!(ts.count, 3);
        assert_eq!(ts.sample, "tsc: TS2304: Cannot find name 'cart'.");
        assert!(ts.promoted());
        assert!(list.iter().any(|m| m.signature == "ruff F401" && m.count == 1));
        let rust = list.iter().find(|m| m.signature == "clippy E0308").unwrap();
        assert!(rust.sample.starts_with("clippy error outside the diff"), "{}", rust.sample);
        assert_eq!(list.len(), 3);

        book.learn_fix("agy", "app/c/page.ts:1 tsc: TS2304: Cannot find name 'x'.", "Import every name from the contract before using it.");
        assert_eq!(
            book.fix_for("agy", "app/z/page.ts:7 tsc: TS2304: Cannot find name 'cartTotal'."),
            Some("Import every name from the contract before using it.")
        );
        assert_eq!(
            classify("eslint: @typescript-eslint/no-explicit-any: Unexpected any")[0].0,
            "eslint @typescript-eslint/no-explicit-any"
        );
        assert_eq!(classify("clippy: clippy::needless_return: unneeded `return`")[0].0, "clippy clippy::needless_return");
        // Text with no code keeps the word signature.
        assert_eq!(classify("review: app/a/page.ts:12 empty id renders a blank header")[0].0, signature("review: empty id renders a blank header"));
    }

    #[test]
    fn only_a_fixed_build_or_test_step_is_a_project_lesson() {
        let failure = "app/a.ts:1 tsc: TS2304: Cannot find name 'x'.\n`npm test` failed:\n\nError: Cannot find module 'vitest/config'\n  at x";
        let (key, note) = project_lesson(failure, "The tests need `vitest` from devDependencies; import the config from `vitest/config`.").unwrap();
        assert_eq!(key, "npm test Error: Cannot find module 'vitest/config'");
        assert!(note.starts_with("`npm test` failed with `Error: Cannot find module 'vitest/config'`; fixed by: The tests need"), "{note}");
        assert_eq!(project_lesson("app/a.ts:1 tsc: TS2304: Cannot find name 'x'.", "Imported the name from the contract file."), None);
        assert_eq!(project_lesson("`npm test` failed:\nboom", "done"), None, "a short reply is not a fix");

        let mut book = Book::default();
        book.note_project(&key, &note);
        book.note_project(&key, "`npm test` failed again; fixed by: installing vitest the same way as before");
        assert_eq!(book.project_notes.len(), 1);
        assert_eq!(book.project_notes[0].count, 2);
    }

    #[test]
    fn a_long_list_of_the_same_errors_is_shown_as_examples_and_a_count() {
        let mut errors: Vec<String> = (1..=50).map(|n| format!("app/a/page.ts:{n} tsc: TS2322: Type 'string' is not assignable to type 'number'.")).collect();
        errors.push("app/a/page.ts:1 tsc: TS2322: Type 'string' is not assignable to type 'number'.".into());
        errors.push("app/a/page.ts:9 ruff: F401: `os` imported but unused".into());
        errors.push("`npm test` failed:".into());
        errors.push("  at render (app/a/page.ts:3:7)".into());
        let shown = digest(&errors.join("\n"));
        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines.iter().filter(|l| l.contains("TS2322")).count(), DIGEST_PER_CODE + 1, "{shown}");
        assert!(shown.contains("ruff: F401") && shown.contains("`npm test` failed:") && shown.contains("at render"));
        assert!(shown.contains("…and 47 more like these, not shown: tsc TS2322 ×47."), "{shown}");
        let short = "app/a.ts:1 tsc: TS2304: Cannot find name 'x'.";
        assert_eq!(digest(short), short, "a short failure is shown whole");
    }

    #[test]
    fn a_short_reply_is_not_stored_as_a_fix() {
        let mut book = Book::default();
        book.record("agy", "worker produced no changes under its owned files");
        book.learn_fix("agy", "worker produced no changes under its owned files", "done");
        assert_eq!(book.models["agy"][0].fix, None);
    }
}
