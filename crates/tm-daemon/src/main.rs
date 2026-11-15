//! `tm-daemon`: carry out a Taskmaster plan.
//!
//! ```text
//! tm-daemon providers [--harness <h>]
//! tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] [--yes]
//!           [--worker <h>] [--mid <h>] [--master <h>]
//!           [--reviewer <h>] [--verifier <h>]
//!           [--worker-model <m>] [--mid-model <m>] [--master-model <m>]
//!           [--reviewer-model <m>] [--verifier-model <m>]
//!           [--parallel <n>] [--timeout <secs>] [--dictionary <dir>] [--commons <dir>]
//!           [--run <id>] [--eval] [--baseline <h>] [--pair]
//! ```
//!
//! Harnesses are `claude`, `agy`, `grok`, `codex` and `opencode`. The worker defaults
//! to `agy`, the reviewer and the Taskmaster to `claude`, and the verifier to `agy`.
//! `--master` chooses the Taskmaster harness. `providers` asks every harness for the
//! models it offers right now and prints JSON the UI can use to install one and sign in.
//! A model flag must name an id from that list. Leaving the flag off keeps the
//! provider's own default and does not ask for the list.
//! `--prompt` asks the Taskmaster harness to write the plan and its Dictionary entries.
//! `--plan` skips that and uses the file. Without `--yes` the plan is recorded and
//! printed; nothing is built until a later run passes `--yes`. A run resumes from
//! `.taskmaster/runs/<id>/`. `--eval` runs the plan again with one model and writes
//! `.taskmaster/evals/<id>.json`. A prompt is planned once and reused for the baseline.
//! `--pair` serves the phone UI on this machine's Tailscale address.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use tm_daemon::{Config, Harnesses, Models, Outcome, PhoneEndpoint, evaluate, is_tailscale, tailscale_ipv4};
use tm_harness::{harness, provider_report, provider_reports, require_model, HARNESSES};

struct Args {
    project: PathBuf,
    plan: Option<PathBuf>,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
    yes: bool,
    parallel: usize,
    timeout: u64,
    dictionary: Option<PathBuf>,
    commons: Option<PathBuf>,
    run_id: String,
    eval: bool,
    baseline: String,
    pair: bool,
    worker: String,
    mid: String,
    master: String,
    reviewer: String,
    verifier: String,
    models: Models,
}

fn main() -> ExitCode {
    match run() {
        Ok(Outcome::Finished { ok: true }) => ExitCode::SUCCESS,
        Ok(Outcome::Finished { ok: false }) => ExitCode::from(1),
        Ok(Outcome::NeedsApproval { .. }) => ExitCode::from(2),
        Err(err) => {
            eprintln!("taskmaster: {err:#}");
            ExitCode::from(1)
        }
    }
}

#[tokio::main]
async fn run() -> anyhow::Result<Outcome> {
    let args = match invocation_from(std::env::args().skip(1))? {
        Invocation::Providers { harness } => {
            print_providers(harness.as_deref()).await?;
            return Ok(Outcome::Finished { ok: true });
        }
        Invocation::Drive(args) => args,
    };
    validate_models(&args).await?;
    let mut config = Config::new(&args.project);
    config.plan = match &args.plan {
        Some(path) => Some(
            serde_json::from_reader(std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?)
                .with_context(|| format!("{} is not a plan", path.display()))?,
        ),
        None => None,
    };
    config.prompt = if let Some(text) = args.prompt.clone() {
        Some(text)
    } else if let Some(path) = &args.prompt_file {
        Some(std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?)
    } else {
        None
    };
    if config.prompt.as_ref().is_some_and(|text| text.trim().is_empty()) {
        bail!("the prompt is empty");
    }
    config.yes = args.yes;
    config.parallel = args.parallel;
    config.timeout = Duration::from_secs(args.timeout);
    config.dictionary = args.dictionary;
    config.commons = args.commons.clone().or_else(home_commons);
    config.run_id = args.run_id.clone();
    config.models = args.models;
    let phone = if args.pair { Some(serve_phone(&args.project).await?) } else { None };
    if let Some(phone) = &phone {
        println!("taskmaster: phone {}  code {}", phone.url, phone.code);
    }
    let has_request = args.plan.is_some() || config.prompt.is_some();
    if !has_request && !args.eval {
        if phone.is_some() {
            std::future::pending::<()>().await;
        }
        bail!("pass --plan or --prompt, or --pair to serve the phone");
    }
    let harnesses = Harnesses {
        worker: harness(&args.worker)?,
        mid: harness(&args.mid)?,
        master: harness(&args.master)?,
        reviewer: harness(&args.reviewer)?,
        verifier: harness(&args.verifier)?,
    };
    let outcome = if args.eval {
        let baseline = harness(&args.baseline)?;
        let (outcome, report) = evaluate(config, harnesses, baseline, &args.baseline).await?;
        println!(
            "taskmaster: eval taskmaster ({}) {}",
            report.taskmaster.harness,
            eval_line(&report.taskmaster)
        );
        if let Some(baseline) = &report.baseline {
            println!("taskmaster: eval baseline ({}) {}", baseline.harness, eval_line(baseline));
        }
        outcome
    } else {
        tm_daemon::drive(config, harnesses).await?
    };
    drop(phone);
    Ok(outcome)
}

fn eval_line(side: &tm_daemon::EvalSide) -> String {
    let state = if side.ok { "finished" } else { "failed" };
    format!("{state}, {} passed, {} failed, {} check failures", side.passed, side.failed, side.check_failures)
}

fn home_commons() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".taskmaster/commons"))
}

async fn serve_phone(project: &std::path::Path) -> anyhow::Result<PhoneEndpoint> {
    let ip = tailscale_ipv4()?;
    if !is_tailscale(ip) {
        bail!("{ip} is not a Tailscale address");
    }
    let primary = SocketAddr::from((IpAddr::V4(ip), 8737));
    match PhoneEndpoint::serve(project, primary).await {
        Ok(endpoint) => Ok(endpoint),
        Err(_) => PhoneEndpoint::serve(project, SocketAddr::from((IpAddr::V4(ip), 0))).await,
    }
}

enum Invocation {
    Providers { harness: Option<String> },
    Drive(Args),
}

fn invocation_from(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Invocation> {
    let argv: Vec<String> = argv.into_iter().collect();
    if argv.first().is_some_and(|arg| arg == "providers") {
        return Ok(Invocation::Providers { harness: providers_harness(&argv[1..])? });
    }
    Ok(Invocation::Drive(args_from(argv)?))
}

fn providers_harness(argv: &[String]) -> anyhow::Result<Option<String>> {
    let mut argv = argv.iter();
    let mut harness = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--harness" => {
                let name = argv.next().context("--harness needs a harness id")?;
                if !HARNESSES.contains(&name.as_str()) {
                    bail!("unknown harness `{name}`: use one of {}", HARNESSES.join(", "));
                }
                harness = Some(name.clone());
            }
            other => bail!("unknown argument `{other}`\nusage: tm-daemon providers [--harness <h>]"),
        }
    }
    Ok(harness)
}

async fn print_providers(harness: Option<&str>) -> anyhow::Result<()> {
    let reports = match harness {
        Some(id) => vec![provider_report(id).await],
        None => provider_reports().await,
    };
    println!("{}", serde_json::to_string_pretty(&reports)?);
    Ok(())
}

async fn validate_models(args: &Args) -> anyhow::Result<()> {
    let roles = [
        ("--worker-model", args.worker.as_str(), args.models.worker.as_deref()),
        ("--mid-model", args.mid.as_str(), args.models.mid.as_deref()),
        ("--master-model", args.master.as_str(), args.models.master.as_deref()),
        ("--reviewer-model", args.reviewer.as_str(), args.models.reviewer.as_deref()),
        ("--verifier-model", args.verifier.as_str(), args.models.verifier.as_deref()),
    ];
    let mut groups: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (flag, id, model) in roles {
        let Some(model) = model else { continue };
        if !HARNESSES.contains(&id) {
            bail!("unknown harness `{id}`: use one of {}", HARNESSES.join(", "));
        }
        groups.entry(id).or_default().push((flag, model));
    }
    for (id, chosen) in groups {
        let report = provider_report(id).await;
        if let Some(note) = report.error.as_deref().filter(|_| !report.models.is_empty()) {
            eprintln!("taskmaster: {id}: {note}");
        }
        for (flag, model) in chosen {
            require_model(&report, flag, model)?;
        }
    }
    Ok(())
}

fn args_from(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Args> {
    let usage = "usage: tm-daemon providers [--harness <h>]\n       \
                 tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] \
                 [--yes] [--worker <h>] [--mid <h>] [--master <h>] [--reviewer <h>] \
                 [--verifier <h>] [--worker-model <m>] [--mid-model <m>] [--master-model <m>] \
                 [--reviewer-model <m>] [--verifier-model <m>] [--parallel <n>] \
                 [--timeout <secs>] [--dictionary <dir>] [--commons <dir>] [--run <id>] \
                 [--eval] [--baseline <h>] [--pair]\n\
                 harnesses: claude, agy, grok, codex, opencode";
    let mut argv = argv.into_iter();
    let project = argv.next().context(usage)?;
    if project.starts_with('-') {
        bail!("{usage}");
    }
    let mut args = Args {
        project: PathBuf::from(project),
        plan: None,
        prompt: None,
        prompt_file: None,
        yes: false,
        parallel: 4,
        timeout: 600,
        dictionary: None,
        commons: None,
        run_id: "current".into(),
        eval: false,
        baseline: "claude".into(),
        pair: false,
        // The reviewer and the verifier are different harnesses, so one model does not
        // check its own work. The worker is the cheap one.
        worker: "agy".into(),
        mid: "grok".into(),
        master: "claude".into(),
        reviewer: "claude".into(),
        verifier: "agy".into(),
        models: Models::default(),
    };
    let mut baseline_set = false;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--yes" => args.yes = true,
            "--plan" => args.plan = Some(need(&mut argv, "--plan", usage)?.into()),
            "--prompt" => args.prompt = Some(need(&mut argv, "--prompt", usage)?),
            "--prompt-file" => args.prompt_file = Some(need(&mut argv, "--prompt-file", usage)?.into()),
            "--worker" => args.worker = need(&mut argv, "--worker", usage)?,
            "--mid" => args.mid = need(&mut argv, "--mid", usage)?,
            "--master" => args.master = need(&mut argv, "--master", usage)?,
            "--reviewer" => args.reviewer = need(&mut argv, "--reviewer", usage)?,
            "--verifier" => args.verifier = need(&mut argv, "--verifier", usage)?,
            "--worker-model" => args.models.worker = Some(need(&mut argv, "--worker-model", usage)?),
            "--mid-model" => args.models.mid = Some(need(&mut argv, "--mid-model", usage)?),
            "--master-model" => args.models.master = Some(need(&mut argv, "--master-model", usage)?),
            "--reviewer-model" => args.models.reviewer = Some(need(&mut argv, "--reviewer-model", usage)?),
            "--verifier-model" => args.models.verifier = Some(need(&mut argv, "--verifier-model", usage)?),
            "--parallel" => {
                args.parallel = need(&mut argv, "--parallel", usage)?.parse().context("--parallel needs a number")?
            }
            "--timeout" => {
                args.timeout = need(&mut argv, "--timeout", usage)?.parse().context("--timeout needs seconds")?
            }
            "--dictionary" => args.dictionary = Some(need(&mut argv, "--dictionary", usage)?.into()),
            "--commons" => args.commons = Some(need(&mut argv, "--commons", usage)?.into()),
            "--run" => args.run_id = need(&mut argv, "--run", usage)?,
            "--eval" => args.eval = true,
            "--baseline" => {
                args.baseline = need(&mut argv, "--baseline", usage)?;
                baseline_set = true;
            }
            "--pair" => args.pair = true,
            other => bail!("unknown argument `{other}`\n{usage}"),
        }
    }
    if !baseline_set {
        args.baseline = args.master.clone();
    }
    if args.plan.is_some() && (args.prompt.is_some() || args.prompt_file.is_some()) {
        bail!("pass only one of --plan and --prompt\n{usage}");
    }
    if args.prompt.is_some() && args.prompt_file.is_some() {
        bail!("pass only one of --prompt and --prompt-file\n{usage}");
    }
    if args.prompt.as_ref().is_some_and(|text| text.trim().is_empty()) {
        bail!("--prompt is empty\n{usage}");
    }
    if args.eval && args.plan.is_none() && args.prompt.is_none() && args.prompt_file.is_none() {
        bail!("--eval needs --plan or --prompt\n{usage}");
    }
    Ok(args)
}

#[cfg(test)]
mod args_tests {
    use super::*;

    fn parse(line: &str) -> anyhow::Result<Args> {
        args_from(line.split_whitespace().map(str::to_string))
    }

    fn rejected(line: &str) -> String {
        match parse(line) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("{line} was accepted"),
        }
    }

    #[test]
    fn prompt_and_plan_are_separate_requests() {
        let args = parse("proj --prompt build-a-page --eval").unwrap();
        assert_eq!(args.prompt.as_deref(), Some("build-a-page"));
        assert!(args.eval);
        assert!(args.plan.is_none());
        assert!(rejected("proj --plan p.json --prompt hi").contains("only one"));
        assert!(rejected("proj --prompt hi --prompt-file p.txt").contains("only one"));
        assert!(rejected("proj --eval").contains("--eval needs"));
        assert!(parse("proj --prompt-file brief.txt").unwrap().prompt_file.is_some());
    }

    #[test]
    fn the_taskmaster_harness_and_its_model_are_chosen() {
        let args = parse("proj --master grok --master-model grok-4.7 --plan p.json").unwrap();
        assert_eq!(args.master, "grok");
        assert_eq!(args.models.master.as_deref(), Some("grok-4.7"));
        assert_eq!(args.baseline, "grok");
    }

    #[test]
    fn providers_lists_models_without_a_project() {
        match invocation_from(["providers".to_string()]).unwrap() {
            Invocation::Providers { harness: None } => {}
            Invocation::Drive(_) => panic!("providers was read as a project"),
            Invocation::Providers { harness: Some(name) } => panic!("unexpected filter {name}"),
        }
        match invocation_from(["providers".into(), "--harness".into(), "grok".into()]).unwrap() {
            Invocation::Providers { harness: Some(name) } => assert_eq!(name, "grok"),
            _ => panic!("expected one harness"),
        }
        let err = match invocation_from(["providers".into(), "--harness".into(), "nope".into()]) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("unknown harness was accepted"),
        };
        assert!(err.contains("unknown harness"), "{err}");
    }
}

fn need(argv: &mut impl Iterator<Item = String>, flag: &str, usage: &str) -> anyhow::Result<String> {
    argv.next().with_context(|| format!("{flag} needs a value\n{usage}"))
}
