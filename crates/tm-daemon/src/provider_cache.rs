//! The provider list, kept for a while.
//!
//! Listing a harness's models starts its CLI, and asking all five takes seconds. Every page
//! that offers a model, and every new run that checks the models it was given, used to pay
//! that. A report younger than [`FRESH`] is used as is. An older one, up to [`KEEP`], is
//! used at once and refreshed behind the answer. An install, update or sign-in forgets the
//! provider's report, and `fresh` asks again whatever the age.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tm_harness::{HARNESSES, ProviderReport, provider_report};

/// A report this young is used without asking again.
pub const FRESH: Duration = Duration::from_secs(60);
/// A report this old is not used at all.
pub const KEEP: Duration = Duration::from_secs(30 * 60);

struct Slot {
    at: Instant,
    report: ProviderReport,
    refreshing: bool,
}

#[derive(Clone, Default)]
pub struct ProviderCache {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

impl ProviderCache {
    /// One harness's report, from the cache when it can be.
    pub async fn report(&self, id: &str) -> ProviderReport {
        let cached = {
            let mut slots = self.slots.lock().unwrap();
            match slots.get_mut(id) {
                Some(slot) if slot.at.elapsed() < FRESH => return slot.report.clone(),
                Some(slot) if slot.at.elapsed() < KEEP => {
                    let start = !slot.refreshing;
                    slot.refreshing = true;
                    Some((slot.report.clone(), start))
                }
                _ => None,
            }
        };
        match cached {
            Some((report, start)) => {
                if start {
                    let me = self.clone();
                    let id = id.to_string();
                    tokio::spawn(async move {
                        me.fetch(&id).await;
                    });
                }
                report
            }
            None => self.fetch(id).await,
        }
    }

    /// Ask the harness now, and keep the answer unless it was a failure to list.
    pub async fn fetch(&self, id: &str) -> ProviderReport {
        let report = provider_report(id).await;
        let mut slots = self.slots.lock().unwrap();
        if report.installed && report.models.is_empty() && report.error.is_some() {
            // A listing that failed is asked again next time rather than kept.
            slots.remove(id);
        } else {
            slots.insert(id.to_string(), Slot { at: Instant::now(), report: report.clone(), refreshing: false });
        }
        report
    }

    /// Every harness's report, in [`HARNESSES`] order, asked at the same time.
    pub async fn reports(&self, fresh: bool) -> Vec<ProviderReport> {
        let asks = HARNESSES.iter().map(|id| async move { if fresh { self.fetch(id).await } else { self.report(id).await } });
        futures_util::future::join_all(asks).await
    }

    /// Forget one harness's report: it was installed, updated or signed in to.
    pub fn forget(&self, id: &str) {
        self.slots.lock().unwrap().remove(id);
    }

    #[cfg(test)]
    fn put(&self, report: ProviderReport, age: Duration) {
        let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        self.slots.lock().unwrap().insert(report.id.clone(), Slot { at, report, refreshing: false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(id: &str, model: &str) -> ProviderReport {
        ProviderReport {
            id: id.into(),
            name: id.into(),
            installed: true,
            install: String::new(),
            login: String::new(),
            models: vec![serde_json::from_value(serde_json::json!({ "id": model, "name": model })).unwrap()],
            error: None,
        }
    }

    #[tokio::test]
    async fn a_young_report_is_used_and_a_forgotten_one_is_asked_again() {
        let cache = ProviderCache::default();
        cache.put(fake("nope-harness", "m1"), Duration::from_secs(1));
        assert_eq!(cache.report("nope-harness").await.models.len(), 1, "a report under a minute old is used as is");
        cache.put(fake("nope-harness", "m2"), FRESH + Duration::from_secs(1));
        let stale = cache.report("nope-harness").await;
        assert_eq!(stale.models.len(), 1, "an older one is still answered at once, and refreshed behind it");
        cache.forget("nope-harness");
        let asked = cache.report("nope-harness").await;
        assert!(asked.error.is_some(), "a forgotten report is asked for again: {asked:?}");
    }
}
