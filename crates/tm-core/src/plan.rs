//! Checks a planner's draft before any Dictionary entry is published.
//!
//! A draft is a [`Plan`] plus the new Dictionary entries its pages cite. An entry that already
//! exists may be cited, and it may be repeated only when the content is the same. A different
//! body is rejected so planning does not publish a successor by accident.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::types::{valid_key, ContractFile, DictKey, EntryKind, PagePlan, Plan};

/// One Dictionary entry the planner wants published. The hash is computed when it is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftEntry {
    pub key: DictKey,
    pub kind: EntryKind,
    pub summary: String,
    pub content: String,
    /// Stack the entry was written for, for example `next@15`. Empty when the planner omitted it.
    #[serde(default)]
    pub stack: String,
}

/// `Ok` when the draft can be locked. The strings are the reasons it cannot.
pub fn check_plan(plan: &Plan, drafts: &[DraftEntry], existing: &BTreeMap<DictKey, String>) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    if plan.pages.is_empty() {
        errors.push("the plan has no pages".into());
    }

    let mut seen_ids = BTreeSet::new();
    for page in &plan.pages {
        check_page(page, drafts, existing, &mut seen_ids, &mut errors);
    }

    let mut seen_paths = BTreeSet::new();
    for contract in &plan.contracts {
        check_contract(contract, &mut seen_paths, &mut errors);
    }

    let mut seen_keys = BTreeSet::new();
    for draft in drafts {
        check_draft(draft, existing, &mut seen_keys, &mut errors);
    }

    if plan.estimate.usd < 0.0 || !plan.estimate.usd.is_finite() {
        errors.push("the estimate is negative".into());
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn check_page(
    page: &PagePlan,
    drafts: &[DraftEntry],
    existing: &BTreeMap<DictKey, String>,
    seen_ids: &mut BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    if !valid_key(&page.id) {
        errors.push(format!("page id `{}` must use letters, digits, `_` and `-`", page.id));
    } else if !seen_ids.insert(page.id.clone()) {
        errors.push(format!("page id `{}` is duplicated", page.id));
    }
    if page.route.trim().is_empty() {
        errors.push(format!("page `{}` has an empty route", page.id));
    }
    if page.brief.trim().is_empty() {
        errors.push(format!("page `{}` has an empty brief", page.id));
    }
    if page.owned_glob.trim().is_empty() {
        errors.push(format!("page `{}` has an empty owned_glob", page.id));
    } else if !safe_rel(&page.owned_glob) {
        errors.push(format!("page `{}` owned_glob `{}` leaves the project", page.id, page.owned_glob));
    }

    let mut seen_cites = BTreeSet::new();
    for cite in &page.cites {
        if !valid_key(cite) {
            errors.push(format!("page `{}` cites `{cite}`, which is not a dictionary key", page.id));
            continue;
        }
        if !seen_cites.insert(cite) {
            errors.push(format!("page `{}` cites `{cite}` twice", page.id));
            continue;
        }
        let defined = drafts.iter().any(|draft| draft.key == *cite) || existing.contains_key(cite);
        if !defined {
            errors.push(format!("page `{}` cites `{cite}`, which has no dictionary entry", page.id));
        }
    }
}

fn check_contract(contract: &ContractFile, seen_paths: &mut BTreeSet<String>, errors: &mut Vec<String>) {
    if contract.path.trim().is_empty() {
        errors.push("a contract path is empty".into());
    } else if !safe_rel(&contract.path) {
        errors.push(format!("contract path `{}` leaves the project", contract.path));
    } else if !seen_paths.insert(contract.path.clone()) {
        errors.push(format!("contract path `{}` is duplicated", contract.path));
    }
    if contract.content.trim().is_empty() {
        errors.push(format!("contract `{}` has empty content", contract.path));
    }
}

fn check_draft(
    draft: &DraftEntry,
    existing: &BTreeMap<DictKey, String>,
    seen_keys: &mut BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    if !valid_key(&draft.key) {
        errors.push(format!("dictionary key `{}` must use letters, digits, `_` and `-`", draft.key));
    } else if !seen_keys.insert(draft.key.clone()) {
        errors.push(format!("dictionary key `{}` is duplicated", draft.key));
    }
    if draft.summary.trim().is_empty() {
        errors.push(format!("dictionary `{}` has an empty summary", draft.key));
    }
    if draft.content.trim().is_empty() {
        errors.push(format!("dictionary `{}` has empty content", draft.key));
    } else if existing.get(&draft.key).is_some_and(|have| have != &draft.content) {
        errors.push(format!(
            "dictionary `{}` already exists with different content; cite it or match it exactly",
            draft.key
        ));
    }
}

/// A relative path that cannot climb out of the project. Globs such as `app/home/**` pass.
fn safe_rel(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute() && !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContractFile, EntryKind, Estimate, PagePlan};

    fn draft(key: &str, content: &str) -> DraftEntry {
        DraftEntry {
            key: key.into(),
            kind: EntryKind::Hint,
            summary: key.into(),
            content: content.into(),
            stack: "next".into(),
        }
    }

    fn plan(cites: &[&str]) -> Plan {
        Plan {
            contracts: vec![ContractFile { path: "contracts/types.ts".into(), content: "export type Id = string;\n".into() }],
            pages: vec![PagePlan {
                id: "home".into(),
                route: "/home".into(),
                owned_glob: "app/home/**".into(),
                cites: cites.iter().map(|cite| (*cite).to_string()).collect(),
                brief: "Implement home.".into(),
                units: vec![],
            }],
            estimate: Estimate { tokens: 1, usd: 0.0 },
        }
    }

    #[test]
    fn accepts_a_new_entry_and_an_unchanged_existing_one() {
        let existing = BTreeMap::from([("theme".into(), "export const theme = \"dark\";\n".into())]);
        check_plan(&plan(&["greet"]), &[draft("greet", "export const greet = 1;\n")], &existing).unwrap();
        check_plan(&plan(&["theme"]), &[], &existing).unwrap();
        check_plan(
            &plan(&["theme"]),
            &[draft("theme", "export const theme = \"dark\";\n")],
            &existing,
        )
        .unwrap();
    }

    #[test]
    fn rejects_a_draft_that_cannot_be_locked() {
        let existing = BTreeMap::from([("theme".into(), "export const theme = \"dark\";\n".into())]);
        let mut broken = plan(&["missing"]);
        broken.pages.push(PagePlan {
            id: "home".into(),
            route: "".into(),
            owned_glob: "../secret/**".into(),
            cites: vec!["../x".into(), "theme".into(), "theme".into()],
            brief: "  ".into(),
            units: vec![],
        });
        broken.contracts.push(ContractFile { path: "contracts/types.ts".into(), content: " \n".into() });
        broken.estimate.usd = -1.0;
        let errors = check_plan(
            &broken,
            &[draft("theme", "export const theme = \"light\";\n"), draft("ok", "   ")],
            &existing,
        )
        .unwrap_err()
        .join("\n");
        assert!(errors.contains("page id `home` is duplicated"), "{errors}");
        assert!(errors.contains("empty route"), "{errors}");
        assert!(errors.contains("leaves the project"), "{errors}");
        assert!(errors.contains("not a dictionary key"), "{errors}");
        assert!(errors.contains("cites `theme` twice"), "{errors}");
        assert!(errors.contains("no dictionary entry"), "{errors}");
        assert!(errors.contains("empty brief"), "{errors}");
        assert!(errors.contains("is duplicated"), "{errors}");
        assert!(errors.contains("empty content"), "{errors}");
        assert!(errors.contains("different content"), "{errors}");
        assert!(errors.contains("negative"), "{errors}");

        let empty = Plan { contracts: vec![], pages: vec![], estimate: Estimate { tokens: 0, usd: 0.0 } };
        assert!(check_plan(&empty, &[], &BTreeMap::new()).unwrap_err().join(" ").contains("no pages"));
    }
}
