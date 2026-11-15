//! Pull the JSON an agent was told to reply with out of its prose.

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;
use tm_core::{ContractFile, DraftEntry, EntryKind, Estimate, Finding, PagePlan, Plan};

#[derive(Deserialize)]
struct RawPlan {
    #[serde(default)]
    contracts: Vec<ContractFile>,
    #[serde(default)]
    pages: Vec<PagePlan>,
    #[serde(default)]
    estimate: Option<Estimate>,
    #[serde(default)]
    dictionary: Vec<DraftEntry>,
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
    let value = last_json(text, '{', '}').context("the plan reply had no JSON object")?;
    let raw: RawPlan = serde_json::from_value(value).context("the plan reply was not a plan")?;
    let estimate = raw.estimate.unwrap_or(Estimate { tokens: 0, usd: 0.0 });
    Ok((Plan { contracts: raw.contracts, pages: raw.pages, estimate }, raw.dictionary))
}

pub fn parse_resolution(text: &str) -> anyhow::Result<Resolution> {
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
        assert!(parse_plan("{\"dictionary\":[{\"key\":\"greet\",\"kind\":\"nope\",\"summary\":\"x\",\"content\":\"y\"}]}").is_err());
    }
}
