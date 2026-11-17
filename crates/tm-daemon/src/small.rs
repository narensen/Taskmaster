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

/// The short planning brief. `dictionary_empty` tells the master up front that there is nothing to search.
pub fn small_brief(prompt: &str, dictionary_empty: bool, template: &str) -> String {
    let mut brief = format!("Plan this small product quickly. Be brief.\n\nRequest:\n{prompt}\n\n");
    if dictionary_empty {
        brief.push_str("The Dictionary is empty: do not search it or call any tools. Add no dictionary entries.\n");
    } else {
        brief.push_str("Add dictionary entries only if a page truly needs one.\n");
    }
    brief.push_str(template);
    brief.push_str(
        "\nReply with one JSON object and do not edit files. Use as few pages as possible (one if it fits). \
         `contracts` only for interfaces shared by more than one file or worker; otherwise `[]`. Keep every \
         contract and brief to a few concise lines. Leave `estimate` at zero.\n\
         {\"contracts\":[],\"pages\":[{\"id\":\"home\",\"route\":\"/\",\"owned_glob\":\"**\",\"cites\":[],\"brief\":\"...\"}],\
         \"estimate\":{\"tokens\":0,\"usd\":0},\"dictionary\":[]}\n\
         Rules: at least one page; page ids use letters, digits, `_` and `-`; owned_glob and contract paths are \
         relative with no `..`; briefs are non-empty.\n",
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
    fn small_brief_is_short_and_skips_search_when_empty() {
        let brief = small_brief("tiny todo", true, "");
        assert!(brief.contains("Dictionary is empty"));
        assert!(!brief.contains("Search the Dictionary"));
        assert!(brief.len() < 1500);
        assert!(!small_brief("tiny todo", false, "").contains("Dictionary is empty"));
    }
}
