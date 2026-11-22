//! One agent instead of a master and its workers.
//!
//! In Agent mode a run has no planning call: one agent owns every file in the project and
//! does the whole request. A note on a finished run is a new turn. In Agent mode (and in
//! Auto, unless the note asks for something big) that turn is one agent too, with the note
//! as its task: it fixes what is wrong across as many files as it takes, and nothing that
//! already passed is built again. In Code mode the turn is the master and its workers, as
//! before.

use serde_json::{Value, json};
use tm_core::{PagePlan, Plan, ProjectKind};

use crate::RunMode;
use crate::live::Note;

/// Words that say a request is something a browser shows.
const WEB_WORDS: [&str; 22] = [
    "web", "website", "site", "page", "landing", "portfolio", "dashboard", "html", "css", "frontend", "front-end", "react",
    "vue", "svelte", "next.js", "vite", "browser", "ui", "game", "app", "blog", "form",
];
/// Words that say it is not, and win over the ones above.
const NOT_WEB_WORDS: [&str; 10] = ["cli", "command-line", "command line", "library", "crate", "daemon", "rest api", "backend only", "script that", "terminal"];

/// Phrases that ask for more than one agent should take on: new pages, a rewrite.
const BIG_ASKS: [&str; 14] = [
    "rewrite",
    "from scratch",
    "redesign",
    "new page",
    "new pages",
    "add a page",
    "add pages",
    "new feature",
    "new features",
    "build a ",
    "create a new ",
    "architecture",
    "multi-phase",
    "full platform",
];

/// What every agent that owns a whole project is told about the live preview.
pub const PREVIEW_RULE: &str = "The project must open in Taskmaster's live preview when you finish. \
Either keep a static `index.html` at the project root (or in `public/`, `src/`, `app/` or `dist/`) that loads its scripts \
and styles by relative paths, or give `package.json` a `dev` script that serves the app on the port in `$PORT` \
(Vite: `vite --port $PORT --host 127.0.0.1`), with its dependencies listed so an install brings them in. \
If neither exists, add whichever fits the project. Never leave a `dev` or `start` script that names a file or tool that is not there.";

/// True when a request reads as something a browser shows.
pub fn looks_like_web(prompt: &str) -> bool {
    let text = format!(" {} ", prompt.to_lowercase());
    if NOT_WEB_WORDS.iter().any(|word| text.contains(word)) {
        return false;
    }
    WEB_WORDS.iter().any(|word| {
        text.match_indices(word).any(|(at, _)| {
            let before = text[..at].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
            let after = text[at + word.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
            before && after
        })
    })
}

/// True when a follow-up asks for more than one agent should do alone.
pub fn wants_master(text: &str) -> bool {
    let text = text.to_lowercase();
    BIG_ASKS.iter().any(|ask| text.contains(ask)) || text.chars().count() > 1200
}

/// Who takes a follow-up: the last note that named a mode decides, then the run's mode.
/// `Auto` gives a follow-up to one agent unless it asks for something big.
pub fn turn_mode(notes: &[Note], run_mode: RunMode) -> RunMode {
    let named = notes.iter().rev().find_map(|note| note.mode.as_deref().and_then(RunMode::parse)).filter(|mode| *mode != RunMode::Auto);
    match named.unwrap_or(run_mode) {
        RunMode::Code => RunMode::Code,
        RunMode::Agent => RunMode::Agent,
        RunMode::Auto => {
            let text: String = notes.iter().map(|note| note.text.as_str()).collect::<Vec<_>>().join("\n");
            if wants_master(&text) { RunMode::Code } else { RunMode::Agent }
        }
    }
}

const CONTEXT: &str = "You are the only agent: there is no master and there are no other workers, and you own every file in the project. \
Inspect with `run_code` (`ls`, `readMany`, `grep`, `symbols`) before you change anything, make the edits across every file that needs one \
(`edit`, `patch`, `applyPatch`, `write`), and verify with `test()` and `check()`. Keep what already works.";

/// The one unit of an agent's plan. A `route` makes it a page a browser shows.
struct Unit {
    id: String,
    brief: String,
    context: String,
    done: &'static str,
    route: Option<String>,
    design: String,
    summary: String,
}

fn one_unit(unit: Unit) -> Plan {
    let web = unit.route.is_some();
    Plan {
        kind: if web { ProjectKind::Web } else { ProjectKind::Migration },
        summary: unit.summary,
        contracts: Vec::new(),
        pages: vec![PagePlan {
            id: unit.id,
            route: unit.route.unwrap_or_default(),
            owned_glob: "**".into(),
            brief: unit.brief,
            context: unit.context,
            done: unit.done.into(),
            ui: Some(web),
            ..Default::default()
        }],
        estimate: tm_core::Estimate::default(),
        design: if web { unit.design } else { String::new() },
        phases: Vec::new(),
        research: Vec::new(),
    }
}

fn headline(text: &str) -> String {
    let first = text.trim().lines().next().unwrap_or("").trim();
    let mut line: String = first.chars().take(100).collect();
    if first.chars().count() > 100 {
        line.push('…');
    }
    line
}

/// The plan of a new run in Agent mode: one unit, owning the whole project, no planning call.
/// `notes` are what the person queued for the master before it started.
pub fn agent_plan(prompt: &str, notes: &[String], existing_files: usize) -> Plan {
    let web = existing_files == 0 && looks_like_web(prompt);
    let mut brief = prompt.trim().to_string();
    if !notes.is_empty() {
        brief.push_str("\n\nNotes from the person running Taskmaster:\n");
        for note in notes {
            brief.push_str(&format!("- {}\n", note.trim()));
        }
    }
    let mut context = String::from(CONTEXT);
    if existing_files == 0 {
        context.push_str(" The project is empty: you build all of it, file by file.");
    } else {
        context.push_str(" This is an existing repository: change only what the request needs.");
    }
    context.push(' ');
    context.push_str(PREVIEW_RULE);
    if web {
        context.push(' ');
        context.push_str(crate::small::QUALITY_BARS.trim());
    }
    one_unit(Unit {
        id: "agent".into(),
        brief,
        context,
        done: "The request is done, the project's checks and tests pass, and it opens in the live preview.",
        route: web.then(|| "/".to_string()),
        design: String::new(),
        summary: format!("One agent: {}", headline(prompt)),
    })
}

/// The plan of follow-up turn `turn` in Agent mode. `previous` is the plan the run had, so
/// a project shown in a browser stays one, with its design.
pub fn turn_plan(turn: usize, notes: &[Note], prompt: Option<&str>, previous: Option<&Plan>, failed: Option<&str>) -> Plan {
    let asked = notes.iter().map(|note| note.text.trim()).filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n\n");
    let mut brief = asked.clone();
    brief.push_str("\n\nThis is a follow-up on an earlier run of this project.");
    if let Some(error) = failed.map(str::trim).filter(|e| !e.is_empty()) {
        let error: String = error.chars().take(1500).collect();
        brief.push_str(&format!(" That run failed with:\n{error}\n"));
    }
    if let Some(prompt) = prompt.map(str::trim).filter(|p| !p.is_empty()) {
        let prompt: String = prompt.chars().take(2000).collect();
        brief.push_str(&format!("\nIts original request was:\n{prompt}\n"));
    }
    brief.push_str(
        "\nWork out what is wrong (a missing file, a broken import, a script that names something absent, a failing test) \
         and fix it in every file that needs it. Do not rebuild what already works.",
    );
    let ui_page = previous.and_then(|plan| plan.pages.iter().find(|page| plan.is_ui(page)));
    let web = ui_page.is_some();
    let route = ui_page.map(|page| page.route.trim()).filter(|r| !r.is_empty()).unwrap_or("/").to_string();
    let design = previous.map(|plan| plan.design.clone()).unwrap_or_default();
    let mut context = format!("{CONTEXT} {PREVIEW_RULE}");
    if web {
        context.push(' ');
        context.push_str(crate::small::QUALITY_BARS.trim());
    }
    one_unit(Unit {
        id: format!("agent-{turn}"),
        brief,
        context,
        done: "The follow-up is handled, the project's checks and tests pass, and it opens in the live preview.",
        route: web.then_some(route),
        design,
        summary: format!("Follow-up {turn}, one agent: {}", headline(&asked)),
    })
}

/// The transcript entry that opens turn `turn`: who said what, and who takes it. It is
/// written by the person, so a reader can split the run's history into turns.
pub fn turn_entry(turn: usize, mode: RunMode, notes: &[Note]) -> (Value, Value) {
    let text = notes.iter().map(|note| note.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
    let ids: Vec<&str> = notes.iter().map(|note| note.id.as_str()).collect();
    (json!({ "id": "person", "role": "person" }), json!({ "turn": turn, "mode": mode.id(), "text": text, "notes": ids }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(text: &str, mode: Option<&str>) -> Note {
        Note { id: "n".into(), to: "master".into(), text: text.into(), at: String::new(), delivered: false, mode: mode.map(str::to_string) }
    }

    #[test]
    fn web_requests_are_told_apart() {
        assert!(looks_like_web("A portfolio site with a project grid"));
        assert!(looks_like_web("a todo app"));
        assert!(!looks_like_web("A CLI that renames photos"));
        assert!(!looks_like_web("a rust library for parsing dates"));
        assert!(!looks_like_web("sum the numbers in a file"), "no web word");
        assert!(!looks_like_web("happen to mention uint"), "`ui` inside a word does not count");
    }

    #[test]
    fn a_follow_up_goes_to_one_agent_unless_it_asks_for_much() {
        assert_eq!(turn_mode(&[note("the preview is blank, fix it", None)], RunMode::Auto), RunMode::Agent);
        assert_eq!(turn_mode(&[note("Add a new page for billing and a settings page", None)], RunMode::Auto), RunMode::Code);
        assert_eq!(turn_mode(&[note("fix it", None)], RunMode::Code), RunMode::Code);
        assert_eq!(turn_mode(&[note("redesign everything", None)], RunMode::Agent), RunMode::Agent);
        assert_eq!(turn_mode(&[note("a", Some("code")), note("b", Some("agent"))], RunMode::Auto), RunMode::Agent, "the last named mode wins");
        assert_eq!(turn_mode(&[note("small fix", Some("auto"))], RunMode::Code), RunMode::Code, "auto on the note defers to the run");
    }

    #[test]
    fn agent_plans_own_everything_and_pass_the_plan_checks() {
        let fresh = agent_plan("A landing page for a bakery", &[], 0);
        assert_eq!(fresh.kind, ProjectKind::Web);
        assert_eq!((fresh.pages.len(), fresh.pages[0].owned_glob.as_str(), fresh.pages[0].route.as_str()), (1, "**", "/"));
        assert!(fresh.pages[0].context.contains("live preview"));
        assert!(tm_core::check_plan(&fresh, &[], &Default::default()).is_ok());

        let existing = agent_plan("fix the dev script", &["keep it small".into()], 7);
        assert_eq!(existing.kind, ProjectKind::Migration);
        assert!(existing.pages[0].route.is_empty());
        assert!(existing.pages[0].brief.contains("keep it small"));
        assert!(tm_core::check_plan(&existing, &[], &Default::default()).is_ok());

        let first = agent_plan("A portfolio", &[], 0);
        let turn = turn_plan(2, &[note("preview does not start: no dev script", None)], Some("A portfolio"), Some(&first), Some("verify failed"));
        assert_eq!(turn.pages[0].id, "agent-2");
        assert_eq!(turn.pages[0].owned_glob, "**");
        assert_eq!(turn.kind, ProjectKind::Web);
        assert!(turn.pages[0].brief.contains("no dev script") && turn.pages[0].brief.contains("A portfolio") && turn.pages[0].brief.contains("verify failed"));
        assert!(tm_core::check_plan(&turn, &[], &Default::default()).is_ok());

        let (agent, fields) = turn_entry(2, RunMode::Agent, &[note("hello", None)]);
        assert_eq!(agent["id"], "person");
        assert_eq!((fields["turn"].as_u64(), fields["mode"].as_str(), fields["text"].as_str()), (Some(2), Some("agent"), Some("hello")));
    }
}
