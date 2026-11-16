//! `tm-commons`: the shared Commons and sign-in service.
//!
//! ```text
//! tm-commons serve     run it; configured by environment variables
//! tm-commons check     check the configuration and the database, then stop
//! tm-commons keygen    print a new signing key and session secret for the .env file
//! ```

use std::io::Read;
use std::process::ExitCode;

use tm_commons::Config;

fn random(n: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

async fn run() -> anyhow::Result<()> {
    let usage = "usage: tm-commons serve | check | keygen";
    match std::env::args().nth(1).as_deref() {
        Some("serve") => tm_commons::serve(Config::from_env()?).await,
        Some("check") => {
            let config = Config::from_env()?;
            tm_commons::open_db(&config.database, config.database_direct.as_deref()).await?;
            tm_token::Signer::from_seed(&config.signing_key)?;
            let providers: Vec<&str> = tm_commons::oauth::Provider::enabled(&config).iter().map(|p| p.label()).collect();
            println!("ok: {} with sign-in by {}", config.url, providers.join(" and "));
            if config.moderators.is_empty() {
                println!("warning: TM_COMMONS_MODERATORS is empty");
            }
            Ok(())
        }
        Some("keygen") => {
            println!("TM_COMMONS_SIGNING_KEY={}", tm_token::Signer::seed_from(&random(32)?)?);
            let secret: String = random(32)?.iter().map(|b| format!("{b:02x}")).collect();
            println!("TM_COMMONS_SESSION_SECRET={secret}");
            Ok(())
        }
        _ => anyhow::bail!("{usage}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tm-commons: {err:#}");
            ExitCode::FAILURE
        }
    }
}
