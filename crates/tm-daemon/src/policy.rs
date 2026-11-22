//! Policy-as-Code (`taskmaster.policy.toml` / `.taskmaster/policy.json`) and universal secret
//! redaction across exports, bundles, and logs.
//!
//! Teams commit `taskmaster.policy.toml` in the repository root to declare protected file globs,
//! denied file globs, fetch allow/deny lists, USD spend caps, cross-family review requirements,
//! and custom redaction literals.

use std::path::{Path, PathBuf};

use anyhow::Context;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Policy {
    /// Globs that require explicit human approval or review before landing.
    pub protected_globs: Vec<String>,
    /// Globs workers may never write (e.g. `.env*`, private keys).
    pub denied_globs: Vec<String>,
    /// Optional host allowlist for agent web fetches.
    pub allowed_fetch_hosts: Vec<String>,
    /// Host denylist blocked unconditionally.
    pub denied_fetch_hosts: Vec<String>,
    /// Maximum USD spend per run (overrides unlimited default when set).
    pub max_run_cost_usd: Option<f64>,
    /// Optional daily team spend cap in USD.
    pub daily_budget_usd: Option<f64>,
    /// Require a different model provider family for review than the builder.
    pub require_cross_family_review: bool,
    /// Additional literal prefixes or secret strings to redact in logs and exports.
    pub redact_patterns: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            protected_globs: vec![],
            denied_globs: vec![
                ".env".into(),
                ".env.*".into(),
                "**/*.pem".into(),
                "**/*.key".into(),
            ],
            allowed_fetch_hosts: vec![],
            denied_fetch_hosts: vec![],
            max_run_cost_usd: None,
            daily_budget_usd: None,
            require_cross_family_review: false,
            redact_patterns: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathVerdict {
    Allowed,
    Protected(String),
    Denied(String),
}

impl Policy {
    /// Where the repo-versioned policy file lives, if present.
    pub fn toml_path(project: &Path) -> PathBuf {
        project.join("taskmaster.policy.toml")
    }

    /// Fallback local JSON policy file under `.taskmaster/`.
    pub fn json_path(project: &Path) -> PathBuf {
        project.join(".taskmaster").join("policy.json")
    }

    /// Load policy from `taskmaster.policy.toml` (preferred), then `.taskmaster/policy.json`,
    /// falling back to safe defaults.
    pub fn load(project: &Path) -> Self {
        if let Ok(raw) = std::fs::read_to_string(Self::toml_path(project))
            && let Ok(parsed) = parse_policy_toml(&raw)
        {
            return parsed;
        }
        if let Ok(raw) = std::fs::read_to_string(Self::json_path(project))
            && let Ok(parsed) = serde_json::from_str::<Policy>(&raw)
        {
            return parsed;
        }
        Self::default()
    }

    /// Which file currently backs this project's policy, if any.
    pub fn source(project: &Path) -> Option<String> {
        if Self::toml_path(project).is_file() {
            return Some("taskmaster.policy.toml".into());
        }
        if Self::json_path(project).is_file() {
            return Some(".taskmaster/policy.json".into());
        }
        None
    }

    /// Save the policy to `taskmaster.policy.toml` if it already exists, otherwise
    /// `.taskmaster/policy.json` (or `taskmaster.policy.toml` when `repo_file` is true).
    pub fn save(&self, project: &Path, repo_file: bool) -> anyhow::Result<String> {
        if repo_file || Self::toml_path(project).is_file() {
            let path = Self::toml_path(project);
            let text = render_policy_toml(self);
            std::fs::write(&path, text)
                .with_context(|| format!("cannot write {}", path.display()))?;
            Ok("taskmaster.policy.toml".into())
        } else {
            let path = Self::json_path(project);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, serde_json::to_string_pretty(self)?)
                .with_context(|| format!("cannot write {}", path.display()))?;
            Ok(".taskmaster/policy.json".into())
        }
    }

    /// Check whether a repo-relative path is allowed, protected, or denied.
    pub fn check_path(&self, rel: &str) -> PathVerdict {
        if let Some(glob) = first_matching_glob(&self.denied_globs, rel) {
            return PathVerdict::Denied(glob);
        }
        if let Some(glob) = first_matching_glob(&self.protected_globs, rel) {
            return PathVerdict::Protected(glob);
        }
        PathVerdict::Allowed
    }
}

fn first_matching_glob(globs: &[String], path: &str) -> Option<String> {
    if globs.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    let mut valid = Vec::new();
    for g in globs {
        if let Ok(compiled) = Glob::new(g) {
            builder.add(compiled);
            valid.push(g.clone());
        }
    }
    let set: GlobSet = builder.build().ok()?;
    let idx = set.matches(path).into_iter().next()?;
    valid.get(idx).cloned()
}

/// Minimal, dependency-free TOML parser for `taskmaster.policy.toml` supporting flat keys,
/// comments (`#`), booleans, numbers, and single-line or multi-line string arrays.
pub fn parse_policy_toml(raw: &str) -> anyhow::Result<Policy> {
    let mut policy = Policy::default();
    let mut lines = raw.lines().peekable();
    while let Some(line) = lines.next() {
        let stripped = strip_toml_comment(line).trim();
        if stripped.is_empty() || stripped.starts_with('[') {
            continue;
        }
        let Some((k, v)) = stripped.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let mut val = v.trim().to_string();
        if val.starts_with('[') && !val.contains(']') {
            for next in lines.by_ref() {
                let part = strip_toml_comment(next).trim();
                val.push(' ');
                val.push_str(part);
                if part.contains(']') {
                    break;
                }
            }
        }
        match key {
            "protected_globs" => policy.protected_globs = parse_string_array(&val),
            "denied_globs" => policy.denied_globs = parse_string_array(&val),
            "allowed_fetch_hosts" => policy.allowed_fetch_hosts = parse_string_array(&val),
            "denied_fetch_hosts" => policy.denied_fetch_hosts = parse_string_array(&val),
            "redact_patterns" => policy.redact_patterns = parse_string_array(&val),
            "max_run_cost_usd" => policy.max_run_cost_usd = val.parse::<f64>().ok().filter(|n| *n > 0.0),
            "daily_budget_usd" => policy.daily_budget_usd = val.parse::<f64>().ok().filter(|n| *n > 0.0),
            "require_cross_family_review" => policy.require_cross_family_review = val == "true",
            _ => {}
        }
    }
    Ok(policy)
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_quote = false;
    let mut prev_escape = false;
    for (idx, ch) in line.char_indices() {
        if ch == '"' && !prev_escape {
            in_quote = !in_quote;
        } else if ch == '#' && !in_quote {
            return &line[..idx];
        }
        prev_escape = ch == '\\' && !prev_escape;
    }
    line
}

fn parse_string_array(val: &str) -> Vec<String> {
    let inner = val.trim().trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut quote_char = '"';
    for ch in inner.chars() {
        if !in_quote {
            if ch == '"' || ch == '\'' {
                in_quote = true;
                quote_char = ch;
                cur.clear();
            }
        } else if ch == quote_char {
            in_quote = false;
            if !cur.trim().is_empty() {
                out.push(cur.clone());
            }
        } else {
            cur.push(ch);
        }
    }
    out
}

pub fn render_policy_toml(policy: &Policy) -> String {
    let fmt_arr = |items: &[String]| -> String {
        let joined = items
            .iter()
            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{joined}]")
    };
    let mut out = String::from("# Taskmaster Policy-as-Code\n");
    out.push_str(&format!("protected_globs = {}\n", fmt_arr(&policy.protected_globs)));
    out.push_str(&format!("denied_globs = {}\n", fmt_arr(&policy.denied_globs)));
    out.push_str(&format!("allowed_fetch_hosts = {}\n", fmt_arr(&policy.allowed_fetch_hosts)));
    out.push_str(&format!("denied_fetch_hosts = {}\n", fmt_arr(&policy.denied_fetch_hosts)));
    if let Some(cap) = policy.max_run_cost_usd {
        out.push_str(&format!("max_run_cost_usd = {cap:.2}\n"));
    }
    if let Some(cap) = policy.daily_budget_usd {
        out.push_str(&format!("daily_budget_usd = {cap:.2}\n"));
    }
    out.push_str(&format!(
        "require_cross_family_review = {}\n",
        policy.require_cross_family_review
    ));
    out.push_str(&format!("redact_patterns = {}\n", fmt_arr(&policy.redact_patterns)));
    out
}

/// Known secret token prefixes scrubbed across exported bundles and logs.
const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-proj-",
    "sk-",
    "xai-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "AIza",
    "AKIA",
];

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

use std::borrow::Cow;

fn has_builtin_secret_trigger(bytes: &[u8]) -> bool {
    // Every built-in pattern needs at least 15 bytes: `sk-` + 12 or `Bearer ` + 8.
    if bytes.len() < 15 {
        return false;
    }
    let len = bytes.len();
    let mut i = 0;
    while i + 3 <= len {
        match bytes[i] {
            b'B' | b'b' if i + 7 <= len && &bytes[i + 1..i + 7] == b"earer " => return true,
            b's' if &bytes[i + 1..i + 3] == b"k-" => return true,
            b'x' if i + 4 <= len && &bytes[i + 1..i + 4] == b"ai-" => return true,
            b'g' if i + 4 <= len
                && (matches!(&bytes[i + 1..i + 4], b"hp_" | b"ho_" | b"hu_" | b"hs_")
                    || (i + 6 <= len && &bytes[i + 1..i + 6] == b"lpat-")
                    || (i + 11 <= len && &bytes[i + 1..i + 11] == b"ithub_pat_")) =>
            {
                return true;
            }
            b'A' if i + 4 <= len && matches!(&bytes[i + 1..i + 4], b"Iza" | b"KIA") => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Fast check before doing any allocations in [`redact_text_cow`].
fn needs_redaction(input: &str, extra_literals: &[String]) -> bool {
    if input.len() < 4 {
        return false;
    }
    for secret in extra_literals {
        let trimmed = secret.trim();
        if trimmed.len() >= 4 && input.contains(trimmed) {
            return true;
        }
    }
    has_builtin_secret_trigger(input.as_bytes())
}

/// Scrub common API key / token formats, `Bearer <token>`, and any extra literal secrets,
/// returning the input untouched with zero allocations when nothing matches.
pub fn redact_text_cow<'a>(input: &'a str, extra_literals: &[String]) -> Cow<'a, str> {
    if !needs_redaction(input, extra_literals) {
        return Cow::Borrowed(input);
    }
    let mut text = input.to_string();
    for secret in extra_literals {
        let trimmed = secret.trim();
        if trimmed.len() >= 4 && text.contains(trimmed) {
            text = text.replace(trimmed, "•••");
        }
    }
    if text.contains("Bearer ") {
        text = redact_after_marker(&text, "Bearer ", 8);
    }
    if text.contains("bearer ") {
        text = redact_after_marker(&text, "bearer ", 8);
    }
    for prefix in SECRET_PREFIXES {
        if text.contains(prefix) {
            text = redact_prefixed_token(&text, prefix, 12);
        }
    }
    if text == input {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(text)
    }
}

/// Scrub common API key / token formats, `Bearer <token>`, and any extra literal secrets.
pub fn redact_text(input: &str, extra_literals: &[String]) -> String {
    redact_text_cow(input, extra_literals).into_owned()
}

/// Byte offset where a trailing partial secret begins, so a streaming writer can hold that
/// small tail until the next delta arrives and catch secrets split across two deltas.
/// Returns `None` on normal text.
pub fn potential_secret_tail_start(input: &str, extra_literals: &[String]) -> Option<usize> {
    if input.is_empty() {
        return None;
    }
    // Look only at the trailing window (up to 128 bytes, aligned to a UTF-8 char boundary).
    let mut win_start = input.len().saturating_sub(128);
    while win_start < input.len() && !input.is_char_boundary(win_start) {
        win_start += 1;
    }
    let tail = &input[win_start..];
    let mut hold: Option<usize> = None;
    let mut consider = |abs_pos: usize| {
        hold = Some(match hold {
            Some(prev) => prev.min(abs_pos),
            None => abs_pos,
        });
    };

    // 1. A known prefix or `Bearer ` already present near the end with only token chars after it.
    let bytes = tail.as_bytes();
    let token_tail_start = {
        let mut idx = bytes.len();
        while idx > 0 && is_token_char(bytes[idx - 1]) {
            idx -= 1;
        }
        idx
    };
    let word = &tail[token_tail_start..];
    if !word.is_empty() {
        // Check `Bearer <partial>` / `bearer <partial>`
        if token_tail_start >= 7 {
            let before = &tail[token_tail_start - 7..token_tail_start];
            if before == "Bearer " || before == "bearer " {
                consider(win_start + token_tail_start - 7);
            }
        }
        // Check a word that starts with (or is a >=2-char prefix of) any known secret prefix.
        for prefix in SECRET_PREFIXES {
            if word.starts_with(prefix) || (word.len() >= 2 && prefix.starts_with(word)) {
                consider(win_start + token_tail_start);
                break;
            }
        }
    }

    // 2. Trailing >=2-char partial match of `Bearer `, `bearer `, or any extra registered secret.
    for marker in ["Bearer ", "bearer "] {
        let max_k = marker.len().min(tail.len());
        for k in (2..=max_k).rev() {
            if tail.ends_with(&marker[..k]) {
                consider(input.len() - k);
                break;
            }
        }
    }
    for secret in extra_literals {
        let trimmed = secret.trim();
        if trimmed.len() < 4 {
            continue;
        }
        let max_k = (trimmed.len() - 1).min(tail.len());
        for k in (2..=max_k).rev() {
            let start = input.len() - k;
            if input.is_char_boundary(start) && trimmed.starts_with(&input[start..]) {
                consider(start);
                break;
            }
        }
    }
    hold
}

fn redact_after_marker(input: &str, marker: &str, min_len: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos + marker.len()]);
        let after = &rest[pos + marker.len()..];
        let tok_len = after.bytes().take_while(|b| is_token_char(*b)).count();
        if tok_len >= min_len {
            out.push_str("•••");
            rest = &after[tok_len..];
        } else {
            out.push_str(&after[..tok_len]);
            rest = &after[tok_len..];
        }
    }
    out.push_str(rest);
    out
}

fn redact_prefixed_token(input: &str, prefix: &str, min_tail: usize) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(prefix) {
        // Ensure we are not mid-word (previous byte is not alphanumeric)
        let boundary_ok = pos == 0 || !rest.as_bytes()[pos - 1].is_ascii_alphanumeric();
        if !boundary_ok {
            out.push_str(&rest[..pos + prefix.len()]);
            rest = &rest[pos + prefix.len()..];
            continue;
        }
        let after = &rest[pos + prefix.len()..];
        let tail_len = after.bytes().take_while(|b| is_token_char(*b)).count();
        if tail_len >= min_tail {
            out.push_str(&rest[..pos]);
            out.push_str("•••");
            rest = &after[tail_len..];
        } else {
            out.push_str(&rest[..pos + prefix.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Recursively redact every string leaf inside a JSON value.
pub fn redact_json(value: &mut Value, extra_literals: &[String]) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(clean) = redact_text_cow(s, extra_literals) {
                *s = clean;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_json(item, extra_literals);
            }
        }
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                redact_json(v, extra_literals);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_toml_roundtrips_and_enforces_path_verdicts() {
        let raw = r#"
# Team policy
protected_globs = ["migrations/**", "**/auth/**"]
denied_globs = [".env", ".env.*", "**/*.pem"]
max_run_cost_usd = 2.50
daily_budget_usd = 25.00
require_cross_family_review = true
redact_patterns = ["SUPER_SECRET_42"]
"#;
        let policy = parse_policy_toml(raw).unwrap();
        assert_eq!(policy.max_run_cost_usd, Some(2.5));
        assert_eq!(policy.daily_budget_usd, Some(25.0));
        assert!(policy.require_cross_family_review);
        assert_eq!(policy.check_path(".env.local"), PathVerdict::Denied(".env.*".into()));
        assert_eq!(
            policy.check_path("src/auth/jwt.ts"),
            PathVerdict::Protected("**/auth/**".into())
        );
        assert_eq!(policy.check_path("src/ui/button.tsx"), PathVerdict::Allowed);

        let rendered = render_policy_toml(&policy);
        let again = parse_policy_toml(&rendered).unwrap();
        assert_eq!(again, policy);
    }

    #[test]
    fn redaction_scrubs_known_tokens_bearer_and_custom_literals() {
        let sample = "Authorization: Bearer abcdef1234567890 key=sk-ant-1234567890abcdef token=ghp_1234567890abcdef custom=MY_TOKEN_99";
        let clean = redact_text(sample, &["MY_TOKEN_99".into()]);
        assert!(!clean.contains("abcdef1234567890"), "{clean}");
        assert!(!clean.contains("sk-ant-"), "{clean}");
        assert!(!clean.contains("ghp_"), "{clean}");
        assert!(!clean.contains("MY_TOKEN_99"), "{clean}");
        assert!(clean.contains("Bearer •••"), "{clean}");
    }

    #[test]
    fn redaction_borrows_clean_text_without_allocating() {
        let clean = "Planning the header layout and checking existing styles in web/src/App.tsx";
        let res = redact_text_cow(clean, &[]);
        assert!(matches!(res, Cow::Borrowed(_)), "clean text should not allocate");
        assert_eq!(res, clean);
        let dirty = "token sk-ant-1234567890abcdef";
        let res = redact_text_cow(dirty, &[]);
        assert!(matches!(res, Cow::Owned(_)));
        assert_eq!(res, "token •••");
    }
}
