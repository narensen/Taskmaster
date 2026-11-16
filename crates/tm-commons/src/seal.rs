//! Values the service hands to a browser and must get back unchanged: the session and
//! the state of a sign-in in progress. Each is JSON with an HMAC, and carries its expiry.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

#[derive(Clone)]
pub struct Sealer {
    key: Vec<u8>,
}

impl Sealer {
    pub fn new(secret: &str) -> Self {
        Sealer { key: secret.as_bytes().to_vec() }
    }

    fn mac(&self, purpose: &str, data: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("any key length works");
        mac.update(purpose.as_bytes());
        mac.update(b"\0");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// `value` must have an `exp` in Unix seconds.
    pub fn seal(&self, purpose: &str, value: &Value) -> String {
        let body = URL_SAFE_NO_PAD.encode(value.to_string());
        let tag = URL_SAFE_NO_PAD.encode(self.mac(purpose, body.as_bytes()));
        format!("{body}.{tag}")
    }

    pub fn open(&self, purpose: &str, sealed: &str) -> Option<Value> {
        let (body, tag) = sealed.split_once('.')?;
        let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).ok()?;
        mac.update(purpose.as_bytes());
        mac.update(b"\0");
        mac.update(body.as_bytes());
        mac.verify_slice(&tag).ok()?;
        let value: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).ok()?).ok()?;
        (value["exp"].as_u64()? > tm_token::now()).then_some(value)
    }

    /// A form token tied to one session, so another site cannot post a form for it.
    pub fn csrf(&self, session: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.mac("csrf", session.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_sealed_value_comes_back_only_unchanged_unexpired_and_for_its_purpose() {
        let sealer = Sealer::new("0123456789abcdef0123456789abcdef");
        let sealed = sealer.seal("session", &json!({ "uid": 7, "exp": tm_token::now() + 60 }));
        assert_eq!(sealer.open("session", &sealed).unwrap()["uid"], 7);
        assert!(sealer.open("oauth", &sealed).is_none());
        assert!(Sealer::new("another secret, just as long as it").open("session", &sealed).is_none());
        let (_, tag) = sealed.split_once('.').unwrap();
        let forged = URL_SAFE_NO_PAD.encode(json!({ "uid": 1, "exp": tm_token::now() + 60 }).to_string());
        assert!(sealer.open("session", &format!("{forged}.{tag}")).is_none());
        let old = sealer.seal("session", &json!({ "uid": 7, "exp": tm_token::now() - 1 }));
        assert!(sealer.open("session", &old).is_none());
        assert_ne!(sealer.csrf("a"), sealer.csrf("b"));
    }
}
