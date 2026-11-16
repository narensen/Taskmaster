//! The service over a real socket, with fake Google and GitHub behind it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::Form;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tm_token::{DAEMON_AUDIENCE, Verifier};

use crate::app::{App, daemon_redirect, router};
use crate::config::{Config, Endpoints};
use crate::db::Memory;
use crate::db::tests::entry;

/// Google and GitHub as far as this service talks to them. The code `good` works once.
fn providers() -> Router {
    fn bearer(headers: &HeaderMap) -> String {
        headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
    }
    Router::new()
        .route(
            "/github/token",
            post(|Form(form): Form<HashMap<String, String>>| async move {
                if form.get("code").map(String::as_str) == Some("good") && form.get("client_secret").map(String::as_str) == Some("gh-secret") {
                    Json(json!({ "access_token": "gh-access", "token_type": "bearer" }))
                } else {
                    Json(json!({ "error": "bad_verification_code" }))
                }
            }),
        )
        .route(
            "/github/api/user",
            get(|headers: HeaderMap| async move {
                if bearer(&headers) != "Bearer gh-access" {
                    return Json(json!({ "message": "Bad credentials" }));
                }
                Json(json!({ "id": 42, "login": "ana", "name": "Ana" }))
            }),
        )
        .route(
            "/github/api/user/emails",
            get(|| async {
                Json(json!([
                    { "email": "old@example.com", "primary": false, "verified": true },
                    { "email": "Ana@Example.com", "primary": true, "verified": true },
                ]))
            }),
        )
        .route(
            "/google/token",
            post(|Form(form): Form<HashMap<String, String>>| async move {
                if form.get("code").map(String::as_str) == Some("good") && form.get("grant_type").map(String::as_str) == Some("authorization_code") {
                    Json(json!({ "access_token": "g-access" }))
                } else {
                    Json(json!({ "error": "invalid_grant" }))
                }
            }),
        )
        .route(
            "/google/userinfo",
            get(|headers: HeaderMap| async move {
                assert_eq!(bearer(&headers), "Bearer g-access");
                Json(json!({ "sub": "g-7", "email": "bo@example.com", "email_verified": true, "name": "Bo" }))
            }),
        )
}

async fn listen(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

struct Browser {
    base: String,
    http: reqwest::Client,
    cookies: HashMap<String, String>,
}

struct Reply {
    status: u16,
    location: String,
    body: String,
}

impl Browser {
    fn new(base: &str) -> Self {
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Browser { base: base.into(), http, cookies: HashMap::new() }
    }

    async fn send(&mut self, request: reqwest::RequestBuilder) -> Reply {
        let cookies: Vec<String> = self.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let reply = request.header("cookie", cookies.join("; ")).send().await.unwrap();
        for value in reply.headers().get_all("set-cookie") {
            let pair = value.to_str().unwrap().split(';').next().unwrap();
            let (name, value) = pair.split_once('=').unwrap();
            if value.is_empty() {
                self.cookies.remove(name);
            } else {
                self.cookies.insert(name.into(), value.into());
            }
        }
        let status = reply.status().as_u16();
        let location = reply.headers().get("location").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
        Reply { status, location, body: reply.text().await.unwrap() }
    }

    async fn get(&mut self, path: &str) -> Reply {
        let request = self.http.get(format!("{}{path}", self.base));
        self.send(request).await
    }

    async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> Reply {
        let request = self.http.post(format!("{}{path}", self.base)).form(form);
        self.send(request).await
    }

    /// Sign in with a provider, the way a browser does.
    async fn sign_in(&mut self, provider: &str, query: &str) -> Reply {
        let start = self.get(&format!("/auth/{provider}/start?{query}")).await;
        assert_eq!(start.status, 303);
        let url = url::Url::parse(&start.location).unwrap();
        let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
        assert!(url.query_pairs().any(|(k, v)| k == "redirect_uri" && v.ends_with(&format!("/auth/{provider}/callback"))));
        self.get(&format!("/auth/{provider}/callback?code=good&state={state}")).await
    }

    async fn csrf(&mut self, path: &str) -> String {
        let page = self.get(path).await.body;
        let at = page.find(r#"name="csrf" value=""#).expect("a form on the page") + r#"name="csrf" value=""#.len();
        page[at..at + page[at..].find('"').unwrap()].to_string()
    }
}

async fn api(base: &str, method: &str, path: &str, token: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let http = reqwest::Client::new();
    let mut request = if method == "GET" { http.get(format!("{base}{path}")) } else { http.post(format!("{base}{path}")) };
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let reply = request.send().await.unwrap();
    let status = reply.status().as_u16();
    (status, reply.json().await.unwrap_or(Value::Null))
}

async fn service() -> String {
    let fake = listen(providers()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let vars: HashMap<&str, String> = [
        ("TM_COMMONS_URL", base.clone()),
        ("DATABASE_URL", "memory".into()),
        ("TM_COMMONS_SIGNING_KEY", tm_token::Signer::seed_from(&[3u8; 32]).unwrap()),
        ("TM_COMMONS_SESSION_SECRET", "0123456789abcdef0123456789abcdef".into()),
        ("GITHUB_CLIENT_ID", "gh-id".into()),
        ("GITHUB_CLIENT_SECRET", "gh-secret".into()),
        ("GOOGLE_CLIENT_ID", "g-id".into()),
        ("GOOGLE_CLIENT_SECRET", "g-secret".into()),
        ("TM_COMMONS_MODERATORS", "google:bo@example.com".into()),
        ("TM_COMMONS_DAILY_LIMIT", "3".into()),
    ]
    .into_iter()
    .collect();
    let mut config = Config::from_vars(|name| vars.get(name).cloned()).unwrap();
    config.endpoints = Endpoints {
        google_authorize: format!("{fake}/google/authorize"),
        google_token: format!("{fake}/google/token"),
        google_userinfo: format!("{fake}/google/userinfo"),
        github_authorize: format!("{fake}/github/authorize"),
        github_token: format!("{fake}/github/token"),
        github_api: format!("{fake}/github/api"),
    };
    let app = Arc::new(App::new(config, Arc::new(Memory::default())).unwrap());
    tokio::spawn(async move { axum::serve(listener, router(app)).await.unwrap() });
    base
}

#[tokio::test]
async fn people_sign_in_share_entries_and_a_moderator_reviews_them_each_week() {
    let base = service().await;

    // Ana signs in with GitHub and makes a token for her Taskmaster.
    let mut ana = Browser::new(&base);
    let landed = ana.sign_in("github", "next=%2Faccount").await;
    assert_eq!((landed.status, landed.location.as_str()), (303, "/account"));
    assert!(ana.get("/account").await.body.contains("@ana"));
    let csrf = ana.csrf("/account").await;
    let made = ana.post("/account/tokens", &[("csrf", &csrf), ("label", "laptop")]).await;
    let at = made.body.find("tmc_").unwrap();
    let token = made.body[at..at + 52].to_string();
    assert!(token.chars().skip(4).all(|c| c.is_ascii_hexdigit()), "{token}");
    assert!(made.body.contains(r#"&quot;allow&quot;: [&quot;github:ana&quot;]"#), "the page shows the config to paste");
    assert_eq!(ana.post("/account/tokens", &[("csrf", "forged"), ("label", "x")]).await.status, 303);

    // A sign-in is refused when its state does not match, or the code is wrong.
    let mut stranger = Browser::new(&base);
    assert_eq!(stranger.get("/auth/github/callback?code=good&state=nope").await.status, 400);
    let start = stranger.get("/auth/github/start").await;
    let state = url::Url::parse(&start.location).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    assert_eq!(stranger.get(&format!("/auth/github/callback?code=bad&state={state}")).await.status, 502);

    // Her Taskmaster shares an entry. It waits for review; nobody gets it yet.
    let card = entry("card", "export const Card = () => null;\n", "next@15,react@19");
    let shared = serde_json::to_value(&card).unwrap();
    assert_eq!(api(&base, "POST", "/api/commons/entries", None, Some(shared.clone())).await.0, 401);
    let (status, sent) = api(&base, "POST", "/api/commons/entries", Some(&token), Some(shared.clone())).await;
    assert_eq!((status, sent["status"].as_str()), (202, Some("pending")));
    assert_eq!(api(&base, "POST", "/api/commons/entries", Some(&token), Some(shared.clone())).await.1["status"], "pending");
    assert!(api(&base, "GET", "/api/commons/entries", None, None).await.1["entries"].as_array().unwrap().is_empty());

    // A credential, a wrong hash, or a path for a key are refused.
    let leaky = entry("db", &format!("const url = \"{}://app:hunter22@db.example.com/x\";\n", "postgres"), "");
    let (status, refused) = api(&base, "POST", "/api/commons/entries", Some(&token), Some(serde_json::to_value(&leaky).unwrap())).await;
    assert_eq!(status, 422, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("database URL"));
    let mut forged = shared.clone();
    forged["content"] = json!("something else");
    assert_eq!(api(&base, "POST", "/api/commons/entries", Some(&token), Some(forged)).await.0, 400);
    let mut path = shared.clone();
    path["key"] = json!("../card");
    assert_eq!(api(&base, "POST", "/api/commons/entries", Some(&token), Some(path)).await.0, 400);

    // Ana is not a moderator. Bo, signed in with Google, is.
    assert_eq!(ana.get("/moderate").await.status, 403);
    let mut bo = Browser::new(&base);
    bo.sign_in("google", "next=%2Fmoderate").await;
    let queue = bo.get("/moderate").await;
    assert!(queue.status == 200 && queue.body.contains("card") && queue.body.contains("by @ana"), "{}", queue.body);
    let csrf = bo.csrf("/moderate").await;
    let decided = bo.post(&format!("/moderate/entries/card/{}", card.hash), &[("csrf", &csrf), ("decision", "approve")]).await;
    assert_eq!((decided.status, decided.location.as_str()), (303, "/moderate"));

    // Now it is served to projects on a matching stack, with a pass rate earned in use.
    let (_, served) = api(&base, "GET", "/api/commons/entries?stack=next@15,react@19,tailwind@4", None, None).await;
    assert_eq!(served["entries"][0]["key"], "card");
    assert_eq!(served["entries"][0]["tier"], "commons");
    assert!(api(&base, "GET", "/api/commons/entries?stack=vue@3", None, None).await.1["entries"].as_array().unwrap().is_empty());
    let used = json!({ "key": "card", "hash": card.hash, "passed": true });
    assert_eq!(api(&base, "POST", "/api/commons/usage", Some(&token), Some(used)).await.0, 204);
    let (_, served) = api(&base, "GET", "/api/commons/entries", None, None).await;
    assert_eq!(served["entries"][0]["stats"]["passes"], 1);
    assert!(ana.get("/entries").await.body.contains("1 run, 100% passed"));
    assert!(ana.get("/account").await.body.contains("served"));

    // Someone reports it; the moderator withdraws it, and Taskmasters are told to drop it.
    let report = json!({ "key": "card", "hash": card.hash, "reason": "it renders nothing" });
    assert_eq!(api(&base, "POST", "/api/commons/reports", Some(&token), Some(report)).await.0, 201);
    assert!(bo.get("/moderate").await.body.contains("it renders nothing"));
    let withdrawn = bo.post("/moderate/reports/1", &[("csrf", &csrf), ("action", "withdraw")]).await;
    assert_eq!(withdrawn.status, 303);
    let (_, served) = api(&base, "GET", "/api/commons/entries", None, None).await;
    assert!(served["entries"].as_array().unwrap().is_empty());
    assert_eq!(served["withdrawn"][0], json!({ "key": "card", "hash": card.hash }));

    // A daily limit holds.
    for n in 0..2 {
        let more = entry(&format!("more-{n}"), &format!("export const N = {n};\n"), "");
        assert_eq!(api(&base, "POST", "/api/commons/entries", Some(&token), Some(serde_json::to_value(&more).unwrap())).await.0, 202);
    }
    let over = entry("over", "export const O = 1;\n", "");
    assert_eq!(api(&base, "POST", "/api/commons/entries", Some(&token), Some(serde_json::to_value(&over).unwrap())).await.0, 429);

    // A ban: Ana's token stops working, and her waiting entries are withdrawn.
    let (_, me) = api(&base, "GET", "/api/me", Some(&token), None).await;
    assert_eq!((me["allow"].as_str(), me["moderator"].as_bool()), (Some("github:ana"), Some(false)));
    let report = json!({ "key": "more-0", "hash": entry("more-0", "export const N = 0;\n", "").hash, "reason": "spam" });
    assert_eq!(api(&base, "POST", "/api/commons/reports", Some(&token), Some(report)).await.0, 201);
    assert_eq!(bo.post("/moderate/reports/2", &[("csrf", &csrf), ("action", "ban")]).await.status, 303);
    assert_eq!(api(&base, "GET", "/api/me", Some(&token), None).await.0, 403);
    assert!(!bo.get("/moderate").await.body.contains("more-1"));
    assert_eq!(ana.get("/account").await.status, 303, "a banned session is over");
}

#[tokio::test]
async fn a_taskmaster_gets_a_token_it_can_check_without_any_secret() {
    let base = service().await;
    let daemon = "http://100.101.102.103:8737/signed-in";
    let mut ana = Browser::new(&base);

    // Not signed in yet: the page names the Taskmaster and offers the providers.
    let asked = ana.get(&format!("/authorize?redirect_uri={}", crate::pages::enc(daemon))).await;
    assert!(asked.body.contains("100.101.102.103:8737") && asked.body.contains("/auth/github/start?redirect_uri="), "{}", asked.body);
    let landed = ana.sign_in("github", &format!("redirect_uri={}", crate::pages::enc(daemon))).await;
    assert!(landed.location.starts_with("/authorize?redirect_uri="), "{}", landed.location);

    // Signed in: it asks before handing anything over.
    let confirm = ana.get(&landed.location).await;
    assert!(confirm.body.contains("as <b>@ana</b>"), "{}", confirm.body);
    let csrf = ana.csrf(&landed.location).await;
    assert_eq!(ana.post("/authorize", &[("csrf", "forged"), ("redirect_uri", daemon)]).await.status, 403);
    let handed = ana.post("/authorize", &[("csrf", &csrf), ("redirect_uri", daemon)]).await;
    assert_eq!(handed.status, 303);
    let token = handed.location.strip_prefix(&format!("{daemon}#token=")).expect("sent back to the Taskmaster").to_string();

    // The Taskmaster checks it with the published key alone.
    let (_, jwks) = api(&base, "GET", "/.well-known/jwks.json", None, None).await;
    let verifier = Verifier::from_jwks(&jwks, &base, DAEMON_AUDIENCE).unwrap();
    let claims = verifier.verify(&token).unwrap();
    assert_eq!((claims.sub.as_str(), claims.login.as_deref(), claims.email.as_deref()), ("github:42", Some("ana"), Some("ana@example.com")));
    assert!(claims.allowed_by(&["github:ana".into()]) && !claims.allowed_by(&["github:bo".into()]));
    assert!(claims.exp > tm_token::now() + 29 * 86_400);

    // Somewhere that is not a Taskmaster gets nothing.
    for bad in ["http://evil.example/signed-in", "https://evil.example/elsewhere", "https://u:p@evil.example/signed-in", "javascript:alert(1)"] {
        assert_eq!(ana.get(&format!("/authorize?redirect_uri={}", crate::pages::enc(bad))).await.status, 400, "{bad}");
        assert_eq!(ana.post("/authorize", &[("csrf", &csrf), ("redirect_uri", bad)]).await.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn privacy_and_terms_are_public_plain_pages_linked_from_every_page() {
    let base = service().await;
    let mut visitor = Browser::new(&base);
    let privacy = visitor.get("/privacy").await;
    assert_eq!(privacy.status, 200);
    assert!(privacy.body.contains("<h1>Privacy</h1>") && privacy.body.contains("verified email") && privacy.body.contains("Cookies"), "{}", privacy.body);
    let terms = visitor.get("/terms").await;
    assert_eq!(terms.status, 200);
    assert!(terms.body.contains("<h1>Terms</h1>") && terms.body.contains("Moderators may"), "{}", terms.body);
    for path in ["/", "/signin", "/privacy", "/terms"] {
        let body = visitor.get(path).await.body;
        assert!(body.contains(r#"href="/privacy""#) && body.contains(r#"href="/terms""#), "{path} links both pages");
    }

    // Signed in, the same pages still show and keep the menu.
    let landed = visitor.sign_in("github", "next=%2Fprivacy").await;
    assert_eq!((landed.status, landed.location.as_str()), (303, "/privacy"));
    let signed_in = visitor.get("/privacy").await;
    assert_eq!(signed_in.status, 200);
    assert!(signed_in.body.contains("@ana") && signed_in.body.contains("Sign out"));
}

#[test]
fn plain_http_is_only_for_this_machine_a_private_network_or_a_tailnet() {
    for good in [
        "http://localhost:8737/signed-in",
        "http://127.0.0.1:8737/signed-in",
        "http://100.64.1.2:8737/signed-in",
        "http://192.168.1.20:8737/signed-in",
        "http://box.tail1234.ts.net:8737/signed-in",
        "http://[::1]:8737/signed-in",
        "https://tm.example.com/signed-in",
    ] {
        assert!(daemon_redirect(good).is_some(), "{good}");
    }
    for bad in [
        "http://8.8.8.8/signed-in",
        "http://100.128.0.1/signed-in",
        "http://example.com/signed-in",
        "https://tm.example.com/signed-in?x=1",
        "https://tm.example.com/signed-in#t",
        "ftp://localhost/signed-in",
    ] {
        assert!(daemon_redirect(bad).is_none(), "{bad}");
    }
}

#[tokio::test]
async fn the_sign_in_flow_is_bound_to_the_browser_that_started_it_and_cookies_are_locked_down() {
    let base = service().await;
    let mut me = Browser::new(&base);

    // The way to the provider: right callback, narrow scopes, a fresh state each time, a cookie to match.
    let first = me.get("/auth/github/start").await;
    let url = url::Url::parse(&first.location).unwrap();
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["redirect_uri"], format!("{base}/auth/github/callback"));
    assert_eq!(query["scope"], "read:user user:email");
    assert_eq!(query["client_id"], "gh-id");
    let state = query["state"].clone();
    assert_eq!(state.len(), 32);
    let alias = me.get("/auth/google").await;
    let g: HashMap<_, _> = url::Url::parse(&alias.location).unwrap().query_pairs().into_owned().collect();
    assert_eq!((g["scope"].as_str(), g["response_type"].as_str()), ("openid email profile", "code"));
    assert_ne!(g["state"], state);

    // Flow cookie: HttpOnly, Lax, short. (Plain HTTP on this machine, so not Secure here.)
    let raw = me.http.get(format!("{base}/auth/github/start")).send().await.unwrap();
    let cookie = raw.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax") && cookie.contains("Max-Age=600") && cookie.contains("Path=/"), "{cookie}");

    // A callback with the Google state to the GitHub endpoint, or a missing code, or a provider's own error, is refused.
    let mut other = Browser::new(&base);
    let started = other.get("/auth/github/start").await;
    let real: HashMap<_, _> = url::Url::parse(&started.location).unwrap().query_pairs().into_owned().collect();
    assert_eq!(other.get(&format!("/auth/google/callback?code=good&state={}", real["state"])).await.status, 400);
    assert_eq!(other.get(&format!("/auth/github/callback?state={}", real["state"])).await.status, 400);
    let cancelled = other.get("/auth/github/callback?error=access_denied").await;
    assert_eq!(cancelled.status, 400);
    assert!(!other.cookies.contains_key("tm_signin"), "a callback uses up the sign-in");
    // Used up: the same state cannot be replayed.
    assert_eq!(other.get(&format!("/auth/github/callback?code=good&state={}", real["state"])).await.status, 400);

    // Someone else's state means nothing without the matching cookie.
    let mut thief = Browser::new(&base);
    assert_eq!(thief.get(&format!("/auth/github/callback?code=good&state={}", real["state"])).await.status, 400);

    // A good sign-in sets the session cookie with every flag, and the reply is not cached.
    let mut ana = Browser::new(&base);
    let started = ana.get("/auth/github/start").await;
    let st = url::Url::parse(&started.location).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.to_string();
    let request = ana.http.get(format!("{base}/auth/github/callback?code=good&state={st}")).header("cookie", format!("tm_signin={}", ana.cookies["tm_signin"]));
    let reply = request.send().await.unwrap();
    assert_eq!(reply.headers().get("cache-control").unwrap(), "no-store");
    let cookies: Vec<String> = reply.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect();
    let session = cookies.iter().find(|c| c.starts_with("tm_session=")).unwrap();
    assert!(session.contains("HttpOnly") && session.contains("SameSite=Lax") && session.contains("Path=/") && session.contains("Max-Age=2592000"), "{session}");
    assert!(cookies.iter().any(|c| c.starts_with("tm_signin=;") && c.contains("Max-Age=0")));
}

#[tokio::test]
async fn sign_in_never_redirects_off_site_and_sign_out_clears_the_session() {
    let base = service().await;
    for next in ["https://evil.example", "//evil.example", "/\\evil.example", "%2F%09%2Fevil.example", "%2F%0A%2Fevil.example", "javascript:1"] {
        let mut browser = Browser::new(&base);
        let landed = browser.sign_in("github", &format!("next={next}")).await;
        assert_eq!((landed.status, landed.location.as_str()), (303, "/account"), "{next}");
    }
    let mut ana = Browser::new(&base);
    ana.sign_in("github", "next=%2Fentries").await;
    assert_eq!(ana.get("/account").await.status, 200);
    let csrf = ana.csrf("/account").await;
    // A forged sign-out does nothing; a real one clears the cookie.
    ana.post("/signout", &[("csrf", "forged")]).await;
    assert_eq!(ana.get("/account").await.status, 200);
    let out = ana.post("/signout", &[("csrf", &csrf)]).await;
    assert_eq!((out.status, out.location.as_str()), (303, "/"));
    assert!(!ana.cookies.contains_key("tm_session"));
    assert_eq!(ana.get("/account").await.status, 303);
}

#[test]
fn the_moderators_are_found_by_github_login_or_verified_google_email_and_nobody_else() {
    use crate::config::Config;
    let vars: HashMap<&str, &str> = [
        ("TM_COMMONS_URL", "https://commons.test"),
        ("DATABASE_URL", "memory"),
        ("TM_COMMONS_SIGNING_KEY", "k"),
        ("TM_COMMONS_SESSION_SECRET", "0123456789abcdef0123456789abcdef"),
        ("GITHUB_CLIENT_ID", "i"),
        ("GITHUB_CLIENT_SECRET", "s"),
        ("TM_COMMONS_MODERATORS", "github:narensen, google:scartegy@gmail.com"),
        ("DATABASE_URL_DIRECT", "postgresql://direct.example/db"),
    ]
    .into_iter()
    .collect();
    let config = Config::from_vars(|n| vars.get(n).map(|v| v.to_string())).unwrap();
    assert_eq!(config.database_direct.as_deref(), Some("postgresql://direct.example/db"));
    let claims = |sub: &str, login: Option<&str>, email: Option<&str>| tm_token::Claims {
        iss: String::new(),
        sub: sub.into(),
        aud: DAEMON_AUDIENCE.into(),
        iat: 0,
        exp: 0,
        login: login.map(Into::into),
        email: email.map(Into::into),
        name: None,
    };
    let allowed = |c: &tm_token::Claims| c.allowed_by(&config.moderators);
    assert!(allowed(&claims("github:1", Some("NarenSen"), None)));
    assert!(allowed(&claims("google:9", None, Some("Scartegy@Gmail.com"))));
    assert!(!allowed(&claims("github:2", Some("narensen2"), None)));
    assert!(!allowed(&claims("google:9", None, Some("other@gmail.com"))));
    // An unverified Google email is never kept, so it can never match.
    assert!(!allowed(&claims("google:9", None, None)));
    // The GitHub rule is for GitHub accounts only, and the Google rule for Google accounts.
    assert!(!allowed(&claims("google:9", Some("narensen"), None)));
    // `google:<email>` is for Google accounts only: a GitHub account with that email is not accepted.
    assert!(!allowed(&claims("github:3", Some("x"), Some("scartegy@gmail.com"))));
}

#[tokio::test]
async fn the_database_is_migrated_over_the_direct_url_only_when_one_is_given() {
    // `memory` has no direct endpoint; it must work with and without one.
    assert!(crate::open_db("memory", None).await.is_ok());
    assert!(crate::open_db("memory", Some("postgresql://unused.example/db")).await.is_ok());
    // A direct URL that cannot be reached is a clear error, not a silent skip.
    let err = crate::open_db("postgresql://u:p@127.0.0.1:1/x", Some("postgresql://u:p@127.0.0.1:1/x?sslmode=disable")).await.err().unwrap();
    assert!(format!("{err:#}").contains("DATABASE_URL"), "{err:#}");
}
