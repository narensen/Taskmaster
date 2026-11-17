//! Web search through a SearXNG-style JSON endpoint. SearXNG sends no CORS headers, so the
//! browser asks the daemon and the daemon asks SearXNG.

use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, Debug, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub content: String,
    pub engine: String,
}

fn encode(q: &str) -> String {
    let mut out = String::new();
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The endpoint with `{q}` replaced by the encoded query, or `q=` appended when it has none.
pub fn build_url(endpoint: &str, q: &str) -> String {
    let q = encode(q);
    if endpoint.contains("{q}") {
        endpoint.replace("{q}", &q)
    } else {
        let sep = if endpoint.contains('?') { '&' } else { '?' };
        format!("{endpoint}{sep}q={q}")
    }
}

fn text(item: &Value, keys: &[&str]) -> String {
    keys.iter().find_map(|k| item.get(*k).and_then(Value::as_str)).unwrap_or("").to_string()
}

pub fn parse(value: &Value) -> Vec<Hit> {
    let list = value
        .as_array()
        .or_else(|| value.get("results").and_then(Value::as_array))
        .or_else(|| value.pointer("/web/results").and_then(Value::as_array))
        .or_else(|| value.get("items").and_then(Value::as_array));
    let Some(list) = list else { return Vec::new() };
    list.iter()
        .filter_map(|item| {
            let url = text(item, &["url", "link", "href"]);
            if !url.starts_with("http") {
                return None;
            }
            let title = Some(text(item, &["title", "name"])).filter(|t| !t.is_empty()).unwrap_or_else(|| url.clone());
            let engine = Some(text(item, &["engine"]))
                .filter(|e| !e.is_empty())
                .or_else(|| item.pointer("/engines/0").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            Some(Hit { title, url, content: text(item, &["content", "snippet", "description"]), engine })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_fixture() {
        let hits = parse(&serde_json::from_str(include_str!("fixtures/searxng.json")).unwrap());
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "Asynchronous Programming in Rust");
        assert_eq!(hits[0].engine, "duckduckgo");
        assert_eq!(hits[1].content, "");
        assert_eq!(hits[2].engine, "bing");
        assert!(hits.iter().all(|h| h.url.starts_with("http")));
    }

    #[test]
    fn empty_inputs_give_nothing() {
        assert!(parse(&json!({"results": []})).is_empty());
        assert!(parse(&json!({})).is_empty());
    }

    #[test]
    fn missing_title_falls_back_to_url() {
        let hits = parse(&json!([{"link": "https://a.example", "snippet": "s"}]));
        assert_eq!(hits[0].title, "https://a.example");
        assert_eq!(hits[0].content, "s");
    }

    #[test]
    fn builds_urls() {
        assert_eq!(build_url("http://h/s?format=json&q={q}", "a b&c"), "http://h/s?format=json&q=a%20b%26c");
        assert_eq!(build_url("http://h/s", "x y"), "http://h/s?q=x%20y");
        assert_eq!(build_url("http://h/s?format=json", "x"), "http://h/s?format=json&q=x");
    }
}
