//! Pull the JSON an agent was told to reply with out of its prose.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_core::{ContractFile, DraftEntry, EntryKind, Estimate, Finding, PagePlan, PhaseSketch, Plan, ProjectKind, ResearchNote};

#[derive(Deserialize)]
struct RawPlan {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default, alias = "overview")]
    summary: String,
    #[serde(default)]
    contracts: Vec<ContractFile>,
    /// Planners write `units` or `workers`; `pages` is still read.
    #[serde(default, alias = "units", alias = "workers")]
    pages: Vec<PagePlan>,
    #[serde(default)]
    estimate: Option<Estimate>,
    #[serde(default)]
    dictionary: Vec<RawEntry>,
    #[serde(default)]
    design: Value,
    #[serde(default)]
    phases: Vec<Value>,
    #[serde(default)]
    research: Vec<Value>,
}

/// A phase of the sketch as a planner wrote it: lists or text are both read, and a phase
/// with no goal is dropped rather than failing the plan.
fn phase_of(value: &Value, index: usize) -> Option<PhaseSketch> {
    let text = |key: &str| match &value[key] {
        Value::String(s) => s.trim().to_string(),
        Value::Array(items) => items.iter().filter_map(|i| i.as_str().map(str::trim).map(str::to_string)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    let goal = [text("goal"), text("summary"), text("title")].into_iter().find(|t| !t.is_empty())?;
    let id = [text("id"), text("name")].into_iter().find(|t| !t.is_empty()).unwrap_or_else(|| format!("phase-{}", index + 1));
    let delivers = match &value["delivers"] {
        Value::Array(items) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Value::String(s) => s.split([',', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    Some(PhaseSketch { id, goal, delivers, interfaces: text("interfaces"), test: text("test"), needs: text("needs") })
}

/// A dictionary item as a planner wrote it. An unknown `kind` is read as a hint rather than
/// throwing the whole plan away.
#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    key: String,
    #[serde(default)]
    kind: Value,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    stack: String,
}

impl RawEntry {
    fn entry(self) -> DraftEntry {
        let kind = serde_json::from_value::<EntryKind>(self.kind).unwrap_or(EntryKind::Hint);
        DraftEntry { key: self.key, kind, summary: self.summary, content: self.content, stack: self.stack }
    }
}

/// The kind a plan names, or, when it names none, the one its units imply: routes, a design
/// or web files mean web; anything else is a library.
fn plan_kind(named: Option<&str>, pages: &[PagePlan], contracts: &[ContractFile], design: &str) -> ProjectKind {
    if let Some(kind) = named.and_then(ProjectKind::parse) {
        return kind;
    }
    const WEB: [&str; 7] = [".html", ".css", ".tsx", ".jsx", ".vue", ".svelte", "app/"];
    let routed = pages.iter().any(|p| p.route.trim().starts_with('/'));
    let web_files = contracts.iter().map(|c| c.path.as_str()).chain(pages.iter().map(|p| p.owned_glob.as_str())).any(|t| WEB.iter().any(|w| t.contains(w)));
    if routed || !design.trim().is_empty() || web_files { ProjectKind::Web } else { ProjectKind::Library }
}

/// The planner's design spec as text. Models write it as a string or as an object of
/// sections (`{"palette": "...", "a11y": ["...", "..."]}`); both become `section: text` lines.
fn design_text(value: &Value) -> String {
    fn flat(value: &Value) -> String {
        match value {
            Value::String(s) => s.trim().to_string(),
            Value::Array(items) => items.iter().map(flat).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("; "),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| format!("{k} {}", flat(v)))
                .collect::<Vec<_>>()
                .join("; "),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k, flat(v)))
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("- {k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        other => flat(other),
    }
}

/// The last complete JSON value in `text` that opens with `open` and ends at the last `close`.
/// Agents wrap answers in fences and sentences; the braces in the prose come first and fail
/// to parse, so the first opening bracket that *does* parse through the end is the value.
fn last_json(text: &str, open: char, close: char) -> Option<Value> {
    let end = text.rfind(close)?;
    let slice = &text[..=end];
    slice.char_indices().filter(|(_, c)| *c == open).find_map(|(start, _)| serde_json::from_str(&slice[start..]).ok())
}

/// Extract markdown fenced code block bodies (outermost to innermost, returned last-to-first).
fn extract_fenced_blocks(text: &str) -> Vec<(bool, String)> {
    let mut blocks: Vec<(bool, String)> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang = rest.trim().to_ascii_lowercase();
            let is_json_fence = lang.is_empty() || lang == "json" || lang == "json5" || lang == "jsonc";
            let start = i + 1;
            let mut end = lines.len();
            let mut j = start;
            while j < lines.len() {
                if lines[j].trim_start().starts_with("```") {
                    end = j;
                    break;
                }
                j += 1;
            }
            if end > start {
                blocks.push((is_json_fence, lines[start..end].join("\n")));
            }
            i = if end < lines.len() { end + 1 } else { lines.len() };
        } else {
            i += 1;
        }
    }
    blocks.reverse();
    blocks
}

/// Skip ASCII whitespace and JSON5/JS comments (`// ...` and `/* ... */`).
fn skip_ws_and_comments(mut s: &str) -> &str {
    loop {
        let trimmed = s.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//") {
            match rest.find('\n') {
                Some(nl) => s = &rest[nl + 1..],
                None => return "",
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/*") {
            match rest.find("*/") {
                Some(end) => s = &rest[end + 2..],
                None => return "",
            }
            continue;
        }
        return trimmed;
    }
}

/// True when `slice` begins with `{` followed by plausible JSON/JSON5 object syntax (not prose like `{this}`).
fn looks_like_json_object_start(slice: &str) -> bool {
    let Some(rest) = slice.strip_prefix('{') else {
        return false;
    };
    let rest = skip_ws_and_comments(rest);
    if rest.is_empty() {
        return false;
    }
    let first = rest.chars().next().unwrap();
    if matches!(first, '}' | '"' | '\'' | '\u{201C}' | '\u{201D}') {
        return true;
    }
    if first.is_ascii_alphabetic() || first == '_' || first == '$' {
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-'))
            .unwrap_or(rest.len());
        let after = skip_ws_and_comments(&rest[end..]);
        if after.starts_with(':') {
            return true;
        }
    }
    false
}

/// Find the outermost balanced `{ ... }` span starting at byte 0 (tracking double-quoted strings loosely).
fn balanced_object_end(slice: &str) -> Option<usize> {
    if !slice.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (idx, ch) in slice.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(idx + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract candidate JSON object substrings from a model reply, ordered most-likely first.
fn json_object_candidates(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |s: &str| {
        let trimmed = s.trim();
        if !trimmed.is_empty() && !out.iter().any(|have| have == trimmed) {
            out.push(trimmed.to_string());
        }
    };

    // 1. Markdown fenced blocks (last to first).
    for (is_json_fence, body) in extract_fenced_blocks(text) {
        if let Some(start) = body.find('{') {
            let tail = &body[start..];
            let slice = match tail.rfind('}') {
                Some(end) => &tail[..=end],
                None => tail,
            };
            if is_json_fence || looks_like_json_object_start(slice) {
                push_unique(slice);
            }
        }
    }

    // 2. Unwrapped text: strip an outer fence if the entire reply is wrapped in one.
    let trimmed = text.trim();
    let unwrapped = if let Some(after_open) = trimmed.strip_prefix("```") {
        let body = after_open.split_once('\n').map_or(after_open, |(_, rest)| rest);
        body.trim_end().strip_suffix("```").unwrap_or(body).trim()
    } else {
        trimmed
    };

    if let Some(first_brace) = unwrapped.find('{') {
        let from_first = &unwrapped[first_brace..];
        if let Some(last_close) = from_first.rfind('}') {
            let outer = &from_first[..=last_close];
            if looks_like_json_object_start(outer) || outer.contains("\":") {
                push_unique(outer);
                if let Some(bal_end) = balanced_object_end(outer)
                    && bal_end < outer.len()
                {
                    push_unique(&outer[..bal_end]);
                }
            }
            // If prose before the JSON contained a brace like `{foo}`, locate the first `{`
            // that actually starts a JSON object.
            if !looks_like_json_object_start(outer) {
                for (offset, ch) in outer.char_indices() {
                    if ch == '{' && offset > 0 && looks_like_json_object_start(&outer[offset..]) {
                        let sub = &outer[offset..];
                        push_unique(sub);
                        if let Some(bal_end) = balanced_object_end(sub)
                            && bal_end < sub.len()
                        {
                            push_unique(&sub[..bal_end]);
                        }
                        break;
                    }
                }
            }
        } else if looks_like_json_object_start(from_first) {
            // Truncated JSON object with no closing `}` yet.
            push_unique(from_first);
        }
    }

    out
}

/// Extract the best raw JSON object candidate from a reply (for `ask_repair` when local repair fails).
pub fn extract_json_object_candidate(text: &str) -> Option<String> {
    json_object_candidates(text).into_iter().next()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Object,
    Array,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    Value,
    KeyOrEnd,
    Colon,
    CommaOrEnd,
}

/// Check if `rest` (already trimmed of leading whitespace/comments) begins a plausible JSON object key followed by `:`.
fn looks_like_object_key(rest: &str, allow_json5: bool) -> bool {
    let Some(first) = rest.chars().next() else {
        return false;
    };
    if matches!(first, '"' | '\u{201C}' | '\u{201D}') {
        let mut i = first.len_utf8();
        let bytes = rest.as_bytes();
        let mut escaped = false;
        let mut chars_seen = 0usize;
        while i < bytes.len() && chars_seen < 160 {
            let ch = rest[i..].chars().next().unwrap();
            i += ch.len_utf8();
            chars_seen += 1;
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '\n' || ch == '\r' {
                return false;
            }
            if ch == '"' || (first != '"' && ch == '\u{201D}') {
                let after = skip_ws_and_comments(&rest[i..]);
                return after.starts_with(':');
            }
        }
        return false;
    }
    if allow_json5 && matches!(first, '\'' | '\u{2018}' | '\u{2019}') {
        let mut i = first.len_utf8();
        let mut escaped = false;
        let mut chars_seen = 0usize;
        while i < rest.len() && chars_seen < 160 {
            let ch = rest[i..].chars().next().unwrap();
            i += ch.len_utf8();
            chars_seen += 1;
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '\n' || ch == '\r' {
                return false;
            }
            if ch == '\'' || (first != '\'' && ch == '\u{2019}') {
                let after = skip_ws_and_comments(&rest[i..]);
                return after.starts_with(':');
            }
        }
        return false;
    }
    if allow_json5 && (first.is_ascii_alphabetic() || first == '_' || first == '$') {
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-'))
            .unwrap_or(rest.len());
        let after = skip_ws_and_comments(&rest[end..]);
        return after.starts_with(':');
    }
    false
}

/// Check if what follows a closing `}` or `]` is plausible given the parent container on `stack`.
fn valid_after_container_close(after: &str, stack: &[Container], allow_json5: bool) -> bool {
    if stack.len() <= 1 {
        return after.is_empty();
    }
    let parent = stack[stack.len() - 2];
    match parent {
        Container::Object => {
            if after.starts_with('}') {
                return true;
            }
            if let Some(after_comma) = after.strip_prefix(',') {
                let next = skip_ws_and_comments(after_comma);
                return next.starts_with('}') || looks_like_object_key(next, allow_json5);
            }
            false
        }
        Container::Array => {
            if after.starts_with(']') {
                return true;
            }
            if let Some(after_comma) = after.strip_prefix(',') {
                let next = skip_ws_and_comments(after_comma);
                return next.starts_with(']') || looks_like_array_value_start(next, allow_json5);
            }
            false
        }
    }
}

/// Check if `rest` (trimmed of ws/comments) begins a plausible JSON array element value.
fn looks_like_array_value_start(rest: &str, allow_json5: bool) -> bool {
    let Some(first) = rest.chars().next() else {
        return false;
    };
    if matches!(first, '{' | '[' | '-' | '0'..='9') {
        return true;
    }
    for kw in ["true", "false", "null"] {
        if let Some(tail) = rest.strip_prefix(kw) {
            let next_ok = tail
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || matches!(c, ',' | ']' | '}' | '/'));
            if next_ok {
                return true;
            }
        }
    }
    if matches!(first, '"' | '\u{201C}' | '\u{201D}')
        || (allow_json5 && matches!(first, '\'' | '\u{2018}' | '\u{2019}'))
    {
        // Confirm the next quoted token is actually a JSON element (followed by `,` or `]`),
        // not a pair of unescaped quotes embedded inside a single string like `"say "hi", "bye" now"`.
        let mut i = first.len_utf8();
        let mut escaped = false;
        while i < rest.len() {
            let ch = rest[i..].chars().next().unwrap();
            i += ch.len_utf8();
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '\n' || ch == '\r' {
                return false;
            }
            let closes = match first {
                '"' => ch == '"',
                '\u{201C}' | '\u{201D}' => ch == '"' || ch == '\u{201D}',
                '\'' => ch == '\'',
                '\u{2018}' | '\u{2019}' => ch == '\'' || ch == '\u{2019}',
                _ => false,
            };
            if closes {
                let after = skip_ws_and_comments(&rest[i..]);
                return after.starts_with(',') || after.starts_with(']');
            }
        }
        return false;
    }
    false
}

/// Decide whether an unescaped quote at `pos_after_quote` validly terminates the current JSON string,
/// or is an unescaped interior quote that must be escaped as `\"`.
fn is_valid_string_close(
    input: &str,
    pos_after_quote: usize,
    expecting_key: bool,
    stack: &[Container],
    allow_json5: bool,
) -> bool {
    let rest = skip_ws_and_comments(&input[pos_after_quote..]);
    if expecting_key {
        return rest.starts_with(':');
    }
    match stack.last() {
        Some(Container::Object) => {
            if let Some(after_brace) = rest.strip_prefix('}') {
                return valid_after_container_close(skip_ws_and_comments(after_brace), stack, allow_json5);
            }
            if let Some(after_comma) = rest.strip_prefix(',') {
                let next = skip_ws_and_comments(after_comma);
                if let Some(after_brace) = next.strip_prefix('}') {
                    return valid_after_container_close(skip_ws_and_comments(after_brace), stack, allow_json5);
                }
                return looks_like_object_key(next, allow_json5);
            }
            false
        }
        Some(Container::Array) => {
            if let Some(after_bracket) = rest.strip_prefix(']') {
                return valid_after_container_close(skip_ws_and_comments(after_bracket), stack, allow_json5);
            }
            if let Some(after_comma) = rest.strip_prefix(',') {
                let next = skip_ws_and_comments(after_comma);
                if let Some(after_bracket) = next.strip_prefix(']') {
                    return valid_after_container_close(skip_ws_and_comments(after_bracket), stack, allow_json5);
                }
                return looks_like_array_value_start(next, allow_json5);
            }
            false
        }
        None => rest.is_empty(),
    }
}

fn push_escaped_control(out: &mut String, ch: char) {
    match ch {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{0008}' => out.push_str("\\b"),
        '\u{000C}' => out.push_str("\\f"),
        c => {
            use std::fmt::Write as _;
            let _ = write!(out, "\\u{:04x}", c as u32);
        }
    }
}

/// Deterministic, single-pass local JSON repair:
/// - strips `//` and `/* ... */` comments outside strings
/// - converts smart/curly quotes used as JSON delimiters
/// - removes trailing commas before `}` and `]`
/// - escapes raw newlines/tabs/control chars inside strings
/// - escapes invalid backslash escapes inside strings (e.g. `\d`, `\'`)
/// - escapes unescaped interior double quotes inside string values using structural lookahead
/// - when `allow_json5` is true, also converts single-quoted strings and unquoted object keys.
fn repair_json(input: &str, allow_json5: bool) -> String {
    let mut out = String::with_capacity(input.len() + 64);
    let mut stack: Vec<Container> = Vec::new();
    let mut state = ParseState::Value;
    let mut i = 0usize;

    while i < input.len() {
        let ch = input[i..].chars().next().unwrap();

        if ch.is_whitespace() {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        // Line comment outside string.
        if input[i..].starts_with("//") {
            match input[i + 2..].find('\n') {
                Some(nl) => {
                    out.push('\n');
                    i += 2 + nl + 1;
                }
                None => break,
            }
            continue;
        }
        // Block comment outside string.
        if input[i..].starts_with("/*") {
            match input[i + 2..].find("*/") {
                Some(end) => i += 2 + end + 2,
                None => break,
            }
            continue;
        }

        match ch {
            '{' => {
                out.push('{');
                stack.push(Container::Object);
                state = ParseState::KeyOrEnd;
                i += 1;
            }
            '}' => {
                out.push('}');
                if matches!(stack.last(), Some(Container::Object)) {
                    stack.pop();
                }
                state = ParseState::CommaOrEnd;
                i += 1;
            }
            '[' => {
                out.push('[');
                stack.push(Container::Array);
                state = ParseState::Value;
                i += 1;
            }
            ']' => {
                out.push(']');
                if matches!(stack.last(), Some(Container::Array)) {
                    stack.pop();
                }
                state = ParseState::CommaOrEnd;
                i += 1;
            }
            ':' => {
                out.push(':');
                state = ParseState::Value;
                i += 1;
            }
            ',' => {
                let next = skip_ws_and_comments(&input[i + 1..]);
                if next.starts_with('}') || next.starts_with(']') {
                    // Drop trailing comma.
                    i += 1;
                } else {
                    out.push(',');
                    state = match stack.last() {
                        Some(Container::Object) => ParseState::KeyOrEnd,
                        _ => ParseState::Value,
                    };
                    i += 1;
                }
            }
            '"' | '\u{201C}' | '\u{201D}' => {
                let opened_smart = ch != '"';
                let expecting_key = state == ParseState::KeyOrEnd;
                out.push('"');
                i += ch.len_utf8();
                while i < input.len() {
                    let sc = input[i..].chars().next().unwrap();
                    if sc == '\\' {
                        let next_pos = i + 1;
                        if next_pos >= input.len() {
                            out.push_str("\\\\");
                            i = next_pos;
                            break;
                        }
                        let esc = input[next_pos..].chars().next().unwrap();
                        if matches!(esc, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                            out.push('\\');
                            out.push(esc);
                            i = next_pos + esc.len_utf8();
                        } else if esc == 'u' {
                            let hex_start = next_pos + 1;
                            let hex_ok = input.len() >= hex_start + 4
                                && input[hex_start..hex_start + 4].bytes().all(|b| b.is_ascii_hexdigit());
                            if hex_ok {
                                out.push_str(&input[i..hex_start + 4]);
                                i = hex_start + 4;
                            } else {
                                out.push_str("\\\\u");
                                i = hex_start;
                            }
                        } else {
                            // Invalid JSON escape like `\d`, `\s`, `\'`: escape the backslash so the next loop iteration handles `esc`.
                            out.push_str("\\\\");
                            i = next_pos;
                        }
                        continue;
                    }
                    if (sc as u32) < 0x20 {
                        push_escaped_control(&mut out, sc);
                        i += sc.len_utf8();
                        continue;
                    }
                    let is_quote_candidate = sc == '"' || (opened_smart && sc == '\u{201D}');
                    if is_quote_candidate {
                        let after = i + sc.len_utf8();
                        if is_valid_string_close(input, after, expecting_key, &stack, allow_json5) {
                            out.push('"');
                            i = after;
                            break;
                        } else {
                            out.push_str("\\\"");
                            i = after;
                            continue;
                        }
                    }
                    out.push(sc);
                    i += sc.len_utf8();
                }
                state = if expecting_key { ParseState::Colon } else { ParseState::CommaOrEnd };
            }
            '\'' | '\u{2018}' | '\u{2019}' if allow_json5 => {
                let opened_smart = ch != '\'';
                let expecting_key = state == ParseState::KeyOrEnd;
                out.push('"');
                i += ch.len_utf8();
                while i < input.len() {
                    let sc = input[i..].chars().next().unwrap();
                    if sc == '\\' {
                        let next_pos = i + 1;
                        if next_pos >= input.len() {
                            out.push_str("\\\\");
                            i = next_pos;
                            break;
                        }
                        let esc = input[next_pos..].chars().next().unwrap();
                        if esc == '\'' {
                            out.push('\'');
                            i = next_pos + 1;
                        } else if matches!(esc, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                            out.push('\\');
                            out.push(esc);
                            i = next_pos + esc.len_utf8();
                        } else if esc == 'u' {
                            let hex_start = next_pos + 1;
                            let hex_ok = input.len() >= hex_start + 4
                                && input[hex_start..hex_start + 4].bytes().all(|b| b.is_ascii_hexdigit());
                            if hex_ok {
                                out.push_str(&input[i..hex_start + 4]);
                                i = hex_start + 4;
                            } else {
                                out.push_str("\\\\u");
                                i = hex_start;
                            }
                        } else {
                            out.push_str("\\\\");
                            i = next_pos;
                        }
                        continue;
                    }
                    if (sc as u32) < 0x20 {
                        push_escaped_control(&mut out, sc);
                        i += sc.len_utf8();
                        continue;
                    }
                    if sc == '"' {
                        out.push_str("\\\"");
                        i += 1;
                        continue;
                    }
                    if sc == '\'' || (opened_smart && sc == '\u{2019}') {
                        let after = i + sc.len_utf8();
                        if is_valid_string_close(input, after, expecting_key, &stack, allow_json5) {
                            out.push('"');
                            i = after;
                            break;
                        } else {
                            out.push('\'');
                            i = after;
                            continue;
                        }
                    }
                    out.push(sc);
                    i += sc.len_utf8();
                }
                state = if expecting_key { ParseState::Colon } else { ParseState::CommaOrEnd };
            }
            _ if allow_json5 && state == ParseState::KeyOrEnd && (ch.is_ascii_alphabetic() || ch == '_' || ch == '$') => {
                let end = i
                    + input[i..]
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-'))
                        .unwrap_or(input.len() - i);
                let ident = &input[i..end];
                let after = skip_ws_and_comments(&input[end..]);
                if after.starts_with(':') {
                    out.push('"');
                    out.push_str(ident);
                    out.push('"');
                    i = end;
                    state = ParseState::Colon;
                } else {
                    out.push_str(ident);
                    i = end;
                }
            }
            _ => {
                out.push(ch);
                i += ch.len_utf8();
                if matches!(state, ParseState::Value) && !ch.is_whitespace() {
                    // Consume the rest of a bare primitive token (number/bool/null).
                    while i < input.len() {
                        let nc = input[i..].chars().next().unwrap();
                        if nc.is_whitespace() || matches!(nc, ',' | '}' | ']' | ':') {
                            break;
                        }
                        out.push(nc);
                        i += nc.len_utf8();
                    }
                    state = ParseState::CommaOrEnd;
                }
            }
        }
    }

    out
}

/// Format an accurate JSON syntax error with line, column, serde message, and short context snippet.
fn format_json_error(candidate: &str, err: &serde_json::Error) -> String {
    let line = err.line();
    let col = err.column();
    let raw_msg = err.to_string();
    let suffix = format!(" at line {line} column {col}");
    let msg = raw_msg.strip_suffix(&suffix).unwrap_or(&raw_msg);

    let snippet = candidate
        .lines()
        .nth(line.saturating_sub(1))
        .map(|line_text| {
            let chars: Vec<char> = line_text.chars().collect();
            let idx = col.saturating_sub(1).min(chars.len());
            let start = idx.saturating_sub(35);
            let end = (idx + 35).min(chars.len());
            let mut s = String::new();
            if start > 0 {
                s.push('…');
            }
            for &c in &chars[start..end] {
                if c == '\t' {
                    s.push(' ');
                } else {
                    s.push(c);
                }
            }
            if end < chars.len() {
                s.push('…');
            }
            s
        })
        .unwrap_or_default();

    if snippet.is_empty() {
        format!("invalid JSON at line {line} column {col}: {msg}")
    } else {
        format!("invalid JSON at line {line} column {col}: {msg} (near `{snippet}`)")
    }
}

/// Parse a JSON object from `text`, trying strict JSON first and then deterministic local repair.
/// Returns `missing_msg` only when no JSON object exists; otherwise reports exact line/column + snippet.
pub(crate) fn parse_json_object_lenient(text: &str, missing_msg: &str) -> anyhow::Result<Value> {
    if let Some(val @ Value::Object(_)) = last_json(text, '{', '}') {
        return Ok(val);
    }
    for (_, body) in extract_fenced_blocks(text) {
        if let Ok(val @ Value::Object(_)) = serde_json::from_str::<Value>(body.trim()) {
            return Ok(val);
        }
    }
    let candidates = json_object_candidates(text);
    if candidates.is_empty() {
        bail!("{missing_msg}");
    }
    for cand in &candidates {
        if let Ok(val @ Value::Object(_)) = serde_json::from_str::<Value>(cand) {
            return Ok(val);
        }
        let repaired = repair_json(cand, false);
        if let Ok(val @ Value::Object(_)) = serde_json::from_str::<Value>(&repaired) {
            return Ok(val);
        }
        let repaired_json5 = repair_json(cand, true);
        if let Ok(val @ Value::Object(_)) = serde_json::from_str::<Value>(&repaired_json5) {
            return Ok(val);
        }
    }
    let best = &candidates[0];
    match serde_json::from_str::<Value>(best) {
        Ok(_) => bail!("{missing_msg}"),
        Err(err) => bail!("{}", format_json_error(best, &err)),
    }
}

/// A worker objecting to a Dictionary entry. `None` when the reply is ordinary work.
pub fn parse_flag(text: &str) -> Option<(String, String)> {
    let value = last_json(text, '{', '}')?;
    let flag = value.get("flag")?;
    let key = flag["key"].as_str()?.trim().to_string();
    let issue = flag["issue"].as_str()?.trim().to_string();
    if key.is_empty() || issue.is_empty() { None } else { Some((key, issue)) }
}

/// One reviewer's findings. `key` is the connection that reviewer was given, used when an
/// item does not name one. No JSON array is a failed review, not a clean one.
pub fn parse_review(text: &str, key: Option<String>) -> anyhow::Result<Vec<Finding>> {
    let array = last_json(text, '[', ']').context("the reply had no JSON array")?;
    let items = array.as_array().context("the reply's JSON was not an array")?;
    let mut out = Vec::new();
    for item in items {
        let Some(page) = item["page"].as_str().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let Some(file) = item["file"].as_str() else { continue };
        let line = item["line"].as_u64().or_else(|| item["line"].as_str().and_then(|s| s.trim().parse().ok()));
        let Some(line) = line else { continue };
        let Ok(line) = u32::try_from(line) else { continue };
        let Some(failure) = item["failure"].as_str().map(str::trim) else { continue };
        let item_key = item["key"].as_str().filter(|s| !s.is_empty()).map(str::to_string).or_else(|| key.clone());
        out.push(Finding {
            page: page.to_string(),
            key: item_key,
            file: file.trim().trim_start_matches("./").to_string(),
            line,
            failure: failure.to_string(),
        });
    }
    Ok(out)
}

/// A verifier's `real` field. Silence and unreadable replies count as not confirmed.
pub fn parse_verdict(text: &str) -> bool {
    last_json(text, '{', '}').and_then(|v| v["real"].as_bool()).unwrap_or(false)
}

/// Parse a batched verifier JSON array `[{"index": 0, "real": true}, ...]`.
/// Missing or malformed entries default to `false` (unconfirmed).
pub fn parse_verdicts_batch(text: &str, count: usize) -> Vec<bool> {
    let mut confirmed = vec![false; count];
    let Some(array) = last_json(text, '[', ']') else { return confirmed };
    let Some(items) = array.as_array() else { return confirmed };
    for (pos, item) in items.iter().enumerate() {
        let idx = item["index"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(pos);
        if idx < count {
            confirmed[idx] = item["real"].as_bool().unwrap_or(false);
        }
    }
    confirmed
}

/// What the Taskmaster decided about a worker's objection.
pub enum Resolution {
    Conform,
    Amend(Amendment),
}

pub struct Amendment {
    pub summary: String,
    pub content: String,
    pub kind: EntryKind,
    pub stack: String,
}

/// The planner's contracts, pages, estimate, and new Dictionary entries.
fn research_of(value: &Value) -> Option<ResearchNote> {
    if let Some(text) = value.as_str() {
        let decision = text.trim().to_string();
        return (!decision.is_empty()).then_some(ResearchNote { decision, source: String::new(), version: String::new() });
    }
    let decision = ["decision", "summary", "note", "finding"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    if decision.is_empty() {
        return None;
    }
    let source = ["source", "url", "link"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    let version = ["version", "applies_to", "package"]
        .iter()
        .find_map(|k| value.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    Some(ResearchNote { decision, source, version })
}

/// Missing `dictionary` and `estimate` mean none and zero.
pub fn parse_plan(text: &str) -> anyhow::Result<(Plan, Vec<DraftEntry>)> {
    if let Some(hit) = limit_of(text) {
        return Err(hit.into());
    }
    let value = parse_json_object_lenient(text, "the plan reply had no JSON object")?;
    let raw: RawPlan = serde_json::from_value(value).context("the plan reply was not a plan")?;
    let estimate = raw.estimate.unwrap_or(Estimate { tokens: 0, usd: 0.0, ..Default::default() });
    let design = design_text(&raw.design);
    let kind = plan_kind(raw.kind.as_deref(), &raw.pages, &raw.contracts, &design);
    let dictionary = raw.dictionary.into_iter().map(RawEntry::entry).collect();
    let phases: Vec<PhaseSketch> = raw.phases.iter().enumerate().filter_map(|(i, v)| phase_of(v, i)).collect();
    // One phase is not a sketch: the plan is the whole job.
    let phases = if phases.len() >= 2 { phases } else { Vec::new() };
    let research: Vec<ResearchNote> = raw.research.iter().filter_map(research_of).collect();
    Ok((
        Plan {
            kind,
            summary: raw.summary.trim().to_string(),
            contracts: raw.contracts,
            pages: raw.pages,
            estimate,
            design,
            phases,
            research,
        },
        dictionary,
    ))
}

pub fn parse_resolution(text: &str) -> anyhow::Result<Resolution> {
    if let Some(hit) = limit_of(text) {
        return Err(hit.into());
    }
    let value = parse_json_object_lenient(text, "the arbitration reply had no JSON object")?;
    match value["resolution"].as_str().unwrap_or("").to_ascii_lowercase().as_str() {
        "conform" => Ok(Resolution::Conform),
        "amended" | "amend" => {
            let content = value["content"].as_str().context("an amendment needs content")?.to_string();
            if content.trim().is_empty() {
                bail!("an amendment's content is empty");
            }
            let summary = value["summary"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("amended entry")
                .to_string();
            let kind = match value["kind"].as_str() {
                Some(kind) => serde_json::from_value(Value::String(kind.into()))
                    .with_context(|| format!("unknown entry kind `{kind}`"))?,
                None => EntryKind::Hint,
            };
            let stack = value["stack"].as_str().unwrap_or("").to_string();
            Ok(Resolution::Amend(Amendment { summary, content, kind, stack }))
        }
        other => bail!("unknown resolution `{other}`"),
    }
}

/// Why a provider stopped the turn. The raw reply stays on the value and out of the sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Quota,
    RateLimit,
    Auth,
    /// The account has no paid balance left (HTTP 402, "usage balance exhausted").
    Balance,
}

/// Exit code returned by `tm-daemon` when a provider hits a temporary quota or HTTP 429 rate
/// limit (`EX_TEMPFAIL` = 75), so outer automation scripts stop immediately instead of firing
/// doomed runs until the reset window passes.
pub const QUOTA_LIMIT_EXIT: u8 = 75;

/// Exit code returned by `tm-daemon` when a provider requires authentication or credit balance
/// before continuing.
pub const AUTH_LIMIT_EXIT: u8 = 3;

impl LimitKind {
    /// Distinct CLI exit code for this provider limit kind.
    pub fn exit_code(self) -> u8 {
        match self {
            LimitKind::Quota | LimitKind::RateLimit => QUOTA_LIMIT_EXIT,
            LimitKind::Auth | LimitKind::Balance => AUTH_LIMIT_EXIT,
        }
    }
}

/// A provider limit found in a reply, before any role or provider name is known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderLimit {
    pub kind: LimitKind,
    pub reset: Option<String>,
    pub raw: String,
}

impl std::fmt::Display for ProviderLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            LimitKind::Quota => write!(f, "the provider hit its usage limit"),
            LimitKind::RateLimit => write!(f, "the provider is rate limited"),
            LimitKind::Auth => write!(f, "the provider needs a sign-in"),
            LimitKind::Balance => write!(f, "the provider has no usage balance left"),
        }
    }
}

impl std::error::Error for ProviderLimit {}

/// The same limit, named for the role that was asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportedLimit {
    pub provider: String,
    pub role: String,
    pub kind: LimitKind,
    pub reset: Option<String>,
    pub raw: String,
}

impl ReportedLimit {
    pub fn new(provider: &str, role: &str, hit: ProviderLimit) -> Self {
        Self { provider: provider.to_string(), role: role.to_string(), kind: hit.kind, reset: hit.reset, raw: hit.raw }
    }

    pub fn exit_code(&self) -> u8 {
        self.kind.exit_code()
    }

    pub fn sentence(&self) -> String {
        let who = provider_label(&self.provider);
        let what = match self.kind {
            LimitKind::Quota => format!("{who} hit its usage limit ({})", self.role),
            LimitKind::RateLimit => format!("{who} is rate limited ({})", self.role),
            LimitKind::Auth => format!("{who} needs a sign-in ({})", self.role),
            LimitKind::Balance => format!("{who} has no usage balance left ({}); add credit or switch the provider, then retry", self.role),
        };
        match &self.reset {
            Some(reset) => format!("{what}. Resets {reset}."),
            None => format!("{what}."),
        }
    }
}

impl std::fmt::Display for ReportedLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.sentence())
    }
}

impl std::error::Error for ReportedLimit {}

pub fn provider_label(id: &str) -> String {
    match id {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        "grok" | "grok-build" => "Grok".into(),
        "cursor" => "Cursor".into(),
        "opencode" => "OpenCode".into(),
        "agy" | "antigravity" => "Antigravity".into(),
        "kiro" => "Kiro".into(),
        "" => "The provider".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "The provider".into(),
            }
        }
    }
}

/// Extract a typed [`ProviderLimit`] from a harness error (preferring a structured
/// [`tm_harness::QuotaError`] emitted by the harness, then falling back to text matching).
pub fn limit_from_error(err: &anyhow::Error) -> Option<ProviderLimit> {
    if let Some(q) = err.downcast_ref::<tm_harness::QuotaError>() {
        return Some(ProviderLimit {
            kind: if q.is_rate_limit { LimitKind::RateLimit } else { LimitKind::Quota },
            reset: q.reset.clone(),
            raw: q.raw.clone(),
        });
    }
    limit_of(&format!("{err:#}"))
}

/// A quota, rate-limit, or auth failure in a harness reply. A plan, a review, or an
/// arbitration that parses is left alone, even if its prose mentions a limit.
pub fn limit_of(text: &str) -> Option<ProviderLimit> {
    let raw = text.trim();
    if raw.is_empty() || has_answer_json(raw) {
        return None;
    }
    let head = raw.lines().find(|line| !line.trim().is_empty()).unwrap_or(raw);
    let window = if raw.chars().count() <= 1200 { raw } else { head };
    let lower = window.to_ascii_lowercase();
    let kind = if balance_failure(&lower) {
        LimitKind::Balance
    } else if auth_failure(&lower) {
        LimitKind::Auth
    } else {
        let hit = tm_harness::detect_quota_or_rate_limit(raw)?;
        return Some(ProviderLimit {
            kind: if hit.is_rate_limit { LimitKind::RateLimit } else { LimitKind::Quota },
            reset: hit.reset,
            raw: hit.raw,
        });
    };
    let raw = {
        let mut cut = raw.chars().take(2000).collect::<String>();
        if raw.chars().count() > 2000 {
            cut.push('…');
        }
        cut
    };
    Some(ProviderLimit { kind, reset: tm_harness::extract_reset_time(window), raw })
}

fn has_answer_json(text: &str) -> bool {
    if let Ok(value) = parse_json_object_lenient(text, "")
        && (value.get("pages").is_some()
            || value.get("units").is_some()
            || value.get("resolution").is_some()
            || value.get("contracts").is_some()
            || value.get("flag").is_some())
    {
        return true;
    }
    last_json(text, '[', ']').and_then(|value| value.as_array().cloned()).is_some_and(|items| {
        items.iter().any(|item| item.get("failure").is_some() || item.get("page").is_some())
    })
}

fn auth_failure(text: &str) -> bool {
    const PHRASES: &[&str] = &[
        "please run /login",
        "please log in",
        "not logged in",
        "not signed in",
        "sign in to",
        "session expired",
        "auth expired",
        "authentication expired",
        "invalid api key",
        "invalid_api_key",
        "authentication_error",
        "401 unauthorized",
    ];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

/// A provider that wants payment before it answers again: an HTTP 402, or its words for an
/// empty prepaid balance.
fn balance_failure(text: &str) -> bool {
    const PHRASES: &[&str] = &[
        "402 payment required",
        "status 402",
        "status: 402",
        "payment_required",
        "usage balance exhausted",
        "balance exhausted",
        "insufficient balance",
        "insufficient_balance",
        "no remaining balance",
    ];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_flag_a_finding_list_and_a_verdict_out_of_prose() {
        let flag = "I object.\n```json\n{\"flag\":{\"key\":\"auth\",\"issue\":\"missing role\"}}\n```";
        assert_eq!(parse_flag(flag), Some(("auth".into(), "missing role".into())));
        assert_eq!(parse_flag("done"), None);

        let review = "Looks wrong [here].\n[{\"page\":\"a\",\"file\":\"./app/a/page.ts\",\"line\":\"2\",\
                      \"failure\":\"empty id renders a blank header for guests\"}]";
        let found = parse_review(review, Some("auth".into())).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, "app/a/page.ts");
        assert_eq!(found[0].key.as_deref(), Some("auth"));
        assert_eq!(found[0].line, 2);
        assert!(parse_review("[]", None).unwrap().is_empty());
        assert!(parse_review("I could not read it.", None).is_err());

        assert!(parse_verdict("Checked. {\"real\": true}"));
        assert!(!parse_verdict("```json\n{\"real\": false, \"why\": \"already handled\"}\n```"));
        assert!(!parse_verdict("no idea"));
    }

    #[test]
    fn reads_conform_and_amend() {
        assert!(matches!(parse_resolution("{\"resolution\":\"conform\"}").unwrap(), Resolution::Conform));
        let Resolution::Amend(a) = parse_resolution(
            "{\"resolution\":\"amended\",\"kind\":\"hint\",\"summary\":\"roles\",\"content\":\"export const R = 1;\",\"stack\":\"next\"}",
        )
        .unwrap() else {
            panic!("expected an amendment");
        };
        assert_eq!(a.content, "export const R = 1;");
        assert_eq!(a.kind, EntryKind::Hint);
        assert!(parse_resolution("{\"resolution\":\"amended\",\"content\":\"\"}").is_err());
    }

    #[test]
    fn reads_a_plan_out_of_prose() {
        let text = "Locked.\n{\"pages\":[{\"id\":\"a\",\"route\":\"/a\",\"owned_glob\":\"app/a/**\",\"cites\":[\"greet\"],\"brief\":\"Build a.\"}],\
                    \"dictionary\":[{\"key\":\"greet\",\"kind\":\"hint\",\"summary\":\"greeting\",\"content\":\"export const g = 1;\"}]}";
        let (plan, dictionary) = parse_plan(text).unwrap();
        assert_eq!(plan.pages[0].id, "a");
        assert_eq!(plan.estimate.tokens, 0);
        assert!(plan.contracts.is_empty());
        assert_eq!(dictionary[0].key, "greet");
        assert_eq!(dictionary[0].stack, "");
        assert!(parse_plan("no json here").is_err());
        // An unknown entry kind is read as a hint, not a reason to lose the plan.
        let (_, entries) = parse_plan("{\"dictionary\":[{\"key\":\"greet\",\"kind\":\"nope\",\"summary\":\"x\",\"content\":\"y\"}]}").unwrap();
        assert_eq!(entries[0].kind, EntryKind::Hint);
    }

    #[test]
    fn reads_a_plan_of_modules_with_no_routes() {
        let text = "{\"kind\":\"server\",\"units\":[{\"id\":\"protocol\",\"owned_glob\":\"toyredis/protocol.py\",\"brief\":\"RESP parser.\"},\
                    {\"id\":\"tests\",\"owned_glob\":\"tests/**\",\"brief\":\"Tests.\",\"parts\":[]}]}";
        let (plan, _) = parse_plan(text).unwrap();
        assert_eq!(plan.kind, ProjectKind::Server);
        assert_eq!(plan.pages.len(), 2);
        assert!(plan.pages.iter().all(|p| p.route.is_empty() && p.cites.is_empty()));
        assert!(!plan.has_ui());
        // No kind named: routes mean web, plain modules mean a library.
        assert_eq!(parse_plan("{\"pages\":[{\"id\":\"a\",\"route\":\"/a\",\"owned_glob\":\"app/a/**\",\"brief\":\"x\"}]}").unwrap().0.kind, ProjectKind::Web);
        assert_eq!(parse_plan("{\"units\":[{\"id\":\"db\",\"owned_glob\":\"tinydb/**\",\"brief\":\"x\"}]}").unwrap().0.kind, ProjectKind::Library);
        assert_eq!(parse_plan("{\"kind\":\"api\",\"units\":[]}").unwrap().0.kind, ProjectKind::Server);
    }

    #[test]
    fn reads_the_sketch_of_phases_loosely() {
        let text = r#"{"kind":"server","phases":[
            {"id":"protocol","goal":"RESP encode and decode","delivers":["toyredis/protocol.py"],"interfaces":["def encode(v) -> bytes"],"test":"python -m pytest tests/test_protocol.py"},
            {"name":"server","summary":"The TCP server","delivers":"toyredis/server.py, tests/test_server.py","needs":"results"},
            {"id":"nothing"}],
            "units":[{"id":"protocol","owned_glob":"toyredis/protocol.py","brief":"x"}]}"#;
        let (plan, _) = parse_plan(text).unwrap();
        assert_eq!(plan.phases.len(), 2, "a phase with no goal is dropped");
        assert_eq!(plan.phases[0].interfaces, "def encode(v) -> bytes");
        assert_eq!(plan.phases[1].id, "server");
        assert_eq!(plan.phases[1].delivers, vec!["toyredis/server.py", "tests/test_server.py"]);
        assert!(plan.phases[0].overlaps() && !plan.phases[1].overlaps());
        let (single, _) = parse_plan(r#"{"phases":[{"id":"all","goal":"everything"}],"units":[]}"#).unwrap();
        assert!(single.phases.is_empty(), "one phase is the whole job");
    }

    #[test]
    fn reads_a_design_spec_as_text_or_sections() {
        let (plan, _) = parse_plan("{\"pages\":[],\"design\":\"palette #111\"}").unwrap();
        assert_eq!(plan.design, "palette #111");
        let (plan, _) = parse_plan("{\"pages\":[],\"design\":{\"palette\":\"bg #0b0b0c\",\"a11y\":[\"label inputs\",\"focus-visible ring\"]}}").unwrap();
        assert_eq!(plan.design, "- a11y: label inputs; focus-visible ring\n- palette: bg #0b0b0c");
        let (plan, _) = parse_plan("{\"pages\":[]}").unwrap();
        assert!(plan.design.is_empty());
    }

    #[test]
    fn a_provider_limit_is_not_read_as_a_missing_plan() {
        // Recorded from Codex 0.169 when the account was over its usage limit.
        let err = parse_plan("You've hit your usage limit.").unwrap_err();
        let hit = err.downcast_ref::<ProviderLimit>().expect("a typed limit");
        assert_eq!(hit.kind, LimitKind::Quota);
        assert!(!err.to_string().contains("JSON"));
        assert_eq!(hit.raw, "You've hit your usage limit.");

        let hit = limit_of("Claude is at its usage limit. Resets 6:30 PM.").unwrap();
        assert_eq!((hit.kind, hit.reset.as_deref()), (LimitKind::Quota, Some("6:30 PM")));
        let named = ReportedLimit::new("claude", "Master", hit);
        assert_eq!(named.sentence(), "Claude hit its usage limit (Master). Resets 6:30 PM.");

        let hit = limit_of("Rate limit exceeded. Try again at 4:00 PM.").unwrap();
        assert_eq!((hit.kind, hit.reset.as_deref()), (LimitKind::RateLimit, Some("4:00 PM")));
        assert_eq!(ReportedLimit::new("codex", "Worker", hit).sentence(), "Codex is rate limited (Worker). Resets 4:00 PM.");

        let hit = limit_of("Please run /login. Your session expired.").unwrap();
        assert_eq!(hit.kind, LimitKind::Auth);
        assert_eq!(ReportedLimit::new("claude", "Reviewer", hit).sentence(), "Claude needs a sign-in (Reviewer).");

        // Recorded from Grok Build when its prepaid balance ran out, as the ACP error arrives.
        let grok = r#"`session/prompt` failed: {"code":-32603,"message":"API error (status 402 Payment Required): Grok Build usage balance exhausted","data":null}"#;
        let hit = limit_of(grok).unwrap();
        assert_eq!(hit.kind, LimitKind::Balance);
        let named = ReportedLimit::new("grok", "Worker", hit);
        assert!(named.sentence().contains("no usage balance left (Worker)"), "{}", named.sentence());
        assert_eq!(limit_of("HTTP 402 Payment Required").unwrap().kind, LimitKind::Balance);

        assert!(limit_of("{\"pages\":[]}").is_none());
        assert!(parse_plan("{\"pages\":[]}").is_ok());
        match parse_resolution("You've hit your usage limit.") {
            Err(err) => assert!(err.downcast_ref::<ProviderLimit>().is_some()),
            Ok(_) => panic!("a usage limit is not an arbitration"),
        }
        assert!(matches!(parse_resolution("{\"resolution\":\"conform\"}").unwrap(), Resolution::Conform));
    }

    #[test]
    fn fix_n9_recognizes_antigravity_quota_429_reset_and_exit_code_75() {
        let agy_text = "\n⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h1m23s.\n";
        let hit = limit_of(agy_text).expect("Antigravity individual quota text");
        assert_eq!(hit.kind, LimitKind::Quota);
        assert_eq!(hit.reset.as_deref(), Some("in 1h1m23s"));
        let reported = ReportedLimit::new("antigravity", "Planner", hit);
        assert_eq!(reported.exit_code(), 75);
        assert_eq!(reported.sentence(), "Antigravity hit its usage limit (Planner). Resets in 1h1m23s.");

        let agy_json = r#"AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 1h1m7s.","status":"RESOURCE_EXHAUSTED","error_code":429}"#;
        let hit2 = limit_of(agy_json).expect("AGY_ERROR 429 JSON");
        assert_eq!(hit2.kind, LimitKind::Quota);
        assert_eq!(hit2.reset.as_deref(), Some("in 1h1m7s"));
        assert_eq!(hit2.kind.exit_code(), 75);

        let bare_429 = limit_of("RESOURCE_EXHAUSTED (code 429). Try again in 5m.").expect("bare 429");
        assert_eq!(bare_429.kind, LimitKind::RateLimit);
        assert_eq!(bare_429.reset.as_deref(), Some("in 5m"));
        assert_eq!(bare_429.kind.exit_code(), 75);

        assert_eq!(LimitKind::Auth.exit_code(), 3);
        assert_eq!(LimitKind::Balance.exit_code(), 3);
    }

    #[test]
    fn parses_batched_verdicts_and_batched_review_keys() {
        let verdicts = parse_verdicts_batch(
            "Checked all three:\n[{\"index\":0,\"real\":true},{\"index\":1,\"real\":false,\"why\":\"handled\"},{\"index\":2,\"real\":true}]",
            3,
        );
        assert_eq!(verdicts, vec![true, false, true]);
        // Missing or unreadable reply defaults every finding to unconfirmed.
        assert_eq!(parse_verdicts_batch("not json", 2), vec![false, false]);

        let findings = parse_review(
            r#"[{"key":"auth","page":"dashboard","file":"app/dashboard/page.ts","line":12,"failure":"missing role check"},
                {"key":"nav","page":"settings","file":"app/settings/page.ts","line":4,"failure":"dead link"}]"#,
            None,
        )
        .unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].key.as_deref(), Some("auth"));
        assert_eq!(findings[1].key.as_deref(), Some("nav"));
    }

    #[test]
    fn repairs_real_n6_rejected_plan_replies_locally() {
        let tmh = include_str!("fixtures/n6_rejected_plan_tmh.txt");
        let (plan_tmh, dict_tmh) = parse_plan(tmh).expect("n6 tmh rejected plan must repair locally");
        assert_eq!(plan_tmh.kind, ProjectKind::Cli);
        assert_eq!(plan_tmh.pages.len(), 6);
        assert!(dict_tmh.len() >= 3);
        let cli_unit = plan_tmh.pages.iter().find(|p| p.id == "cli_integration").expect("cli_integration unit");
        assert!(cli_unit.context.contains("`None` -> `\"\"`"), "unescaped empty quotes must survive repair:\n{}", cli_unit.context);
        let cli_entry = dict_tmh.iter().find(|d| d.content.contains("empty string `\"\"`"));
        assert!(cli_entry.is_some(), "second unescaped \"\" in dictionary entry must survive repair");

        let tmhcode = include_str!("fixtures/n6_rejected_plan_tmhcode.txt");
        let (plan_code, dict_code) = parse_plan(tmhcode).expect("n6 tmhcode rejected plan must repair locally");
        assert_eq!(plan_code.kind, ProjectKind::Cli);
        assert_eq!(plan_code.pages.len(), 4);
        assert!(!dict_code.is_empty());
        let cli_docs = plan_code.pages.iter().find(|p| p.id == "cli_and_docs").expect("cli_and_docs unit");
        assert!(cli_docs.context.contains("return `\"\"`"), "unescaped \"\" in tmhcode context must survive repair:\n{}", cli_docs.context);
    }

    #[test]
    fn repairs_trailing_commas_control_chars_smart_quotes_and_json5() {
        // Code fence + trailing prose with braces + trailing commas + unescaped quotes + raw newline/tab + smart quotes
        let messy = "Here is the plan:\n```json\n{\n  // header comment\n  \u{201C}kind\u{201D}: \u{201C}cli\u{201D},\n  \"summary\": \"Line 1\n\tLine 2 with \"quoted\" text and \"\" empty\",\n  \"units\": [\n    {\n      \"id\": \"core\",\n      \"owned_glob\": \"src/**\",\n      \"brief\": \"Format None as \"\", other values as str(v)\",\n    },\n  ],\n}\n```\nNote: ignore {x} in this note.\n";
        let (plan, _) = parse_plan(messy).expect("messy JSON must repair locally");
        assert_eq!(plan.kind, ProjectKind::Cli);
        assert!(plan.summary.contains("Line 1\n\tLine 2"), "raw newline/tab preserved: {:?}", plan.summary);
        assert!(plan.summary.contains("\"quoted\""), "{:?}", plan.summary);
        assert_eq!(plan.pages.len(), 1);
        assert_eq!(plan.pages[0].brief, "Format None as \"\", other values as str(v)");

        // JSON5-ish: unquoted keys, single-quoted strings, block comment, trailing comma
        let json5 = "/* plan */\n{ kind: 'library', units: [{ id: 'a', owned_glob: 'a.py', brief: 'Build \"a\"', },], }";
        let (plan5, _) = parse_plan(json5).expect("json5-ish plan must repair locally");
        assert_eq!(plan5.kind, ProjectKind::Library);
        assert_eq!(plan5.pages[0].id, "a");
        assert_eq!(plan5.pages[0].brief, "Build \"a\"");
    }

    #[test]
    fn reports_accurate_invalid_json_line_and_column_vs_no_json_object() {
        let broken = "{\n  \"kind\": \"cli\",\n  \"units\": [\n    {\"id\": \"a\", \"owned_glob\": \"a.py\", \"brief\": [}\n  ]\n}";
        let err = parse_plan(broken).unwrap_err().to_string();
        assert!(err.starts_with("invalid JSON at line "), "expected accurate line/col error, got: {err}");
        assert!(err.contains("column "), "{err}");
        assert!(err.contains("(near `"), "expected short context snippet, got: {err}");
        assert!(!err.contains("no JSON object"), "must not falsely claim no JSON object: {err}");
        assert!(extract_json_object_candidate(broken).is_some());

        // Truncated JSON with no closing brace is also invalid JSON, not "no JSON object".
        let truncated = "{\"kind\": \"cli\", \"units\": [{\"id\": \"a\"";
        let err_trunc = parse_plan(truncated).unwrap_err().to_string();
        assert!(err_trunc.starts_with("invalid JSON at line "), "got: {err_trunc}");

        // Truly no JSON object (plain prose, or prose with non-JSON braces) reports "no JSON object".
        for prose in ["no json here", "I cannot plan {this} right now."] {
            let err_none = parse_plan(prose).unwrap_err().to_string();
            assert!(err_none.contains("no JSON object"), "for {prose:?} got: {err_none}");
            assert!(extract_json_object_candidate(prose).is_none(), "prose must not yield candidate: {prose:?}");
        }
    }
}
