//! What each agent is told. A reviewer knows nothing about the project, so the brief
//! names the context files to read before anything else.

use crate::CONTEXT_DIR;
use crate::context::Scope;
use crate::findings::Finding;

fn list(files: &[String]) -> String {
    if files.is_empty() {
        return "- none\n".into();
    }
    files.iter().map(|f| format!("- `{f}`\n")).collect()
}

const REPLY_FORMAT: &str = "Reply with only a JSON array. Each item is \
    {\"file\": path, \"line\": number in the file as it is now, \"kind\": \"bug\" | \"rule\" | \"security\", \
    \"failure\": what input or state goes wrong and what happens}. Report nothing you cannot point to \
    a line for. An empty array `[]` means you found nothing.";

pub fn review(scope: &Scope, diff_file: &str) -> String {
    format!(
        "You are reviewing a code change. This directory is a checkout of the code after the change.\n\n\
         Read these first, in this order:\n\
         - `{CONTEXT_DIR}/ARCHITECTURE.md`: how the code fits together\n\
         - `{CONTEXT_DIR}/CHANGES.md`: everything that changed\n\
         - `{CONTEXT_DIR}/RULES.md`: this project's review rules\n\
         - `{CONTEXT_DIR}/HISTORY.md`: what earlier reviews found in these files\n\
         - `{CONTEXT_DIR}/{diff_file}`: the diff for your part of the change\n\n\
         Your part of the change:\n{files}\n\
         Unchanged files that import them, and so may be broken by the change:\n{dependents}\n\
         Read the changed files in full, not only the diff, and read each dependent where it uses \
         what changed.\n\n\
         Look for, in this order: a change that breaks a file depending on it; a change that \
         contradicts the architecture; a broken review rule; a logic error in the changed lines; \
         an obvious performance problem. Ignore style and naming unless a rule covers them.\n\n\
         {REPLY_FORMAT}",
        files = list(&scope.files),
        dependents = list(&scope.dependents),
    )
}

pub fn security(files: &[String]) -> String {
    format!(
        "You are reviewing a code change for security problems only. This directory is a checkout \
         of the code after the change.\n\n\
         Read `{CONTEXT_DIR}/ARCHITECTURE.md`, `{CONTEXT_DIR}/CHANGES.md`, `{CONTEXT_DIR}/RULES.md` and \
         `{CONTEXT_DIR}/DIFF.patch` first.\n\n\
         Changed files:\n{files}\n\
         Look for: input reaching HTML, a query, a shell, a file path or a redirect unchecked; data \
         read or changed without an access check; secrets or tokens in code or logs; an id from the \
         request trusted without checking who is asking. Ignore everything that is not a security \
         problem introduced or exposed by this change.\n\n\
         For `failure`, state what an attacker sends and what they gain. {REPLY_FORMAT}",
        files = list(files),
    )
}

pub fn verify(finding: &Finding) -> String {
    format!(
        "A reviewer reported a problem in this code. Decide whether it is real.\n\n\
         Claim: in `{file}` at line {line}: {failure}\n\n\
         Read that file and whatever it depends on. Try to disprove the claim: check that the line \
         says what the claim says, whether the code already handles the case, and whether the \
         failure can actually happen. `{CONTEXT_DIR}/RULES.md` holds the project's rules, if the \
         claim is about one.\n\n\
         Reply with only `{{\"real\": true}}` or `{{\"real\": false, \"why\": \"one sentence\"}}`. If you \
         cannot tell, answer false.",
        file = finding.file,
        line = finding.line,
        failure = finding.failure,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Kind;

    #[test]
    fn the_review_brief_names_every_context_file_and_the_scope() {
        let scope = Scope {
            name: "lib".into(),
            files: vec!["lib/money.ts".into()],
            dependents: vec!["web/cart.tsx".into()],
        };
        let brief = review(&scope, "diff-1.patch");
        for file in ["ARCHITECTURE.md", "CHANGES.md", "RULES.md", "HISTORY.md", "diff-1.patch"] {
            assert!(brief.contains(&format!("{CONTEXT_DIR}/{file}")), "{file}");
        }
        assert!(brief.contains("- `lib/money.ts`") && brief.contains("- `web/cart.tsx`"));
        assert!(brief.contains("JSON array"));
    }

    #[test]
    fn the_verify_brief_states_the_claim_and_defaults_to_false() {
        let finding = Finding { kind: Kind::Bug, file: "a.ts".into(), line: 9, failure: "divides by zero when n is 0".into() };
        let brief = verify(&finding);
        assert!(brief.contains("`a.ts` at line 9: divides by zero when n is 0"));
        assert!(brief.contains("If you cannot tell, answer false"));
        assert!(security(&[]).contains("- none"));
    }
}
