//! Mechanical lints for a git review. None of these spend tokens.
//!
//! Clippy is the connection check for Rust: it compiles the crates the change
//! touches, so a break in a file that depends on the change shows up even when
//! that file itself did not change. Warnings stay limited to the diff. `tsc --noEmit`
//! does the same job for TypeScript when the checkout has TypeScript installed.
//!
//! `--install-lints` installs toolchain linters only (clippy, through rustup).
//! A project linter such as eslint or ruff is run when the repository already
//! has it, and never installed, because installing one would change the project.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::Value;

use crate::findings::{Finding, Kind};

const LINT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The linter is installed; run it.
    Run,
    /// Missing, and the user asked to install toolchain linters.
    InstallThenRun,
    /// Do not run. `note` is what to tell the user.
    Skip { note: String },
}

/// What to do about clippy for this diff.
pub fn clippy_step(rust_changed: bool, clippy_installed: bool, rustup_installed: bool, install: bool) -> Step {
    if !rust_changed {
        return Step::Skip { note: String::new() };
    }
    if clippy_installed {
        return Step::Run;
    }
    if install && rustup_installed {
        return Step::InstallThenRun;
    }
    if install {
        return Step::Skip {
            note: "Rust files changed, but clippy is not installed and rustup is not available, \
                   so it cannot be installed from here."
                .into(),
        };
    }
    Step::Skip {
        note: "Rust files changed, but clippy is not installed. Re-run with --install-lints to \
               install it (toolchain only; the repository is not modified)."
            .into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Diag {
    file: String,
    line: u32,
    /// `error` breaks a connection and is kept even outside the diff. `warning` is kept
    /// only in a changed file.
    level: String,
    message: String,
}

/// Cargo manifests that own the changed Rust files, nearest first.
pub fn manifests_for(root: &Path, changed: &[String]) -> Vec<PathBuf> {
    let mut found = BTreeSet::new();
    for path in changed {
        if !is_rust_path(path) {
            continue;
        }
        if let Some(manifest) = nearest_manifest(root, Path::new(path)) {
            found.insert(manifest);
        }
    }
    found.into_iter().collect()
}

fn is_rust_path(path: &str) -> bool {
    path.ends_with(".rs") || path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock")
}

fn nearest_manifest(root: &Path, rel: &Path) -> Option<PathBuf> {
    let start = root.join(rel);
    let mut dir = if start.is_dir() { start } else { start.parent()?.to_path_buf() };
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir == root {
            return None;
        }
        dir = dir.parent()?.to_path_buf();
    }
}

/// Read `cargo clippy --message-format=json`. `package` is the directory of the manifest
/// that was checked, used when a path is relative to the package rather than the repo.
pub fn clippy_diagnostics(jsonl: &str, worktree: &Path, package: &Path, changed: &BTreeSet<String>) -> Vec<Finding> {
    clippy_diags(jsonl, worktree, package, changed, false)
}

fn clippy_diags(
    jsonl: &str,
    worktree: &Path,
    package: &Path,
    changed: &BTreeSet<String>,
    errors_only: bool,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let Some(diag) = parse_clippy_line(line, worktree, package) else { continue };
        if !keep_diag(&diag, changed) || (errors_only && diag.level != "error") {
            continue;
        }
        out.push(to_finding(&diag, changed));
    }
    out
}

fn parse_clippy_line(line: &str, worktree: &Path, package: &Path) -> Option<Diag> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value["reason"] != "compiler-message" {
        return None;
    }
    let message = &value["message"];
    let level = message["level"].as_str()?.to_string();
    if level != "error" && level != "warning" {
        return None;
    }
    let spans = message["spans"].as_array()?;
    let span = spans.iter().find(|s| s["is_primary"] == true).or_else(|| spans.first())?;
    let file_name = span["file_name"].as_str()?;
    let file = locate(worktree, package, file_name)?;
    let line_no = span["line_start"].as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0)?;
    let code = message["code"]["code"].as_str().unwrap_or("clippy");
    let text = one_line(message["message"].as_str().unwrap_or("lint finding"));
    Some(Diag { file, line: line_no, level, message: format!("{code}: {text}") })
}

fn keep_diag(diag: &Diag, changed: &BTreeSet<String>) -> bool {
    diag.level == "error" || changed.contains(&diag.file)
}

fn to_finding(diag: &Diag, changed: &BTreeSet<String>) -> Finding {
    let failure = if changed.contains(&diag.file) {
        format!("clippy: {}", diag.message)
    } else {
        format!(
            "clippy error outside the diff, in a file this change can break: {}",
            diag.message
        )
    };
    Finding { kind: Kind::Lint, file: diag.file.clone(), line: diag.line, failure }
}

/// ESLint's JSON formatter, limited to the files it was asked to check.
pub fn eslint_diagnostics(json: &str, worktree: &Path) -> Vec<Finding> {
    eslint_diags(json, worktree, false)
}

fn eslint_diags(json: &str, worktree: &Path, errors_only: bool) -> Vec<Finding> {
    let Ok(value) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let Some(files) = value.as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for file in files {
        let Some(path) = file["filePath"].as_str() else { continue };
        let Some(rel) = locate(worktree, worktree, path) else { continue };
        let Some(messages) = file["messages"].as_array() else { continue };
        for message in messages {
            if errors_only && message["severity"].as_u64() != Some(2) {
                continue;
            }
            let Some(line) = message["line"].as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0) else {
                continue;
            };
            let rule = message["ruleId"].as_str().unwrap_or("eslint");
            let text = one_line(message["message"].as_str().unwrap_or("lint finding"));
            out.push(Finding {
                kind: Kind::Lint,
                file: rel.clone(),
                line,
                failure: format!("eslint: {rule}: {text}"),
            });
        }
    }
    out
}

/// `ruff check --output-format json`.
pub fn ruff_diagnostics(json: &str, worktree: &Path) -> Vec<Finding> {
    ruff_diags(json, worktree, false)
}

/// Ruff codes that mean the file is broken, not just untidy: syntax errors, undefined
/// names, and misplaced statements.
fn ruff_breaks(code: Option<&str>) -> bool {
    match code {
        None => true,
        Some(code) => ["E9", "F82", "F63", "F7"].iter().any(|prefix| code.starts_with(prefix)),
    }
}

fn ruff_diags(json: &str, worktree: &Path, errors_only: bool) -> Vec<Finding> {
    let Ok(value) = serde_json::from_str::<Value>(json) else { return Vec::new() };
    let Some(items) = value.as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for item in items {
        if errors_only && !ruff_breaks(item["code"].as_str()) {
            continue;
        }
        let Some(path) = item["filename"].as_str() else { continue };
        let Some(rel) = locate(worktree, worktree, path) else { continue };
        let Some(line) = item["location"]["row"].as_u64().and_then(|n| u32::try_from(n).ok()).filter(|n| *n > 0)
        else {
            continue;
        };
        let code = item["code"].as_str().unwrap_or("ruff");
        let text = one_line(item["message"].as_str().unwrap_or("lint finding"));
        out.push(Finding { kind: Kind::Lint, file: rel, line, failure: format!("ruff: {code}: {text}") });
    }
    out
}

/// `tsc --noEmit --pretty false`. Each error line is `path(line,col): error TS2322: text`.
/// Every error is kept: a type error in an unchanged file is a connection the change broke.
pub fn tsc_diagnostics(text: &str, worktree: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((place, rest)) = line.split_once("): error ") else { continue };
        let Some((path, position)) = place.rsplit_once('(') else { continue };
        let Some(row) = position.split(',').next().and_then(|n| n.trim().parse::<u32>().ok()).filter(|n| *n > 0)
        else {
            continue;
        };
        let Some(rel) = locate(worktree, worktree, path.trim()) else { continue };
        out.push(Finding { kind: Kind::Lint, file: rel, line: row, failure: format!("tsc: {}", one_line(rest)) });
    }
    out
}

fn locate(worktree: &Path, package: &Path, file_name: &str) -> Option<String> {
    let raw = Path::new(file_name);
    let candidates = if raw.is_absolute() {
        vec![raw.to_path_buf()]
    } else {
        vec![worktree.join(raw), package.join(raw)]
    };
    for candidate in candidates {
        if let Ok(rel) = candidate.strip_prefix(worktree) {
            if !rel.as_os_str().is_empty() && !rel.components().any(|c| c.as_os_str() == "..") {
                return Some(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    None
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let clipped: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() { format!("{clipped}…") } else { clipped }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub notes: Vec<String>,
}

impl Report {
    pub fn markdown(&self) -> String {
        let mut out = String::from(
            "# Mechanical lint\n\nAlready checked, at no token cost. Do not repeat these findings.\n\n",
        );
        if self.findings.is_empty() {
            out.push_str("No lint findings.\n");
        }
        for finding in &self.findings {
            out.push_str(&format!("- `{}:{}` {}\n", finding.file, finding.line, finding.failure));
        }
        if !self.notes.is_empty() {
            out.push_str("\n");
            for note in &self.notes {
                out.push_str(&format!("- {note}\n"));
            }
        }
        out
    }
}

/// Run the linters this diff calls for. `install` adds clippy when rustup can, and
/// nothing else.
pub fn run(worktree: &Path, changed: &[String], install: bool) -> Report {
    run_with(worktree, changed, install, false)
}

/// Errors only: type errors, compile errors, lint rules the project set to `error`, and
/// Python that cannot run. This is what a build sends back to a worker, so style warnings
/// stay out. Nothing is installed.
pub fn run_errors(worktree: &Path, changed: &[String]) -> Report {
    run_with(worktree, changed, false, true)
}

fn run_with(worktree: &Path, changed: &[String], install: bool, errors_only: bool) -> Report {
    let mut findings = Vec::new();
    let mut notes = Vec::new();
    let changed_set: BTreeSet<String> = changed.iter().cloned().collect();

    let rust = changed.iter().any(|p| is_rust_path(p));
    // The toolchain is only asked when the diff has Rust in it.
    let has_clippy = rust && command_ok("cargo", &["clippy", "--version"]);
    let has_rustup = rust && !has_clippy && install && command_ok("rustup", &["--version"]);
    match clippy_step(rust, has_clippy, has_rustup, install) {
        Step::Skip { note } => {
            if !note.is_empty() {
                notes.push(note);
            }
        }
        Step::InstallThenRun => match install_clippy() {
            Ok(()) => findings.extend(run_clippy(worktree, changed, &changed_set, errors_only, &mut notes)),
            Err(e) => notes.push(format!("could not install clippy: {e}")),
        },
        Step::Run => findings.extend(run_clippy(worktree, changed, &changed_set, errors_only, &mut notes)),
    }

    notes.extend(project_lints(worktree, changed, errors_only, &mut findings));
    findings.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    findings.dedup();
    Report { findings, notes }
}

fn run_clippy(
    worktree: &Path,
    changed: &[String],
    changed_set: &BTreeSet<String>,
    errors_only: bool,
    notes: &mut Vec<String>,
) -> Vec<Finding> {
    let manifests = manifests_for(worktree, changed);
    if manifests.is_empty() {
        notes.push(
            "Rust files changed, but no Cargo.toml owns them, so clippy was not run.".into(),
        );
        return Vec::new();
    }
    let mut found = Vec::new();
    for manifest in &manifests {
        let package = manifest.parent().unwrap_or(worktree);
        match cargo_clippy(worktree, manifest) {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let diags = clippy_diags(&stdout, worktree, package, changed_set, errors_only);
                if diags.is_empty() && !output.status.success() {
                    let err_text = String::from_utf8_lossy(&output.stderr);
                    let offline = if err_text.contains("offline") {
                        " Clippy does not download dependencies. Build the crate once in the repository, then review again."
                    } else {
                        ""
                    };
                    notes.push(format!(
                        "clippy failed for {}{}{offline}",
                        manifest.strip_prefix(worktree).unwrap_or(manifest).display(),
                        tail(&err_text)
                    ));
                } else {
                    notes.push(format!(
                        "clippy checked {} ({} finding(s); errors outside the diff are kept because they are connections the change can break)",
                        manifest.strip_prefix(worktree).unwrap_or(manifest).display(),
                        diags.len()
                    ));
                    found.extend(diags);
                }
            }
            Err(e) => notes.push(format!("clippy did not finish for {}: {e}", manifest.display())),
        }
    }
    found
}

fn cargo_clippy(worktree: &Path, manifest: &Path) -> anyhow::Result<Output> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(worktree)
        .args(["clippy", "--manifest-path"])
        .arg(manifest)
        .args(["--message-format=json", "--no-deps", "--offline"])
        .env("CARGO_TERM_COLOR", "never");
    let lock = manifest.parent().is_some_and(|dir| dir.join("Cargo.lock").is_file())
        || worktree.join("Cargo.lock").is_file();
    if lock {
        cmd.arg("--locked");
    }
    cmd.arg("--").args(["-W", "clippy::all"]);
    // Keep the build out of the checkout so `--keep` does not leave a target directory behind.
    let target = std::env::temp_dir().join(format!("tm-clippy-{}", std::process::id()));
    cmd.env("CARGO_TARGET_DIR", &target);
    output_with_timeout(&mut cmd, LINT_TIMEOUT).context("failed to start cargo clippy")
}

fn install_clippy() -> anyhow::Result<()> {
    let mut cmd = Command::new("rustup");
    cmd.args(["component", "add", "clippy"]);
    let output = output_with_timeout(&mut cmd, LINT_TIMEOUT).context("failed to start rustup")?;
    if output.status.success() {
        return Ok(());
    }
    anyhow::bail!("{}", tail(&String::from_utf8_lossy(&output.stderr)))
}

/// Project linters. They run only when the repo already configured them.
fn project_lints(worktree: &Path, changed: &[String], errors_only: bool, findings: &mut Vec<Finding>) -> Vec<String> {
    let mut notes = Vec::new();
    let js: Vec<&str> = changed
        .iter()
        .filter(|p| matches_ext(p, &["js", "jsx", "ts", "tsx", "mjs", "cjs"]))
        .map(String::as_str)
        .collect();
    let typed = changed.iter().any(|p| matches_ext(p, &["ts", "tsx", "mts", "cts"]));
    if typed && worktree.join("tsconfig.json").is_file() {
        match run_tsc(worktree) {
            Some(text) => {
                let found = tsc_diagnostics(&text, worktree);
                notes.push(format!("tsc checked the project ({} type error(s))", found.len()));
                findings.extend(found);
            }
            None => notes.push(
                "TypeScript files changed, but `node_modules/.bin/tsc` is not in this checkout, so \
                 types were not checked. Taskmaster does not install dependencies."
                    .into(),
            ),
        }
    }
    if !js.is_empty() {
        if eslint_configured(worktree) {
            match run_eslint(worktree, &js) {
                Some(json) => {
                    let found = eslint_diags(&json, worktree, errors_only);
                    notes.push(format!("eslint checked {} file(s) ({} finding(s))", js.len(), found.len()));
                    findings.extend(found);
                }
                None => notes.push(
                    "eslint is configured in this repository but is not installed. Taskmaster does \
                     not install it, because that would change the project."
                        .into(),
                ),
            }
        } else {
            notes.push(
                "JavaScript or TypeScript files changed, and the repository has no eslint config. \
                 Taskmaster does not add one."
                    .into(),
            );
        }
    }
    let py: Vec<&str> = changed.iter().filter(|p| p.ends_with(".py")).map(String::as_str).collect();
    if !py.is_empty() {
        if ruff_configured(worktree) {
            match run_ruff(worktree, &py) {
                Some(json) => {
                    let found = ruff_diags(&json, worktree, errors_only);
                    notes.push(format!("ruff checked {} file(s) ({} finding(s))", py.len(), found.len()));
                    findings.extend(found);
                }
                None => notes.push(
                    "ruff is configured in this repository but is not installed. Taskmaster does \
                     not install it, because that would change the project."
                        .into(),
                ),
            }
        } else if command_ok("ruff", &["--version"]) {
            match run_ruff(worktree, &py) {
                Some(json) => {
                    let found = ruff_diags(&json, worktree, errors_only);
                    notes.push(format!("ruff checked {} file(s) ({} finding(s))", py.len(), found.len()));
                    findings.extend(found);
                }
                None => notes.push("ruff is installed but did not finish.".into()),
            }
        } else {
            notes.push(
                "Python files changed. ruff is not installed; Taskmaster does not install project \
                 linters. Install ruff yourself if you want it on later reviews."
                    .into(),
            );
        }
    }
    notes
}

fn matches_ext(path: &str, exts: &[&str]) -> bool {
    exts.iter().any(|ext| path.ends_with(&format!(".{ext}")))
}

fn eslint_configured(worktree: &Path) -> bool {
    const NAMES: [&str; 6] =
        ["eslint.config.js", "eslint.config.mjs", "eslint.config.cjs", ".eslintrc", ".eslintrc.js", ".eslintrc.json"];
    if NAMES.iter().any(|name| worktree.join(name).is_file()) {
        return true;
    }
    worktree.join(".eslintrc.yml").is_file()
        || std::fs::read_to_string(worktree.join("package.json")).is_ok_and(|t| t.contains("\"eslint\""))
}

fn ruff_configured(worktree: &Path) -> bool {
    worktree.join("ruff.toml").is_file()
        || worktree.join(".ruff.toml").is_file()
        || std::fs::read_to_string(worktree.join("pyproject.toml")).is_ok_and(|t| t.contains("[tool.ruff]"))
}

/// `None` when the linter cannot be started.
fn run_eslint(worktree: &Path, files: &[&str]) -> Option<String> {
    let local = worktree.join("node_modules/.bin/eslint");
    let mut cmd = if local.is_file() {
        let mut cmd = Command::new(local);
        cmd.current_dir(worktree);
        cmd
    } else if command_ok("eslint", &["--version"]) {
        let mut cmd = Command::new("eslint");
        cmd.current_dir(worktree);
        cmd
    } else {
        return None;
    };
    cmd.args(["-f", "json", "--no-error-on-unmatched-pattern"]);
    cmd.args(files);
    let output = output_with_timeout(&mut cmd, LINT_TIMEOUT).ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `None` when the checkout has no TypeScript of its own. A global `tsc` is not used,
/// because its version decides what counts as an error.
fn run_tsc(worktree: &Path) -> Option<String> {
    let local = worktree.join("node_modules/.bin/tsc");
    if !local.is_file() {
        return None;
    }
    let mut cmd = Command::new(local);
    cmd.current_dir(worktree).args(["--noEmit", "--pretty", "false"]);
    let output = output_with_timeout(&mut cmd, LINT_TIMEOUT).ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_ruff(worktree: &Path, files: &[&str]) -> Option<String> {
    if !command_ok("ruff", &["--version"]) {
        return None;
    }
    let mut cmd = Command::new("ruff");
    cmd.current_dir(worktree).args(["check", "--output-format", "json"]);
    cmd.args(files);
    let output = output_with_timeout(&mut cmd, LINT_TIMEOUT).ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut stdout = child.stdout.take().context("stdout was not captured")?;
    let mut stderr = child.stderr.take().context("stderr was not captured")?;
    let out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).map(|_| buf)
    });
    let err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stderr.read_to_end(&mut buf).map(|_| buf)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out.join().unwrap_or(Ok(Vec::new())).unwrap_or_default();
    let stderr = err.join().unwrap_or(Ok(Vec::new())).unwrap_or_default();
    Ok(Output { status, stdout, stderr })
}

fn tail(text: &str) -> String {
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
    format!(": {}", one_line(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tm-lint-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clippy_is_installed_only_when_asked_and_only_for_rust() {
        assert_eq!(clippy_step(false, false, true, true), Step::Skip { note: String::new() });
        assert_eq!(clippy_step(true, true, false, false), Step::Run);
        assert_eq!(clippy_step(true, false, true, true), Step::InstallThenRun);
        let skipped = clippy_step(true, false, true, false);
        assert!(matches!(skipped, Step::Skip { note } if note.contains("--install-lints")));
        let no_rustup = clippy_step(true, false, false, true);
        assert!(matches!(no_rustup, Step::Skip { note } if note.contains("rustup")));
    }

    #[test]
    fn the_nearest_manifest_owns_the_changed_file() {
        let root = scratch("manifests");
        std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
        std::fs::create_dir_all(root.join("crates/a/src")).unwrap();
        std::fs::write(root.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\nversion = \"0.1.0\"\n").unwrap();
        std::fs::write(root.join("crates/a/src/lib.rs"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();

        let manifests = manifests_for(
            &root,
            &["crates/a/src/lib.rs".into(), "README.md".into(), "src/main.rs".into()],
        );
        assert_eq!(
            manifests,
            vec![root.join("Cargo.toml"), root.join("crates/a/Cargo.toml")]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clippy_keeps_errors_outside_the_diff_and_drops_unrelated_warnings() {
        let root = scratch("clippy-parse");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn changed() {}\n").unwrap();
        std::fs::write(root.join("src/caller.rs"), "fn caller() {}\n").unwrap();
        std::fs::write(root.join("src/old.rs"), "fn old() {}\n").unwrap();
        let jsonl = r#"
{"reason":"compiler-message","message":{"level":"warning","message":"this function has too many arguments","code":{"code":"clippy::too_many_arguments"},"spans":[{"file_name":"src/lib.rs","line_start":1,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/caller.rs","line_start":1,"is_primary":true}]}}
{"reason":"compiler-message","message":{"level":"warning","message":"unused import: foo","code":{"code":"unused_imports"},"spans":[{"file_name":"src/old.rs","line_start":1,"is_primary":true}]}}
{"reason":"build-finished","success":false}
"#;
        let changed = BTreeSet::from(["src/lib.rs".to_string()]);
        let found = clippy_diagnostics(jsonl, &root, &root, &changed);
        let files: Vec<&str> = found.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(files, vec!["src/lib.rs", "src/caller.rs"]);
        assert!(found[1].failure.contains("outside the diff"));
        assert!(found.iter().all(|f| f.kind == Kind::Lint && f.is_actionable()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_build_keeps_errors_and_drops_style_warnings() {
        let root = scratch("errors");
        std::fs::write(root.join("a.js"), "x").unwrap();
        std::fs::write(root.join("a.py"), "x").unwrap();
        let eslint = format!(
            r#"[{{"filePath":"{}","messages":[
                {{"ruleId":"no-undef","severity":2,"message":"'x' is not defined.","line":3}},
                {{"ruleId":"semi","severity":1,"message":"Missing semicolon.","line":4}}]}}]"#,
            root.join("a.js").display()
        );
        let kept = eslint_diags(&eslint, &root, true);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(kept[0].failure.contains("no-undef"));
        assert_eq!(eslint_diags(&eslint, &root, false).len(), 2);
        let ruff = format!(
            r#"[{{"filename":"{0}","code":"F821","message":"Undefined name `y`","location":{{"row":2}}}},
                {{"filename":"{0}","code":"F401","message":"`os` imported but unused","location":{{"row":1}}}}]"#,
            root.join("a.py").display()
        );
        let kept = ruff_diags(&ruff, &root, true);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert!(kept[0].failure.contains("F821"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tsc_errors_keep_their_file_and_line() {
        let root = scratch("tsc");
        std::fs::create_dir_all(root.join("app/a")).unwrap();
        std::fs::write(root.join("app/a/page.tsx"), "x").unwrap();
        let text = "app/a/page.tsx(12,5): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                    Found 1 error.\n\
                    ../outside.ts(1,1): error TS1005: ';' expected.\n";
        let found = tsc_diagnostics(text, &root);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].file.as_str(), found[0].line), ("app/a/page.tsx", 12));
        assert!(found[0].failure.starts_with("tsc: TS2322"), "{}", found[0].failure);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn eslint_and_ruff_json_become_lint_findings() {
        let root = scratch("project-lint");
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("web/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(root.join("app.py"), "x = 1\n").unwrap();
        let eslint = r#"[{"filePath": "web/a.ts", "messages": [
            {"ruleId": "no-undef", "severity": 2, "message": "bar is not defined", "line": 1}
        ]}]"#;
        let found = eslint_diagnostics(eslint, &root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file, "web/a.ts");
        assert!(found[0].failure.contains("eslint: no-undef"));
        let ruff = r#"[{"code":"F401","message":"os imported but unused","filename":"app.py","location":{"row":1,"column":1}}]"#;
        let found = ruff_diagnostics(ruff, &root);
        assert_eq!((found[0].file.as_str(), found[0].line), ("app.py", 1));
        assert!(found[0].failure.contains("ruff: F401"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_rust_change_without_clippy_reports_the_install_option() {
        let root = scratch("no-clippy");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.0.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let report = run(&root, &["src/lib.rs".into()], false);
        if command_ok("cargo", &["clippy", "--version"]) {
            assert!(report.notes.iter().any(|n| n.contains("clippy checked") || n.contains("clippy failed")));
        } else {
            assert!(report.findings.is_empty());
            assert!(report.notes.iter().any(|n| n.contains("--install-lints")));
            assert!(report.markdown().contains("--install-lints"));
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
