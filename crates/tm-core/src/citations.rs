//! The citation graph used to decide which pages to rebuild.
//!
//! A page is fresh when it was built before and every Dictionary key it cites still has
//! the hash it was built against. Anything else is rebuilt. Pages that cite nothing are
//! fresh once they have an empty record.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DictKey;

/// One cited entry as it was when the page was built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    pub key: DictKey,
    pub hash: String,
}

/// The last successful build, keyed by page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Citations {
    pub pages: BTreeMap<String, Vec<Citation>>,
}

/// `current` maps a key to its hash now. A missing key is an empty hash.
pub fn page_is_fresh(
    cites: &[DictKey],
    recorded: Option<&[Citation]>,
    current: &BTreeMap<DictKey, String>,
) -> bool {
    let Some(recorded) = recorded else { return false };
    if cites.len() != recorded.len() {
        return false;
    }
    cites.iter().all(|key| {
        let now = current.get(key).map(String::as_str).unwrap_or("");
        recorded.iter().any(|citation| citation.key == *key && citation.hash == now)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(key: &str, hash: &str) -> Citation {
        Citation { key: key.into(), hash: hash.into() }
    }

    #[test]
    fn only_a_changed_citation_is_rebuilt() {
        let recorded = vec![cite("auth", "aaa"), cite("nav", "bbb")];
        let mut current = BTreeMap::from([("auth".into(), "aaa".into()), ("nav".into(), "bbb".into())]);
        let cites = vec!["auth".into(), "nav".into()];
        assert!(page_is_fresh(&cites, Some(&recorded), &current));
        current.insert("nav".into(), "ccc".into());
        assert!(!page_is_fresh(&cites, Some(&recorded), &current));
        assert!(!page_is_fresh(&cites, None, &current));
        assert!(page_is_fresh(&[], Some(&[]), &BTreeMap::new()));
        assert!(!page_is_fresh(&["auth".into()], Some(&[]), &current));
    }
}
