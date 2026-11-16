//! A Commons shared over HTTP: the Taskmaster Commons service, or any daemon acting as
//! a hub (`GET` and `POST /api/commons/entries` on either).
//!
//! Pulling needs no account. Sharing does: the token is one made on the service's account
//! page (or one the hub daemon issued when this machine paired with it). Sharing is off
//! unless `share` is set, because an entry is code from this project. Reports of how
//! Commons entries did are on unless `report` is turned off; they say which entry was
//! used and whether the page passed, nothing else.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::json;
use tm_core::DictionaryEntry;
use tm_dictionary::{CommonsRemote, LocalStore, matches_hash};

pub struct HttpCommons {
    base: String,
    token: String,
    share: bool,
    report: bool,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct Listing {
    entries: Vec<DictionaryEntry>,
    /// Entries the Commons no longer serves. A hub daemon does not send this.
    #[serde(default)]
    withdrawn: Vec<Withdrawn>,
}

#[derive(Deserialize)]
struct Withdrawn {
    key: String,
    hash: String,
}

impl HttpCommons {
    pub fn new(base: &str, token: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
        HttpCommons { base: base.trim_end_matches('/').to_string(), token: token.to_string(), share: true, report: true, agent }
    }

    /// Whether promoted entries are offered to the Commons.
    pub fn sharing(mut self, share: bool) -> Self {
        self.share = share;
        self
    }

    /// Whether to say how Commons entries did in this machine's runs.
    pub fn reporting(mut self, report: bool) -> Self {
        self.report = report;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/commons/{path}", self.base)
    }

    fn post(&self, path: &str) -> ureq::Request {
        self.agent.post(&self.url(path)).set("authorization", &format!("Bearer {}", self.token))
    }
}

impl CommonsRemote for HttpCommons {
    fn pull(&self, commons: &LocalStore) -> anyhow::Result<usize> {
        let listing: Listing = self
            .agent
            .get(&self.url("entries"))
            .set("authorization", &format!("Bearer {}", self.token))
            .call()
            .with_context(|| format!("cannot reach the Commons at {}", self.base))?
            .into_json()
            .context("the Commons sent something that is not a list of entries")?;
        for gone in &listing.withdrawn {
            if tm_core::valid_key(&gone.key) && commons.withdraw(&gone.key, &gone.hash)? {
                eprintln!("taskmaster: the Commons withdrew `{}`, so it was deleted here", gone.key);
            }
        }
        let mut arrived = 0;
        for entry in listing.entries {
            if !matches_hash(&entry) {
                anyhow::bail!("the Commons at {} sent `{}` with content that does not match its hash", self.base, entry.key);
            }
            // Same content already here: keep it, with the pass and fail counts made here.
            if commons.get(&entry.key)?.is_some_and(|have| have.hash == entry.hash) {
                continue;
            }
            commons.put(&entry)?;
            arrived += 1;
        }
        Ok(arrived)
    }

    fn push(&self, entry: &DictionaryEntry) -> anyhow::Result<()> {
        if !self.share {
            return Ok(());
        }
        self.post("entries")
            .send_json(entry)
            .with_context(|| format!("cannot send `{}` to the Commons at {}", entry.key, self.base))?;
        Ok(())
    }

    fn report(&self, entry: &DictionaryEntry, passed: bool) -> anyhow::Result<()> {
        if !self.report || self.token.is_empty() {
            return Ok(());
        }
        match self.post("usage").send_json(json!({ "key": entry.key, "hash": entry.hash, "passed": passed })) {
            Ok(_) => Ok(()),
            // An entry this Commons does not serve, such as one from a hub daemon.
            Err(ureq::Error::Status(404, _)) => Ok(()),
            Err(err) => Err(err).with_context(|| format!("cannot report `{}` to the Commons at {}", entry.key, self.base)),
        }
    }
}
