//! Schema versioning and forward migrations for persisted run records (`status.json` and
//! `events.jsonl`). Pure transforms with no I/O.

use serde_json::{Value, json};

/// Current on-disk record schema version.
/// - v0 (unversioned): legacy records written before schema versioning existed.
/// - v1: adds explicit `schema_version`, normalized `phase`, and default `undone: false`.
/// - v2: ensures `checkpoints` / `issue_url` / `plan_review` metadata slots can coexist cleanly.
pub const SCHEMA_VERSION: u32 = 2;

/// Upgrade a `status.json` object forward to [`SCHEMA_VERSION`].
/// Returns `(migrated_value, original_version)`.
pub fn migrate_status(mut value: Value) -> (Value, u32) {
    let original = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let Some(obj) = value.as_object_mut() else {
        return (value, original);
    };
    let mut version = original;
    if version < 1 {
        // v0 -> v1: ensure phase default is explicit; keep optional fields like `undone` untouched.
        if !obj.contains_key("phase") {
            obj.insert("phase".into(), json!("planning"));
        }
        version = 1;
    }
    if version < 2 {
        // v1 -> v2: normalize legacy `error: ""` to null and stamp schema_version = 2.
        if obj.get("error").and_then(Value::as_str).is_some_and(str::is_empty) {
            obj.insert("error".into(), Value::Null);
        }
        version = 2;
    }
    obj.insert("schema_version".into(), json!(version.max(SCHEMA_VERSION)));
    (value, original)
}

/// Parse one line of `events.jsonl`, stripping an optional schema header comment or envelope.
/// Supports:
/// - v0 raw Event JSON lines (`{"plan_ready": ...}`)
/// - v1/v2 versioned envelopes (`{"v": 2, "event": {"plan_ready": ...}}`)
/// - Metadata header lines (`{"_schema": 2}`) which return `None`.
pub fn unwrap_event_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
        return Some(trimmed.to_string());
    };
    if val.get("_schema").is_some() && val.get("event").is_none() {
        return None;
    }
    if let Some(inner) = val.get("event") {
        return serde_json::to_string(inner).ok();
    }
    Some(trimmed.to_string())
}

/// Format the optional schema header line for a new `events.jsonl` file.
pub fn events_header() -> String {
    format!("{{\"_schema\":{SCHEMA_VERSION}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_v0_status_through_v1_and_v2() {
        let legacy = json!({
            "id": "run-1",
            "error": ""
        });
        let (migrated, from) = migrate_status(legacy);
        assert_eq!(from, 0);
        assert_eq!(migrated["schema_version"], SCHEMA_VERSION);
        assert_eq!(migrated["phase"], "planning");
        assert!(migrated.get("undone").is_none());
        assert!(migrated["error"].is_null());
    }

    #[test]
    fn migrates_v1_status_to_v2_preserving_fields() {
        let v1 = json!({
            "schema_version": 1,
            "id": "run-2",
            "phase": "done",
            "undone": true,
            "error": ""
        });
        let (migrated, from) = migrate_status(v1);
        assert_eq!(from, 1);
        assert_eq!(migrated["schema_version"], 2);
        assert_eq!(migrated["phase"], "done");
        assert_eq!(migrated["undone"], true);
        assert!(migrated["error"].is_null());
    }

    #[test]
    fn unwraps_v0_and_v2_event_lines_and_skips_schema_header() {
        assert_eq!(unwrap_event_line(&events_header()), None);
        assert_eq!(unwrap_event_line("   "), None);
        assert_eq!(
            unwrap_event_line(r#""plan_approved""#).as_deref(),
            Some(r#""plan_approved""#)
        );
        assert_eq!(
            unwrap_event_line(r#"{"v":2,"event":"plan_approved"}"#).as_deref(),
            Some(r#""plan_approved""#)
        );
    }
}
