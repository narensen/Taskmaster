//! Checks that run the code. They cost no tokens.
//!
//! [`run_steps`] runs the project's own build and tests: the `build` and `test` scripts
//! in `package.json`, `cargo test` for a Rust project, and pytest wherever there are Python
//! tests, through the project's own runner (poetry, uv, hatch) or pip, in a venv the run makes
//! for itself with nothing inherited from the daemon's environment. [`browse`] starts a built Next.js app, opens every
//! page's route in headless Chrome over the DevTools protocol, and reports uncaught
//! errors, console errors, failed loads and error statuses. It keeps a screenshot of each
//! page.
//!
//! [`attribute`] sends each line of output to the page whose file or route it names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use base64::Engine;
use serde_json::{Value, json};
use crate::browser::Browser;
use crate::checks::Timing;
use tm_core::PageId;
use tm_verify::Owners;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Output kept from a failed step: the end, where the errors are.
const KEEP: usize = 6000;

/// The UI checks of one page may take this long.
const AUDIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Step {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// Set on top of the cleaned environment (see [`clean_env`]).
    pub env: Vec<(String, String)>,
    /// Preparing the environment (a venv, an install). A failure here is kept and shown with
    /// the next check's output, but does not stop the checks: an install that cannot reach
    /// the network may still leave enough to run the tests.
    pub setup: bool,
    /// Written once the step succeeds, so an unchanged environment is not prepared twice.
    pub stamp: Option<(PathBuf, String)>,
}

impl Step {
    fn new(name: &str, program: impl Into<String>, args: &[&str]) -> Self {
        Step { name: name.into(), program: program.into(), args: args.iter().map(|a| a.to_string()).collect(), ..Default::default() }
    }

    /// How the step reads in a log line.
    pub fn line(&self) -> String {
        format!("{} {}", self.program, self.args.join(" ")).trim().to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub step: String,
    pub output: String,
}

/// Variables that tie a process to someone else's Python or Conda environment. Verify and the
/// workers' shells run without them, so a venv the daemon was started from never leaks in.
pub use tm_harness::sandbox::LEAKY_VARS;

/// `PATH` without any directory that belongs to a virtualenv or a Conda environment.
pub fn clean_path() -> std::ffi::OsString {
    tm_harness::sandbox::clean_path()
}

/// Strip what [`LEAKY_VARS`] names and every `CONDA_*` variable, and use [`clean_path`].
pub fn clean_env(cmd: &mut Command) {
    for name in LEAKY_VARS {
        cmd.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("CONDA_") {
            cmd.env_remove(name);
        }
    }
    cmd.env("PATH", clean_path());
}

fn on_path(program: &str) -> bool {
    let names = tm_harness::sandbox::program_names(program);
    std::env::split_paths(&clean_path()).any(|dir| names.iter().any(|name| dir.join(name).is_file()))
}

/// Python test files: a `tests/` or `test/` directory with Python in it, or `test_*.py` and
/// `*_test.py` beside the code.
pub fn python_tests(tree: &Path) -> bool {
    let is_test = |name: &str| name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"));
    let any_py = |dir: &Path| {
        walk(dir, 3).into_iter().any(|p| p.extension().is_some_and(|e| e == "py"))
    };
    if ["tests", "test"].iter().any(|d| tree.join(d).is_dir() && any_py(&tree.join(d))) {
        return true;
    }
    walk(tree, 2).into_iter().any(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_test))
}

/// Files under `dir`, `depth` levels down, skipping hidden directories and environments.
fn walk(dir: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let skip = name.starts_with('.') || ["node_modules", "venv", "target", "__pycache__", "site-packages"].contains(&name.as_str());
            if depth > 1 && !skip {
                out.extend(walk(&path, depth - 1));
            }
        } else {
            out.push(path);
        }
    }
    out
}

/// Standalone Node built-in test files (`*.test.mjs`, `*.test.js`, `*.test.cjs`) in a
/// project without `package.json`, relative to `tree`.
pub fn node_test_files(tree: &Path) -> Vec<String> {
    if tree.join("package.json").is_file() {
        return Vec::new();
    }
    let mut found: Vec<String> = walk(tree, 3)
        .into_iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(tree).ok()?;
            let name = p.file_name()?.to_str()?;
            let is_test = name.ends_with(".test.mjs") || name.ends_with(".test.js") || name.ends_with(".test.cjs");
            is_test.then(|| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    found.sort();
    found
}

/// Whether the project has tests of its own. Such a project must have them run: a verify
/// that ran nothing is a failure, not a pass.
pub fn has_tests(tree: &Path) -> bool {
    if let Ok(Some(loaded)) = crate::project_config::load(tree)
        && loaded.trusted
        && loaded.config.test.as_deref().is_some_and(|s| !s.trim().is_empty())
    {
        return true;
    }
    let npm_test = std::fs::read_to_string(tree.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|package| package["scripts"]["test"].as_str().map(|s| !s.contains("no test specified")))
        .unwrap_or(false);
    python_tests(tree) || tree.join("Cargo.toml").is_file() || npm_test || !node_test_files(tree).is_empty()
}

/// The dependency files that decide what a Python environment holds, as one stamp.
fn python_stamp(tree: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut names: Vec<PathBuf> = ["pyproject.toml", "poetry.lock", "uv.lock", "setup.py", "setup.cfg", "Pipfile.lock"].iter().map(|f| tree.join(f)).collect();
    names.extend(requirement_files(tree).into_iter().map(|f| tree.join(f)));
    for name in names {
        name.file_name().hash(&mut hasher);
        std::fs::read(&name).unwrap_or_default().hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

fn requirement_files(tree: &Path) -> Vec<String> {
    let mut out: Vec<String> = ["requirements.txt", "requirements-dev.txt", "requirements_dev.txt", "requirements-test.txt", "requirements_test.txt", "dev-requirements.txt", "test-requirements.txt"]
        .iter()
        .filter(|f| tree.join(f).is_file())
        .map(|f| f.to_string())
        .collect();
    if let Ok(entries) = std::fs::read_dir(tree.join("requirements")) {
        let mut more: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".txt")).map(|n| format!("requirements/{n}")).collect();
        more.sort();
        out.extend(more);
    }
    out
}

/// The extras of `[project.optional-dependencies]` that hold what tests need.
fn test_extras(pyproject: &str) -> Vec<String> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in pyproject.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[project.optional-dependencies]";
            continue;
        }
        if !inside {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim().trim_matches('"');
            if ["dev", "test", "tests", "testing"].contains(&key) {
                out.push(key.to_string());
            }
        }
    }
    out
}

/// How a Python project's tests are run, in its own runner where it has one, in `venv`.
fn python_steps(tree: &Path, venv: &Path) -> Vec<Step> {
    let pyproject = std::fs::read_to_string(tree.join("pyproject.toml")).unwrap_or_default();
    let python = if on_path("python3") { "python3" } else if on_path("python") { "python" } else { return Vec::new() };
    let bin = venv.join(if cfg!(windows) { "Scripts" } else { "bin" });
    let mut path = std::ffi::OsString::from(bin.as_os_str());
    path.push(if cfg!(windows) { ";" } else { ":" });
    path.push(clean_path());
    let path = path.to_string_lossy().into_owned();
    let venv_env = vec![("VIRTUAL_ENV".to_string(), venv.display().to_string()), ("PATH".to_string(), path), ("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into())];
    let stamp = python_stamp(tree);
    let stamp_file = venv.join(".taskmaster-deps");
    let ready = std::fs::read_to_string(&stamp_file).is_ok_and(|have| have == stamp) && bin.is_dir();
    let mut out = Vec::new();
    let with_env = |mut step: Step| {
        step.env = venv_env.clone();
        step
    };
    let setup = |mut step: Step| {
        step.setup = true;
        step
    };
    if !ready {
        // System packages stay visible underneath, so a machine with no network still has pytest.
        out.push(setup(Step::new("setup", python, &["-m", "venv", "--system-site-packages", &venv.display().to_string()])));
    }
    let venv_python = bin.join(if cfg!(windows) { "python.exe" } else { "python" }).display().to_string();
    let poetry = (pyproject.contains("[tool.poetry") || tree.join("poetry.lock").is_file()) && on_path("poetry");
    let uv = (tree.join("uv.lock").is_file() || pyproject.contains("[tool.uv")) && on_path("uv");
    let hatch = pyproject.contains("[tool.hatch.envs") && on_path("hatch");
    let mut install: Vec<Step> = Vec::new();
    let test = if poetry {
        install.push(with_env(Step::new("setup", "poetry", &["install", "--no-interaction"])));
        with_env(Step::new("test", "poetry", &["run", "pytest", "-q"]))
    } else if uv {
        let mut sync = with_env(Step::new("setup", "uv", &["sync", "--all-extras"]));
        sync.env.push(("UV_PROJECT_ENVIRONMENT".into(), venv.display().to_string()));
        install.push(sync);
        let mut run = with_env(Step::new("test", "uv", &["run", "pytest", "-q"]));
        run.env.push(("UV_PROJECT_ENVIRONMENT".into(), venv.display().to_string()));
        run
    } else if hatch {
        with_env(Step::new("test", "hatch", &["run", "pytest", "-q"]))
    } else {
        let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into(), "-q".into()];
        let installable = pyproject.contains("[project]") || pyproject.contains("[build-system]") || tree.join("setup.py").is_file() || tree.join("setup.cfg").is_file();
        if installable {
            let extras = test_extras(&pyproject);
            args.push("-e".into());
            args.push(if extras.is_empty() { ".".into() } else { format!(".[{}]", extras.join(",")) });
        }
        for file in requirement_files(tree) {
            args.push("-r".into());
            args.push(file);
        }
        args.push("pytest".into());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        install.push(with_env(Step::new("setup", venv_python.clone(), &refs)));
        with_env(Step::new("test", venv_python, &["-m", "pytest", "-q"]))
    };
    if !ready {
        for step in install {
            out.push(setup(step));
        }
        if let Some(last) = out.last_mut() {
            last.stamp = Some((stamp_file, stamp));
        }
    }
    out.push(test);
    out
}

/// The dependency files that decide what `node_modules` holds, as one stamp.
fn node_stamp(tree: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in ["package.json", "package-lock.json", "pnpm-lock.yaml", "yarn.lock", "bun.lock", "bun.lockb"] {
        file.hash(&mut hasher);
        std::fs::read(tree.join(file)).unwrap_or_default().hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

fn package_has_deps(package: &Value) -> bool {
    let lists = |key: &str| package[key].as_object().is_some_and(|deps| !deps.is_empty());
    lists("dependencies") || lists("devDependencies")
}

/// Whether a `package.json` script invokes a command that is already on `PATH` (for example
/// `node --test tests/app.test.mjs`), so it does not require `node_modules/.bin`.
fn script_runs_without_modules(body: &str) -> bool {
    let first = body
        .split_whitespace()
        .find(|token| !token.contains('=') || token.starts_with('='))
        .unwrap_or("");
    !first.is_empty() && on_path(first)
}

/// What to run in `tree`, in order. A step whose tool is missing is left out. Python runs in
/// `venv`, an environment of the run's own that is made and filled on first use.
pub fn steps(tree: &Path, venv: &Path) -> Vec<Step> {
    let mut out = Vec::new();
    if let Ok(Some(loaded)) = crate::project_config::load(tree)
        && loaded.trusted
        && let Some(test_cmd) = loaded.config.test.as_deref().filter(|s| !s.trim().is_empty())
    {
        out.push(Step::new("test", "sh", &["-c", test_cmd]));
        return out;
    }
    if let Ok(text) = std::fs::read_to_string(tree.join("package.json"))
        && let Ok(package) = serde_json::from_str::<Value>(&text)
    {
        let has_deps = package_has_deps(&package);
        let has_modules = tree.join("node_modules").exists();
        let manager = if tree.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if tree.join("yarn.lock").is_file() {
            "yarn"
        } else if tree.join("bun.lockb").is_file() || tree.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        if on_path(manager) {
            if has_deps {
                let stamp = node_stamp(tree);
                let stamp_file = tree.join("node_modules/.taskmaster-deps");
                let stale = std::fs::read_to_string(&stamp_file).is_ok_and(|have| have != stamp);
                if !has_modules || stale {
                    let args: &[&str] = match manager {
                        "npm" => &["install", "--no-audit", "--no-fund", "--loglevel=error"],
                        _ => &["install"],
                    };
                    let mut install = Step::new("setup", manager, args);
                    install.setup = true;
                    install.stamp = Some((stamp_file, stamp));
                    out.push(install);
                }
            }
            for script in ["build", "test"] {
                let Some(body) = package["scripts"][script].as_str() else { continue };
                // What `npm init` writes: a test script that only fails.
                if body.contains("no test specified") {
                    continue;
                }
                if !has_modules && !has_deps && !script_runs_without_modules(body) {
                    continue;
                }
                out.push(Step::new(script, manager, &["run", script]));
            }
        }
    }
    let node_tests = node_test_files(tree);
    if !node_tests.is_empty() && on_path("node") {
        let mut args: Vec<String> = vec!["--test".into()];
        args.extend(node_tests);
        out.push(Step { name: "test".into(), program: "node".into(), args, ..Default::default() });
    }
    if tree.join("Cargo.toml").is_file() && on_path("cargo") {
        out.push(Step::new("test", "cargo", &["test", "--quiet"]));
    }
    if python_tests(tree) {
        out.extend(python_steps(tree, venv));
    }
    out
}

/// Why a project with tests has nothing to run them with, when that is so.
pub fn missing_runner(tree: &Path) -> Option<String> {
    if python_tests(tree) && !on_path("python3") && !on_path("python") {
        return Some("the project has Python tests, but no python3 is on PATH".into());
    }
    if tree.join("Cargo.toml").is_file() && !on_path("cargo") {
        return Some("the project is a Rust crate, but cargo is not on PATH".into());
    }
    if let Ok(text) = std::fs::read_to_string(tree.join("package.json"))
        && let Ok(package) = serde_json::from_str::<Value>(&text)
        && let Some(body) = package["scripts"]["test"].as_str()
        && !body.contains("no test specified")
    {
        let manager = if tree.join("pnpm-lock.yaml").is_file() {
            "pnpm"
        } else if tree.join("yarn.lock").is_file() {
            "yarn"
        } else if tree.join("bun.lockb").is_file() || tree.join("bun.lock").is_file() {
            "bun"
        } else {
            "npm"
        };
        if !on_path(manager) {
            return Some(format!("the project has a test script, but {manager} is not on PATH"));
        }
        if !tree.join("node_modules").exists()
            && !package_has_deps(&package)
            && !script_runs_without_modules(body)
        {
            return Some("the project has a test script, but its packages are not installed (no node_modules)".into());
        }
    }
    None
}

fn tail(text: &str) -> String {
    if text.len() <= KEEP {
        return text.to_string();
    }
    let mut start = text.len() - KEEP;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…\n{}", &text[start..])
}

/// Run the steps in order and stop at the first check that fails: tests after a failed build
/// say nothing new. A failed setup step is kept and shown with the next failure.
pub async fn run_steps(tree: &Path, steps: &[Step], timeout: Duration) -> (Vec<Timing>, Option<Failure>) {
    let mut timings = Vec::new();
    let mut setup_said = String::new();
    for step in steps {
        let began = Instant::now();
        eprintln!("taskmaster: running `{}`", step.line());
        let mut cmd = Command::new(&step.program);
        clean_env(&mut cmd);
        cmd.args(&step.args)
            .current_dir(tree)
            .env("CI", "1")
            .env("NEXT_TELEMETRY_DISABLED", "1")
            .env("FORCE_COLOR", "0")
            .env("NO_COLOR", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1");
        for (key, value) in &step.env {
            cmd.env(key, value);
        }
        let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                timings.push(Timing::new(&step.name, began.elapsed(), "failed"));
                let said = format!("could not start {}: {err}", step.program);
                if step.setup {
                    setup_said.push_str(&format!("`{}` failed: {said}\n", step.line()));
                    continue;
                }
                return (timings, Some(Failure { step: step.name.clone(), output: format!("{setup_said}{said}") }));
            }
        };
        let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let read = async {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let _ = tokio::join!(out.read_to_end(&mut a), err.read_to_end(&mut b));
            let status = child.wait().await;
            (status, a, b)
        };
        match tokio::time::timeout(timeout, read).await {
            Ok((Ok(status), _, _)) if status.success() => {
                timings.push(Timing::new(&step.name, began.elapsed(), "ok"));
                if let Some((file, stamp)) = &step.stamp {
                    let _ = std::fs::write(file, stamp);
                }
            }
            Ok((_, a, b)) => {
                let text = format!("{}\n{}", String::from_utf8_lossy(&a), String::from_utf8_lossy(&b));
                timings.push(Timing::new(&step.name, began.elapsed(), "failed"));
                if step.setup {
                    setup_said.push_str(&format!("`{}` failed:\n{}\n", step.line(), tail(text.trim())));
                    continue;
                }
                return (timings, Some(Failure { step: step.name.clone(), output: tail(&format!("{setup_said}{}", text.trim())) }));
            }
            Err(_) => {
                timings.push(Timing::new(&step.name, began.elapsed(), "timeout"));
                let said = format!("`{}` did not finish in {} seconds", step.line(), timeout.as_secs());
                if step.setup {
                    setup_said.push_str(&format!("{said}\n"));
                    continue;
                }
                return (timings, Some(Failure { step: step.name.clone(), output: format!("{setup_said}{said}") }));
            }
        }
    }
    (timings, None)
}

/// Paths in output, as `./app/x/page.tsx:12:5`, `app/x/page.tsx(12,5)` or `src/x.rs:3`.
pub fn paths_in(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in line.split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '(' | ')' | ',' | '<' | '>')) {
        let word = word.trim_start_matches("./").trim_end_matches(|c: char| c == ':' || c == '.' || c.is_ascii_digit());
        let word = word.split(':').next().unwrap_or("");
        let has_ext = [".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".css", ".rs", ".py"].iter().any(|ext| word.ends_with(ext));
        if has_ext && !word.starts_with('/') && !word.contains("node_modules") && !word.contains("://") {
            out.push(word.to_string());
        }
    }
    // A route group or segment in brackets is split off by the separators above.
    if let Some(start) = line.find("./") {
        let rest = &line[start + 2..];
        let end = rest.find(|c: char| c.is_whitespace() || c == ':' || c == '\'' || c == '"').unwrap_or(rest.len());
        let candidate = &rest[..end];
        if candidate.contains('.') && !out.iter().any(|have| have == candidate) {
            out.push(candidate.to_string());
        }
    }
    out
}

/// Which page each line of `output` is about, by a file it names or the route it names
/// (`prerendering page "/books"`). Lines that name no page are returned apart.
pub fn attribute(output: &str, owners: &Owners, routes: &[(PageId, String)]) -> (BTreeMap<PageId, String>, String) {
    let mut pages: BTreeMap<PageId, String> = BTreeMap::new();
    let mut rest = String::new();
    let mut current: Option<PageId> = None;
    for line in output.lines() {
        let by_file = paths_in(line).iter().find_map(|path| owners.owner(path).cloned());
        let by_route = routes.iter().find_map(|(page, route)| {
            let quoted = [format!("\"{route}\""), format!("'{route}'"), format!("page {route}"), format!("route {route}")];
            quoted.iter().any(|q| line.contains(q.as_str())).then(|| page.clone())
        });
        if let Some(page) = by_file.or(by_route) {
            current = Some(page);
        } else if line.trim().is_empty() {
            current = None;
        }
        match &current {
            Some(page) => {
                let text = pages.entry(page.clone()).or_default();
                text.push_str(line);
                text.push('\n');
            }
            None => {
                rest.push_str(line);
                rest.push('\n');
            }
        }
    }
    (pages, rest.trim().to_string())
}

// The browser.

/// A Chrome or Chromium to drive: `$TASKMASTER_CHROME`, or one on `PATH`.
pub fn chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TASKMASTER_CHROME").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }
    let paths = std::env::var_os("PATH")?;
    for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"] {
        for dir in std::env::split_paths(&paths) {
            let path = dir.join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Whether `tree` is a Next.js app that has been built.
pub fn is_built_next(tree: &Path) -> bool {
    let next = tree.join("node_modules/.bin/next");
    let package = std::fs::read_to_string(tree.join("package.json")).unwrap_or_default();
    next.exists() && package.contains("\"next\"") && tree.join(".next/BUILD_ID").is_file()
}

/// Whether `tree` declares a Node server/SSR backend that needs its own server process
/// rather than serving a static `dist/` or `build/` bundle directly.
fn has_server_backend(tree: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(tree.join("package.json")) else { return false };
    let Ok(package) = serde_json::from_str::<Value>(&text) else { return false };
    let has_dep = |name: &str| package["dependencies"][name].is_string() || package["devDependencies"][name].is_string();
    const BACKEND_DEPS: [&str; 13] = [
        "express",
        "fastify",
        "koa",
        "hono",
        "@hono/node-server",
        "next",
        "nuxt",
        "@sveltejs/kit",
        "remix",
        "@remix-run/node",
        "socket.io",
        "ws",
        "@nestjs/core",
    ];
    if BACKEND_DEPS.iter().any(|dep| has_dep(dep)) {
        return true;
    }
    if let Some(start) = package["scripts"]["start"].as_str() {
        let lower = start.to_ascii_lowercase();
        if ["node ", "tsx ", "ts-node ", "bun ", "deno "].iter().any(|k| lower.contains(k)) {
            return true;
        }
    }
    false
}

/// A built static SPA output directory (`dist/` or `build/`), when there is no backend server.
fn built_spa_dir(tree: &Path) -> Option<PathBuf> {
    if has_server_backend(tree) {
        return None;
    }
    ["dist", "build"]
        .into_iter()
        .map(|dir| tree.join(dir))
        .find(|dir| dir.join("index.html").is_file())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WebApp {
    /// Built Next.js app started with `next start`.
    Next,
    /// Built static SPA (`dist/` or `build/`) served directly with SPA fallback.
    BuiltSpa(PathBuf),
    /// Any web project with a `dev`, `start`, or `preview` script (or `.taskmaster.yaml` preview).
    DevServer,
    /// Pure static HTML site (no package.json dev/start/preview scripts).
    Static(&'static str),
}

fn detect_web_app(tree: &Path) -> Option<WebApp> {
    if is_built_next(tree) {
        return Some(WebApp::Next);
    }
    if let Some(dir) = built_spa_dir(tree) {
        return Some(WebApp::BuiltSpa(dir));
    }
    if let Ok(Some(loaded)) = crate::project_config::load(tree)
        && loaded.trusted
        && loaded.config.preview.as_deref().is_some_and(|s| !s.trim().is_empty())
    {
        return Some(WebApp::DevServer);
    }
    if crate::preview::dev_command(tree, "", 1).is_some() {
        return Some(WebApp::DevServer);
    }
    if crate::static_site::has_no_scripts(tree)
        && let Some(start) = crate::static_site::start_page(tree)
    {
        return Some(WebApp::Static(start));
    }
    None
}

/// Whether `browse()` knows how to launch and verify `tree` in a browser.
pub fn can_browse(tree: &Path) -> bool {
    detect_web_app(tree).is_some()
}

#[derive(Debug, Default)]
pub struct Browsed {
    /// Errors per page, ready to hand to the page's owner.
    pub pages: BTreeMap<PageId, String>,
    /// What the free UI checks found on each page (see `audit.rs`).
    pub audits: BTreeMap<PageId, crate::audit::Audit>,
    pub screenshots: Vec<(PageId, Vec<u8>)>,
    /// What could not be checked, and why.
    pub notes: Vec<String>,
}

/// A route with its dynamic segments filled in, and whether it had any. `[id]` becomes
/// `1` and an optional catch-all is dropped. A dynamic page may answer 404 for a made-up
/// id, so that is not held against it.
pub fn sample_path(route: &str) -> (String, bool) {
    let mut dynamic = false;
    let mut parts = Vec::new();
    for segment in route.split('/') {
        if segment.starts_with("[[") {
            dynamic = true;
            continue;
        }
        if segment.starts_with('[') {
            dynamic = true;
            parts.push("1".to_string());
        } else if !(segment.starts_with('(') && segment.ends_with(')')) {
            parts.push(segment.to_string());
        }
    }
    let path = parts.join("/");
    let path = if path.starts_with('/') { path } else { format!("/{path}") };
    (path.replace("//", "/"), dynamic)
}

struct Server(tokio::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.id() {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &format!("-{pid}")])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.0.start_kill();
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) async fn free_port() -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}

/// Wait until `127.0.0.1:port` accepts a TCP connection or the process exits / times out.
async fn wait_for_server(
    mut child: tokio::process::Child,
    port: u16,
    label: &str,
    within: Duration,
) -> anyhow::Result<(Server, u16)> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut server = Server(child);
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok((server, port));
        }
        if let Ok(Some(status)) = server.0.try_wait() {
            let mut out = String::new();
            let mut err = String::new();
            if let Some(mut s) = stdout {
                let _ = tokio::time::timeout(Duration::from_secs(1), s.read_to_string(&mut out)).await;
            }
            if let Some(mut s) = stderr {
                let _ = tokio::time::timeout(Duration::from_secs(1), s.read_to_string(&mut err)).await;
            }
            let combined = format!("{}\n{}", out.trim(), err.trim());
            bail!("`{label}` exited early ({status}): {}", tail(combined.trim()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut out = String::new();
    let mut err = String::new();
    if let Some(mut s) = stdout {
        let _ = tokio::time::timeout(Duration::from_secs(1), s.read_to_string(&mut out)).await;
    }
    if let Some(mut s) = stderr {
        let _ = tokio::time::timeout(Duration::from_secs(1), s.read_to_string(&mut err)).await;
    }
    let combined = format!("{}\n{}", out.trim(), err.trim());
    bail!("`{label}` did not start in {} seconds: {}", within.as_secs(), tail(combined.trim()))
}

/// Start a built Next.js app and wait until it answers.
async fn start_next_app(tree: &Path) -> anyhow::Result<(Server, u16)> {
    let port = free_port().await?;
    let mut cmd = Command::new(tree.join("node_modules/.bin/next"));
    clean_env(&mut cmd);
    cmd.args(["start", "-p", &port.to_string(), "-H", "127.0.0.1"])
        .current_dir(tree)
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().context("could not start the app with `next start`")?;
    wait_for_server(child, port, "next start", Duration::from_secs(60)).await
}

/// Start a dev/start/preview server (Vite, Express, React, etc.) and wait until it answers.
async fn start_dev_app(tree: &Path) -> anyhow::Result<(Server, u16)> {
    let port = free_port().await?;
    let yaml_preview = crate::project_config::load(tree)
        .ok()
        .flatten()
        .filter(|l| l.trusted)
        .and_then(|l| l.config.preview.as_deref().map(str::to_string))
        .map(|cmd| cmd.replace("$PORT", &port.to_string()).replace("${PORT}", &port.to_string()));
    let command = yaml_preview
        .or_else(|| crate::preview::dev_command(tree, "", port))
        .context("no dev/start/preview command available")?;
    let within = if command.contains(" install") && command.contains(" && ") {
        Duration::from_secs(180)
    } else {
        Duration::from_secs(60)
    };
    let mut cmd = Command::new("sh");
    clean_env(&mut cmd);
    cmd.args(["-c", &command])
        .current_dir(tree)
        .env("PORT", port.to_string())
        .env("HOST", "127.0.0.1")
        .env("BROWSER", "none")
        .env("CI", "1")
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().with_context(|| format!("could not start `{command}`"))?;
    wait_for_server(child, port, &command, within).await
}

fn is_checked_subresource(kind: &str) -> bool {
    matches!(kind, "Image" | "Script" | "Stylesheet" | "Fetch" | "XHR" | "Media")
}

/// What went wrong on one page, from the events Chrome sent for its session.
pub(crate) fn problems(events: &[Value], session: &str, url: &str, dynamic: bool) -> Vec<String> {
    let mut request_urls: BTreeMap<String, String> = BTreeMap::new();
    for event in events.iter().filter(|e| e["sessionId"] == session) {
        if event["method"].as_str() == Some("Network.requestWillBeSent") {
            let params = &event["params"];
            if let (Some(id), Some(req_url)) = (params["requestId"].as_str(), params["request"]["url"].as_str()) {
                request_urls.insert(id.to_string(), req_url.to_string());
            }
        }
    }
    let mut reported_urls: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for event in events.iter().filter(|e| e["sessionId"] == session) {
        let params = &event["params"];
        match event["method"].as_str().unwrap_or("") {
            "Network.responseReceived" => {
                let kind = params["type"].as_str().unwrap_or("");
                let status = params["response"]["status"].as_u64().unwrap_or(0);
                let resp_url = params["response"]["url"].as_str().unwrap_or("");
                if kind == "Document" {
                    if status >= 400 && !(status == 404 && dynamic) {
                        out.push(format!("the page answered HTTP {status}"));
                    }
                } else if status >= 400
                    && is_checked_subresource(kind)
                    && !resp_url.ends_with("/favicon.ico")
                    && !resp_url.starts_with("data:")
                {
                    if !resp_url.is_empty() {
                        reported_urls.insert(resp_url.to_string());
                    }
                    out.push(format!("FAILED_RESOURCE ({kind}): HTTP {status} loading {resp_url}"));
                }
            }
            "Network.loadingFailed" => {
                let kind = params["type"].as_str().unwrap_or("");
                let err_text = params["errorText"].as_str().unwrap_or("unknown error");
                let canceled = params["canceled"].as_bool().unwrap_or(false);
                if kind == "Document" {
                    out.push(format!("the page did not load: {err_text}"));
                } else if !canceled && err_text != "net::ERR_ABORTED" && is_checked_subresource(kind) {
                    let req_url = params["requestId"]
                        .as_str()
                        .and_then(|id| request_urls.get(id))
                        .map(String::as_str)
                        .unwrap_or("");
                    if req_url.ends_with("/favicon.ico") {
                        continue;
                    }
                    if !req_url.is_empty() {
                        reported_urls.insert(req_url.to_string());
                    }
                    let target = if req_url.is_empty() { String::new() } else { format!(" loading {req_url}") };
                    out.push(format!("FAILED_RESOURCE ({kind}): {err_text}{target}"));
                }
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let text = details["exception"]["description"].as_str().or(details["text"].as_str()).unwrap_or("an exception");
                let at = details["url"].as_str().map(|u| format!(" at {u}:{}", details["lineNumber"])).unwrap_or_default();
                // The message and the first frame; the rest is the framework.
                out.push(format!("uncaught error{at}: {}", text.lines().take(2).collect::<Vec<_>>().join("\n  ")));
            }
            "Runtime.consoleAPICalled" if params["type"] == "error" => {
                let args: Vec<String> = params["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| a["value"].as_str().map(str::to_string).or(a["description"].as_str().map(str::to_string)).unwrap_or_default())
                    .collect();
                out.push(format!("console error: {}", args.join(" ").lines().take(6).collect::<Vec<_>>().join("\n  ")));
            }
            "Log.entryAdded" if params["entry"]["level"] == "error" => {
                let entry = &params["entry"];
                let source = entry["url"].as_str().unwrap_or("");
                // The page's own status is reported above. A browser asks for a favicon by
                // itself, and no page asked for one. Subresources already reported via
                // Network.responseReceived/loadingFailed are not duplicated.
                let about_page = source.trim_end_matches('/') == url.trim_end_matches('/');
                if about_page || source.ends_with("/favicon.ico") || (!source.is_empty() && reported_urls.contains(source)) {
                    continue;
                }
                let at = entry["url"].as_str().map(|u| format!(" ({u})")).unwrap_or_default();
                out.push(format!("browser error: {}{at}", entry["text"].as_str().unwrap_or("")));
            }
            _ => {}
        }
    }
    out.dedup();
    out
}

/// Wait for in-DOM images to finish loading, then report broken `<img>` tags (`naturalWidth === 0`)
/// and completely blank rendered pages.
const DOM_HEALTH_JS: &str = r#"(async () => {
  const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
  const imgs = [...document.querySelectorAll("img")].filter((img) => (img.getAttribute("src") || "").trim());
  const pending = imgs.filter((img) => !img.complete);
  if (pending.length) {
    await Promise.race([
      Promise.all(
        pending.map(
          (img) =>
            new Promise((done) => {
              img.addEventListener("load", done, { once: true });
              img.addEventListener("error", done, { once: true });
            }),
        ),
      ),
      sleep(4000),
    ]);
  }
  const issues = [];
  const broken = [];
  for (const img of document.querySelectorAll("img")) {
    const style = getComputedStyle(img);
    if (style.display === "none" || style.visibility === "hidden") continue;
    const rawSrc = (img.getAttribute("src") || "").trim();
    if (!rawSrc || !img.complete || img.naturalWidth === 0 || img.naturalHeight === 0) {
      const alt = (img.getAttribute("alt") || "").trim().slice(0, 40);
      const shown = rawSrc ? rawSrc.slice(0, 120) : "(empty src)";
      broken.push(alt ? `alt="${alt}" src="${shown}"` : `src="${shown}"`);
    }
  }
  if (broken.length) {
    const sample = broken.slice(0, 4).join(", ");
    const extra = broken.length > 4 ? ` (+${broken.length - 4} more)` : "";
    issues.push(`BROKEN_IMAGE: ${broken.length} <img> element(s) failed to load (naturalWidth=0): ${sample}${extra}`);
  }
  const textLen = (document.body?.innerText || "").trim().length;
  const hasVisibleMedia = [...document.querySelectorAll("img, svg, canvas, video")].some((el) => {
    const r = el.getBoundingClientRect();
    return r.width > 4 && r.height > 4;
  });
  if (textLen === 0 && !hasVisibleMedia) {
    issues.push("BLANK_PAGE: the page rendered completely blank (no visible text or media)");
  }
  return issues;
})()"#;

async fn dom_health_issues(browser: &mut Browser, session: &str) -> Vec<String> {
    let Ok(out) = browser
        .call("Runtime.evaluate", json!({ "expression": DOM_HEALTH_JS, "returnByValue": true, "awaitPromise": true }), Some(session))
        .await
    else {
        return Vec::new();
    };
    out["result"]["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Open each page in Chrome. `None` when the project is not a browsable web app, or there
/// is no Chrome; the reason is printed.
pub async fn browse(tree: &Path, routes: &[(PageId, String)]) -> anyhow::Result<Option<Browsed>> {
    let Some(app) = detect_web_app(tree) else {
        return Ok(None);
    };
    let Some(chrome) = chrome() else {
        eprintln!("taskmaster: no Chrome or Chromium found, so pages were not opened in a browser (set TASKMASTER_CHROME)");
        return Ok(None);
    };
    let mut browsed = Browsed::default();
    let static_start = match &app {
        WebApp::Static(start) => Some(*start),
        _ => None,
    };
    let (_server, port, _site) = match app {
        WebApp::Static(_) => {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            let site = tokio::spawn(crate::static_site::serve(listener, tree.to_path_buf()));
            (None, port, Some(AbortOnDrop(site)))
        }
        WebApp::BuiltSpa(dist_dir) => {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let port = listener.local_addr()?.port();
            let site = tokio::spawn(crate::static_site::serve_spa(listener, dist_dir));
            (None, port, Some(AbortOnDrop(site)))
        }
        WebApp::Next => match start_next_app(tree).await {
            Ok((server, port)) => (Some(server), port, None),
            Err(err) => {
                let msg = format!("APP_START_FAILED: the app server failed to start:\n{err:#}");
                browsed.notes.push(msg.clone());
                if let Some((page, _)) = routes.first() {
                    browsed.pages.insert(page.clone(), msg);
                }
                return Ok(Some(browsed));
            }
        },
        WebApp::DevServer => match start_dev_app(tree).await {
            Ok((server, port)) => (Some(server), port, None),
            Err(err) => {
                let msg = format!("APP_START_FAILED: the app server failed to start:\n{err:#}");
                browsed.notes.push(msg.clone());
                if let Some((page, _)) = routes.first() {
                    browsed.pages.insert(page.clone(), msg);
                }
                return Ok(Some(browsed));
            }
        },
    };
    let mut browser = Browser::launch(&chrome).await?;
    // A static site has no routes of its own: its start page stands for the first page.
    let static_routes: Vec<(PageId, String)> = match static_start {
        Some(start) => routes.first().map(|(page, _)| (page.clone(), format!("/{start}"))).into_iter().collect(),
        None => Vec::new(),
    };
    let routes = if static_start.is_some() { &static_routes[..] } else { routes };
    for (page, route) in routes {
        let (path, dynamic) = sample_path(route);
        let url = format!("http://127.0.0.1:{port}{path}");
        let target = browser.call("Target.createTarget", json!({ "url": "about:blank" }), None).await?;
        let target_id = target["targetId"].as_str().context("no target")?.to_string();
        let attached = browser.call("Target.attachToTarget", json!({ "targetId": target_id, "flatten": true }), None).await?;
        let session = attached["sessionId"].as_str().context("no session")?.to_string();
        for domain in ["Page.enable", "Runtime.enable", "Log.enable", "Network.enable"] {
            browser.call(domain, json!({}), Some(&session)).await?;
        }
        browser.events.clear();
        browser.call("Page.navigate", json!({ "url": url }), Some(&session)).await?;
        let mine = session.clone();
        browser.wait_for(Duration::from_secs(20), move |e| e["sessionId"] == mine.as_str() && e["method"] == "Page.loadEventFired").await;
        // Errors from hydration and effects come just after the load.
        browser.wait_for(Duration::from_millis(1500), |_| false).await;
        let mut found = problems(&browser.events, &session, &url, dynamic);
        let doc_ok = found.iter().all(|p| !p.starts_with("the page answered") && !p.starts_with("the page did not load"));
        let dynamic_404 = dynamic
            && browser.events.iter().any(|e| {
                e["sessionId"] == session
                    && e["method"] == "Network.responseReceived"
                    && e["params"]["type"] == "Document"
                    && e["params"]["response"]["status"].as_u64() == Some(404)
            });
        // Wait for images and check for broken <img> elements and blank rendered pages before screenshot.
        if doc_ok && !dynamic_404 {
            let dom_issues = dom_health_issues(&mut browser, &session).await;
            // Re-read events so any late image/fetch failures that occurred during waitImages are captured too.
            found = problems(&browser.events, &session, &url, dynamic);
            for issue in dom_issues {
                if !found.contains(&issue) {
                    found.push(issue);
                }
            }
        }
        if let Ok(shot) = browser.call("Page.captureScreenshot", json!({ "format": "png" }), Some(&session)).await
            && let Some(data) = shot["data"].as_str()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
        {
            browsed.screenshots.push((page.clone(), bytes));
        }
        // A page that loaded gets the UI checks; one that failed to has nothing to look at.
        if doc_ok && !dynamic_404 {
            match tokio::time::timeout(AUDIT_TIMEOUT, crate::audit::audit(&mut browser, &session)).await {
                Ok(Ok(audit)) => {
                    browsed.audits.insert(page.clone(), audit);
                }
                Ok(Err(err)) => browsed.notes.push(format!("the UI checks of {path} did not finish: {err:#}")),
                Err(_) => browsed.notes.push(format!("the UI checks of {path} took longer than {} seconds", AUDIT_TIMEOUT.as_secs())),
            }
        }
        if !found.is_empty() {
            let text = found.iter().map(|p| format!("In the browser at {path}: {p}")).collect::<Vec<_>>().join("\n");
            browsed.pages.insert(page.clone(), text);
        }
        let _ = browser.call("Target.closeTarget", json!({ "targetId": target_id }), None).await;
    }
    Ok(Some(browsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tm_core::{PagePlan, Plan};

    fn owners() -> Owners {
        let page = |id: &str, glob: &str| PagePlan {
            id: id.into(),
            route: format!("/{id}"),
            owned_glob: glob.into(),
            ..Default::default()
        };
        Owners::from_plan(&Plan {
            pages: vec![page("book", "app/books/**"), page("cart", "app/cart/**")],
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn build_output_goes_to_the_page_whose_file_or_route_it_names() {
        let output = "Creating an optimized production build ...\n\
                      Failed to compile.\n\
                      \n\
                      ./app/books/[id]/page.tsx:12:5\n\
                      Type error: Property 'title' does not exist on type 'Book'.\n\
                      \n\
                      Error occurred prerendering page \"/cart\". Read more\n\
                      TypeError: Cannot read properties of undefined (reading 'map')\n\
                      \n\
                      next.config.mjs is invalid";
        let routes = vec![("book".to_string(), "/books/[id]".to_string()), ("cart".to_string(), "/cart".to_string())];
        let (pages, rest) = attribute(output, &owners(), &routes);
        assert!(pages["book"].contains("Property 'title'"), "{pages:?}");
        assert!(pages["cart"].contains("reading 'map'"), "{pages:?}");
        assert!(rest.contains("next.config.mjs is invalid") && !rest.contains("title"), "{rest}");
        let (none, _) = attribute("src/app/books/page.tsx(3,1): error", &owners(), &routes);
        assert!(none.is_empty(), "a path outside every page's files goes to nobody");
    }

    #[test]
    fn a_dynamic_route_is_opened_with_a_sample_id() {
        assert_eq!(sample_path("/books/[id]"), ("/books/1".into(), true));
        assert_eq!(sample_path("/"), ("/".into(), false));
        assert_eq!(sample_path("/docs/[[...slug]]"), ("/docs".into(), true));
        assert_eq!(sample_path("/(shop)/cart"), ("/cart".into(), false));
    }

    #[test]
    fn browser_events_become_problems_for_that_page_only() {
        let s = "S1";
        let events = vec![
            json!({ "sessionId": s, "method": "Network.responseReceived", "params": { "type": "Document", "response": { "status": 500 } } }),
            json!({ "sessionId": s, "method": "Runtime.exceptionThrown", "params": { "exceptionDetails": { "text": "Uncaught", "url": "http://x/app.js", "lineNumber": 3, "exception": { "description": "TypeError: books.map is not a function" } } } }),
            json!({ "sessionId": s, "method": "Runtime.consoleAPICalled", "params": { "type": "error", "args": [{ "value": "Warning: Each child needs a key" }] } }),
            json!({ "sessionId": s, "method": "Runtime.consoleAPICalled", "params": { "type": "log", "args": [{ "value": "fine" }] } }),
            json!({ "sessionId": "other", "method": "Runtime.exceptionThrown", "params": { "exceptionDetails": { "text": "elsewhere" } } }),
        ];
        let found = problems(&events, s, "http://x/books", false);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("HTTP 500") && found[1].contains("books.map") && found[2].contains("key"));

        let missing = vec![
            json!({ "sessionId": s, "method": "Network.responseReceived", "params": { "type": "Document", "response": { "status": 404 } } }),
            json!({ "sessionId": s, "method": "Log.entryAdded", "params": { "entry": { "level": "error", "text": "Failed to load resource: 404", "url": "http://x/books/1" } } }),
        ];
        assert!(problems(&missing, s, "http://x/books/1", true).is_empty(), "a made-up id may be missing");
        assert_eq!(problems(&missing, s, "http://x/books/1", false), vec!["the page answered HTTP 404".to_string()]);
        let favicon = vec![json!({ "sessionId": s, "method": "Log.entryAdded", "params": { "entry": { "level": "error", "text": "404", "url": "http://x/favicon.ico" } } })];
        assert!(problems(&favicon, s, "http://x/", false).is_empty());
    }

    #[tokio::test]
    async fn the_projects_own_scripts_run_and_a_failure_keeps_its_output() {
        let dir = std::env::temp_dir().join(format!("tm-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ok = Step::new("build", "sh", &["-c", "echo built"]);
        let bad = Step::new("test", "sh", &["-c", "echo 'app/cart/page.tsx:1 boom' >&2; exit 1"]);
        let never = Step::new("after", "sh", &["-c", "touch ran"]);
        let (timed, failure) = run_steps(&dir, &[ok, bad, never], Duration::from_secs(10)).await;
        let failure = failure.unwrap();
        assert_eq!(timed.iter().map(|t| (t.name.as_str(), t.status)).collect::<Vec<_>>(), vec![("build", "ok"), ("test", "failed")]);
        assert_eq!(failure.step, "test");
        assert!(failure.output.contains("boom"));
        assert!(!dir.join("ran").exists(), "nothing runs after a failure");
        let slow = Step::new("build", "sleep", &["5"]);
        let (timed, slow_failure) = run_steps(&dir, &[slow], Duration::from_millis(200)).await;
        assert!(slow_failure.unwrap().output.contains("did not finish"));
        assert_eq!(timed[0].status, "timeout");

        std::fs::write(dir.join("package.json"), r#"{"scripts":{"build":"next build","test":"echo \"Error: no test specified\" && exit 1"}}"#).unwrap();
        let venv = dir.join("venv");
        assert!(steps(&dir, &venv).is_empty(), "nothing is installed yet");
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        let found = steps(&dir, &venv);
        if on_path("npm") {
            assert_eq!(found.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["build"]);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn python_tests_are_found_without_any_config_file() {
        let dir = std::env::temp_dir().join(format!("tm-pytests-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("toyredis")).unwrap();
        std::fs::write(dir.join("toyredis/server.py"), "x = 1\n").unwrap();
        assert!(!python_tests(&dir) && !has_tests(&dir));
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/test_server.py"), "def test(): pass\n").unwrap();
        assert!(python_tests(&dir) && has_tests(&dir), "a tests/ directory alone means pytest");
        let _ = std::fs::remove_dir_all(dir.join("tests"));
        std::fs::write(dir.join("test_server.py"), "def test(): pass\n").unwrap();
        assert!(python_tests(&dir), "so does a test_*.py beside the code");
        assert_eq!(test_extras("[project]\nname='x'\n[project.optional-dependencies]\ndev = ['pytest-cov']\ndocs = ['x']\ntest=['y']\n"), vec!["dev", "test"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Runs pytest for real, in a venv the run makes, where Python and pytest are installed.
    #[tokio::test]
    async fn python_tests_run_in_the_runs_own_venv_and_a_failure_is_kept() {
        let pytest = std::process::Command::new("python3").args(["-c", "import pytest"]).status().is_ok_and(|s| s.success());
        if !pytest {
            eprintln!("python3 with pytest is not installed; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("tm-pyvenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::write(dir.join("conftest.py"), "").unwrap();
        std::fs::write(dir.join("tests/test_calc.py"), "from calc import add\n\ndef test_add():\n    assert add(1, 1) == 2\n").unwrap();
        let venv = dir.join(".run/venv");
        let found = steps(&dir, &venv);
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.last(), Some(&"test"), "{names:?}");
        assert!(found.iter().filter(|s| s.setup).count() >= 2, "a venv, then an install: {names:?}");
        assert!(found.last().unwrap().program.starts_with(&venv.display().to_string()), "pytest runs from the run's venv");
        let (_, failure) = run_steps(&dir, &found, Duration::from_secs(120)).await;
        let failure = failure.expect("the broken add fails its test");
        assert_eq!(failure.step, "test");
        assert!(failure.output.contains("test_add") && failure.output.contains("1 failed"), "{}", failure.output);
        assert!(venv.join("pyvenv.cfg").is_file());
        assert!(!dir.join("tests/__pycache__").exists(), "no bytecode is left in the tree");
        // The environment is kept: the second verify only runs the tests.
        std::fs::write(dir.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        let again = steps(&dir, &venv);
        assert_eq!(again.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["test"]);
        let (_, failure) = run_steps(&dir, &again, Duration::from_secs(120)).await;
        assert!(failure.is_none(), "{failure:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Opens a real app in Chrome when `TM_BROWSER_TEST_APP` names a built Next.js app with
    /// the pages `/`, `/broken` (throws after hydration), `/noisy` (logs an error) and
    /// `/books/[id]` (404 for an unknown id). Skipped otherwise.
    #[tokio::test]
    async fn a_real_app_is_opened_and_each_page_gets_its_own_problems() {
        let Some(app) = std::env::var_os("TM_BROWSER_TEST_APP").map(PathBuf::from) else {
            eprintln!("TM_BROWSER_TEST_APP is not set; skipping");
            return;
        };
        let routes: Vec<(PageId, String)> = [("home", "/"), ("broken", "/broken"), ("noisy", "/noisy"), ("book", "/books/[id]"), ("gone", "/gone")]
            .iter()
            .map(|(p, r)| (p.to_string(), r.to_string()))
            .collect();
        let browsed = browse(&app, &routes).await.unwrap().expect("a built app and a Chrome");
        assert!(browsed.notes.is_empty(), "{:?}", browsed.notes);
        assert_eq!(browsed.screenshots.len(), 5);
        assert!(browsed.screenshots.iter().all(|(_, png)| png.starts_with(b"\x89PNG")));
        assert!(!browsed.pages.contains_key("home"), "{:?}", browsed.pages);
        assert!(!browsed.pages.contains_key("book"), "a made-up id may be missing: {:?}", browsed.pages);
        assert!(browsed.pages["broken"].contains("map"), "{:?}", browsed.pages);
        assert!(browsed.pages["noisy"].contains("cart total is NaN"), "{:?}", browsed.pages);
        assert!(browsed.pages["gone"].contains("HTTP 404"), "{:?}", browsed.pages);
        if let Some(dir) = std::env::var_os("TM_BROWSER_TEST_SHOTS") {
            for (page, png) in &browsed.screenshots {
                std::fs::write(PathBuf::from(&dir).join(format!("{page}.png")), png).unwrap();
            }
        }
        for (page, text) in &browsed.pages {
            eprintln!("{page}:\n{text}");
        }
    }

    #[test]
    fn node_projects_with_dependencies_emit_a_setup_install_step_when_uninstalled() {
        let dir = std::env::temp_dir().join(format!("tm-node-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"react":"^18.0.0"},"scripts":{"build":"vite build","dev":"vite"}}"#,
        )
        .unwrap();
        let found = steps(&dir, &dir.join("venv"));
        if on_path("npm") {
            let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, vec!["setup", "build"]);
            assert!(found[0].setup);
            assert!(found[0].stamp.is_some());
        }
        assert!(can_browse(&dir), "a project with a dev script is browsable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subresource_errors_and_failed_image_loads_are_reported() {
        let s = "S1";
        let events = vec![
            json!({ "sessionId": s, "method": "Network.requestWillBeSent", "params": { "requestId": "r1", "request": { "url": "http://127.0.0.1:4173/images/missing-horse.jpg" } } }),
            json!({ "sessionId": s, "method": "Network.responseReceived", "params": { "type": "Image", "response": { "status": 404, "url": "http://127.0.0.1:4173/images/missing-horse.jpg" } } }),
            json!({ "sessionId": s, "method": "Log.entryAdded", "params": { "entry": { "level": "error", "text": "Failed to load resource: 404", "url": "http://127.0.0.1:4173/images/missing-horse.jpg" } } }),
            json!({ "sessionId": s, "method": "Network.requestWillBeSent", "params": { "requestId": "r2", "request": { "url": "https://broken.invalid/horse.jpg" } } }),
            json!({ "sessionId": s, "method": "Network.loadingFailed", "params": { "requestId": "r2", "type": "Image", "errorText": "net::ERR_NAME_NOT_RESOLVED", "canceled": false } }),
            json!({ "sessionId": s, "method": "Network.loadingFailed", "params": { "requestId": "r3", "type": "Image", "errorText": "net::ERR_ABORTED", "canceled": true } }),
            json!({ "sessionId": s, "method": "Network.responseReceived", "params": { "type": "Image", "response": { "status": 404, "url": "http://127.0.0.1:4173/favicon.ico" } } }),
        ];
        let found = problems(&events, s, "http://127.0.0.1:4173/", false);
        assert_eq!(found.len(), 2, "duplicate Log.entryAdded, canceled loads, and favicon are excluded: {found:?}");
        assert!(found[0].contains("FAILED_RESOURCE (Image): HTTP 404") && found[0].contains("missing-horse.jpg"));
        assert!(found[1].contains("FAILED_RESOURCE (Image): net::ERR_NAME_NOT_RESOLVED") && found[1].contains("broken.invalid"));
    }

    #[tokio::test]
    async fn built_spa_detects_broken_images_blank_pages_and_passes_when_fixed() {
        if chrome().is_none() {
            eprintln!("no Chrome here; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("tm-spa-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"dev":"vite","build":"vite build"}}"#).unwrap();
        // Round 1: broken image (404 -> naturalWidth=0).
        std::fs::write(
            dir.join("dist/index.html"),
            r#"<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1"><style>body{font:16px system-ui}</style></head><body><h1>Horse Tinder</h1><img src="/nope.jpg" alt="Thunder"></body></html>"#,
        )
        .unwrap();
        let routes = vec![("app".to_string(), "/".to_string())];
        let browsed = browse(&dir, &routes).await.unwrap().expect("built SPA is browsed");
        let err = browsed.pages.get("app").expect("broken image fails verification");
        assert!(err.contains("BROKEN_IMAGE") && err.contains("nope.jpg"), "{err}");
        assert!(err.contains("FAILED_RESOURCE (Image): HTTP 404"), "{err}");

        // Round 2: completely blank page.
        std::fs::write(dir.join("dist/index.html"), "<!doctype html><html><body><div id=\"root\"></div></body></html>").unwrap();
        let browsed = browse(&dir, &routes).await.unwrap().expect("built SPA is browsed");
        let err = browsed.pages.get("app").expect("blank page fails verification");
        assert!(err.contains("BLANK_PAGE"), "{err}");

        // Round 3: fixed with a real valid PNG image!
        let tiny_png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
            0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
            0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
            0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(dir.join("dist/horse.png"), tiny_png).unwrap();
        std::fs::write(
            dir.join("dist/index.html"),
            r#"<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1"><style>body{font:16px system-ui}img{width:120px;height:120px}</style></head><body><h1>Horse Tinder</h1><img src="/horse.png" alt="Thunder"></body></html>"#,
        )
        .unwrap();
        let browsed = browse(&dir, &routes).await.unwrap().expect("built SPA is browsed");
        assert!(browsed.pages.is_empty(), "fixed SPA should pass cleanly: {:?}", browsed.pages);
        assert!(browsed.audits["app"].found.is_empty(), "{:?}", browsed.audits["app"].found);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn crashing_dev_server_fails_page_verification_instead_of_skipping() {
        if chrome().is_none() {
            eprintln!("no Chrome here; skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("tm-crash-dev-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"scripts":{"dev":"echo 'SyntaxError: broken config' >&2; exit 1"}}"#).unwrap();
        let routes = vec![("app".to_string(), "/".to_string())];
        let browsed = browse(&dir, &routes).await.unwrap().expect("dev app is attempted");
        let err = browsed.pages.get("app").expect("crashing server must fail verification");
        assert!(err.contains("APP_START_FAILED") && err.contains("SyntaxError: broken config"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_dep_node_test_scripts_and_standalone_node_tests_run_without_node_modules() {
        let dir = std::env::temp_dir().join(format!("tm-node-zero-dep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/app.test.mjs"), "import test from 'node:test'; test('ok', () => {});\n").unwrap();
        let venv = dir.join("venv");

        // Standalone node test file without package.json.
        assert!(has_tests(&dir));
        if on_path("node") {
            let found = steps(&dir, &venv);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].program, "node");
            assert_eq!(found[0].args, vec!["--test", "tests/app.test.mjs"]);
        }

        // Zero-dependency package.json invoking node --test (no node_modules needed).
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"app","type":"module","scripts":{"test":"node --test tests/app.test.mjs"}}"#,
        )
        .unwrap();
        assert!(has_tests(&dir));
        if on_path("npm") && on_path("node") {
            assert_eq!(missing_runner(&dir), None);
            let found = steps(&dir, &venv);
            assert_eq!(found.iter().map(|s| s.line()).collect::<Vec<_>>(), vec!["npm run test"]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
