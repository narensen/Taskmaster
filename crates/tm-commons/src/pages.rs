//! The service's few pages, rendered on the server. Plain HTML forms: no script needed.

use crate::db::{Counts, Report, Status, Submission, TokenInfo, User};
use crate::oauth::Provider;

pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn enc(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

const STYLE: &str = r#"
:root{color-scheme:dark;--bg:#0b0b0c;--panel:#141416;--line:#2a2a2f;--text:#ededee;--muted:#9a9aa2;--accent:#d9b45b;--ink:#1a1405;--good:#4fbf7f;--bad:#e5604f;--ask:#e2a93b}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:15px/1.5 Inter,ui-sans-serif,system-ui,-apple-system,"Segoe UI",Roboto,sans-serif}
a{color:var(--accent);text-decoration:none}header{display:flex;justify-content:space-between;align-items:center;gap:12px;padding:14px 20px;border-bottom:1px solid var(--line)}
header .brand{display:flex;align-items:center;gap:10px;color:var(--text);font-weight:700}header nav{display:flex;gap:14px;align-items:center;flex-wrap:wrap;font-size:.92rem}
main{max-width:860px;margin:0 auto;padding:24px 18px 64px;display:flex;flex-direction:column;gap:16px}h1{font-size:1.5rem;margin:0}h2{font-size:.8rem;text-transform:uppercase;letter-spacing:.08em;color:var(--muted);margin:12px 0 0}
p{margin:0}.muted{color:var(--muted)}.small{font-size:.86rem}.card{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:14px 16px;display:flex;flex-direction:column;gap:10px}
.row{display:flex;gap:10px;align-items:center;flex-wrap:wrap}.between{justify-content:space-between}.mono,pre,code{font-family:ui-monospace,"SF Mono",Menlo,Consolas,monospace;font-size:.86rem}
pre{background:var(--bg);border:1px solid var(--line);border-radius:8px;padding:10px 12px;margin:0;overflow:auto;max-height:420px}
button,.button{font:inherit;border-radius:10px;border:1px solid var(--line);background:#1b1b1e;color:var(--text);padding:9px 16px;min-height:42px;cursor:pointer;display:inline-flex;align-items:center;justify-content:center;gap:8px}
.primary{background:var(--accent);border-color:var(--accent);color:var(--ink);font-weight:650}.danger{color:var(--bad)}.link{background:none;border:0;padding:0;min-height:0;color:var(--accent)}
input,textarea{font:inherit;font-size:16px;width:100%;background:var(--bg);color:var(--text);border:1px solid var(--line);border-radius:10px;padding:9px 12px}
form{margin:0}.inline{display:inline}.stack{display:flex;flex-direction:column;gap:10px}.badge{border:1px solid currentColor;border-radius:999px;padding:1px 9px;font-size:.78rem;font-weight:600}
.good{color:var(--good)}.bad{color:var(--bad)}.ask{color:var(--ask)}.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:8px}
.stat{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:12px 14px}.stat b{display:block;font-size:1.35rem}
.providers{display:flex;flex-direction:column;gap:10px;max-width:340px}.warn{color:var(--ask)}.center{max-width:420px;margin:40px auto;text-align:center;align-items:center}
details summary{cursor:pointer}
"#;

pub struct Viewer<'a> {
    pub user: Option<&'a User>,
    pub moderator: bool,
    pub csrf: String,
}

pub fn layout(title: &str, viewer: &Viewer, body: &str) -> String {
    let nav = match viewer.user {
        Some(user) => format!(
            r#"<a href="/entries">Entries</a><a href="/account">{}</a>{}<form method="post" action="/signout" class="inline"><input type="hidden" name="csrf" value="{}"><button class="link">Sign out</button></form>"#,
            esc(&user.handle()),
            if viewer.moderator { r#"<a href="/moderate">Moderate</a>"# } else { "" },
            esc(&viewer.csrf),
        ),
        None => r#"<a href="/entries">Entries</a><a href="/signin">Sign in</a>"#.to_string(),
    };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{} · Taskmaster Commons</title><style>{STYLE}</style></head><body>
<header><a class="brand" href="/">Taskmaster Commons</a><nav>{nav}</nav></header><main>{body}</main></body></html>"#,
        esc(title)
    )
}

pub fn providers(enabled: &[Provider], query: &str) -> String {
    let mut out = String::from(r#"<div class="providers">"#);
    for provider in enabled {
        out.push_str(&format!(
            r#"<a class="button primary" href="/auth/{}/start?{}">Continue with {}</a>"#,
            provider.id(),
            query,
            provider.label()
        ));
    }
    out.push_str("</div>");
    out
}

pub fn home(counts: &Counts, signed_in: bool, enabled: &[Provider]) -> String {
    format!(
        r#"<h1>The Commons</h1>
<p class="muted">Code every Taskmaster can cite: schemas, component APIs and patterns that worked in someone's build. Each Taskmaster pulls the entries that suit its project before it plans. What you share is reviewed by a person before anyone else gets it, and its pass rate is earned in other people's runs.</p>
<div class="stats"><div class="stat"><b>{}</b><span class="muted small">entries served</span></div><div class="stat"><b>{}</b><span class="muted small">waiting for review</span></div><div class="stat"><b>{}</b><span class="muted small">people</span></div></div>
{}"#,
        counts.approved,
        counts.pending,
        counts.users,
        if signed_in {
            r#"<p><a href="/account">Make a token for your Taskmaster</a> to share entries and report how they did.</p>"#.to_string()
        } else {
            format!("<h2>Sign in to share</h2>{}", providers(enabled, "next=%2Faccount"))
        }
    )
}

pub fn signin(enabled: &[Provider], next: &str) -> String {
    format!(
        r#"<div class="card center"><h1>Sign in</h1><p class="muted">Only your account id, your username or name, and a verified email are kept.</p>{}</div>"#,
        providers(enabled, &format!("next={}", enc(next)))
    )
}

pub fn message(title: &str, text: &str) -> String {
    format!(r#"<div class="card center"><h1>{}</h1><p class="muted">{}</p><a href="/">Back to the Commons</a></div>"#, esc(title), esc(text))
}

/// Ask before handing a sign-in token to a Taskmaster, and say which one.
pub fn authorize(destination: &str, redirect: &str, user: Option<&User>, enabled: &[Provider], csrf: &str) -> String {
    match user {
        None => format!(
            r#"<div class="card center"><h1>Sign in to your Taskmaster</h1><p class="muted">The Taskmaster at <b class="mono">{}</b> asked you to sign in.</p>{}</div>"#,
            esc(destination),
            providers(enabled, &format!("redirect_uri={}", enc(redirect)))
        ),
        Some(user) => format!(
            r#"<div class="card center"><h1>Sign in to your Taskmaster?</h1>
<p>You are signing in to the Taskmaster at <b class="mono">{}</b> as <b>{}</b>.</p>
<p class="muted small">Only continue if you opened that Taskmaster yourself. It will let you in only if your account is on its list.</p>
<form method="post" action="/authorize" class="stack"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="redirect_uri" value="{}">
<button class="primary">Continue</button></form><a href="/">Cancel</a></div>"#,
            esc(destination),
            esc(&user.handle()),
            esc(csrf),
            esc(redirect)
        ),
    }
}

fn status_badge(status: Status) -> &'static str {
    match status {
        Status::Pending => r#"<span class="badge ask">waiting for review</span>"#,
        Status::Approved => r#"<span class="badge good">served</span>"#,
        Status::Rejected => r#"<span class="badge bad">not served</span>"#,
    }
}

fn usage(entry: &Submission) -> String {
    let used = entry.passes + entry.fails;
    if used == 0 {
        return "not used yet".into();
    }
    format!(
        "{} run{}, {}% passed, {} {}",
        used,
        if used == 1 { "" } else { "s" },
        entry.passes * 100 / used,
        entry.users,
        if entry.users == 1 { "person" } else { "people" }
    )
}

pub fn account(user: &User, tokens: &[TokenInfo], mine: &[Submission], csrf: &str, url: &str, new_token: Option<&str>) -> String {
    let mut out = format!(
        r#"<h1>{}</h1><p class="muted small mono">{}{}</p>"#,
        esc(&user.handle()),
        esc(&user.sub()),
        user.email.as_deref().map(|e| format!(" · {}", esc(e))).unwrap_or_default()
    );
    if let Some(token) = new_token {
        out.push_str(&format!(
            r#"<div class="card"><b>Your new token. Copy it now: it is not shown again.</b><pre>{}</pre>
<p class="muted small">Put it in <span class="mono">.taskmaster/config.json</span> in your project, or <span class="mono">~/.taskmaster/config.json</span> for every project:</p>
<pre>{}</pre></div>"#,
            esc(token),
            esc(&format!(
                "{{\n  \"commons\": {{ \"remote\": \"{url}\", \"token\": \"{token}\", \"share\": true }},\n  \"auth\": {{ \"issuer\": \"{url}\", \"allow\": [\"{}\"] }}\n}}",
                allow_entry(user)
            ))
        ));
    }
    out.push_str(r#"<h2>Tokens for your Taskmasters</h2><div class="card">"#);
    if tokens.is_empty() {
        out.push_str(r#"<p class="muted">None yet. A token lets your Taskmaster share entries and report how they did.</p>"#);
    }
    for token in tokens {
        out.push_str(&format!(
            r#"<div class="row between"><span><b>{}</b> <span class="muted small">made {} · {}</span></span>
<form method="post" action="/account/tokens/{}/revoke"><input type="hidden" name="csrf" value="{}"><button class="danger">Revoke</button></form></div>"#,
            esc(&token.label),
            esc(&token.created_at[..10.min(token.created_at.len())]),
            token.last_used.as_deref().map(|at| format!("last used {}", esc(&at[..10.min(at.len())]))).unwrap_or_else(|| "never used".into()),
            esc(&token.id),
            esc(csrf)
        ));
    }
    out.push_str(&format!(
        r#"<form method="post" action="/account/tokens" class="row"><input type="hidden" name="csrf" value="{}"><input name="label" placeholder="Which machine, such as laptop" maxlength="60" required style="flex:1;min-width:200px"><button class="primary">Make a token</button></form></div>"#,
        esc(csrf)
    ));
    out.push_str("<h2>What you shared</h2>");
    if mine.is_empty() {
        out.push_str(r#"<p class="muted">Nothing yet. With <span class="mono">"share": true</span>, your Taskmaster sends an entry here once it has passed in your own runs.</p>"#);
    }
    for entry in mine {
        out.push_str(&format!(
            r#"<div class="card"><div class="row between"><b class="mono">{}</b>{}</div><span class="muted small">{} · {}</span>{}</div>"#,
            esc(&entry.key),
            status_badge(entry.status),
            esc(&entry.summary),
            usage(entry),
            entry.note.as_deref().map(|note| format!(r#"<span class="small">Reviewer's note: {}</span>"#, esc(note))).unwrap_or_default()
        ));
    }
    out
}

/// How this person goes in a Taskmaster's `auth.allow` list.
pub fn allow_entry(user: &User) -> String {
    match (user.provider.as_str(), &user.login, &user.email) {
        ("github", Some(login), _) => format!("github:{login}"),
        ("google", _, Some(email)) => format!("google:{email}"),
        _ => user.sub(),
    }
}

pub fn entries(list: &[Submission], query: &str, csrf: Option<&str>) -> String {
    let mut out = format!(
        r#"<h1>Entries</h1><form class="row" method="get"><input name="q" value="{}" placeholder="Search keys and summaries" style="flex:1"><button>Search</button></form>"#,
        esc(query)
    );
    if list.is_empty() {
        out.push_str(r#"<p class="muted">Nothing here yet.</p>"#);
    }
    for entry in list {
        out.push_str(&format!(
            r#"<details class="card"><summary><div class="row between"><b class="mono">{}</b><span class="muted small">{} · {}</span></div><span class="muted small">{}</span></summary>
<span class="muted small">by {} · {} · <span class="mono">{}</span></span><pre>{}</pre>{}</details>"#,
            esc(&entry.key),
            esc(&entry.stack),
            usage(entry),
            esc(&entry.summary),
            esc(&entry.author_handle),
            esc(&entry.created_at[..10.min(entry.created_at.len())]),
            esc(&entry.hash[..12.min(entry.hash.len())]),
            esc(&entry.content),
            match csrf {
                Some(csrf) => format!(
                    r#"<form method="post" action="/entries/{}/{}/report" class="row"><input type="hidden" name="csrf" value="{}"><input name="reason" placeholder="What is wrong with it?" maxlength="1000" required style="flex:1;min-width:200px"><button class="danger">Report</button></form>"#,
                    esc(&entry.key),
                    esc(&entry.hash),
                    esc(csrf)
                ),
                None => r#"<a class="small" href="/signin?next=%2Fentries">Sign in to report a problem</a>"#.to_string(),
            }
        ));
    }
    out
}

pub struct Queue<'a> {
    pub pending: &'a [Submission],
    pub reports: &'a [(Report, Option<Submission>)],
    pub struggling: &'a [Submission],
    pub csrf: &'a str,
}

pub fn moderate(queue: &Queue) -> String {
    let csrf = esc(queue.csrf);
    let mut out = format!(
        r#"<h1>This week's review</h1><p class="muted">{} waiting, {} reports, {} served entries failing more than they pass.</p><h2>Waiting for review, oldest first</h2>"#,
        queue.pending.len(),
        queue.reports.len(),
        queue.struggling.len()
    );
    if queue.pending.is_empty() {
        out.push_str(r#"<p class="muted">Nothing waiting.</p>"#);
    }
    for entry in queue.pending {
        let found = crate::scan::credentials(&entry.content);
        out.push_str(&format!(
            r#"<div class="card"><div class="row between"><b class="mono">{}</b><span class="muted small">{:?} · {}</span></div>
<span class="small">{}</span><span class="muted small">by {} on {}</span>{}<pre>{}</pre>
<form method="post" action="/moderate/entries/{}/{}" class="row"><input type="hidden" name="csrf" value="{csrf}"><input name="note" placeholder="Note to the author (optional)" maxlength="500" style="flex:1;min-width:200px">
<button class="primary" name="decision" value="approve">Approve</button><button class="danger" name="decision" value="reject">Reject</button></form></div>"#,
            esc(&entry.key),
            entry.kind,
            esc(&entry.stack),
            esc(&entry.summary),
            esc(&entry.author_handle),
            esc(&entry.created_at[..10.min(entry.created_at.len())]),
            if found.is_empty() { String::new() } else { format!(r#"<span class="warn small">Looks like it contains {}</span>"#, esc(&found.join(", "))) },
            esc(&entry.content),
            esc(&entry.key),
            esc(&entry.hash),
        ));
    }
    out.push_str("<h2>Reports</h2>");
    if queue.reports.is_empty() {
        out.push_str(r#"<p class="muted">No open reports.</p>"#);
    }
    for (report, entry) in queue.reports {
        out.push_str(&format!(
            r#"<div class="card"><div class="row between"><b class="mono">{}</b>{}</div><span>“{}”</span><span class="muted small">reported by {} on {}</span>{}
<form method="post" action="/moderate/reports/{}" class="row"><input type="hidden" name="csrf" value="{csrf}">
<button class="danger" name="action" value="withdraw">Withdraw the entry</button><button class="danger" name="action" value="ban">Withdraw and ban the author</button><button name="action" value="dismiss">Dismiss</button></form></div>"#,
            esc(&report.key),
            entry.as_ref().map(|e| status_badge(e.status)).unwrap_or(""),
            esc(&report.reason),
            esc(&report.reporter_handle),
            esc(&report.created_at[..10.min(report.created_at.len())]),
            entry
                .as_ref()
                .map(|e| format!(r#"<span class="muted small">by {} · {}</span><pre>{}</pre>"#, esc(&e.author_handle), usage(e), esc(&e.content)))
                .unwrap_or_default(),
            report.id,
        ));
    }
    out.push_str("<h2>Served and struggling</h2>");
    if queue.struggling.is_empty() {
        out.push_str(r#"<p class="muted">Every served entry passes more often than it fails.</p>"#);
    }
    for entry in queue.struggling {
        out.push_str(&format!(
            r#"<div class="card"><div class="row between"><b class="mono">{}</b><span class="muted small">{}</span></div><span class="muted small">by {}</span>
<details><summary class="small">Show</summary><pre>{}</pre></details>
<form method="post" action="/moderate/entries/{}/{}" class="row"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="note" value="withdrawn: it failed more often than it passed"><button class="danger" name="decision" value="reject">Withdraw</button></form></div>"#,
            esc(&entry.key),
            usage(entry),
            esc(&entry.author_handle),
            esc(&entry.content),
            esc(&entry.key),
            esc(&entry.hash),
        ));
    }
    out
}
