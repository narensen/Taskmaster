//! Who may talk to the API.
//!
//! [`Auth`] is the seam, and there are two ways through it. [`PairingAuth`]: the daemon
//! prints a code, a device posts it once and gets a token. [`AccountAuth`]: a sign-in
//! service the daemon trusts issues a signed token, and the daemon checks the signature.
//! [`AnyAuth`] accepts either, which is how accounts are added without locking out the
//! devices already paired.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::Duration;

use serde_json::{Value, json};
use tm_token::{DAEMON_AUDIENCE, Verifier};

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

    /// What a frontend needs to know to offer the right sign-in. Never a secret.
    fn describe(&self) -> Value {
        json!({ "methods": ["pairing"] })
    }
}

/// Accounts. The Commons service signs people in with GitHub or Google and hands this
/// daemon a token signed with its Ed25519 key. The daemon checks the token against the
/// key the service publishes at `/.well-known/jwks.json`, so it holds nothing that could
/// make a token. Anyone can have an account, so a token is accepted only for an account
/// on this daemon's `allow` list.
pub struct AccountAuth {
    /// The service: what a token's `iss` must be, and where to send people to sign in.
    issuer: String,
    allow: Vec<String>,
    keys: Arc<RwLock<Option<Verifier>>>,
    /// Asks the background fetch to look again, when a token names a key not seen yet.
    refetch: Mutex<Option<mpsc::Sender<()>>>,
}

impl AccountAuth {
    /// Fetches the service's keys now and every hour after, on a thread of its own.
    pub fn new(issuer: &str, allow: Vec<String>) -> Self {
        let issuer = issuer.trim_end_matches('/').to_string();
        let keys = Arc::new(RwLock::new(None));
        let (tx, rx) = mpsc::channel::<()>();
        let (url, slot, iss) = (format!("{issuer}/.well-known/jwks.json"), keys.clone(), issuer.clone());
        std::thread::spawn(move || {
            let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
            loop {
                let fetched = agent
                    .get(&url)
                    .call()
                    .map_err(anyhow::Error::from)
                    .and_then(|reply| Ok(reply.into_json::<Value>()?))
                    .and_then(|jwks| Verifier::from_jwks(&jwks, &iss, DAEMON_AUDIENCE));
                match fetched {
                    Ok(verifier) => *slot.write().unwrap() = Some(verifier),
                    Err(err) => eprintln!("taskmaster: cannot read the sign-in keys at {url}: {err:#}"),
                }
                // Wait an hour, or until asked; then no sooner than a minute from now.
                match rx.recv_timeout(Duration::from_secs(3600)) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    _ => std::thread::sleep(Duration::from_secs(60)),
                }
                while rx.try_recv().is_ok() {}
            }
        });
        AccountAuth { issuer, allow, keys, refetch: Mutex::new(Some(tx)) }
    }

    /// With keys already in hand. Nothing is fetched.
    pub fn with_keys(issuer: &str, allow: Vec<String>, verifier: Verifier) -> Self {
        AccountAuth {
            issuer: issuer.trim_end_matches('/').to_string(),
            allow,
            keys: Arc::new(RwLock::new(Some(verifier))),
            refetch: Mutex::new(None),
        }
    }
}

impl Auth for AccountAuth {
    fn identify(&self, token: &str) -> Option<Principal> {
        let claims = {
            let keys = self.keys.read().unwrap();
            let verifier = keys.as_ref();
            let claims = verifier.and_then(|v| v.verify(token));
            let unknown_key = Verifier::key_id_of(token).is_some_and(|kid| !verifier.is_some_and(|v| v.has_key(&kid)));
            if claims.is_none()
                && unknown_key
                && let Some(tx) = self.refetch.lock().unwrap().as_ref()
            {
                let _ = tx.send(());
            }
            claims?
        };
        if !claims.allowed_by(&self.allow) {
            return None;
        }
        Some(Principal { id: claims.sub })
    }

    /// The credential is the token the service handed over. A good one is the bearer
    /// token itself, so there is nothing to exchange.
    fn enroll(&self, credential: &str) -> anyhow::Result<Option<String>> {
        Ok(self.identify(credential.trim()).map(|_| credential.trim().to_string()))
    }

    fn describe(&self) -> Value {
        json!({ "methods": ["account"], "sign_in": self.issuer })
    }
}

/// Several ways in. A token is good if any of them knows it.
pub struct AnyAuth(pub Vec<Arc<dyn Auth>>);

impl Auth for AnyAuth {
    fn identify(&self, token: &str) -> Option<Principal> {
        self.0.iter().find_map(|auth| auth.identify(token))
    }

    fn enroll(&self, credential: &str) -> anyhow::Result<Option<String>> {
        let mut refused = None;
        for auth in &self.0 {
            match auth.enroll(credential) {
                Ok(Some(token)) => return Ok(Some(token)),
                Ok(None) => {}
                Err(err) => refused = Some(err),
            }
        }
        match refused {
            Some(err) => Err(err),
            None => Ok(None),
        }
    }

    fn invitation(&self) -> Option<String> {
        self.0.iter().find_map(|auth| auth.invitation())
    }

    fn describe(&self) -> Value {
        let mut methods = Vec::new();
        let mut sign_in = Value::Null;
        for auth in &self.0 {
            let described = auth.describe();
            methods.extend(described["methods"].as_array().cloned().unwrap_or_default());
            if !described["sign_in"].is_null() {
                sign_in = described["sign_in"].clone();
            }
        }
        json!({ "methods": methods, "sign_in": sign_in })
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

    fn service() -> tm_token::Signer {
        tm_token::Signer::from_seed(&tm_token::Signer::seed_from(&[5u8; 32]).unwrap()).unwrap()
    }

    fn token(signer: &tm_token::Signer, issuer: &str, login: &str, exp: u64) -> String {
        signer.sign(&tm_token::Claims {
            iss: issuer.into(),
            sub: "github:42".into(),
            aud: DAEMON_AUDIENCE.into(),
            iat: tm_token::now(),
            exp,
            login: Some(login.into()),
            email: None,
            name: None,
        })
    }

    fn accounts(allow: &[&str]) -> AccountAuth {
        let verifier = Verifier::from_jwks(&service().jwks(), "https://commons.test", DAEMON_AUDIENCE).unwrap();
        AccountAuth::with_keys("https://commons.test/", allow.iter().map(|a| a.to_string()).collect(), verifier)
    }

    #[test]
    fn an_account_token_is_checked_by_key_issuer_expiry_and_the_allow_list() {
        let signer = service();
        let later = tm_token::now() + 600;
        let auth = accounts(&["github:naren"]);
        let good = token(&signer, "https://commons.test", "naren", later);
        assert_eq!(auth.identify(&good), Some(Principal { id: "github:42".into() }));
        assert_eq!(auth.enroll(&good).unwrap().as_deref(), Some(good.as_str()));
        assert_eq!(auth.describe()["sign_in"], "https://commons.test");

        let stranger = tm_token::Signer::from_seed(&tm_token::Signer::seed_from(&[6u8; 32]).unwrap()).unwrap();
        let bad = [
            // Anyone can sign in to the service; only listed accounts get in here.
            token(&signer, "https://commons.test", "someone-else", later),
            token(&stranger, "https://commons.test", "naren", later),
            token(&signer, "https://commons.test", "naren", 10),
            token(&signer, "https://other.test", "naren", later),
            "not.a.token".into(),
            String::new(),
        ];
        for bad in bad {
            assert_eq!(auth.identify(&bad), None, "{bad}");
        }
        assert_eq!(accounts(&[]).identify(&good), None, "an empty list lets nobody in");
    }

    #[test]
    fn either_way_in_is_accepted_when_both_are_configured() {
        let (records, dir) = records("any");
        let pairing = Arc::new(PairingAuth::new(records).unwrap());
        let code = pairing.code().to_string();
        let any = AnyAuth(vec![pairing, Arc::new(accounts(&["github:naren"]))]);
        let account = token(&service(), "https://commons.test", "naren", tm_token::now() + 600);
        let device = any.enroll(&code).unwrap().unwrap();
        assert!(any.identify(&device).is_some() && any.identify(&account).is_some());
        assert!(any.identify("nope").is_none());
        assert_eq!(any.describe()["methods"], json!(["pairing", "account"]));
        assert_eq!(any.describe()["sign_in"], "https://commons.test");
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
