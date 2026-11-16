//! The daemon's records in a SQLite database.
//!
//! One table, one row per record: the key the file store would use as a path, and the
//! bytes. That is all [`Store`] asks for, so a run behaves the same on either. A server
//! database is a third implementation with the same table.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use crate::store::Store;

pub struct SqliteStore {
    db: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        // Readers do not block the writer, and a crash leaves the last commit intact.
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute("CREATE TABLE IF NOT EXISTS records (key TEXT PRIMARY KEY, body BLOB NOT NULL) WITHOUT ROWID", [])?;
        Ok(SqliteStore { db: Mutex::new(db) })
    }
}

impl Store for SqliteStore {
    fn read(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let db = self.db.lock().unwrap();
        Ok(db.query_row("SELECT body FROM records WHERE key = ?1", [key], |row| row.get(0)).optional()?)
    }

    fn write(&self, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO records (key, body) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET body = excluded.body",
            params![key, bytes],
        )?;
        Ok(())
    }

    fn append(&self, key: &str, line: &str) -> anyhow::Result<()> {
        let line = format!("{}\n", line.trim_end_matches('\n'));
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO records (key, body) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET body = CAST(body || excluded.body AS BLOB)",
            params![key, line.as_bytes()],
        )?;
        Ok(())
    }

    fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.db.lock().unwrap().execute("DELETE FROM records WHERE key = ?1", [key])?;
        Ok(())
    }

    fn list(&self, prefix: &str) -> anyhow::Result<Vec<String>> {
        let start = format!("{prefix}/");
        let db = self.db.lock().unwrap();
        // `substr` rather than LIKE, so `_` and `%` in a run id mean themselves.
        let mut query = db.prepare("SELECT key FROM records WHERE substr(key, 1, ?1) = ?2 ORDER BY key")?;
        let keys = query.query_map(params![start.len() as i64, start], |row| row.get::<_, String>(0))?;
        let mut names: Vec<String> = Vec::new();
        for key in keys {
            let key = key?;
            let name = key[start.len()..].split('/').next().unwrap_or_default().to_string();
            if names.last() != Some(&name) {
                names.push(name);
            }
        }
        names.sort();
        names.dedup();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Records;
    use std::sync::Arc;

    #[test]
    fn a_database_holds_the_same_records_as_files() {
        let path = std::env::temp_dir().join(format!("tm-sqlite-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let records = Records::new(Arc::new(SqliteStore::open(&path).unwrap()));
        crate::store::tests::exercise(&records);
        // A second connection sees what the first one wrote.
        drop(records);
        let again = SqliteStore::open(&path).unwrap();
        assert!(again.read("runs/r1/events.jsonl").unwrap().is_some());
        again.append("runs/r2/ledger.jsonl", "{\"a\":1}").unwrap();
        again.append("runs/r2/ledger.jsonl", "{\"a\":2}").unwrap();
        assert_eq!(again.read("runs/r2/ledger.jsonl").unwrap().unwrap(), b"{\"a\":1}\n{\"a\":2}\n");
        assert_eq!(again.list("runs").unwrap(), vec!["r1".to_string(), "r2".to_string()]);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
