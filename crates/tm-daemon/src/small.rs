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
pub fn small_brief(prompt: &str, entries: &[String], template: &str) -> String {
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
         Keep each brief to a few concise lines and each contract short. Leave `estimate` at zero.\n\
         {\"contracts\":[],\"pages\":[{\"id\":\"home\",\"route\":\"/\",\"owned_glob\":\"**\",\"cites\":[],\"brief\":\"...\"}],\
         \"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n\
         Rules: at least one page; page ids use letters, digits, `_` and `-`; owned_glob and contract paths are \
         relative with no `..`; every cited key is listed above or in `dictionary`; briefs are non-empty. Optionally give a page `units`: 2-4 work units, each {\"id\",\"owned_glob\",\"brief\"}, built by parallel workers. Split by file or concern (for example markup+styles, scripts, docs) only when the units' files are independent given the contracts; unit globs must be disjoint and inside the page glob (braces list files: `{app/index.html,app/styles.css}`); the page owned_glob must cover them all. Omit `units` when splitting is unsafe.\n",
    );
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
        let brief = small_brief("tiny todo", &[], "");
        assert!(brief.contains("Dictionary is empty"));
        assert!(brief.contains("Do not call any tools"));
        assert!(!brief.contains("Search the Dictionary"));
        assert!(brief.len() < 1700);
        let listed = small_brief("tiny todo", &["- `todo-dom` (commons, component_api): ids\nul#todo-list\n".into()], "");
        assert!(!listed.contains("Dictionary is empty"));
        assert!(listed.contains("`todo-dom`"));
        assert!(listed.contains("never copy them"));
    }
}
