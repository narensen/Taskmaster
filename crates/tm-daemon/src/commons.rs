//! A Commons shared over HTTP.
//!
//! Any Taskmaster daemon is a hub: `GET /api/commons/entries` lists its Commons and
//! `POST /api/commons/entries` adds to it. [`HttpCommons`] is the other end. Point it at a
//! hub and this machine pulls what others promoted before it plans, and sends what it
//! promotes. The token is one the hub issued, the same as for any other API call.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use tm_core::DictionaryEntry;
use tm_dictionary::{CommonsRemote, LocalStore, matches_hash};

pub struct HttpCommons {
    base: String,
    token: String,
    agent: ureq::Agent,
}

#[derive(Deserialize)]
struct Listing {
    entries: Vec<DictionaryEntry>,
}

impl HttpCommons {
    pub fn new(base: &str, token: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
        HttpCommons { base: base.trim_end_matches('/').to_string(), token: token.to_string(), agent }
    }

    fn url(&self) -> String {
        format!("{}/api/commons/entries", self.base)
    }
}

impl CommonsRemote for HttpCommons {
    fn pull(&self, commons: &LocalStore) -> anyhow::Result<usize> {
        let listing: Listing = self
            .agent
            .get(&self.url())
            .set("authorization", &format!("Bearer {}", self.token))
            .call()
            .with_context(|| format!("cannot reach the Commons at {}", self.base))?
            .into_json()
            .context("the Commons sent something that is not a list of entries")?;
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
        self.agent
            .post(&self.url())
            .set("authorization", &format!("Bearer {}", self.token))
            .send_json(entry)
            .with_context(|| format!("cannot send `{}` to the Commons at {}", entry.key, self.base))?;
        Ok(())
    }
}
