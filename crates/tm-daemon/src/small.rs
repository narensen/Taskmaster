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

/// The short planning brief. `entries` lists stored Dictionary entries, one per item, already
/// formatted; they are given in full here, so the master needs no tools at all.
/// How a plan may split a page into work units, given the run's `slots` parallel workers.
/// More units than slots would queue behind each other, so the cap is the slot count.
pub fn units_rule(slots: usize) -> String {
    let most = slots.min(4);
    if most < 2 {
        return "Do not give pages `units`: this run builds with one worker at a time.\n".into();
    }
    let range = if most == 2 { "2".to_string() } else { format!("2-{most}") };
    format!(
        "Optionally give a page `units`: {range} work units (never more than {most}, the parallel workers this run has), \
         each {{\"id\",\"owned_glob\",\"brief\",\"done\"}}, one agent per unit, built in parallel. Split by file or concern (for example markup+styles, \
         scripts, docs) only when the units' files are independent given the contracts; unit globs must be disjoint and \
         inside the page glob (braces list files: `{{app/index.html,app/styles.css}}`); the page owned_glob must cover them \
         all. Omit `units` when splitting is unsafe.\n"
    )
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
        "\nReply with one JSON object only and do not edit files. Use as few pages as possible (one if it fits). \
         `contracts` are shared source files, only for interfaces used by more than one file or worker; otherwise `[]`. \
         Each page or unit is one agent: `brief` is its whole task in a few lines, `done` one line saying when it is finished. \
         Keep contracts short. Leave `estimate` at zero.\n\
         {\"contracts\":[],\"pages\":[{\"id\":\"home\",\"route\":\"/\",\"owned_glob\":\"**\",\"cites\":[],\"brief\":\"...\",\"done\":\"...\"}],\
         \"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n\
         Rules: at least one page; ids use letters, digits, `_` and `-`; globs and contract paths are relative, no `..`; \
         cited keys are listed above or in `dictionary`; briefs are non-empty.\n",
    );
    brief.push_str(&units_rule(slots));
    if slots >= 2 {
        brief.push_str(
            "Parallel units finish sooner: when a page has separate markup and styles, scripts, and docs, split it into units \
             and pin every id, file name and interface they share in a contract, a cited entry or each unit's brief.\n",
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
        assert!(brief.len() < 1700, "{}", brief.len());
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
        assert!(units_rule(1).contains("Do not give pages `units`"));
        assert!(!small_brief("tiny todo", &[], "", 1).contains("Parallel units finish sooner"));
    }
}
