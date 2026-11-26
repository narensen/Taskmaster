//! Fast path for small prompts: a short planning brief with minimal contracts.

const HINTS: [&str; 8] = ["tiny", "simple", "single page", "single-page", "one page", "static", "minimal", "small"];
const MAX_HINTED: usize = 700;
const MAX_BARE: usize = 240;

/// True when the prompt asks for something small. `existing_files` counts project files
/// other than a README and dotfiles.
pub fn is_small_prompt(prompt: &str, existing_files: usize) -> bool {
    let text = prompt.trim().to_lowercase();
    let len = text.chars().count();
    if len == 0 || existing_files > 0 {
        return false;
    }
    let hinted = HINTS.iter().any(|hint| text.contains(hint));
    (hinted && len <= MAX_HINTED) || len <= MAX_BARE
}

/// Reasoning effort for the small planning pass: one short JSON reply needs little thought.
pub const SMALL_EFFORT: &str = "low";

/// What the planner must say about the look and the accessibility of anything shown in a
/// browser. Workers run at low effort: the precision has to come from the plan.
pub const DESIGN_RULE: &str = "For a `web` plan, or one with a `\"ui\": true` unit, only: \
`design` is the one design system every agent follows, written for a finished, polished product, \
in short lines: palette (hex for background, surface, border, text, muted text, accent, danger; text contrast at least 4.5:1); \
type scale (font stack, base 16px, h1, h2 and small sizes); spacing scale (for example 4/8/12/16/24/32px); radius and shadow; \
form controls (inputs and buttons inherit the font and are at least 44px tall; checkboxes and radios 18-20px with `accent-color`, \
never stretched by a rule for text inputs); hover, a visible `:focus-visible` ring, and done and disabled states; \
headings (exactly one `<h1>` per page naming it, `<h2>` for sections); the empty state (its copy, shown only while a list is empty \
and hidden as soon as it has an item; a `[hidden]` element must stay hidden even if a class sets `display`); \
responsive layout (max width, works at 360px wide with no sideways scroll, tap targets of 44px). Accessibility rules: \
`<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">`, a `<label>` or `aria-label` on every input \
and checkbox (also ones made in script), `aria-label` on icon-only buttons, `prefers-reduced-motion` respected. \
Robustness: blank or whitespace-only input adds nothing; user text is shown with `textContent`, never `innerHTML`; \
storage and JSON parsing are wrapped in try/catch and corrupted saved data falls back to empty. \
Every `done` repeats the quality bars that apply to that agent's files. Use \"\" when nothing is shown in a browser.\n";

/// Fixed quality bars every agent of a plan with a design spec is held to. The free checks
/// test each of them in a browser.
pub const QUALITY_BARS: &str = "Quality bars, checked in a browser before the run is done: \
exactly one `<h1>`; every input, checkbox and icon-only button has a label or aria-label, including ones made in script; \
form controls use the design's sizes (no stretched or unstyled native checkbox, inputs and buttons inherit the font); \
a visible `:focus-visible` style; no sideways scroll at 360px wide; the empty state is hidden once a list has items; \
blank input adds nothing; user text is escaped (`textContent`); corrupted localStorage does not break the page.\n";

/// How decisive a planner is told to be. A planner that works out every file in its head
/// spends the whole time limit and never writes the plan.
pub const DECIDE_RULE: &str = "Decide quickly: settle the pages, the shared contracts and the design system, then write the JSON at once. \
Do not work out each file's code or every CSS rule in your head; the workers write those from the design system, and the free \
checks catch what they miss. A plan written now beats a perfect plan that is never written.\n";

/// Maximum units recommended for a single-phase plan (one parallel wave).
pub const SINGLE_PHASE_MAX_UNITS: usize = 4;

/// Effective unit cap for a single-phase plan: at most `slots` units in one wave, capped at 4,
/// and never below 2 so multi-module projects are not forced into one mega-unit.
pub fn single_phase_wave_cap(slots: usize) -> usize {
    slots.clamp(2, SINGLE_PHASE_MAX_UNITS)
}

/// Tell the master model upfront exactly how many parallel workers it can call upon and how
/// to delegate work to them in JSON with self-contained context for each worker.
pub fn worker_pool_rule(slots: usize, worker_label: Option<&str>) -> String {
    let who = worker_label
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    let cap = slots.min(tm_core::MAX_UNITS);
    if cap < 2 {
        return format!(
            "Worker pool: you have 1 worker agent{who} available for this run. \
             Write a concise `summary` of the overall plan, and give the worker complete, self-contained `brief` and `context` fields \
             (exact file paths, exported symbols/types, DOM ids or API shapes, conventions, and edge cases) so it can build cleanly without guessing.\n"
        );
    }
    let wave_cap = single_phase_wave_cap(cap);
    format!(
        "Worker pool: you can call upon up to {cap} parallel worker agents{who} simultaneously in this run. \
         Each unit launches a fresh worker session with cold-start context overhead, so split into cohesive modules that fit in one parallel wave \
         (typically 2-{wave_cap} units for a multi-module project, at most {wave_cap} for a single-phase plan). \
         Merge closely coupled small files (for example tokenizer+parser or btree+storage) into one cohesive unit rather than spawning tiny units, \
         and do not collapse a multi-module project into a single mega-unit. \
         Only use `depends_on: [\"id\"]` for concrete implementation imports (never for locked `contracts`), with chains no deeper than 2 for <=6 units. \
         Include a concise `summary`. Every worker runs in isolation and only sees its `brief`, `context`, `done`, locked `contracts`, `design`, and cited Dictionary entries: \
         give each worker concise, non-redundant `brief` + `context` (only unit-specific notes and sibling boundaries not already stated in locked `contracts` or cited `dictionary` entries).\n"
    )
}

/// How a plan may split a page into work units, given the run's `slots` parallel workers.
/// More units than slots would queue behind each other, so the cap is the slot count.
pub fn units_rule(slots: usize) -> String {
    let most = slots.min(tm_core::MAX_UNITS);
    if most < 2 {
        return "Do not split units into `parts`: this run builds with one worker at a time.\n".into();
    }
    let range = if most == 2 { "2".to_string() } else { format!("2-{most}") };
    format!(
        "Only split a single oversized page/unit (>400 lines or clearly independent large files) into `parts` ({range} work units, up to {most} parallel workers), \
         each {{\"id\",\"owned_glob\",\"brief\",\"context\",\"done\"}}, one worker agent per part. \
         Part globs must be disjoint and inside the unit glob (braces list files: `{{app/index.html,app/styles.css}}`); the unit `owned_glob` must cover them all. \
         Leave ordinary modules unsplit so you do not pay extra cold-session overhead.\n"
    )
}

/// One example plan per kind, so a library or a server is never squeezed into the shape of
/// a website. Kept short: they show the shape, not a product.
pub const PLAN_EXAMPLES: &str = concat!(
    "One example per `kind`:\n",
    "- web: {\"kind\":\"web\",\"summary\":\"...\",\"design\":\"...\",\"contracts\":[{\"path\":\"app/types.ts\",\"content\":\"export type Item = { id: string; title: string };\"}],",
    "\"units\":[{\"id\":\"home\",\"route\":\"/\",\"owned_glob\":\"app/home/**\",\"cites\":[\"theme\"],\"brief\":\"...\",\"context\":\"...\",\"done\":\"...\",\"skills\":[]}],",
    "\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[{\"key\":\"theme\",\"kind\":\"hint\",\"summary\":\"...\",\"content\":\"...\",\"stack\":\"next\"}]}\n",
    "- library: {\"kind\":\"library\",\"summary\":\"...\",\"contracts\":[{\"path\":\"tinydb/contract.py\",\"content\":\"class Table:\\n    def insert(self, doc: dict) -> int: ...\\n\"}],",
    "\"units\":[{\"id\":\"table\",\"owned_glob\":\"{tinydb/table.py,tests/test_table.py}\",\"brief\":\"...\",\"context\":\"...\",\"done\":\"... and `python -m pytest tests/test_table.py` passes\"}],",
    "\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "- cli: {\"kind\":\"cli\",\"summary\":\"...\",\"contracts\":[],\"units\":[{\"id\":\"cli\",\"owned_glob\":\"{src/**,tests/**}\",\"brief\":\"...\",\"context\":\"...\",\"done\":\"... and the tests pass\"}],\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "- server: {\"kind\":\"server\",\"summary\":\"...\",\"contracts\":[{\"path\":\"toyredis/contract.py\",\"content\":\"def encode(value) -> bytes: ...\\ndef decode(data: bytes) -> tuple: ...\\n\"}],",
    "\"units\":[{\"id\":\"protocol\",\"owned_glob\":\"{toyredis/protocol.py,tests/test_protocol.py}\",\"brief\":\"...\",\"context\":\"...\",\"done\":\"...\"},",
    "{\"id\":\"server\",\"owned_glob\":\"{toyredis/server.py,tests/test_server.py}\",\"brief\":\"...\",\"context\":\"...\",\"done\":\"...\"}],\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "- migration: {\"kind\":\"migration\",\"summary\":\"...\",\"contracts\":[],\"units\":[{\"id\":\"upgrade\",\"owned_glob\":\"**\",\"brief\":\"...\",\"context\":\"...\",\"done\":\"... and the existing tests pass\"}],\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "Only a `web` unit (or one with `\"ui\": true`) has a `route`, and only such a plan has a `design`. ",
    "A unit owns its own tests and its `done` names the command that runs them. ",
    "A Python contract sits inside the package the tests import (for example `tinydb/contract.py`), never loose at the project root.\n",
);

/// How the first planning call sketches a job bigger than a small one: the phases in rough,
/// and only the first in detail. Later phases are planned while earlier ones build.
pub const PHASES_RULE: &str = "Sketch first, then detail only the start. A large multi-subsystem job (multiple distinct layers/apps that cannot fit in one parallel wave of up to 4 cohesive units) gets `phases`: \
2-6 phases in build order, each {\"id\",\"goal\",\"delivers\":[paths],\"interfaces\":\"signatures only, no bodies\",\"test\":\"the command \
that runs this phase's tests\",\"needs\":\"interfaces\" or \"results\"}. `needs` is `results` only when a phase cannot be planned \
until the one before it has been built and tested. With `phases`, `units` and `contracts` cover the first phase only; each later phase \
is planned, with its own units, once the phases before it have locked their interfaces. A single-package or small job \
has no `phases`: plan it whole across 2-4 cohesive units in one wave. Do not over-split small modules into tiny units, and do not collapse a multi-module job into one mega-unit.\n";

/// Slot-aware single-phase planning rule: at most `slots` units in one wave (max 4), without collapsing to one mega-unit.
pub fn single_phase_rule(slots: usize) -> String {
    let cap = single_phase_wave_cap(slots);
    let range = if cap == 2 { "2".to_string() } else { format!("2-{cap}") };
    format!(
        "Plan this whole job in a single phase (`\"phases\": []`): put at most {cap} cohesive units in `units` \
         (one parallel wave, {range} units when the job spans multiple distinct modules; never exceed {cap}, and do not collapse a multi-module project into one mega-unit) \
         with disjoint `owned_glob` patterns and shared `contracts` so every worker starts immediately with no phase barrier and no second planning pass. \
         Merge closely coupled small modules (such as tokenizer+parser or btree+storage) into one cohesive unit rather than spawning tiny units.\n"
    )
}

const MULTI_PHASE_CUES: [&str; 16] = [
    "multi-phase",
    "phase 1",
    "phase 2",
    "phases",
    "pages on top",
    "shared core and pages",
    "many parts",
    "full platform",
    "saas",
    "microservices",
    "monorepo",
    "frontend and backend",
    "multiple pages",
    "multiple services",
    "admin panel",
    "entire app",
];

const SINGLE_PACKAGE_CUES: [&str; 24] = [
    "cli",
    "repl",
    "command-line",
    "command line",
    "library",
    "package",
    "module",
    "modules",
    "python -m",
    "stdlib",
    "standard library",
    "single-file",
    "single file",
    "minimal",
    "tiny",
    "small",
    "parser",
    "interpreter",
    "compiler",
    "database engine",
    "kv store",
    "key-value",
    "pytest",
    "cargo test",
];

/// True when a fresh prompt describes a single-package CLI/library/tool (roughly <= 8 units)
/// that fits cleanly in one parallel phase without a phased sketch or heavy planning effort.
pub fn is_single_phase_prompt(prompt: &str, existing_files: usize, parallel: usize) -> bool {
    if is_small_prompt(prompt, existing_files) {
        return true;
    }
    if existing_files > 0 || parallel == 0 {
        return false;
    }
    let text = prompt.trim().to_lowercase();
    let len = text.chars().count();
    if len == 0 || len > 2500 {
        return false;
    }
    if MULTI_PHASE_CUES.iter().any(|cue| text.contains(cue)) {
        return false;
    }
    SINGLE_PACKAGE_CUES.iter().any(|cue| text.contains(cue))
}

/// The brief that plans one later phase of a sketched run.
pub fn phase_brief(
    number: usize,
    total: usize,
    phase: &tm_core::PhaseSketch,
    project_md: &str,
    planned: &str,
    slots: usize,
    worker_label: Option<&str>,
) -> String {
    let mut brief = format!(
        "Plan phase {number} of {total} of this project: `{id}`, {goal}. Do not call any tools: everything you need is below.\n\n\
         {pool}\n\
         PROJECT.md:\n{project_md}\n\nUnits already planned (do not reuse their ids or own their files):\n{planned}\n\
         Reply with one JSON object for this phase only: {{\"summary\":\"...\",\"contracts\":[new interface files only],\"units\":[...],\"dictionary\":[]}}. \
         The locked contracts of earlier phases stand; add a contract only for an interface this phase shares between its own units. \
         Each unit owns its module and its tests, includes complete `brief` and `context`, and its `done` names the command that runs them",
        id = phase.id,
        goal = phase.goal.trim_end_matches('.'),
        pool = worker_pool_rule(slots, worker_label),
    );
    if !phase.test.is_empty() {
        brief.push_str(&format!(" (this phase's tests run with `{}`)", phase.test));
    }
    brief.push_str(".\n");
    brief.push_str(JSON_AND_CONTRACT_LIMIT_RULE);
    brief.push_str(&tm_core::rules_text());
    brief.push_str(&units_rule(slots));
    brief.push_str(DECIDE_RULE);
    brief
}

/// Up-front JSON formatting and contract line-cap rules shown before plan examples so
/// planners avoid avoidable JSON syntax errors and >200-line contract rejections.
pub const JSON_AND_CONTRACT_LIMIT_RULE: &str = "Strict output limits: reply with ONE valid JSON object only \
(no prose outside JSON, no trailing commas, escape quotes and use \\n inside JSON strings—never raw unescaped newlines). \
Keep every locked contract concise: target <= 120 lines and NEVER exceed the hard 200-line limit per contract (400 lines total across the plan).\n";

/// Compact repair prompt for a warm planner session that already has the request and its
/// rejected plan in context: sends only the rejection reasons instead of resending the full plan.
pub fn warm_repair_brief(errors: &[String]) -> String {
    if errors.len() == 1 && errors[0].starts_with("invalid JSON at line ") {
        return format!(
            "Your last JSON reply had a syntax error:\n- {}\n\
             Fix only the JSON syntax (no trailing commas, escape quotes and use \\n inside strings) without rewriting content, \
             and reply with the complete corrected JSON object and nothing else. Do not call any tools.\n",
            errors[0],
        );
    }
    let mut brief = String::from(
        "Your last plan was rejected for these reasons:\n",
    );
    for error in errors {
        brief.push_str(&format!("- {error}\n"));
    }
    brief.push_str(
        "Change ONLY what the reasons above name, keep everything else identical, obey the 200-line hard limit per contract \
         and strict JSON syntax/escaping, and reply with the complete corrected JSON object and nothing else. Do not call any tools.\n",
    );
    brief
}

/// The brief that repairs a rejected plan instead of planning again from nothing: the plan,
/// the reasons, and the rules. The planner changes only what the reasons name.
pub fn repair_brief(prompt: &str, plan_json: &str, errors: &[String]) -> String {
    if errors.len() == 1 && errors[0].starts_with("invalid JSON at line ") {
        return format!(
            "Fix the JSON syntax error in this plan without rewriting or dropping any content. \
             Change only what is needed to make it valid JSON, and reply with the complete corrected JSON object and nothing else. Do not call any tools.\n\n\
             JSON error:\n- {}\n\nThe broken plan JSON:\n{plan_json}\n",
            errors[0],
        );
    }
    let mut brief = String::from(
        "Repair this plan. It was rejected for the reasons below. Change only what the reasons name, keep everything else \
         exactly as it is, and reply with the whole corrected JSON object and nothing else. Do not call any tools.\n\nThe request it plans:\n",
    );
    brief.push_str(prompt.trim());
    brief.push_str("\n\nWhy it was rejected:\n");
    for error in errors {
        brief.push_str(&format!("- {error}\n"));
    }
    brief.push_str("\nThe plan:\n");
    brief.push_str(plan_json);
    brief.push_str("\n\n");
    brief.push_str(&tm_core::rules_text());
    brief
}

/// What the plan reply looks like and what it is checked against, for both planning briefs.
pub fn plan_shape() -> String {
    format!("{PLAN_EXAMPLES}{}", tm_core::rules_text())
}

pub fn small_brief(prompt: &str, entries: &[String], template: &str, slots: usize, worker_label: Option<&str>) -> String {
    let mut brief = format!(
        "Plan this small product quickly. Be brief. Do not call any tools: everything you need is below.\n\n{}\nRequest:\n{prompt}\n\n",
        worker_pool_rule(slots, worker_label),
    );
    if entries.is_empty() {
        brief.push_str("The Dictionary is empty. Add no dictionary entries unless a page truly needs one.\n");
    } else {
        brief.push_str(
            "Stored Dictionary entries. Cite the ones that fit by key in `cites`; never copy them into `dictionary`. \
             Contracts and briefs must agree with the cited entries.\n",
        );
        for entry in entries {
            brief.push_str(entry);
        }
        brief.push_str("Add new dictionary entries only if a page truly needs one that is not listed.\n");
    }
    brief.push_str(template);
    let wave_cap = single_phase_wave_cap(slots);
    let unit_guidance = if slots >= 2 {
        format!("Use at most {wave_cap} cohesive `units` in one parallel wave (2-{wave_cap} when there are multiple distinct modules; do not over-split small files, and do not collapse a multi-module project into one mega-unit).")
    } else {
        "Use 1-2 cohesive units (never over-split tiny files).".to_string()
    };
    brief.push_str(&format!(
        "\n{JSON_AND_CONTRACT_LIMIT_RULE}Do not edit files. {unit_guidance} \
         Include a short `summary`. `kind` is web, library, cli, server or migration. \
         `contracts` are shared interface files, only for interfaces used by more than one file or worker; otherwise `[]`. \
         Each unit (or part) is one worker agent: `brief` is its task, `context` is the exact shared ids/interfaces/conventions it needs, `done` one line saying when it is finished. \
         Leave `estimate` at zero.\n"
    ));
    brief.push_str(&plan_shape());
    let io_examples = crate::gates::extract_prompt_io_examples(prompt);
    if !io_examples.is_empty() {
        brief.push_str(&crate::gates::format_prompt_acceptance_guidance(&io_examples));
    }
    brief.push_str(DESIGN_RULE);
    brief.push_str(DECIDE_RULE);
    brief.push_str(&units_rule(slots));
    brief.push_str("Optional unit `difficulty`: easy|normal|hard (hard starts on the mid tier).\n");
    brief
}

const HEAVY_HINTS: [&str; 17] = [
    "rewrite",
    "from scratch",
    "entire app",
    "multi-phase",
    "architecture",
    "migrate",
    "full platform",
    "redesign",
    "new feature",
    "new page",
    "build page",
    "build a ",
    "create a new ",
    "add a new page",
    "across all",
    "every page",
    "multiple ",
];

const LITE_EDIT_CUES: [&str; 18] = [
    "fix ",
    "fixes ",
    "bug",
    "typo",
    "rename ",
    "tweak ",
    "adjust ",
    "patch ",
    "hotfix",
    "revert ",
    "change the ",
    "update the ",
    "remove the ",
    "replace the ",
    "correct ",
    "resolve ",
    "prevent ",
    "avoid ",
];

const SOURCE_EXTS: [&str; 20] = [
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".py", ".go", ".css",
    ".scss", ".html", ".json", ".toml", ".yaml", ".yml", ".md", ".sql", ".sh", ".vue",
];

/// True when `prompt` mentions a concrete source or config file path (e.g. `src/app.rs`).
fn names_a_file(prompt: &str) -> bool {
    prompt.split_whitespace().any(|raw| {
        let token = raw.trim_matches(|c: char| matches!(c, '`' | '\'' | '"' | ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']'));
        let lower = token.to_ascii_lowercase();
        SOURCE_EXTS.iter().any(|ext| lower.len() > ext.len() && lower.ends_with(ext))
    })
}

/// True when the prompt is a scoped bug fix / small edit in an existing repository that can
/// bypass heavy Master Planning altogether (Direct / Lite Fast-Path: 0 planner tokens ->
/// 1 Code-Mode worker -> deterministic free checks -> done).
pub fn is_lite_edit(prompt: &str, existing_files: usize) -> bool {
    if existing_files == 0 {
        return false;
    }
    let text = prompt.trim().to_lowercase();
    let len = text.chars().count();
    if len == 0 || len > 280 {
        return false;
    }
    if HEAVY_HINTS.iter().any(|h| text.contains(h)) {
        return false;
    }
    LITE_EDIT_CUES.iter().any(|cue| text.contains(cue)) || names_a_file(prompt)
}

/// Synthesize a minimal, valid 1-unit plan deterministically in Rust (0 planning tokens)
/// for scoped edits in existing repos.
pub fn lite_edit_plan(prompt: &str) -> tm_core::Plan {
    let trimmed = prompt.trim();
    let first_line = trimmed.lines().next().unwrap_or(trimmed).trim();
    let headline: String = first_line.chars().take(120).collect();
    tm_core::Plan {
        kind: tm_core::ProjectKind::Migration,
        summary: format!("Direct edit: {headline}"),
        contracts: Vec::new(),
        pages: vec![tm_core::PagePlan {
            id: "edit".into(),
            route: String::new(),
            owned_glob: "**".into(),
            cites: Vec::new(),
            brief: format!(
                "{trimmed}\n\nKeep the change tightly scoped to the files needed for this edit. \
                 Run the relevant project checks and tests before finishing."
            ),
            context: "Direct fast-path edit in an existing repository: use `run_code` (`readMany`/`symbols`/`grep`) \
                      to inspect only the target files, apply minimal edits (`edit`/`patch`/`applyPatch`), \
                      and verify with `test()` / `check()`."
                .into(),
            done: "The requested edit is applied cleanly and the project's deterministic checks and tests pass.".into(),
            ..Default::default()
        }],
        estimate: tm_core::Estimate::default(),
        design: String::new(),
        phases: Vec::new(),
        research: Vec::new(),
    }
}

/// True when the prompt is a short, scoped edit in an existing repository that can use the
/// fast planning pass with the precomputed repo map (0 exploration tool calls).
pub fn is_quick_edit(prompt: &str, existing_files: usize) -> bool {
    if existing_files == 0 {
        return false;
    }
    let text = prompt.trim().to_lowercase();
    let len = text.chars().count();
    if len == 0 || len > 320 {
        return false;
    }
    !HEAVY_HINTS[..8].iter().any(|h| text.contains(h))
}

/// Fast single-shot planning brief for scoped edits in an existing repo, powered by the
/// `.flow` repository summary (or deterministic repo map fallback) so the planner skips file-reading turns.
pub fn quick_edit_brief(
    prompt: &str,
    repo_summary: &str,
    uses_flow_summary: bool,
    entries: &[String],
    slots: usize,
    worker_label: Option<&str>,
) -> String {
    let intro = if uses_flow_summary {
        "Plan this small product quickly. This is a focused edit in an existing repository — the `.flow` repository summary and touched-symbol map below already cover the codebase structure and target symbols; plan directly from it without reading repo files (`read_file`, `ls`, `grep`, and `readMany` are disabled for planning)."
    } else {
        "Plan this small product quickly. This is a focused edit in an existing repository — everything you need is in the repository map below; avoid slow exploration tool calls (or batch at most one read-only `run_code` `readMany` call if you must check a specific file)."
    };
    let mut brief = format!(
        "{intro}\n\n{}\nRequest:\n{prompt}\n\n",
        worker_pool_rule(slots, worker_label),
    );
    if !repo_summary.is_empty() {
        brief.push_str(repo_summary);
        brief.push('\n');
    }
    if !entries.is_empty() {
        brief.push_str("Stored Dictionary entries (cite by key if relevant):\n");
        for entry in entries {
            brief.push_str(entry);
        }
        brief.push('\n');
    }
    brief.push_str(JSON_AND_CONTRACT_LIMIT_RULE);
    brief.push_str(
        "Keep it compact (no `phases`, only the 1-3 units actually touched, exact existing file paths in `owned_glob`, \
         optional `difficulty`: `\"easy\"` | `\"normal\"` | `\"hard\"`, and `done` naming the verification command). \
         Do not edit files. Leave `estimate` at zero.\n",
    );
    brief.push_str(&plan_shape());
    brief.push_str(DECIDE_RULE);
    brief
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_small_prompts() {
        assert!(is_small_prompt("Build a tiny static todo web page (plain HTML, CSS and vanilla JS, no build step).", 0));
        assert!(is_small_prompt("a counter", 0));
        assert!(!is_small_prompt("Build a tiny todo page", 3));
        assert!(!is_small_prompt("", 0));
        assert!(!is_small_prompt(&"build a full SaaS billing platform with teams and roles ".repeat(8), 0));
        assert!(!is_small_prompt(&format!("simple {}", "x ".repeat(400)), 0));
    }

    #[test]
    fn detects_lite_edits_in_existing_repos_and_builds_valid_plan() {
        assert!(is_lite_edit("Fix the off-by-one bug in src/parser.rs", 5));
        assert!(is_lite_edit("Rename fetchUser to loadUser in app/auth.ts", 5));
        assert!(is_lite_edit("Typo in the checkout button label", 2));
        assert!(is_lite_edit("Adjust timeout in crates/tm-daemon/src/code.rs to 120s", 4));
        // Empty repos, multi-module/new-page features, and rewrites must NOT skip Master Planning.
        assert!(!is_lite_edit("Fix the off-by-one bug in src/parser.rs", 0));
        assert!(!is_lite_edit("Build page a with a greeting.", 3));
        assert!(!is_lite_edit("Build a new dashboard page with billing and analytics", 5));
        assert!(!is_lite_edit("Rewrite the auth architecture across all pages", 5));

        let plan = lite_edit_plan("Fix off-by-one in src/parser.rs");
        assert_eq!(plan.pages.len(), 1);
        assert_eq!(plan.pages[0].id, "edit");
        assert_eq!(plan.pages[0].owned_glob, "**");
        assert!(tm_core::check_plan(&plan, &[], &std::collections::BTreeMap::new()).is_ok());
    }

    #[test]
    fn small_brief_is_short_and_needs_no_tools() {
        let brief = small_brief("tiny todo", &[], "", 3, Some("claude / sonnet"));
        assert!(brief.contains("Dictionary is empty"));
        assert!(brief.contains("Do not call any tools"));
        assert!(!brief.contains("Search the Dictionary"));
        assert!(brief.contains("up to 3 parallel worker agents (claude / sonnet)"));
        assert!(!brief.contains("Use as few units as possible"));
        // The examples, the rules and the design system are most of it; keep it under two thousand tokens.
        assert!(brief.len() < 8000, "{}", brief.len());
        assert!(brief.contains("\"kind\":\"library\"") && brief.contains(tm_core::Rule::ContractNotOwned.text()));
        assert!(brief.contains("`design`") && brief.contains("viewport"));
        let listed = small_brief("tiny todo", &["- `todo-dom` (commons, component_api): ids\nul#todo-list\n".into()], "", 3, None);
        assert!(!listed.contains("Dictionary is empty"));
        assert!(listed.contains("`todo-dom`"));
        assert!(listed.contains("never copy them"));
    }

    #[test]
    fn units_are_capped_at_the_parallel_workers() {
        assert!(units_rule(3).contains("2-3 work units"));
        assert!(units_rule(8).contains("2-8 work units"));
        assert!(units_rule(24).contains("2-16 work units"));
        assert!(units_rule(2).contains("(2 work units"));
        assert!(units_rule(1).contains("Do not split units into `parts`"));
        assert!(!small_brief("tiny todo", &[], "", 1, None).contains("Parallel parts finish sooner"));
    }

    #[test]
    fn worker_pool_rule_always_informs_count_and_context() {
        let multi = worker_pool_rule(4, Some("codex / gpt-5"));
        assert!(multi.contains("up to 4 parallel worker agents (codex / gpt-5)"));
        assert!(!multi.contains("ALWAYS delegate across multiple parallel workers"));
        assert!(multi.contains("cohesive modules"));
        assert!(multi.contains("do not collapse a multi-module project into a single mega-unit"));
        assert!(multi.contains("`context`"));
        let single = worker_pool_rule(1, Some("claude"));
        assert!(single.contains("1 worker agent (claude)"));
        assert!(single.contains("`context`"));
    }

    #[test]
    fn single_phase_rule_caps_units_at_slots_max_four() {
        assert_eq!(single_phase_wave_cap(1), 2);
        assert_eq!(single_phase_wave_cap(2), 2);
        assert_eq!(single_phase_wave_cap(3), 3);
        assert_eq!(single_phase_wave_cap(4), 4);
        assert_eq!(single_phase_wave_cap(8), 4);

        let r3 = single_phase_rule(3);
        assert!(r3.contains("at most 3 cohesive units"));
        assert!(r3.contains("do not collapse a multi-module project into one mega-unit"));
        let r8 = single_phase_rule(8);
        assert!(r8.contains("at most 4 cohesive units"));
        assert!(!units_rule(4).contains("Only omit `parts` when a unit is truly one tightly coupled file"));
        assert!(!small_brief("tiny todo", &[], "", 4, None).contains("Parallel parts finish sooner"));
    }
}
