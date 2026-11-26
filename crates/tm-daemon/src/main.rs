//! `tm-daemon`: carry out a Taskmaster plan.
//!
//! ```text
//! tm-daemon providers [--harness <h>]
//! tm-daemon report [<project>] [--run <id>] [--json]
//! tm-daemon serve <project> [--bind <addr>] [--lan] [--tailscale] [--dictionary <dir>] [--commons <dir>]
//!                 [--web <dir>]
//! tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] [--yes]
//!           [--worker <h>] [--mid <h>] [--master <h>]
//!           [--reviewer <h>] [--verifier <h>]
//!           [--worker-model <m>] [--mid-model <m>] [--master-model <m>]
//!           [--reviewer-model <m>] [--verifier-model <m>]
//!           [--parallel <n>] [--timeout <secs>] [--dictionary <dir>] [--commons <dir>]
//!           [--run <id>] [--template <next | dir>] [--prices <file>] [--eval] [--baseline <h>] [--pair]
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
//! run finishes clean, 1 when it fails, 2 when the plan waits for `--yes`, 3 when a
//! provider requires sign-in or credit balance, and 75 (`EX_TEMPFAIL`) when a provider
//! hits a quota limit or HTTP 429 rate limit (`RESOURCE_EXHAUSTED`): outer scripts can
//! stop on 75 instead of launching doomed runs, and re-running the same command picks up
//! where the run stopped once the reset time passes.
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
    /// Disable effort-based thinking routing and pin explicit model ids verbatim.
    no_effort_routing: bool,
    parallel: usize,
    /// Seconds per agent call; 0 is no limit. None takes the settings file's limit.
    timeout: Option<u64>,
    dictionary: Option<PathBuf>,
    commons: Option<PathBuf>,
    prices: Option<PathBuf>,
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
    /// Listen on every address, so the local network and the tailnet can pair.
    lan: bool,
    auto_resume: bool,
    dictionary: Option<PathBuf>,
    commons: Option<PathBuf>,
    web: Option<PathBuf>,
}

enum EvalMode {
    Suite { dir: PathBuf, check_only: bool },
    Gate { baseline: PathBuf, candidate: PathBuf, max_cost_pct: f64 },
}

/// Set when `serve` handed its agents to the next daemon (H3). It then exits with
/// [`HANDED_OFF_EXIT`], which a service manager restarts on.
static HANDED_OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The exit status of a daemon that handed off: `EX_TEMPFAIL`, "come back".
const HANDED_OFF_EXIT: u8 = 75;

fn main() -> ExitCode {
    // `tm-daemon hold <socket>`: the holder that keeps builders across a daemon update (H3).
    // It is started by the daemon itself and runs its own small runtime.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().is_some_and(|arg| arg == "hold") {
        return hold(argv.get(1).map(PathBuf::from));
    }
    let outcome = run();
    if HANDED_OFF.load(std::sync::atomic::Ordering::SeqCst) {
        return ExitCode::from(HANDED_OFF_EXIT);
    }
    match outcome {
        Ok(Outcome::Finished { ok: true }) => ExitCode::SUCCESS,
        Ok(Outcome::Finished { ok: false }) => ExitCode::from(1),
        Ok(Outcome::NeedsApproval { .. } | Outcome::Rejected) => ExitCode::from(2),
        // A provider stopped the run (quota / HTTP 429 rate limit -> exit 75 EX_TEMPFAIL;
        // auth / balance -> exit 3): the run is kept where it stopped so the same command resumes it.
        Err(err) if err.downcast_ref::<tm_daemon::reply::ReportedLimit>().is_some() => {
            let limit = err.downcast_ref::<tm_daemon::reply::ReportedLimit>().expect("checked");
            eprintln!("taskmaster: {limit}");
            eprintln!(
                "taskmaster: the run is kept where it stopped. Run the same command again to resume it, \
                 or pass a different --master/--mid/--worker/--reviewer/--verifier to switch the provider."
            );
            ExitCode::from(limit.exit_code())
        }
        Err(err) => {
            eprintln!("taskmaster: {err:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(unix)]
fn hold(socket: Option<PathBuf>) -> ExitCode {
    let Some(socket) = socket.or_else(tm_harness::hold::default_socket) else {
        eprintln!("usage: tm-daemon hold <socket>");
        return ExitCode::from(1);
    };
    match tm_harness::hold::run_holder(&socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("taskmaster: the holder stopped: {err:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(unix))]
fn hold(_socket: Option<PathBuf>) -> ExitCode {
    eprintln!("taskmaster: the holder needs a Unix system");
    ExitCode::from(1)
}

/// The holder this daemon starts builders in (H3): `$TASKMASTER_HOLD` or the default socket,
/// started from this program when none is running. `TASKMASTER_HOLD=off` turns it off.
fn holder() -> Option<tm_harness::hold::Holder> {
    if !cfg!(unix) {
        return None;
    }
    let socket = tm_harness::hold::default_socket()?;
    // An update replaces the program under a running daemon; the path still names the new one.
    let program = std::env::current_exe().ok().map(|exe| {
        let text = exe.to_string_lossy();
        match text.strip_suffix(" (deleted)") {
            Some(path) => PathBuf::from(path),
            None => exe,
        }
    });
    Some(tm_harness::hold::Holder::new(socket, program))
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
        Invocation::Eval(mode) => {
            let ok = run_eval_cli(mode)?;
            return Ok(Outcome::Finished { ok });
        }
        Invocation::Version => {
            println!("tm-daemon {}", env!("CARGO_PKG_VERSION"));
            return Ok(Outcome::Finished { ok: true });
        }
        Invocation::Help => {
            println!("{USAGE}");
            return Ok(Outcome::Finished { ok: true });
        }
        Invocation::Report { project, run, json, append } => {
            let project = project.canonicalize().with_context(|| format!("cannot open {}", project.display()))?;
            let records = Settings::load(&project)?.records(&project)?;
            if records.events(&run)?.is_empty() && records.ledger(&run)?.is_empty() {
                bail!("there is no run `{run}` in {}", project.display());
            }
            let report = tm_daemon::report::build(&records, &run)?;
            // One line per run, for a benchmark's results file (bench/results.jsonl).
            if let Some(file) = &append {
                use std::io::Write;
                let mut line = serde_json::to_value(&report)?;
                line["project"] = serde_json::json!(project.display().to_string());
                line["at"] = serde_json::json!(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
                let mut out = std::fs::OpenOptions::new().create(true).append(true).open(file).with_context(|| format!("cannot open {}", file.display()))?;
                writeln!(out, "{}", serde_json::to_string(&line)?)?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", tm_daemon::report::render(&report));
            }
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
        if let Ok(settings) = Settings::load(&project)
            && let Ok(records) = settings.records(&project)
        {
            args.roles.resolve_auto(&records, &mut args.models, "normal", "general");
        }
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
    let project_root = args.project.canonicalize().unwrap_or_else(|_| args.project.clone());
    let settings = Settings::load(&project_root)?;
    config.store = settings.store(&config.project.canonicalize().unwrap_or_else(|_| config.project.clone()))?;
    config.checker = settings.checker.enabled;
    config.code_mode = settings.code_mode.enabled;
    config.code_script_secs = settings.code_mode.script_secs;
    config.code_sh_secs = settings.code_mode.sh_secs;
    config.worker_shell = settings.sandbox.shell;
    config.package_indexes = settings.sandbox.package_indexes;
    config.effort_routing = if args.no_effort_routing {
        false
    } else {
        tm_daemon::settings::effective_effort_routing(settings.agents.effort_routing)
    };
    config.prices = tm_daemon::settings::load_price_table(&settings, &project_root, args.prices.as_deref())?;
    config.remote = settings.remote();
    let phone = if args.pair {
        let ip = tailscale_ipv4()?;
        let options = ServeArgs {
            project: args.project.clone(),
            bind: None,
            tailscale: false,
            lan: false,
            auto_resume: false,
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
    options.holder = holder();
    options.push = tm_daemon::push::default_path();
    let server = Server::serve(options).await?;
    server.warm_providers();
    println!("taskmaster: api {}  code {code}", server.url);
    // The same code as a link: open it on this machine, or on a device that can reach one
    // of these addresses, and it signs in with nothing to type.
    for link in server.pairing_links(&code) {
        println!("taskmaster: pair {link}");
    }
    Ok(server)
}

/// Where the built frontend is when `--web` is not given: `$TASKMASTER_WEB`, where the
/// install scripts put it, or where an installer put it beside this program.
fn installed_web() -> Option<PathBuf> {
    let from_env = std::env::var_os("TASKMASTER_WEB").map(PathBuf::from);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    let installed = data.map(|dir| dir.join("taskmaster/web"));
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let bundled = exe.as_deref().map(bundled_web).unwrap_or_default();
    [from_env, installed].into_iter().flatten().chain(bundled).find(|dir| dir.join("index.html").is_file())
}

/// Where the desktop installers put the frontend, relative to `tm-daemon` itself: the .deb
/// and the AppImage (`usr/bin/tm-daemon`, `usr/lib/Taskmaster/web`), a macOS app
/// (`Contents/MacOS`, `Contents/Resources/web`), and a folder with `web/` beside the program.
fn bundled_web(exe: &std::path::Path) -> Vec<PathBuf> {
    let Some(dir) = exe.parent() else { return Vec::new() };
    let mut out = vec![dir.join("web")];
    if let Some(up) = dir.parent() {
        out.push(up.join("lib/Taskmaster/web"));
        out.push(up.join("lib/taskmaster/web"));
        out.push(up.join("share/taskmaster/web"));
        out.push(up.join("Resources/web"));
    }
    out
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let bind = match (args.bind, args.tailscale, args.lan) {
        (Some(addr), _, _) => addr,
        (None, _, true) => SocketAddr::from(([0, 0, 0, 0], 8737)),
        (None, true, false) => SocketAddr::from((IpAddr::V4(tailscale_ipv4()?), 8737)),
        (None, false, false) => SocketAddr::from(([127, 0, 0, 1], 8737)),
    };
    if args.lan || bind.ip().is_unspecified() {
        eprintln!("taskmaster: listening on the local network and the tailnet; a pairing code or link is all that keeps other devices out");
    } else if !bind.ip().is_loopback() && !matches!(bind.ip(), IpAddr::V4(ip) if tm_daemon::is_tailscale(ip)) {
        eprintln!("taskmaster: {bind} is neither this machine nor its tailnet; anyone who can reach it can try the code");
    }
    if args.auto_resume
        && let Ok(project) = args.project.canonicalize()
        && let Ok(settings) = Settings::load(&project)
        && let Ok(records) = settings.records(&project)
    {
        let resumable = detect_interrupted_runs(&records);
        if !resumable.is_empty() {
            eprintln!(
                "taskmaster: --auto-resume detected {} interrupted run(s): {}",
                resumable.len(),
                resumable.join(", ")
            );
        }
    }
    let server = start_server(&args, bind).await?;
    if wait_for_shutdown().await? == Shutdown::HandOff {
        let kept = server.hand_off().await;
        eprintln!("taskmaster: handed {kept} agent(s) to the next daemon; exiting with {HANDED_OFF_EXIT} so it starts");
        HANDED_OFF.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    drop(server);
    Ok(())
}

/// Detect runs interrupted mid-flight by inspecting `status.json` (`phase` or legacy `state`)
/// and falling back to replaying `events.jsonl` when a crash happened before `status.json` was written.
fn detect_interrupted_runs(records: &tm_daemon::store::Records) -> Vec<String> {
    let Ok(runs) = records.run_ids() else { return Vec::new() };
    runs.into_iter()
        .filter(|id| {
            if let Ok(Some(status)) = records.status(id) {
                if let Some(phase) = status["phase"].as_str() {
                    return !matches!(phase, "Done" | "Failed" | "done" | "failed");
                }
                if let Some(state) = status["state"].as_str() {
                    return matches!(state, "running" | "stopped" | "interrupted");
                }
            }
            match records.events(id) {
                Ok(events) if !events.is_empty() => {
                    let run = tm_core::Run::replay(1, events);
                    !matches!(run.phase, tm_core::RunPhase::Done | tm_core::RunPhase::Failed)
                }
                _ => false,
            }
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Shutdown {
    /// Ctrl-C or SIGTERM: stop, and everything this daemon started stops with it.
    Stop,
    /// SIGUSR1, sent by an updater: hand the builders to the next daemon first (H3).
    HandOff,
}

async fn wait_for_shutdown() -> anyhow::Result<Shutdown> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).context("cannot listen for SIGTERM")?;
        let mut hand_off = signal(SignalKind::user_defined1()).context("cannot listen for SIGUSR1")?;
        tokio::select! {
            res = tokio::signal::ctrl_c() => { res.context("cannot wait for ctrl-c")?; Ok(Shutdown::Stop) }
            _ = term.recv() => { eprintln!("taskmaster: received SIGTERM, shutting down cleanly"); Ok(Shutdown::Stop) }
            _ = hand_off.recv() => { eprintln!("taskmaster: received SIGUSR1, handing the agents to the next daemon"); Ok(Shutdown::HandOff) }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("cannot wait for ctrl-c")?;
        Ok(Shutdown::Stop)
    }
}

fn run_eval_cli(mode: EvalMode) -> anyhow::Result<bool> {
    match mode {
        EvalMode::Suite { dir, check_only } => {
            let tasks = tm_daemon::eval_suite::load_suite(&dir)
                .with_context(|| format!("cannot load eval suite from {}", dir.display()))?;
            if tasks.is_empty() {
                bail!("no eval tasks found under {}", dir.display());
            }
            let mut ok = true;
            let mut summary = Vec::new();
            for task in &tasks {
                if check_only {
                    let scratch = std::env::temp_dir().join(format!(
                        "tm-eval-check-{}-{}",
                        std::process::id(),
                        task.id
                    ));
                    let _ = std::fs::remove_dir_all(&scratch);
                    tm_daemon::eval_suite::seed_task(task, &scratch)?;
                    let (passed_on_starter, _) = tm_daemon::eval_suite::run_hidden_test(task, &scratch)?;
                    let _ = std::fs::remove_dir_all(&scratch);
                    let valid = !passed_on_starter;
                    if !valid {
                        ok = false;
                    }
                    summary.push(serde_json::json!({
                        "id": task.id,
                        "category": task.category,
                        "language": task.language,
                        "difficulty": task.difficulty,
                        "starter_fails_hidden_test": valid,
                    }));
                } else {
                    summary.push(serde_json::json!({
                        "id": task.id,
                        "category": task.category,
                        "language": task.language,
                        "difficulty": task.difficulty,
                        "hidden_test_file": task.hidden_test_file,
                    }));
                }
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": ok,
                    "count": tasks.len(),
                    "tasks": summary,
                }))?
            );
            Ok(ok)
        }
        EvalMode::Gate { baseline, candidate, max_cost_pct } => {
            let base = tm_daemon::eval_suite::read_results_jsonl(&baseline)
                .with_context(|| format!("cannot read baseline {}", baseline.display()))?;
            let cand = tm_daemon::eval_suite::read_results_jsonl(&candidate)
                .with_context(|| format!("cannot read candidate {}", candidate.display()))?;
            let verdict = tm_daemon::eval_suite::check_regression_gate(&base, &cand, max_cost_pct);
            println!("{}", serde_json::to_string_pretty(&verdict)?);
            Ok(verdict.ok)
        }
    }
}

enum Invocation {
    Providers { harness: Option<String> },
    Version,
    Help,
    /// `tm-daemon report [<project>] [--run <id>] [--json] [--append <file>]`
    Report { project: PathBuf, run: String, json: bool, append: Option<PathBuf> },
    Eval(EvalMode),
    Serve(ServeArgs),
    Drive(Box<Args>),
}

fn invocation_from(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Invocation> {
    let argv: Vec<String> = argv.into_iter().collect();
    if argv.first().is_some_and(|arg| arg == "providers") {
        return Ok(Invocation::Providers { harness: providers_harness(&argv[1..])? });
    }
    if argv.first().is_some_and(|arg| arg == "--version" || arg == "-V") {
        return Ok(Invocation::Version);
    }
    if argv.first().is_some_and(|arg| arg == "--help" || arg == "-h" || arg == "help") {
        return Ok(Invocation::Help);
    }
    if argv.first().is_some_and(|arg| arg == "eval") {
        return Ok(Invocation::Eval(eval_args(&argv[1..])?));
    }
    if argv.first().is_some_and(|arg| arg == "report") {
        let usage = "usage: tm-daemon report [<project>] [--run <id>] [--json] [--append <file>]";
        let (mut project, mut run, mut json, mut append) = (PathBuf::from("."), "current".to_string(), false, None);
        let mut rest = argv[1..].iter();
        while let Some(arg) = rest.next() {
            match arg.as_str() {
                "--run" => run = rest.next().cloned().with_context(|| usage.to_string())?,
                "--json" => json = true,
                "--append" => append = Some(PathBuf::from(rest.next().cloned().with_context(|| usage.to_string())?)),
                flag if flag.starts_with('-') => bail!("unknown option `{flag}`\n{usage}"),
                path => project = PathBuf::from(path),
            }
        }
        return Ok(Invocation::Report { project, run, json, append });
    }
    if argv.first().is_some_and(|arg| arg == "serve") {
        return Ok(Invocation::Serve(serve_args(&argv[1..])?));
    }
    Ok(Invocation::Drive(Box::new(args_from(argv)?)))
}

fn eval_args(argv: &[String]) -> anyhow::Result<EvalMode> {
    let usage = "usage: tm-daemon eval --suite <dir> [--check-only]\n       \
                 tm-daemon eval --gate <baseline.jsonl> <candidate.jsonl> [--max-cost-pct <n>]";
    let mut it = argv.iter().cloned();
    let mut suite: Option<PathBuf> = None;
    let mut check_only = false;
    let mut gate: Option<(PathBuf, PathBuf)> = None;
    let mut max_cost_pct = 10.0_f64;
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--suite" => suite = Some(need(&mut it, "--suite", usage)?.into()),
            "--check-only" => check_only = true,
            "--gate" => {
                let base = need(&mut it, "--gate", usage)?.into();
                let cand = need(&mut it, "--gate", usage)?.into();
                gate = Some((base, cand));
            }
            "--max-cost-pct" => {
                max_cost_pct = need(&mut it, "--max-cost-pct", usage)?
                    .parse()
                    .context("--max-cost-pct needs a number")?;
            }
            other => bail!("unknown argument `{other}`\n{usage}"),
        }
    }
    if let Some((baseline, candidate)) = gate {
        Ok(EvalMode::Gate { baseline, candidate, max_cost_pct })
    } else if let Some(dir) = suite {
        Ok(EvalMode::Suite { dir, check_only })
    } else {
        bail!("{usage}")
    }
}

fn serve_args(argv: &[String]) -> anyhow::Result<ServeArgs> {
    let usage = "usage: tm-daemon serve <project> [--bind <addr>] [--lan] [--tailscale] [--auto-resume] [--dictionary <dir>] \
                 [--commons <dir>] [--web <dir>]";
    let mut argv = argv.iter().cloned();
    let project = argv.next().filter(|arg| !arg.starts_with('-')).context(usage)?;
    let mut args = ServeArgs {
        project: PathBuf::from(project),
        bind: None,
        tailscale: false,
        lan: false,
        auto_resume: false,
        dictionary: None,
        commons: None,
        web: None,
    };
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--tailscale" => args.tailscale = true,
            "--lan" => args.lan = true,
            "--auto-resume" => args.auto_resume = true,
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

/// What `tm-daemon --help` prints, and what a wrong command line is answered with.
const USAGE: &str = "usage: tm-daemon providers [--harness <h>]\n       \
                     tm-daemon serve <project> [--bind <addr>] [--lan] [--tailscale] [--auto-resume] [--web <dir>]\n       \
                     tm-daemon eval --suite <dir> [--check-only]\n       \
                     tm-daemon eval --gate <baseline.jsonl> <candidate.jsonl> [--max-cost-pct <n>]\n       \
                     tm-daemon report [<project>] [--run <id>] [--json] [--append <file>]\n       \
                     tm-daemon hold [<socket>]   (started by serve; SIGUSR1 to serve hands agents to the next daemon)\n       \
                     tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] \
                     [--yes] [--worker <h | auto>] [--mid <h>] [--master <h>] [--reviewer <h>] \
                     [--verifier <h>] [--worker-model <m>] [--mid-model <m>] [--master-model <m>] \
                     [--reviewer-model <m>] [--verifier-model <m>] [--parallel <n>] \
                     [--timeout <secs, 0 for none>] [--dictionary <dir>] [--commons <dir>] [--run <id>] \
                     [--template <next | dir>] [--no-runtime-checks] [--no-effort-routing] \
                     [--eval] [--baseline <h>] [--pair]\n       \
                     tm-daemon --version\n\
                     harnesses: claude, agy, grok, codex, opencode (worker also accepts auto)";

fn args_from(argv: impl IntoIterator<Item = String>) -> anyhow::Result<Args> {
    let usage = USAGE;
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
        prices: None,
        run_id: "current".into(),
        template: None,
        eval: false,
        baseline: "claude".into(),
        pair: false,
        no_runtime_checks: false,
        no_effort_routing: false,
        roles: Roles::default(),
        models: Models::default(),
    };
    let mut baseline_set = false;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--yes" => args.yes = true,
            "--no-runtime-checks" => args.no_runtime_checks = true,
            "--no-effort-routing" => args.no_effort_routing = true,
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
            "--prices" => args.prices = Some(need(&mut argv, "--prices", usage)?.into()),
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
    fn the_installers_web_app_is_found_beside_the_program() {
        let deb = bundled_web(std::path::Path::new("/usr/bin/tm-daemon"));
        assert!(deb.contains(&PathBuf::from("/usr/lib/Taskmaster/web")));
        let mac = bundled_web(std::path::Path::new("/Applications/Taskmaster.app/Contents/MacOS/tm-daemon"));
        assert!(mac.contains(&PathBuf::from("/Applications/Taskmaster.app/Contents/Resources/web")));
        let tarball = bundled_web(std::path::Path::new("/opt/taskmaster/tm-daemon"));
        assert_eq!(tarball[0], PathBuf::from("/opt/taskmaster/web"));
    }

    #[test]
    fn version_and_help_are_answered_without_a_project() {
        assert!(matches!(invocation_from(["--version".to_string()]).unwrap(), Invocation::Version));
        assert!(matches!(invocation_from(["-h".to_string()]).unwrap(), Invocation::Help));
        assert!(USAGE.contains("tm-daemon report") && USAGE.contains("0 for none"));
    }

    #[test]
    fn report_reads_a_project_and_a_run() {
        match invocation_from(["report".into(), "proj".into(), "--run".into(), "r1".into(), "--json".into()]).unwrap() {
            Invocation::Report { project, run, json, append } => {
                assert_eq!((project, run.as_str(), json, append), (PathBuf::from("proj"), "r1", true, None));
            }
            _ => panic!("report was not read as a report"),
        }
        match invocation_from(["report".to_string()]).unwrap() {
            Invocation::Report { project, run, json, append } => {
                assert_eq!((project, run.as_str(), json, append), (PathBuf::from("."), "current", false, None));
            }
            _ => panic!("report was not read as a report"),
        }
        assert!(invocation_from(["report".into(), "--bogus".into()]).is_err());
    }

    #[test]
    fn providers_lists_models_without_a_project() {
        match invocation_from(["providers".to_string()]).unwrap() {
            Invocation::Providers { harness: None } => {}
            Invocation::Drive(_)
            | Invocation::Serve(_)
            | Invocation::Report { .. }
            | Invocation::Eval(_)
            | Invocation::Version
            | Invocation::Help => {
                panic!("providers was read as a project")
            }
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

    #[test]
    fn eval_and_auto_resume_flags_parse() {
        match invocation_from(["eval".into(), "--suite".into(), "bench/suite".into(), "--check-only".into()]).unwrap() {
            Invocation::Eval(EvalMode::Suite { dir, check_only }) => {
                assert_eq!(dir, PathBuf::from("bench/suite"));
                assert!(check_only);
            }
            _ => panic!("expected Eval::Suite"),
        }
        match invocation_from(["eval".into(), "--gate".into(), "base.jsonl".into(), "cand.jsonl".into()]).unwrap() {
            Invocation::Eval(EvalMode::Gate { baseline, candidate, max_cost_pct }) => {
                assert_eq!(baseline, PathBuf::from("base.jsonl"));
                assert_eq!(candidate, PathBuf::from("cand.jsonl"));
                assert!((max_cost_pct - 10.0).abs() < 1e-6);
            }
            _ => panic!("expected Eval::Gate"),
        }
        match invocation_from(["serve".into(), ".".into(), "--auto-resume".into()]).unwrap() {
            Invocation::Serve(args) => assert!(args.auto_resume),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn detect_interrupted_runs_finds_in_progress_runs_without_status_state_field() {
        let dir = std::env::temp_dir().join(format!("tm-auto-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let records = tm_daemon::store::Records::files(&dir);

        // 1. Crashed mid-build with events.jsonl and no status.json yet -> must be detected!
        let plan = tm_core::Plan {
            pages: vec![tm_core::PagePlan { id: "a".into(), owned_glob: "a/**".into(), ..Default::default() }],
            ..Default::default()
        };
        records
            .save_events("in-flight", &[tm_core::Event::PlanReady(plan), tm_core::Event::PlanApproved])
            .unwrap();
        // 2. Active status.json using `phase` (the actual field written by write_status) -> detected!
        records
            .save_status("verifying", &serde_json::json!({ "ok": false, "phase": "Verifying" }))
            .unwrap();
        // 3. Finished run (`phase: "Done"`) -> NOT detected!
        records
            .save_status("done", &serde_json::json!({ "ok": true, "phase": "Done" }))
            .unwrap();

        let mut found = detect_interrupted_runs(&records);
        found.sort();
        assert_eq!(found, vec!["in-flight".to_string(), "verifying".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
