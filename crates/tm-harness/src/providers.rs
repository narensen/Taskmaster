//! Each harness is asked for its own model list. The ids are not stored here: a provider
//! adds and removes models, and a chosen model has to be one that provider just returned.
//!
//! Listing never starts a coding turn. Claude Code has no models command, so its list is
//! the public catalog Claude Code itself downloads. The other harnesses print their list
//! and exit. Tests parse recorded output and do not run the CLIs.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sandbox::resolve_binary;

const LIST_TIMEOUT: Duration = Duration::from_secs(20);
const CLAUDE_CATALOG_URL: &str = "https://downloads.claude.ai/model-catalog/v1/catalog.json";

/// One model a provider currently offers. `id` is the string passed to that harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    pub id: String,
    pub name: String,
    /// Set when the provider itself marks this model as its default.
    #[serde(default)]
    pub default: bool,
}

/// What the UI needs in order to install a harness and let someone pick a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderReport {
    /// Harness id used by `--master` and the other role flags.
    pub id: String,
    pub name: String,
    pub installed: bool,
    /// Command that installs the CLI on Linux and macOS.
    pub install: String,
    /// Command that signs in. The UI runs it; this crate does not.
    pub login: String,
    pub models: Vec<ModelChoice>,
    /// Why the list is missing or stale. `None` when the live list was read.
    pub error: Option<String>,
}

struct Provider {
    id: &'static str,
    name: &'static str,
    binary: &'static str,
    install: &'static str,
    login: &'static str,
}

const PROVIDERS: [Provider; 5] = [
    Provider {
        id: "claude",
        name: "Claude Code",
        binary: "claude",
        install: "curl -fsSL https://claude.ai/install.sh | bash",
        login: "claude auth login",
    },
    Provider {
        id: "agy",
        name: "Antigravity",
        binary: "agy",
        install: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
        login: "agy",
    },
    Provider {
        id: "grok",
        name: "Grok",
        binary: "grok",
        install: "curl -fsSL https://x.ai/cli/install.sh | bash",
        login: "grok login",
    },
    Provider {
        id: "codex",
        name: "Codex",
        binary: "codex",
        install: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
        login: "codex login",
    },
    Provider {
        id: "opencode",
        name: "OpenCode",
        binary: "opencode",
        install: "curl -fsSL https://opencode.ai/v2/install | bash",
        login: "opencode auth login",
    },
];

/// Ask every harness which models it offers right now.
pub async fn provider_reports() -> Vec<ProviderReport> {
    let (claude, agy, grok, codex, opencode) = tokio::join!(
        provider_report("claude"),
        provider_report("agy"),
        provider_report("grok"),
        provider_report("codex"),
        provider_report("opencode"),
    );
    vec![claude, agy, grok, codex, opencode]
}

/// Ask one harness which models it offers right now.
pub async fn provider_report(id: &str) -> ProviderReport {
    let Some(spec) = PROVIDERS.iter().find(|spec| spec.id == id) else {
        return ProviderReport {
            id: id.to_string(),
            name: id.to_string(),
            installed: false,
            install: String::new(),
            login: String::new(),
            models: Vec::new(),
            error: Some(format!("unknown harness `{id}`")),
        };
    };
    if resolve_binary(spec.binary).is_err() {
        return report(spec, false, Vec::new(), None);
    }
    match list_models(spec).await {
        Ok((models, note)) if models.is_empty() => {
            let why = note.unwrap_or_else(|| format!("{} returned no models you can choose", spec.id));
            report(spec, true, models, Some(why))
        }
        Ok((models, note)) => report(spec, true, models, note),
        Err(err) => report(spec, true, Vec::new(), Some(err.to_string())),
    }
}

/// Reject `model` unless this report's live list contains it.
pub fn require_model(report: &ProviderReport, flag: &str, model: &str) -> anyhow::Result<()> {
    if !report.installed {
        bail!(
            "{} is not installed, so its models cannot be listed. Install it with:\n{}",
            report.id,
            report.install
        );
    }
    if report.models.is_empty() {
        let why = report.error.clone().unwrap_or_else(|| "it returned no models".to_string());
        bail!("could not list models for {}: {why}", report.id);
    }
    if report.models.iter().any(|choice| choice.id == model) {
        return Ok(());
    }
    let mut ids: Vec<&str> = report.models.iter().map(|choice| choice.id.as_str()).collect();
    let extra = ids.len().saturating_sub(40);
    ids.truncate(40);
    let mut offered = ids.join(", ");
    if extra > 0 {
        offered.push_str(&format!(", and {extra} more"));
    }
    bail!("{flag} `{model}` is not a model {} offers. It offers: {offered}", report.id)
}

fn report(spec: &Provider, installed: bool, models: Vec<ModelChoice>, error: Option<String>) -> ProviderReport {
    ProviderReport {
        id: spec.id.to_string(),
        name: spec.name.to_string(),
        installed,
        install: spec.install.to_string(),
        login: spec.login.to_string(),
        models,
        error,
    }
}

async fn list_models(spec: &Provider) -> anyhow::Result<(Vec<ModelChoice>, Option<String>)> {
    match spec.id {
        "claude" => list_claude().await,
        "agy" => list_text(spec.binary, &["models"], parse_agy_models).await,
        "grok" => list_text(spec.binary, &["models"], parse_grok_models).await,
        "codex" => list_codex().await,
        "opencode" => list_text(spec.binary, &["models"], parse_opencode_models).await,
        other => bail!("unknown harness `{other}`"),
    }
}

async fn list_text(
    program: &str,
    args: &[&str],
    parse: fn(&str) -> anyhow::Result<Vec<ModelChoice>>,
) -> anyhow::Result<(Vec<ModelChoice>, Option<String>)> {
    let captured = run_cmd(program, args).await?;
    if !captured.ok {
        bail!("{}", command_failure(program, &captured));
    }
    Ok((parse(&captured.stdout)?, None))
}

async fn list_claude() -> anyhow::Result<(Vec<ModelChoice>, Option<String>)> {
    if resolve_binary("curl").is_err() {
        return from_claude_cache("curl is not installed, so the live catalog could not be fetched");
    }
    let fetched = run_cmd("curl", &["-fsSL", "--max-time", "15", CLAUDE_CATALOG_URL]).await;
    let live_failure = match fetched {
        Err(err) => format!("the live catalog could not be fetched ({err})"),
        Ok(captured) if !captured.ok => {
            let detail = brief(&captured.stderr)
                .or_else(|| brief(&captured.stdout))
                .unwrap_or_else(|| "curl failed".to_string());
            format!("the live catalog could not be fetched ({detail})")
        }
        Ok(captured) => match parse_claude_catalog(&captured.stdout) {
            Ok(models) if !models.is_empty() => return Ok((models, None)),
            Ok(_) => "the live catalog listed no models".to_string(),
            Err(err) => format!("the live catalog could not be read ({err})"),
        },
    };
    from_claude_cache(&live_failure)
}

fn from_claude_cache(live_failure: &str) -> anyhow::Result<(Vec<ModelChoice>, Option<String>)> {
    let Some(text) = cached_claude_catalog() else {
        bail!("{live_failure}");
    };
    match parse_claude_catalog(&text) {
        Ok(models) if !models.is_empty() => {
            Ok((models, Some(format!("{live_failure}; showing the last saved catalog"))))
        }
        _ => bail!("{live_failure}"),
    }
}

/// Claude Code's last successful catalog download. Used only when the live file cannot be read.
fn cached_claude_catalog() -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".claude/cache/model-catalog");
    let path = newest_json(&dir, Some("-cc.json")).or_else(|| newest_json(&dir, None))?;
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() { None } else { Some(text) }
}

fn newest_json(dir: &Path, suffix: Option<&str>) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if !name.ends_with(".json") {
            continue;
        }
        if suffix.is_some_and(|suffix| !name.ends_with(suffix)) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(time, _)| modified >= *time) {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

async fn list_codex() -> anyhow::Result<(Vec<ModelChoice>, Option<String>)> {
    match run_codex(&["debug", "models"]).await {
        Ok(models) => Ok((models, None)),
        Err(live) => match run_codex(&["debug", "models", "--bundled"]).await {
            Ok(models) if !models.is_empty() => Ok((
                models,
                Some(format!(
                    "the live model list could not be fetched ({live}); showing the catalog shipped with codex"
                )),
            )),
            _ => Err(live),
        },
    }
}

async fn run_codex(args: &[&str]) -> anyhow::Result<Vec<ModelChoice>> {
    let captured = run_cmd("codex", args).await?;
    if !captured.ok {
        bail!("{}", command_failure("codex", &captured));
    }
    parse_codex_models(&captured.stdout)
}

struct Captured {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn run_cmd(program: &str, args: &[&str]) -> anyhow::Result<Captured> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(LIST_TIMEOUT, cmd.output()).await {
        Ok(result) => result.with_context(|| format!("could not run {program}"))?,
        Err(_) => bail!("{program} timed out after 20s"),
    };
    Ok(Captured {
        ok: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn command_failure(program: &str, captured: &Captured) -> String {
    match brief(&captured.stderr).or_else(|| brief(&captured.stdout)) {
        Some(line) => format!("{program} failed: {line}"),
        None => format!("{program} failed"),
    }
}

fn brief(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(200).collect())
}

fn push_model(out: &mut Vec<ModelChoice>, id: &str, name: &str, default: bool) {
    let id = id.trim();
    if id.is_empty() || out.iter().any(|choice| choice.id == id) {
        return;
    }
    let name = name.trim();
    out.push(ModelChoice {
        id: id.to_string(),
        name: if name.is_empty() { id.to_string() } else { name.to_string() },
        default,
    });
}

/// `grok models`: bullet lines, with `(default)` on the provider's default.
pub fn parse_grok_models(text: &str) -> anyhow::Result<Vec<ModelChoice>> {
    let mut models = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let rest = line.strip_prefix("* ").or_else(|| line.strip_prefix("- ")).unwrap_or("").trim();
        if rest.is_empty() {
            continue;
        }
        let (id, default) = match rest.strip_suffix(" (default)") {
            Some(id) => (id.trim(), true),
            None => (rest, false),
        };
        if id.is_empty() || id.contains(char::is_whitespace) {
            continue;
        }
        push_model(&mut models, id, id, default);
    }
    if models.is_empty() {
        bail!("grok printed no models");
    }
    Ok(models)
}

/// `agy models`: a status line, then `id<TAB>name` rows. Antigravity marks no default.
pub fn parse_agy_models(text: &str) -> anyhow::Result<Vec<ModelChoice>> {
    let mut models = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((id, name)) = line.split_once('\t') else { continue };
        if id.eq_ignore_ascii_case("id") || id.eq_ignore_ascii_case("model") {
            continue;
        }
        push_model(&mut models, id, name, false);
    }
    if models.is_empty() {
        bail!("agy printed no models");
    }
    Ok(models)
}

/// `opencode models`: one `provider/model` per line.
pub fn parse_opencode_models(text: &str) -> anyhow::Result<Vec<ModelChoice>> {
    let mut models = Vec::new();
    for line in text.lines() {
        let Some(token) = line.split_whitespace().next() else { continue };
        if token.contains('/') && !token.contains("://") {
            push_model(&mut models, token, token, false);
        }
    }
    if models.is_empty() {
        bail!("opencode printed no models");
    }
    Ok(models)
}

/// `codex debug models`: a JSON catalog. Models with visibility other than `list` are hidden.
/// A missing visibility is still choosable. Codex does not mark a default in this catalog.
pub fn parse_codex_models(text: &str) -> anyhow::Result<Vec<ModelChoice>> {
    let value = json_document(text)?;
    let Some(entries) = value.get("models").and_then(Value::as_array) else {
        bail!("codex catalog has no models");
    };
    let mut models = Vec::new();
    let mut saw_slug = false;
    for entry in entries {
        let Some(id) = entry.get("slug").and_then(Value::as_str) else { continue };
        saw_slug = true;
        if let Some(visibility) = entry.get("visibility").and_then(Value::as_str)
            && visibility != "list" {
                continue;
            }
        let name = entry.get("display_name").and_then(Value::as_str).unwrap_or(id);
        push_model(&mut models, id, name, false);
    }
    if models.is_empty() && !saw_slug && !entries.is_empty() {
        bail!("codex catalog listed no models");
    }
    Ok(models)
}

fn json_document(text: &str) -> anyhow::Result<Value> {
    let text = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
            && let Ok(value) = serde_json::from_str::<Value>(&text[start..=end]) {
                return Ok(value);
            }
    bail!("codex catalog is not JSON")
}

/// Claude Code's public catalog, or the cache file written after a successful download.
/// Full ids and the short aliases (`opus`, `sonnet`, `haiku`, `fable`) are both choosable.
/// The default is the catalog's `global_default`, not a model someone already picked.
pub fn parse_claude_catalog(text: &str) -> anyhow::Result<Vec<ModelChoice>> {
    let value: Value = serde_json::from_str(text.trim()).context("claude catalog is not JSON")?;
    let (models, aliases, state) = claude_parts(&value).context("claude catalog has no model list")?;
    let entries = models.as_array().context("claude catalog has no model list")?;
    let default_id = catalog_default(state);
    let mut out = Vec::new();
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_str) else { continue };
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .or_else(|| entry.get("short_name").and_then(Value::as_str))
            .unwrap_or(id);
        push_model(&mut out, id, name, default_id == Some(id));
    }
    if let Some(aliases) = aliases.and_then(Value::as_object) {
        for alias in aliases.keys() {
            push_model(&mut out, alias, alias, false);
        }
    }
    if out.is_empty() {
        bail!("claude catalog listed no models");
    }
    Ok(out)
}

fn claude_parts(value: &Value) -> Option<(&Value, Option<&Value>, Option<&Value>)> {
    if let Some(config) = value.pointer("/surfaces/cc/model_selector_config/0") {
        return Some((
            config.get("models")?,
            config.get("provider_alias_targets"),
            value.pointer("/surfaces/cc/model_selector_state/0"),
        ));
    }
    if let Some(catalog) = value.get("catalog") {
        let config = catalog.get("config")?;
        return Some((config.get("models")?, config.get("provider_alias_targets"), catalog.get("state")));
    }
    Some((value.get("models")?, value.get("provider_alias_targets"), None))
}

fn catalog_default(state: Option<&Value>) -> Option<&str> {
    let state = state?;
    let source = state.get("selection_source").and_then(Value::as_str);
    if source.is_some_and(|source| source != "global_default") {
        return None;
    }
    state.get("model").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_has_an_install_and_a_list() {
        assert_eq!(PROVIDERS.len(), crate::HARNESSES.len());
        for id in crate::HARNESSES {
            let spec = PROVIDERS.iter().find(|spec| spec.id == id).unwrap_or_else(|| panic!("{id}"));
            assert!(!spec.install.is_empty());
            assert!(!spec.login.is_empty());
            assert!(!spec.binary.is_empty());
        }
    }

    #[test]
    fn grok_marks_its_default_and_ignores_the_preamble() {
        let text = "\
You are logged in with grok.com.
Default model: grok-4.7
Available models:
* grok-4.7 (default)
- grok-4.7-build-fast
- grok-4.6
";
        let models = parse_grok_models(text).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "grok-4.7");
        assert!(models[0].default);
        assert_eq!(models[1].id, "grok-4.7-build-fast");
        assert!(!models[1].default);
        assert!(parse_grok_models("You are logged in.\n").is_err());
    }

    #[test]
    fn agy_reads_tab_separated_ids() {
        let text = "\
Fetching available models...
id\tName
gemini-3.8-flash-high\tGemini 3.8 Flash High
claude-sonnet-4-6\tClaude Sonnet 4.6
";
        let models = parse_agy_models(text).unwrap();
        assert_eq!(
            models,
            vec![
                ModelChoice { id: "gemini-3.8-flash-high".into(), name: "Gemini 3.8 Flash High".into(), default: false },
                ModelChoice { id: "claude-sonnet-4-6".into(), name: "Claude Sonnet 4.6".into(), default: false },
            ]
        );
    }

    #[test]
    fn codex_keeps_listed_models_and_hides_the_rest() {
        let text = r#"note
{"models":[
  {"slug":"gpt-6-luna","display_name":"GPT-6 Luna","visibility":"list"},
  {"slug":"gpt-reserve","display_name":"Reserve","visibility":"hide"},
  {"slug":"plain","display_name":"Plain"}
]}"#;
        let models = parse_codex_models(text).unwrap();
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-luna", "plain"]);
        assert!(models.iter().all(|model| !model.default));
        assert_eq!(models[0].name, "GPT-6 Luna");
        assert!(parse_codex_models("not json").is_err());
    }

    #[test]
    fn claude_catalog_offers_full_ids_and_aliases() {
        let models = parse_claude_catalog(CLAUDE_FIXTURE).unwrap();
        assert!(models[0].default);
        assert_eq!(models[0].id, "claude-opus-5-5");
        assert_eq!(models[0].name, "Opus 5.5");
        assert!(!models[1].default);
        let ids: Vec<_> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(ids, ["claude-opus-5-5", "claude-sonnet-5-5", "opus", "sonnet"]);
    }

    #[test]
    fn a_saved_catalog_does_not_treat_a_user_choice_as_the_default() {
        let text = r#"{
            "catalog": {
                "state": {"model": "claude-sonnet-5-5", "selection_source": "user_setting"},
                "config": {
                    "models": [
                        {"id": "claude-opus-5-5", "name": "Opus 5.5"},
                        {"id": "claude-sonnet-5-5", "name": "Sonnet 5.5"}
                    ],
                    "provider_alias_targets": {"haiku": {"default": "claude-haiku-5-5"}}
                }
            }
        }"#;
        let models = parse_claude_catalog(text).unwrap();
        assert!(models.iter().all(|model| !model.default));
        assert!(models.iter().any(|model| model.id == "haiku"));
    }

    #[test]
    fn opencode_reads_provider_model_lines() {
        let models = parse_opencode_models("openai/gpt-5\nanthropic/claude-sonnet extra\n\nnot a model\n").unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openai/gpt-5");
        assert_eq!(models[1].id, "anthropic/claude-sonnet");
    }

    #[test]
    fn a_missing_install_rejects_a_chosen_model() {
        let report = ProviderReport {
            id: "opencode".into(),
            name: "OpenCode".into(),
            installed: false,
            install: "curl -fsSL https://opencode.ai/v2/install | bash".into(),
            login: "opencode auth login".into(),
            models: Vec::new(),
            error: None,
        };
        let err = require_model(&report, "--master-model", "openai/gpt-5").unwrap_err().to_string();
        assert!(err.contains("not installed"), "{err}");
        assert!(err.contains("opencode.ai/v2/install"), "{err}");
    }

    #[test]
    fn a_chosen_model_must_be_one_the_provider_listed() {
        let models = parse_claude_catalog(CLAUDE_FIXTURE).unwrap();
        let report = ProviderReport {
            id: "claude".into(),
            name: "Claude Code".into(),
            installed: true,
            install: "curl".into(),
            login: "claude auth login".into(),
            models,
            error: None,
        };
        require_model(&report, "--master-model", "opus").unwrap();
        require_model(&report, "--master-model", "claude-sonnet-5-5").unwrap();
        let err = require_model(&report, "--master-model", "claude-nope").unwrap_err().to_string();
        assert!(err.contains("is not a model claude offers"), "{err}");
        assert!(err.contains("claude-opus-5-5"), "{err}");
    }

    #[test]
    fn an_unlistable_provider_rejects_the_override() {
        let report = ProviderReport {
            id: "grok".into(),
            name: "Grok".into(),
            installed: true,
            install: "curl".into(),
            login: "grok login".into(),
            models: Vec::new(),
            error: Some("grok timed out after 20s".into()),
        };
        let err = require_model(&report, "--worker-model", "grok-4.7").unwrap_err().to_string();
        assert!(err.contains("could not list models for grok"), "{err}");
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn a_report_is_json_the_ui_can_read() {
        let report = ProviderReport {
            id: "grok".into(),
            name: "Grok".into(),
            installed: true,
            install: "curl -fsSL https://x.ai/cli/install.sh | bash".into(),
            login: "grok login".into(),
            models: vec![ModelChoice { id: "grok-4.7".into(), name: "grok-4.7".into(), default: true }],
            error: None,
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["id"], "grok");
        assert_eq!(json["installed"], true);
        assert_eq!(json["models"][0]["id"], "grok-4.7");
        assert_eq!(json["models"][0]["default"], true);
        assert!(json["error"].is_null());
        assert!(json["install"].as_str().unwrap().contains("x.ai/cli/install.sh"));
    }

    #[test]
    fn newest_json_prefers_the_claude_code_cache_name() {
        let dir = std::env::temp_dir().join(format!("tm-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "no").unwrap();
        std::fs::write(dir.join("other.json"), "{}").unwrap();
        std::fs::write(dir.join("saved-cc.json"), "{\"models\":[]}").unwrap();
        let found = newest_json(&dir, Some("-cc.json")).unwrap();
        assert_eq!(found.file_name().unwrap(), "saved-cc.json");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const CLAUDE_FIXTURE: &str = r#"{
        "surfaces": {"cc": {
            "model_selector_state": [{"model": "claude-opus-5-5", "selection_source": "global_default"}],
            "model_selector_config": [{
                "models": [
                    {"id": "claude-opus-5-5", "name": "Opus 5.5"},
                    {"id": "claude-sonnet-5-5", "name": "Sonnet 5.5"}
                ],
                "provider_alias_targets": {
                    "opus": {"default": "claude-opus-5-5"},
                    "sonnet": {"default": "claude-sonnet-5-5"}
                }
            }]
        }}
    }"#;
}
