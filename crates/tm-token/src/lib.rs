//! Sign-in tokens: JWTs signed with Ed25519 (`alg: EdDSA`).
//!
//! The Commons service holds the private key and signs a token when someone signs in. It
//! publishes the public key as a JWK set at `/.well-known/jwks.json`. A daemon fetches
//! that set and checks tokens with it, so no daemon holds anything that could mint one.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The audience of a token that signs someone in to a daemon.
pub const DAEMON_AUDIENCE: &str = "taskmaster-daemon";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    /// `github:<id>` or `google:<id>`. Stable when a login or an email changes.
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    /// The GitHub username, for a GitHub account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// A verified email address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Claims {
    /// Whether an entry of an allow-list names this account. An entry is `github:<login>`,
    /// `google:<email>`, a bare email, or the `sub` itself.
    pub fn allowed_by(&self, allow: &[String]) -> bool {
        let provider = self.sub.split(':').next().unwrap_or("");
        allow.iter().map(|entry| entry.trim()).filter(|entry| !entry.is_empty()).any(|entry| {
            if entry == self.sub {
                return true;
            }
            let (kind, value) = entry.split_once(':').unwrap_or(("", entry));
            let login = self.login.as_deref().map(str::to_ascii_lowercase);
            let email = self.email.as_deref().map(str::to_ascii_lowercase);
            let value = value.to_ascii_lowercase();
            match kind {
                "github" => provider == "github" && login.as_deref() == Some(value.as_str()),
                "google" => provider == "google" && email.as_deref() == Some(value.as_str()),
                "" => email.as_deref() == Some(value.as_str()),
                _ => false,
            }
        })
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text.trim_end_matches('=')).ok()
}

fn key_id(key: &VerifyingKey) -> String {
    Sha256::digest(key.as_bytes()).iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

pub struct Signer {
    key: SigningKey,
    kid: String,
}

impl Signer {
    /// From the 32-byte seed, as base64url. `tm-commons keygen` makes one.
    pub fn from_seed(seed: &str) -> anyhow::Result<Self> {
        let bytes = unb64(seed.trim()).context("the signing key is not base64url")?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| anyhow::anyhow!("the signing key is not 32 bytes"))?;
        let key = SigningKey::from_bytes(&seed);
        let kid = key_id(&key.verifying_key());
        Ok(Signer { key, kid })
    }

    /// A new seed from 32 random bytes, as base64url.
    pub fn seed_from(random: &[u8]) -> anyhow::Result<String> {
        if random.len() != 32 {
            bail!("a seed is 32 bytes");
        }
        Ok(b64(random))
    }

    pub fn sign(&self, claims: &Claims) -> String {
        let head = b64(json!({ "alg": "EdDSA", "typ": "JWT", "kid": self.kid }).to_string().as_bytes());
        let body = b64(serde_json::to_vec(claims).unwrap_or_default().as_slice());
        let signature = self.key.sign(format!("{head}.{body}").as_bytes());
        format!("{head}.{body}.{}", b64(&signature.to_bytes()))
    }

    /// The public half, as a JWK set.
    pub fn jwks(&self) -> Value {
        let public = self.key.verifying_key();
        json!({ "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "alg": "EdDSA",
            "use": "sig",
            "kid": self.kid,
            "x": b64(public.as_bytes()),
        }] })
    }

    pub fn verifier(&self, issuer: &str, audience: &str) -> Verifier {
        Verifier { keys: vec![(self.kid.clone(), self.key.verifying_key())], issuer: issuer.into(), audience: audience.into() }
    }
}

/// Checks tokens against a set of public keys, an issuer and an audience.
#[derive(Clone)]
pub struct Verifier {
    keys: Vec<(String, VerifyingKey)>,
    issuer: String,
    audience: String,
}

impl Verifier {
    pub fn from_jwks(jwks: &Value, issuer: &str, audience: &str) -> anyhow::Result<Self> {
        let mut keys = Vec::new();
        for key in jwks["keys"].as_array().context("a JWK set has a `keys` list")? {
            if key["kty"] != "OKP" || key["crv"] != "Ed25519" {
                continue;
            }
            let Some(x) = key["x"].as_str().and_then(unb64) else { continue };
            let Ok(x) = <[u8; 32]>::try_from(x) else { continue };
            let Ok(public) = VerifyingKey::from_bytes(&x) else { continue };
            keys.push((key["kid"].as_str().unwrap_or("").to_string(), public));
        }
        if keys.is_empty() {
            bail!("the JWK set has no Ed25519 key");
        }
        Ok(Verifier { keys, issuer: issuer.into(), audience: audience.into() })
    }

    pub fn has_key(&self, kid: &str) -> bool {
        self.keys.iter().any(|(have, _)| have == kid)
    }

    /// The key id a token names, so a caller can fetch new keys when it does not know it.
    pub fn key_id_of(token: &str) -> Option<String> {
        let head: Value = serde_json::from_slice(&unb64(token.split('.').next()?)?).ok()?;
        head["kid"].as_str().map(str::to_string)
    }

    pub fn verify(&self, token: &str) -> Option<Claims> {
        let mut parts = token.trim().split('.');
        let (head, body, sign) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let header: Value = serde_json::from_slice(&unb64(head)?).ok()?;
        // One algorithm only: a token naming another, or `none`, is refused.
        if header["alg"] != "EdDSA" {
            return None;
        }
        let signature = Signature::from_slice(&unb64(sign)?).ok()?;
        let message = format!("{head}.{body}");
        let kid = header["kid"].as_str().unwrap_or("");
        let signed = self
            .keys
            .iter()
            .filter(|(have, _)| kid.is_empty() || have == kid)
            .any(|(_, key)| key.verify(message.as_bytes(), &signature).is_ok());
        if !signed {
            return None;
        }
        let claims: Claims = serde_json::from_slice(&unb64(body)?).ok()?;
        if claims.iss != self.issuer || claims.aud != self.audience || claims.exp <= now() || claims.sub.is_empty() {
            return None;
        }
        Some(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Signer {
        Signer::from_seed(&Signer::seed_from(&[7u8; 32]).unwrap()).unwrap()
    }

    fn claims(exp: u64) -> Claims {
        Claims {
            iss: "https://commons.test".into(),
            sub: "github:42".into(),
            aud: DAEMON_AUDIENCE.into(),
            iat: now(),
            exp,
            login: Some("Naren".into()),
            email: Some("n@example.com".into()),
            name: None,
        }
    }

    #[test]
    fn a_token_is_checked_by_the_published_key_issuer_audience_and_expiry() {
        let signer = signer();
        let verifier = Verifier::from_jwks(&signer.jwks(), "https://commons.test", DAEMON_AUDIENCE).unwrap();
        let token = signer.sign(&claims(now() + 60));
        assert_eq!(verifier.verify(&token).unwrap().sub, "github:42");
        assert!(verifier.has_key(&Verifier::key_id_of(&token).unwrap()));

        assert!(verifier.verify(&signer.sign(&claims(now() - 1))).is_none());
        let mut other = claims(now() + 60);
        other.iss = "https://evil.test".into();
        assert!(verifier.verify(&signer.sign(&other)).is_none());
        other = claims(now() + 60);
        other.aud = "commons-session".into();
        assert!(verifier.verify(&signer.sign(&other)).is_none());

        // Another key, a changed body, or `alg: none` are refused.
        let stranger = Signer::from_seed(&Signer::seed_from(&[9u8; 32]).unwrap()).unwrap();
        assert!(verifier.verify(&stranger.sign(&claims(now() + 60))).is_none());
        let (head, rest) = token.split_once('.').unwrap();
        let sign = rest.split_once('.').unwrap().1;
        let mut forged = claims(now() + 60);
        forged.sub = "github:1".into();
        let body = b64(&serde_json::to_vec(&forged).unwrap());
        assert!(verifier.verify(&format!("{head}.{body}.{sign}")).is_none());
        let none = b64(br#"{"alg":"none"}"#);
        assert!(verifier.verify(&format!("{none}.{body}.")).is_none());
    }

    #[test]
    fn an_allow_list_names_accounts_by_login_email_or_subject() {
        let me = claims(now() + 60);
        assert!(me.allowed_by(&["github:naren".into()]));
        assert!(me.allowed_by(&["n@example.com".into()]));
        assert!(me.allowed_by(&["github:42".into()]));
        assert!(!me.allowed_by(&["google:n@example.com".into()]));
        assert!(!me.allowed_by(&["github:someone".into(), " ".into()]));
        assert!(!me.allowed_by(&[]));
    }
}
