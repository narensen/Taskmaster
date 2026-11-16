//! The Taskmaster Commons: one service everyone's Taskmaster shares.
//!
//! - Sign-in with GitHub or Google. A Taskmaster sends people here to sign in, and gets
//!   back a token signed with this service's key (see `tm-token`).
//! - The Commons: entries people chose to share, served once a moderator approved them,
//!   with pass rates earned in other people's runs.
//! - The weekly review: moderators approve or reject what is waiting, act on reports, and
//!   withdraw entries that fail more than they pass.

pub mod app;
pub mod config;
pub mod db;
pub mod oauth;
pub mod pages;
pub mod pg;
pub mod scan;
pub mod seal;
pub mod time;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use anyhow::Context;

pub use app::{App, router};
pub use config::Config;
pub use db::{Db, Memory};

/// `memory` keeps everything in this process; anything else is a Postgres URL.
///
/// `direct` is an optional non-pooled URL used only to create the tables; the service
/// itself then runs over `url`, which may be a transaction-mode pooler.
pub async fn open_db(url: &str, direct: Option<&str>) -> anyhow::Result<Arc<dyn Db>> {
    if url == "memory" {
        eprintln!("tm-commons: DATABASE_URL is `memory`: nothing survives a restart");
        let db: Arc<dyn Db> = Arc::new(Memory::default());
        db.migrate().await?;
        return Ok(db);
    }
    match direct {
        Some(direct) if direct != url => {
            let migrator = pg::Postgres::connect(direct).await.context("DATABASE_URL_DIRECT")?;
            migrator.migrate().await?;
        }
        _ => {}
    }
    let db: Arc<dyn Db> = Arc::new(pg::Postgres::connect(url).await?);
    if direct.is_none_or(|d| d == url) {
        db.migrate().await?;
    }
    Ok(db)
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    if config.moderators.is_empty() {
        eprintln!("tm-commons: TM_COMMONS_MODERATORS is empty, so nobody can review entries");
    }
    let bind = config.bind;
    let url = config.url.clone();
    let db = open_db(&config.database, config.database_direct.as_deref()).await?;
    let app = Arc::new(App::new(config, db)?);
    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("cannot listen on {bind}"))?;
    eprintln!("tm-commons: {url} (listening on {bind})");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
