//! Which page owns which file, from the plan's owned globs.

use anyhow::Context;
use globset::{Glob, GlobMatcher};
use tm_core::{PageId, Plan};

pub struct Owners {
    pages: Vec<(PageId, GlobMatcher)>,
}

impl Owners {
    pub fn from_plan(plan: &Plan) -> anyhow::Result<Self> {
        let pages = plan
            .pages
            .iter()
            .map(|p| {
                let glob = Glob::new(&p.owned_glob)
                    .with_context(|| format!("page `{}` has an invalid glob `{}`", p.id, p.owned_glob))?;
                Ok((p.id.clone(), glob.compile_matcher()))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Owners { pages })
    }

    /// The page that owns `file`, or `None` for shared code (contracts, the shell, the UI kit).
    pub fn owner(&self, file: &str) -> Option<&PageId> {
        self.pages.iter().find(|(_, glob)| glob.is_match(file)).map(|(id, _)| id)
    }
}

/// Files a worker changed that its page does not own. These are dropped at merge and flagged.
pub fn outside_owned<'a>(owners: &Owners, page: &str, changed: &'a [String]) -> Vec<&'a str> {
    changed
        .iter()
        .filter(|file| owners.owner(file).map(String::as_str) != Some(page))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
pub(crate) fn test_plan(pages: &[(&str, &str)]) -> Plan {
    use tm_core::{Estimate, PagePlan};
    Plan {
        kind: Default::default(),
        contracts: vec![],
        pages: pages
            .iter()
            .map(|(id, glob)| PagePlan {
                id: id.to_string(),
                route: format!("/{id}"),
                ui: None,
                owned_glob: glob.to_string(),
                cites: vec![],
                brief: String::new(),
                done: String::new(),
                units: vec![],
                skills: vec![],
            })
            .collect(),
        estimate: Estimate { tokens: 0, usd: 0.0 },
        design: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_owner_and_treats_the_rest_as_shared() {
        let owners = Owners::from_plan(&test_plan(&[("a", "app/a/**"), ("b", "app/b/**")])).unwrap();
        assert_eq!(owners.owner("app/a/page.tsx").map(String::as_str), Some("a"));
        assert_eq!(owners.owner("app/b/parts/list.tsx").map(String::as_str), Some("b"));
        assert_eq!(owners.owner("contracts/types.ts"), None);
        // A sibling directory with a longer name is not owned by `a`.
        assert_eq!(owners.owner("app/admin/page.tsx"), None);
    }

    #[test]
    fn reports_changes_outside_the_owned_files() {
        let owners = Owners::from_plan(&test_plan(&[("a", "app/a/**"), ("b", "app/b/**")])).unwrap();
        let changed: Vec<String> =
            ["app/a/page.tsx", "app/b/page.tsx", "contracts/types.ts"].map(String::from).to_vec();
        assert_eq!(outside_owned(&owners, "a", &changed), vec!["app/b/page.tsx", "contracts/types.ts"]);
    }

    #[test]
    fn an_invalid_glob_names_its_page() {
        let err = Owners::from_plan(&test_plan(&[("a", "app/[")])).err().unwrap();
        assert!(err.to_string().contains("page `a`"));
    }
}
