//! Signing in with Google or GitHub: the authorization-code flow.
//!
//! The browser goes to the provider with a random `state`, which is also sealed in a
//! cookie. The provider sends it back with a code. The code is exchanged for an access
//! token here, server to server, and the token is used once to read who the person is.
//! Only a verified email is kept. The access token is then dropped.

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Config, OAuthClient};
use crate::db::Identity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    GitHub,
}

impl Provider {
    pub fn parse(name: &str) -> Option<Provider> {
        match name {
            "google" => Some(Provider::Google),
            "github" => Some(Provider::GitHub),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::GitHub => "github",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Google => "Google",
            Provider::GitHub => "GitHub",
        }
    }

    pub fn client(self, config: &Config) -> Option<&OAuthClient> {
        match self {
            Provider::Google => config.google.as_ref(),
            Provider::GitHub => config.github.as_ref(),
        }
    }

    /// The providers this service is set up for, in the order they are offered.
    pub fn enabled(config: &Config) -> Vec<Provider> {
        [Provider::GitHub, Provider::Google].into_iter().filter(|p| p.client(config).is_some()).collect()
    }

    pub fn callback(self, config: &Config) -> String {
        format!("{}/auth/{}/callback", config.url, self.id())
    }

    /// Where to send the browser to sign in.
    pub fn authorize_url(self, config: &Config, state: &str) -> Option<String> {
        let client = self.client(config)?;
        let (base, scope) = match self {
            Provider::Google => (&config.endpoints.google_authorize, "openid email profile"),
            // Who the person is and their verified email. Nothing in their repositories.
            Provider::GitHub => (&config.endpoints.github_authorize, "read:user user:email"),
        };
        let mut url = url::Url::parse(base).ok()?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("client_id", &client.id)
                .append_pair("redirect_uri", &self.callback(config))
                .append_pair("scope", scope)
                .append_pair("state", state);
            if self == Provider::Google {
                query.append_pair("response_type", "code").append_pair("prompt", "select_account");
            }
        }
        Some(url.into())
    }

    /// Trade the code the provider sent back for who the person is.
    pub async fn identify(self, http: &reqwest::Client, config: &Config, code: &str) -> anyhow::Result<Identity> {
        let client = self.client(config).context("this provider is not set up")?;
        match self {
            Provider::Google => google(http, config, client, code).await,
            Provider::GitHub => github(http, config, client, code).await,
        }
    }
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

impl TokenReply {
    fn token(self, provider: &str) -> anyhow::Result<String> {
        match (self.access_token, self.error) {
            (Some(token), _) if !token.is_empty() => Ok(token),
            (_, Some(error)) => bail!("{provider} refused the sign-in: {error} {}", self.error_description.unwrap_or_default()),
            _ => bail!("{provider} sent no access token"),
        }
    }
}

async fn google(http: &reqwest::Client, config: &Config, client: &OAuthClient, code: &str) -> anyhow::Result<Identity> {
    let reply: TokenReply = http
        .post(&config.endpoints.google_token)
        .form(&[
            ("code", code),
            ("client_id", &client.id),
            ("client_secret", &client.secret),
            ("redirect_uri", &Provider::Google.callback(config)),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .context("cannot reach Google")?
        .json()
        .await
        .context("Google sent something that is not a token")?;
    let token = reply.token("Google")?;
    let info: Value = http
        .get(&config.endpoints.google_userinfo)
        .bearer_auth(token)
        .send()
        .await
        .context("cannot reach Google")?
        .error_for_status()
        .context("Google would not say who signed in")?
        .json()
        .await?;
    let subject = info["sub"].as_str().filter(|s| !s.is_empty()).context("Google sent no account id")?;
    let verified = info["email_verified"].as_bool().unwrap_or(false);
    Ok(Identity {
        provider: "google".into(),
        subject: subject.into(),
        login: None,
        email: info["email"].as_str().filter(|_| verified).map(str::to_ascii_lowercase),
        name: info["name"].as_str().map(str::to_string),
    })
}

async fn github(http: &reqwest::Client, config: &Config, client: &OAuthClient, code: &str) -> anyhow::Result<Identity> {
    let reply: TokenReply = http
        .post(&config.endpoints.github_token)
        .header("accept", "application/json")
        .form(&[
            ("code", code),
            ("client_id", &client.id),
            ("client_secret", &client.secret),
            ("redirect_uri", &Provider::GitHub.callback(config)),
        ])
        .send()
        .await
        .context("cannot reach GitHub")?
        .json()
        .await
        .context("GitHub sent something that is not a token")?;
    let token = reply.token("GitHub")?;
    let api = |path: &str| {
        http.get(format!("{}{path}", config.endpoints.github_api))
            .bearer_auth(&token)
            .header("accept", "application/vnd.github+json")
            .header("user-agent", "taskmaster-commons")
    };
    let user: Value = api("/user")
        .send()
        .await
        .context("cannot reach GitHub")?
        .error_for_status()
        .context("GitHub would not say who signed in")?
        .json()
        .await?;
    let id = user["id"].as_u64().context("GitHub sent no account id")?;
    // Best effort: an account can hide its emails, and that is fine.
    let emails: Value = match api("/user/emails").send().await {
        Ok(reply) if reply.status().is_success() => reply.json().await.unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let email = emails
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["primary"] == true && e["verified"] == true)
        .and_then(|e| e["email"].as_str())
        .map(str::to_ascii_lowercase);
    Ok(Identity {
        provider: "github".into(),
        subject: id.to_string(),
        login: user["login"].as_str().map(str::to_string),
        email,
        name: user["name"].as_str().map(str::to_string),
    })
}
