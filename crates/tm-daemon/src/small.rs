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

/// The short planning brief. `entries` lists stored Dictionary entries, one per item, already
/// formatted; they are given in full here, so the master needs no tools at all.
/// How a plan may split a page into work units, given the run's `slots` parallel workers.
/// More units than slots would queue behind each other, so the cap is the slot count.
pub fn units_rule(slots: usize) -> String {
    let most = slots.min(4);
    if most < 2 {
        return "Do not split units into `parts`: this run builds with one worker at a time.\n".into();
    }
    let range = if most == 2 { "2".to_string() } else { format!("2-{most}") };
    format!(
        "Optionally split a unit into `parts`: {range} work units (never more than {most}, the parallel workers this run has), \
         each {{\"id\",\"owned_glob\",\"brief\",\"done\"}}, one agent per part, built in parallel. Split by file or concern (for example markup+styles, \
         scripts, docs, or one module and its tests) only when the parts' files are independent given the contracts; part globs must be disjoint and \
         inside the unit glob (braces list files: `{{app/index.html,app/styles.css}}`); the unit owned_glob must cover them \
         all. Omit `parts` when splitting is unsafe.\n"
    )
}

/// One example plan per kind, so a library or a server is never squeezed into the shape of
/// a website. Kept short: they show the shape, not a product.
pub const PLAN_EXAMPLES: &str = concat!(
    "One example per `kind`:\n",
    "- web: {\"kind\":\"web\",\"design\":\"...\",\"contracts\":[{\"path\":\"app/types.ts\",\"content\":\"export type Item = { id: string; title: string };\"}],",
    "\"units\":[{\"id\":\"home\",\"route\":\"/\",\"owned_glob\":\"app/home/**\",\"cites\":[\"theme\"],\"brief\":\"...\",\"done\":\"...\",\"skills\":[]}],",
    "\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[{\"key\":\"theme\",\"kind\":\"hint\",\"summary\":\"...\",\"content\":\"...\",\"stack\":\"next\"}]}\n",
    "- library: {\"kind\":\"library\",\"contracts\":[{\"path\":\"tinydb/contract.py\",\"content\":\"class Table:\\n    def insert(self, doc: dict) -> int: ...\\n\"}],",
    "\"units\":[{\"id\":\"table\",\"owned_glob\":\"{tinydb/table.py,tests/test_table.py}\",\"brief\":\"...\",\"done\":\"... and `python -m pytest tests/test_table.py` passes\"}],",
    "\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "- cli: {\"kind\":\"cli\",\"contracts\":[],\"units\":[{\"id\":\"cli\",\"owned_glob\":\"{src/**,tests/**}\",\"brief\":\"...\",\"done\":\"... and the tests pass\"}],\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "- server: {\"kind\":\"server\",\"contracts\":[{\"path\":\"toyredis/contract.py\",\"content\":\"def encode(value) -> bytes: ...\\ndef decode(data: bytes) -> tuple: ...\\n\"}],",
    "\"units\":[{\"id\":\"protocol\",\"owned_glob\":\"{toyredis/protocol.py,tests/test_protocol.py}\",\"brief\":\"...\",\"done\":\"...\"},",
    "{\"id\":\"server\",\"owned_glob\":\"{toyredis/server.py,tests/test_server.py}\",\"brief\":\"...\",\"done\":\"...\"}],\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "- migration: {\"kind\":\"migration\",\"contracts\":[],\"units\":[{\"id\":\"upgrade\",\"owned_glob\":\"**\",\"brief\":\"...\",\"done\":\"... and the existing tests pass\"}],\"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n",
    "Only a `web` unit (or one with `\"ui\": true`) has a `route`, and only such a plan has a `design`. ",
    "A unit owns its own tests and its `done` names the command that runs them. ",
    "A Python contract sits inside the package the tests import (for example `tinydb/contract.py`), never loose at the project root.\n",
);

/// How the first planning call sketches a job bigger than a small one: the phases in rough,
/// and only the first in detail. Later phases are planned while earlier ones build.
pub const PHASES_RULE: &str = "Sketch first, then detail only the start. A job of more than about five files gets `phases`: \
2-6 phases in build order, each {\"id\",\"goal\",\"delivers\":[paths],\"interfaces\":\"signatures only, no bodies\",\"test\":\"the command \
that runs this phase's tests\",\"needs\":\"interfaces\" or \"results\"}. `needs` is `results` only when a phase cannot be planned \
until the one before it has been built and tested. With `phases`, `units` and `contracts` cover the first phase only; each later phase \
is planned, with its own units, once the phases before it have locked their interfaces. A small job (a dependency upgrade, a fix, \
or fewer than about five files) has no `phases`: plan it whole with one or two units. Scale the units to the work: one per \
independent module and its tests, never more than the parallel workers.\n";

/// The brief that plans one later phase of a sketched run.
pub fn phase_brief(number: usize, total: usize, phase: &tm_core::PhaseSketch, project_md: &str, planned: &str, slots: usize) -> String {
    let mut brief = format!(
        "Plan phase {number} of {total} of this project: `{id}`, {goal}. Do not call any tools: everything you need is below.\n\n\
         PROJECT.md:\n{project_md}\n\nUnits already planned (do not reuse their ids or own their files):\n{planned}\n\
         Reply with one JSON object for this phase only: {{\"contracts\":[new interface files only],\"units\":[...],\"dictionary\":[]}}. \
         The locked contracts of earlier phases stand; add a contract only for an interface this phase shares between its own units. \
         Each unit owns its module and its tests, and its `done` names the command that runs them",
        id = phase.id,
        goal = phase.goal.trim_end_matches('.'),
    );
    if !phase.test.is_empty() {
        brief.push_str(&format!(" (this phase's tests run with `{}`)", phase.test));
    }
    brief.push_str(".\n");
    brief.push_str(&tm_core::rules_text());
    brief.push_str(&units_rule(slots));
    brief.push_str(DECIDE_RULE);
    brief
}

/// The brief that repairs a rejected plan instead of planning again from nothing: the plan,
/// the reasons, and the rules. The planner changes only what the reasons name.
pub fn repair_brief(prompt: &str, plan_json: &str, errors: &[String]) -> String {
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

pub fn small_brief(prompt: &str, entries: &[String], template: &str, slots: usize) -> String {
    let mut brief = format!(
        "Plan this small product quickly. Be brief. Do not call any tools: everything you need is below.\n\nRequest:\n{prompt}\n\n"
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
    brief.push_str(
        "\nReply with one JSON object only and do not edit files. Use as few units as possible (one if it fits). \
         `kind` is web, library, cli, server or migration. \
         `contracts` are shared interface files, only for interfaces used by more than one file or worker; otherwise `[]`. \
         Each unit (or part) is one agent: `brief` is its whole task in a few lines, `done` one line saying when it is finished. \
         Keep contracts short. Leave `estimate` at zero.\n",
    );
    brief.push_str(&plan_shape());
    brief.push_str(DESIGN_RULE);
    brief.push_str(DECIDE_RULE);
    brief.push_str(&units_rule(slots));
    if slots >= 2 {
        brief.push_str(
            "Parallel parts finish sooner: when a unit has separate markup and styles, scripts, docs, or modules and their tests, split it into parts \
             and pin every id, file name and interface they share in a contract, a cited entry or each part's brief.\n",
        );
    }
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
    fn small_brief_is_short_and_needs_no_tools() {
        let brief = small_brief("tiny todo", &[], "", 3);
        assert!(brief.contains("Dictionary is empty"));
        assert!(brief.contains("Do not call any tools"));
        assert!(!brief.contains("Search the Dictionary"));
        // The examples, the rules and the design system are most of it; keep it under two thousand tokens.
        assert!(brief.len() < 8000, "{}", brief.len());
        assert!(brief.contains("\"kind\":\"library\"") && brief.contains(tm_core::Rule::ContractNotOwned.text()));
        assert!(brief.contains("`design`") && brief.contains("viewport"));
        let listed = small_brief("tiny todo", &["- `todo-dom` (commons, component_api): ids\nul#todo-list\n".into()], "", 3);
        assert!(!listed.contains("Dictionary is empty"));
        assert!(listed.contains("`todo-dom`"));
        assert!(listed.contains("never copy them"));
    }

    #[test]
    fn units_are_capped_at_the_parallel_workers() {
        assert!(units_rule(3).contains("2-3 work units"));
        assert!(units_rule(8).contains("2-4 work units"));
        assert!(units_rule(2).contains(": 2 work units"));
        assert!(units_rule(1).contains("Do not split units into `parts`"));
        assert!(!small_brief("tiny todo", &[], "", 1).contains("Parallel parts finish sooner"));
    }
}
