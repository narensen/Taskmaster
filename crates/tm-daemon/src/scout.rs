//! Threshold-gated, read-only brownfield LLM scope-scout extractor (tb18 #7).
//!
//! For larger brownfield repositories (default `>= 25` source files), runs one cheap,
//! low-effort, read-only, offline call after [`tm_flow::build`] and before the master
//! planner to pinpoint relevant `.flow` symbol IDs, frozen interfaces, edit targets,
//! proposed new files, and test conventions.
//!
//! Design invariants:
//! - Skipped automatically for small/medium repos (`source_files < min_files`) and when
//!   disabled in settings (`scout.enabled = false`).
//! - Writes ONLY an ephemeral per-run artifact at `.taskmaster/runs/<id>/scope.json`
//!   (NEVER modifies `.flow/files/*.json` and NEVER locks editable files as `Plan.contracts`).
//! - Hydrates referenced node IDs deterministically from `.flow` via [`tm_flow::FlowQuery`]
//!   so the planner receives ground-truth signatures/spans/source rather than LLM paraphrase.
//! - Any error, timeout, or invalid JSON falls back silently to deterministic `.flow` packing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_flow::FlowQuery;

/// Minimum tracked source files required before invoking the LLM scope scout.
/// Repos below this threshold use zero-cost deterministic `.flow` + Atlas packing alone.
pub const DEFAULT_MIN_FILES: usize = 25;
/// Hard tool-call budget for the scout commit watcher.
pub const SCOUT_MAX_TOOLS: usize = 4;
/// Hard wall-clock silence/deliberation budget before stopping the scout.
pub const SCOUT_COMMIT_SECS: u64 = 45;
/// Hard overall timeout for the scout call.
pub const SCOUT_TIMEOUT_SECS: u64 = 90;
/// Character budget for the hydrated scope section injected into the planner brief.
pub const SCOUT_HYDRATE_BUDGET: usize = 4_000;

const MAX_ITEMS_PER_LIST: usize = 20;

/// One existing symbol or file the scout identified for modification.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifyTarget {
    /// `.flow` node ID (e.g. `py:pkg/store.py#Store.put`) or relative file path.
    pub id: String,
    /// Concise one-line intent note describing what changes here.
    #[serde(default)]
    pub note: String,
}

/// Ephemeral brownfield scope extracted by the scout and stored at
/// `.taskmaster/runs/<id>/scope.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrownfieldScope {
    /// Existing interfaces/types whose signatures or contracts must remain compatible
    /// (advisory only — never locked as `Plan.contracts`).
    pub frozen_interfaces: Vec<String>,
    /// Existing `.flow` node IDs or files that need modification, with intent notes.
    pub modify_targets: Vec<ModifyTarget>,
    /// Proposed new files to create.
    pub new_files: Vec<String>,
    /// Concise summary of how existing tests are organized and executed.
    pub test_conventions: String,
}

impl BrownfieldScope {
    pub fn is_empty(&self) -> bool {
        self.frozen_interfaces.is_empty()
            && self.modify_targets.is_empty()
            && self.new_files.is_empty()
            && self.test_conventions.trim().is_empty()
    }

    /// Extract `.flow` IDs / paths relevant to a worker's `owned_files` so worker
    /// `preinjected_context` prioritizes symbols the scout flagged for this unit.
    pub fn worker_hint_for(&self, owned_files: &[String]) -> String {
        if self.modify_targets.is_empty() && self.frozen_interfaces.is_empty() {
            return String::new();
        }
        let owned_norm: Vec<String> = owned_files
            .iter()
            .map(|p| p.trim_start_matches("./").replace('\\', "/"))
            .collect();
        let mut lines = Vec::new();
        for target in &self.modify_targets {
            let matches_owned = owned_norm.is_empty()
                || owned_norm.iter().any(|p| target.id.contains(p.as_str()));
            if matches_owned {
                if target.note.is_empty() {
                    lines.push(target.id.clone());
                } else {
                    lines.push(format!("{}: {}", target.id, target.note));
                }
            }
        }
        if !lines.is_empty() {
            for iface in self.frozen_interfaces.iter().take(6) {
                lines.push(iface.clone());
            }
        }
        lines.join("\n")
    }
}

/// Count code source files in `tree` using the shared workspace walker.
pub fn source_file_count(tree: &Path) -> usize {
    crate::lsp::workspace_source_files(tree).len()
}

/// True when the brownfield LLM scope scout should run for `tree`.
pub fn should_run(tree: &Path, enabled: bool, min_files: usize) -> bool {
    enabled && source_file_count(tree) >= min_files.max(1)
}

/// Ephemeral per-run artifact path: `.taskmaster/runs/<run_id>/scope.json`.
pub fn scope_path(project: &Path, run_id: &str) -> PathBuf {
    project.join(".taskmaster").join("runs").join(run_id).join("scope.json")
}

/// Save `scope` atomically to `.taskmaster/runs/<run_id>/scope.json`.
pub fn save_scope(project: &Path, run_id: &str, scope: &BrownfieldScope) -> anyhow::Result<PathBuf> {
    let path = scope_path(project, run_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(scope)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Load a previously saved ephemeral `scope.json` for `run_id` (used when resuming).
pub fn load_scope(project: &Path, run_id: &str) -> Option<BrownfieldScope> {
    let raw = std::fs::read_to_string(scope_path(project, run_id)).ok()?;
    let scope: BrownfieldScope = serde_json::from_str(&raw).ok()?;
    (!scope.is_empty()).then_some(scope)
}

/// Detect an OpenAPI / Swagger spec at common repository locations and return a concise
/// endpoint summary (empty string for CLIs/libraries without one).
pub fn detect_api_spec_hint(tree: &Path, max_chars: usize) -> String {
    if max_chars < 64 {
        return String::new();
    }
    const CANDIDATES: &[&str] = &[
        "openapi.json",
        "openapi.yaml",
        "openapi.yml",
        "swagger.json",
        "swagger.yaml",
        "docs/openapi.json",
        "docs/openapi.yaml",
        "api/openapi.json",
        "api/openapi.yaml",
    ];
    for rel in CANDIDATES {
        let full = tree.join(rel);
        let Ok(raw) = std::fs::read_to_string(&full) else { continue };
        if rel.ends_with(".json")
            && let Ok(val) = serde_json::from_str::<Value>(&raw)
            && let Some(paths) = val.get("paths").and_then(Value::as_object)
        {
            let mut lines = vec![format!("API spec (`{rel}`):")];
            for (route, methods) in paths.iter().take(24) {
                let verbs: Vec<&str> = methods
                    .as_object()
                    .map(|m| {
                        m.keys()
                            .map(String::as_str)
                            .filter(|k| matches!(k.to_ascii_lowercase().as_str(), "get" | "post" | "put" | "patch" | "delete"))
                            .collect()
                    })
                    .unwrap_or_default();
                if verbs.is_empty() {
                    lines.push(format!("- {route}"));
                } else {
                    lines.push(format!("- {} {}", verbs.join("/").to_ascii_uppercase(), route));
                }
            }
            return clip_lines(&lines.join("\n"), max_chars);
        }
        // Lightweight YAML route scan (lines under `paths:` starting with `/`).
        let mut in_paths = false;
        let mut routes = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed == "paths:" {
                in_paths = true;
                continue;
            }
            if in_paths {
                if !line.starts_with([' ', '\t']) && !trimmed.is_empty() && !trimmed.starts_with('#') {
                    break;
                }
                if trimmed.starts_with('/') && trimmed.ends_with(':') {
                    routes.push(format!("- {}", trimmed.trim_end_matches(':')));
                    if routes.len() >= 24 {
                        break;
                    }
                }
            }
        }
        if !routes.is_empty() {
            let header = format!("API spec (`{rel}`):\n{}", routes.join("\n"));
            return clip_lines(&header, max_chars);
        }
    }
    String::new()
}

/// Build the compact read-only prompt sent to the cheap `-low` brownfield scope scout.
pub fn scout_brief(tree: &Path, prompt: &str, code_mode: bool, harness_id: &str) -> String {
    let atlas = crate::atlas::build_or_load(tree, false).render_for_planner(1_800);
    let flow_summary = tm_flow::repo_summary(tree, prompt, 3_600).unwrap_or_default();
    let api_hint = detect_api_spec_hint(tree, 800);
    let tool_note = if code_mode {
        let rc = tm_harness::run_code_tool_name(harness_id, "dictionary");
        format!(
            "You have read-only `{rc}` (`flow(\"find\"|\"node\"|\"neighbors\"|\"source\", ...)`, \
             `outline(path)`, `definition(sym)`, `references(sym)`, `readMany(globs)`, `grep(text, glob?)`, `ls(glob?)`). \
             Use at most {SCOUT_MAX_TOOLS} tool call(s) (1 batched script is best) to confirm exact `.flow` node IDs and callers, then stop immediately."
        )
    } else {
        let fq = tm_harness::mcp_tool_name(harness_id, "dictionary", "flow_query");
        format!(
            "You have read-only `{fq}` (`find`, `node`, `neighbors`, `source`, `batch`). \
             Use at most {SCOUT_MAX_TOOLS} tool call(s) to confirm exact `.flow` node IDs and callers, then stop immediately."
        )
    };

    let mut out = format!(
        "You are a fast, read-only brownfield scope scout. Identify ONLY the exact existing symbols, interfaces, \
         and files relevant to the request so the master planner can draft a clean plan without re-scanning the repo.\n\
         Do NOT edit files, do NOT write implementation code, and do NOT duplicate source bodies.\n\n\
         Request:\n{prompt}\n\n"
    );
    if !atlas.is_empty() {
        out.push_str(&atlas);
        out.push('\n');
    }
    if !flow_summary.is_empty() {
        out.push_str(&flow_summary);
        out.push('\n');
    }
    if !api_hint.is_empty() {
        out.push_str(&api_hint);
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "{tool_note}\n\n\
         Reply with ONE compact JSON object (no prose outside JSON):\n\
         {{\n\
           \"frozen_interfaces\": [\"<exact .flow node id or relative path whose public signature must stay compatible>\"],\n\
           \"modify_targets\": [{{\"id\": \"<exact .flow node id or relative path>\", \"note\": \"<1-line change intent>\"}}],\n\
           \"new_files\": [\"<relative path of new file to add, if any>\"],\n\
           \"test_conventions\": \"<1-line summary of existing test layout, fixtures, and runner>\"\n\
         }}\n"
    ));
    out
}

/// Parse the scout's reply into a normalized, bounded [`BrownfieldScope`], reusing
/// [`crate::reply::parse_json_object_lenient`].
pub fn parse_scope(text: &str) -> anyhow::Result<BrownfieldScope> {
    let val = crate::reply::parse_json_object_lenient(text, "scope scout did not return a JSON object")?;
    let mut scope = BrownfieldScope::default();

    let mut seen_frozen = BTreeSet::new();
    if let Some(arr) = val.get("frozen_interfaces").and_then(Value::as_array) {
        for item in arr {
            let s = item
                .as_str()
                .or_else(|| item.get("id").and_then(Value::as_str))
                .map(str::trim)
                .unwrap_or("");
            if !s.is_empty() && seen_frozen.insert(s.to_string()) {
                scope.frozen_interfaces.push(clip_inline(s, 180));
                if scope.frozen_interfaces.len() >= MAX_ITEMS_PER_LIST {
                    break;
                }
            }
        }
    }

    let mut seen_targets = BTreeSet::new();
    if let Some(arr) = val.get("modify_targets").and_then(Value::as_array) {
        for item in arr {
            let (id, note) = match item {
                Value::String(s) => (s.trim(), ""),
                Value::Object(map) => (
                    map.get("id")
                        .or_else(|| map.get("path"))
                        .or_else(|| map.get("symbol"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or(""),
                    map.get("note")
                        .or_else(|| map.get("reason"))
                        .or_else(|| map.get("intent"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or(""),
                ),
                _ => ("", ""),
            };
            if !id.is_empty() && seen_targets.insert(id.to_string()) {
                scope.modify_targets.push(ModifyTarget {
                    id: clip_inline(id, 180),
                    note: clip_inline(note, 200),
                });
                if scope.modify_targets.len() >= MAX_ITEMS_PER_LIST {
                    break;
                }
            }
        }
    }

    let mut seen_new = BTreeSet::new();
    if let Some(arr) = val.get("new_files").and_then(Value::as_array) {
        for item in arr {
            let s = item.as_str().map(str::trim).unwrap_or("");
            if !s.is_empty()
                && !s.starts_with('/')
                && !s.contains("..")
                && seen_new.insert(s.to_string())
            {
                scope.new_files.push(clip_inline(s, 180));
                if scope.new_files.len() >= MAX_ITEMS_PER_LIST {
                    break;
                }
            }
        }
    }

    if let Some(tc) = val.get("test_conventions").and_then(Value::as_str) {
        scope.test_conventions = clip_inline(tc.trim(), 400);
    }

    if scope.is_empty() {
        bail!("scope scout returned an empty scope");
    }
    Ok(scope)
}

/// Deterministically hydrate `scope` from the parser-generated `.flow` index into a
/// budget-capped planner prompt section. Unknown or hallucinated node IDs that do not exist
/// in `.flow` or on disk are silently dropped so the parser `.flow` graph remains the sole
/// source of truth.
pub fn hydrate_for_planner(tree: &Path, scope: &BrownfieldScope, budget_chars: usize) -> String {
    if scope.is_empty() || budget_chars < 160 {
        return String::new();
    }
    let query = FlowQuery::open(tree).ok();
    let resolve_id = |raw: &str| -> Option<tm_flow::NodeDetail> {
        let q = query.as_ref()?;
        if let Ok(Some(detail)) = q.node(raw) {
            return Some(detail);
        }
        // Fallback: if scout gave a symbol name or qualified label, look it up in `.flow`.
        if let Ok(found) = q.find(raw, 2)
            && let Some(first) = found.nodes.first()
            && let Ok(Some(detail)) = q.node(&first.id)
        {
            return Some(detail);
        }
        None
    };

    let mut lines: Vec<String> = Vec::new();
    lines.push("## Brownfield scope brief (scout, hydrated from .flow)".into());
    lines.push(
        "Advisory scope only — existing editable files below belong in unit `owned_glob`s, NEVER in locked `contracts`."
            .into(),
    );

    let mut verified_any = false;
    if !scope.modify_targets.is_empty() {
        let mut target_lines = Vec::new();
        for target in &scope.modify_targets {
            if let Some(detail) = resolve_id(&target.id) {
                verified_any = true;
                let n = &detail.node;
                let sig = if n.sig.is_empty() {
                    String::new()
                } else {
                    format!(" :: {}", clip_inline(&n.sig, 90))
                };
                let note = if target.note.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", target.note)
                };
                let callers: Vec<String> = detail
                    .incoming
                    .iter()
                    .filter(|e| e.kind == tm_flow::EdgeKind::Calls)
                    .take(4)
                    .map(|e| if e.peer_name.is_empty() { e.peer_id.clone() } else { e.peer_name.clone() })
                    .collect();
                let caller_str = if callers.is_empty() {
                    String::new()
                } else {
                    format!(" [callers: {}]", callers.join(", "))
                };
                target_lines.push(format!(
                    "- {} ({}:{}-{}){sig}{caller_str}{note}",
                    n.id, n.path, n.span[0], n.span[1]
                ));
            } else {
                let rel = target.id.trim_start_matches("./");
                if !rel.contains("..") && !rel.starts_with('/') && tree.join(rel).is_file() {
                    verified_any = true;
                    let note = if target.note.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", target.note)
                    };
                    target_lines.push(format!("- {rel}{note}"));
                }
            }
        }
        if !target_lines.is_empty() {
            lines.push("Modify targets (verified in .flow/disk):".into());
            lines.extend(target_lines);
        }
    }

    if !scope.frozen_interfaces.is_empty() {
        let mut iface_lines = Vec::new();
        for raw in &scope.frozen_interfaces {
            if let Some(detail) = resolve_id(raw) {
                verified_any = true;
                let n = &detail.node;
                let sig = if n.sig.is_empty() {
                    String::new()
                } else {
                    format!(" :: {}", clip_inline(&n.sig, 90))
                };
                iface_lines.push(format!("- {} ({}:{}-{}){sig}", n.id, n.path, n.span[0], n.span[1]));
            } else {
                let rel = raw.trim_start_matches("./");
                if !rel.contains("..") && !rel.starts_with('/') && tree.join(rel).is_file() {
                    verified_any = true;
                    iface_lines.push(format!("- {rel}"));
                }
            }
        }
        if !iface_lines.is_empty() {
            lines.push("Existing interfaces to keep compatible:".into());
            lines.extend(iface_lines);
        }
    }

    if !scope.new_files.is_empty() {
        verified_any = true;
        lines.push(format!("Suggested new files: {}", scope.new_files.join(", ")));
    }
    if !scope.test_conventions.trim().is_empty() {
        verified_any = true;
        lines.push(format!("Test conventions: {}", scope.test_conventions.trim()));
    }

    if !verified_any {
        return String::new();
    }
    let joined = format!("{}\n", lines.join("\n"));
    clip_lines(&joined, budget_chars)
}

fn clip_inline(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        flat
    } else {
        let cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn clip_lines(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    for line in text.lines() {
        if out.len() + line.len() + 1 > max_chars {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tb18_scout_parses_hydrates_from_flow_and_skips_small_repos() {
        let dir = std::env::temp_dir().join(format!("tm-scout-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(
            dir.join("pkg/store.py"),
            "class RecordStore:\n    def put_item(self, key: str, value: bytes) -> None:\n        pass\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("pkg/api.py"),
            "from pkg.store import RecordStore\n\ndef save_entry(s: RecordStore, k: str, v: bytes) -> None:\n    s.put_item(k, v)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("openapi.json"),
            r#"{"openapi":"3.1.0","paths":{"/items":{"get":{},"post":{}},"/health":{"get":{}}}}"#,
        )
        .unwrap();
        tm_flow::build(&dir).unwrap();

        // 1. Threshold gating: 2-file repo is skipped under default threshold (25),
        //    and runs when threshold <= 2 and enabled.
        assert_eq!(source_file_count(&dir), 2);
        assert!(!should_run(&dir, true, DEFAULT_MIN_FILES));
        assert!(!should_run(&dir, false, 1));
        assert!(should_run(&dir, true, 2));

        // 2. OpenAPI detection produces compact route outline.
        let api_hint = detect_api_spec_hint(&dir, 400);
        assert!(api_hint.contains("openapi.json") && api_hint.contains("/items"), "{api_hint}");

        // 3. Lenient JSON parsing handles prose + fenced JSON + string shorthand,
        //    and drops unsafe paths (`..` / absolute).
        let raw = r#"Here is the scope:
```json
{
  "frozen_interfaces": ["py:pkg/api.py#save_entry", "py:pkg/nonexistent.py#Fake"],
  "modify_targets": [
    {"id": "py:pkg/store.py#RecordStore.put_item", "note": "add optional ttl_secs parameter"},
    "py:pkg/ghost.py#Hallucinated"
  ],
  "new_files": ["pkg/ttl.py", "../escape.py", "/etc/passwd"],
  "test_conventions": "pytest under tests/test_*.py"
}
```"#;
        let parsed = parse_scope(raw).unwrap();
        assert_eq!(parsed.new_files, vec!["pkg/ttl.py"]);
        assert_eq!(parsed.modify_targets.len(), 2);

        // 4. Deterministic hydration from `.flow` keeps verified nodes with real signatures/callers
        //    and silently drops hallucinated IDs!
        let hydrated = hydrate_for_planner(&dir, &parsed, SCOUT_HYDRATE_BUDGET);
        assert!(hydrated.contains("## Brownfield scope brief (scout, hydrated from .flow)"), "{hydrated}");
        assert!(hydrated.contains("NEVER in locked `contracts`"), "{hydrated}");
        assert!(hydrated.contains("py:pkg/store.py#RecordStore.put_item"), "{hydrated}");
        assert!(hydrated.contains("add optional ttl_secs parameter"), "{hydrated}");
        assert!(hydrated.contains("py:pkg/api.py#save_entry"), "{hydrated}");
        assert!(!hydrated.contains("Hallucinated"), "hallucinated node IDs must be dropped:\n{hydrated}");
        assert!(!hydrated.contains("Fake"), "hallucinated interfaces must be dropped:\n{hydrated}");
        assert!(hydrated.len() <= SCOUT_HYDRATE_BUDGET);

        // 5. Ephemeral save/load round-trip at `.taskmaster/runs/<id>/scope.json`.
        let saved = save_scope(&dir, "run-1", &parsed).unwrap();
        assert!(saved.ends_with(".taskmaster/runs/run-1/scope.json"));
        assert_eq!(load_scope(&dir, "run-1").as_ref(), Some(&parsed));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
