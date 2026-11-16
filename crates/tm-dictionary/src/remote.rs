//! Where a Commons shared between people will plug in.
//!
//! The Commons on this machine is a directory. A [`CommonsRemote`] is the other side: a
//! server, a bucket, a git repository. Nothing in this crate implements one yet. The daemon
//! pulls before it plans and pushes each entry it promotes, so an implementation only has
//! to move entries; it never decides what is promoted.

use tm_core::DictionaryEntry;

use crate::store::LocalStore;

pub trait CommonsRemote: Send + Sync {
    /// Copy entries other people published into `commons`. Returns how many arrived.
    /// Entries are content-addressed, so storing one twice changes nothing.
    fn pull(&self, commons: &LocalStore) -> anyhow::Result<usize>;

    /// Offer one entry that was just promoted on this machine.
    fn push(&self, entry: &DictionaryEntry) -> anyhow::Result<()>;
}
