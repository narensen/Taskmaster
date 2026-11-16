//! Who may talk to the API.
//!
//! [`Auth`] is the seam, and there are two ways through it. [`PairingAuth`]: the daemon
//! prints a code, a device posts it once and gets a token. [`AccountAuth`]: a sign-in
//! service the daemon trusts issues a signed token, and the daemon checks the signature.
//! [`AnyAuth`] accepts either, which is how accounts are added without locking out the
//! devices already paired.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

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

/// Accounts. A sign-in service issues a token signed with a secret this daemon shares
/// (a JWT, HS256). Nothing is stored here: the signature and the expiry are the check.
pub struct AccountAuth {
    secret: Vec<u8>,
    /// The `iss` a token must carry, and where a frontend sends people to sign in.
    issuer: String,
}

impl AccountAuth {
    pub fn new(issuer: &str, secret: &str) -> Self {
        AccountAuth { secret: secret.as_bytes().to_vec(), issuer: issuer.to_string() }
    }

    fn claims(&self, token: &str) -> Option<Value> {
        let mut parts = token.split('.');
        let (head, body, sign) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let header: Value = serde_json::from_slice(&unbase64(head)?).ok()?;
        // Only the one algorithm: a token that names another, or none, is refused.
        if header["alg"] != "HS256" {
            return None;
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).ok()?;
        mac.update(format!("{head}.{body}").as_bytes());
        mac.verify_slice(&unbase64(sign)?).ok()?;
        let claims: Value = serde_json::from_slice(&unbase64(body)?).ok()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        if claims["exp"].as_u64()? <= now || claims["iss"].as_str()? != self.issuer {
            return None;
        }
        Some(claims)
    }
}

impl Auth for AccountAuth {
    fn identify(&self, token: &str) -> Option<Principal> {
        let claims = self.claims(token)?;
        Some(Principal { id: claims["sub"].as_str().filter(|sub| !sub.is_empty())?.to_string() })
    }

    /// The credential is the token the sign-in service issued. A good one is the bearer
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

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64 as tokens carry it: the URL alphabet, no padding.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut bits, mut have) = (0u32, 0u8);
    for byte in text.trim_end_matches('=').bytes() {
        let value = BASE64URL.iter().position(|c| *c == byte)? as u32;
        bits = (bits << 6) | value;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
            bits &= (1 << have) - 1;
        }
    }
    Some(out)
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

#[cfg(test)]
pub(crate) fn sign(secret: &str, claims: &Value) -> String {
    fn base64(bytes: &[u8]) -> String {
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk.iter().fold(0u32, |acc, b| (acc << 8) | *b as u32) << (8 * (3 - chunk.len()));
            for i in 0..=chunk.len() {
                out.push(BASE64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }
    let head = base64(br#"{"alg":"HS256","typ":"JWT"}"#);
    let body = base64(claims.to_string().as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{head}.{body}").as_bytes());
    format!("{head}.{body}.{}", base64(&mac.finalize().into_bytes()))
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

    #[test]
    fn an_account_token_is_checked_by_signature_issuer_and_expiry() {
        let auth = AccountAuth::new("https://auth.test", "a-long-shared-secret");
        let later = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
        let good = sign("a-long-shared-secret", &json!({ "sub": "naren", "iss": "https://auth.test", "exp": later }));
        assert_eq!(auth.identify(&good), Some(Principal { id: "naren".into() }));
        assert_eq!(auth.enroll(&good).unwrap().as_deref(), Some(good.as_str()));

        let forged = sign("another-secret", &json!({ "sub": "naren", "iss": "https://auth.test", "exp": later }));
        let expired = sign("a-long-shared-secret", &json!({ "sub": "naren", "iss": "https://auth.test", "exp": 10 }));
        let elsewhere = sign("a-long-shared-secret", &json!({ "sub": "naren", "iss": "https://other.test", "exp": later }));
        // The same claims with the algorithm switched off and the signature removed.
        let unsigned = format!("eyJhbGciOiJub25lIn0.{}.", good.split('.').nth(1).unwrap());
        for bad in [forged, expired, elsewhere, unsigned, "not.a.token".into(), String::new()] {
            assert_eq!(auth.identify(&bad), None, "{bad}");
        }
    }

    #[test]
    fn either_way_in_is_accepted_when_both_are_configured() {
        let (records, dir) = records("any");
        let pairing = Arc::new(PairingAuth::new(records).unwrap());
        let code = pairing.code().to_string();
        let any = AnyAuth(vec![pairing, Arc::new(AccountAuth::new("https://auth.test", "a-long-shared-secret"))]);
        let later = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 600;
        let account = sign("a-long-shared-secret", &json!({ "sub": "naren", "iss": "https://auth.test", "exp": later }));
        let device = any.enroll(&code).unwrap().unwrap();
        assert!(any.identify(&device).is_some() && any.identify(&account).is_some());
        assert!(any.identify("nope").is_none());
        assert_eq!(any.describe()["methods"], json!(["pairing", "account"]));
        assert_eq!(any.describe()["sign_in"], "https://auth.test");
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
