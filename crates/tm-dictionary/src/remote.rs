//! A Commons shared between people.
//!
//! The Commons on this machine is a directory. A [`CommonsRemote`] is the other side: the
//! Taskmaster Commons service, or another daemon acting as a hub. The daemon pulls before
//! it plans, offers each entry it promotes, and reports how each Commons entry did, so an
//! implementation only moves entries and results; it never decides what is promoted.

use tm_core::DictionaryEntry;

use crate::store::LocalStore;

pub trait CommonsRemote: Send + Sync {
    /// Copy entries other people published into `commons`, and delete ones the Commons has
    /// withdrawn. Returns how many arrived. Entries are content-addressed, so storing one
    /// twice changes nothing.
    fn pull(&self, commons: &LocalStore) -> anyhow::Result<usize>;

    /// Offer one entry that was just promoted on this machine.
    fn push(&self, entry: &DictionaryEntry) -> anyhow::Result<()>;

    /// Say whether a page that cited this Commons entry passed. This is how an entry's
    /// pass rate is earned in other people's runs.
    fn report(&self, _entry: &DictionaryEntry, _passed: bool) -> anyhow::Result<()> {
        Ok(())
    }
}
