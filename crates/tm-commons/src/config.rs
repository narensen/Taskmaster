//! The service is configured by environment variables, normally from a `.env` file the
//! systemd unit loads. `deploy/commons/commons.env.example` lists them with placeholders.
//! A placeholder counts as missing, and the service will not start until every required
//! one is filled in, so a half-configured service never goes live.

use std::net::SocketAddr;

use anyhow::bail;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub id: String,
    pub secret: String,
}

/// Where the sign-in providers are. Tests point these at a fake provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub google_authorize: String,
    pub google_token: String,
    pub google_userinfo: String,
    pub github_authorize: String,
    pub github_token: String,
    pub github_api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            google_authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            google_token: "https://oauth2.googleapis.com/token".into(),
            google_userinfo: "https://openidconnect.googleapis.com/v1/userinfo".into(),
            github_authorize: "https://github.com/login/oauth/authorize".into(),
            github_token: "https://github.com/login/oauth/access_token".into(),
            github_api: "https://api.github.com".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Where people reach the service, such as `https://commons.example.com`. Tokens carry
    /// it as their issuer, and the providers send people back to it.
    pub url: String,
    pub bind: SocketAddr,
    /// A Postgres connection string, or `memory` to keep everything in this process.
    pub database: String,
    /// The Ed25519 seed that signs sign-in tokens. `tm-commons keygen` makes one.
    pub signing_key: String,
    /// Signs cookies. `tm-commons keygen` makes one.
    pub session_secret: String,
    pub google: Option<OAuthClient>,
    pub github: Option<OAuthClient>,
    /// Who may review entries: `github:<login>`, `google:<email>`, or an email.
    pub moderators: Vec<String>,
    /// Entries one person may send in a day.
    pub daily_limit: u64,
    pub endpoints: Endpoints,
}

/// Empty, or still the value from the example file.
pub fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || value.contains("example.invalid") || value.contains("replace-me")
}

impl Config {
    pub fn from_env() -> anyhow::Result<Config> {
        Config::from_vars(|name| std::env::var(name).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Config> {
        let mut missing = Vec::new();
        let mut need = |name: &str| -> String {
            match get(name).filter(|value| !is_placeholder(value)) {
                Some(value) => value.trim().to_string(),
                None => {
                    missing.push(name.to_string());
                    String::new()
                }
            }
        };
        let url = need("TM_COMMONS_URL").trim_end_matches('/').to_string();
        let database = need("DATABASE_URL");
        let signing_key = need("TM_COMMONS_SIGNING_KEY");
        let session_secret = need("TM_COMMONS_SESSION_SECRET");
        if !missing.is_empty() {
            bail!("set {} (see deploy/commons/commons.env.example)", missing.join(", "));
        }
        let pair = |id: &str, secret: &str| -> anyhow::Result<Option<OAuthClient>> {
            let id_value = get(id).filter(|v| !is_placeholder(v));
            let secret_value = get(secret).filter(|v| !is_placeholder(v));
            match (id_value, secret_value) {
                (Some(id), Some(secret)) => Ok(Some(OAuthClient { id: id.trim().into(), secret: secret.trim().into() })),
                (None, None) => Ok(None),
                (Some(_), None) => bail!("{id} is set but {secret} is not"),
                (None, Some(_)) => bail!("{secret} is set but {id} is not"),
            }
        };
        let google = pair("GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET")?;
        let github = pair("GITHUB_CLIENT_ID", "GITHUB_CLIENT_SECRET")?;
        if google.is_none() && github.is_none() {
            bail!("set GOOGLE_CLIENT_ID and GOOGLE_CLIENT_SECRET, or GITHUB_CLIENT_ID and GITHUB_CLIENT_SECRET, or both");
        }
        let local = url.starts_with("http://localhost") || url.starts_with("http://127.0.0.1");
        if !url.starts_with("https://") && !local {
            bail!("TM_COMMONS_URL must be https://, or http://localhost while trying it out");
        }
        if session_secret.len() < 32 {
            bail!("TM_COMMONS_SESSION_SECRET is shorter than 32 characters; `tm-commons keygen` makes one");
        }
        let bind = match get("TM_COMMONS_BIND").filter(|v| !v.trim().is_empty()) {
            Some(addr) => addr.trim().parse().map_err(|_| anyhow::anyhow!("TM_COMMONS_BIND `{addr}` is not an address like 127.0.0.1:8740"))?,
            None => SocketAddr::from(([127, 0, 0, 1], 8740)),
        };
        let moderators = get("TM_COMMONS_MODERATORS")
            .unwrap_or_default()
            .split(',')
            .map(|entry| entry.trim().to_string())
            .filter(|entry| !entry.is_empty() && !is_placeholder(entry))
            .collect();
        let daily_limit = match get("TM_COMMONS_DAILY_LIMIT").filter(|v| !v.trim().is_empty()) {
            Some(n) => n.trim().parse().map_err(|_| anyhow::anyhow!("TM_COMMONS_DAILY_LIMIT is not a number"))?,
            None => 50,
        };
        Ok(Config {
            url,
            bind,
            database,
            signing_key,
            session_secret,
            google,
            github,
            moderators,
            daily_limit,
            endpoints: Endpoints::default(),
        })
    }

    /// Cookies are `Secure` unless the service is on this machine over plain HTTP.
    pub fn secure_cookies(&self) -> bool {
        self.url.starts_with("https://")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_example_file_is_all_placeholders_and_names_what_is_missing() {
        let example = include_str!("../../../deploy/commons/commons.env.example");
        let pairs: Vec<(&str, &str)> = example
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .collect();
        let err = Config::from_vars(vars(&pairs)).unwrap_err().to_string();
        for name in ["TM_COMMONS_URL", "DATABASE_URL", "TM_COMMONS_SIGNING_KEY", "TM_COMMONS_SESSION_SECRET"] {
            assert!(err.contains(name), "{err}");
        }
    }

    #[test]
    fn one_provider_is_enough_and_half_a_provider_is_refused() {
        let base = [
            ("TM_COMMONS_URL", "https://commons.test/"),
            ("DATABASE_URL", "memory"),
            ("TM_COMMONS_SIGNING_KEY", "k"),
            ("TM_COMMONS_SESSION_SECRET", "0123456789abcdef0123456789abcdef"),
            ("TM_COMMONS_MODERATORS", "github:ana, n@example.com,"),
        ];
        let mut with_github = base.to_vec();
        with_github.extend([("GITHUB_CLIENT_ID", "id"), ("GITHUB_CLIENT_SECRET", "secret")]);
        let config = Config::from_vars(vars(&with_github)).unwrap();
        assert_eq!(config.url, "https://commons.test");
        assert!(config.google.is_none() && config.github.is_some());
        assert_eq!(config.moderators, vec!["github:ana".to_string(), "n@example.com".to_string()]);
        assert_eq!((config.bind.port(), config.daily_limit), (8740, 50));

        assert!(Config::from_vars(vars(&base)).unwrap_err().to_string().contains("GOOGLE_CLIENT_ID"));
        let mut half = base.to_vec();
        half.push(("GOOGLE_CLIENT_ID", "id"));
        assert!(Config::from_vars(vars(&half)).unwrap_err().to_string().contains("GOOGLE_CLIENT_SECRET is not"));
        let mut plain = with_github.clone();
        plain[0] = ("TM_COMMONS_URL", "http://commons.test");
        assert!(Config::from_vars(vars(&plain)).is_err());
    }
}
