//! `tm-daemon`: carry out a Taskmaster plan.
//!
//! ```text
//! tm-daemon providers [--harness <h>]
//! tm-daemon serve <project> [--bind <addr>] [--tailscale] [--dictionary <dir>] [--commons <dir>]
//!                 [--web <dir>]
//! tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] [--yes]
//!           [--worker <h>] [--mid <h>] [--master <h>]
//!           [--reviewer <h>] [--verifier <h>]
//!           [--worker-model <m>] [--mid-model <m>] [--master-model <m>]
//!           [--reviewer-model <m>] [--verifier-model <m>]
//!           [--parallel <n>] [--timeout <secs>] [--dictionary <dir>] [--commons <dir>]
//!           [--run <id>] [--template <next | dir>] [--eval] [--baseline <h>] [--pair]
//!           [--no-runtime-checks]
//! ```
//!
//! Harnesses are `claude`, `agy`, `grok`, `codex` and `opencode`. There is no built-in
//! choice: a role left off the command line takes the harness saved in the settings
//! file, and the run stops if neither names one. `--master` chooses the Taskmaster harness. `providers` asks every harness for the
//! models it offers right now and prints JSON the UI can use to install one and sign in.
//! A model flag must name an id from that list. Leaving the flag off keeps the
//! provider's own default and does not ask for the list.
//! `--prompt` asks the Taskmaster harness to write the plan and its Dictionary entries.
//! `--plan` skips that and uses the file. Without `--yes` the plan is recorded and
//! printed; nothing is built until a later run passes `--yes`. A run resumes from
//! `.taskmaster/runs/<id>/`. `--eval` runs the plan again with one model and writes
//! `.taskmaster/evals/<id>.json`. A prompt is planned once and reused for the baseline.
//! `--timeout 0` lets an agent call run with no time limit. The exit code is 0 when the
//! run finishes clean, 1 when it fails, 2 when the plan waits for `--yes`, and 3 when a
//! provider stopped it (a usage limit, no balance left, a sign-in): run the same command
//! again to resume.
//! `--pair` serves the API and the phone page on this machine's Tailscale address for as
//! long as the run lasts. `serve` keeps the API up with no run of its own: a frontend
//! starts, approves and cancels runs through it. It listens on `127.0.0.1:8737` unless
//! `--bind` or `--tailscale` says otherwise.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};

use tm_daemon::{
    Config, Harnesses, Models, Outcome, Roles, Server, ServerOptions, Settings, check_models, evaluate, tailscale_ipv4,
};
use tm_harness::{harness, provider_report, provider_reports, HARNESSES};

struct Args {
    project: PathBuf,
    plan: Option<PathBuf>,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
    yes: bool,
    /// Skip the project's build, its tests and the browser.
    no_runtime_checks: bool,
    parallel: usize,
    /// Seconds per agent call; 0 is no limit. None takes the settings file's limit.
    timeout: Option<u64>,
    dictionary: Option<PathBuf>,
    commons: Option<PathBuf>,
    run_id: String,
    template: Option<String>,
    eval: bool,
    baseline: String,
    pair: bool,
    roles: Roles,
    models: Models,
}

struct ServeArgs {
    project: PathBuf,
    bind: Option<SocketAddr>,
    tailscale: bool,
    dictionary: Option<PathBuf>,
    commons: Option<PathBuf>,
    web: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(Outcome::Finished { ok: true }) => ExitCode::SUCCESS,
        Ok(Outcome::Finished { ok: false }) => ExitCode::from(1),
        Ok(Outcome::NeedsApproval { .. } | Outcome::Rejected) => ExitCode::from(2),
        // A provider stopped the run (a usage limit, no balance, a sign-in): the run is kept
        // where it stopped, and the same command picks it up again.
        Err(err) if err.downcast_ref::<tm_daemon::reply::ReportedLimit>().is_some() => {
            eprintln!("taskmaster: {err:#}");
            eprintln!(
                "taskmaster: the run is kept where it stopped. Run the same command again to resume it, \
                 or pass a different --master/--mid/--worker/--reviewer/--verifier to switch the provider."
            );
            ExitCode::from(3)
        }
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
        Invocation::Serve(args) => {
            serve(args).await?;
            return Ok(Outcome::Finished { ok: true });
        }
        Invocation::Drive(args) => *args,
    };
    let mut args = args;
    {
        let project = args.project.canonicalize().unwrap_or_else(|_| args.project.clone());
        let chosen = match serde_json::to_value(&args.roles)? {
            serde_json::Value::Object(map) => map,
            _ => Default::default(),
        };
        args.roles = Roles::resolve(&Settings::saved_roles(&project), &chosen)
            .context("pass --master, --mid, --worker, --reviewer and --verifier, or save them in Settings")?;
        if args.baseline.is_empty() {
            args.baseline = args.roles.master.clone();
        }
    }
    check_models(&args.roles, &args.models).await?;
    let mut config = Config::new(&args.project);
    config.skills = Some(tm_daemon::skills::default_root());
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
    config.runtime_checks = !args.no_runtime_checks;
    config.parallel = args.parallel;
    config.timeout = match args.timeout {
        Some(secs) => tm_daemon::settings::agent_limit(secs),
        None => {
            let project = args.project.canonicalize().unwrap_or_else(|_| args.project.clone());
            tm_daemon::settings::agent_limit(Settings::load(&project).map(|s| s.agents.timeout_secs).unwrap_or(tm_daemon::settings::DEFAULT_AGENT_SECS))
        }
    };
    config.dictionary = args.dictionary;
    config.commons = args.commons.clone().or_else(home_commons);
    config.run_id = args.run_id.clone();
    config.template = args.template.clone();
    config.models = args.models;
    let settings = Settings::load(&args.project.canonicalize().unwrap_or_else(|_| args.project.clone()))?;
    config.store = settings.store(&config.project.canonicalize().unwrap_or_else(|_| config.project.clone()))?;
    config.checker = settings.checker.enabled;
    config.code_mode = settings.code_mode.enabled;
    config.code_script_secs = settings.code_mode.script_secs;
    config.code_sh_secs = settings.code_mode.sh_secs;
    config.worker_shell = settings.sandbox.shell;
    config.package_indexes = settings.sandbox.package_indexes;
    config.prices = tm_daemon::settings::price_table(&settings);
    config.remote = settings.remote();
    let phone = if args.pair {
        let ip = tailscale_ipv4()?;
        let options = ServeArgs {
            project: args.project.clone(),
            bind: None,
            tailscale: false,
            dictionary: config.dictionary.clone(),
            commons: config.commons.clone(),
            web: None,
        };
        let primary = SocketAddr::from((IpAddr::V4(ip), 8737));
        Some(match start_server(&options, primary).await {
            Ok(server) => server,
            Err(_) => start_server(&options, SocketAddr::from((IpAddr::V4(ip), 0))).await?,
        })
    } else {
        None
    };
    let has_request = args.plan.is_some() || config.prompt.is_some();
    if !has_request && !args.eval {
        if phone.is_some() {
            std::future::pending::<()>().await;
        }
        bail!("pass --plan or --prompt, or --pair to serve the phone");
    }
    let harnesses = Harnesses::from_roles(&args.roles, &harness)?;
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
    let mut line = format!(
        "{state}, {} passed, {} failed, {} check failures, {} tokens plus {} read from cache in {} calls",
        side.passed,
        side.failed,
        side.check_failures,
        side.fresh_tokens,
        side.tokens - side.fresh_tokens,
        side.agent_calls
    );
    if side.unpriced_calls < side.agent_calls {
        line.push_str(&format!(", ${:.2}", side.cost_usd));
    }
    line
}

fn home_commons() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".taskmaster/commons"))
}

/// Start the API and print where it is and the code that pairs a device with it.
async fn start_server(args: &ServeArgs, bind: SocketAddr) -> anyhow::Result<Server> {
    let project = args.project.canonicalize().with_context(|| format!("cannot open {}", args.project.display()))?;
    let settings = Settings::load(&project)?;
    let (auth, code) = settings.auth(settings.records(&project)?)?;
    let mut options = ServerOptions::new(project.clone(), bind, auth);
    options.store = settings.store(&project)?;
    options.remote = settings.remote();
    options.approvals = settings.approval_policy()?;
    options.editor = settings.editor.clone();
    options.preview_command = settings.preview.command.clone();
    options.dictionary = args.dictionary.clone();
    options.commons = args.commons.clone().or_else(home_commons);
    options.web = args.web.clone().or_else(installed_web);
    let server = Server::serve(options).await?;
    println!("taskmaster: api {}  code {code}", server.url);
    Ok(server)
}

/// Where `scripts/install.sh` puts the built frontend.
fn installed_web() -> Option<PathBuf> {
    let from_env = std::env::var_os("TASKMASTER_WEB").map(PathBuf::from);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let installed = data.map(|dir| dir.join("taskmaster/web"));
    [from_env, installed].into_iter().flatten().find(|dir| dir.join("index.html").is_file())
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let bind = match (args.bind, args.tailscale) {
        (Some(addr), _) => addr,
        (None, true) => SocketAddr::from((IpAddr::V4(tailscale_ipv4()?), 8737)),
        (None, false) => SocketAddr::from(([127, 0, 0, 1], 8737)),
    };
    if !bind.ip().is_loopback() && !matches!(bind.ip(), IpAddr::V4(ip) if tm_daemon::is_tailscale(ip)) {
        eprintln!("taskmaster: {bind} is neither this machine nor its tailnet; anyone who can reach it can try the code");
    }
    let _server = start_server(&args, bind).await?;
    tokio::signal::ctrl_c().await.context("cannot wait for ctrl-c")?;
    Ok(())
}

enum Invocation {
    Providers { harness: Option<String> },
    Serve(ServeArgs),
    Drive(Box<Args>),
}

fn invocation_from(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Invocation> {
    let argv: Vec<String> = argv.into_iter().collect();
    if argv.first().is_some_and(|arg| arg == "providers") {
        return Ok(Invocation::Providers { harness: providers_harness(&argv[1..])? });
    }
    if argv.first().is_some_and(|arg| arg == "serve") {
        return Ok(Invocation::Serve(serve_args(&argv[1..])?));
    }
    Ok(Invocation::Drive(Box::new(args_from(argv)?)))
}

fn serve_args(argv: &[String]) -> anyhow::Result<ServeArgs> {
    let usage = "usage: tm-daemon serve <project> [--bind <addr>] [--tailscale] [--dictionary <dir>] \
                 [--commons <dir>] [--web <dir>]";
    let mut argv = argv.iter().cloned();
    let project = argv.next().filter(|arg| !arg.starts_with('-')).context(usage)?;
    let mut args =
        ServeArgs { project: PathBuf::from(project), bind: None, tailscale: false, dictionary: None, commons: None, web: None };
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--tailscale" => args.tailscale = true,
            "--bind" => {
                let value = need(&mut argv, "--bind", usage)?;
                args.bind = Some(value.parse().with_context(|| format!("`{value}` is not an address like 127.0.0.1:8737"))?);
            }
            "--dictionary" => args.dictionary = Some(need(&mut argv, "--dictionary", usage)?.into()),
            "--commons" => args.commons = Some(need(&mut argv, "--commons", usage)?.into()),
            "--web" => args.web = Some(need(&mut argv, "--web", usage)?.into()),
            other => bail!("unknown argument `{other}`\n{usage}"),
        }
    }
    Ok(args)
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

fn args_from(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Args> {
    let usage = "usage: tm-daemon providers [--harness <h>]\n       \
                 tm-daemon serve <project> [--bind <addr>] [--tailscale]\n       \
                 tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] \
                 [--yes] [--worker <h>] [--mid <h>] [--master <h>] [--reviewer <h>] \
                 [--verifier <h>] [--worker-model <m>] [--mid-model <m>] [--master-model <m>] \
                 [--reviewer-model <m>] [--verifier-model <m>] [--parallel <n>] \
                 [--timeout <secs>] [--dictionary <dir>] [--commons <dir>] [--run <id>] \
                 [--template <next | dir>] [--no-runtime-checks] \
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
        timeout: None,
        dictionary: None,
        commons: None,
        run_id: "current".into(),
        template: None,
        eval: false,
        baseline: "claude".into(),
        pair: false,
        no_runtime_checks: false,
        roles: Roles::default(),
        models: Models::default(),
    };
    let mut baseline_set = false;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--yes" => args.yes = true,
            "--no-runtime-checks" => args.no_runtime_checks = true,
            "--plan" => args.plan = Some(need(&mut argv, "--plan", usage)?.into()),
            "--prompt" => args.prompt = Some(need(&mut argv, "--prompt", usage)?),
            "--prompt-file" => args.prompt_file = Some(need(&mut argv, "--prompt-file", usage)?.into()),
            "--worker" => args.roles.worker = need(&mut argv, "--worker", usage)?,
            "--mid" => args.roles.mid = need(&mut argv, "--mid", usage)?,
            "--master" => args.roles.master = need(&mut argv, "--master", usage)?,
            "--reviewer" => args.roles.reviewer = need(&mut argv, "--reviewer", usage)?,
            "--verifier" => args.roles.verifier = need(&mut argv, "--verifier", usage)?,
            "--worker-model" => args.models.worker = Some(need(&mut argv, "--worker-model", usage)?),
            "--mid-model" => args.models.mid = Some(need(&mut argv, "--mid-model", usage)?),
            "--master-model" => args.models.master = Some(need(&mut argv, "--master-model", usage)?),
            "--reviewer-model" => args.models.reviewer = Some(need(&mut argv, "--reviewer-model", usage)?),
            "--verifier-model" => args.models.verifier = Some(need(&mut argv, "--verifier-model", usage)?),
            "--parallel" => {
                args.parallel = need(&mut argv, "--parallel", usage)?.parse().context("--parallel needs a number")?
            }
            "--timeout" => {
                args.timeout = Some(need(&mut argv, "--timeout", usage)?.parse().context("--timeout needs seconds, or 0 for no limit")?)
            }
            "--dictionary" => args.dictionary = Some(need(&mut argv, "--dictionary", usage)?.into()),
            "--commons" => args.commons = Some(need(&mut argv, "--commons", usage)?.into()),
            "--run" => args.run_id = need(&mut argv, "--run", usage)?,
            "--template" => args.template = Some(need(&mut argv, "--template", usage)?),
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
        args.baseline = args.roles.master.clone();
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

fn need(argv: &mut impl Iterator<Item = String>, flag: &str, usage: &str) -> anyhow::Result<String> {
    argv.next().with_context(|| format!("{flag} needs a value\n{usage}"))
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
        // The benchmark's own invocation: 0 is no limit.
        let bench = parse("proj --prompt-file F --yes --worker grok --mid grok --master grok --reviewer grok --verifier grok --parallel 3 --timeout 0").unwrap();
        assert_eq!(bench.timeout, Some(0));
        assert_eq!(bench.parallel, 3);
        assert!(bench.yes);
        assert_eq!(tm_daemon::settings::agent_limit(0), tm_daemon::settings::NO_LIMIT);
    }

    #[test]
    fn the_taskmaster_harness_and_its_model_are_chosen() {
        let args = parse("proj --master grok --master-model grok-4.7 --plan p.json").unwrap();
        assert_eq!(args.roles.master, "grok");
        assert_eq!(args.models.master.as_deref(), Some("grok-4.7"));
        assert_eq!(args.baseline, "grok");
    }

    #[test]
    fn serve_takes_a_project_and_an_address() {
        let argv = "serve proj --bind 127.0.0.1:9000 --commons /tmp/c".split_whitespace().map(str::to_string);
        match invocation_from(argv).unwrap() {
            Invocation::Serve(args) => {
                assert_eq!(args.project, PathBuf::from("proj"));
                assert_eq!(args.bind, Some("127.0.0.1:9000".parse().unwrap()));
                assert!(!args.tailscale);
            }
            _ => panic!("serve was read as a project"),
        }
        assert!(invocation_from(["serve".to_string()]).is_err());
    }

    #[test]
    fn providers_lists_models_without_a_project() {
        match invocation_from(["providers".to_string()]).unwrap() {
            Invocation::Providers { harness: None } => {}
            Invocation::Drive(_) | Invocation::Serve(_) => panic!("providers was read as a project"),
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
