//! The Commons in Postgres. Written for Neon, and plain Postgres 14 or later works too.

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::{ConnectOptions, Row};
use tm_core::{DictionaryEntry, EntryKind};

use crate::db::{Counts, Db, Identity, Report, Status, Submission, TokenInfo, User};

const SCHEMA: &str = r#"
create table if not exists users (
    id bigserial primary key,
    provider text not null,
    subject text not null,
    login text,
    email text,
    name text,
    banned boolean not null default false,
    created_at timestamptz not null default now(),
    unique (provider, subject)
);
create table if not exists api_tokens (
    hash text primary key,
    user_id bigint not null references users(id) on delete cascade,
    label text not null,
    created_at timestamptz not null default now(),
    last_used timestamptz,
    revoked boolean not null default false
);
create index if not exists api_tokens_user on api_tokens(user_id);
create table if not exists entries (
    key text not null,
    hash text not null,
    kind text not null,
    summary text not null,
    content text not null,
    stack text not null,
    author bigint not null references users(id),
    status text not null default 'pending',
    note text,
    reviewed_by bigint references users(id),
    reviewed_at timestamptz,
    created_at timestamptz not null default now(),
    primary key (key, hash)
);
create index if not exists entries_status on entries(status);
create index if not exists entries_author on entries(author, created_at);
create table if not exists usage (
    key text not null,
    hash text not null,
    user_id bigint not null references users(id),
    passes bigint not null default 0,
    fails bigint not null default 0,
    primary key (key, hash, user_id),
    foreign key (key, hash) references entries(key, hash) on delete cascade
);
create table if not exists reports (
    id bigserial primary key,
    key text not null,
    hash text not null,
    reporter bigint not null references users(id),
    reason text not null,
    created_at timestamptz not null default now(),
    resolved boolean not null default false
);
create index if not exists reports_open on reports(resolved);
"#;

const STAMP: &str = r#"'YYYY-MM-DD"T"HH24:MI:SS"Z"'"#;

pub struct Postgres {
    pool: PgPool,
}

impl Postgres {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        // No named statements are kept, so a transaction-mode pooler in front works too.
        let options = PgConnectOptions::from_str(url)
            .context("DATABASE_URL is not a Postgres connection string")?
            .statement_cache_capacity(0)
            .disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(options)
            .await
            .context("cannot reach the database in DATABASE_URL")?;
        Ok(Postgres { pool })
    }
}

fn kind_text(kind: EntryKind) -> String {
    serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn kind_from(text: &str) -> anyhow::Result<EntryKind> {
    serde_json::from_value(serde_json::Value::String(text.into())).with_context(|| format!("unknown entry kind `{text}`"))
}

fn user_from(row: &PgRow) -> User {
    User {
        id: row.get("id"),
        provider: row.get("provider"),
        subject: row.get("subject"),
        login: row.get("login"),
        email: row.get("email"),
        name: row.get("name"),
        banned: row.get("banned"),
    }
}

fn submission_from(row: &PgRow) -> anyhow::Result<Submission> {
    let author = User {
        id: row.get("author"),
        provider: row.get("provider"),
        subject: row.get("subject"),
        login: row.get("login"),
        email: row.get("email"),
        name: row.get("name"),
        banned: row.get("banned"),
    };
    let status: String = row.get("status");
    Ok(Submission {
        key: row.get("key"),
        hash: row.get("hash"),
        kind: kind_from(row.get::<&str, _>("kind"))?,
        summary: row.get("summary"),
        content: row.get("content"),
        stack: row.get("stack"),
        author: author.id,
        author_handle: author.handle(),
        status: Status::parse(&status).with_context(|| format!("unknown status `{status}`"))?,
        created_at: row.get("created_at"),
        note: row.get("note"),
        passes: row.get::<i64, _>("passes") as u64,
        fails: row.get::<i64, _>("fails") as u64,
        users: row.get::<i64, _>("users") as u64,
    })
}

/// Every submission column, its author, and how it has done in use.
fn submissions_where(condition: &str) -> String {
    format!(
        "select e.key, e.hash, e.kind, e.summary, e.content, e.stack, e.author, e.status, e.note, \
                to_char(e.created_at at time zone 'UTC', {STAMP}) as created_at, \
                u.provider, u.subject, u.login, u.email, u.name, u.banned, \
                coalesce(sum(g.passes), 0)::bigint as passes, \
                coalesce(sum(g.fails), 0)::bigint as fails, \
                count(g.user_id)::bigint as users \
         from entries e \
         join users u on u.id = e.author \
         left join usage g on g.key = e.key and g.hash = e.hash \
         where {condition} \
         group by e.key, e.hash, u.id \
         order by e.created_at"
    )
}

#[async_trait]
impl Db for Postgres {
    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::raw_sql(SCHEMA).execute(&self.pool).await.context("cannot create the tables")?;
        Ok(())
    }

    async fn upsert_user(&self, identity: &Identity) -> anyhow::Result<User> {
        let row = sqlx::query(
            "insert into users (provider, subject, login, email, name) values ($1, $2, $3, $4, $5) \
             on conflict (provider, subject) do update \
             set login = excluded.login, email = excluded.email, name = excluded.name \
             returning id, provider, subject, login, email, name, banned",
        )
        .bind(&identity.provider)
        .bind(&identity.subject)
        .bind(&identity.login)
        .bind(&identity.email)
        .bind(&identity.name)
        .fetch_one(&self.pool)
        .await?;
        Ok(user_from(&row))
    }

    async fn user(&self, id: i64) -> anyhow::Result<Option<User>> {
        let row = sqlx::query("select id, provider, subject, login, email, name, banned from users where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(user_from))
    }

    async fn set_banned(&self, id: i64, banned: bool) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("update users set banned = $2 where id = $1").bind(id).bind(banned).execute(&mut *tx).await?;
        if banned {
            sqlx::query("update entries set status = 'rejected', note = 'the author was banned' where author = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("update api_tokens set revoked = true where user_id = $1").bind(id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn add_token(&self, user: i64, hash: &str, label: &str) -> anyhow::Result<()> {
        sqlx::query("insert into api_tokens (hash, user_id, label) values ($1, $2, $3)")
            .bind(hash)
            .bind(user)
            .bind(label)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn token_user(&self, hash: &str) -> anyhow::Result<Option<User>> {
        let row = sqlx::query(
            "with used as (update api_tokens set last_used = now() where hash = $1 and not revoked returning user_id) \
             select u.id, u.provider, u.subject, u.login, u.email, u.name, u.banned from users u join used on used.user_id = u.id",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(user_from))
    }

    async fn tokens(&self, user: i64) -> anyhow::Result<Vec<TokenInfo>> {
        let rows = sqlx::query(&format!(
            "select left(hash, 12) as id, label, to_char(created_at at time zone 'UTC', {STAMP}) as created_at, \
                    to_char(last_used at time zone 'UTC', {STAMP}) as last_used \
             from api_tokens where user_id = $1 and not revoked order by created_at"
        ))
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| TokenInfo {
                id: row.get("id"),
                label: row.get("label"),
                created_at: row.get("created_at"),
                last_used: row.get("last_used"),
            })
            .collect())
    }

    async fn revoke_token(&self, user: i64, id: &str) -> anyhow::Result<bool> {
        if id.len() < 12 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(false);
        }
        let done = sqlx::query("update api_tokens set revoked = true where user_id = $1 and left(hash, $3) = $2 and not revoked")
            .bind(user)
            .bind(id)
            .bind(id.len() as i32)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn submit(&self, entry: &DictionaryEntry, author: i64) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "insert into entries (key, hash, kind, summary, content, stack, author) values ($1, $2, $3, $4, $5, $6, $7) \
             on conflict (key, hash) do nothing",
        )
        .bind(&entry.key)
        .bind(&entry.hash)
        .bind(kind_text(entry.kind))
        .bind(&entry.summary)
        .bind(&entry.content)
        .bind(&entry.stack)
        .bind(author)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn submitted_since_yesterday(&self, author: i64) -> anyhow::Result<u64> {
        let n: i64 = sqlx::query_scalar("select count(*) from entries where author = $1 and created_at > now() - interval '1 day'")
            .bind(author)
            .fetch_one(&self.pool)
            .await?;
        Ok(n as u64)
    }

    async fn submissions_by(&self, author: i64) -> anyhow::Result<Vec<Submission>> {
        let rows = sqlx::query(&submissions_where("e.author = $1")).bind(author).fetch_all(&self.pool).await?;
        let mut out = rows.iter().map(submission_from).collect::<anyhow::Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }

    async fn submissions(&self, status: Status) -> anyhow::Result<Vec<Submission>> {
        let rows = sqlx::query(&submissions_where("e.status = $1")).bind(status.as_str()).fetch_all(&self.pool).await?;
        rows.iter().map(submission_from).collect()
    }

    async fn submission(&self, key: &str, hash: &str) -> anyhow::Result<Option<Submission>> {
        let row =
            sqlx::query(&submissions_where("e.key = $1 and e.hash = $2")).bind(key).bind(hash).fetch_optional(&self.pool).await?;
        row.as_ref().map(submission_from).transpose()
    }

    async fn review(&self, key: &str, hash: &str, status: Status, moderator: i64, note: Option<&str>) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "update entries set status = $3, note = $5, reviewed_by = $4, reviewed_at = now() where key = $1 and hash = $2",
        )
        .bind(key)
        .bind(hash)
        .bind(status.as_str())
        .bind(moderator)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn record_usage(&self, key: &str, hash: &str, user: i64, passed: bool) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "insert into usage (key, hash, user_id, passes, fails) \
             select key, hash, $3, $4, $5 from entries where key = $1 and hash = $2 and status = 'approved' \
             on conflict (key, hash, user_id) do update \
             set passes = usage.passes + excluded.passes, fails = usage.fails + excluded.fails",
        )
        .bind(key)
        .bind(hash)
        .bind(user)
        .bind(i64::from(passed))
        .bind(i64::from(!passed))
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn report(&self, key: &str, hash: &str, user: i64, reason: &str) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "insert into reports (key, hash, reporter, reason) \
             select key, hash, $3, $4 from entries where key = $1 and hash = $2",
        )
        .bind(key)
        .bind(hash)
        .bind(user)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn open_reports(&self) -> anyhow::Result<Vec<Report>> {
        let rows = sqlx::query(&format!(
            "select r.id, r.key, r.hash, r.reporter, r.reason, to_char(r.created_at at time zone 'UTC', {STAMP}) as created_at, \
                    u.id as uid, u.provider, u.subject, u.login, u.email, u.name, u.banned \
             from reports r join users u on u.id = r.reporter where not r.resolved order by r.created_at"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|row| {
                let reporter = User {
                    id: row.get("uid"),
                    provider: row.get("provider"),
                    subject: row.get("subject"),
                    login: row.get("login"),
                    email: row.get("email"),
                    name: row.get("name"),
                    banned: row.get("banned"),
                };
                Report {
                    id: row.get("id"),
                    key: row.get("key"),
                    hash: row.get("hash"),
                    reporter: reporter.id,
                    reporter_handle: reporter.handle(),
                    reason: row.get("reason"),
                    created_at: row.get("created_at"),
                }
            })
            .collect())
    }

    async fn resolve_report(&self, id: i64) -> anyhow::Result<bool> {
        let done = sqlx::query("update reports set resolved = true where id = $1 and not resolved").bind(id).execute(&self.pool).await?;
        Ok(done.rows_affected() > 0)
    }

    async fn counts(&self) -> anyhow::Result<Counts> {
        let row = sqlx::query(
            "select (select count(*) from entries where status = 'approved') as approved, \
                    (select count(*) from entries where status = 'pending') as pending, \
                    (select count(*) from users) as users, \
                    (select count(*) from reports where not resolved) as open_reports",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(Counts {
            approved: row.get::<i64, _>("approved") as u64,
            pending: row.get::<i64, _>("pending") as u64,
            users: row.get::<i64, _>("users") as u64,
            open_reports: row.get::<i64, _>("open_reports") as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    /// Runs the shared suite against a real database when `TM_COMMONS_TEST_DATABASE` names
    /// one. The database should be empty: the suite expects to make the first users.
    #[tokio::test]
    async fn postgres_keeps_the_commons() {
        let Ok(url) = std::env::var("TM_COMMONS_TEST_DATABASE") else {
            eprintln!("TM_COMMONS_TEST_DATABASE is not set; skipping the Postgres suite");
            return;
        };
        let db = super::Postgres::connect(&url).await.unwrap();
        crate::db::tests::exercise(&db).await;
    }
}
