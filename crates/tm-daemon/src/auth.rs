//! Who may talk to the API.
//!
//! [`Auth`] is the seam. [`PairingAuth`] is what exists today: the daemon prints a code,
//! a device posts it once and gets a token. Accounts, when they come, are another
//! implementation: `enroll` takes whatever the sign-in returns, and `identify` says whose
//! token it is. The API asks nothing else of it.

use std::collections::BTreeSet;
use std::sync::Mutex;

use crate::secret::{fingerprint, random_bytes, random_hex};
use crate::store::Records;

/// Wrong codes accepted before pairing is closed until the daemon restarts.
const MAX_BAD_CODES: u32 = 8;

/// The caller behind a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable for one token. With accounts this becomes the user id.
    pub id: String,
}

pub trait Auth: Send + Sync {
    /// Whose token this is. `None` refuses the request.
    fn identify(&self, token: &str) -> Option<Principal>;

    /// Trade a credential for a token. `Ok(None)` means the credential was wrong.
    fn enroll(&self, credential: &str) -> anyhow::Result<Option<String>>;

    /// What to show the person at this machine so they can enroll a device.
    fn invitation(&self) -> Option<String> {
        None
    }
}

/// A printed code, one token per device. Only token hashes are stored.
pub struct PairingAuth {
    code: String,
    records: Records,
    devices: Mutex<BTreeSet<String>>,
    bad_codes: Mutex<u32>,
}

impl PairingAuth {
    /// Devices paired before a restart stay paired.
    pub fn new(records: Records) -> anyhow::Result<Self> {
        let devices = records.devices()?.into_iter().collect();
        Ok(PairingAuth { code: pairing_code()?, records, devices: Mutex::new(devices), bad_codes: Mutex::new(0) })
    }

    pub fn code(&self) -> &str {
        &self.code
    }
}

impl Auth for PairingAuth {
    fn identify(&self, token: &str) -> Option<Principal> {
        let hash = fingerprint(token);
        let known = self.devices.lock().unwrap().contains(&hash);
        known.then(|| Principal { id: hash[..12].to_string() })
    }

    fn enroll(&self, credential: &str) -> anyhow::Result<Option<String>> {
        let mut bad = self.bad_codes.lock().unwrap();
        if *bad >= MAX_BAD_CODES {
            anyhow::bail!("too many wrong codes; restart the daemon for a new one");
        }
        if !same(credential.trim().to_uppercase().as_bytes(), self.code.as_bytes()) {
            *bad += 1;
            return Ok(None);
        }
        let token = random_hex(32)?;
        let mut devices = self.devices.lock().unwrap();
        devices.insert(fingerprint(&token));
        self.records.save_devices(&devices.iter().cloned().collect::<Vec<_>>())?;
        Ok(Some(token))
    }

    fn invitation(&self) -> Option<String> {
        Some(self.code.clone())
    }
}

/// Compare without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn pairing_code() -> anyhow::Result<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    Ok(random_bytes(8)?.iter().map(|byte| ALPHABET[(byte & 31) as usize] as char).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn records(name: &str) -> (Records, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("tm-auth-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Records::new(Arc::new(crate::store::FileStore::new(&dir))), dir)
    }

    #[test]
    fn a_paired_device_survives_a_restart_and_its_token_is_not_stored() {
        let (records, dir) = records("pair");
        let auth = PairingAuth::new(records.clone()).unwrap();
        assert_eq!(auth.enroll("WRONG").unwrap(), None);
        let token = auth.enroll(&auth.code().to_lowercase()).unwrap().unwrap();
        assert!(auth.identify(&token).is_some());
        assert!(auth.identify("not-a-token").is_none());
        assert!(!std::fs::read_to_string(dir.join("devices.json")).unwrap().contains(&token));

        let restarted = PairingAuth::new(records).unwrap();
        assert_ne!(restarted.code(), auth.code());
        assert_eq!(restarted.identify(&token), auth.identify(&token));
        assert!(restarted.identify(&token).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pairing_closes_after_too_many_wrong_codes() {
        let (records, dir) = records("lock");
        let auth = PairingAuth::new(records).unwrap();
        for _ in 0..MAX_BAD_CODES {
            assert_eq!(auth.enroll("NOPE").unwrap(), None);
        }
        let closed = auth.enroll(auth.code()).unwrap_err().to_string();
        assert!(closed.contains("too many"), "{closed}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
