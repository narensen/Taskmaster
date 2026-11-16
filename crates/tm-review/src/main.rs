//! `tm-review`: review a range of commits in any git repository.
//!
//! ```text
//! tm-review <repo | url> [--base <rev>] [--head <rev>] [--pr <n>] [--dirty]
//!           [--reviewer <harness>] [--verifier <harness>]
//!           [--reviewer-model <m>] [--verifier-model <m>] [--install-lints] [--keep]
//! ```
//!
//! `url` is a GitHub, GitLab, Bitbucket, Gitea, Forgejo, Codeberg, Azure DevOps or SourceHut
//! repository or request. `--pr` reviews that request on the local `origin`. `--dirty`
//! reviews uncommitted changes.

use anyhow::{Context, bail};
use tm_harness::harness;
use tm_review::run::{Options, run};

struct Args {
    options: Options,
    reviewer: String,
    verifier: String,
}

fn args() -> anyhow::Result<Args> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: tm-review <repo | url> [--base <rev>] [--head <rev>] [--pr <n>] [--dirty] \
                 [--reviewer <harness>] [--verifier <harness>] [--reviewer-model <m>] \
                 [--verifier-model <m>] [--install-lints] [--keep]\n\
                 url: GitHub, GitLab, Bitbucket, Gitea, Forgejo, Codeberg, Azure DevOps, SourceHut\n\
                 harnesses: claude, agy, grok, codex, opencode";
    let spec = args.next().context(usage)?;
    let mut o = Options::new(spec);
    // Different harnesses by default, so the verifier does not share the reviewer's blind spots.
    let mut reviewer = String::from("claude");
    let mut verifier = String::from("agy");
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--keep" => {
                o.keep = true;
                continue;
            }
            "--install-lints" => {
                o.install_lints = true;
                continue;
            }
            "--dirty" => {
                o.dirty = true;
                continue;
            }
            _ => {}
        }
        let value = args.next().with_context(|| format!("{flag} needs a value\n{usage}"))?;
        match flag.as_str() {
            "--base" => o.base = Some(value),
            "--head" => o.head = Some(value),
            "--pr" => {
                o.pr = Some(value.parse().with_context(|| format!("`--pr` needs a request number\n{usage}"))?);
            }
            "--reviewer" => reviewer = value,
            "--verifier" => verifier = value,
            "--reviewer-model" => o.reviewer_model = Some(value),
            "--verifier-model" => o.verifier_model = Some(value),
            other => bail!("unknown option `{other}`\n{usage}"),
        }
    }
    Ok(Args { options: o, reviewer, verifier })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = args()?;
    let report = run(&args.options, harness(&args.reviewer)?, harness(&args.verifier)?).await?;
    print!("{}", report.render());
    Ok(())
}
