//! What the Commons keeps: people, their daemon tokens, entries and their review state,
//! how entries fare in use, and reports.
//!
//! [`Db`] is the seam. [`Memory`] keeps it all in this process, for tests and for trying
//! the service out; `crate::pg::Postgres` is the real one.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;
use tm_core::{DictTier, DictionaryEntry, EntryKind, EntryStats};

use crate::time::rfc3339;

/// Who someone is at the sign-in provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `github` or `google`.
    pub provider: String,
    /// The provider's stable id for the account.
    pub subject: String,
    pub login: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: i64,
    pub provider: String,
    pub subject: String,
    pub login: Option<String>,
    pub email: Option<String>,
    pub name: Option<String>,
    pub banned: bool,
}

impl User {
    pub fn sub(&self) -> String {
        format!("{}:{}", self.provider, self.subject)
    }

    /// How to show this person: the GitHub login, else the name, else the email.
    pub fn handle(&self) -> String {
        match (&self.login, &self.name, &self.email) {
            (Some(login), _, _) => format!("@{login}"),
            (None, Some(name), _) => name.clone(),
            (None, None, Some(email)) => email.clone(),
            _ => self.sub(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting for the weekly review. Not served.
    Pending,
    Approved,
    Rejected,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Rejected => "rejected",
        }
    }

    pub fn parse(text: &str) -> Option<Status> {
        match text {
            "pending" => Some(Status::Pending),
            "approved" => Some(Status::Approved),
            "rejected" => Some(Status::Rejected),
            _ => None,
        }
    }
}

/// An entry as it was sent, with who sent it and where its review stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Submission {
    pub key: String,
    pub hash: String,
    pub kind: EntryKind,
    pub summary: String,
    pub content: String,
    pub stack: String,
    pub author: i64,
    pub author_handle: String,
    pub status: Status,
    pub created_at: String,
    pub note: Option<String>,
    /// Runs that used it and passed, and failed, counting every reporter.
    pub passes: u64,
    pub fails: u64,
    /// Different people whose runs used it.
    pub users: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub id: i64,
    pub key: String,
    pub hash: String,
    pub reporter: i64,
    pub reporter_handle: String,
    pub reason: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TokenInfo {
    /// The first characters of the token's hash: enough to name it, useless to sign in.
    pub id: String,
    pub label: String,
    pub created_at: String,
    pub last_used: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    pub approved: u64,
    pub pending: u64,
    pub users: u64,
    pub open_reports: u64,
}

#[async_trait]
pub trait Db: Send + Sync {
    /// Create whatever is missing. Safe to run on every start.
    async fn migrate(&self) -> anyhow::Result<()>;

    /// The account for this identity, made on first sign-in and kept up to date after.
    async fn upsert_user(&self, identity: &Identity) -> anyhow::Result<User>;
    async fn user(&self, id: i64) -> anyhow::Result<Option<User>>;
    /// A ban also withdraws everything the person sent, served or not.
    async fn set_banned(&self, id: i64, banned: bool) -> anyhow::Result<()>;

    /// `hash` is the token's fingerprint; the token itself is never stored.
    async fn add_token(&self, user: i64, hash: &str, label: &str) -> anyhow::Result<()>;
    /// Whose token this is, if it is live. Marks it used.
    async fn token_user(&self, hash: &str) -> anyhow::Result<Option<User>>;
    async fn tokens(&self, user: i64) -> anyhow::Result<Vec<TokenInfo>>;
    async fn revoke_token(&self, user: i64, id: &str) -> anyhow::Result<bool>;

    /// `false` when this exact entry was sent before.
    async fn submit(&self, entry: &DictionaryEntry, author: i64) -> anyhow::Result<bool>;
    /// Entries this person sent in the last 24 hours.
    async fn submitted_since_yesterday(&self, author: i64) -> anyhow::Result<u64>;
    async fn submissions_by(&self, author: i64) -> anyhow::Result<Vec<Submission>>;
    async fn submissions(&self, status: Status) -> anyhow::Result<Vec<Submission>>;
    async fn submission(&self, key: &str, hash: &str) -> anyhow::Result<Option<Submission>>;
    async fn review(&self, key: &str, hash: &str, status: Status, moderator: i64, note: Option<&str>) -> anyhow::Result<bool>;

    /// `false` when the entry is not one this Commons serves.
    async fn record_usage(&self, key: &str, hash: &str, user: i64, passed: bool) -> anyhow::Result<bool>;

    async fn report(&self, key: &str, hash: &str, user: i64, reason: &str) -> anyhow::Result<bool>;
    async fn open_reports(&self) -> anyhow::Result<Vec<Report>>;
    async fn resolve_report(&self, id: i64) -> anyhow::Result<bool>;

    async fn counts(&self) -> anyhow::Result<Counts>;
}

/// The entries to serve: for each key, the approved version that has done best in use.
pub fn best_per_key(approved: Vec<Submission>) -> Vec<DictionaryEntry> {
    let mut best: BTreeMap<String, Submission> = BTreeMap::new();
    for entry in approved {
        let score = |s: &Submission| (s.passes as i64 - 2 * s.fails as i64, s.created_at.clone());
        match best.get(&entry.key) {
            Some(have) if score(have) >= score(&entry) => {}
            _ => {
                best.insert(entry.key.clone(), entry);
            }
        }
    }
    best.into_values().map(served).collect()
}

/// How an approved entry is sent to a daemon.
pub fn served(entry: Submission) -> DictionaryEntry {
    let used = entry.passes + entry.fails;
    DictionaryEntry {
        key: entry.key,
        tier: DictTier::Commons,
        kind: entry.kind,
        summary: entry.summary,
        content: entry.content,
        hash: entry.hash,
        stack: entry.stack,
        stats: Some(EntryStats {
            reuses: used,
            pass_rate: if used == 0 { 1.0 } else { entry.passes as f32 / used as f32 },
            passes: entry.passes,
            fails: entry.fails,
        }),
    }
}

/// Whether an entry made for `entry_stack` suits a project on `wanted`. Both are lists
/// such as `next@15,react@19`. An entry suits when every part of its stack is wanted; an
/// entry with no stack suits everything, and so does a request that names none.
pub fn suits(entry_stack: &str, wanted: Option<&str>) -> bool {
    let Some(wanted) = wanted.filter(|w| !w.trim().is_empty()) else { return true };
    let wanted: Vec<String> = wanted.split(',').map(|part| part.trim().to_ascii_lowercase()).collect();
    entry_stack
        .split(',')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .all(|part| {
            let name = part.split('@').next().unwrap_or("").to_string();
            wanted.iter().any(|w| *w == part || (w.split('@').next() == Some(name.as_str()) && !part.contains('@')))
        })
}

#[derive(Default)]
struct State {
    users: Vec<User>,
    tokens: Vec<(String, i64, String, String, Option<String>, bool)>,
    entries: Vec<(Submission, Option<i64>)>,
    usage: BTreeMap<(String, String, i64), (u64, u64)>,
    reports: Vec<(Report, bool)>,
    next_report: i64,
}

/// Everything in this process. Lost on restart.
#[derive(Default)]
pub struct Memory(Mutex<State>);

impl State {
    fn handle(&self, id: i64) -> String {
        self.users.iter().find(|user| user.id == id).map(User::handle).unwrap_or_default()
    }

    fn with_usage(&self, mut entry: Submission) -> Submission {
        let (mut passes, mut fails, mut users) = (0, 0, 0);
        for ((key, hash, _), (p, f)) in &self.usage {
            if *key == entry.key && *hash == entry.hash {
                passes += p;
                fails += f;
                users += 1;
            }
        }
        (entry.passes, entry.fails, entry.users) = (passes, fails, users);
        entry.author_handle = self.handle(entry.author);
        entry
    }
}

#[async_trait]
impl Db for Memory {
    async fn migrate(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn upsert_user(&self, identity: &Identity) -> anyhow::Result<User> {
        let mut state = self.0.lock().unwrap();
        let id = state.users.len() as i64 + 1;
        if let Some(user) =
            state.users.iter_mut().find(|user| user.provider == identity.provider && user.subject == identity.subject)
        {
            user.login = identity.login.clone();
            user.email = identity.email.clone();
            user.name = identity.name.clone();
            return Ok(user.clone());
        }
        let user = User {
            id,
            provider: identity.provider.clone(),
            subject: identity.subject.clone(),
            login: identity.login.clone(),
            email: identity.email.clone(),
            name: identity.name.clone(),
            banned: false,
        };
        state.users.push(user.clone());
        Ok(user)
    }

    async fn user(&self, id: i64) -> anyhow::Result<Option<User>> {
        Ok(self.0.lock().unwrap().users.iter().find(|user| user.id == id).cloned())
    }

    async fn set_banned(&self, id: i64, banned: bool) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(user) = state.users.iter_mut().find(|user| user.id == id) {
            user.banned = banned;
        }
        if banned {
            for (entry, _) in state.entries.iter_mut().filter(|(entry, _)| entry.author == id) {
                entry.status = Status::Rejected;
                entry.note = Some("the author was banned".into());
            }
        }
        Ok(())
    }

    async fn add_token(&self, user: i64, hash: &str, label: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().tokens.push((hash.into(), user, label.into(), rfc3339(), None, false));
        Ok(())
    }

    async fn token_user(&self, hash: &str) -> anyhow::Result<Option<User>> {
        let mut state = self.0.lock().unwrap();
        let Some(token) = state.tokens.iter_mut().find(|token| token.0 == hash && !token.5) else { return Ok(None) };
        token.4 = Some(rfc3339());
        let id = token.1;
        Ok(state.users.iter().find(|user| user.id == id).cloned())
    }

    async fn tokens(&self, user: i64) -> anyhow::Result<Vec<TokenInfo>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .tokens
            .iter()
            .filter(|token| token.1 == user && !token.5)
            .map(|token| TokenInfo {
                id: token.0[..12].to_string(),
                label: token.2.clone(),
                created_at: token.3.clone(),
                last_used: token.4.clone(),
            })
            .collect())
    }

    async fn revoke_token(&self, user: i64, id: &str) -> anyhow::Result<bool> {
        let mut state = self.0.lock().unwrap();
        let found = state.tokens.iter_mut().find(|token| token.1 == user && token.0.starts_with(id) && id.len() >= 12 && !token.5);
        Ok(found.map(|token| token.5 = true).is_some())
    }

    async fn submit(&self, entry: &DictionaryEntry, author: i64) -> anyhow::Result<bool> {
        let mut state = self.0.lock().unwrap();
        if state.entries.iter().any(|(have, _)| have.key == entry.key && have.hash == entry.hash) {
            return Ok(false);
        }
        state.entries.push((
            Submission {
                key: entry.key.clone(),
                hash: entry.hash.clone(),
                kind: entry.kind,
                summary: entry.summary.clone(),
                content: entry.content.clone(),
                stack: entry.stack.clone(),
                author,
                author_handle: String::new(),
                status: Status::Pending,
                created_at: rfc3339(),
                note: None,
                passes: 0,
                fails: 0,
                users: 0,
            },
            None,
        ));
        Ok(true)
    }

    async fn submitted_since_yesterday(&self, author: i64) -> anyhow::Result<u64> {
        let since = crate::time::rfc3339_at(tm_token::now().saturating_sub(86_400));
        Ok(self.0.lock().unwrap().entries.iter().filter(|(entry, _)| entry.author == author && entry.created_at >= since).count()
            as u64)
    }

    async fn submissions_by(&self, author: i64) -> anyhow::Result<Vec<Submission>> {
        let state = self.0.lock().unwrap();
        let mut out: Vec<Submission> =
            state.entries.iter().filter(|(entry, _)| entry.author == author).map(|(entry, _)| state.with_usage(entry.clone())).collect();
        out.reverse();
        Ok(out)
    }

    async fn submissions(&self, status: Status) -> anyhow::Result<Vec<Submission>> {
        let state = self.0.lock().unwrap();
        Ok(state.entries.iter().filter(|(entry, _)| entry.status == status).map(|(entry, _)| state.with_usage(entry.clone())).collect())
    }

    async fn submission(&self, key: &str, hash: &str) -> anyhow::Result<Option<Submission>> {
        let state = self.0.lock().unwrap();
        Ok(state
            .entries
            .iter()
            .find(|(entry, _)| entry.key == key && entry.hash == hash)
            .map(|(entry, _)| state.with_usage(entry.clone())))
    }

    async fn review(&self, key: &str, hash: &str, status: Status, moderator: i64, note: Option<&str>) -> anyhow::Result<bool> {
        let mut state = self.0.lock().unwrap();
        let Some((entry, by)) = state.entries.iter_mut().find(|(entry, _)| entry.key == key && entry.hash == hash) else {
            return Ok(false);
        };
        entry.status = status;
        entry.note = note.map(str::to_string);
        *by = Some(moderator);
        Ok(true)
    }

    async fn record_usage(&self, key: &str, hash: &str, user: i64, passed: bool) -> anyhow::Result<bool> {
        let mut state = self.0.lock().unwrap();
        if !state.entries.iter().any(|(entry, _)| entry.key == key && entry.hash == hash && entry.status == Status::Approved) {
            return Ok(false);
        }
        let slot = state.usage.entry((key.into(), hash.into(), user)).or_default();
        if passed {
            slot.0 += 1;
        } else {
            slot.1 += 1;
        }
        Ok(true)
    }

    async fn report(&self, key: &str, hash: &str, user: i64, reason: &str) -> anyhow::Result<bool> {
        let mut state = self.0.lock().unwrap();
        if !state.entries.iter().any(|(entry, _)| entry.key == key && entry.hash == hash) {
            return Ok(false);
        }
        state.next_report += 1;
        let report = Report {
            id: state.next_report,
            key: key.into(),
            hash: hash.into(),
            reporter: user,
            reporter_handle: state.handle(user),
            reason: reason.into(),
            created_at: rfc3339(),
        };
        state.reports.push((report, false));
        Ok(true)
    }

    async fn open_reports(&self) -> anyhow::Result<Vec<Report>> {
        Ok(self.0.lock().unwrap().reports.iter().filter(|(_, done)| !done).map(|(report, _)| report.clone()).collect())
    }

    async fn resolve_report(&self, id: i64) -> anyhow::Result<bool> {
        let mut state = self.0.lock().unwrap();
        Ok(state.reports.iter_mut().find(|(report, done)| report.id == id && !done).map(|(_, done)| *done = true).is_some())
    }

    async fn counts(&self) -> anyhow::Result<Counts> {
        let state = self.0.lock().unwrap();
        let count = |status| state.entries.iter().filter(|(entry, _)| entry.status == status).count() as u64;
        Ok(Counts {
            approved: count(Status::Approved),
            pending: count(Status::Pending),
            users: state.users.len() as u64,
            open_reports: state.reports.iter().filter(|(_, done)| !done).count() as u64,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! One suite for every [`Db`], so the Postgres one is held to what the memory one does.

    use super::*;
    use sha2::{Digest, Sha256};

    pub fn entry(key: &str, content: &str, stack: &str) -> DictionaryEntry {
        DictionaryEntry {
            key: key.into(),
            tier: DictTier::Local,
            kind: EntryKind::Pattern,
            summary: format!("{key} summary"),
            content: content.into(),
            hash: Sha256::digest(content.as_bytes()).iter().map(|b| format!("{b:02x}")).collect(),
            stack: stack.into(),
            stats: None,
        }
    }

    fn who(provider: &str, subject: &str, login: Option<&str>) -> Identity {
        Identity {
            provider: provider.into(),
            subject: subject.into(),
            login: login.map(str::to_string),
            email: Some(format!("{subject}@example.com")),
            name: None,
        }
    }

    pub async fn exercise(db: &dyn Db) {
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();

        let ana = db.upsert_user(&who("github", "1", Some("ana"))).await.unwrap();
        let again = db.upsert_user(&who("github", "1", Some("ana-renamed"))).await.unwrap();
        assert_eq!((again.id, again.login.as_deref()), (ana.id, Some("ana-renamed")));
        let bo = db.upsert_user(&who("google", "2", None)).await.unwrap();
        assert_ne!(ana.id, bo.id);
        assert_eq!(db.user(bo.id).await.unwrap().unwrap().email.as_deref(), Some("2@example.com"));

        db.add_token(ana.id, &"a".repeat(64), "laptop").await.unwrap();
        assert_eq!(db.token_user(&"a".repeat(64)).await.unwrap().unwrap().id, ana.id);
        assert!(db.token_user(&"b".repeat(64)).await.unwrap().is_none());
        let tokens = db.tokens(ana.id).await.unwrap();
        assert_eq!((tokens.len(), tokens[0].label.as_str()), (1, "laptop"));
        assert!(tokens[0].last_used.is_some());
        assert!(!db.revoke_token(bo.id, &tokens[0].id).await.unwrap());
        assert!(db.revoke_token(ana.id, &tokens[0].id).await.unwrap());
        assert!(db.token_user(&"a".repeat(64)).await.unwrap().is_none());

        let card = entry("card", "export const Card = 1;\n", "next@15,react@19");
        assert!(db.submit(&card, ana.id).await.unwrap());
        assert!(!db.submit(&card, bo.id).await.unwrap());
        assert_eq!(db.submitted_since_yesterday(ana.id).await.unwrap(), 1);
        let pending = db.submissions(Status::Pending).await.unwrap();
        assert_eq!((pending.len(), pending[0].author_handle.as_str()), (1, "@ana-renamed"));
        assert!(db.submissions(Status::Approved).await.unwrap().is_empty());
        // Not served yet, so its use is not counted.
        assert!(!db.record_usage("card", &card.hash, bo.id, true).await.unwrap());

        assert!(db.review("card", &card.hash, Status::Approved, bo.id, None).await.unwrap());
        assert!(!db.review("card", "nope", Status::Approved, bo.id, None).await.unwrap());
        assert!(db.record_usage("card", &card.hash, bo.id, true).await.unwrap());
        assert!(db.record_usage("card", &card.hash, bo.id, true).await.unwrap());
        assert!(db.record_usage("card", &card.hash, ana.id, false).await.unwrap());
        let served = db.submission("card", &card.hash).await.unwrap().unwrap();
        assert_eq!((served.status, served.passes, served.fails, served.users), (Status::Approved, 2, 1, 2));

        let better = entry("card", "export const Card = 2;\n", "next@15");
        db.submit(&better, bo.id).await.unwrap();
        db.review("card", &better.hash, Status::Approved, ana.id, Some("fine")).await.unwrap();
        for _ in 0..3 {
            db.record_usage("card", &better.hash, ana.id, true).await.unwrap();
        }
        let out = best_per_key(db.submissions(Status::Approved).await.unwrap());
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].hash.as_str(), out[0].stats.as_ref().unwrap().passes), (better.hash.as_str(), 3));
        assert_eq!(out[0].tier, DictTier::Commons);

        assert!(db.report("card", &card.hash, bo.id, "it leaks a key").await.unwrap());
        assert!(!db.report("nope", &card.hash, bo.id, "x").await.unwrap());
        let reports = db.open_reports().await.unwrap();
        assert_eq!((reports.len(), reports[0].reason.as_str()), (1, "it leaks a key"));
        assert!(db.resolve_report(reports[0].id).await.unwrap());
        assert!(!db.resolve_report(reports[0].id).await.unwrap());

        let counts = db.counts().await.unwrap();
        assert_eq!((counts.approved, counts.pending, counts.users, counts.open_reports), (2, 0, 2, 0));

        // A ban withdraws what the person sent.
        db.set_banned(bo.id, true).await.unwrap();
        assert!(db.user(bo.id).await.unwrap().unwrap().banned);
        let left = best_per_key(db.submissions(Status::Approved).await.unwrap());
        assert_eq!(left[0].hash, card.hash);
        assert_eq!(db.submissions_by(bo.id).await.unwrap()[0].status, Status::Rejected);
    }

    #[tokio::test]
    async fn memory_keeps_the_commons() {
        exercise(&Memory::default()).await;
    }

    #[test]
    fn an_entry_suits_a_project_whose_stack_covers_its_own() {
        assert!(suits("next@15,react@19", Some("next@15, react@19, tailwind@4")));
        assert!(!suits("next@15,react@19", Some("next@14,react@19")));
        assert!(suits("", Some("next@15")));
        assert!(suits("next@15", None));
        assert!(suits("react", Some("react@19")));
    }
}
