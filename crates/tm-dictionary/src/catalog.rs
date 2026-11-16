//! Local dictionary plus the shared Commons.
//!
//! `resolve` prefers the project entry. `search` returns Commons hits first so a worker
//! looks there before inventing a new contract. `promote` copies a local entry into the
//! Commons without dropping its history there.

use anyhow::Context;
use tm_core::{DictTier, DictionaryEntry, EntryStats};

use crate::store::LocalStore;

pub struct Catalog {
    pub local: LocalStore,
    pub commons: LocalStore,
}

impl Catalog {
    pub fn open(local: impl Into<std::path::PathBuf>, commons: impl Into<std::path::PathBuf>) -> anyhow::Result<Self> {
        Ok(Catalog { local: LocalStore::open(local)?, commons: LocalStore::open(commons)? })
    }

    /// The project entry when it exists, otherwise the Commons entry.
    pub fn resolve(&self, key: &str) -> anyhow::Result<Option<DictionaryEntry>> {
        if let Some(entry) = self.local.get(key)? {
            return Ok(Some(entry));
        }
        self.commons.get(key)
    }

    /// Commons matches first, then local keys that are not already listed.
    pub fn search(&self, query: &str) -> anyhow::Result<Vec<DictionaryEntry>> {
        let query = query.trim().to_lowercase();
        let mut hits = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for entry in self.commons.entries()? {
            if matches(&entry, &query) {
                seen.insert(entry.key.clone());
                hits.push(entry);
            }
        }
        for entry in self.local.entries()? {
            if seen.contains(&entry.key) || !matches(&entry, &query) {
                continue;
            }
            hits.push(entry);
        }
        hits.truncate(20);
        Ok(hits)
    }

    /// Copy the local HEAD into the Commons. The same content keeps the Commons stats.
    pub fn promote(&self, key: &str) -> anyhow::Result<Option<String>> {
        let Some(entry) = self.local.get(key)? else { return Ok(None) };
        if let Some(existing) = self.commons.get(key)?
            && existing.hash == entry.hash {
                return Ok(Some(existing.hash));
            }
        let mut shared = entry;
        shared.tier = DictTier::Commons;
        shared.stats = Some(EntryStats { reuses: 0, pass_rate: 1.0, passes: 0, fails: 0 });
        let hash = shared.hash.clone();
        self.commons.put(&shared)?;
        Ok(Some(hash))
    }

    pub fn note_reuse(&self, key: &str) -> anyhow::Result<()> {
        self.commons_edit(key, |stats| stats.reuses += 1)
    }

    pub fn note_result(&self, key: &str, passed: bool) -> anyhow::Result<()> {
        self.commons_edit(key, |stats| {
            if passed {
                stats.passes += 1;
            } else {
                stats.fails += 1;
            }
            let total = stats.passes + stats.fails;
            stats.pass_rate = if total == 0 { 1.0 } else { stats.passes as f32 / total as f32 };
        })
    }

    fn commons_edit(&self, key: &str, edit: impl FnOnce(&mut EntryStats)) -> anyhow::Result<()> {
        let Some(mut entry) = self.commons.get(key)? else { return Ok(()) };
        if entry.tier != DictTier::Commons {
            return Ok(());
        }
        let stats = entry.stats.get_or_insert(EntryStats { reuses: 0, pass_rate: 1.0, passes: 0, fails: 0 });
        edit(stats);
        self.commons.put(&entry).with_context(|| format!("cannot update commons entry {key}"))?;
        Ok(())
    }
}

fn matches(entry: &DictionaryEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    entry.key.to_lowercase().contains(query)
        || entry.summary.to_lowercase().contains(query)
        || entry.content.to_lowercase().contains(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::EntryKind;

    fn dirs(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "tm-commons-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        (root.join("local"), root.join("commons"))
    }

    #[test]
    fn promote_search_and_stats() {
        let (local, commons) = dirs("one");
        let catalog = Catalog::open(&local, &commons).unwrap();
        catalog.local.publish("auth", EntryKind::Hint, "role list", "export const ROLES = ['admin'];\n", "next").unwrap();
        catalog.commons.publish("theme", EntryKind::Pattern, "color tokens", "export const INK = '#fff';\n", "next").unwrap();
        // publish() always writes a local-tier entry. Mark the shared one as Commons.
        let mut theme = catalog.commons.get("theme").unwrap().unwrap();
        theme.tier = DictTier::Commons;
        theme.stats = Some(EntryStats { reuses: 0, pass_rate: 1.0, passes: 0, fails: 0 });
        catalog.commons.put(&theme).unwrap();

        let hits = catalog.search("color").unwrap();
        assert_eq!(hits[0].key, "theme");
        assert_eq!(hits[0].tier, DictTier::Commons);
        assert!(catalog.search("role").unwrap().iter().any(|entry| entry.key == "auth"));

        let hash = catalog.promote("auth").unwrap().unwrap();
        assert_eq!(catalog.commons.get("auth").unwrap().unwrap().hash, hash);
        assert_eq!(catalog.commons.get("auth").unwrap().unwrap().tier, DictTier::Commons);
        // Promoting the same bytes again keeps the entry.
        catalog.note_reuse("auth").unwrap();
        catalog.note_result("auth", true).unwrap();
        let again = catalog.promote("auth").unwrap().unwrap();
        assert_eq!(again, hash);
        let stats = catalog.commons.get("auth").unwrap().unwrap().stats.unwrap();
        assert_eq!(stats.reuses, 1);
        assert_eq!(stats.passes, 1);

        // The project copy wins over the Commons copy.
        catalog.local.publish("theme", EntryKind::Pattern, "local colors", "export const INK = '#000';\n", "next").unwrap();
        assert_eq!(catalog.resolve("theme").unwrap().unwrap().content, "export const INK = '#000';\n");
        let _ = std::fs::remove_dir_all(local.parent().unwrap());
    }
}
