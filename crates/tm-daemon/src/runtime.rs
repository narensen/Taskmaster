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
pub(crate) const KEEP: usize = 4096;

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
    python_tests(tree) || tree.join("Cargo.toml").is_file() || npm_test
}

/// Dependency-relevant subset extracted from `pyproject.toml`.
/// Cosmetic edits (`version`, `description`, `[tool.pytest.ini_options]`, `[tool.ruff]`, empty
/// `dependencies = []`, etc.) leave this empty so adding a stdlib-only `pyproject.toml` mid-run
/// never invalidates a working shared venv.
#[derive(Debug, Default, PartialEq, Eq)]
struct PyprojectDepInfo {
    deps: Vec<String>,
    scripts: Vec<String>,
    env_lines: Vec<String>,
}

impl PyprojectDepInfo {
    fn has_third_party_deps(&self) -> bool {
        self.deps.iter().any(|d| is_third_party_python_spec(d))
    }

    fn has_scripts(&self) -> bool {
        !self.scripts.is_empty()
    }

    fn fingerprint(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.deps.len() + self.scripts.len() + self.env_lines.len());
        for d in &self.deps {
            out.push(format!("dep:{d}"));
        }
        for s in &self.scripts {
            out.push(format!("script:{s}"));
        }
        for e in &self.env_lines {
            out.push(format!("env:{e}"));
        }
        out
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn extract_quoted_strings(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            let quote = ch;
            let mut val = String::new();
            while let Some(c) = chars.next() {
                if c == '\\' && quote == '"' {
                    if let Some(next) = chars.next() {
                        val.push(next);
                    }
                    continue;
                }
                if c == quote {
                    break;
                }
                val.push(c);
            }
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

fn is_third_party_python_spec(spec: &str) -> bool {
    let trimmed = spec.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('-') {
        return false;
    }
    let pkg: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .collect();
    let norm = pkg.to_ascii_lowercase().replace(['_', '.'], "-");
    !norm.is_empty() && norm != "pytest" && norm != "python"
}

fn requirement_file_has_third_party(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .is_some_and(|raw| raw.lines().any(is_third_party_python_spec))
}

fn parse_pyproject_dep_info(pyproject: &str) -> PyprojectDepInfo {
    let mut info = PyprojectDepInfo::default();
    let mut section = String::new();
    let mut collecting_array_key: Option<String> = None;
    let mut array_buf = String::new();

    for raw in pyproject.lines() {
        let clean = strip_toml_comment(raw).trim();
        if clean.is_empty() {
            continue;
        }
        if let Some(key) = collecting_array_key.clone() {
            array_buf.push(' ');
            array_buf.push_str(clean);
            if clean.contains(']') {
                for item in extract_quoted_strings(&array_buf) {
                    if section == "[project.optional-dependencies]" {
                        info.deps.push(format!("{key}:{item}"));
                    } else {
                        info.deps.push(item);
                    }
                }
                collecting_array_key = None;
                array_buf.clear();
            }
            continue;
        }
        if clean.starts_with('[') && clean.ends_with(']') {
            section = clean.to_ascii_lowercase();
            continue;
        }
        if section == "[project]" {
            if let Some((k, v)) = clean.split_once('=')
                && k.trim() == "dependencies"
            {
                let v = v.trim();
                if v.starts_with('[') {
                    if v.contains(']') {
                        info.deps.extend(extract_quoted_strings(v));
                    } else {
                        collecting_array_key = Some("dependencies".into());
                        array_buf = v.to_string();
                    }
                }
            }
        } else if section == "[project.optional-dependencies]" {
            if let Some((k, v)) = clean.split_once('=') {
                let key = k.trim().trim_matches('"').to_string();
                let v = v.trim();
                if v.starts_with('[') {
                    if v.contains(']') {
                        for item in extract_quoted_strings(v) {
                            info.deps.push(format!("{key}:{item}"));
                        }
                    } else {
                        collecting_array_key = Some(key);
                        array_buf = v.to_string();
                    }
                }
            }
        } else if section == "[project.scripts]"
            || section == "[project.gui-scripts]"
            || section.starts_with("[project.entry-points")
            || section == "[tool.poetry.scripts]"
        {
            if clean.contains('=') {
                info.scripts.push(clean.to_string());
            }
        } else if section == "[tool.poetry.dependencies]"
            || section == "[tool.poetry.dev-dependencies]"
            || section.starts_with("[tool.poetry.group.")
        {
            if let Some((k, v)) = clean.split_once('=') {
                let pkg = k.trim().trim_matches('"');
                if !pkg.eq_ignore_ascii_case("python") {
                    info.deps.push(format!("{pkg}={}", v.trim()));
                }
            }
        } else if section.starts_with("[tool.uv") || section.starts_with("[tool.hatch.envs") {
            info.env_lines.push(format!("{section}:{clean}"));
        }
    }

    info.deps.sort();
    info.deps.dedup();
    info.scripts.sort();
    info.scripts.dedup();
    info.env_lines.sort();
    info.env_lines.dedup();
    info
}

/// True when `tree` has a `pyproject.toml` declaring console/entry-point scripts (requiring
/// editable installation so `venv/bin/<script>` is generated).
pub(crate) fn pyproject_has_scripts(tree: &Path) -> bool {
    std::fs::read_to_string(tree.join("pyproject.toml"))
        .ok()
        .is_some_and(|raw| parse_pyproject_dep_info(&raw).has_scripts())
}

/// True when `tree` declares external third-party packages (Node, Python, Rust, or Go),
/// indicating the local `docs(package)` MCP tool may be useful or a venv needs third-party installs.
pub(crate) fn project_has_external_deps(tree: &Path) -> bool {
    if tree.join("node_modules").is_dir() {
        return true;
    }
    if let Ok(raw) = std::fs::read_to_string(tree.join("package.json"))
        && let Ok(val) = serde_json::from_str::<Value>(&raw)
    {
        for key in ["dependencies", "devDependencies", "peerDependencies"] {
            if val[key].as_object().is_some_and(|m| !m.is_empty()) {
                return true;
            }
        }
    }
    for req in requirement_files(tree) {
        if requirement_file_has_third_party(&tree.join(req)) {
            return true;
        }
    }
    if let Ok(raw) = std::fs::read_to_string(tree.join("pyproject.toml"))
        && parse_pyproject_dep_info(&raw).has_third_party_deps()
    {
        return true;
    }
    if let Ok(raw) = std::fs::read_to_string(tree.join("Cargo.toml"))
        && let Some((_, rest)) = raw.split_once("[dependencies]")
    {
        let has_crate = rest
            .lines()
            .take_while(|l| !l.trim_start().starts_with('['))
            .map(str::trim)
            .any(|l| !l.is_empty() && !l.starts_with('#') && l.contains('='));
        if has_crate {
            return true;
        }
    }
    if let Ok(raw) = std::fs::read_to_string(tree.join("go.mod"))
        && raw.lines().any(|l| l.trim_start().starts_with("require "))
    {
        return true;
    }
    false
}

/// The dependency files that decide what a Python environment holds, as one stamp.
/// Only dependency-affecting fields of `pyproject.toml` are hashed so cosmetic metadata or
/// `[tool.pytest.ini_options]` changes never invalidate an existing venv.
pub(crate) fn python_stamp(tree: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(raw) = std::fs::read_to_string(tree.join("pyproject.toml")) {
        let fp = parse_pyproject_dep_info(&raw).fingerprint();
        if !fp.is_empty() {
            "pyproject.toml".hash(&mut hasher);
            fp.hash(&mut hasher);
        }
    }
    let mut names: Vec<PathBuf> = ["poetry.lock", "uv.lock", "setup.py", "setup.cfg", "Pipfile.lock"]
        .iter()
        .map(|f| tree.join(f))
        .collect();
    names.extend(requirement_files(tree).into_iter().map(|f| tree.join(f)));
    for name in names {
        if let Ok(bytes) = std::fs::read(&name) {
            // Skip requirements files that only declare `pytest` (already in the shared venv).
            if name
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".txt"))
                && !requirement_file_has_third_party(&name)
            {
                continue;
            }
            name.file_name().hash(&mut hasher);
            bytes.hash(&mut hasher);
        }
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

/// Absolute `PYTHONPATH` entries for importing Python packages from `tree` regardless of a child
/// process's working directory (e.g. acceptance tests running `python -m pkg` with `cwd=tmpdir`).
///
/// Includes `<abs_tree>/src` first when `tree` uses a PEP 517 `src/` layout (`src/` directory
/// without `src/__init__.py`), followed by `<abs_tree>`, followed by any non-empty entries from
/// `existing` (deduplicated, preserving order).
pub fn pythonpath_for_tree(tree: &Path, existing: Option<&str>) -> String {
    let abs = if tree.is_absolute() {
        tree.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(tree))
            .unwrap_or_else(|_| tree.to_path_buf())
    };
    let mut entries: Vec<PathBuf> = Vec::new();
    let src = abs.join("src");
    if src.is_dir() && !src.join("__init__.py").is_file() {
        entries.push(src);
    }
    entries.push(abs);
    if let Some(prev) = existing {
        for p in std::env::split_paths(prev) {
            if !p.as_os_str().is_empty() && !entries.contains(&p) {
                entries.push(p);
            }
        }
    }
    std::env::join_paths(&entries)
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| {
            let sep = if cfg!(windows) { ";" } else { ":" };
            entries
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(sep)
        })
}

/// Prepend [`pythonpath_for_tree`] into `env`'s `PYTHONPATH` entry (updating in place if present,
/// or appending a new entry). Idempotent when called repeatedly for the same `tree`.
pub fn prepend_pythonpath(env: &mut Vec<(String, String)>, tree: &Path) {
    if let Some((_, val)) = env.iter_mut().find(|(k, _)| k == "PYTHONPATH") {
        *val = pythonpath_for_tree(tree, Some(val.as_str()));
    } else {
        env.push(("PYTHONPATH".into(), pythonpath_for_tree(tree, None)));
    }
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
    let venv_env = vec![
        ("VIRTUAL_ENV".to_string(), venv.display().to_string()),
        ("PATH".to_string(), path),
        ("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into()),
        // Never let host `~/.local/lib` user site-packages mask missing venv installs: workers run
        // inside bubblewrap with `--tmpfs $HOME`, so every dependency must live inside `venv` (or
        // system site-packages under `/usr`).
        ("PYTHONNOUSERSITE".into(), "1".into()),
        ("PIP_USER".into(), "0".into()),
    ];
    let python_bin = bin.join(if cfg!(windows) { "python.exe" } else { "python" });
    let stamp = python_stamp(tree);
    let stamp_file = venv.join(".taskmaster-deps");
    let ready = std::fs::read_to_string(&stamp_file).is_ok_and(|have| have == stamp) && python_bin.is_file();
    let mut out = Vec::new();
    let with_env = |mut step: Step| {
        step.env = venv_env.clone();
        step
    };
    let setup = |mut step: Step| {
        step.setup = true;
        step
    };
    // Never recreate an existing venv over a working `bin/python`: only create it when missing.
    if !python_bin.is_file() {
        // System packages stay visible underneath, so a machine with no network still has pytest.
        out.push(setup(Step::new("setup", python, &["-m", "venv", "--system-site-packages", &venv.display().to_string()])));
    }
    let venv_python = python_bin.display().to_string();
    let py_info = parse_pyproject_dep_info(&pyproject);
    let poetry = (pyproject.contains("[tool.poetry") || tree.join("poetry.lock").is_file()) && on_path("poetry");
    let uv = (tree.join("uv.lock").is_file() || pyproject.contains("[tool.uv")) && on_path("uv");
    let hatch = pyproject.contains("[tool.hatch.envs") && on_path("hatch");
    let mut install: Vec<Step> = Vec::new();
    let test = if poetry {
        install.push(with_env(Step::new("setup", "poetry", &["install", "--no-interaction"])));
        with_env(Step::new("test", "poetry", &["run", "pytest", "-q", "-x", "--tb=short"]))
    } else if uv {
        let mut sync = with_env(Step::new("setup", "uv", &["sync", "--all-extras"]));
        sync.env.push(("UV_PROJECT_ENVIRONMENT".into(), venv.display().to_string()));
        install.push(sync);
        let mut run = with_env(Step::new("test", "uv", &["run", "pytest", "-q", "-x", "--tb=short"]));
        run.env.push(("UV_PROJECT_ENVIRONMENT".into(), venv.display().to_string()));
        run
    } else if hatch {
        with_env(Step::new("test", "hatch", &["run", "pytest", "-q", "-x", "--tb=short"]))
    } else {
        let mut args: Vec<String> = vec!["-m".into(), "pip".into(), "install".into(), "-q".into()];
        let has_legacy_setup = tree.join("setup.py").is_file() || tree.join("setup.cfg").is_file();
        let reqs = requirement_files(tree);
        let has_external = py_info.has_third_party_deps()
            || has_legacy_setup
            || reqs.iter().any(|f| requirement_file_has_third_party(&tree.join(f)));
        let installable = pyproject.contains("[project]") || pyproject.contains("[build-system]") || has_legacy_setup;
        if installable {
            if !has_external && !py_info.has_scripts() {
                // Pure stdlib package with no console scripts: skip `-e .` so offline runs never
                // invoke PEP 517 build isolation against PyPI.
            } else if !has_external && py_info.has_scripts() {
                // Stdlib package with `[project.scripts]`: install console entry points offline-safely.
                args.push("--no-build-isolation".into());
                args.push("--no-deps".into());
                args.push("-e".into());
                args.push(".".into());
            } else {
                let extras = test_extras(&pyproject);
                args.push("-e".into());
                args.push(if extras.is_empty() { ".".into() } else { format!(".[{}]", extras.join(",")) });
            }
        }
        for file in reqs {
            if requirement_file_has_third_party(&tree.join(&file)) {
                args.push("-r".into());
                args.push(file);
            }
        }
        if !args.iter().any(|a| a == "--no-deps") {
            args.push("pytest".into());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        install.push(with_env(Step::new("setup", venv_python.clone(), &refs)));
        let mut run = with_env(Step::new("test", venv_python, &["-m", "pytest", "-q", "-x", "--tb=short"]));
        // Always set an absolute per-tree PYTHONPATH (never relative `.`) so subprocesses spawned
        // from other working directories (e.g. `python -m pkg` with `cwd=tmpdir`) import this tree's
        // package reliably even when `-e .` is skipped or a shared read-only venv is reused.
        prepend_pythonpath(&mut run.env, tree);
        run
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
        && tree.join("node_modules").exists()
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
        if on_path(manager) {
            for script in ["build", "test"] {
                let Some(body) = package["scripts"][script].as_str() else { continue };
                // What `npm init` writes: a test script that only fails.
                if body.contains("no test specified") {
                    continue;
                }
                out.push(Step::new(script, manager, &["run", script]));
            }
        }
    }
    if tree.join("Cargo.toml").is_file() && on_path("cargo") {
        out.push(Step::new("test", "cargo", &["test", "--quiet"]));
    }
    if python_tests(tree) {
        out.extend(python_steps(tree, venv));
    }
    out
}

/// Derive `.flow/env.json` facts from a set of verified [`Step`]s that just passed.
pub fn env_facts_from_steps(steps: &[Step], venv: &Path) -> Option<tm_flow::EnvFacts> {
    let test_step = steps
        .iter()
        .rev()
        .find(|s| s.name == "test" && !s.setup)
        .or_else(|| steps.iter().rev().find(|s| !s.setup))?;
    let test_cmd = test_step.line();
    if test_cmd.is_empty() {
        return None;
    }
    let is_pytest = is_pytest_step(test_step);
    let runner = if is_pytest {
        if matches!(test_step.program.as_str(), "poetry" | "uv" | "hatch") {
            test_step.program.clone()
        } else {
            "pytest".to_string()
        }
    } else if test_step.program == "cargo" {
        "cargo".to_string()
    } else if matches!(test_step.program.as_str(), "npm" | "pnpm" | "yarn" | "bun") {
        test_step.program.clone()
    } else {
        "custom".to_string()
    };

    let env_venv = test_step
        .env
        .iter()
        .find(|(k, _)| k == "VIRTUAL_ENV" || k == "UV_PROJECT_ENVIRONMENT")
        .map(|(_, v)| PathBuf::from(v))
        .or_else(|| {
            (is_pytest && venv.join("pyvenv.cfg").is_file()).then(|| venv.to_path_buf())
        });
    let venv_str = env_venv
        .as_ref()
        .filter(|p| p.is_dir())
        .map(|p| p.display().to_string());

    let python = env_venv.as_ref().and_then(|v| {
        let bin = v.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
        if bin.is_file() {
            Some(bin.display().to_string())
        } else if test_step.program.contains("python") {
            Some(test_step.program.clone())
        } else {
            None
        }
    });

    let install_cmd = steps
        .iter()
        .rev()
        .find(|s| s.setup && !s.args.iter().any(|a| a == "venv"))
        .map(Step::line);

    Some(tm_flow::EnvFacts {
        runner,
        test_cmd,
        venv: venv_str,
        python,
        install_cmd,
    })
}

/// Derive `.flow/env.json` facts from a set of verified [`Step`]s and relativize paths against
/// `base` (the project root) so committed `.flow/env.json` files never store absolute machine paths.
pub fn env_facts_from_steps_rel(steps: &[Step], venv: &Path, base: &Path) -> Option<tm_flow::EnvFacts> {
    env_facts_from_steps(steps, venv).map(|facts| facts.relativize(base))
}

/// Prepare the run's test/runtime environment BEFORE wave 1 starts (creating and verifying the
/// shared Python venv if needed, without running the project's tests themselves yet) and return
/// relative [`tm_flow::EnvFacts`] ready to commit into `.flow/env.json`.
pub async fn prepare_env(
    tree: &Path,
    venv: &Path,
    project_root: &Path,
    plan_has_python: bool,
    timeout: Duration,
) -> Option<tm_flow::EnvFacts> {
    let mut all_steps = steps(tree, venv);
    if all_steps.is_empty() && plan_has_python {
        all_steps = python_steps(tree, venv);
    }
    if all_steps.is_empty() {
        return None;
    }

    let is_python_plan = all_steps.iter().any(is_pytest_step);
    let stamp_file = venv.join(".taskmaster-deps");
    // Self-heal a stale `.taskmaster-deps` stamp (e.g. created when host `~/.local/lib` masked a
    // missing venv pytest): clear the stamp and re-generate setup steps so pytest is installed
    // into the shared venv itself.
    if is_python_plan && stamp_file.is_file() && !venv_has_pytest(venv).await {
        let _ = std::fs::remove_file(&stamp_file);
        all_steps = steps(tree, venv);
        if all_steps.is_empty() && plan_has_python {
            all_steps = python_steps(tree, venv);
        }
    }

    let setup_steps: Vec<Step> = all_steps.iter().filter(|s| s.setup).cloned().collect();
    if !setup_steps.is_empty() {
        let mut idx = 0usize;
        while idx < setup_steps.len() {
            let step = &setup_steps[idx];
            // Only skip a bare `pip install -q pytest` when `pytest` is ALREADY importable inside
            // the same sandbox view workers get (e.g. system site-packages under `/usr`, never `~/.local`).
            let is_bare_pytest_install = step.args.iter().any(|a| a == "pip")
                && step.args.iter().any(|a| a == "pytest")
                && !step.args.iter().any(|a| a == "-e" || a == "-r");
            if is_bare_pytest_install && venv_has_pytest(venv).await {
                if let Some((file, stamp)) = &step.stamp {
                    let _ = std::fs::write(file, stamp);
                }
                idx += 1;
                continue;
            }
            let _ = run_steps(tree, std::slice::from_ref(step), timeout).await;
            idx += 1;
        }
        // Stamp only when pytest is genuinely importable inside the sandbox view.
        if venv_has_pytest(venv).await {
            if !stamp_file.is_file() {
                let _ = std::fs::write(&stamp_file, python_stamp(tree));
            }
        } else if is_python_plan {
            let _ = std::fs::remove_file(&stamp_file);
        }
    }

    if is_python_plan && venv.is_dir() && !venv_has_pytest(venv).await {
        let _ = std::fs::remove_file(&stamp_file);
        // Venv exists but pytest is unavailable inside the worker sandbox; record safe fallback without broken venv.
        return Some(tm_flow::EnvFacts {
            runner: "pytest".into(),
            test_cmd: "pytest -q -x --tb=short".into(),
            venv: None,
            python: None,
            install_cmd: None,
        });
    }

    env_facts_from_steps_rel(&all_steps, venv, project_root)
}

/// True when `step` is a non-setup test step that runs `pytest`.
pub(crate) fn is_pytest_step(step: &Step) -> bool {
    !step.setup
        && (step.program.ends_with("pytest")
            || step.program.ends_with("pytest.exe")
            || step.args.iter().any(|a| a == "pytest"))
}

/// Check whether `venv`'s Python can import `pytest` under the same sandbox view workers get:
/// bubblewrap with `--tmpfs $HOME` when usable, plus `-s` and `PYTHONNOUSERSITE=1` everywhere
/// so host `~/.local/lib` user site-packages can never mask a missing venv package.
pub(crate) async fn venv_has_pytest(venv: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    venv_has_pytest_with_home(venv, home.as_deref()).await
}

async fn venv_has_pytest_with_home(venv: &Path, home: Option<&Path>) -> bool {
    let bin = venv.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
    if !bin.is_file() {
        return false;
    }
    let mut cmd = if tm_harness::sandbox::bwrap_usable()
        && let Some(home) = home
    {
        let sandbox = tm_harness::sandbox::Sandbox {
            home: home.to_path_buf(),
            state_dirs: Vec::new(),
        };
        let args = sandbox.bwrap_ro_probe_args(
            &bin,
            &["-s".into(), "-m".into(), "pytest".into(), "--version".into()],
            &[venv.to_path_buf()],
            &[
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
                ("PYTHONNOUSERSITE".into(), "1".into()),
            ],
        );
        let mut c = Command::new("bwrap");
        c.args(args);
        c
    } else {
        let mut c = Command::new(&bin);
        clean_env(&mut c);
        c.args(["-s", "-m", "pytest", "--version"])
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONNOUSERSITE", "1")
            .env_remove("PYTHONUSERBASE");
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(15), cmd.status())
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|s| s.success())
}

/// Exclude later-phase contract test paths from test runner steps (e.g. pytest `--ignore=<path>`)
/// so intermediate-phase verify does not execute tests for phases that have not been built yet.
pub fn deselect_test_paths(steps: &mut [Step], ignore_paths: &[String]) {
    if ignore_paths.is_empty() {
        return;
    }
    for step in steps.iter_mut() {
        if step.name != "test" {
            continue;
        }
        if is_pytest_step(step) {
            for path in ignore_paths {
                let flag = format!("--ignore={path}");
                if !step.args.contains(&flag) {
                    step.args.push(flag);
                }
            }
        }
    }
}

/// Narrow non-setup `test` steps to explicit target test paths/filters (e.g. `tests/test_foo.py`,
/// `tests/test_foo.py::test_bar`, `tests/integration.rs`). When `targets` is empty, steps are
/// left untouched so bare `test()` continues to run the full suite (including acceptance checks).
pub fn filter_test_steps(steps: &mut [Step], targets: &[String]) {
    let mut unique: Vec<&str> = Vec::new();
    for raw in targets {
        let t = raw.trim();
        if !t.is_empty() && !unique.contains(&t) {
            unique.push(t);
        }
    }
    if unique.is_empty() {
        return;
    }
    for step in steps.iter_mut() {
        if step.setup || step.name != "test" {
            continue;
        }
        if is_pytest_step(step) {
            for t in &unique {
                let s = (*t).to_string();
                if !step.args.contains(&s) {
                    step.args.push(s);
                }
            }
        } else if step.program == "cargo" && step.args.first().is_some_and(|a| a == "test") {
            for t in &unique {
                let norm = t.trim_start_matches("./");
                if let Some(rest) = norm.strip_prefix("tests/").and_then(|s| s.strip_suffix(".rs"))
                    && !rest.is_empty()
                    && !rest.contains('/')
                {
                    step.args.push("--test".into());
                    step.args.push(rest.to_string());
                } else {
                    let filter = Path::new(norm)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(norm);
                    step.args.push(filter.to_string());
                }
            }
        } else if matches!(step.program.as_str(), "npm" | "pnpm" | "yarn" | "bun") {
            if step.program == "npm" && !step.args.iter().any(|a| a == "--") {
                step.args.push("--".into());
            }
            for t in &unique {
                step.args.push((*t).to_string());
            }
        }
    }
}

/// Why a project with tests has nothing to run them with, when that is so.
pub fn missing_runner(tree: &Path) -> Option<String> {
    if python_tests(tree) && !on_path("python3") && !on_path("python") {
        return Some("the project has Python tests, but no python3 is on PATH".into());
    }
    if tree.join("Cargo.toml").is_file() && !on_path("cargo") {
        return Some("the project is a Rust crate, but cargo is not on PATH".into());
    }
    let npm_test = std::fs::read_to_string(tree.join("package.json")).is_ok_and(|t| t.contains("\"test\""));
    if npm_test && !tree.join("node_modules").exists() {
        return Some("the project has a test script, but its packages are not installed (no node_modules)".into());
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
                let said = format!("could not start {}: {err}", step.program);
                timings.push(Timing::with_reason(&step.name, began.elapsed(), "failed", said.clone()));
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

/// True when a test step's failure output clearly points to a deterministic assertion, logic,
/// import, syntax, or compile error rather than a transient/flaky failure (such as a timeout,
/// connection reset, port collision, or lock contention). Used to skip wasteful flaky-test reruns.
pub fn is_deterministic_test_failure(output: &str) -> bool {
    if output.trim().is_empty() {
        return false;
    }
    let lower = output.to_ascii_lowercase();
    const TRANSIENT_MARKERS: &[&str] = &[
        "timeout",
        "timed out",
        "deadline exceeded",
        "econnrefused",
        "econnreset",
        "etimedout",
        "ehostunreach",
        "enetunreach",
        "eaddrinuse",
        "eaddrnotavail",
        "epipe",
        "eagain",
        "connection refused",
        "connection reset",
        "connection aborted",
        "connection closed",
        "connectionerror",
        "broken pipe",
        "socket hang up",
        "address already in use",
        "port already in use",
        "temporarily unavailable",
        "service unavailable",
        "too many requests",
        "rate limit",
        "http 429",
        "http 503",
        "502 bad gateway",
        "504 gateway",
        "deadlock",
        "database is locked",
        "sqlite_busy",
        "getaddrinfo",
        "name or service not known",
        "temporary failure in name resolution",
        "segmentation fault",
        "segfault",
        "sigkill",
        "sigterm",
    ];
    if TRANSIENT_MARKERS.iter().any(|m| lower.contains(m)) {
        return false;
    }

    const DETERMINISTIC_MARKERS: &[&str] = &[
        // Python / pytest / JS / Java
        "AssertionError",
        "SyntaxError",
        "IndentationError",
        "TabError",
        "ImportError",
        "ModuleNotFoundError",
        "NameError",
        "TypeError",
        "AttributeError",
        "KeyError",
        "IndexError",
        "ValueError",
        "ZeroDivisionError",
        "UnboundLocalError",
        "NotImplementedError",
        "RecursionError",
        // JS / TS
        "ReferenceError",
        "RangeError",
        "Cannot find module",
        "ERR_MODULE_NOT_FOUND",
        "ERR_ASSERTION",
        // Rust
        "assertion failed",
        "assertion `left == right` failed",
        "assertion left == right failed",
        "panicked at",
        "error[E",
        // Go
        "[build failed]",
        "compilation failed",
    ];
    if DETERMINISTIC_MARKERS.iter().any(|m| output.contains(m)) {
        return true;
    }
    // Pytest short-traceback assertion lines (`E   assert ...`).
    if output.lines().any(|line| {
        line.trim_start()
            .strip_prefix("E ")
            .is_some_and(|rest| rest.trim_start().starts_with("assert "))
    }) {
        return true;
    }
    // Jest / Vitest expectation diff (`Expected:` + `Received:`).
    if output.contains("Expected:") && output.contains("Received:") {
        return true;
    }
    // C / C++ assert macro (`Assertion `...` failed`).
    output.contains("Assertion `") && output.contains("` failed")
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

/// Start the built app and wait until it answers.
async fn start_app(tree: &Path) -> anyhow::Result<(Server, u16)> {
    let port = free_port().await?;
    let mut child = Command::new(tree.join("node_modules/.bin/next"))
        .args(["start", "-p", &port.to_string(), "-H", "127.0.0.1"])
        .current_dir(tree)
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not start the app with `next start`")?;
    let stderr = child.stderr.take();
    let server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok((server, port));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let mut said = String::new();
    if let Some(mut stderr) = stderr {
        let _ = tokio::time::timeout(Duration::from_secs(1), stderr.read_to_string(&mut said)).await;
    }
    bail!("the app did not start in 60 seconds: {}", tail(said.trim()))
}

/// What went wrong on one page, from the events Chrome sent for its session.
pub(crate) fn problems(events: &[Value], session: &str, url: &str, dynamic: bool) -> Vec<String> {
    let mut out = Vec::new();
    for event in events.iter().filter(|e| e["sessionId"] == session) {
        let params = &event["params"];
        match event["method"].as_str().unwrap_or("") {
            "Network.responseReceived" if params["type"] == "Document" => {
                let status = params["response"]["status"].as_u64().unwrap_or(0);
                if status >= 400 && !(status == 404 && dynamic) {
                    out.push(format!("the page answered HTTP {status}"));
                }
            }
            "Network.loadingFailed" if params["type"] == "Document" => {
                out.push(format!("the page did not load: {}", params["errorText"].as_str().unwrap_or("unknown error")));
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
                // itself, and no page asked for one.
                let about_page = source.trim_end_matches('/') == url.trim_end_matches('/');
                if about_page || source.ends_with("/favicon.ico") {
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

/// Open each page in Chrome. `None` when this is neither a built Next.js app nor a static
/// site, or there is no Chrome; the reason is printed.
pub async fn browse(tree: &Path, routes: &[(PageId, String)]) -> anyhow::Result<Option<Browsed>> {
    let next = is_built_next(tree);
    let static_start = if next || !crate::static_site::has_no_scripts(tree) { None } else { crate::static_site::start_page(tree) };
    if !next && static_start.is_none() {
        return Ok(None);
    }
    let Some(chrome) = chrome() else {
        eprintln!("taskmaster: no Chrome or Chromium found, so pages were not opened in a browser (set TASKMASTER_CHROME)");
        return Ok(None);
    };
    let mut browsed = Browsed::default();
    // A static site is served from here; a Next.js app is started.
    let (_server, port, _site) = if static_start.is_some() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let site = tokio::spawn(crate::static_site::serve(listener, tree.to_path_buf()));
        (None, port, Some(AbortOnDrop(site)))
    } else {
        match start_app(tree).await {
            Ok((server, port)) => (Some(server), port, None),
            Err(err) => {
                browsed.notes.push(format!("{err:#}"));
                return Ok(Some(browsed));
            }
        }
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
        if let Ok(shot) = browser.call("Page.captureScreenshot", json!({ "format": "png" }), Some(&session)).await
            && let Some(data) = shot["data"].as_str()
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
        {
            browsed.screenshots.push((page.clone(), bytes));
        }
        let found = problems(&browser.events, &session, &url, dynamic);
        // A page that loaded gets the UI checks; one that failed to has nothing to look at.
        if found.iter().all(|p| !p.starts_with("the page answered") && !p.starts_with("the page did not load")) {
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

    #[test]
    fn env_facts_extracted_from_python_cargo_and_npm_steps() {
        let dir = std::env::temp_dir().join(format!("tm-env-facts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let venv = dir.join("venv");
        let bin = venv.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(venv.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
        let py = bin.join("python");
        std::fs::write(&py, "#!/bin/sh\n").unwrap();

        let py_step = Step {
            name: "test".into(),
            program: py.display().to_string(),
            args: vec!["-m".into(), "pytest".into(), "-q".into()],
            env: vec![("VIRTUAL_ENV".into(), venv.display().to_string())],
            setup: false,
            stamp: None,
        };
        let facts = env_facts_from_steps(&[py_step], &venv).unwrap();
        assert_eq!(facts.runner, "pytest");
        assert!(facts.test_cmd.contains("-m pytest -q"), "{}", facts.test_cmd);
        assert_eq!(facts.venv.as_deref(), Some(venv.to_str().unwrap()));
        assert_eq!(facts.python.as_deref(), Some(py.to_str().unwrap()));

        let cargo_facts = env_facts_from_steps(&[Step::new("test", "cargo", &["test", "--quiet"])], &venv).unwrap();
        assert_eq!(cargo_facts.runner, "cargo");
        assert_eq!(cargo_facts.test_cmd, "cargo test --quiet");
        assert_eq!(cargo_facts.venv, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn venv_has_pytest_rejects_user_site_pytest_hidden_by_sandbox() {
        let has_python3 = std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !has_python3 {
            eprintln!("python3 not installed; skipping");
            return;
        }
        // Discover Python's major.minor for site-packages layout.
        let ver_out = std::process::Command::new("python3")
            .args(["-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"])
            .output()
            .unwrap();
        let py_mm = String::from_utf8_lossy(&ver_out.stdout).trim().to_string();

        let dir = std::env::temp_dir().join(format!("tm-venv-hidden-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let fake_home = dir.join("home");
        let user_site = fake_home.join(format!(".local/lib/python{py_mm}/site-packages/pytest"));
        std::fs::create_dir_all(&user_site).unwrap();
        std::fs::write(user_site.join("__init__.py"), "__version__ = '0.0.0-user'\n").unwrap();
        std::fs::write(user_site.join("__main__.py"), "print('pytest 0.0.0-user')\n").unwrap();

        // Create a venv WITHOUT `--system-site-packages`? Wait: `--system-site-packages` is what
        // `python_steps` uses, and we want to reproduce the exact case where host `~/.local/lib`
        // makes `pytest` importable outside bwrap while hidden inside bwrap (`--tmpfs $HOME`).
        // If the host happens to have system-wide `/usr/lib/python3/dist-packages/pytest`, test a
        // plain venv so system pytest cannot mask the assertion; otherwise use `--system-site-packages`
        // directly to reproduce the exact production path.
        let system_has_pytest = std::process::Command::new("python3")
            .args(["-s", "-m", "pytest", "--version"])
            .env("PYTHONNOUSERSITE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let venv = dir.join("venv");
        let mut mk_args = vec!["-m", "venv"];
        if !system_has_pytest {
            mk_args.push("--system-site-packages");
        }
        let venv_s = venv.display().to_string();
        mk_args.push(&venv_s);
        let mk = std::process::Command::new("python3").args(&mk_args).status().unwrap();
        assert!(mk.success(), "failed to create venv");

        // Reproduce the old bug: outside bwrap without `-s` / `PYTHONNOUSERSITE=1`, user site-packages
        // under `fake_home/.local/lib` makes `venv/bin/python -m pytest --version` appear to succeed
        // (when `--system-site-packages` is active).
        let venv_python = venv.join("bin/python");
        if !system_has_pytest {
            let unisolated = std::process::Command::new(&venv_python)
                .args(["-m", "pytest", "--version"])
                .env("HOME", &fake_home)
                .env("PYTHONUSERBASE", fake_home.join(".local"))
                .env_remove("PYTHONNOUSERSITE")
                .output()
                .unwrap();
            assert!(
                unisolated.status.success(),
                "expected unisolated host probe to see fake_home/.local pytest: stderr={}",
                String::from_utf8_lossy(&unisolated.stderr)
            );
        }

        // Under the sandbox view (`venv_has_pytest_with_home`), `~/.local/lib` is hidden so the
        // empty venv MUST report false — never fooling `prepare_env` into skipping installation.
        assert!(
            !venv_has_pytest_with_home(&venv, Some(&fake_home)).await,
            "venv_has_pytest must reject a venv whose pytest only exists in hidden ~/.local/lib"
        );

        // Also check `python_steps` sets `PYTHONNOUSERSITE=1` and `PIP_USER=0` so `pip install`
        // installs into the venv instead of saying "Requirement already satisfied" in ~/.local.
        let proj = dir.join("proj");
        std::fs::create_dir_all(proj.join("tests")).unwrap();
        std::fs::write(proj.join("tests/test_a.py"), "def test_ok(): pass\n").unwrap();
        let py_steps = python_steps(&proj, &venv);
        let pip_step = py_steps.iter().find(|s| s.setup && s.args.iter().any(|a| a == "pip")).expect("pip setup step");
        assert!(
            pip_step.env.iter().any(|(k, v)| k == "PYTHONNOUSERSITE" && v == "1"),
            "pip setup step must set PYTHONNOUSERSITE=1: {:?}",
            pip_step.env
        );
        assert!(
            pip_step.env.iter().any(|(k, v)| k == "PIP_USER" && v == "0"),
            "pip setup step must set PIP_USER=0: {:?}",
            pip_step.env
        );

        // Now install a self-contained `pytest` directly inside `venv`'s own `site-packages`:
        // `venv_has_pytest_with_home` must now succeed inside the sandbox view!
        let venv_site = venv.join(format!("lib/python{py_mm}/site-packages/pytest"));
        std::fs::create_dir_all(&venv_site).unwrap();
        std::fs::write(venv_site.join("__init__.py"), "__version__ = '0.0.0-venv'\n").unwrap();
        std::fs::write(venv_site.join("__main__.py"), "print('pytest 0.0.0-venv')\n").unwrap();
        assert!(
            venv_has_pytest_with_home(&venv, Some(&fake_home)).await,
            "self-contained venv with pytest in its own site-packages must pass sandbox check"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tok14_default_pytest_args_are_compact_and_tail_caps_output_preserving_traceback() {
        let dir = std::env::temp_dir().join(format!("tm-tok14-pytest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/test_sample.py"), "def test_ok():\n    assert True\n").unwrap();
        let venv = dir.join("venv");

        let py_steps = python_steps(&dir, &venv);
        let test_step = py_steps.iter().find(|s| s.name == "test" && !s.setup).expect("pytest step");
        assert!(
            test_step.args.windows(4).any(|w| w == ["pytest", "-q", "-x", "--tb=short"]),
            "default pytest args should include `-q -x --tb=short`, got: {:?}",
            test_step.args
        );

        // Long output is capped around 4 KB while preserving the trailing traceback/failure summary.
        let noise = "passing line of noisy stdout output from earlier tests\n".repeat(200);
        let tb = "FAILED tests/test_sample.py::test_boom - AssertionError: expected 42\n1 failed in 0.12s\n";
        let combined = format!("{noise}{tb}");
        assert!(combined.len() > KEEP * 2);
        let clipped = tail(&combined);
        assert!(clipped.len() <= KEEP + 8, "clipped len {} exceeded ~4KB cap", clipped.len());
        assert!(clipped.starts_with("…\n"), "expected truncation marker: {clipped}");
        assert!(clipped.ends_with(tb), "must keep trailing traceback: {clipped}");
        assert!(!clipped.contains(&noise), "noisy prefix must be dropped");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_pytest_step_matches_runners_and_ignores_setup_steps() {
        assert!(is_pytest_step(&Step::new("test", "/venv/bin/python", &["-m", "pytest", "-q"])));
        assert!(is_pytest_step(&Step::new("test", "pytest", &["-q"])));
        assert!(is_pytest_step(&Step::new("test", "C:\\venv\\Scripts\\pytest.exe", &["-q"])));
        assert!(is_pytest_step(&Step::new("test", "uv", &["run", "pytest", "-q"])));
        let mut setup_step = Step::new("setup", "/venv/bin/python", &["-m", "pip", "install", "pytest"]);
        setup_step.setup = true;
        assert!(!is_pytest_step(&setup_step));
        assert!(!is_pytest_step(&Step::new("test", "cargo", &["test", "--quiet"])));
    }

    #[test]
    fn fix_n9_deterministic_test_failure_detection() {
        // Real pytest AssertionError output from tb15-mdtodo-tmhcode.
        let pytest_assert = concat!(
            "F\n",
            "=================================== FAILURES ===================================\n",
            "tests/test_acceptance_contract.py:44: in test_storage_format_and_core_commands\n",
            "    assert todo.read_text(encoding=\"utf-8\") == expected_md\n",
            "E   AssertionError: assert '# Todo\\n- [ ' == '# Todo\\n<!-- next-id: 4 -->\\n'\n",
            "FAILED tests/test_acceptance_contract.py::test_storage_format_and_core_commands - AssertionError\n",
        );
        assert!(is_deterministic_test_failure(pytest_assert));
        assert!(is_deterministic_test_failure("E   assert 1 == 2\n1 failed in 0.05s"));
        assert!(is_deterministic_test_failure("ModuleNotFoundError: No module named 'mdtodo'"));
        assert!(is_deterministic_test_failure("SyntaxError: invalid syntax"));
        assert!(is_deterministic_test_failure("thread 'main' panicked at 'assertion `left == right` failed'"));
        assert!(is_deterministic_test_failure("ReferenceError: foo is not defined"));
        assert!(is_deterministic_test_failure("Expected: 42\nReceived: 41"));

        // Transient / flaky failures must still return false so the flaky rerun stays active.
        assert!(!is_deterministic_test_failure("timeout waiting for port 3000"));
        assert!(!is_deterministic_test_failure("Error: connect ECONNREFUSED 127.0.0.1:8080"));
        assert!(!is_deterministic_test_failure("OSError: [Errno 98] Address already in use"));
        assert!(!is_deterministic_test_failure("AssertionError: timed out waiting for server"));
        assert!(!is_deterministic_test_failure(""));
        assert!(!is_deterministic_test_failure("exit status 1"));
    }

    #[test]
    fn tb18_filter_test_steps_targets_pytest_npm_and_cargo_and_preserves_bare_full_suite() {
        let mut py = vec![
            Step { setup: true, ..Step::new("setup", "python3", &["-m", "venv", "venv"]) },
            Step::new("test", "/venv/bin/python", &["-m", "pytest", "-q", "-x", "--tb=short"]),
        ];
        // Empty targets must keep bare full suite untouched.
        filter_test_steps(&mut py, &[]);
        assert_eq!(py[1].args, vec!["-m", "pytest", "-q", "-x", "--tb=short"]);

        // Explicit pytest targets are appended and deduplicated; setup steps are untouched.
        filter_test_steps(
            &mut py,
            &["tests/test_parser.py".into(), "tests/test_parser.py".into(), "tests/test_cli.py::test_help".into()],
        );
        assert_eq!(py[0].args, vec!["-m", "venv", "venv"]);
        assert_eq!(
            py[1].args,
            vec!["-m", "pytest", "-q", "-x", "--tb=short", "tests/test_parser.py", "tests/test_cli.py::test_help"]
        );

        // npm receives `--` separator before target paths.
        let mut npm = vec![Step::new("test", "npm", &["run", "test"])];
        filter_test_steps(&mut npm, &["tests/alpha.test.ts".into()]);
        assert_eq!(npm[0].args, vec!["run", "test", "--", "tests/alpha.test.ts"]);

        // cargo maps `tests/<stem>.rs` to `--test <stem>` and other paths to stem filters.
        let mut cargo = vec![Step::new("test", "cargo", &["test", "--quiet"])];
        filter_test_steps(&mut cargo, &["tests/integration.rs".into(), "src/parser.rs".into()]);
        assert_eq!(cargo[0].args, vec!["test", "--quiet", "--test", "integration", "parser"]);
    }

    #[test]
    fn tb18_pyproject_cosmetic_change_preserves_stamp_and_existing_venv_not_recreated() {
        let dir = std::env::temp_dir().join(format!("tm-tb18-venv-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/test_calc.py"), "def test_ok(): assert True\n").unwrap();
        let venv = dir.join("venv");
        let bin = venv.join(if cfg!(windows) { "Scripts" } else { "bin" });
        std::fs::create_dir_all(&bin).unwrap();
        let py_bin = bin.join(if cfg!(windows) { "python.exe" } else { "python" });
        std::fs::write(&py_bin, "#!/bin/sh\n").unwrap();

        let stamp_before = python_stamp(&dir);
        std::fs::write(venv.join(".taskmaster-deps"), &stamp_before).unwrap();

        // 1. Adding a stdlib-only `pyproject.toml` with `[tool.pytest.ini_options]` and empty
        //    `dependencies = []` keeps the EXACT same stamp and emits zero setup steps!
        std::fs::write(
            dir.join("pyproject.toml"),
            "[build-system]\nrequires = [\"setuptools\"]\n\n[project]\nname = \"calcx\"\nversion = \"0.1.0\"\ndependencies = []\n\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n",
        )
        .unwrap();
        assert_eq!(python_stamp(&dir), stamp_before, "cosmetic/stdlib pyproject.toml must keep stamp stable");
        assert!(!project_has_external_deps(&dir));
        let steps_stdlib = python_steps(&dir, &venv);
        assert!(steps_stdlib.iter().all(|s| !s.setup), "ready venv must need zero setup steps: {steps_stdlib:?}");

        // 2. Adding `[project.scripts]` changes the stamp, does NOT emit `python -m venv` over
        //    existing `bin/python`, and installs offline-safely with `--no-build-isolation --no-deps -e .`.
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"calcx\"\nversion = \"0.1.0\"\ndependencies = []\n\n[project.scripts]\ncalcx = \"calcx.cli:main\"\n",
        )
        .unwrap();
        assert_ne!(python_stamp(&dir), stamp_before);
        let steps_scripts = python_steps(&dir, &venv);
        assert!(
            !steps_scripts.iter().any(|s| s.args.iter().any(|a| a == "venv")),
            "existing bin/python must never trigger `python -m venv` again: {steps_scripts:?}"
        );
        let pip_step = steps_scripts.iter().find(|s| s.setup && s.args.iter().any(|a| a == "pip")).expect("pip setup step");
        assert!(pip_step.args.iter().any(|a| a == "--no-build-isolation"), "{pip_step:?}");
        assert!(pip_step.args.iter().any(|a| a == "--no-deps"), "{pip_step:?}");
        assert!(pip_step.args.iter().any(|a| a == "-e"), "{pip_step:?}");

        // 3. Adding a real third-party dependency changes the stamp, reports external deps, and
        //    does NOT recreate `bin/python`.
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"calcx\"\ndependencies = [\n  \"requests>=2.31\",\n]\n",
        )
        .unwrap();
        assert!(project_has_external_deps(&dir));
        let steps_ext = python_steps(&dir, &venv);
        assert!(
            !steps_ext.iter().any(|s| s.args.iter().any(|a| a == "venv")),
            "existing bin/python must not be recreated when adding third-party deps: {steps_ext:?}"
        );
        let pip_ext = steps_ext.iter().find(|s| s.setup && s.args.iter().any(|a| a == "pip")).expect("pip setup step");
        assert!(pip_ext.args.iter().any(|a| a == "-e"), "{pip_ext:?}");
        assert!(!pip_ext.args.iter().any(|a| a == "--no-deps"), "{pip_ext:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tb18_stdlib_python_package_imports_from_other_cwd_via_absolute_pythonpath() {
        let Some(py_mm) = std::process::Command::new("python3")
            .args(["-c", "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        else {
            return;
        };

        let dir = std::env::temp_dir().join(format!("tm-tb18-abs-pypath-{}", std::process::id()));
        let other_cwd = std::env::temp_dir().join(format!("tm-tb18-abs-pypath-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_cwd);
        std::fs::create_dir_all(dir.join("mdtodo")).unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::create_dir_all(&other_cwd).unwrap();

        // Stdlib-only package with NO `[project.scripts]` (the exact n10bm-mdtodo-tmhcode layout).
        std::fs::write(
            dir.join("pyproject.toml"),
            "[build-system]\nrequires = [\"setuptools\"]\n\n[project]\nname = \"mdtodo\"\nversion = \"0.1.0\"\ndependencies = []\n",
        )
        .unwrap();
        std::fs::write(dir.join("mdtodo/__init__.py"), "VERSION = '0.1.0'\n").unwrap();
        std::fs::write(dir.join("mdtodo/__main__.py"), "print('mdtodo-ok')\n").unwrap();

        // Acceptance-style test that launches `python -m mdtodo` from ANOTHER cwd (`other_cwd`).
        let test_script = format!(
            "import subprocess, sys\n\
def test_cli_from_other_cwd():\n\
\x20   res = subprocess.run([sys.executable, '-m', 'mdtodo'], cwd={:?}, capture_output=True, text=True)\n\
\x20   assert res.returncode == 0, res.stderr\n\
\x20   assert 'mdtodo-ok' in res.stdout, res.stdout\n\
if __name__ == '__main__':\n\
\x20   test_cli_from_other_cwd()\n",
            other_cwd.display().to_string()
        );
        std::fs::write(dir.join("tests/test_cli.py"), &test_script).unwrap();

        // 1. Prove the test FAILS on the old relative `PYTHONPATH="."` code path.
        let old_broken = std::process::Command::new("python3")
            .arg(dir.join("tests/test_cli.py"))
            .current_dir(&dir)
            .env("PYTHONPATH", ".")
            .output()
            .unwrap();
        assert!(!old_broken.status.success(), "relative PYTHONPATH='.' must fail from other_cwd");
        let old_stderr = String::from_utf8_lossy(&old_broken.stderr);
        assert!(old_stderr.contains("No module named mdtodo"), "expected No module named mdtodo, got: {old_stderr}");

        // 2. Build a real local venv with a lightweight offline pytest runner shim so `run_steps`
        //    executes the exact generated `Step` (including `clean_env` + `step.env`).
        let venv = dir.join(".venv-test");
        let mk = std::process::Command::new("python3")
            .args(["-m", "venv", &venv.display().to_string()])
            .status()
            .unwrap();
        if !mk.success() {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_dir_all(&other_cwd);
            return;
        }
        let pytest_pkg = venv.join(format!("lib/python{py_mm}/site-packages/pytest"));
        std::fs::create_dir_all(&pytest_pkg).unwrap();
        std::fs::write(pytest_pkg.join("__init__.py"), "").unwrap();
        std::fs::write(
            pytest_pkg.join("__main__.py"),
            "import runpy, sys\nrunpy.run_path('tests/test_cli.py', run_name='__main__')\n",
        )
        .unwrap();
        std::fs::write(venv.join(".taskmaster-deps"), python_stamp(&dir)).unwrap();

        let py_steps = python_steps(&dir, &venv);
        assert!(py_steps.iter().all(|s| !s.setup), "ready venv must need zero setup steps: {py_steps:?}");
        let test_step = py_steps.iter().find(|s| s.name == "test").expect("test step");
        let pypath = test_step
            .env
            .iter()
            .find(|(k, _)| k == "PYTHONPATH")
            .map(|(_, v)| v.as_str())
            .expect("PYTHONPATH must be set");
        assert_ne!(pypath, ".", "PYTHONPATH must be absolute, never relative '.'");
        assert!(Path::new(pypath).is_absolute(), "expected absolute PYTHONPATH, got: {pypath}");
        assert_eq!(pypath, pythonpath_for_tree(&dir, None));

        // 3. Running the generated steps via `run_steps` passes the other-cwd `python -m mdtodo` test!
        let (_, failure) = run_steps(&dir, &py_steps, Duration::from_secs(20)).await;
        assert!(failure.is_none(), "expected run_steps to pass with absolute PYTHONPATH, got: {failure:?}");

        // 4. `pythonpath_for_tree` prepends `src/` for src-layout repos and preserves existing entries.
        let src_repo = dir.join("src-layout-check");
        std::fs::create_dir_all(src_repo.join("src/pkg")).unwrap();
        let combined = pythonpath_for_tree(&src_repo, Some("/opt/extra"));
        let parts: Vec<PathBuf> = std::env::split_paths(&combined).collect();
        assert_eq!(parts, vec![src_repo.join("src"), src_repo.clone(), PathBuf::from("/opt/extra")]);

        // 5. Contract mutation sanity rewrites `PYTHONPATH` from original tree to the isolated temp copy,
        //    so stubbing `mdtodo/__main__.py` is Caught rather than masked by the unmutated original tree.
        let (_, mut_env) = crate::gates::mutation_test_command(&dir, &venv, "tests/test_cli.py")
            .expect("mutation command");
        let cmd_owned = [
            venv.join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" })
                .display()
                .to_string(),
            "tests/test_cli.py".to_string(),
        ];
        let cmd_refs: Vec<&str> = cmd_owned.iter().map(String::as_str).collect();
        let verdict = crate::gates::contract_mutation_sanity_with_env(
            &dir,
            "mdtodo/__main__.py",
            "raise SystemExit(1)\n",
            &cmd_refs,
            &mut_env,
        )
        .unwrap();
        assert_eq!(verdict, crate::gates::MutationVerdict::Caught);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_cwd);
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
}
