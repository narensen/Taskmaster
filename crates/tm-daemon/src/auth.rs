//! Who may talk to the API.
//!
//! [`Auth`] is the seam, and there are two ways through it. [`PairingAuth`]: the daemon
//! prints a code, a device posts it once and gets a token. [`AccountAuth`]: a sign-in
//! service the daemon trusts issues a signed token, and the daemon checks the signature.
//! [`AnyAuth`] accepts either, which is how accounts are added without locking out the
//! devices already paired.
//!
//! Pairing also makes one-time offers: a longer code, good once and for a few minutes,
//! carried in a link (`/pair#code=…`) or a QR code, so a phone or another computer signs in
//! without typing anything. Each paired device has a name, a kind and a last-seen time,
//! and can be renamed or revoked; a revoked token stops working at once.

use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tm_token::{DAEMON_AUDIENCE, Verifier};

use crate::secret::{fingerprint, random_bytes, random_hex};
use crate::store::Records;

/// Wrong codes accepted before pairing is closed until the daemon restarts.
const MAX_BAD_CODES: u32 = 8;
/// How long a one-time pairing offer is good for, unless the person asks for another time.
pub const OFFER_TTL: Duration = Duration::from_secs(10 * 60);
/// Offers waiting at once. An old one goes when a new one would pass this.
const MAX_OFFERS: usize = 16;
/// Last-seen times are written at most this often.
const SEEN_FLUSH: Duration = Duration::from_secs(5 * 60);

/// The kinds of device a person can pair. Anything else is kept as `browser`.
pub const DEVICE_KINDS: [&str; 5] = ["browser", "phone", "desktop", "machine", "agent"];

fn kind_of(kind: &str) -> String {
    let kind = kind.trim().to_ascii_lowercase();
    if DEVICE_KINDS.contains(&kind.as_str()) { kind } else { "browser".into() }
}

fn label_of(label: &str, kind: &str) -> String {
    let clean: String = label.chars().filter(|c| !c.is_control()).take(60).collect();
    let clean = clean.trim();
    if !clean.is_empty() {
        return clean.to_string();
    }
    match kind {
        "phone" => "Phone",
        "desktop" => "Desktop app",
        "machine" => "Another machine",
        "agent" => "Outside agent",
        _ => "Browser",
    }
    .to_string()
}

/// One paired device as the daemon keeps it. Only the token's hash is stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceRecord {
    pub hash: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub last_seen: Option<String>,
}

impl DeviceRecord {
    /// The first 12 characters of the hash: stable, shown, and of no use as a credential.
    pub fn id(&self) -> String {
        self.hash.chars().take(12).collect()
    }

    pub fn view(&self) -> DeviceView {
        DeviceView { id: self.id(), label: self.label.clone(), kind: self.kind.clone(), created: self.created.clone(), last_seen: self.last_seen.clone() }
    }
}

/// A paired device as the API shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceView {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub created: String,
    pub last_seen: Option<String>,
}

/// A one-time pairing code with the name and kind the device will get.
#[derive(Debug, Clone)]
pub struct Offer {
    pub code: String,
    pub label: String,
    pub kind: String,
    /// RFC 3339, for the person.
    pub expires_at: String,
    expires: Instant,
}

/// The caller behind a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Stable for one token. With accounts this becomes the user id.
    pub id: String,
    /// The paired device's kind, or `account`. An `agent` token may use only `/mcp`.
    pub kind: String,
}

impl Principal {
    /// An outside agent's token: it reaches the MCP server and nothing else.
    pub fn is_agent(&self) -> bool {
        self.kind == "agent"
    }
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

    /// [`Auth::enroll`], naming the device. `label` and `kind` may be empty.
    fn enroll_as(&self, credential: &str, _label: &str, _kind: &str) -> anyhow::Result<Option<String>> {
        self.enroll(credential)
    }

    /// A one-time code good for `ttl`. `None` when this way in has no pairing.
    fn offer(&self, _label: &str, _kind: &str, _ttl: Duration) -> anyhow::Result<Option<Offer>> {
        Ok(None)
    }

    /// A token for a device that cannot open a link, such as an outside agent, made for a
    /// person who is already signed in. `None` when this way in cannot make one.
    fn mint(&self, _label: &str, _kind: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    fn devices(&self) -> Vec<DeviceView> {
        Vec::new()
    }

    /// Forget a device; its token stops working at once. False when there is no such device.
    fn revoke(&self, _id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn rename(&self, _id: &str, _label: &str) -> anyhow::Result<bool> {
        Ok(false)
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
        Some(Principal { id: claims.sub, kind: "account".into() })
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

    fn enroll_as(&self, credential: &str, label: &str, kind: &str) -> anyhow::Result<Option<String>> {
        let mut refused = None;
        for auth in &self.0 {
            match auth.enroll_as(credential, label, kind) {
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

    fn offer(&self, label: &str, kind: &str, ttl: Duration) -> anyhow::Result<Option<Offer>> {
        for auth in &self.0 {
            if let Some(offer) = auth.offer(label, kind, ttl)? {
                return Ok(Some(offer));
            }
        }
        Ok(None)
    }

    fn mint(&self, label: &str, kind: &str) -> anyhow::Result<Option<String>> {
        for auth in &self.0 {
            if let Some(token) = auth.mint(label, kind)? {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    fn devices(&self) -> Vec<DeviceView> {
        self.0.iter().flat_map(|auth| auth.devices()).collect()
    }

    fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        for auth in &self.0 {
            if auth.revoke(id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn rename(&self, id: &str, label: &str) -> anyhow::Result<bool> {
        for auth in &self.0 {
            if auth.rename(id, label)? {
                return Ok(true);
            }
        }
        Ok(false)
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

/// A printed code, and one-time offers; one token per device. Only token hashes are stored.
pub struct PairingAuth {
    code: String,
    records: Records,
    devices: Mutex<Vec<DeviceRecord>>,
    offers: Mutex<Vec<Offer>>,
    bad_codes: Mutex<u32>,
    /// When last-seen times were last written.
    flushed: Mutex<Instant>,
}

impl PairingAuth {
    /// Devices paired before a restart stay paired.
    pub fn new(records: Records) -> anyhow::Result<Self> {
        let devices = records.devices()?;
        Ok(PairingAuth {
            code: pairing_code(8)?,
            records,
            devices: Mutex::new(devices),
            offers: Mutex::new(Vec::new()),
            bad_codes: Mutex::new(0),
            flushed: Mutex::new(Instant::now()),
        })
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// Add a device for a fresh token, and keep the list.
    fn add(&self, label: &str, kind: &str) -> anyhow::Result<String> {
        let token = random_hex(32)?;
        let kind = kind_of(kind);
        let record = DeviceRecord {
            hash: fingerprint(&token),
            label: label_of(label, &kind),
            kind,
            created: crate::time::rfc3339_now(),
            last_seen: Some(crate::time::rfc3339_now()),
        };
        let mut devices = self.devices.lock().unwrap();
        devices.push(record);
        self.records.save_devices(&devices)?;
        Ok(token)
    }
}

impl Auth for PairingAuth {
    fn identify(&self, token: &str) -> Option<Principal> {
        let hash = fingerprint(token);
        let mut devices = self.devices.lock().unwrap();
        let device = devices.iter_mut().find(|device| device.hash == hash)?;
        let id = device.id();
        let kind = device.kind.clone();
        device.last_seen = Some(crate::time::rfc3339_now());
        // Last-seen is a hint: it is written now and then, not on every request.
        let mut flushed = self.flushed.lock().unwrap();
        if flushed.elapsed() >= SEEN_FLUSH {
            *flushed = Instant::now();
            let _ = self.records.save_devices(&devices);
        }
        Some(Principal { id, kind })
    }

    fn enroll(&self, credential: &str) -> anyhow::Result<Option<String>> {
        self.enroll_as(credential, "", "")
    }

    fn enroll_as(&self, credential: &str, label: &str, kind: &str) -> anyhow::Result<Option<String>> {
        let mut bad = self.bad_codes.lock().unwrap();
        if *bad >= MAX_BAD_CODES {
            anyhow::bail!("too many wrong codes; restart the daemon for a new one");
        }
        let given = credential.trim().to_uppercase();
        if same(given.as_bytes(), self.code.as_bytes()) {
            drop(bad);
            return self.add(label, kind).map(Some);
        }
        let offer = {
            let mut offers = self.offers.lock().unwrap();
            offers.retain(|offer| offer.expires > Instant::now());
            // Check every offer, so the time taken does not say which one was close.
            let mut found = None;
            for (index, offer) in offers.iter().enumerate() {
                if same(given.as_bytes(), offer.code.as_bytes()) {
                    found = Some(index);
                }
            }
            found.map(|index| offers.remove(index))
        };
        let Some(offer) = offer else {
            *bad += 1;
            return Ok(None);
        };
        drop(bad);
        // The name the person gave when making the link wins over the device's own guess.
        let label = if offer.label.trim().is_empty() { label.to_string() } else { offer.label.clone() };
        let kind = if offer.kind.trim().is_empty() { kind.to_string() } else { offer.kind.clone() };
        self.add(&label, &kind).map(Some)
    }

    fn invitation(&self) -> Option<String> {
        Some(self.code.clone())
    }

    fn offer(&self, label: &str, kind: &str, ttl: Duration) -> anyhow::Result<Option<Offer>> {
        let ttl = ttl.clamp(Duration::from_secs(60), Duration::from_secs(24 * 3600));
        let expires_at = crate::time::rfc3339_after(ttl);
        let offer = Offer {
            code: pairing_code(16)?,
            label: if label.trim().is_empty() { String::new() } else { label_of(label, kind) },
            kind: if kind.trim().is_empty() { String::new() } else { kind_of(kind) },
            expires_at,
            expires: Instant::now() + ttl,
        };
        let mut offers = self.offers.lock().unwrap();
        offers.retain(|offer| offer.expires > Instant::now());
        if offers.len() >= MAX_OFFERS {
            offers.remove(0);
        }
        offers.push(offer.clone());
        Ok(Some(offer))
    }

    fn mint(&self, label: &str, kind: &str) -> anyhow::Result<Option<String>> {
        self.add(label, kind).map(Some)
    }

    fn devices(&self) -> Vec<DeviceView> {
        self.devices.lock().unwrap().iter().map(DeviceRecord::view).collect()
    }

    fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let mut devices = self.devices.lock().unwrap();
        let before = devices.len();
        devices.retain(|device| device.id() != id);
        if devices.len() == before {
            return Ok(false);
        }
        self.records.save_devices(&devices)?;
        Ok(true)
    }

    fn rename(&self, id: &str, label: &str) -> anyhow::Result<bool> {
        let mut devices = self.devices.lock().unwrap();
        let Some(device) = devices.iter_mut().find(|device| device.id() == id) else { return Ok(false) };
        device.label = label_of(label, &device.kind.clone());
        self.records.save_devices(&devices)?;
        Ok(true)
    }
}

/// Compare without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `len` characters a person can read aloud: no 0/O or 1/I. Five bits each.
fn pairing_code(len: usize) -> anyhow::Result<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    Ok(random_bytes(len)?.iter().map(|byte| ALPHABET[(byte & 31) as usize] as char).collect())
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
        assert_eq!(auth.identify(&good), Some(Principal { id: "github:42".into(), kind: "account".into() }));
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
    fn a_one_time_offer_pairs_once_names_the_device_and_expires() {
        let (records, dir) = records("offer");
        let auth = PairingAuth::new(records.clone()).unwrap();
        let offer = auth.offer("Naren's phone", "phone", OFFER_TTL).unwrap().unwrap();
        assert_eq!(offer.code.len(), 16);
        let token = auth.enroll_as(&offer.code.to_lowercase(), "Chrome on Android", "browser").unwrap().unwrap();
        let me = auth.identify(&token).unwrap();
        let listed = auth.devices();
        let device = listed.iter().find(|d| d.id == me.id).unwrap();
        assert_eq!((device.label.as_str(), device.kind.as_str()), ("Naren's phone", "phone"), "the link's name wins");
        assert_eq!(auth.enroll_as(&offer.code, "", "").unwrap(), None, "an offer is good once");

        // A link with no name takes the device's own.
        let plain = auth.offer("", "", OFFER_TTL).unwrap().unwrap();
        let other = auth.enroll_as(&plain.code, "Firefox on Linux", "browser").unwrap().unwrap();
        let id = auth.identify(&other).unwrap().id;
        assert_eq!(auth.devices().iter().find(|d| d.id == id).unwrap().label, "Firefox on Linux");

        // An expired offer is refused.
        let stale = auth.offer("x", "phone", OFFER_TTL).unwrap().unwrap();
        auth.offers.lock().unwrap().iter_mut().for_each(|o| o.expires = Instant::now() - Duration::from_secs(1));
        assert_eq!(auth.enroll_as(&stale.code, "", "").unwrap(), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_revoked_device_is_out_at_once_and_names_survive_a_restart() {
        let (records, dir) = records("revoke");
        let auth = PairingAuth::new(records.clone()).unwrap();
        let laptop = auth.enroll_as(auth.code(), "Laptop", "browser").unwrap().unwrap();
        let agent = auth.mint("Claude Code", "agent").unwrap().unwrap();
        let agent_id = auth.identify(&agent).unwrap().id;
        assert!(auth.rename(&agent_id, "Claude Code on the server").unwrap());
        assert!(!auth.rename("nope", "x").unwrap());

        let restarted = PairingAuth::new(records.clone()).unwrap();
        let names: Vec<(String, String)> = restarted.devices().into_iter().map(|d| (d.label, d.kind)).collect();
        assert!(names.contains(&("Claude Code on the server".into(), "agent".into())), "{names:?}");
        assert!(restarted.revoke(&agent_id).unwrap());
        assert!(restarted.identify(&agent).is_none(), "revoked at once");
        assert!(restarted.identify(&laptop).is_some(), "the others stay");
        assert!(!restarted.revoke(&agent_id).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn devices_paired_before_names_existed_stay_paired() {
        let (records, dir) = records("legacy");
        let token = "0123456789abcdef";
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("devices.json"), serde_json::to_string(&vec![fingerprint(token)]).unwrap()).unwrap();
        let auth = PairingAuth::new(records).unwrap();
        assert!(auth.identify(token).is_some());
        assert_eq!(auth.devices()[0].label, "Paired earlier");
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
