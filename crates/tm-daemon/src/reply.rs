//! Pull the JSON an agent was told to reply with out of its prose.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_core::{ContractFile, DraftEntry, EntryKind, Estimate, Finding, PagePlan, Plan, ProjectKind};

#[derive(Deserialize)]
struct RawPlan {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    contracts: Vec<ContractFile>,
    /// Planners now write `units`; `pages` is still read.
    #[serde(default, alias = "units")]
    pages: Vec<PagePlan>,
    #[serde(default)]
    estimate: Option<Estimate>,
    #[serde(default)]
    dictionary: Vec<RawEntry>,
    #[serde(default)]
    design: Value,
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
/// Missing `dictionary` and `estimate` mean none and zero.
pub fn parse_plan(text: &str) -> anyhow::Result<(Plan, Vec<DraftEntry>)> {
    if let Some(hit) = limit_of(text) {
        return Err(hit.into());
    }
    let value = last_json(text, '{', '}').context("the plan reply had no JSON object")?;
    let raw: RawPlan = serde_json::from_value(value).context("the plan reply was not a plan")?;
    let estimate = raw.estimate.unwrap_or(Estimate { tokens: 0, usd: 0.0 });
    let design = design_text(&raw.design);
    let kind = plan_kind(raw.kind.as_deref(), &raw.pages, &raw.contracts, &design);
    let dictionary = raw.dictionary.into_iter().map(RawEntry::entry).collect();
    Ok((Plan { kind, contracts: raw.contracts, pages: raw.pages, estimate, design }, dictionary))
}

pub fn parse_resolution(text: &str) -> anyhow::Result<Resolution> {
    if let Some(hit) = limit_of(text) {
        return Err(hit.into());
    }
    let value = last_json(text, '{', '}').context("the arbitration reply had no JSON object")?;
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
    } else if rate_failure(&lower) {
        LimitKind::RateLimit
    } else if quota_failure(&lower) {
        LimitKind::Quota
    } else {
        return None;
    };
    let raw = {
        let mut cut = raw.chars().take(2000).collect::<String>();
        if raw.chars().count() > 2000 {
            cut.push('…');
        }
        cut
    };
    Some(ProviderLimit { kind, reset: reset_time(window), raw })
}

fn has_answer_json(text: &str) -> bool {
    if let Some(value) = last_json(text, '{', '}')
        && (value.get("pages").is_some() || value.get("resolution").is_some() || value.get("contracts").is_some() || value.get("flag").is_some())
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

fn rate_failure(text: &str) -> bool {
    const PHRASES: &[&str] = &["rate limit", "rate_limit", "too many requests", "429 too many"];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

fn quota_failure(text: &str) -> bool {
    const PHRASES: &[&str] = &[
        "usage limit",
        "usage_limit",
        "you've hit your usage",
        "you have hit your usage",
        "hit your limit",
        "insufficient_quota",
        "credit balance is too low",
        "out of credits",
        "spend limit",
        "exceeded your current quota",
    ];
    PHRASES.iter().any(|phrase| text.contains(phrase))
}

fn reset_time(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    for marker in ["resets at ", "reset at ", "resets ", "try again at ", "retry after ", "available again at "] {
        let Some(at) = lower.find(marker) else { continue };
        let rest = text[at + marker.len()..].trim();
        let end = rest.find(['.', '\n']).unwrap_or(rest.len());
        let phrase = rest[..end].trim().trim_matches(|c: char| c == ',' || c == ';');
        if phrase.is_empty() || phrase.chars().count() > 40 {
            continue;
        }
        return Some(phrase.to_string());
    }
    None
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
}
