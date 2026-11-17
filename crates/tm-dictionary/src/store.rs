//! Local tier: `<root>/<key>/<hash>.json` per version, `<root>/<key>/HEAD` naming the latest.
//! Entries are never edited. A change publishes a successor and moves HEAD.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use tm_core::{DictTier, DictionaryEntry, EntryKind};

#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
}

/// What the index shows every worker: enough to decide what to cite, nothing more.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IndexLine {
    pub key: String,
    pub tier: tm_core::DictTier,
    pub kind: EntryKind,
    pub summary: String,
}

impl LocalStore {
    pub fn open(root: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(LocalStore { root })
    }

    /// Publish an entry. Returns its hash. Publishing identical content again is a no-op.
    pub fn publish(
        &self,
        key: &str,
        kind: EntryKind,
        summary: &str,
        content: &str,
        stack: &str,
    ) -> anyhow::Result<String> {
        check_key(key)?;
        let hash = hex(&Sha256::digest(content.as_bytes()));
        let entry = DictionaryEntry {
            key: key.into(),
            tier: DictTier::Local,
            kind,
            summary: summary.into(),
            content: content.into(),
            hash: hash.clone(),
            stack: stack.into(),
            stats: None,
        };
        let dir = self.root.join(key);
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join(format!("{hash}.json")), &serde_json::to_vec_pretty(&entry)?)?;
        write_atomic(&dir.join("HEAD"), hash.as_bytes())?;
        Ok(hash)
    }

    /// The latest version of `key`, if any.
    pub fn get(&self, key: &str) -> anyhow::Result<Option<DictionaryEntry>> {
        check_key(key)?;
        let dir = self.root.join(key);
        let Ok(head) = fs::read_to_string(dir.join("HEAD")) else { return Ok(None) };
        self.version(key, head.trim())
    }

    pub fn version(&self, key: &str, hash: &str) -> anyhow::Result<Option<DictionaryEntry>> {
        check_key(key)?;
        if !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid hash");
        }
        let path = self.root.join(key).join(format!("{hash}.json"));
        let Ok(bytes) = fs::read(&path) else { return Ok(None) };
        let entry: DictionaryEntry = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt entry {}", path.display()))?;
        // The hash is the address: content that no longer matches it has been tampered with.
        if hex(&Sha256::digest(entry.content.as_bytes())) != entry.hash {
            bail!("entry {key}@{hash} does not match its hash");
        }
        Ok(Some(entry))
    }

    pub fn index(&self) -> anyhow::Result<Vec<IndexLine>> {
        let mut lines = Vec::new();
        for dir in fs::read_dir(&self.root)? {
            let name = dir?.file_name().to_string_lossy().into_owned();
            if let Some(e) = self.get(&name)? {
                lines.push(IndexLine { key: e.key, tier: e.tier, kind: e.kind, summary: e.summary });
            }
        }
        lines.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(lines)
    }

    /// Every current entry.
    pub fn entries(&self) -> anyhow::Result<Vec<DictionaryEntry>> {
        let mut out = Vec::new();
        let Ok(dirs) = fs::read_dir(&self.root) else { return Ok(out) };
        for dir in dirs {
            let name = dir?.file_name().to_string_lossy().into_owned();
            if let Some(entry) = self.get(&name)? {
                out.push(entry);
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    /// Write an entry whose hash already matches its content, and point HEAD at it.
    pub fn put(&self, entry: &DictionaryEntry) -> anyhow::Result<()> {
        check_key(&entry.key)?;
        if !matches_hash(entry) {
            bail!("entry {} does not match its hash", entry.key);
        }
        if !entry.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("invalid hash");
        }
        let dir = self.root.join(&entry.key);
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join(format!("{}.json", entry.hash)), &serde_json::to_vec_pretty(entry)?)?;
        write_atomic(&dir.join("HEAD"), entry.hash.as_bytes())?;
        Ok(())
    }

    /// Delete one version, as when the Commons withdraws it. With it gone from HEAD, the
    /// key is gone: an older version is not brought back, since nobody chose it.
    /// Returns whether anything was deleted.
    pub fn withdraw(&self, key: &str, hash: &str) -> anyhow::Result<bool> {
        check_key(key)?;
        if !hash.chars().all(|c| c.is_ascii_hexdigit()) || hash.is_empty() {
            bail!("invalid hash");
        }
        let dir = self.root.join(key);
        let file = dir.join(format!("{hash}.json"));
        if !file.is_file() {
            return Ok(false);
        }
        let head = fs::read_to_string(dir.join("HEAD")).unwrap_or_default();
        if head.trim() == hash {
            fs::remove_dir_all(&dir)?;
        } else {
            fs::remove_file(file)?;
        }
        Ok(true)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Keys become directory names, so they must not be able to name anything else.
/// Write `bytes` to `path` so a reader sees the old file or the new one, never a torn one.
/// Entries are rewritten while parallel workers read them (pass counts, Commons pulls).
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()));
    fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn check_key(key: &str) -> anyhow::Result<()> {
    if tm_core::valid_key(key) {
        Ok(())
    } else {
        bail!("invalid key `{key}`: use letters, digits, `_` and `-`");
    }
}

/// Whether an entry's content is what its hash says. Check this before trusting an entry
/// that came from another machine.
pub fn matches_hash(entry: &DictionaryEntry) -> bool {
    hex(&Sha256::digest(entry.content.as_bytes())) == entry.hash
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (LocalStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tm-dict-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = fs::remove_dir_all(&dir);
        (LocalStore::open(&dir).unwrap(), dir)
    }

    #[test]
    fn publish_get_and_successor() {
        let (s, dir) = store();
        let h1 = s.publish("auth", EntryKind::Schema, "auth types", "type A = 1;", "next@15").unwrap();
        assert_eq!(s.get("auth").unwrap().unwrap().content, "type A = 1;");
        let h2 = s.publish("auth", EntryKind::Schema, "auth types v2", "type A = 2;", "next@15").unwrap();
        assert_ne!(h1, h2);
        assert_eq!(s.get("auth").unwrap().unwrap().hash, h2);
        // The old version is still addressable.
        assert_eq!(s.version("auth", &h1).unwrap().unwrap().content, "type A = 1;");
        assert_eq!(s.index().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_path_like_keys() {
        let (s, dir) = store();
        for bad in ["../x", "a/b", "", ".", "a b"] {
            assert!(s.get(bad).is_err(), "{bad}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detects_tampering() {
        let (s, dir) = store();
        let h = s.publish("k", EntryKind::Hint, "s", "good", "x").unwrap();
        let path = dir.join("k").join(format!("{h}.json"));
        let tampered = fs::read_to_string(&path).unwrap().replace("good", "evil");
        fs::write(&path, tampered).unwrap();
        assert!(s.get("k").is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
