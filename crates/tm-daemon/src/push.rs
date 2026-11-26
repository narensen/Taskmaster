//! Notifications that reach a phone: Web Push and ntfy.
//!
//! **Web Push** (RFC 8030, encrypted as RFC 8291 `aes128gcm`, signed with a VAPID key as
//! RFC 8292) reaches a browser or a home-screen app that subscribed. Browsers only offer it on
//! HTTPS (or `localhost`), so a phone on a plain `http://192.168…` address cannot subscribe.
//!
//! **ntfy** works from anywhere: the person installs the ntfy app and subscribes to a topic only
//! this daemon knows, and the daemon posts each notification there. Nothing is sent until they
//! turn it on, and a notification carries only the run's name and what it waits for.
//!
//! Everything is kept in one file (`push.json`, mode `0600`): the VAPID key, the subscriptions
//! and the ntfy topic.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The public ntfy server, used unless the person names their own.
pub const NTFY_SERVER: &str = "https://ntfy.sh";

/// One notification, as every channel sends it.
#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// The page it opens, such as `/runs/<id>`.
    pub path: String,
    /// Notifications with one tag replace each other (one per run).
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subscription {
    pub endpoint: String,
    /// The browser's P-256 public key, base64url.
    pub p256dh: String,
    /// The browser's 16-byte auth secret, base64url.
    pub auth: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub added: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ntfy {
    pub server: String,
    pub topic: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    /// The VAPID signing key, PKCS#8, base64url.
    vapid: String,
    subscriptions: Vec<Subscription>,
    ntfy: Option<Ntfy>,
}

/// What a send reached.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Sent {
    pub web: usize,
    pub ntfy: bool,
    pub errors: Vec<String>,
}

pub struct Push {
    path: PathBuf,
    saved: Mutex<Saved>,
}

impl Push {
    /// Open (or start) the store at `path`, making a VAPID key the first time.
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Push> {
        let path = path.into();
        let mut saved: Saved = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let fresh = B64.decode(&saved.vapid).ok().and_then(|der| EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new()).ok()).is_none();
        let push = Push { path, saved: Mutex::new(Saved::default()) };
        if fresh {
            let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new()).map_err(|_| anyhow::anyhow!("could not make a VAPID key"))?;
            saved.vapid = B64.encode(der.as_ref());
            *push.saved.lock().unwrap() = saved;
            push.save()?;
        } else {
            *push.saved.lock().unwrap() = saved;
        }
        Ok(push)
    }

    fn save(&self) -> anyhow::Result<()> {
        let saved = self.saved.lock().unwrap();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&*saved)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn key(&self) -> anyhow::Result<EcdsaKeyPair> {
        let der = B64.decode(&self.saved.lock().unwrap().vapid)?;
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &der, &SystemRandom::new()).map_err(|_| anyhow::anyhow!("the VAPID key is unreadable"))
    }

    /// The VAPID public key a browser subscribes with (uncompressed P-256, base64url).
    pub fn public_key(&self) -> String {
        self.key().map(|key| B64.encode(key.public_key().as_ref())).unwrap_or_default()
    }

    /// What the person sees: the subscriptions (by label and push service) and ntfy.
    pub fn view(&self) -> Value {
        let (subscriptions, ntfy) = {
            let saved = self.saved.lock().unwrap();
            let subscriptions: Vec<Value> = saved
                .subscriptions
                .iter()
                .map(|s| json!({ "endpoint": s.endpoint, "service": service_of(&s.endpoint), "label": s.label, "added": s.added }))
                .collect();
            let ntfy = saved.ntfy.as_ref().map(|n| {
                let host = n.server.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
                json!({ "server": n.server, "topic": n.topic, "web_url": format!("{}/{}", n.server.trim_end_matches('/'), n.topic), "app_url": format!("ntfy://{host}/{}", n.topic) })
            });
            (subscriptions, ntfy)
        };
        json!({ "public_key": self.public_key(), "subscriptions": subscriptions, "ntfy": ntfy })
    }

    pub fn subscribe(&self, mut sub: Subscription) -> anyhow::Result<()> {
        check_endpoint(&sub.endpoint)?;
        let key = B64.decode(sub.p256dh.trim_end_matches('=')).context("p256dh is not base64url")?;
        let auth = B64.decode(sub.auth.trim_end_matches('=')).context("auth is not base64url")?;
        if key.len() != 65 || key[0] != 4 || auth.len() != 16 {
            bail!("this is not a Web Push subscription");
        }
        sub.label = sub.label.chars().filter(|c| !c.is_control()).take(60).collect();
        if sub.added.is_empty() {
            sub.added = crate::time::rfc3339_now();
        }
        {
            let mut saved = self.saved.lock().unwrap();
            saved.subscriptions.retain(|have| have.endpoint != sub.endpoint);
            saved.subscriptions.push(sub);
        }
        self.save()
    }

    pub fn unsubscribe(&self, endpoint: &str) -> anyhow::Result<bool> {
        let gone = {
            let mut saved = self.saved.lock().unwrap();
            let before = saved.subscriptions.len();
            saved.subscriptions.retain(|have| have.endpoint != endpoint);
            saved.subscriptions.len() != before
        };
        self.save()?;
        Ok(gone)
    }

    /// Turn ntfy on with a new private topic (or the server named), or off.
    pub fn set_ntfy(&self, on: bool, server: Option<&str>) -> anyhow::Result<Option<Ntfy>> {
        let next = if on {
            let server = server.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(NTFY_SERVER).trim_end_matches('/').to_string();
            if !(server.starts_with("https://") || server.starts_with("http://127.0.0.1") || server.starts_with("http://localhost")) {
                bail!("the ntfy server must start with https://");
            }
            let mut bytes = [0u8; 12];
            SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("no randomness"))?;
            // The topic is the secret: anyone who knows it can read and post.
            Some(Ntfy { server, topic: format!("tm-{}", B64.encode(bytes).replace(['-', '_'], "x")) })
        } else {
            None
        };
        self.saved.lock().unwrap().ntfy = next.clone();
        self.save()?;
        Ok(next)
    }

    /// Send `note` to every subscription and to ntfy. `base` is the address a phone opens
    /// the page at (ntfy needs a whole URL; a browser opens the path on its own origin).
    pub async fn send(&self, note: &Note, base: &str) -> Sent {
        let (subs, ntfy) = {
            let saved = self.saved.lock().unwrap();
            (saved.subscriptions.clone(), saved.ntfy.clone())
        };
        let mut sent = Sent::default();
        if !subs.is_empty() {
            match self.key() {
                Ok(key) => {
                    let payload = json!({ "title": note.title, "body": note.body, "url": note.path, "tag": note.tag }).to_string();
                    let mut gone = Vec::new();
                    for sub in subs {
                        match web_push(&key, &sub, payload.as_bytes()).await {
                            Ok(Delivery::Delivered) => sent.web += 1,
                            Ok(Delivery::Gone) => gone.push(sub.endpoint),
                            Err(err) => sent.errors.push(format!("{}: {err:#}", service_of(&sub.endpoint))),
                        }
                    }
                    for endpoint in gone {
                        let _ = self.unsubscribe(&endpoint);
                    }
                }
                Err(err) => sent.errors.push(format!("{err:#}")),
            }
        }
        if let Some(ntfy) = ntfy {
            let url = format!("{}/{}", ntfy.server, ntfy.topic);
            let click = format!("{}{}", base.trim_end_matches('/'), note.path);
            let (title, body, tag) = (note.title.clone(), note.body.clone(), note.tag.clone());
            #[allow(clippy::result_large_err)] // ureq::Error is large by design; result is consumed right here
            let posted = tokio::task::spawn_blocking(move || {
                ureq::AgentBuilder::new()
                    .timeout(Duration::from_secs(8))
                    .build()
                    .post(&url)
                    .set("Title", &ascii(&title))
                    .set("Click", &click)
                    .set("Tags", "taskmaster")
                    .set("X-Sequence-Id", &tag)
                    .send_string(&body)
            })
            .await;
            match posted {
                Ok(Ok(_)) => sent.ntfy = true,
                Ok(Err(err)) => sent.errors.push(format!("ntfy: {err}")),
                Err(err) => sent.errors.push(format!("ntfy: {err}")),
            }
        }
        sent
    }
}

/// Header values must be ASCII; a run name with other letters keeps what it can.
fn ascii(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii() && !c.is_ascii_control() { c } else { '?' }).collect()
}

/// A push service's name, for the list: `fcm.googleapis.com`.
fn service_of(endpoint: &str) -> String {
    endpoint.trim_start_matches("https://").trim_start_matches("http://").split('/').next().unwrap_or("").to_string()
}

/// Only HTTPS push services; plain HTTP only on this machine (tests).
fn check_endpoint(endpoint: &str) -> anyhow::Result<()> {
    let ok = endpoint.starts_with("https://") || endpoint.starts_with("http://127.0.0.1:") || endpoint.starts_with("http://localhost:");
    if !ok || endpoint.len() > 2048 {
        bail!("a push endpoint must be an https:// URL");
    }
    Ok(())
}

enum Delivery {
    Delivered,
    /// The browser unsubscribed: forget it.
    Gone,
}

async fn web_push(key: &EcdsaKeyPair, sub: &Subscription, payload: &[u8]) -> anyhow::Result<Delivery> {
    let body = encrypt(payload, &sub.p256dh, &sub.auth)?;
    let auth = vapid(key, &sub.endpoint)?;
    let endpoint = sub.endpoint.clone();
    #[allow(clippy::result_large_err)] // ureq::Error is large by design; result is consumed right here
    let answer = tokio::task::spawn_blocking(move || {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()
            .post(&endpoint)
            .set("TTL", "86400")
            .set("Urgency", "high")
            .set("Content-Encoding", "aes128gcm")
            .set("Content-Type", "application/octet-stream")
            .set("Authorization", &auth)
            .send_bytes(&body)
    })
    .await?;
    match answer {
        Ok(_) => Ok(Delivery::Delivered),
        Err(ureq::Error::Status(404 | 410, _)) => Ok(Delivery::Gone),
        Err(ureq::Error::Status(code, _)) => bail!("the push service answered {code}"),
        Err(err) => bail!("{err}"),
    }
}

/// The `Authorization: vapid t=…, k=…` header for `endpoint` (RFC 8292).
fn vapid(key: &EcdsaKeyPair, endpoint: &str) -> anyhow::Result<String> {
    let rest = endpoint.split_once("://").context("the endpoint has no scheme")?;
    let audience = format!("{}://{}", rest.0, rest.1.split('/').next().unwrap_or(""));
    let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() + 12 * 3600;
    let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(json!({ "aud": audience, "exp": exp, "sub": "https://localhost" }).to_string());
    let signing = format!("{header}.{claims}");
    let signature = key.sign(&SystemRandom::new(), signing.as_bytes()).map_err(|_| anyhow::anyhow!("could not sign"))?;
    Ok(format!("vapid t={signing}.{}, k={}", B64.encode(signature.as_ref()), B64.encode(key.public_key().as_ref())))
}

struct Len(usize);

impl ring::hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> anyhow::Result<Vec<u8>> {
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt).extract(ikm);
    let mut out = vec![0u8; len];
    prk.expand(&[info], Len(len)).and_then(|okm| okm.fill(&mut out)).map_err(|_| anyhow::anyhow!("hkdf failed"))?;
    Ok(out)
}

/// The keys both sides derive: content key and nonce (RFC 8291 §3.3–3.4, RFC 8188).
fn derive(ecdh: &[u8], auth: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public);
    let ikm = hkdf(auth, ecdh, &info, 32)?;
    let cek = hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf(salt, &ikm, b"Content-Encoding: nonce\0", 12)?;
    Ok((cek, nonce))
}

/// Encrypt `payload` for one subscription, as one `aes128gcm` record.
fn encrypt(payload: &[u8], p256dh: &str, auth: &str) -> anyhow::Result<Vec<u8>> {
    use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
    let ua_public = B64.decode(p256dh.trim_end_matches('='))?;
    let auth = B64.decode(auth.trim_end_matches('='))?;
    let rng = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&ECDH_P256, &rng).map_err(|_| anyhow::anyhow!("no key"))?;
    let as_public = private.compute_public_key().map_err(|_| anyhow::anyhow!("no key"))?.as_ref().to_vec();
    let ecdh = agree_ephemeral(private, &UnparsedPublicKey::new(&ECDH_P256, &ua_public), |secret| secret.to_vec())
        .map_err(|_| anyhow::anyhow!("the browser's key is not a P-256 point"))?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt).map_err(|_| anyhow::anyhow!("no randomness"))?;
    let (cek, nonce) = derive(&ecdh, &auth, &ua_public, &as_public, &salt)?;
    let sealing = ring::aead::LessSafeKey::new(ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek).map_err(|_| anyhow::anyhow!("bad key"))?);
    let mut record = payload.to_vec();
    record.push(2); // the last (and only) record
    let nonce = ring::aead::Nonce::try_assume_unique_for_key(&nonce).map_err(|_| anyhow::anyhow!("bad nonce"))?;
    sealing.seal_in_place_append_tag(nonce, ring::aead::Aad::empty(), &mut record).map_err(|_| anyhow::anyhow!("could not encrypt"))?;
    let mut body = Vec::with_capacity(86 + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&record);
    Ok(body)
}

/// Where the store lives for a daemon: beside the hosts file, in `~/.taskmaster`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".taskmaster/push.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};

    /// A browser's side: decrypt what [`encrypt`] made with its private key and auth secret.
    fn decrypt(body: &[u8], ua: EphemeralPrivateKey, ua_public: &[u8], auth: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 4096);
        let len = body[20] as usize;
        let as_public = &body[21..21 + len];
        let ecdh = agree_ephemeral(ua, &UnparsedPublicKey::new(&ECDH_P256, as_public), |s| s.to_vec()).unwrap();
        let (cek, nonce) = derive(&ecdh, auth, ua_public, as_public, salt).unwrap();
        let key = ring::aead::LessSafeKey::new(ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek).unwrap());
        let mut record = body[21 + len..].to_vec();
        let plain = key.open_in_place(ring::aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(), ring::aead::Aad::empty(), &mut record).unwrap();
        assert_eq!(plain.last(), Some(&2), "one last record");
        plain[..plain.len() - 1].to_vec()
    }

    #[test]
    fn a_browser_reads_what_the_daemon_encrypts_for_it() {
        let rng = SystemRandom::new();
        let ua = EphemeralPrivateKey::generate(&ECDH_P256, &rng).unwrap();
        let ua_public = ua.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        let body = encrypt(br#"{"title":"hi"}"#, &B64.encode(&ua_public), &B64.encode(auth)).unwrap();
        assert_eq!(decrypt(&body, ua, &ua_public, &auth), br#"{"title":"hi"}"#);
    }

    #[test]
    fn the_vapid_header_is_a_signed_token_for_the_push_service() {
        let dir = std::env::temp_dir().join(format!("tm-push-vapid-{}", std::process::id()));
        let push = Push::open(dir.join("push.json")).unwrap();
        let header = vapid(&push.key().unwrap(), "https://fcm.googleapis.com/fcm/send/abc").unwrap();
        let (token, key) = header.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(key, push.public_key());
        let parts: Vec<&str> = token.split('.').collect();
        let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        let public = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, B64.decode(key).unwrap());
        public.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &B64.decode(parts[2]).unwrap()).expect("the signature checks out");
        // The key is kept: opening again gives the same one, and the file is this user's alone.
        assert_eq!(Push::open(dir.join("push.json")).unwrap().public_key(), push.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join("push.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_real_subscriptions_are_kept() {
        let dir = std::env::temp_dir().join(format!("tm-push-subs-{}", std::process::id()));
        let push = Push::open(dir.join("push.json")).unwrap();
        let rng = SystemRandom::new();
        let ua = EphemeralPrivateKey::generate(&ECDH_P256, &rng).unwrap();
        let good = Subscription {
            endpoint: "https://web.push.apple.com/x".into(),
            p256dh: B64.encode(ua.compute_public_key().unwrap().as_ref()),
            auth: B64.encode([7u8; 16]),
            label: "phone\u{7}".into(),
            added: String::new(),
        };
        assert!(push.subscribe(Subscription { endpoint: "http://evil.example/x".into(), ..good.clone() }).is_err());
        assert!(push.subscribe(Subscription { auth: B64.encode([1u8; 3]), ..good.clone() }).is_err());
        push.subscribe(good.clone()).unwrap();
        push.subscribe(good.clone()).unwrap();
        let view = push.view();
        assert_eq!(view["subscriptions"].as_array().unwrap().len(), 1, "one entry per endpoint");
        assert_eq!(view["subscriptions"][0]["label"], "phone");
        assert_eq!(view["subscriptions"][0]["service"], "web.push.apple.com");
        assert!(push.unsubscribe(&good.endpoint).unwrap());
        let ntfy = push.set_ntfy(true, None).unwrap().unwrap();
        assert!(ntfy.topic.starts_with("tm-") && ntfy.topic.len() > 12);
        assert_eq!(push.view()["ntfy"]["app_url"], format!("ntfy://ntfy.sh/{}", ntfy.topic));
        assert!(push.set_ntfy(true, Some("http://evil.example")).is_err());
        assert!(push.set_ntfy(false, None).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
