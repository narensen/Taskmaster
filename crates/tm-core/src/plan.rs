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

/// The most lines one contract file may have. A contract is an interface; a body belongs to
/// the unit that owns the file.
pub const CONTRACT_LINES: usize = 120;
/// The most contract lines a whole plan may lock.
pub const PLAN_CONTRACT_LINES: usize = 400;

/// Everything a plan is checked against. The planner is shown [`rules_text`], built from this
/// same list, so what it is told and what is checked cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    AtLeastOneUnit,
    UnitIds,
    UnitBrief,
    OwnedGlob,
    Routes,
    Cites,
    ContractPaths,
    ContractContent,
    ContractSize,
    ContractNotOwned,
    DictionaryKeys,
    DictionaryContent,
    Estimate,
}

impl Rule {
    pub const ALL: [Rule; 13] = [
        Rule::AtLeastOneUnit,
        Rule::UnitIds,
        Rule::UnitBrief,
        Rule::OwnedGlob,
        Rule::Routes,
        Rule::Cites,
        Rule::ContractPaths,
        Rule::ContractContent,
        Rule::ContractSize,
        Rule::ContractNotOwned,
        Rule::DictionaryKeys,
        Rule::DictionaryContent,
        Rule::Estimate,
    ];

    /// The rule as the planner reads it.
    pub fn text(self) -> &'static str {
        match self {
            Rule::AtLeastOneUnit => "the plan has at least one unit",
            Rule::UnitIds => "unit ids are unique and use only letters, digits, `_` and `-`",
            Rule::UnitBrief => "every unit has a non-empty `brief`",
            Rule::OwnedGlob => "every unit has an `owned_glob`: relative, without `..`",
            Rule::Routes => "a unit shown in a browser (every unit of a `web` plan, or one with `\"ui\": true`) has a `route`; any other unit has no `route`, and only a plan with such a unit has a `design`",
            Rule::Cites => "every key in `cites` is cited once and is a stored Dictionary entry or one of the plan's `dictionary` items",
            Rule::ContractPaths => "contract paths are unique, relative and without `..`",
            Rule::ContractContent => "every contract has content",
            Rule::ContractSize => "a contract is an interface, never an implementation: signatures, types, constants, error strings, wire formats and file schemas, with every body left as `...`, `pass` or `raise NotImplementedError` (or the language's own stub); at most 120 lines per contract and 400 across the plan",
            Rule::ContractNotOwned => "no contract path matches any unit's `owned_glob`: a file a unit must write is never locked as a contract",
            Rule::DictionaryKeys => "dictionary keys are unique and use only letters, digits, `_` and `-`",
            Rule::DictionaryContent => "every dictionary item has a summary and content, and a stored key repeats its stored content exactly",
            Rule::Estimate => "`estimate` stays at zero",
        }
    }
}

/// The rules, one line each, for a planning brief.
pub fn rules_text() -> String {
    let mut out = String::from("Rules, each checked before the plan is accepted:\n");
    for rule in Rule::ALL {
        out.push_str("- ");
        out.push_str(rule.text());
        out.push('\n');
    }
    out
}

/// One reason a plan cannot be locked, and the rule it breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub rule: Rule,
    pub detail: String,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// `Ok` when the draft can be locked. The strings are the reasons it cannot.
pub fn check_plan(plan: &Plan, drafts: &[DraftEntry], existing: &BTreeMap<DictKey, String>) -> Result<(), Vec<String>> {
    let found = rejections(plan, drafts, existing);
    if found.is_empty() { Ok(()) } else { Err(found.into_iter().map(|r| r.detail).collect()) }
}

/// Every reason the draft cannot be locked, each with its rule.
pub fn rejections(plan: &Plan, drafts: &[DraftEntry], existing: &BTreeMap<DictKey, String>) -> Vec<Rejection> {
    let mut errors = Vec::new();
    if plan.pages.is_empty() {
        reject(&mut errors, Rule::AtLeastOneUnit, "the plan has no units".into());
    }

    let mut seen_ids = BTreeSet::new();
    for page in &plan.pages {
        check_page(plan, page, drafts, existing, &mut seen_ids, &mut errors);
    }
    if !plan.design.trim().is_empty() && !plan.has_ui() {
        reject(&mut errors, Rule::Routes, format!("a `{}` plan with no unit shown in a browser has a design", plan.kind.id()));
    }

    let mut seen_paths = BTreeSet::new();
    let mut total = 0;
    let globs: Vec<(String, globset::GlobMatcher)> = plan
        .pages
        .iter()
        .flat_map(|p| std::iter::once((p.id.clone(), p.owned_glob.clone())).chain(p.units.iter().map(|u| (format!("{}/{}", p.id, u.id), u.owned_glob.clone()))))
        .filter_map(|(id, glob)| globset::Glob::new(glob.trim()).ok().map(|g| (id, g.compile_matcher())))
        .collect();
    for contract in &plan.contracts {
        check_contract(contract, &mut seen_paths, &mut errors);
        total += contract.content.lines().count();
        if let Some((id, _)) = globs.iter().find(|(_, glob)| glob.is_match(contract.path.trim())) {
            reject(
                &mut errors,
                Rule::ContractNotOwned,
                format!("contract `{}` is inside unit `{id}`'s owned_glob, so that unit could never change it; move the interface to a shared path or drop the contract", contract.path),
            );
        }
    }
    if total > PLAN_CONTRACT_LINES {
        reject(&mut errors, Rule::ContractSize, format!("the contracts have {total} lines in all, more than {PLAN_CONTRACT_LINES}: keep them to interfaces"));
    }

    let mut seen_keys = BTreeSet::new();
    for draft in drafts {
        check_draft(draft, existing, &mut seen_keys, &mut errors);
    }

    if plan.estimate.usd < 0.0 || !plan.estimate.usd.is_finite() {
        reject(&mut errors, Rule::Estimate, "the estimate is negative".into());
    }
    errors
}

fn reject(errors: &mut Vec<Rejection>, rule: Rule, detail: String) {
    errors.push(Rejection { rule, detail });
}

fn check_page(
    plan: &Plan,
    page: &PagePlan,
    drafts: &[DraftEntry],
    existing: &BTreeMap<DictKey, String>,
    seen_ids: &mut BTreeSet<String>,
    errors: &mut Vec<Rejection>,
) {
    if !valid_key(&page.id) {
        reject(errors, Rule::UnitIds, format!("unit id `{}` must use letters, digits, `_` and `-`", page.id));
    } else if !seen_ids.insert(page.id.clone()) {
        reject(errors, Rule::UnitIds, format!("unit id `{}` is duplicated", page.id));
    }
    let ui = plan.is_ui(page);
    if ui && page.route.trim().is_empty() {
        reject(errors, Rule::Routes, format!("unit `{}` is shown in a browser and has an empty route", page.id));
    } else if !ui && !page.route.trim().is_empty() {
        reject(errors, Rule::Routes, format!("unit `{}` is not shown in a browser and has route `{}`", page.id, page.route));
    }
    if page.brief.trim().is_empty() {
        reject(errors, Rule::UnitBrief, format!("unit `{}` has an empty brief", page.id));
    }
    if page.owned_glob.trim().is_empty() {
        reject(errors, Rule::OwnedGlob, format!("unit `{}` has an empty owned_glob", page.id));
    } else if !safe_rel(&page.owned_glob) {
        reject(errors, Rule::OwnedGlob, format!("unit `{}` owned_glob `{}` leaves the project", page.id, page.owned_glob));
    }

    let mut seen_cites = BTreeSet::new();
    for cite in &page.cites {
        if !valid_key(cite) {
            reject(errors, Rule::Cites, format!("unit `{}` cites `{cite}`, which is not a dictionary key", page.id));
            continue;
        }
        if !seen_cites.insert(cite) {
            reject(errors, Rule::Cites, format!("unit `{}` cites `{cite}` twice", page.id));
            continue;
        }
        let defined = drafts.iter().any(|draft| draft.key == *cite) || existing.contains_key(cite);
        if !defined {
            reject(errors, Rule::Cites, format!("unit `{}` cites `{cite}`, which has no dictionary entry", page.id));
        }
    }
}

fn check_contract(contract: &ContractFile, seen_paths: &mut BTreeSet<String>, errors: &mut Vec<Rejection>) {
    if contract.path.trim().is_empty() {
        reject(errors, Rule::ContractPaths, "a contract path is empty".into());
    } else if !safe_rel(&contract.path) {
        reject(errors, Rule::ContractPaths, format!("contract path `{}` leaves the project", contract.path));
    } else if !seen_paths.insert(contract.path.clone()) {
        reject(errors, Rule::ContractPaths, format!("contract path `{}` is duplicated", contract.path));
    }
    if contract.content.trim().is_empty() {
        reject(errors, Rule::ContractContent, format!("contract `{}` has empty content", contract.path));
    }
    let lines = contract.content.lines().count();
    if lines > CONTRACT_LINES {
        reject(
            errors,
            Rule::ContractSize,
            format!("contract `{}` has {lines} lines, more than {CONTRACT_LINES}: keep signatures and types, and leave bodies to the unit that writes them", contract.path),
        );
    }
}

fn check_draft(
    draft: &DraftEntry,
    existing: &BTreeMap<DictKey, String>,
    seen_keys: &mut BTreeSet<String>,
    errors: &mut Vec<Rejection>,
) {
    if !valid_key(&draft.key) {
        reject(errors, Rule::DictionaryKeys, format!("dictionary key `{}` must use letters, digits, `_` and `-`", draft.key));
    } else if !seen_keys.insert(draft.key.clone()) {
        reject(errors, Rule::DictionaryKeys, format!("dictionary key `{}` is duplicated", draft.key));
    }
    if draft.summary.trim().is_empty() {
        reject(errors, Rule::DictionaryContent, format!("dictionary `{}` has an empty summary", draft.key));
    }
    if draft.content.trim().is_empty() {
        reject(errors, Rule::DictionaryContent, format!("dictionary `{}` has empty content", draft.key));
    } else if existing.get(&draft.key).is_some_and(|have| have != &draft.content) {
        reject(
            errors,
            Rule::DictionaryContent,
            format!("dictionary `{}` already exists with different content; cite it or match it exactly", draft.key),
        );
    }
}

/// Fix what can be fixed in place, so a long planning call is not thrown away for a field
/// that has one sensible value. Returns one line per change. What is left is for
/// [`check_plan`] to reject.
pub fn normalize_plan(plan: &mut Plan, drafts: &mut Vec<DraftEntry>) -> Vec<String> {
    let mut notes = Vec::new();
    let tidy = |path: &str| -> String {
        let mut p = path.trim();
        while let Some(rest) = p.strip_prefix("./") {
            p = rest;
        }
        p.trim_start_matches('/').to_string()
    };
    let mut ids: BTreeSet<String> = plan.pages.iter().map(|p| p.id.clone()).collect();
    for i in 0..plan.pages.len() {
        let ui = plan.is_ui(&plan.pages[i]);
        let page = &mut plan.pages[i];
        if !valid_key(&page.id) {
            let fixed: String = page.id.trim().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
            let fixed = fixed.trim_matches('-').to_string();
            if valid_key(&fixed) && !ids.contains(&fixed) {
                notes.push(format!("renamed unit `{}` to `{fixed}`", page.id));
                ids.insert(fixed.clone());
                page.id = fixed;
            }
        }
        let glob = tidy(&page.owned_glob);
        if glob != page.owned_glob && !glob.is_empty() {
            notes.push(format!("unit `{}`: owned_glob `{}` read as `{glob}`", page.id, page.owned_glob));
            page.owned_glob = glob;
        }
        for unit in &mut page.units {
            let glob = tidy(&unit.owned_glob);
            if glob != unit.owned_glob && !glob.is_empty() {
                unit.owned_glob = glob;
            }
        }
        if !ui && !page.route.trim().is_empty() {
            notes.push(format!("dropped route `{}` from unit `{}`: it is not shown in a browser", page.route, page.id));
            page.route.clear();
        } else if ui && page.route.trim().is_empty() {
            notes.push(format!("unit `{}` is shown in a browser and had no route, so it opens at `/`", page.id));
            page.route = "/".into();
        }
        let before = page.cites.len();
        let mut seen = BTreeSet::new();
        page.cites.retain(|c| seen.insert(c.clone()));
        if page.cites.len() != before {
            notes.push(format!("unit `{}` cited a key twice; kept one", page.id));
        }
    }
    if !plan.design.trim().is_empty() && !plan.has_ui() {
        notes.push(format!("dropped the design: nothing in this `{}` plan is shown in a browser", plan.kind.id()));
        plan.design.clear();
    }
    for contract in &mut plan.contracts {
        let path = tidy(&contract.path);
        if path != contract.path && !path.is_empty() {
            contract.path = path;
        }
    }
    // One unit owns everything: a contract inside its files has nobody to share with, so it
    // becomes the starting interface in that unit's brief instead of a locked file.
    if plan.pages.len() == 1 && plan.pages[0].units.len() < 2 && !plan.contracts.is_empty() {
        let glob = globset::Glob::new(plan.pages[0].owned_glob.trim()).ok().map(|g| g.compile_matcher());
        if let Some(glob) = glob {
            let (inside, keep): (Vec<ContractFile>, Vec<ContractFile>) = std::mem::take(&mut plan.contracts).into_iter().partition(|c| glob.is_match(c.path.trim()));
            plan.contracts = keep;
            let page = &mut plan.pages[0];
            for contract in inside {
                notes.push(format!("contract `{}` is a file unit `{}` writes, so it is in that unit's brief instead of locked", contract.path, page.id));
                page.brief.push_str(&format!(
                    "\n\nWrite `{}` to this interface (fill in the bodies):\n```\n{}\n```",
                    contract.path,
                    contract.content.trim_end()
                ));
            }
        }
    }
    if plan.estimate.usd < 0.0 || !plan.estimate.usd.is_finite() {
        plan.estimate.usd = 0.0;
    }
    let mut kept: Vec<DraftEntry> = Vec::new();
    for draft in std::mem::take(drafts) {
        if kept.iter().any(|k| k.key == draft.key && k.content == draft.content) {
            notes.push(format!("dictionary `{}` was listed twice; kept one", draft.key));
            continue;
        }
        kept.push(draft);
    }
    *drafts = kept;
    notes
}

/// A relative path that cannot climb out of the project. Globs such as `app/home/**` pass.
fn safe_rel(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute() && !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContractFile, EntryKind, Estimate, PagePlan, ProjectKind};

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
            kind: Default::default(),
            contracts: vec![ContractFile { path: "contracts/types.ts".into(), content: "export type Id = string;\n".into() }],
            pages: vec![PagePlan {
                id: "home".into(),
                route: "/home".into(),
                owned_glob: "app/home/**".into(),
                cites: cites.iter().map(|cite| (*cite).to_string()).collect(),
                brief: "Implement home.".into(),
                ..Default::default()
            }],
            estimate: Estimate { tokens: 1, usd: 0.0, ..Default::default() },
            ..Default::default()
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
            ..Default::default()
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
        assert!(errors.contains("unit id `home` is duplicated"), "{errors}");
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

        let empty = Plan::default();
        assert!(check_plan(&empty, &[], &BTreeMap::new()).unwrap_err().join(" ").contains("no units"));
    }

    fn module(id: &str, glob: &str) -> PagePlan {
        PagePlan { id: id.into(), owned_glob: glob.into(), brief: format!("Write {id}."), ..Default::default() }
    }

    #[test]
    fn a_library_plan_needs_no_routes_and_no_design() {
        let plan = Plan {
            kind: ProjectKind::Library,
            contracts: vec![ContractFile { path: "tinydb/api.py".into(), content: "def insert(doc: dict) -> int: ...\n".into() }],
            pages: vec![module("engine", "tinydb/engine.py"), module("readme", "README.md")],
            ..Default::default()
        };
        check_plan(&plan, &[], &BTreeMap::new()).unwrap();
        assert!(!plan.has_ui());
        assert!(plan.ui_routes().is_empty());
    }

    #[test]
    fn a_bad_field_is_fixed_in_place_instead_of_rejecting_the_plan() {
        let mut plan = Plan {
            kind: ProjectKind::Server,
            pages: vec![
                PagePlan { route: "tcp://127.0.0.1:6380".into(), cites: vec!["k".into(), "k".into()], ..module("toy redis", "./redis/**") },
                PagePlan { route: "".into(), ui: Some(true), ..module("console", "web/**") },
            ],
            design: String::new(),
            phases: vec![],
            estimate: Estimate { tokens: 0, usd: -3.0, ..Default::default() },
            ..Default::default()
        };
        let mut drafts = vec![
            DraftEntry { key: "k".into(), kind: EntryKind::Hint, summary: "k".into(), content: "x".into(), stack: String::new() },
            DraftEntry { key: "k".into(), kind: EntryKind::Hint, summary: "k".into(), content: "x".into(), stack: String::new() },
        ];
        let notes = normalize_plan(&mut plan, &mut drafts).join("\n");
        assert_eq!(plan.pages[0].id, "toy-redis", "{notes}");
        assert_eq!(plan.pages[0].owned_glob, "redis/**");
        assert!(plan.pages[0].route.is_empty(), "{notes}");
        assert_eq!(plan.pages[0].cites, ["k"]);
        assert_eq!(plan.pages[1].route, "/");
        assert_eq!(plan.estimate.usd, 0.0);
        assert_eq!(drafts.len(), 1);
        check_plan(&plan, &drafts, &BTreeMap::new()).unwrap();

        // A design on a plan nothing of which is shown in a browser is dropped.
        let mut cli = Plan { kind: ProjectKind::Cli, pages: vec![module("main", "src/**")], design: "palette #000".into(), ..Default::default() };
        let notes = normalize_plan(&mut cli, &mut Vec::new());
        assert!(cli.design.is_empty(), "{notes:?}");
    }

    #[test]
    fn a_contract_is_an_interface_and_never_a_units_own_file() {
        let long = "def f(): ...\n".repeat(CONTRACT_LINES + 1);
        let plan = Plan {
            kind: ProjectKind::Library,
            contracts: vec![
                ContractFile { path: "src/models.py".into(), content: "class Model: ...\n".into() },
                ContractFile { path: "contracts/big.py".into(), content: long },
            ],
            pages: vec![module("models", "src/models.py"), module("api", "src/api/**")],
            ..Default::default()
        };
        let found = rejections(&plan, &[], &BTreeMap::new());
        let owned = found.iter().find(|r| r.rule == Rule::ContractNotOwned).expect("the owned contract is rejected");
        assert!(owned.detail.contains("`src/models.py`") && owned.detail.contains("`models`"), "{}", owned.detail);
        assert!(found.iter().any(|r| r.rule == Rule::ContractSize && r.detail.contains("contracts/big.py")));

        // With a single unit the contract becomes that unit's starting interface instead.
        let mut single = Plan {
            kind: ProjectKind::Library,
            contracts: vec![ContractFile { path: "src/models.py".into(), content: "class Model: ...\n".into() }],
            pages: vec![module("models", "src/**")],
            ..Default::default()
        };
        normalize_plan(&mut single, &mut Vec::new());
        assert!(single.contracts.is_empty());
        assert!(single.pages[0].brief.contains("src/models.py") && single.pages[0].brief.contains("class Model"));
        check_plan(&single, &[], &BTreeMap::new()).unwrap();
    }

    #[test]
    fn every_rejection_is_a_rule_the_planner_is_shown() {
        let text = rules_text();
        for rule in Rule::ALL {
            assert!(text.contains(rule.text()), "{rule:?} is not in the planner's rules");
        }
        // A plan that breaks every rule at once: each reason found is one of those rules.
        let existing = BTreeMap::from([("theme".into(), "a".into())]);
        let broken = Plan {
            kind: ProjectKind::Library,
            contracts: vec![
                ContractFile { path: "../x.py".into(), content: " ".into() },
                ContractFile { path: "lib/a.py".into(), content: "x\n".repeat(PLAN_CONTRACT_LINES + 1) },
            ],
            pages: vec![
                PagePlan { id: "a b".into(), route: "/a".into(), owned_glob: "".into(), brief: " ".into(), cites: vec!["nope".into()], ..Default::default() },
                PagePlan { id: "lib".into(), owned_glob: "lib/**".into(), brief: "x".into(), ..Default::default() },
            ],
            estimate: Estimate { tokens: 0, usd: -1.0, ..Default::default() },
            design: "palette".into(),
            ..Default::default()
        };
        let drafts = [DraftEntry { key: "theme".into(), kind: EntryKind::Hint, summary: " ".into(), content: "b".into(), stack: String::new() }, DraftEntry { key: "bad key".into(), kind: EntryKind::Hint, summary: "s".into(), content: "c".into(), stack: String::new() }];
        let found = rejections(&broken, &drafts, &existing);
        let rules: BTreeSet<_> = found.iter().map(|r| format!("{:?}", r.rule)).collect();
        for rule in Rule::ALL.iter().filter(|r| **r != Rule::AtLeastOneUnit) {
            assert!(rules.contains(&format!("{rule:?}")), "{rule:?} was not found in {found:#?}");
        }
    }
}
