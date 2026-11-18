//! The mid-tier checker: a second look at a page right after its worker reports done.
//!
//! Off by default (`checker.enabled` in the settings file). When it is on, a page that
//! passed its own checks and is worth a look (it changes UI files, or the change is
//! large, or it took a stronger tier to build) gets one read-only call on the mid model
//! with the cheap inputs only: the diff, a screenshot when the worker had its page open,
//! the page's brief and done line, and the worker's last words. It answers `pass` or up to
//! five fixes.
//!
//! The call runs beside the rest of the build. Its fixes join the errors of the verify
//! step, so a page is repaired the same way a failing type check repairs it. Verification
//! waits at most [`GRACE`] for a check still running; one that is later is dropped. Each
//! page is checked once, so the checker can add at most one repair per page.

use std::time::Duration;

/// How long verification waits for checks still running before it goes on without them.
pub const GRACE: Duration = Duration::from_secs(20);
/// The most fixes one check may ask for.
pub const MAX_FIXES: usize = 5;
const UI: [&str; 8] = [".tsx", ".jsx", ".html", ".css", ".scss", ".vue", ".svelte", ".astro"];

/// Why a page is worth a check, or `None` to skip it.
pub fn worth_checking(files: &[String], diff_lines: usize, escalated: bool, units: usize) -> Option<&'static str> {
    if files.iter().any(|f| UI.iter().any(|ext| f.ends_with(ext))) {
        return Some("it changes UI files");
    }
    if escalated {
        return Some("a stronger tier built it");
    }
    if units > 1 {
        return Some("several workers built it");
    }
    if diff_lines > 200 || files.len() > 4 {
        return Some("the change is large");
    }
    None
}

pub struct Inputs<'a> {
    pub page: &'a str,
    pub route: &'a str,
    pub brief: &'a str,
    pub done: &'a str,
    /// The end of what the worker said when it finished.
    pub said: &'a str,
    pub screenshot: bool,
}

/// The checker's whole task. It reads `diff.patch` (and `screen.jpg`) in its directory.
pub fn brief(inputs: &Inputs<'_>) -> String {
    let look = if inputs.screenshot {
        " and look at screen.jpg (the page in a browser, as the builder left it)"
    } else {
        ""
    };
    let done = if inputs.done.trim().is_empty() { "every file it owns is written and does its part" } else { inputs.done.trim() };
    format!(
        "Check one page another agent has just built. Read diff.patch (the whole change){look}; read nothing else.\n\n\
         Page `{page}`, route `{route}`.\nIts brief: {brief}\nDone when: {done}\nThe builder's last words: {said}\n\n\
         Reply with the single word `pass` when the change does what the brief asks and nothing a user would hit is broken. \
         Otherwise reply with at most {MAX_FIXES} lines, each `- <file>: <what is wrong, and the fix>`. \
         Name only real defects: missing behaviour, broken markup, a wrong import, an unreadable layout. No style notes, no praise.",
        page = inputs.page,
        route = inputs.route,
        brief = inputs.brief.trim(),
        said = inputs.said.trim(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fixes(Vec<String>),
}

/// Read a checker's reply. Anything that is neither `pass` nor a fix list passes: a
/// confused checker must not cost a repair.
pub fn verdict(reply: &str) -> Verdict {
    let fixes: Vec<String> = reply
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")))
        .map(|fix| fix.trim().to_string())
        .filter(|fix| fix.len() > 3)
        .take(MAX_FIXES)
        .collect();
    let first = reply.trim().trim_matches(['`', '*', '.']).to_ascii_lowercase();
    if fixes.is_empty() || first == "pass" {
        Verdict::Pass
    } else {
        Verdict::Fixes(fixes)
    }
}

/// The text a repair gets for these fixes.
pub fn as_errors(fixes: &[String]) -> String {
    let mut out = String::from("A second model checked this page against its brief and asks for these fixes:\n");
    for fix in fixes {
        out.push_str(&format!("- {fix}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_large_escalated_and_split_pages_are_checked_and_small_logic_is_not() {
        let files = |list: &[&str]| list.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        assert_eq!(worth_checking(&files(&["app/cart/page.tsx"]), 20, false, 1), Some("it changes UI files"));
        assert_eq!(worth_checking(&files(&["src/total.ts"]), 20, true, 1), Some("a stronger tier built it"));
        assert_eq!(worth_checking(&files(&["src/a.ts"]), 20, false, 2), Some("several workers built it"));
        assert_eq!(worth_checking(&files(&["src/a.ts"]), 450, false, 1), Some("the change is large"));
        assert_eq!(worth_checking(&files(&["src/total.ts", "src/tax.ts"]), 60, false, 1), None);
    }

    #[test]
    fn a_reply_is_pass_or_at_most_five_fixes() {
        assert_eq!(verdict("pass"), Verdict::Pass);
        assert_eq!(verdict("`Pass`."), Verdict::Pass);
        assert_eq!(verdict("Looks fine to me."), Verdict::Pass);
        let many = (1..=8).map(|n| format!("- app/a.tsx: problem {n}")).collect::<Vec<_>>().join("\n");
        match verdict(&format!("Two things.\n{many}")) {
            Verdict::Fixes(fixes) => {
                assert_eq!(fixes.len(), MAX_FIXES);
                assert_eq!(fixes[0], "app/a.tsx: problem 1");
            }
            other => panic!("{other:?}"),
        }
        assert!(as_errors(&["app/a.tsx: the button does nothing".into()]).contains("- app/a.tsx: the button does nothing"));
    }

    #[test]
    fn the_brief_names_its_inputs_and_asks_for_little() {
        let inputs = Inputs { page: "cart", route: "/cart", brief: "A cart.", done: "", said: "Done.", screenshot: true };
        let text = brief(&inputs);
        assert!(text.contains("diff.patch") && text.contains("screen.jpg") && text.contains("`pass`"));
        assert!(text.contains("at most 5 lines"));
        assert!(!brief(&Inputs { screenshot: false, ..inputs }).contains("screen.jpg"));
    }
}
